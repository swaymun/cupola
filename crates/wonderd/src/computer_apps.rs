//! Installed Mac apps for the phone's app launcher: a folder scan of the
//! Applications folders, a small icon per app and a launch by bundle ID.
//!
//! Enumeration informed by Sill (https://github.com/Saffsanity/sill,
//! Apache-2.0, `InstalledApps.swift`), independently implemented: scan
//! breadth-first, never descend into a bundle or follow a symlink, dedupe by
//! bundle ID with the first folder winning, sort by name. Unlike Sill the list
//! is rescanned when it is older than a few minutes, so a newly installed app
//! appears without restarting.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstalledApp {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

/// Bounds: suites nest a level or two; nothing reasonable goes deeper.
const MAX_DEPTH: usize = 3;
const MAX_APPS: usize = 2_000;
const MAX_ENTRIES: usize = 20_000;
const MAX_PLIST_BYTES: u64 = 1024 * 1024;
const MAX_ICNS_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const ICON_PIXELS: u32 = 64;
const RESCAN_AFTER: Duration = Duration::from_secs(5 * 60);

/// The folders macOS installs apps into, in precedence order.
pub(crate) fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots
}

/// The same rule as window sources' app IDs: a reverse-DNS bundle identifier.
pub(crate) fn valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 150
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
}

pub(crate) fn scan(roots: &[PathBuf]) -> Vec<InstalledApp> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    let mut entries = 0usize;
    for root in roots {
        let mut queue = VecDeque::from([(root.clone(), 0usize)]);
        while let Some((folder, depth)) = queue.pop_front() {
            let Ok(listing) = std::fs::read_dir(&folder) else {
                continue;
            };
            let mut children: Vec<_> = listing.flatten().collect();
            children.sort_by_key(|entry| entry.file_name());
            for entry in children {
                entries += 1;
                if entries > MAX_ENTRIES || apps.len() >= MAX_APPS {
                    return sorted(apps);
                }
                // Never follow a symlink inside a folder.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if !kind.is_dir() {
                    continue;
                }
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    continue;
                }
                match path.extension().and_then(|ext| ext.to_str()) {
                    Some(ext) if ext.eq_ignore_ascii_case("app") => {
                        let id = bundle_identifier(&path).filter(|id| seen.insert(id.clone()));
                        if let Some(id) = id {
                            let display = name[..name.len() - ext.len() - 1].to_owned();
                            apps.push(InstalledApp {
                                id,
                                name: display,
                                path,
                            });
                        }
                    }
                    // Any other package (framework, plugin, bundle) stays closed.
                    Some(_) => {}
                    None if depth < MAX_DEPTH => queue.push_back((path, depth + 1)),
                    None => {}
                }
            }
        }
    }
    sorted(apps)
}

fn sorted(mut apps: Vec<InstalledApp>) -> Vec<InstalledApp> {
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    apps
}

pub(crate) fn info_plist(app: &Path) -> Option<String> {
    let path = app.join("Contents/Info.plist");
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PLIST_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    if bytes.starts_with(b"bplist") {
        return binary_plist_as_xml(&path);
    }
    String::from_utf8(bytes).ok()
}

/// A few Apple apps ship a binary Info.plist; macOS's own tool reads it.
#[cfg(target_os = "macos")]
fn binary_plist_as_xml(path: &Path) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "xml1", "-o", "-"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

#[cfg(not(target_os = "macos"))]
fn binary_plist_as_xml(_path: &Path) -> Option<String> {
    None
}

fn bundle_identifier(app: &Path) -> Option<String> {
    let id = plist_string(&info_plist(app)?, "CFBundleIdentifier")?;
    valid_bundle_id(&id).then_some(id)
}

