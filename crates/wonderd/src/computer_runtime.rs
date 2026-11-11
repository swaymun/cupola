//! Native provider tools control the desktop. Wonder's capture helper serves
//! the phone's live viewer; it is not an agent tool implementation.
use super::*;
use serde_json::{json, Value};

/// All providers other than bundled Codex use the host's Cua Driver.
/// Name the disabled driver too, so a resumed runtime drops its previous adapter.
pub(super) async fn configure_project(
    state: &AppState,
    enabled: bool,
    params: &mut Value,
) -> Result<(), String> {
    configure_project_with(state, enabled, installed_cua_driver(), params).await
}

async fn configure_project_with(
    state: &AppState,
    enabled: bool,
    driver: Option<PathBuf>,
    params: &mut Value,
) -> Result<(), String> {
    let enabled = enabled && driver_setting(state).await.unwrap_or(false);
    if !params["config"].is_object() {
        params["config"] = json!({});
    }
    params["config"]["mcp_servers.cua_driver"] = match driver_backend(enabled, driver) {
        ComputerUse::CuaDriver(command) => {
            json!({"command": command.to_string_lossy(), "args": ["mcp"],
            "env": display_environment(), "enabled": true})
        }
        _ => off_server(),
    };
    Ok(())
}

#[derive(Debug, PartialEq)]
pub(crate) enum ComputerUse {
    CuaDriver(PathBuf),
    Desktop(Value),
    Off,
    Missing,
}

impl ComputerUse {
    fn status(&self) -> &'static str {
        match self {
            Self::CuaDriver(_) => "Using Cua Driver",
            Self::Desktop(_) => "Using ChatGPT's computer use",
            Self::Off => "Off",
            Self::Missing => "Cua Driver isn't installed",
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::CuaDriver(_) => "cuaDriver",
            Self::Desktop(_) => "chatGPT",
            Self::Off => "off",
            Self::Missing => "missing",
        }
    }
}

/// A manifest in Codex's cache is not proof that Wonder runs ChatGPT's Codex.
/// Resolve symlinks so a plain CLI that shares CODEX_HOME never inherits it.
fn chatgpt_bundled_codex(binary: &FsPath) -> bool {
    let Ok(binary) = crate::canonical_path::canonicalize(binary) else {
        return false;
    };
    binary.is_file()
        && binary.ancestors().any(|path| {
            (path.file_name().is_some_and(|name| name == "ChatGPT.app")
                && binary.starts_with(path.join("Contents/Resources")))
                || crate::provider_runtime::windows_chatgpt_root(path)
        })
}

const SETTING_UNREADABLE: &str = "Off. Cupola couldn’t read the Cua Driver setting. Try again.";
const SETTING_UNREADABLE_CONTEXT: &str = "Computer use is unavailable for this turn because the host could not read its Cua Driver setting.";
const HOST_DISABLED: &str = "Computer use is off on this computer";

/// One failure policy for listing and all providers: unreadable means Off.
async fn driver_setting(state: &AppState) -> Result<bool, &'static str> {
    if !state.computer_use_enabled {
        return Ok(false);
    }
    state
        .store
        .cua_driver_enabled()
        .await
        .map_err(|_| SETTING_UNREADABLE)
}

fn choose(
    family: AgentFamily,
    bundled_codex: bool,
    enabled: bool,
    desktop: Option<Value>,
    driver: Option<PathBuf>,
) -> ComputerUse {
    if family == AgentFamily::Codex && bundled_codex {
        if let Some(server) = desktop {
            return ComputerUse::Desktop(server);
        }
    }
    driver_backend(enabled, driver)
}

fn driver_backend(enabled: bool, driver: Option<PathBuf>) -> ComputerUse {
    if !enabled {
        return ComputerUse::Off;
    }
    match driver {
        Some(path) => ComputerUse::CuaDriver(path),
        None => ComputerUse::Missing,
    }
}

/// Re-resolve when loading a thread. Already-loaded Codex threads retain their
/// MCP configuration until the runtime restarts. Native use ignores the toggle.
pub(crate) async fn configure_codex(state: &AppState, params: &mut Value) -> Option<&'static str> {
    configure_codex_with(state, installed_cua_driver(), params).await
}

