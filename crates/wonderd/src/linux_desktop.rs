//! Resolve a shared desktop at launch, not at service installation. A lingering
//! user service outlives graphical logins (and their display/authentication).
//! logind chooses the owner's active local session; the user manager carries
//! the environment imported by that desktop. Never mutate the daemon's global
//! environment, or copy provider credentials from the manager to a helper.
use std::{collections::BTreeMap, ffi::OsString, time::Duration};

type Environment = BTreeMap<OsString, OsString>;

pub const SESSION_KEYS: &[&str] = &[
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_ID",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// No refresh for Cupola's private Xvfb: it has its own display and cookie.
pub async fn refresh(mut env: Environment) -> Environment {
    let Some(display) = env.get(std::ffi::OsStr::new("WONDER_COMPUTER_DISPLAY")) else {
        return env;
    };
    if !display.to_string_lossy().starts_with(':') {
        return env;
    }
    match tokio::time::timeout(Duration::from_secs(2), active_environment()).await {
        Ok(Some(active)) if !active.is_empty() => replace_session(&mut env, active),
        _ => {
            // No usable graphical login also includes a successful but empty
            // logind lookup. The display and cookie belong to the owner's
            // explicit WONDER_COMPUTER_DISPLAY choice, so they stay in that
            // case, just as when logind is unavailable. Only the markers of a
            // saved login go: a retry must not treat a leftover Wayland or
            // x11 login as the current desktop after logout.
            let graphical = env
                .get(std::ffi::OsStr::new("XDG_SESSION_TYPE"))
                .is_some_and(|kind| kind == "wayland" || kind == "x11");
            if graphical {
                env.remove(std::ffi::OsStr::new("XDG_SESSION_TYPE"));
            }
            env.remove(std::ffi::OsStr::new("XDG_SESSION_ID"));
            env.remove(std::ffi::OsStr::new("WAYLAND_DISPLAY"));
        }
    }
    env
}

fn replace_session(env: &mut Environment, active: Environment) {
    for name in SESSION_KEYS {
        env.remove(std::ffi::OsStr::new(name));
    }
    env.extend(active);
}

async fn output(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    (output.status.success() && output.stdout.len() <= 256 * 1024)
        .then(|| String::from_utf8(output.stdout).ok())?
}

fn properties(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .collect()
}

async fn active_environment() -> Option<Environment> {
    let uid = unsafe { libc::getuid() }.to_string();
    let id = output(
        "loginctl",
        &["show-user", &uid, "--property=Display", "--value"],
    )
    .await?;
    let id = id.trim();
    if id.is_empty() {
        // A user manager can run before login and after logout. Its imported
        // environment may still name the previous desktop at both boundaries.
        return Some(Environment::new());
    }
    let session = output(
        "loginctl",
        &[
            "show-session",
            id,
            "--property=User",
            "--property=Active",
            "--property=Remote",
            "--property=Type",
            "--property=Class",
            "--property=Display",
        ],
    )
    .await?;
    // Read the manager's raw string array, not show-environment's shell quoting.
    let manager = output(
        "busctl",
        &[
            "--user",
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "Environment",
        ],
    )
    .await?;
    let manager: serde_json::Value = serde_json::from_str(&manager).ok()?;
    let manager: BTreeMap<_, _> = manager["data"]
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str()?.split_once('='))
        .collect();
    // A logout/switch during the awaits must not apply the old session.
    let still = output(
        "loginctl",
        &["show-user", &uid, "--property=Display", "--value"],
    )
    .await?;
    if still.trim() != id {
        return Some(Environment::new());
    }
    Some(session_environment(
        &uid,
        id,
        &properties(&session),
        &manager,
    ))
}