/// The string value for a top-level key of an XML property list.
pub(crate) fn plist_string(xml: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let rest = &xml[xml.find(&marker)? + marker.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("<string>")?;
    let value = &rest[..rest.find("</string>")?];
    let value = value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn icon_file(app: &Path) -> Option<PathBuf> {
    let resources = app.join("Contents/Resources");
    let named = info_plist(app)
        .and_then(|xml| plist_string(&xml, "CFBundleIconFile"))
        .filter(|name| !name.contains('/') && !name.starts_with('.'))
        .map(|name| {
            if name.to_ascii_lowercase().ends_with(".icns") {
                name
            } else {
                format!("{name}.icns")
            }
        });
    named
        .into_iter()
        .chain(std::iter::once("AppIcon.icns".to_owned()))
        .map(|name| resources.join(name))
        .find(|path| path.is_file())
}

/// The app's icon as a square PNG of `ICON_PIXELS`, from the PNG artwork inside
/// its .icns (the smallest one at least that size). None when it has no icon or
/// only older artwork formats; the phone then shows a generic symbol.
pub(crate) fn icon_png(app: &Path) -> Option<Vec<u8>> {
    let path = icon_file(app)?;
    if std::fs::metadata(&path).ok()?.len() > MAX_ICNS_BYTES {
        return None;
    }
    let icns = std::fs::read(path).ok()?;
    let png = best_icns_png(&icns, ICON_PIXELS)?;
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
    let image = image.resize_exact(
        ICON_PIXELS,
        ICON_PIXELS,
        image::imageops::FilterType::Triangle,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Picks the smallest embedded PNG whose width is at least `pixels`, or the
/// largest one when all are smaller.
pub(crate) fn best_icns_png(icns: &[u8], pixels: u32) -> Option<&[u8]> {
    if icns.len() < 8 || &icns[..4] != b"icns" {
        return None;
    }
    let total = (u32::from_be_bytes(icns[4..8].try_into().ok()?) as usize).min(icns.len());
    let mut offset = 8;
    let mut best: Option<(u32, &[u8])> = None;
    while offset + 8 <= total {
        let length = u32::from_be_bytes(icns[offset + 4..offset + 8].try_into().ok()?) as usize;
        if length < 8 || offset + length > total {
            break;
        }
        let data = &icns[offset + 8..offset + length];
        if data.starts_with(PNG_SIGNATURE) && data.len() >= 24 && &data[12..16] == b"IHDR" {
            let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
            let better = match best {
                None => true,
                Some((current, _)) if current >= pixels => width >= pixels && width < current,
                Some((current, _)) => width > current,
            };
            if better && width > 0 {
                best = Some((width, data));
            }
        }
        offset += length;
    }
    best.map(|(_, data)| data)
}

static CATALOG: Mutex<Option<(Instant, Arc<Vec<InstalledApp>>)>> = Mutex::new(None);

/// The installed apps, rescanned off the async runtime when the list is stale.
pub(crate) async fn catalog() -> Arc<Vec<InstalledApp>> {
    let cached = CATALOG.lock().ok().and_then(|cache| cache.clone());
    if let Some((_, apps)) = cached.filter(|(scanned, _)| scanned.elapsed() < RESCAN_AFTER) {
        return apps;
    }
    let apps = tokio::task::spawn_blocking(|| Arc::new(scan(&default_roots())))
        .await
        .unwrap_or_default();
    if let Ok(mut cache) = CATALOG.lock() {
        *cache = Some((Instant::now(), apps.clone()));
    }
    apps
}

/// Opens an app from the catalog on the computer, in front, like the Dock.
#[cfg(all(target_os = "macos", not(test)))]
pub(crate) fn launch(app: &InstalledApp) -> std::io::Result<()> {
    let status = std::process::Command::new("/usr/bin/open")
        .arg("-b")
        .arg(&app.id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("open refused the app"))
    }
}

// ---------------------------------------------------------------- dock

/// What the phone's "Windows & apps" menu shows: the host's pinned apps, then
/// the apps in use. Each OS supplies the raw list its own way (macOS reads the
/// Dock's preferences here; Linux and Windows ask the computer helper); this
/// module bounds and validates whatever arrives so the wire shape is one.
#[cfg(target_os = "macos")]
pub(crate) const MAX_PINNED: usize = 24;
pub(crate) const MAX_RECENT: usize = 8;
/// A helper (Linux, Windows) lists installed apps, not a few pins.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) const MAX_LISTED: usize = 64;
/// A normalised 64 px icon above this is dropped rather than sent.
pub(crate) const MAX_ICON_BYTES: usize = 32 * 1024;
/// The largest icon file taken from a helper before resizing.
#[cfg_attr(target_os = "macos", allow(dead_code))]
const MAX_SOURCE_ICON_BYTES: usize = 192 * 1024;
const MAX_NAME_CHARS: usize = 80;
#[cfg(not(test))]
const DOCK_CACHE_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockApp {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub icon: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Dock {
    pub available: bool,
    pub pinned: Vec<DockApp>,
    pub recent: Vec<DockApp>,
}

impl Dock {
    pub(crate) fn find(&self, id: &str) -> Option<&DockApp> {
        self.pinned
            .iter()
            .chain(&self.recent)
            .find(|app| app.id == id)
    }

    /// The `GET /api/v1/computer/dock` answer. `iconURL` is present only when
    /// the host has an icon to serve at that route.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let app = |app: &DockApp| {
            let mut value = serde_json::json!({
                "id": app.id,
                "name": app.name,
                "running": app.running,
            });
            if app.icon.is_some() {
                value["iconURL"] = format!("/api/v1/computer/apps/icon?id={}", app.id).into();
            }
            value
        };
        serde_json::json!({
            "available": self.available,
            "pinned": self.pinned.iter().map(app).collect::<Vec<_>>(),
            "recent": self.recent.iter().map(app).collect::<Vec<_>>(),
        })
    }
}

fn clean_name(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().chars().take(MAX_NAME_CHARS).collect()
}

/// Any PNG becomes the one icon size the phone expects, or nothing.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn normalize_icon(png: &[u8]) -> Option<Vec<u8>> {
    if png.len() > MAX_SOURCE_ICON_BYTES || !png.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
    if image.width() == 0 || image.height() == 0 || image.width() > 1024 || image.height() > 1024 {
        return None;
    }
    let image = image.resize_exact(
        ICON_PIXELS,
        ICON_PIXELS,
        image::imageops::FilterType::Triangle,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    let out = out.into_inner();
    (out.len() <= MAX_ICON_BYTES).then_some(out)
}

/// Builds the bounded dock from raw entries: ids must be launchable names, a
/// pinned app is never also recent, duplicates keep their first place.
#[cfg(target_os = "macos")]
pub(crate) fn assemble(available: bool, pinned: Vec<DockApp>, recent: Vec<DockApp>) -> Dock {
    assemble_limited(available, pinned, recent, MAX_PINNED)
}

fn assemble_limited(
    available: bool,
    pinned: Vec<DockApp>,
    recent: Vec<DockApp>,
    max_pinned: usize,
) -> Dock {
    let mut seen = HashSet::new();
    let mut keep = |apps: Vec<DockApp>, limit: usize| -> Vec<DockApp> {
        let mut kept = Vec::new();
        for mut app in apps {
            app.name = clean_name(&app.name);
            if kept.len() >= limit || !valid_bundle_id(&app.id) || !seen.insert(app.id.clone()) {
                continue;
            }
            if app.name.is_empty() {
                app.name = app.id.clone();
            }
            if app
                .icon
                .as_ref()
                .is_some_and(|icon| icon.len() > MAX_ICON_BYTES)
            {
                app.icon = None;
            }
            kept.push(app);
        }
        kept
    };
    let pinned = keep(pinned, max_pinned);
    let recent = keep(recent, MAX_RECENT);
    Dock {
        available,
        pinned,
        recent,
    }
}

/// Reads the computer helper's `--dock` answer: `{available, pinned:[…],
/// recent:[…]}` with `{id, name, running, iconPNG?}` apps (base64 PNG).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn from_helper(value: &serde_json::Value) -> Option<Dock> {
    use base64::Engine;
    let list = |key: &str| -> Option<Vec<DockApp>> {
        value
            .get(key)?
            .as_array()?
            .iter()
            .take(MAX_LISTED + 16)
            .map(|entry| {
                let icon = entry
                    .get("iconPNG")
                    .and_then(|icon| icon.as_str())
                    .filter(|icon| icon.len() <= MAX_SOURCE_ICON_BYTES * 4 / 3 + 8)
                    .and_then(|icon| base64::engine::general_purpose::STANDARD.decode(icon).ok())
                    .and_then(|png| normalize_icon(&png));
                Some(DockApp {
                    id: entry.get("id")?.as_str()?.to_owned(),
                    name: entry.get("name")?.as_str()?.to_owned(),
                    running: entry.get("running")?.as_bool()?,
                    icon,
                })
            })
            .collect()
    };
    Some(assemble_limited(
        value.get("available")?.as_bool()?,
        list("pinned")?,
        list("recent")?,
        MAX_LISTED,
    ))
}

/// One Dock tile from `com.apple.dock`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockTile {
    pub id: String,
    pub label: Option<String>,
    pub path: Option<PathBuf>,
}

