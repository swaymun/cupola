//! The service carries only host launch configuration and the owner's selected
//! provider settings. Values live in the private launch-environment.json file.
use serde_json::{Map, Value};
use wonder_app_server::provider_environment::{selected, SELECTION};

const HOST: &[&str] = &[
    "PATH",
    "TAILSCALE_BIN",
    "TAILSCALE_SOCKET",
    "WONDER_CODEX_BIN",
    "WONDER_CLAUDE_BRIDGE",
    "WONDER_OPENCODE_BRIDGE",
    "WONDER_CURSOR_BRIDGE",
    "WONDER_OPENCODE_BIN",
    "WONDER_CURSOR_BIN",
    "WONDER_NODE_BIN",
    "WONDER_DAEMON_BIN",
    "WONDER_COMPUTER_PYTHON",
    "WONDER_COMPUTER_BACKEND",
    // Persist the sharing choice, not the installing login's transient display,
    // cookie or Wayland markers. Those are resolved on each start and retry.
    "WONDER_COMPUTER_DISPLAY",
];

// Accept old private launch files during upgrades; new installs omit these.
const LEGACY_SESSION: &[&str] = &["XAUTHORITY", "XDG_SESSION_TYPE", "WAYLAND_DISPLAY"];

pub fn collect(
    source: impl IntoIterator<Item = (String, String)>,
    data: &std::path::Path,
) -> Map<String, Value> {
    let source: std::collections::BTreeMap<_, _> = source.into_iter().collect();
    let mut env: Map<String, Value> = selected(&source)
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect();
    // launchd/systemd do not start in the installing shell's working folder.
    if let Ok(cwd) = std::env::current_dir() {
        for key in [
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "CURSOR_CONFIG_DIR",
            "CURSOR_DATA_DIR",
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_DIR",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "NODE_EXTRA_CA_CERTS",
        ] {
            if let Some(Value::String(value)) = env.get_mut(key) {
                if !value.is_empty() && std::path::Path::new(value).is_relative() {
                    *value = cwd.join(&*value).to_string_lossy().into_owned();
                }
            }
        }
    }
    for key in HOST {
        if let Some(value) = source.get(*key) {
            env.insert((*key).into(), value.clone().into());
        }
    }
    // A manually managed X server (no graphical login, e.g. a container) may
    // have an explicit cookie. Keep that override; logind still replaces it
    // whenever an active desktop exists. Never pin a graphical login's cookie.
    if !source
        .get("XDG_SESSION_TYPE")
        .is_some_and(|kind| matches!(kind.as_str(), "x11" | "wayland"))
        && !source.contains_key("WAYLAND_DISPLAY")
    {
        if let Some(value) = source.get("XAUTHORITY") {
            env.insert("XAUTHORITY".into(), value.clone().into());
        }
    }
    env.insert(
        "WONDER_DATA_DIR".into(),
        data.to_string_lossy().into_owned().into(),
    );
    env
}