async fn configure_codex_with(
    state: &AppState,
    driver: Option<PathBuf>,
    params: &mut Value,
) -> Option<&'static str> {
    // Without the host capability, leave Codex's own configuration alone.
    // Explicit disabled overrides are needed only while Wonder manages the
    // computer-use choice, so inherited MCPs cannot bypass the driver switch.
    if !state.computer_use_enabled {
        return None;
    }
    let setting = driver_setting(state).await;
    let choice = if setting.is_ok() {
        choose(
            AgentFamily::Codex,
            chatgpt_bundled_codex(&state.projects.codex_bin()),
            setting.unwrap_or(false),
            installed_cua(&state.codex_home).await.ok().flatten(),
            driver,
        )
    } else {
        ComputerUse::Off
    };
    let notice = setting
        .err()
        .map(|_| SETTING_UNREADABLE_CONTEXT)
        .or_else(|| (choice == ComputerUse::Missing).then_some(CUA_DRIVER_MISSING));
    if !params["config"].is_object() {
        params["config"] = json!({});
    }
    for (name, server) in codex_servers(choice, cfg!(windows)) {
        params["config"][format!("mcp_servers.{name}")] = server;
    }
    notice
}

fn off_server() -> Value {
    json!({"command": "/usr/bin/true", "enabled": false})
}

/// Overrides inherited computer servers too, preserving unrelated MCPs. Windows
/// desktop computer use also registers node_repl; leave it only for native use.
fn codex_servers(choice: ComputerUse, windows: bool) -> Vec<(&'static str, Value)> {
    let (driver, desktop) = match choice {
        ComputerUse::Missing | ComputerUse::Off => (off_server(), off_server()),
        ComputerUse::CuaDriver(path) => (cua_driver_server(&path), off_server()),
        ComputerUse::Desktop(server) => {
            return vec![("cua_driver", off_server()), ("cua_repl", server)]
        }
    };
    let mut servers = vec![("cua_driver", driver), ("cua_repl", desktop)];
    if windows {
        servers.push(("node_repl", off_server()));
    }
    servers
}

/// Turn context for a computer without Cua Driver.
pub(crate) const CUA_DRIVER_MISSING: &str = "Computer use is not set up on this computer: \
Cua Driver is not installed. If the person asks you to use apps, windows or the screen, \
say so and tell them to install Cua Driver (https://cua.ai/docs/cua-driver/quickstart), \
then run `cupola doctor` to check it.";

/// The `additionalContext` key of that notice on Codex turns.
pub(crate) const CUA_DRIVER_CONTEXT_KEY: &str = "wonder_computer_use";