/// The content of the array under a top-level key of an XML property list,
/// matching nested arrays.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_array<'a>(xml: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("<key>{key}</key>");
    let rest = xml[xml.find(&marker)? + marker.len()..].trim_start();
    if rest.starts_with("<array/>") {
        return Some("");
    }
    let rest = rest.strip_prefix("<array>")?;
    let mut depth = 1usize;
    let mut at = 0;
    loop {
        let open = rest[at..].find("<array>");
        let close = rest[at..].find("</array>")?;
        if open.is_some_and(|open| open < close) {
            depth += 1;
            at += open.unwrap_or(0) + "<array>".len();
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(&rest[..at + close]);
            }
            at += close + "</array>".len();
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn file_url_path(url: &str) -> Option<PathBuf> {
    let path = url.strip_prefix("file://")?;
    let mut bytes = Vec::new();
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 3 <= raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(bytes).ok()?;
    let decoded = decoded.trim_end_matches('/');
    (decoded.starts_with('/') && !decoded.contains("/../")).then(|| PathBuf::from(decoded))
}

/// The apps in one Dock section (`persistent-apps` or `recent-apps`). Spacers
/// and folder or web tiles carry no bundle identifier and are skipped.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn dock_tiles(xml: &str, section: &str) -> Vec<DockTile> {
    let Some(array) = plist_array(xml, section) else {
        return Vec::new();
    };
    const KEY: &str = "<key>bundle-identifier</key>";
    let starts: Vec<usize> = array.match_indices(KEY).map(|(i, _)| i).collect();
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, start)| {
            let end = starts.get(n + 1).copied().unwrap_or(array.len());
            let chunk = &array[*start..end];
            let id = plist_string(chunk, "bundle-identifier").filter(|id| valid_bundle_id(id))?;
            Some(DockTile {
                id,
                label: plist_string(chunk, "file-label"),
                path: plist_string(chunk, "_CFURLString").and_then(|url| file_url_path(&url)),
            })
        })
        .collect()
}