pub fn valid(env: &Map<String, Value>) -> bool {
    let selection = env.get(SELECTION).and_then(Value::as_str).unwrap_or("");
    env.iter().all(|(key, value)| {
        value.is_string()
            && (HOST.contains(&key.as_str())
                || LEGACY_SESSION.contains(&key.as_str())
                || key == "WONDER_DATA_DIR"
                || key == SELECTION
                || (wonder_app_server::provider_environment::selectable(key)
                    && selection.split(',').any(|item| item.trim() == key)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        process::Stdio,
        time::Duration,
    };

    #[test]
    fn provider_selection_is_opt_in_and_keeps_relative_config_paths_stable() {
        let data = std::path::Path::new("/fixture/state");
        let env = collect([("ANTHROPIC_API_KEY".into(), "fixture-only".into())], data);
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        let env = collect(
            [
                (SELECTION.into(), "CLAUDE_CONFIG_DIR".into()),
                ("CLAUDE_CONFIG_DIR".into(), "relative-claude".into()),
            ],
            data,
        );
        assert_eq!(
            env["CLAUDE_CONFIG_DIR"],
            std::env::current_dir()
                .unwrap()
                .join("relative-claude")
                .to_str()
                .unwrap()
        );
        assert!(valid(&env));
    }

    // Real child processes launched from the persisted service environment.
    // Scripted provider executables make an HTTP request instead of spending a
    // model call. The runtime's production environment filter is applied too.
    #[cfg(unix)]
    #[tokio::test]
    async fn service_launch_reaches_fixture_endpoints_and_excludes_unselected_secrets() {
        let root = tempfile::tempdir().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let source = [
            (SELECTION, "OPENAI_BASE_URL,ANTHROPIC_BASE_URL,FIXTURE_API_KEY,CUSTOM_PROVIDER_SECRET,HTTPS_PROXY,CLAUDE_CONFIG_DIR,NODE_OPTIONS,TS_AUTHKEY"),
            ("OPENAI_BASE_URL", endpoint.as_str()), ("ANTHROPIC_BASE_URL", endpoint.as_str()),
            ("FIXTURE_API_KEY", "fixture-only"), ("CUSTOM_PROVIDER_SECRET", "custom-key"), ("HTTPS_PROXY", endpoint.as_str()),
            ("CLAUDE_CONFIG_DIR", root.path().to_str().unwrap()),
            ("UNSELECTED_API_KEY", "must-not-leak"), ("NODE_OPTIONS", "must-not-run"), ("TS_AUTHKEY", "must-not-leak"),
        ];
        let environment = collect(
            source.into_iter().map(|(k, v)| (k.into(), v.into())),
            root.path(),
        );
        assert!(valid(&environment));
        assert!(!environment.contains_key("UNSELECTED_API_KEY"));
        assert!(!environment.contains_key("NODE_OPTIONS"));
        assert!(!environment.contains_key("TS_AUTHKEY"));
        let path = root.path().join("launch-environment.json");
        super::super::write_private(&path, &serde_json::to_vec(&environment).unwrap()).unwrap();
        #[cfg(target_os = "macos")]
        let environment = super::super::macos::private_environment(&path).unwrap();
        #[cfg(not(target_os = "macos"))]
        let environment: Map<String, Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let env: Vec<_> = environment
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let unit = super::super::unit_file(
            std::path::Path::new("/fixture/cupola"),
            &root.path().join("log"),
            &env,
        )
        .unwrap();
        assert!(unit.contains("OPENAI_BASE_URL="));
        assert!(!unit.contains("UNSELECTED_API_KEY="));
        for family in ["codex", "claude", "opencode", "cursor"] {
            let mut child = tokio::process::Command::new("/usr/bin/python3");
            child.env_clear().envs(
                wonder_app_server::provider_environment::runtime_environment(
                    family,
                    env.iter().map(|(k, v)| (k.into(), v.into())),
                ),
            );
            child.args(["-c", r#"
import os, sys, urllib.request
assert 'UNSELECTED_API_KEY' not in os.environ
assert 'NODE_OPTIONS' not in os.environ
assert os.environ['FIXTURE_API_KEY'] == 'fixture-only'
assert os.environ['CUSTOM_PROVIDER_SECRET'] == 'custom-key'
assert os.environ['CLAUDE_CONFIG_DIR'] == sys.argv[2]
url = os.environ['ANTHROPIC_BASE_URL' if sys.argv[1] == 'claude' else 'OPENAI_BASE_URL']
assert os.environ['HTTPS_PROXY'] == url
request = urllib.request.Request(url + '/' + sys.argv[1], headers={'Authorization':'Bearer ' + os.environ['FIXTURE_API_KEY']})
assert urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=10).read() == b'ok'
"#, family, root.path().to_str().unwrap()]).kill_on_drop(true).stdout(Stdio::piped()).stderr(Stdio::piped());
            let child = child.spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match server.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("fixture endpoint not reached by {family}: {error}"),
                }
            };
            // accept inherits O_NONBLOCK on macOS. A read timeout cannot
            // make a nonblocking socket wait for the child's HTTP headers.
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = [0; 4096];
            let size = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.contains(&format!("GET /{family} ")));
            assert!(request.contains("Bearer fixture-only"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            drop(socket);
            let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{family}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut tampered = environment;
        tampered.insert("UNSELECTED_API_KEY".into(), "secret".into());
        assert!(!valid(&tampered));
    }
}