/// Codex starts MCP servers with a minimal environment; the driver also
/// needs the session's display (the virtual screen on Linux).
fn cua_driver_server(path: &FsPath) -> Value {
    json!({"command": path.to_string_lossy(), "args": ["mcp"], "enabled": true,
        "env_vars": ["DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]})
}

/// The X display the phone views (`wonder init --computer` gives wonderd its
/// Xvfb display). Agent runtimes pass providers a filtered environment, so the
/// driver is told the display explicitly and acts where the phone looks.
fn display_environment() -> serde_json::Value {
    let mut env = serde_json::Map::new();
    if cfg!(target_os = "linux") {
        for name in ["DISPLAY", "XAUTHORITY"] {
            if let Some(value) = std::env::var_os(name).and_then(|v| v.into_string().ok()) {
                env.insert(name.into(), value.into());
            }
        }
    }
    serde_json::Value::Object(env)
}

/// The installed `cua-driver`: WONDER_CUA_DRIVER_BIN, then PATH, then where
/// Cua's installers put it (`~/.local/bin` on macOS and Linux, the app bundle
/// on macOS, `%LOCALAPPDATA%\Programs\Cua\cua-driver\bin` on Windows). A
/// service's PATH often lacks `~/.local/bin`, hence the fixed locations.
pub fn installed_cua_driver() -> Option<PathBuf> {
    find_cua_driver(|name| std::env::var_os(name))
}

fn find_cua_driver(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "cua-driver.exe"
    } else {
        "cua-driver"
    };
    let explicit = env("WONDER_CUA_DRIVER_BIN").map(PathBuf::from);
    let on_path = env("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let installed = if cfg!(windows) {
        env("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Programs/Cua/cua-driver/bin/cua-driver.exe"))
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        let mut paths = env("HOME")
            .map(|home| PathBuf::from(home).join(".local/bin/cua-driver"))
            .into_iter()
            .collect::<Vec<_>>();
        if cfg!(target_os = "macos") {
            paths.push("/Applications/CuaDriver.app/Contents/MacOS/cua-driver".into());
        }
        paths
    };
    explicit
        .into_iter()
        .chain(on_path)
        .chain(installed)
        .find(|path| path.is_file())
}

async fn computer_use_listing(state: &AppState) -> Response {
    computer_use_listing_with(state, installed_cua_driver()).await
}

async fn computer_use_listing_with(state: &AppState, driver: Option<PathBuf>) -> Response {
    let setting = driver_setting(state).await;
    let enabled = setting.unwrap_or(false);
    let desktop = installed_cua(&state.codex_home).await.ok().flatten();
    let bundled = chatgpt_bundled_codex(&state.projects.codex_bin());
    let native = state.computer_use_enabled && setting.is_ok() && bundled && desktop.is_some();
    let providers: Vec<_> = AgentFamily::ALL
        .into_iter()
        .map(|family| {
            let backend = if state.computer_use_enabled && setting.is_ok() {
                choose(family, bundled, enabled, desktop.clone(), driver.clone())
            } else {
                ComputerUse::Off
            };
            let status = if !state.computer_use_enabled {
                HOST_DISABLED
            } else {
                setting.err().unwrap_or(backend.status())
            };
            json!({"family": family, "inUse": backend.name(), "status": status})
        })
        .collect();
    let status = if !state.computer_use_enabled {
        HOST_DISABLED
    } else {
        setting
            .err()
            .unwrap_or(driver_backend(enabled, driver.clone()).status())
    };
    Json(json!({"computerUseAvailable": state.computer_use_enabled,
        "cuaDriverEnabled": enabled, "cuaDriverInstalled": driver.is_some(),
        "cuaDriverStatus": status, "codexUsesChatGPT": native, "providers": providers}))
    .into_response()
}

pub(crate) async fn computer_use(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    computer_use_listing(&state).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetComputerUse {
    cua_driver_enabled: bool,
}

pub(crate) async fn set_computer_use(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Json(request): Json<SetComputerUse>,
) -> Response {
    if state
        .store
        .set_cua_driver_enabled(request.cua_driver_enabled, &projects::now_text())
        .await
        .is_err()
    {
        return projects::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The setting could not be saved. Try again.",
        );
    }
    computer_use_listing(&state).await
}

async fn installed_cua(home: &FsPath) -> Result<Option<serde_json::Value>, String> {
    let root = home.join("plugins/cache/openai-bundled/unified-computer-use");
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Codex Computer Use could not be loaded: {error}")),
    };
    let mut versions = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(version) = name
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
        {
            versions.push((version, entry.path()));
        }
    }
    versions.sort_by(|a, b| b.0.cmp(&a.0));
    let Some((_, path)) = versions.first() else {
        return Ok(None);
    };
    let manifest = tokio::fs::read(path.join(".mcp.json"))
        .await
        .map_err(|e| e.to_string())?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest).map_err(|e| e.to_string())?;
    let mut server = manifest
        .pointer("/mcpServers/cua_repl")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or("The installed Codex Computer Use manifest is incompatible.")?;
    // The plugin cache outlives the desktop app: once ChatGPT (or the
    // plugin's runtime) is removed, its manifest names files that are gone.
    // Treat that as not installed, so automatic selection falls back to Cua
    // Driver (or the `wonder doctor` notice) instead of a server that cannot
    // start.
    if !manifest_runnable(&server, path) {
        return Ok(None);
    }
    server["enabled"] = serde_json::json!(true);
    Ok(Some(server))
}

/// Whether the files a manifest's server starts from still exist: its
/// command (an absolute path, a path relative to its `cwd` or the plugin, or
/// a name on PATH) and any absolute paths among its arguments.
fn manifest_runnable(server: &Value, plugin: &FsPath) -> bool {
    let Some(command) = server["command"].as_str().filter(|c| !c.is_empty()) else {
        return false;
    };
    let base = server["cwd"]
        .as_str()
        .map(|cwd| plugin.join(cwd))
        .unwrap_or_else(|| plugin.to_path_buf());
    let command = FsPath::new(command);
    let command_exists = if command.is_absolute() || command.components().count() > 1 {
        base.join(command).is_file()
    } else {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join(command).is_file()
                    || (cfg!(windows) && dir.join(command).with_extension("exe").is_file())
            })
        })
    };
    command_exists
        && server["args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(FsPath::new)
            .filter(|arg| arg.is_absolute())
            .all(FsPath::exists)
}