/// `show-recents` defaults to on.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn dock_shows_recents(xml: &str) -> bool {
    let marker = "<key>show-recents</key>";
    xml.find(marker)
        .map(|at| {
            !xml[at + marker.len()..]
                .trim_start()
                .starts_with("<false/>")
        })
        .unwrap_or(true)
}

/// A running foreground app from `lsappinfo list`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunningApp {
    pub id: String,
    pub name: String,
    pub path: Option<PathBuf>,
}

/// Foreground apps in `lsappinfo list` order (oldest launch first).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_lsappinfo(text: &str) -> Vec<RunningApp> {
    let quoted = |line: &str, prefix: &str| -> Option<String> {
        let rest = line.strip_prefix(prefix)?.strip_prefix('"')?;
        Some(rest[..rest.find('"')?].to_owned())
    };
    let mut apps = Vec::new();
    let mut current: Option<(String, Option<String>, Option<PathBuf>, bool)> = None;
    let mut flush = |current: &mut Option<(String, Option<String>, Option<PathBuf>, bool)>| {
        if let Some((name, Some(id), path, true)) = current.take() {
            if valid_bundle_id(&id) {
                apps.push(RunningApp { id, name, path });
            }
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if !line.starts_with(char::is_whitespace) || trimmed.contains(" ASN:") {
            if let Some(name) = trimmed
                .find('"')
                .filter(|_| trimmed.contains(" ASN:"))
                .and_then(|at| quoted(&trimmed[at..], ""))
            {
                flush(&mut current);
                current = Some((name, None, None, false));
                continue;
            }
        }
        if let Some(entry) = current.as_mut() {
            if let Some(id) = quoted(trimmed, "bundleID=") {
                entry.1 = Some(id);
            } else if let Some(path) = quoted(trimmed, "bundle path=") {
                entry.2 = Some(PathBuf::from(path));
            } else if trimmed.contains(" type=\"Foreground\"")
                || trimmed.starts_with("type=\"Foreground\"")
            {
                entry.3 = true;
            }
        }
    }
    flush(&mut current);
    apps
}

/// The Mac's Dock: pinned apps from `com.apple.dock`; the Dock's other
/// running apps, then its recents, as `recent`.
#[cfg(target_os = "macos")]
fn read_mac_dock() -> Dock {
    let output = |program: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).ok())
            .flatten()
    };
    let Some(xml) = output("/usr/bin/defaults", &["export", "com.apple.dock", "-"]) else {
        return Dock::default();
    };
    let running = output("/usr/bin/lsappinfo", &["list"])
        .map(|text| parse_lsappinfo(&text))
        .unwrap_or_default();
    let installed = std::cell::OnceCell::new();
    let tiles_to_apps = |tiles: Vec<DockTile>, running: &[RunningApp]| -> Vec<DockApp> {
        tiles
            .into_iter()
            .map(|tile| {
                let live = running.iter().find(|app| app.id == tile.id);
                let path = tile
                    .path
                    .or_else(|| live.and_then(|app| app.path.clone()))
                    .or_else(|| {
                        // Scan the Applications folders once, and only if a tile lacks a path.
                        installed
                            .get_or_init(|| scan(&default_roots()))
                            .iter()
                            .find(|app| app.id == tile.id)
                            .map(|app| app.path.clone())
                    });
                DockApp {
                    name: tile
                        .label
                        .or_else(|| live.map(|app| app.name.clone()))
                        .unwrap_or_else(|| tile.id.clone()),
                    running: live.is_some(),
                    icon: path.and_then(|path| icon_png(&path)),
                    id: tile.id,
                }
            })
            .collect()
    };
    let pinned = tiles_to_apps(dock_tiles(&xml, "persistent-apps"), &running);
    let mut recent: Vec<DockApp> = running
        .iter()
        .rev()
        .map(|app| DockApp {
            id: app.id.clone(),
            name: app.name.clone(),
            running: true,
            icon: app.path.as_deref().and_then(icon_png),
        })
        .collect();
    if dock_shows_recents(&xml) {
        recent.extend(tiles_to_apps(dock_tiles(&xml, "recent-apps"), &running));
    }
    assemble(true, pinned, recent)
}

