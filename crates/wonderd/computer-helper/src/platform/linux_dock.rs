//! The Linux dock for the phone's "Windows & apps" menu: the installed GUI
//! applications (`.desktop` entries from the XDG data directories), running
//! ones first, then alphabetical. Desktop-specific pins (GNOME favourites, KDE
//! launchers) are not read: they do not exist on XFCE, Cinnamon, MATE, i3 or
//! Wayland compositors, while the application list works everywhere. Windows
//! whose app has no desktop entry follow as recent; excluded entries stay absent.
//! Pure std and serde_json:
//! the parsing is tested without X11.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Icon files larger than this are skipped; the daemon resizes the rest.
const MAX_ICON_FILE_BYTES: u64 = 64 * 1024;
const MAX_DESKTOP_BYTES: u64 = 64 * 1024;
const MAX_DESKTOP_FILES: usize = 4000;
/// The most applications listed; running ones take priority at the cap.
const MAX_ENTRIES: usize = 64;
/// Windows of apps with no listed entry.
const MAX_RECENT: usize = 8;
const ICON_SIZES: [&str; 10] = [
    "64x64", "48x48", "128x128", "96x96", "256x256", "32x32", "512x512", "24x24", "22x22", "16x16",
];

/// The same rule as the daemon's app IDs: letters, digits and `._-`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 150
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desktop {
    /// The desktop file ID, `org.gnome.Nautilus.desktop`.
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub wm_class: Option<String>,
    /// The program the entry starts, for matching windows without a class hint.
    pub program: Option<String>,
    /// `NoDisplay`/`Hidden`, a non-application `Type`: not an app to list.
    pub background: bool,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
}

impl Desktop {
    /// Is this an app the dock lists on the desktop named by
    /// `XDG_CURRENT_DESKTOP` (colon-separated, case-insensitive)?
    pub fn listed(&self, current: &[String]) -> bool {
        let any = |names: &[String]| {
            names
                .iter()
                .any(|n| current.iter().any(|c| c.eq_ignore_ascii_case(n)))
        };
        !self.background
            && (self.only_show_in.is_empty() || any(&self.only_show_in))
            && !any(&self.not_show_in)
    }
}

pub fn current_desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn parse_desktop(id: &str, text: &str) -> Option<Desktop> {
    let mut in_entry = false;
    let (mut name, mut icon, mut class, mut exec) = (None, None, None, None);
    let mut hidden = false;
    let mut not_app = false;
    let (mut only_show_in, mut not_show_in) = (Vec::new(), Vec::new());
    let list = |v: &str| -> Vec<String> {
        v.split(';')
            .filter(|i| !i.is_empty())
            .map(str::to_owned)
            .collect()
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            match key.trim_end() {
                "Name" if name.is_none() => name = Some(value.to_owned()),
                "Icon" => icon = Some(value.to_owned()).filter(|v| !v.is_empty()),
                "StartupWMClass" => class = Some(value.to_owned()).filter(|v| !v.is_empty()),
                "Exec" => exec = Some(value.to_owned()),
                "Hidden" | "NoDisplay" => hidden |= value == "true",
                "Type" => not_app = value != "Application",
                "OnlyShowIn" => only_show_in = list(value),
                "NotShowIn" => not_show_in = list(value),
                _ => {}
            }
        }
    }
    let program = exec.and_then(|exec| {
        // `env FOO=1 /usr/bin/app %U` → `app`.
        exec.split_whitespace()
            .find(|word| !word.contains('=') && *word != "env")
            .and_then(|word| word.trim_matches('"').rsplit('/').next().map(str::to_owned))
    });
    Some(Desktop {
        id: id.to_owned(),
        name: name.filter(|name| !name.is_empty())?,
        icon,
        wm_class: class,
        program,
        background: hidden || not_app,
        only_show_in,
        not_show_in,
    })
}

/// Does this window's `WM_CLASS` belong to the app? The class hint is matched
/// against `StartupWMClass`, then the desktop ID, then the program name.
pub fn matches_class(desktop: &Desktop, class: &str) -> bool {
    let class = class.to_lowercase();
    let stem = desktop.id.trim_end_matches(".desktop").to_lowercase();
    desktop
        .wm_class
        .as_ref()
        .is_some_and(|c| c.to_lowercase() == class)
        || stem == class
        || stem.rsplit('.').next() == Some(class.as_str())
        || desktop
            .program
            .as_ref()
            .is_some_and(|p| p.to_lowercase() == class)
}