pub(super) fn turn_ended(state: &AppState, params: &serde_json::Value) {
    let (Some(thread), Some(turn), Some(runtime)) = (
        params.get("threadId").and_then(serde_json::Value::as_str),
        params
            .pointer("/turn/id")
            .or_else(|| params.get("turnId"))
            .and_then(serde_json::Value::as_str),
        params
            .get("_wonderRuntimeId")
            .and_then(serde_json::Value::as_str),
    ) else {
        return;
    };
    let (thread, turn, runtime) = (thread.to_owned(), turn.to_owned(), runtime.to_owned());
    let state = state.clone();
    // Reuse the native plugin's Stop hook. Do not block durable ingestion or
    // touch a replacement runtime if the daemon restarted during completion.
    tokio::spawn(async move {
        let rpc = state.projects.codex.lock().await.rpc();
        if rpc.health().id() != runtime {
            return;
        }
        let result = rpc
            .request(
                "mcpServer/tool/call",
                serde_json::json!({
                    "threadId": thread, "server":"cua_repl", "tool":"turn_ended",
                    "arguments":{"hook_event_name":"Stop", "session_id":thread, "turn_id":turn}
                }),
            )
            .await;
        // An unavailable server has no native session to release. The helper
        // itself handles idempotency for repeated terminal notifications.
        if let Err(error) = result {
            let _ = state.logger.record(
                "warn",
                "native_computer_cleanup_failed",
                serde_json::json!({"detail":error.to_string()}),
            );
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Writes the desktop app's plugin manifest under `home`, naming a
    /// runtime at `command`, and returns the plugin folder.
    async fn desktop_plugin(home: &FsPath, version: &str, command: &FsPath) -> PathBuf {
        let plugin = home
            .join("plugins/cache/openai-bundled/unified-computer-use")
            .join(version);
        tokio::fs::create_dir_all(&plugin).await.unwrap();
        let server = json!({"command": command.to_string_lossy(), "args": [version]});
        tokio::fs::write(
            plugin.join(".mcp.json"),
            json!({"mcpServers": {"cua_repl": server}}).to_string(),
        )
        .await
        .unwrap();
        plugin
    }

    // Contract: non-Codex providers receive only Cua Driver, including on
    // macOS, and every turn replaces a previous on/off configuration.
    #[tokio::test]
    async fn project_turn_observes_toggle_and_never_gets_desktop_tools() {
        let (dir, mut state) = ingestion::tests::fixture().await;
        state.codex_home = dir.path().join("codex-home");
        state.computer_use_enabled = true;
        let node = dir.path().join("node");
        std::fs::write(&node, "").unwrap();
        desktop_plugin(&state.codex_home, "26.10.1", &node).await;
        let driver = Some(PathBuf::from("/installed/cua-driver"));
        let mut params = json!({});
        for (installed, enabled, allowed, expected) in [
            (true, true, true, true),
            (true, false, true, false),
            (true, true, false, false),
            (false, true, true, false),
            (true, true, true, true),
        ] {
            state
                .store
                .set_cua_driver_enabled(enabled, "now")
                .await
                .unwrap();
            configure_project_with(
                &state,
                allowed,
                driver.clone().filter(|_| installed),
                &mut params,
            )
            .await
            .unwrap();
            assert_eq!(
                params["config"]["mcp_servers.cua_driver"]["enabled"],
                expected
            );
            assert!(params["config"].get("mcp_servers.cua_repl").is_none());
        }
        state.computer_use_enabled = false;
        configure_project_with(&state, true, driver, &mut params)
            .await
            .unwrap();
        assert_eq!(params["config"]["mcp_servers.cua_driver"]["enabled"], false);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Cua Driver is found where its installers put it, even when a service's
    // PATH lacks it; an explicit path wins.
    #[test]
    fn cua_driver_is_found_where_installed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = if cfg!(windows) {
            "cua-driver.exe"
        } else {
            "cua-driver"
        };
        let (explicit, on_path) = (dir.path().join("explicit"), dir.path().join("path"));
        let installed = if cfg!(windows) {
            dir.path().join("Programs/Cua/cua-driver/bin")
        } else {
            dir.path().join(".local/bin")
        };
        for folder in [&explicit, &on_path, &installed] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join(exe), "").unwrap();
        }
        let home = if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        };
        let env = |vars: Vec<(&'static str, std::path::PathBuf)>| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone().into_os_string())
            }
        };
        let all = vec![
            ("WONDER_CUA_DRIVER_BIN", explicit.join(exe)),
            ("PATH", on_path.clone()),
            (home, dir.path().to_path_buf()),
        ];
        assert_eq!(find_cua_driver(env(all.clone())), Some(explicit.join(exe)));
        assert_eq!(
            find_cua_driver(env(all[1..].to_vec())),
            Some(on_path.join(exe))
        );
        assert_eq!(
            find_cua_driver(env(all[2..].to_vec())),
            Some(installed.join(exe))
        );
        let missing = dir.path().join("missing");
        if !cfg!(target_os = "macos") {
            assert_eq!(find_cua_driver(env(vec![(home, missing)])), None);
        }
    }

    // Contract: native tools require ChatGPT's bundled Codex and a runnable
    // desktop manifest; every other provider obeys the computer-wide switch.
    #[test]
    fn automatic_selection_truth_table() {
        // bundled Codex, desktop runtime available, toggle, driver installed,
        // literal expected backend. No expected values are computed by choose().
        for (bundled, desktop, enabled, installed, expected) in [
            (false, false, false, false, "off"),
            (false, false, false, true, "off"),
            (false, false, true, false, "missing"),
            (false, false, true, true, "cuaDriver"),
            (false, true, false, false, "off"),
            (false, true, false, true, "off"),
            (false, true, true, false, "missing"),
            (false, true, true, true, "cuaDriver"),
            (true, false, false, false, "off"),
            (true, false, false, true, "off"),
            (true, false, true, false, "missing"),
            (true, false, true, true, "cuaDriver"),
            (true, true, false, false, "chatGPT"),
            (true, true, false, true, "chatGPT"),
            (true, true, true, false, "chatGPT"),
            (true, true, true, true, "chatGPT"),
        ] {
            assert_eq!(choose(AgentFamily::Codex, bundled, enabled,
                desktop.then(|| json!({"command": "native"})),
                installed.then(|| PathBuf::from("/cua-driver"))).name(), expected,
                "Codex bundled={bundled}, desktop={desktop}, enabled={enabled}, installed={installed}");
        }
        for family in [
            AgentFamily::Claude,
            AgentFamily::OpenCode,
            AgentFamily::Cursor,
        ] {
            for bundled in [false, true] {
                for desktop in [false, true] {
                    for (enabled, installed, expected) in [
                        (false, false, "off"),
                        (false, true, "off"),
                        (true, false, "missing"),
                        (true, true, "cuaDriver"),
                    ] {
                        assert_eq!(choose(family, bundled, enabled,
                            desktop.then(|| json!({"command": "native"})),
                            installed.then(|| PathBuf::from("/cua-driver"))).name(), expected,
                            "{family:?}: bundled={bundled}, desktop={desktop}, enabled={enabled}, installed={installed}");
                    }
                }
            }
        }
    }

    async fn response_json(response: Response) -> Value {
        assert_eq!(response.status(), StatusCode::OK);
        let value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        crate::tests::validate_http_contract("computerUseSettings", &value);
        value
    }

    #[tokio::test]
    async fn codex_turn_and_host_listing_resolve_the_running_binary_and_saved_toggle() {
        let (dir, mut state) = ingestion::tests::fixture().await;
        state.codex_home = dir.path().join("codex-home");
        state.computer_use_enabled = true;
        let node = dir.path().join("ChatGPT.app/Contents/Resources/node");
        let binary = node.with_file_name("codex");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, "").unwrap();
        std::fs::write(&binary, "").unwrap();
        desktop_plugin(&state.codex_home, "26.10.1", &node).await;
        let plain = state.projects.codex_bin();
        let driver = Some(PathBuf::from("/installed/cua-driver"));
        for bundled in [true, false] {
            state.projects.set_codex_bin(if bundled {
                binary.clone()
            } else {
                plain.clone()
            });
            for enabled in [true, false, true] {
                state
                    .store
                    .set_cua_driver_enabled(enabled, "now")
                    .await
                    .unwrap();
                for installed in [true, false] {
                    let driver = driver.clone().filter(|_| installed);
                    let mut start = json!({});
                    let notice = configure_codex_with(&state, driver.clone(), &mut start).await;
                    let expected = if bundled {
                        "cua_repl"
                    } else if enabled && installed {
                        "cua_driver"
                    } else {
                        ""
                    };
                    let active = enabled_after_merge(
                        &json!({"cua_repl": {"enabled": true}, "cua_driver": {"enabled": true},
                        "docs": {"command": "docs"}}),
                        &start["config"],
                    );
                    let mut names = vec!["docs".to_owned()];
                    if !expected.is_empty() {
                        names.push(expected.to_owned());
                    }
                    names.sort();
                    assert_eq!(active, names);
                    assert_eq!(
                        notice,
                        (!bundled && enabled && !installed).then_some(CUA_DRIVER_MISSING)
                    );
                    let listing =
                        response_json(computer_use_listing_with(&state, driver).await).await;
                    assert_eq!(listing["cuaDriverEnabled"], enabled);
                    assert_eq!(listing["cuaDriverInstalled"], installed);
                    assert_eq!(listing["codexUsesChatGPT"], bundled);
                    assert_eq!(
                        listing["providers"][0]["status"],
                        if bundled {
                            "Using ChatGPT's computer use"
                        } else if !enabled {
                            "Off"
                        } else if installed {
                            "Using Cua Driver"
                        } else {
                            "Cua Driver isn't installed"
                        }
                    );
                    for provider in listing["providers"].as_array().unwrap().iter().skip(1) {
                        assert_ne!(provider["inUse"], "chatGPT");
                        assert_eq!(
                            provider["status"],
                            if !enabled {
                                "Off"
                            } else if installed {
                                "Using Cua Driver"
                            } else {
                                "Cua Driver isn't installed"
                            }
                        );
                    }
                }
            }
        }
        // Removing ChatGPT leaves a stale manifest: no native route survives.
        state.projects.set_codex_bin(binary.clone());
        std::fs::remove_file(&node).unwrap();
        let mut start = json!({});
        assert_eq!(
            configure_codex_with(&state, None, &mut start).await,
            Some(CUA_DRIVER_MISSING)
        );
        state.computer_use_enabled = false;
        configure_codex_with(&state, driver, &mut start).await;
        assert_eq!(start["config"]["mcp_servers.cua_driver"]["enabled"], false);
        assert_eq!(start["config"]["mcp_servers.cua_repl"]["enabled"], false);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn settings_write_round_trip_and_old_choice_is_rejected() {
        let (_dir, mut state) = ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        assert!(serde_json::from_value::<SetComputerUse>(json!({"codex": "desktop"})).is_err());
        assert!(
            serde_json::from_value::<SetComputerUse>(json!({"cuaDriverEnabled": "false"})).is_err()
        );
        for enabled in [false, true] {
            let body = json!({"cuaDriverEnabled": enabled});
            crate::tests::validate_http_contract("setComputerUseRequest", &body);
            let request = serde_json::from_value(body).unwrap();
            let listing = response_json(
                set_computer_use(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Json(request),
                )
                .await,
            )
            .await;
            assert_eq!(listing["cuaDriverEnabled"], enabled);
            assert_eq!(state.store.cua_driver_enabled().await.unwrap(), enabled);
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn windows_bundled_codex_uses_native_even_with_driver_off() {
        let (dir, mut state) = ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        state.codex_home = dir.path().join("home");
        for (root, app) in [
            ("ChatGPT", "ChatGPT.exe"),
            ("OpenAI.ChatGPT-Desktop_1.0_x64__fixture", "app/ChatGPT.exe"),
        ] {
            let root = dir.path().join(root);
            let binary = root.join("app/resources/codex-cli/bin/codex.exe");
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, "").unwrap();
            std::fs::write(root.join(app), "").unwrap();
            desktop_plugin(&state.codex_home, "26.10.1", &binary).await;
            state.projects.set_codex_bin(binary);
            state
                .store
                .set_cua_driver_enabled(false, "now")
                .await
                .unwrap();
            let mut params = json!({});
            assert_eq!(configure_codex_with(&state, None, &mut params).await, None);
            assert_eq!(params["config"]["mcp_servers.cua_repl"]["enabled"], true);
            assert_eq!(params["config"]["mcp_servers.cua_driver"]["enabled"], false);
            let listing = response_json(computer_use_listing_with(&state, None).await).await;
            assert_eq!(listing["codexUsesChatGPT"], true);
            assert_eq!(listing["cuaDriverEnabled"], false);
            assert_eq!(listing["cuaDriverStatus"], "Off");
            assert_eq!(
                listing["providers"][0]["status"],
                "Using ChatGPT's computer use"
            );
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a host started without computer use must distinguish host
    // availability from installation and the saved driver preference. The
    // phone must never offer a switch that cannot become effective.
    #[tokio::test]
    async fn host_disabled_listing_explains_unavailable_switch() {
        let (_dir, mut state) = ingestion::tests::fixture().await;
        state.computer_use_enabled = false;
        for saved in [true, false] {
            state
                .store
                .set_cua_driver_enabled(saved, "now")
                .await
                .unwrap();
            for installed in [true, false] {
                let driver = installed.then(|| PathBuf::from("/installed/cua-driver"));
                let mut params = json!({"config": {"unrelated": true}});
                assert_eq!(
                    configure_codex_with(&state, driver.clone(), &mut params).await,
                    None
                );
                assert_eq!(params, json!({"config": {"unrelated": true}}));
                let listing = response_json(computer_use_listing_with(&state, driver).await).await;
                assert_eq!(listing["computerUseAvailable"], false);
                assert_eq!(listing["cuaDriverEnabled"], false);
                assert_eq!(listing["cuaDriverInstalled"], installed);
                assert_eq!(
                    listing["cuaDriverStatus"],
                    "Computer use is off on this computer"
                );
                assert_eq!(listing["codexUsesChatGPT"], false);
                for provider in listing["providers"].as_array().unwrap() {
                    assert_eq!(provider["inUse"], "off");
                    assert_eq!(provider["status"], "Computer use is off on this computer");
                }
                assert_eq!(state.store.cua_driver_enabled().await.unwrap(), saved);
            }
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a failed settings read disables tools for every provider;
    // model context states the fact without interactive UI retry wording.
    #[tokio::test]
    async fn unreadable_setting_fails_closed_with_separate_model_context() {
        let (dir, mut state) = ingestion::tests::fixture().await;
        let driver = Some(PathBuf::from("/installed/cua-driver"));
        state.computer_use_enabled = true;
        let binary = dir.path().join("ChatGPT.app/Contents/Resources/codex");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "").unwrap();
        state.codex_home = dir.path().join("home");
        desktop_plugin(&state.codex_home, "26.10.1", &binary).await;
        state.projects.set_codex_bin(binary);
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("DROP TABLE computer_use_settings")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let mut params = json!({});
        configure_project_with(&state, true, driver.clone(), &mut params)
            .await
            .unwrap();
        assert_eq!(params["config"]["mcp_servers.cua_driver"]["enabled"], false);
        assert!(params["config"].get("mcp_servers.cua_repl").is_none());
        let notice = configure_codex_with(&state, driver.clone(), &mut params)
            .await
            .unwrap();
        assert_eq!(notice, "Computer use is unavailable for this turn because the host could not read its Cua Driver setting.");
        assert_eq!(params["config"]["mcp_servers.cua_driver"]["enabled"], false);
        let listing = response_json(computer_use_listing_with(&state, driver).await).await;
        assert_eq!(listing["computerUseAvailable"], true);
        assert_eq!(listing["cuaDriverEnabled"], false);
        assert_eq!(listing["codexUsesChatGPT"], false);
        assert!(listing["cuaDriverStatus"]
            .as_str()
            .unwrap()
            .starts_with("Off"));
        assert!(listing["cuaDriverStatus"]
            .as_str()
            .unwrap()
            .contains("couldn’t read"));
        for provider in listing["providers"].as_array().unwrap() {
            assert_eq!(provider["inUse"], "off");
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Codex's view of a thread's MCP servers: the owner's `config.toml`
    /// servers with the thread's `mcp_servers.<name>` overrides merged over
    /// them key by key. Returns the names that end up enabled.
    fn enabled_after_merge(inherited: &Value, overrides: &Value) -> Vec<String> {
        let mut servers = inherited.clone();
        for (key, value) in overrides.as_object().unwrap() {
            let Some(name) = key.strip_prefix("mcp_servers.") else {
                continue;
            };
            let entry = &mut servers[name];
            if !entry.is_object() {
                *entry = json!({});
            }
            for (field, value) in value.as_object().unwrap() {
                entry[field] = value.clone();
            }
        }
        let mut enabled: Vec<_> = servers
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, server)| server["enabled"].as_bool().unwrap_or(true))
            .map(|(name, _)| name.clone())
            .collect();
        enabled.sort();
        enabled
    }

    #[tokio::test]
    async fn native_manifest_preserves_provider_runtime_and_uses_newest_version() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_cua(dir.path()).await.unwrap().is_none());
        let node = dir.path().join("node");
        std::fs::write(&node, "").unwrap();
        for version in ["26.9.9", "26.10.1"] {
            let root = dir
                .path()
                .join("plugins/cache/openai-bundled/unified-computer-use")
                .join(version);
            tokio::fs::create_dir_all(&root).await.unwrap();
            let server = serde_json::json!({"command":node, "args":[version], "env":{"CODEX_HOME":"/provider/home"}, "enabled":true});
            tokio::fs::write(
                root.join(".mcp.json"),
                serde_json::json!({"mcpServers":{"cua_repl":server}}).to_string(),
            )
            .await
            .unwrap();
        }
        let server = installed_cua(dir.path()).await.unwrap().unwrap();
        assert_eq!(server["args"], serde_json::json!(["26.10.1"]));
        assert_eq!(server["env"]["CODEX_HOME"], "/provider/home");
    }

    // A manifest is usable only while the files it starts from exist: a
    // relative command resolves in the plugin, and a removed script counts.
    #[tokio::test]
    async fn stale_desktop_manifest_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = desktop_plugin(dir.path(), "26.10.1", FsPath::new("bin/node")).await;
        assert!(installed_cua(dir.path()).await.unwrap().is_none());
        std::fs::create_dir_all(plugin.join("bin")).unwrap();
        std::fs::write(plugin.join("bin/node"), "").unwrap();
        assert!(installed_cua(dir.path()).await.unwrap().is_some());
        let script = dir.path().join("cua.mjs");
        let server = json!({"command": "bin/node", "args": [script.to_string_lossy()]});
        assert!(!manifest_runnable(&server, &plugin));
        std::fs::write(&script, "").unwrap();
        assert!(manifest_runnable(&server, &plugin));
        assert!(!manifest_runnable(&json!({"command": ""}), &plugin));
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    // On Windows the ChatGPT app registers its computer use as `node_repl` in
    // the owner's Codex config: Cua Driver (or nothing) turns it off with
    // `cua_repl`, and the native route leaves it as the app registered it.
    #[test]
    fn windows_desktop_servers_are_disabled_outside_native_route() {
        let driver = || ComputerUse::CuaDriver("/cua-driver".into());
        let app = || ComputerUse::Desktop(json!({"command": "/provider/node"}));
        // (choice, Windows) -> servers set on the thread, and whether each is on
        let table = [
            (
                driver(),
                true,
                vec![
                    ("cua_driver", true),
                    ("cua_repl", false),
                    ("node_repl", false),
                ],
            ),
            (
                ComputerUse::Missing,
                true,
                vec![
                    ("cua_driver", false),
                    ("cua_repl", false),
                    ("node_repl", false),
                ],
            ),
            (app(), true, vec![("cua_driver", false), ("cua_repl", true)]),
            (
                driver(),
                false,
                vec![("cua_driver", true), ("cua_repl", false)],
            ),
            (
                ComputerUse::Missing,
                false,
                vec![("cua_driver", false), ("cua_repl", false)],
            ),
            (
                app(),
                false,
                vec![("cua_driver", false), ("cua_repl", true)],
            ),
        ];
        for (choice, windows, expected) in table {
            let label = format!("{choice:?} on Windows {windows}");
            let servers: Vec<_> = codex_servers(choice, windows)
                .into_iter()
                .map(|(name, server)| (name, server["enabled"].as_bool().unwrap_or(true)))
                .collect();
            assert_eq!(servers, expected, "{label}");
        }
    }
}