fn session_environment(
    uid: &str,
    id: &str,
    session: &BTreeMap<&str, &str>,
    manager: &BTreeMap<&str, &str>,
) -> Environment {
    let mut env = Environment::new();
    if session.get("User") != Some(&uid)
        || session.get("Active") != Some(&"yes")
        || session.get("Remote") != Some(&"no")
        || session.get("Class") != Some(&"user")
    {
        return env;
    }
    let kind = session.get("Type").copied().unwrap_or("");
    if !matches!(kind, "x11" | "wayland") {
        return env;
    }
    env.insert("XDG_SESSION_TYPE".into(), kind.into());
    env.insert("XDG_SESSION_ID".into(), id.into());
    env.insert("XDG_RUNTIME_DIR".into(), format!("/run/user/{uid}").into());
    env.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path=/run/user/{uid}/bus").into(),
    );
    if kind == "x11" {
        // Some login managers register the session before assigning its X
        // address, so logind's optional Display stays empty. Xsession imports
        // the address/cookie into the user manager once the server is ready.
        let display = session
            .get("Display")
            .copied()
            .filter(|v| !v.is_empty())
            .or_else(|| manager.get("DISPLAY").copied())
            .unwrap_or("");
        if display.starts_with(':') {
            env.insert("DISPLAY".into(), display.into());
            // Only take a cookie from the manager for this display. Wayland's
            // stale Xwayland authority and WAYLAND_DISPLAY must not survive.
            if manager.get("DISPLAY") == Some(&display) {
                if let Some(authority) = manager.get("XAUTHORITY").filter(|v| !v.is_empty()) {
                    env.insert("XAUTHORITY".into(), (*authority).into());
                }
            }
        }
    } else if let Some(display) = manager.get("WAYLAND_DISPLAY").filter(|v| !v.is_empty()) {
        env.insert("WAYLAND_DISPLAY".into(), (*display).into());
    }
    // Wayland intentionally has no DISPLAY: Xwayland is only a partial desktop.
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_sessions_replaces_all_stale_values_without_copying_secrets() {
        let session =
            properties("User=1000\nActive=yes\nRemote=no\nClass=user\nType=x11\nDisplay=:2");
        let manager = properties("DISPLAY=:2\nXAUTHORITY=/run/user/1000/new cookie\nWAYLAND_DISPLAY=wayland-0\nXDG_SESSION_TYPE=wayland\nSECRET=keep-private");
        let mut env = Environment::from([
            ("WONDER_COMPUTER_DISPLAY".into(), ":0".into()),
            ("XDG_SESSION_TYPE".into(), "wayland".into()),
            ("WAYLAND_DISPLAY".into(), "wayland-0".into()),
            ("XAUTHORITY".into(), "/old/cookie".into()),
        ]);
        replace_session(
            &mut env,
            session_environment("1000", "2", &session, &manager),
        );
        assert_eq!(env[std::ffi::OsStr::new("DISPLAY")], ":2");
        assert_eq!(env[std::ffi::OsStr::new("XDG_SESSION_TYPE")], "x11");
        assert_eq!(
            env[std::ffi::OsStr::new("XAUTHORITY")],
            "/run/user/1000/new cookie"
        );
        assert!(!env.contains_key(std::ffi::OsStr::new("WAYLAND_DISPLAY")));
        assert!(!env.contains_key(std::ffi::OsStr::new("SECRET")));
        for (key, value) in [
            ("Type", "wayland"),
            ("Active", "no"),
            ("Remote", "yes"),
            ("User", "1001"),
            ("Class", "greeter"),
        ] {
            let mut switched = session.clone();
            switched.insert(key, value);
            replace_session(
                &mut env,
                session_environment("1000", "3", &switched, &manager),
            );
            assert!(
                !env.contains_key(std::ffi::OsStr::new("DISPLAY")),
                "{key}={value}"
            );
            assert!(
                !env.contains_key(std::ffi::OsStr::new("XAUTHORITY")),
                "{key}={value}"
            );
        }
        let foreign_cookie = properties("DISPLAY=:3\nXAUTHORITY=/other/cookie");
        let env = session_environment("1000", "2", &session, &foreign_cookie);
        assert!(!env.contains_key(std::ffi::OsStr::new("XAUTHORITY")));
        let mut early_login = session.clone();
        early_login.insert("Display", "");
        let env = session_environment("1000", "2", &early_login, &manager);
        assert_eq!(env[std::ffi::OsStr::new("DISPLAY")], ":2");
        assert_eq!(
            env[std::ffi::OsStr::new("XAUTHORITY")],
            "/run/user/1000/new cookie"
        );
        assert!(!env.contains_key(std::ffi::OsStr::new("WAYLAND_DISPLAY")));
    }

    #[tokio::test]
    async fn private_display_keeps_its_own_authority() {
        let private = Environment::from([
            ("DISPLAY".into(), ":1".into()),
            ("XAUTHORITY".into(), "/cupola/cookie".into()),
        ]);
        assert_eq!(refresh(private.clone()).await, private);
    }

    #[tokio::test]
    async fn missing_graphical_session_preserves_manual_servers() {
        const CHILD: &str = "WONDER_TEST_EMPTY_LOGIND";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;

            // Exercise production discovery with a successful, empty logind
            // reply. Isolate PATH in a child so parallel tests keep their env.
            let fixture = tempfile::tempdir().unwrap();
            let loginctl = fixture.path().join("loginctl");
            std::fs::write(&loginctl, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&loginctl, std::fs::Permissions::from_mode(0o755)).unwrap();
            let output = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "linux_desktop::tests::missing_graphical_session_preserves_manual_servers",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("PATH", fixture.path())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("refresh fixture exceeded its hang guard")
            .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let manual = Environment::from([
            ("WONDER_COMPUTER_DISPLAY".into(), ":99".into()),
            ("DISPLAY".into(), ":99".into()),
            ("XAUTHORITY".into(), "/manual/cookie".into()),
            ("KEEP_UNRELATED".into(), "unchanged".into()),
        ]);
        assert_eq!(refresh(manual.clone()).await, manual);
        let mut tty = manual.clone();
        tty.insert("XDG_SESSION_TYPE".into(), "tty".into());
        assert_eq!(refresh(tty.clone()).await, tty);

        // The same empty reply must keep the chosen server, even when the
        // service inherited a graphical login's markers, but retire those
        // markers so a retry cannot treat the saved login as current.
        for (key, value) in [
            ("XDG_SESSION_TYPE", "x11"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ] {
            let mut graphical = manual.clone();
            graphical.insert(key.into(), value.into());
            graphical.insert("XDG_SESSION_ID".into(), "2".into());
            let refreshed = refresh(graphical).await;
            assert_eq!(refreshed, manual, "{key}={value}");
        }
    }
}
