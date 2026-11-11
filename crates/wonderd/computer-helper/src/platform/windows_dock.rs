//! Bounded Windows dock data and launch lookups. Native shortcut resolution,
//! window enumeration and ShellExecute stay in the helper's Win32 module.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MAX_PINNED: usize = 24;
const MAX_RECENT: usize = 8;
const MAX_ICON_BASE64: usize = 3072;

/// The lower-case file name, matching the window list's app IDs.
pub fn exe_name(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .to_lowercase()
}

// Opaque IDs preserve Unicode and distinguish stems such as "A B" and "A-B".
// Hashing also bounds long shortcut names to the daemon's ID limit.
pub fn pin_id(stem: &str) -> String {
    format!("pin-{:x}", Sha256::digest(stem.as_bytes()))
}
pub fn running_id(exe: &str) -> String {
    format!("exe-{:x}", Sha256::digest(exe.to_lowercase().as_bytes()))
}

pub struct Pin {
    pub id: String,
    pub name: String,
    pub program: String,
    /// Open the original shortcut to preserve arguments and working directory.
    pub shortcut: String,
}
pub struct Running {
    pub exe: String,
    pub source_id: String,
    pub icon: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Launch<'a> {
    Shortcut(&'a str),
    Window(&'a str),
}

/// Resolve only against freshly read pins/windows, never interpret an ID as a
/// path or command. A listed running app brings its current window forward.
pub fn launch<'a>(id: &str, pins: &'a [Pin], running: &'a [Running]) -> Option<Launch<'a>> {
    if let Some(pin) = pins.iter().find(|pin| pin.id == id) {
        return Some(
            running
                .iter()
                .find(|app| app.exe == exe_name(&pin.program))
                .map(|app| Launch::Window(app.source_id.as_str()))
                .unwrap_or(Launch::Shortcut(&pin.shortcut)),
        );
    }
    running
        .iter()
        .find(|app| running_id(&app.exe) == id)
        .map(|app| Launch::Window(app.source_id.as_str()))
}

pub fn build(
    pins: &[Pin],
    running: &[Running],
    mut icon: impl FnMut(&str) -> Option<String>,
) -> Value {
    let entry = |id: &str, name: &str, running: bool, icon: Option<String>| {
        let mut app = json!({"id": id, "name": name, "running": running});
        if let Some(icon) = icon.filter(|icon| icon.len() <= MAX_ICON_BASE64) {
            app["iconPNG"] = json!(icon);
        }
        app
    };
    let mut pinned = Vec::new();
    let mut taken = std::collections::HashSet::new();
    let mut pinned_exes = std::collections::HashSet::new();
    for pin in pins {
        if pinned.len() >= MAX_PINNED {
            break;
        }
        if !taken.insert(pin.id.clone()) {
            continue;
        }
        let exe = exe_name(&pin.program);
        let live = running.iter().any(|app| app.exe == exe);
        pinned_exes.insert(exe);
        pinned.push(entry(&pin.id, &pin.name, live, icon(&pin.shortcut)));
    }
    let mut recent = Vec::new();
    for app in running {
        if recent.len() >= MAX_RECENT {
            break;
        }
        if pinned_exes.contains(&app.exe) {
            continue;
        }
        let id = running_id(&app.exe);
        if taken.insert(id.clone()) {
            recent.push(entry(
                &id,
                app.exe.trim_end_matches(".exe"),
                true,
                app.icon.clone(),
            ));
        }
    }
    json!({"available": true, "pinned": pinned, "recent": recent})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_open_programs_become_launchable_bounded_dock() {
        let pin = |name: &str, program: &str| Pin {
            id: pin_id(name),
            name: name.into(),
            program: program.into(),
            shortcut: format!(r"C:\pins\{name}.lnk"),
        };
        let pins = vec![
            pin(
                "Mozilla Firefox",
                r"C:\Program Files\Mozilla Firefox\firefox.exe",
            ),
            pin("Notepad", r"C:\Windows\notepad.exe"),
        ];
        let running = vec![
            Running {
                exe: "code.exe".into(),
                source_id: "app:code.exe#1".into(),
                icon: Some("PNG".into()),
            },
            Running {
                exe: "firefox.exe".into(),
                source_id: "app:firefox.exe#2".into(),
                icon: None,
            },
        ];
        assert_eq!(
            build(&pins, &running, |_| None),
            json!({
                "available": true,
                "pinned": [
                    {"id": pin_id("Mozilla Firefox"), "name": "Mozilla Firefox", "running": true},
                    {"id": pin_id("Notepad"), "name": "Notepad", "running": false},
                ],
                "recent": [{"id": running_id("code.exe"), "name": "code", "running": true, "iconPNG": "PNG"}],
            })
        );
        for (id, expected) in [
            (
                pin_id("Mozilla Firefox"),
                Some(Launch::Window("app:firefox.exe#2")),
            ),
            (
                pin_id("Notepad"),
                Some(Launch::Shortcut(r"C:\pins\Notepad.lnk")),
            ),
            (
                running_id("code.exe"),
                Some(Launch::Window("app:code.exe#1")),
            ),
            ("C:\\Windows\\cmd.exe".into(), None),
            ("--input-owner".into(), None),
            ("exe-stale".into(), None),
        ] {
            assert_eq!(launch(&id, &pins, &running), expected, "{id}");
        }
        assert_ne!(pin_id("A B"), pin_id("A-B"));
        assert_ne!(pin_id("日本語"), pin_id("中文"));
        assert!(pin_id(&"a".repeat(260)).len() <= 150);
        assert_eq!(exe_name(r"C:\A\B\App.EXE"), "app.exe");

        let pins: Vec<_> = (0..40)
            .map(|n| pin(&format!("Pin {n}"), &format!("pin{n}.exe")))
            .collect();
        let running: Vec<_> = (0..40)
            .flat_map(|n| [format!("app{n}.exe"), format!("app{n}.exe")])
            .map(|exe| Running {
                source_id: format!("app:{exe}#1"),
                exe,
                icon: None,
            })
            .collect();
        let answer = build(&pins, &running, |_| Some("x".repeat(MAX_ICON_BASE64 + 1)));
        assert_eq!(answer["pinned"].as_array().unwrap().len(), 24);
        assert_eq!(answer["recent"].as_array().unwrap().len(), 8);
        for app in answer["pinned"]
            .as_array()
            .unwrap()
            .iter()
            .chain(answer["recent"].as_array().unwrap())
        {
            let id = app["id"].as_str().unwrap();
            assert!(
                launch(id, &pins, &running).is_some(),
                "every listed ID resolves"
            );
            assert!(app.get("iconPNG").is_none());
        }
        assert_eq!(answer["recent"][0]["name"], "app0");
        assert_eq!(answer["recent"][1]["name"], "app1");
        assert_eq!(
            build(&[], &[], |_| None),
            json!({"available":true,"pinned":[],"recent":[]})
        );
    }
}