#[derive(Clone, Debug, Default)]
pub struct Env {
    /// XDG data directories, user first; `applications/` and `icons/` inside.
    pub data_dirs: Vec<PathBuf>,
    /// Extra icon roots such as `~/.icons`.
    pub icon_roots: Vec<PathBuf>,
}

impl Env {
    pub fn from_environment() -> Self {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let mut data_dirs = Vec::new();
        match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            Some(dir) => data_dirs.push(PathBuf::from(dir)),
            None => data_dirs.extend(home.iter().map(|h| h.join(".local/share"))),
        }
        match std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|d| !d.is_empty())
        {
            Some(dirs) => data_dirs.extend(dirs.split(':').map(PathBuf::from)),
            None => data_dirs.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from)),
        }
        data_dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
        data_dirs.extend(
            home.iter()
                .map(|h| h.join(".local/share/flatpak/exports/share")),
        );
        data_dirs.push(PathBuf::from("/var/lib/snapd/desktop"));
        let mut seen = Vec::new();
        data_dirs.retain(|dir| {
            dir.is_absolute() && !seen.contains(dir) && {
                seen.push(dir.clone());
                true
            }
        });
        Self {
            data_dirs,
            icon_roots: home.iter().map(|h| h.join(".icons")).collect(),
        }
    }

    fn desktop_files(&self) -> Vec<(String, PathBuf)> {
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        for dir in &self.data_dirs {
            let root = dir.join("applications");
            let mut stack = vec![(root.clone(), 0)];
            while let Some((folder, depth)) = stack.pop() {
                let Ok(listing) = std::fs::read_dir(&folder) else {
                    continue;
                };
                let mut children: Vec<_> = listing.flatten().collect();
                children.sort_by_key(|e| e.file_name());
                for entry in children {
                    if found.len() >= MAX_DESKTOP_FILES {
                        return found;
                    }
                    let path = entry.path();
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() && depth < 2 {
                        stack.push((path, depth + 1));
                    } else if path.extension().is_some_and(|e| e == "desktop") {
                        // Subfolders join with `-`: kde/foo.desktop is kde-foo.desktop.
                        let Ok(relative) = path.strip_prefix(&root) else {
                            continue;
                        };
                        let id = relative.to_string_lossy().replace('/', "-");
                        if !found.iter().any(|(known, _)| *known == id) {
                            found.push((id, path));
                        }
                    }
                }
            }
        }
        found
    }

    fn load(&self, id: &str, path: &Path) -> Option<Desktop> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_DESKTOP_BYTES {
            return None;
        }
        parse_desktop(id, &std::fs::read_to_string(path).ok()?)
    }

    pub fn find_desktop(&self, id: &str) -> Option<Desktop> {
        if !valid_id(id) || !id.ends_with(".desktop") {
            return None;
        }
        self.desktop_files()
            .into_iter()
            .find(|(known, _)| known == id)
            .and_then(|(known, path)| self.load(&known, &path))
    }

    /// A PNG file for an `Icon=` value: an absolute path, or the theme lookup
    /// (preferred sizes first, hicolor first) and pixmaps. Vector-only icons
    /// give nothing; the phone then shows a generic symbol.
    pub fn find_icon(&self, icon: &str) -> Option<PathBuf> {
        let usable = |path: &Path| {
            path.extension().is_some_and(|e| e == "png")
                && std::fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && m.len() <= MAX_ICON_FILE_BYTES)
        };
        if icon.starts_with('/') {
            return usable(Path::new(icon)).then(|| PathBuf::from(icon));
        }
        if icon.is_empty() || icon.contains('/') || icon.starts_with('.') {
            return None;
        }
        let mut roots: Vec<PathBuf> = self.data_dirs.iter().map(|d| d.join("icons")).collect();
        roots.extend(self.icon_roots.iter().cloned());
        for size in ICON_SIZES {
            for root in &roots {
                let mut themes: Vec<String> = std::fs::read_dir(root)
                    .map(|l| {
                        l.flatten()
                            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .take(24)
                            .collect()
                    })
                    .unwrap_or_default();
                themes.sort_by_key(|t| (t != "hicolor", t.clone()));
                for theme in themes {
                    let path = root
                        .join(&theme)
                        .join(size)
                        .join("apps")
                        .join(format!("{icon}.png"));
                    if usable(&path) {
                        return Some(path);
                    }
                }
            }
        }
        self.data_dirs
            .iter()
            .map(|d| d.join("pixmaps").join(format!("{icon}.png")))
            .find(|path| usable(path))
    }
}