#[cfg(not(test))]
static DOCK: Mutex<Option<(Instant, Arc<Dock>)>> = Mutex::new(None);

/// The most recent dock answer (even if stale), for icon and launch lookups.
pub(crate) fn last_dock() -> Option<Arc<Dock>> {
    // Tests see only their own fixture, never another test's cached answer.
    #[cfg(test)]
    return TEST_DOCK.lock().unwrap().clone().map(Arc::new);
    #[cfg(not(test))]
    DOCK.lock()
        .ok()
        .and_then(|cache| cache.as_ref().map(|(_, dock)| dock.clone()))
}

/// The host's dock, cached for a few seconds. `helper` is the computer helper
/// binary that answers on Linux and Windows.
pub(crate) async fn dock(helper: Option<&Path>) -> Arc<Dock> {
    #[cfg(test)]
    return Arc::new(read_dock(helper));
    #[cfg(not(test))]
    dock_cached(helper).await
}

#[cfg(not(test))]
async fn dock_cached(helper: Option<&Path>) -> Arc<Dock> {
    let cached = DOCK.lock().ok().and_then(|cache| cache.clone());
    if let Some((at, dock)) = cached {
        if at.elapsed() < DOCK_CACHE_FOR {
            return dock;
        }
    }
    let helper = helper.map(Path::to_owned);
    let fresh = tokio::task::spawn_blocking(move || read_dock(helper.as_deref()))
        .await
        .unwrap_or_default();
    let fresh = Arc::new(fresh);
    if let Ok(mut cache) = DOCK.lock() {
        *cache = Some((Instant::now(), fresh.clone()));
    }
    fresh
}

#[cfg(all(target_os = "macos", not(test)))]
fn read_dock(_helper: Option<&Path>) -> Dock {
    read_mac_dock()
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn read_dock(helper: Option<&Path>) -> Dock {
    helper_dock(helper).unwrap_or_default()
}

#[cfg(test)]
fn read_dock(_helper: Option<&Path>) -> Dock {
    TEST_DOCK.lock().unwrap().clone().unwrap_or_default()
}

#[cfg(test)]
pub(crate) static TEST_DOCK: Mutex<Option<Dock>> = Mutex::new(None);

/// Runs `helper --dock` (one JSON line) with a short deadline and a size cap.
#[cfg(not(target_os = "macos"))]
#[cfg_attr(test, allow(dead_code))]
fn helper_dock(helper: Option<&Path>) -> Option<Dock> {
    use std::io::Read;
    let mut child = std::process::Command::new(helper?)
        .arg("--dock")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(4 * 1024 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let bytes = reader.join().ok()?;
    from_helper(&serde_json::from_slice(&bytes).ok()?)
}

/// Opens (or brings forward) a dock app by its dock ID.
#[cfg(all(target_os = "macos", not(test)))]
pub(crate) fn launch_dock_app(_helper: Option<&Path>, id: &str) -> std::io::Result<()> {
    launch(&InstalledApp {
        id: id.to_owned(),
        name: String::new(),
        path: PathBuf::new(),
    })
}

#[cfg(all(not(target_os = "macos"), not(test)))]
pub(crate) fn launch_dock_app(helper: Option<&Path>, id: &str) -> std::io::Result<()> {
    let helper = helper.ok_or_else(|| std::io::Error::other("no computer helper"))?;
    let status = std::process::Command::new(helper)
        .args(["--launch-app", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("the helper could not open the app"))
    }
}

#[cfg(test)]
pub(crate) fn launch_dock_app(_helper: Option<&Path>, id: &str) -> std::io::Result<()> {
    LAUNCHED.lock().unwrap().push(id.to_owned());
    Ok(())
}

#[cfg(test)]
pub(crate) static LAUNCHED: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn launch(app: &InstalledApp) -> std::io::Result<()> {
    LAUNCHED.lock().unwrap().push(app.id.clone());
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(test)))]
pub(crate) fn launch(_app: &InstalledApp) -> std::io::Result<()> {
    Err(std::io::Error::other("app launching is only on Mac"))
}

#[cfg(test)]
pub(crate) fn set_catalog_for_test(apps: Vec<InstalledApp>) {
    *CATALOG.lock().unwrap() = Some((Instant::now(), Arc::new(apps)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plist(id: &str, icon: Option<&str>) -> String {
        let icon = icon
            .map(|name| format!("<key>CFBundleIconFile</key>\n\t<string>{name}</string>"))
            .unwrap_or_default();
        format!(
            "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict>\n\t<key>CFBundleIdentifier</key>\n\t<string>{id}</string>\n\t{icon}\n</dict></plist>"
        )
    }

    fn app(root: &Path, relative: &str, id: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.join("Contents/Resources")).unwrap();
        std::fs::write(path.join("Contents/Info.plist"), plist(id, Some("Icon"))).unwrap();
        path
    }

    fn png(width: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, width, image::Rgba([10, 120, 240, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn icns(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let body: Vec<u8> = chunks
            .iter()
            .flat_map(|(kind, data)| {
                let mut chunk = kind.to_vec();
                chunk.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
                chunk.extend_from_slice(data);
                chunk
            })
            .collect();
        let mut file = b"icns".to_vec();
        file.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        file.extend(body);
        file
    }

    #[test]
    fn scan_finds_nested_apps_dedupes_by_bundle_id_and_never_opens_bundles() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        app(first.path(), "Zed.app", "dev.zed.Zed");
        app(first.path(), "Utilities/Terminal.app", "com.apple.Terminal");
        app(
            first.path(),
            "Adobe Photoshop 2026/Adobe Photoshop 2026.app",
            "com.adobe.Photoshop",
        );
        // An app inside another app's bundle is a helper, not a launcher entry.
        app(
            first.path(),
            "Zed.app/Contents/Helpers/Zed Helper.app",
            "dev.zed.helper",
        );
        // Frameworks and other packages stay closed.
        app(
            first.path(),
            "Thing.framework/Inner.app",
            "com.example.inner",
        );
        // Too deep to be an install.
        app(first.path(), "a/b/c/d/Deep.app", "com.example.deep");
        // Hidden folders and apps without a usable identifier are skipped.
        app(first.path(), ".hidden/Secret.app", "com.example.secret");
        app(first.path(), "Broken.app", "not valid!");
        // The first folder wins a duplicate bundle ID.
        app(second.path(), "Terminal copy.app", "com.apple.Terminal");
        app(second.path(), "calculator.app", "com.apple.calculator");
        #[cfg(unix)]
        std::os::unix::fs::symlink(first.path().join("Utilities"), second.path().join("Linked"))
            .unwrap();

        let apps = scan(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        let names: Vec<_> = apps
            .iter()
            .map(|app| (app.name.as_str(), app.id.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("Adobe Photoshop 2026", "com.adobe.Photoshop"),
                ("calculator", "com.apple.calculator"),
                ("Terminal", "com.apple.Terminal"),
                ("Zed", "dev.zed.Zed"),
            ]
        );
        assert!(apps[2].path.ends_with("Utilities/Terminal.app"));
    }

    #[test]
    fn plist_reading_takes_the_top_string_and_decodes_entities() {
        let xml = plist("com.example.a&amp;b", None);
        assert_eq!(
            plist_string(&xml, "CFBundleIdentifier").as_deref(),
            Some("com.example.a&b")
        );
        assert_eq!(plist_string(&xml, "CFBundleIconFile"), None);
        assert_eq!(
            plist_string("<key>CFBundleIdentifier</key><true/>", "CFBundleIdentifier"),
            None
        );
        assert!(!valid_bundle_id("com.example.a&b"));
        assert!(valid_bundle_id("com.apple.Safari"));
    }

    #[test]
    fn icns_picks_the_smallest_png_at_least_the_icon_size() {
        let file = icns(&[
            (b"ic07", png(128)),
            (b"icp4", png(16)),
            (b"ic12", png(64)),
            (b"ic10", png(1024)),
            // JPEG 2000 and other artwork is ignored.
            (b"ic09", vec![0, 0, 0, 12, b'j', b'P', 0, 0, 0, 0, 0, 0]),
        ]);
        let chosen = best_icns_png(&file, 64).unwrap();
        assert_eq!(&chosen[16..20], &64u32.to_be_bytes());
        // Only small artwork: take the largest.
        let small = icns(&[(b"icp4", png(16)), (b"icp5", png(32))]);
        assert_eq!(
            &best_icns_png(&small, 64).unwrap()[16..20],
            &32u32.to_be_bytes()
        );
        // Truncated or foreign files give nothing rather than a panic.
        assert!(best_icns_png(b"icns\0\0\0\x20ic07\0\0\xff\xff", 64).is_none());
        assert!(best_icns_png(b"\x89PNG", 64).is_none());
        let _ = best_icns_png(&file[..file.len() / 2], 64);
    }

    #[test]
    fn icon_png_is_a_64_pixel_png_from_the_named_or_default_icns() {
        let root = tempfile::tempdir().unwrap();
        let named = app(root.path(), "Named.app", "com.example.named");
        std::fs::write(
            named.join("Contents/Resources/Icon.icns"),
            icns(&[(b"ic07", png(128))]),
        )
        .unwrap();
        let fallback = root.path().join("Fallback.app");
        std::fs::create_dir_all(fallback.join("Contents/Resources")).unwrap();
        std::fs::write(
            fallback.join("Contents/Info.plist"),
            plist("com.example.fallback", None),
        )
        .unwrap();
        std::fs::write(
            fallback.join("Contents/Resources/AppIcon.icns"),
            icns(&[(b"ic11", png(64))]),
        )
        .unwrap();
        for path in [named, fallback] {
            let icon = icon_png(&path).expect("icon");
            let decoded = image::load_from_memory(&icon).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (64, 64));
        }
        let bare = app(root.path(), "Bare.app", "com.example.bare");
        assert!(icon_png(&bare).is_none());
    }

    fn dock_app(id: &str, running: bool, icon: Option<Vec<u8>>) -> serde_json::Value {
        use base64::Engine;
        let mut app =
            serde_json::json!({"id": id, "name": format!("App {id}"), "running": running});
        if let Some(icon) = icon {
            app["iconPNG"] = base64::engine::general_purpose::STANDARD
                .encode(icon)
                .into();
        }
        app
    }

    /// The protocol contract of `GET /api/v1/computer/dock`: shape, caps and icon size.
    #[test]
    fn dock_answers_are_validated_and_bounded() {
        let mut pinned: Vec<_> = (0..80)
            .map(|n| dock_app(&format!("pin.{n}"), n % 2 == 0, Some(png(20))))
            .collect();
        pinned.insert(1, dock_app("bad id!", false, None));
        pinned.insert(2, dock_app("pin.0", true, None));
        let mut recent: Vec<_> = (0..20)
            .map(|n| dock_app(&format!("recent.{n}"), true, None))
            .collect();
        // A pinned app is never also recent; junk icons are dropped, not trusted.
        recent.insert(0, dock_app("pin.3", true, None));
        recent.insert(1, dock_app("junk.icon", true, Some(b"not a png".to_vec())));
        recent.insert(2, dock_app("huge.icon", true, Some(png(900))));
        let dock = from_helper(
            &serde_json::json!({"available": true, "pinned": pinned, "recent": recent}),
        )
        .unwrap();
        assert_eq!(dock.pinned.len(), MAX_LISTED);
        assert_eq!(dock.recent.len(), MAX_RECENT);
        assert_eq!(dock.pinned[0].id, "pin.0");
        assert_eq!(
            dock.pinned[1].id, "pin.1",
            "invalid and duplicate IDs are dropped"
        );
        assert_eq!(dock.recent[0].id, "junk.icon");
        assert!(dock.recent[0].icon.is_none());
        assert!(dock
            .find("pin.3")
            .is_some_and(|app| app.name == "App pin.3"));
        assert!(!dock.recent.iter().any(|app| app.id == "pin.3"));
        for app in dock.pinned.iter().chain(&dock.recent) {
            assert!(valid_bundle_id(&app.id) && !app.name.is_empty());
            if let Some(icon) = &app.icon {
                assert!(icon.len() <= MAX_ICON_BYTES);
                let decoded = image::load_from_memory(icon).unwrap();
                assert_eq!(
                    (decoded.width(), decoded.height()),
                    (ICON_PIXELS, ICON_PIXELS)
                );
            }
        }
        // The wire shape: exactly these keys, and iconURL only with an icon.
        let wire = dock.to_json();
        assert_eq!(
            wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["available", "pinned", "recent"]
        );
        let first = wire["pinned"][0].as_object().unwrap();
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            ["iconURL", "id", "name", "running"]
        );
        assert_eq!(first["iconURL"], "/api/v1/computer/apps/icon?id=pin.0");
        assert_eq!(first["running"], true);
        assert!(wire["pinned"][1]["running"] == false);
        assert!(wire["recent"][0].get("iconURL").is_none());
        assert!(wire["pinned"]
            .as_array()
            .unwrap()
            .iter()
            .all(|app| app["iconURL"].is_string()));
        // Malformed answers are refused whole rather than guessed at.
        for bad in [
            serde_json::json!({"pinned": [], "recent": []}),
            serde_json::json!({"available": true, "pinned": [{"id": "a.b", "name": "A"}], "recent": []}),
            serde_json::json!({"available": true, "pinned": {}, "recent": []}),
        ] {
            assert!(from_helper(&bad).is_none());
        }
        let off = from_helper(&serde_json::json!({"available": false, "pinned": [], "recent": []}))
            .unwrap();
        assert!(!off.available && off.to_json()["pinned"] == serde_json::json!([]));
    }

    const DOCK_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
	<key>persistent-apps</key>
	<array>
		<dict><key>tile-data</key><dict>
			<key>bundle-identifier</key>
			<string>com.apple.finder</string>
			<key>file-data</key><dict><key>_CFURLString</key><string>file:///System/Library/CoreServices/Finder.app/</string><key>_CFURLStringType</key><integer>15</integer></dict>
			<key>file-label</key>
			<string>Finder</string>
		</dict><key>tile-type</key><string>file-tile</string></dict>
		<dict><key>tile-data</key><dict></dict><key>tile-type</key><string>spacer-tile</string></dict>
		<dict><key>tile-data</key><dict>
			<key>bundle-identifier</key>
			<string>com.google.Chrome</string>
			<key>dock-extra</key><array><string>x</string></array>
			<key>file-data</key><dict><key>_CFURLString</key><string>file:///Applications/Google%20Chrome.app/</string></dict>
			<key>file-label</key>
			<string>Google &amp; Chrome</string>
		</dict></dict>
	</array>
	<key>recent-apps</key>
	<array>
		<dict><key>tile-data</key><dict><key>bundle-identifier</key><string>com.apple.Notes</string><key>file-label</key><string>Notes</string></dict></dict>
	</array>
	<key>show-recents</key>
	<false/>
</dict></plist>"#;

    #[test]
    fn dock_preferences_give_pinned_and_recent_tiles() {
        let pinned = dock_tiles(DOCK_PLIST, "persistent-apps");
        assert_eq!(
            pinned,
            [
                DockTile {
                    id: "com.apple.finder".into(),
                    label: Some("Finder".into()),
                    path: Some("/System/Library/CoreServices/Finder.app".into()),
                },
                DockTile {
                    id: "com.google.Chrome".into(),
                    label: Some("Google & Chrome".into()),
                    path: Some("/Applications/Google Chrome.app".into()),
                },
            ]
        );
        let recent = dock_tiles(DOCK_PLIST, "recent-apps");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, "com.apple.Notes");
        assert!(!dock_shows_recents(DOCK_PLIST));
        assert!(dock_shows_recents("<plist><dict></dict></plist>"));
        assert!(dock_tiles(DOCK_PLIST, "persistent-others").is_empty());
        assert_eq!(file_url_path("file:///a/../b"), None);
        assert_eq!(file_url_path("http://x"), None);
    }

    #[test]
    fn lsappinfo_lists_foreground_apps_only() {
        let text = " 1) \"loginwindow\" ASN:0x0-0x1001:\n    bundleID=\"com.apple.loginwindow\"\n    bundle path=\"/System/Library/CoreServices/loginwindow.app\"\n    pid = 6 !signalled type=\"BackgroundOnly\" flavor=3\n\n 2) \"Finder\" ASN:0x0-0x2002:\n    bundleID=\"com.apple.finder\"\n    bundle path=\"/System/Library/CoreServices/Finder.app\"\n    pid = 7 type=\"Foreground\" flavor=3\n\n 3) \"Safari\" ASN:0x0-0x3003:\n    bundleID=\"com.apple.Safari\"\n    pid = 8 type=\"Foreground\" flavor=3\n\n 4) \"Control Center\" ASN:0x0-0x4004:\n    bundleID=\"com.apple.controlcenter\"\n    bundle path=\"/System/Library/CoreServices/ControlCenter.app\"\n    pid = 9 type=\"UIElement\" flavor=3\n";
        let apps = parse_lsappinfo(text);
        assert_eq!(
            apps.iter()
                .map(|a| (a.id.as_str(), a.name.as_str()))
                .collect::<Vec<_>>(),
            [
                ("com.apple.finder", "Finder"),
                ("com.apple.Safari", "Safari")
            ]
        );
        assert_eq!(apps[1].path, None);
    }

    /// Prints this Mac's real Dock as the route would answer (run by hand).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "reads the live Dock"]
    fn live_mac_dock_prints_json() {
        println!("{}", read_mac_dock().to_json());
    }
}