fn pretty_class(class: &str) -> String {
    let last = class.rsplit('.').next().unwrap_or(class);
    last.replace(['-', '_'], " ")
}

fn entry(env: &Env, desktop: Option<&Desktop>, id: &str, name: &str, running: bool) -> Value {
    use std::io::Read;
    let mut app = json!({"id": id, "name": name, "running": running});
    let png = desktop
        .and_then(|d| d.icon.as_deref())
        .and_then(|icon| env.find_icon(icon))
        .and_then(|path| {
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .ok()?
                .take(MAX_ICON_FILE_BYTES)
                .read_to_end(&mut bytes)
                .ok()?;
            Some(bytes)
        });
    if let Some(png) = png {
        app["iconPNG"] = json!(base64_encode(&png));
    }
    app
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The dock answer. `pinned` is the installed application list (the key keeps
/// the wire shape stable): running first, then alphabetical. `recent` holds
/// windows of apps with no listed entry, most recently used first. `running`
/// is the `WM_CLASS` of the windows open now, most recently used first.
pub fn build(env: &Env, current_desktops: &[String], running: &[String]) -> Value {
    let all: Vec<Desktop> = env
        .desktop_files()
        .iter()
        .filter_map(|(id, path)| env.load(id, path))
        .collect();
    let is_running = |desktop: &Desktop| running.iter().any(|class| matches_class(desktop, class));
    let mut listed: Vec<(bool, &Desktop)> = all
        .iter()
        .filter(|d| d.listed(current_desktops))
        .map(|d| (is_running(d), d))
        .collect();
    listed.sort_by_cached_key(|(running, d)| (!running, d.name.to_lowercase(), d.id.clone()));
    listed.truncate(MAX_ENTRIES);
    let pinned: Vec<Value> = listed
        .iter()
        .map(|(running, d)| entry(env, Some(d), &d.id, &d.name, *running))
        .collect();

    let mut recent: Vec<Value> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    for class in running {
        if recent.len() >= MAX_RECENT || !valid_id(class) {
            continue;
        }
        if listed.iter().any(|(_, d)| matches_class(d, class)) {
            continue;
        }
        // Keep excluded desktop entries out of recent too. Retain them in `all`
        // so a matching window cannot fall back to an unlabelled class entry.
        match all.iter().find(|d| matches_class(d, class)) {
            Some(d) if d.listed(current_desktops) && !shown.contains(&d.id) => {
                shown.push(d.id.clone());
                recent.push(entry(env, Some(d), &d.id, &d.name, true));
            }
            Some(_) => {}
            None if !shown.contains(class) => {
                shown.push(class.clone());
                recent.push(entry(env, None, class, &pretty_class(class), true));
            }
            None => {}
        }
    }
    json!({"available": true, "pinned": pinned, "recent": recent})
}

/// `--dock`: the answer for the machine this runs on. The daemon caches it.
pub fn dock(running: Result<Vec<String>, String>) -> Value {
    match running {
        Ok(running) => build(&Env::from_environment(), &current_desktops(), &running),
        Err(_) => json!({"available": false, "pinned": [], "recent": []}),
    }
}

/// `--launch-app ID`: starts a desktop file by ID with the desktop's own
/// launcher. Only desktop file IDs launch; a running app without one cannot.
pub fn launch(id: &str) -> Result<(), String> {
    if !valid_id(id) || !id.ends_with(".desktop") {
        return Err("not_launchable".into());
    }
    let env = Env::from_environment();
    if env.find_desktop(id).is_none() {
        return Err("app_not_found".into());
    }
    let stem = id.trim_end_matches(".desktop");
    for (program, args) in [("gtk-launch", vec![stem]), ("gio", vec!["launch", id])] {
        let started = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if started.is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    Err("launch_failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway folder, removed on drop (the helper has no tempfile dependency).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("linux-dock-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn desktop_entries_read_name_class_and_program() {
        let text = "[Desktop Entry]\nName=Files\nName[de]=Dateien\nExec=env GDK_X=1 /usr/bin/nautilus --new-window %U\nIcon=org.gnome.Nautilus\nStartupWMClass=Org.gnome.Nautilus\n[Desktop Action new]\nName=New\n";
        let d = parse_desktop("org.gnome.Nautilus.desktop", text).unwrap();
        assert_eq!(d.name, "Files");
        assert_eq!(d.program.as_deref(), Some("nautilus"));
        assert_eq!(d.wm_class.as_deref(), Some("Org.gnome.Nautilus"));
        assert!(matches_class(&d, "org.gnome.nautilus"));
        assert!(matches_class(&d, "nautilus"));
        assert!(!matches_class(&d, "firefox"));
        assert!(d.listed(&[]));
        let hidden = parse_desktop("x.desktop", "[Desktop Entry]\nName=X\nHidden=true\n").unwrap();
        assert!(!hidden.listed(&[]));
        assert!(parse_desktop("x.desktop", "[Desktop Entry]\nExec=x\n").is_none());
    }

    #[test]
    fn dock_lists_installed_apps_running_first_then_alphabetical() {
        let root = Scratch::new("build");
        let app = |name: &str, extra: &str| {
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n{extra}")
        };
        write(
            root.path(),
            "applications/firefox.desktop",
            &app("Firefox", "Icon=firefox\nStartupWMClass=firefox\n"),
        );
        write(
            root.path(),
            "applications/org.gnome.Nautilus.desktop",
            &app("Files", "Icon=/nowhere.png\n"),
        );
        write(
            root.path(),
            "applications/gedit.desktop",
            &app("Text Editor", "StartupWMClass=Gedit\n"),
        );
        write(
            root.path(),
            "applications/hidden.desktop",
            &app("Hidden Helper", "NoDisplay=true\n"),
        );
        write(
            root.path(),
            "applications/gone.desktop",
            &app("Gone", "Hidden=true\n"),
        );
        write(
            root.path(),
            "applications/kde.desktop",
            &app("KDE Only", "OnlyShowIn=KDE;\n"),
        );
        write(
            root.path(),
            "applications/notgnome.desktop",
            &app("Not Gnome", "NotShowIn=GNOME;\n"),
        );
        write(
            root.path(),
            "applications/link.desktop",
            "[Desktop Entry]\nType=Link\nName=Link\nURL=x\n",
        );
        write(root.path(), "icons/hicolor/48x48/apps/firefox.png", "PNG48");
        write(root.path(), "icons/hicolor/64x64/apps/firefox.png", "PNG64");
        let env = Env {
            data_dirs: vec![root.path().to_path_buf()],
            icon_roots: vec![],
        };
        // Excluded entries must stay absent even while running. The unmatched
        // xterm window still belongs in recent.
        let running = [
            "Gedit", "xterm", "hidden", "gone", "kde", "notgnome", "link", "firefox",
        ]
        .map(str::to_owned);
        let answer = build(&env, &["GNOME".to_owned()], &running);
        assert_eq!(answer["available"], true);
        let names = |key: &str| -> Vec<(String, bool)> {
            answer[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| {
                    (
                        a["name"].as_str().unwrap().into(),
                        a["running"].as_bool().unwrap(),
                    )
                })
                .collect()
        };
        // No gsettings involved: running (alphabetical) then the rest (alphabetical).
        assert_eq!(
            names("pinned"),
            [
                ("Firefox".into(), true),
                ("Text Editor".into(), true),
                ("Files".into(), false),
            ]
        );
        assert_eq!(names("recent"), [("xterm".into(), true)]);
        // The 64 px theme icon wins over 48 px; the others have none.
        assert_eq!(answer["pinned"][0]["iconPNG"], base64_encode(b"PNG64"));
        assert!(answer["pinned"][2].get("iconPNG").is_none());
        // On KDE the KDE-only entry appears and the NotShowIn=GNOME one stays.
        let kde = build(&env, &["KDE".to_owned()], &[]);
        let ids: Vec<&str> = kde["pinned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["Files", "Firefox", "KDE Only", "Not Gnome", "Text Editor"]
        );
    }

    #[test]
    fn a_host_without_apps_shows_running_windows_as_recent() {
        let env = Env {
            data_dirs: vec![],
            icon_roots: vec![],
        };
        let answer = build(&env, &[], &["XTerm".to_owned(), "bad id".to_owned()]);
        assert_eq!(answer["pinned"], json!([]));
        assert_eq!(
            answer["recent"],
            json!([{"id": "XTerm", "name": "XTerm", "running": true}])
        );
        assert_eq!(dock(Err("no display".into()))["available"], false);
    }

    #[test]
    fn launch_refuses_anything_but_a_desktop_file_id() {
        for id in ["xterm", "../x.desktop", "a b.desktop", ""] {
            assert_eq!(launch(id), Err("not_launchable".into()));
        }
    }
}
