//! Provider updates, with one owner per provider (the T3 Code model): what is
//! installed (`--version`), what is newest (npm, Homebrew or Cupola's own
//! Claude runtime) and the updater that owns the binary. Cupola runs only that
//! updater, then checks the installed version again. Running conversations
//! are never stopped: the new version is used once that provider is idle
//! (Codex through the existing replaced-install restart, Claude through the
//! runtime's own switch between turns).
//!
//! | Installed by            | Newest from             | Update runs                         |
//! |-------------------------|-------------------------|-------------------------------------|
//! | Cupola (Claude runtime) | the runtime's npm check | the runtime's own check and staging |
//! | ChatGPT (bundled Codex) | unknown                 | nothing: ChatGPT updates itself     |
//! | npm (`@openai/codex`)   | registry `latest`       | `npm install -g --prefix …`         |
//! | Homebrew (`codex`)      | `brew info --json=v2`   | `brew upgrade [--cask] codex`       |
//! | anything else           | unknown                 | nothing: update it where installed  |

use crate::{AppState, OwnerAuthority};
use axum::{
    extract::{Extension, Path as RoutePath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use wonder_store::AgentFamily;

const CODEX_PACKAGE: &str = "@openai/codex";
const UPDATE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CLAUDE_CHECK_TIMEOUT: Duration = Duration::from_secs(4 * 60);
const LATEST_TIMEOUT: Duration = Duration::from_secs(4);
const LATEST_FOUND_AGE: Duration = Duration::from_secs(60 * 60);
const LATEST_MISSING_AGE: Duration = Duration::from_secs(15 * 60);
const LATEST_LOOKUP_LIMIT: Duration = Duration::from_secs(20);
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// Who installed a provider's binary, proven from its real path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// Cupola's bundled Claude runtime stages its own SDK patches.
    Cupola,
    /// ChatGPT's bundled Codex: ChatGPT replaces it when it updates.
    ChatGpt,
    /// `<prefix>/lib/node_modules/@openai/codex`, with that prefix's npm.
    Npm { npm: PathBuf, prefix: PathBuf },
    /// A Homebrew formula or cask under that Homebrew's prefix.
    Homebrew { brew: PathBuf, cask: bool },
    /// Installed some other way: update it with the tool that installed it.
    Manual,
}

impl Owner {
    fn as_str(&self) -> &'static str {
        match self {
            Owner::Cupola => "cupola",
            Owner::ChatGpt => "chatgpt",
            Owner::Npm { .. } => "npm",
            Owner::Homebrew { .. } => "homebrew",
            Owner::Manual => "manual",
        }
    }

    fn can_update(&self) -> bool {
        matches!(
            self,
            Owner::Cupola | Owner::Npm { .. } | Owner::Homebrew { .. }
        )
    }
}

/// The owner of the Codex at `binary`. Only a path that proves an installer
/// gets that installer's updater; everything else is manual.
pub(crate) fn codex_owner(binary: &Path) -> Owner {
    let real = crate::canonical_path::canonicalize(binary).unwrap_or_else(|_| binary.to_owned());
    if real.components().any(|c| c.as_os_str() == "ChatGPT.app")
        || real
            .ancestors()
            .any(crate::provider_runtime::windows_chatgpt_root)
    {
        return Owner::ChatGpt;
    }
    let text = real.to_string_lossy().replace('\\', "/");
    for marker in [
        "/lib/node_modules/@openai/codex/",
        "/node_modules/@openai/codex/",
    ] {
        if let Some(at) = text.find(marker) {
            let prefix = PathBuf::from(&text[..at]);
            let npm = [prefix.join("bin/npm"), prefix.join("npm.cmd")]
                .into_iter()
                .find(|npm| npm.is_file());
            if let Some(npm) = npm {
                return Owner::Npm { npm, prefix };
            }
        }
    }
    for (marker, cask) in [("/Cellar/codex/", false), ("/Caskroom/codex/", true)] {
        if let Some(at) = text.find(marker) {
            let brew = Path::new(&text[..at]).join("bin/brew");
            if brew.is_file() {
                return Owner::Homebrew { brew, cask };
            }
        }
    }
    Owner::Manual
}

/// What to check when this Codex binary could not start, naming the Codex
/// in use: ChatGPT's bundled one or a Codex CLI installed on its own.
pub(crate) fn codex_start_failure(binary: &Path) -> &'static str {
    match codex_owner(binary) {
        Owner::ChatGpt => {
            "Codex could not start on your computer. Check that ChatGPT is installed and signed in."
        }
        _ => {
            "Codex could not start on your computer. Check that the Codex CLI is installed and signed in."
        }
    }
}

/// The newest version the owner would install, as last looked up: never
/// waits on the network or Homebrew, so Settings → Providers and Reconnect
/// answer at once. A stale or missing value starts one background lookup per
/// owner; a found version is kept an hour, a failed lookup 15 minutes. None
/// while unknown or when the owner has no channel.
fn codex_latest(owner: &Owner) -> Option<String> {
    let lookup: std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> =
        match owner {
            Owner::Npm { .. } => Box::pin(npm_latest()),
            Owner::Homebrew { brew, cask } => {
                Box::pin(homebrew_latest(brew.clone(), *cask, "codex"))
            }
            Owner::Cupola | Owner::ChatGpt | Owner::Manual => return None,
        };
    cached_latest(format!("{owner:?}"), lookup)
}

#[derive(Default)]
struct Latest {
    value: Option<String>,
    /// When the value goes stale: an hour after a found version, 15 minutes
    /// after a failed lookup (which keeps the last version found).
    stale_at: Option<Instant>,
    looking: bool,
}

/// Serve `key`'s cached latest version and, when it is stale, start `lookup`
/// in the background unless one is already running for that key.
fn cached_latest(
    key: String,
    lookup: impl std::future::Future<Output = Option<String>> + Send + 'static,
) -> Option<String> {
    static CACHE: LazyLock<Mutex<HashMap<String, Latest>>> = LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = cache.entry(key.clone()).or_default();
    let fresh = entry.stale_at.is_some_and(|at| Instant::now() < at);
    if !fresh && !entry.looking {
        entry.looking = true;
        tokio::spawn(async move {
            let found = tokio::time::timeout(LATEST_LOOKUP_LIMIT, lookup)
                .await
                .ok()
                .flatten();
            let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let entry = cache.entry(key).or_default();
            let age = if found.is_some() {
                LATEST_FOUND_AGE
            } else {
                LATEST_MISSING_AGE
            };
            entry.value = found.or(entry.value.take());
            entry.stale_at = Some(Instant::now() + age);
            entry.looking = false;
        });
    }
    entry.value.clone()
}

async fn npm_latest() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(LATEST_TIMEOUT)
        .build()
        .ok()?;
    let body: Value = client
        .get("https://registry.npmjs.org/@openai%2Fcodex/latest")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    version_text(body["version"].as_str()?)
}

async fn homebrew_latest(brew: PathBuf, cask: bool, package: &str) -> Option<String> {
    let mut command = tokio::process::Command::new(&brew);
    command
        .args(["info", "--json=v2"])
        .args(cask.then_some("--cask"))
        .arg(package)
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = command.output().await.ok()?;
    let info: Value = serde_json::from_slice(&output.stdout).ok()?;
    let version = if cask {
        info["casks"][0]["version"].as_str()
    } else {
        info["formulae"][0]["versions"]["stable"].as_str()
    }?;
    version_text(version)
}

/// A version string short and plain enough to show and compare.
fn version_text(version: &str) -> Option<String> {
    let version = version.trim();
    (!version.is_empty()
        && version.len() <= 40
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')))
    .then(|| version.to_owned())
}

/// How this computer's Homebrew installed Cupola itself, proven from the
/// running program's real path, and the command that updates it.
fn host_homebrew(exe: &Path) -> Option<(PathBuf, bool)> {
    let real = crate::canonical_path::canonicalize(exe).unwrap_or_else(|_| exe.to_owned());
    let text = real.to_string_lossy().replace('\\', "/");
    for (marker, cask) in [("/Cellar/cupola/", false), ("/Caskroom/cupola/", true)] {
        if let Some(at) = text.find(marker) {
            let brew = Path::new(&text[..at]).join("bin/brew");
            if brew.is_file() {
                return Some((brew, cask));
            }
        }
    }
    // A cask moves the signed app out of the Caskroom; its symlink stays.
    for prefix in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
        let brew = Path::new(prefix).join("bin/brew");
        let Ok(versions) = std::fs::read_dir(Path::new(prefix).join("Caskroom/cupola")) else {
            continue;
        };
        for version in versions.flatten() {
            let app = crate::canonical_path::canonicalize(version.path().join("Cupola.app"));
            if brew.is_file() && app.is_ok_and(|app| real.starts_with(app)) {
                return Some((brew, true));
            }
        }
    }
    None
}

/// The command a person runs on this computer to update Cupola.
fn host_update_command(brew: Option<&(PathBuf, bool)>) -> &'static str {
    match brew {
        Some((_, true)) | None => "brew upgrade --cask cupola",
        Some((_, false)) => "brew upgrade cupola",
    }
}

/// The release feed and channel of an install.sh/install.ps1 install: the
/// `install.json` receipt that names this very executable (in an ancestor
/// folder, or the install folder whose receipt names this app). Mirrors the
/// `cupola update` CLI, which is what that install updates with.
fn host_receipt(exe: &Path) -> Option<(String, String)> {
    let real = crate::canonical_path::canonicalize(exe).unwrap_or_else(|_| exe.to_owned());
    let mut folders: Vec<PathBuf> = real
        .ancestors()
        .skip(1)
        .take(6)
        .map(Path::to_path_buf)
        .collect();
    folders.extend(
        std::env::var_os("WONDER_INSTALL_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    folders.extend(std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share/cupola")));
    for dir in folders {
        let Ok(bytes) = std::fs::read(dir.join("install.json")) else {
            continue;
        };
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        let Ok(receipt) = serde_json::from_slice::<Value>(bytes) else {
            continue;
        };
        let named = |key: &str| {
            receipt[key]
                .as_str()
                .and_then(|p| crate::canonical_path::canonicalize(p).ok())
        };
        let own_folder = crate::canonical_path::canonicalize(&dir).ok();
        let owned = receipt["owner"] == "wonder-cli-v1"
            && named("prefix").is_some()
            && named("prefix") == own_folder
            && (real.starts_with(own_folder.as_deref()?)
                || named("app").is_some_and(|app| real.starts_with(app)));
        if owned {
            let feed = receipt["feed"].as_str().filter(|f| !f.is_empty())?;
            let channel = receipt["channel"].as_str().unwrap_or("stable");
            return Some((feed.to_owned(), channel.to_owned()));
        }
    }
    None
}

/// The newest version the install's own feed names for its channel.
async fn feed_latest(feed: String, channel: String) -> Option<String> {
    let base = reqwest::Url::parse(&format!("{}/", feed.trim_end_matches('/'))).ok()?;
    let loopback = matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if !base.username().is_empty()
        || !(base.scheme() == "https" || (base.scheme() == "http" && loopback))
        || !channel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let url = base.join(&format!("{channel}.json")).ok()?;
    let response = reqwest::Client::new().get(url).send().await.ok()?;
    let release: Value = response.error_for_status().ok()?.json().await.ok()?;
    version_text(release["version"].as_str()?)
}

/// The release of a newer Cupola.app that Homebrew already put on disk, when
/// this running host predates it: read from the bundle that holds this program
/// (or, when Homebrew removed that folder, the newest cask version).
fn downloaded_bundle_version(exe: &Path, brew: &Path) -> Option<String> {
    let own = exe
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|e| e == "app"))
        .map(Path::to_owned);
    let caskroom = brew.parent()?.parent()?.join("Caskroom/cupola");
    let mut apps: Vec<PathBuf> = own.into_iter().collect();
    for version in std::fs::read_dir(caskroom).into_iter().flatten().flatten() {
        apps.push(version.path().join("Cupola.app"));
    }
    newer_bundle_version(
        crate::RELEASE_VERSION,
        apps.iter().filter_map(|app| {
            let xml = crate::computer_apps::info_plist(app)?;
            let version =
                crate::computer_apps::plist_string(&xml, "CFBundleVersion").or_else(|| {
                    crate::computer_apps::plist_string(&xml, "CFBundleShortVersionString")
                })?;
            version_text(&version)
        }),
    )
}

/// The newest of `found` that is newer than the running `version`.
fn newer_bundle_version(version: &str, found: impl Iterator<Item = String>) -> Option<String> {
    found
        .filter(|v| {
            wonder_app_server::compare_codex_versions(v, version)
                == Some(std::cmp::Ordering::Greater)
        })
        .fold(None, |best: Option<String>, v| match best {
            Some(b)
                if wonder_app_server::compare_codex_versions(&v, &b)
                    != Some(std::cmp::Ordering::Greater) =>
            {
                Some(b)
            }
            _ => Some(v),
        })
}

/// How to start the downloaded release: through the login service when one is
/// installed, else by stopping and starting the host.
fn restart_command() -> String {
    let service = std::env::var_os("HOME").is_some_and(|home| {
        Path::new(&home)
            .join("Library/LaunchAgents/com.saimun.wonder.cli.plist")
            .is_file()
    });
    if service {
        "cupola service restart".into()
    } else {
        "cupola stop; cupola start".into()
    }
}

/// This computer's Cupola for `GET /api/v1/providers`: what runs now, the
/// newest release its own installer knows (Homebrew, or the install receipt's
/// feed; never waited on, like the providers') and the command that updates
/// it. An earlier host sends no `host` at all, which is how a newer app knows
/// the computer is older than it needs.
pub(crate) fn host_entry() -> Value {
    let exe = std::env::current_exe().ok();
    let brew = exe.as_deref().and_then(host_homebrew);
    if brew.is_none() {
        if let Some((feed, channel)) = exe.as_deref().and_then(host_receipt) {
            let latest = cached_latest(
                format!("cupola-host:{feed}:{channel}"),
                feed_latest(feed, channel),
            );
            return host_json_with(crate::RELEASE_VERSION, latest.as_deref(), "cupola update");
        }
    }
    // `brew upgrade --cask` replaces the app on disk but not the running host:
    // the newer release is already downloaded, so restarting is the update.
    if let (Some(exe), Some((brew, true))) = (exe.as_deref(), brew.as_ref()) {
        if let Some(on_disk) = downloaded_bundle_version(exe, brew) {
            return host_json_with(crate::RELEASE_VERSION, Some(&on_disk), &restart_command());
        }
    }
    let latest = brew.as_ref().and_then(|(brew, cask)| {
        let (brew, cask) = (brew.clone(), *cask);
        cached_latest(
            format!("cupola-host:{brew:?}:{cask}"),
            homebrew_latest(brew, cask, "cupola"),
        )
    });
    host_json(crate::RELEASE_VERSION, latest.as_deref(), brew.as_ref())
}

fn host_json(version: &str, latest: Option<&str>, brew: Option<&(PathBuf, bool)>) -> Value {
    host_json_with(version, latest, host_update_command(brew))
}

fn host_json_with(version: &str, latest: Option<&str>, command: &str) -> Value {
    let available = latest.is_some_and(|latest| {
        wonder_app_server::compare_codex_versions(latest, version)
            == Some(std::cmp::Ordering::Greater)
    });
    json!({
        "version": version,
        "latestVersion": latest,
        "available": available,
        "command": command,
    })
}

/// The last update a device started for one provider on this computer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Run {
    /// updating, ready, succeeded, unchanged or failed.
    status: &'static str,
    message: Option<String>,
}

type Runs = Mutex<HashMap<(PathBuf, AgentFamily), Run>>;
static RUNS: LazyLock<Runs> = LazyLock::new(Default::default);

fn run_of(state: &AppState, family: AgentFamily) -> Option<Run> {
    RUNS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(state.data_dir.clone(), family))
        .cloned()
}

fn record(state: &AppState, family: AgentFamily, status: &'static str, message: Option<String>) {
    RUNS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((state.data_dir.clone(), family), Run { status, message });
}

/// Claim the provider's update, or None when one is already running.
fn start(state: &AppState, family: AgentFamily) -> bool {
    let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let key = (state.data_dir.clone(), family);
    if runs.get(&key).is_some_and(|run| run.status == "updating") {
        return false;
    }
    runs.insert(
        key,
        Run {
            status: "updating",
            message: None,
        },
    );
    true
}

/// What Cupola's Claude runtime reports about its SDK updates, read without
/// starting it. None when it is not running.
async fn claude_runtime_status(state: &AppState, method: &str, wait: Duration) -> Option<Value> {
    let rpc = crate::projects::live_rpc_for(state, AgentFamily::Claude).await?;
    let response = tokio::time::timeout(wait, rpc.request(method, json!({})))
        .await
        .ok()?
        .ok()?;
    response.error.is_none().then_some(response.result?)
}

/// The `update` object of a provider entry in `GET /api/v1/providers`, and
/// whether a downloaded update waits to be switched to ("Update ready").
pub(crate) async fn advisory(
    state: &AppState,
    family: AgentFamily,
    installed_version: &str,
) -> Option<(Value, bool)> {
    let run = run_of(state, family);
    match family {
        AgentFamily::Claude => {
            let runtime = claude_runtime_status(state, "runtime/update/read", STATUS_TIMEOUT).await;
            let field = |name: &str| {
                runtime
                    .as_ref()
                    .and_then(|r| r[name].as_str())
                    .map(str::to_owned)
            };
            let (active, pending, newest) = (
                field("activeVersion"),
                field("pendingVersion"),
                field("newestCompatibleVersion"),
            );
            let available =
                pending.is_some() || newest.as_ref().is_some_and(|v| Some(v) != active.as_ref());
            Some((
                advisory_json(
                    &Owner::Cupola,
                    available,
                    (active.as_deref(), newest.as_deref()),
                    runtime.is_some(),
                    field("adapterUpdateVersion").is_some(),
                    run.as_ref(),
                    runtime.as_ref().is_some_and(|r| r["checking"] == true),
                ),
                pending.is_some(),
            ))
        }
        AgentFamily::Codex => {
            let owner = codex_owner(&state.projects.launch_config().codex_bin);
            let latest = codex_latest(&owner);
            let available = latest.as_deref().is_some_and(|latest| {
                wonder_app_server::compare_codex_versions(latest, installed_version)
                    == Some(std::cmp::Ordering::Greater)
            });
            Some((
                advisory_json(
                    &owner,
                    available,
                    (
                        Some(installed_version).filter(|v| !v.is_empty()),
                        latest.as_deref(),
                    ),
                    owner.can_update(),
                    false,
                    run.as_ref(),
                    false,
                ),
                false,
            ))
        }
        AgentFamily::OpenCode | AgentFamily::Cursor => None,
    }
}

fn advisory_json(
    owner: &Owner,
    available: bool,
    // (installed, newest) as the owner reports them.
    (installed, latest): (Option<&str>, Option<&str>),
    can_update: bool,
    cupola_update_needed: bool,
    run: Option<&Run>,
    checking: bool,
) -> Value {
    let (status, message) = match run {
        Some(run) => (run.status, run.message.clone()),
        None if checking => ("updating", None),
        None => ("idle", None),
    };
    json!({
        "owner": owner.as_str(),
        "available": available,
        "installedVersion": installed,
        "latestVersion": latest,
        "canUpdate": can_update,
        "cupolaUpdateNeeded": cupola_update_needed,
        "status": status,
        "message": message,
    })
}

/// `POST /api/v1/providers/{id}/update`: run the updater that owns this
/// provider's install, from a paired device. Answers at once with the
/// provider's entry (status `updating`); the result appears in later reads.
pub(crate) async fn owner_update(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    RoutePath(provider): RoutePath<String>,
) -> Response {
    let refuse = |status: StatusCode, detail: &str| {
        (status, Json(json!({"detail": detail}))).into_response()
    };
    let family = match provider.as_str() {
        "codex" => AgentFamily::Codex,
        "claude" => AgentFamily::Claude,
        "opencode" | "cursor" => {
            return refuse(
                StatusCode::CONFLICT,
                "Update this agent on your computer with the app or tool that installed it.",
            )
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let owner = match family {
        AgentFamily::Claude => Owner::Cupola,
        _ => codex_owner(&state.projects.launch_config().codex_bin),
    };
    match &owner {
        Owner::ChatGpt => {
            return refuse(
                StatusCode::CONFLICT,
                "ChatGPT updates its Codex on its own. Cupola switches to the new version when agents finish.",
            )
        }
        Owner::Manual => {
            return refuse(
                StatusCode::CONFLICT,
                "Update Codex on your computer with the app or tool that installed it.",
            )
        }
        _ => {}
    }
    if family == AgentFamily::Claude
        && crate::projects::live_rpc_for(&state, family)
            .await
            .is_none()
    {
        return refuse(
            StatusCode::CONFLICT,
            "Claude isn’t running on your computer. Reconnect it, then try again.",
        );
    }
    if !start(&state, family) {
        return refuse(
            StatusCode::CONFLICT,
            "An update is already running. Try again when it finishes.",
        );
    }
    let task = state.clone();
    tokio::spawn(async move {
        let binary = task.projects.launch_config().codex_bin;
        let (status, message) = match family {
            AgentFamily::Claude => update_claude(&task).await,
            _ => {
                let latest = codex_latest(&owner);
                update_codex(&task, &owner, &binary, latest.as_deref()).await
            }
        };
        record(&task, family, status, Some(message));
    });
    (
        StatusCode::ACCEPTED,
        Json(
            json!({"id": family.as_str(), "update": advisory(&state, family, "").await
            .map(|(update, _)| update)}),
        ),
    )
        .into_response()
}

/// Run the installer that owns Codex, then read `--version` again. A newer
/// version is used once Codex is idle; running turns keep the current one.
async fn update_codex(
    state: &AppState,
    owner: &Owner,
    binary: &Path,
    latest: Option<&str>,
) -> (&'static str, String) {
    let before = codex_version(binary).await;
    let mut command = match owner {
        Owner::Npm { npm, prefix } => {
            // npm's launcher is a Node script; prefer the Node beside it.
            let node = [prefix.join("bin/node"), prefix.join("node.exe")]
                .into_iter()
                .find(|node| node.is_file());
            let mut command = match node {
                Some(node) => {
                    let mut command = tokio::process::Command::new(node);
                    command.arg(npm);
                    command
                }
                None => tokio::process::Command::new(npm),
            };
            command
                .args(["install", "-g", "--no-audit", "--no-fund", "--prefix"])
                .arg(prefix)
                .arg(format!("{CODEX_PACKAGE}@latest"));
            command
        }
        Owner::Homebrew { brew, cask } => {
            let mut command = tokio::process::Command::new(brew);
            command
                .arg("upgrade")
                .args(cask.then_some("--cask"))
                .arg("codex");
            command
        }
        Owner::Cupola | Owner::ChatGpt | Owner::Manual => {
            return (
                "failed",
                "Update Codex on your computer with the app or tool that installed it.".into(),
            )
        }
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let outcome = tokio::time::timeout(UPDATE_TIMEOUT, command.output()).await;
    let installer = match owner {
        Owner::Homebrew { .. } => "Homebrew",
        _ => "npm",
    };
    let failure = match &outcome {
        Err(_) => Some("timed out".to_owned()),
        Ok(Err(error)) => Some(error.to_string()),
        Ok(Ok(output)) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Some(
                stderr
                    .lines()
                    .rfind(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect(),
            )
        }
        Ok(Ok(_)) => None,
    };
    if let Some(reason) = failure {
        let _ = state.logger.record(
            "warn",
            "provider_update_failed",
            json!({"agentFamily": AgentFamily::Codex, "owner": owner.as_str(), "reason": reason}),
        );
        return (
            "failed",
            format!("{installer} couldn’t update Codex. Cupola keeps using the current version."),
        );
    }
    crate::provider_runtime::forget_installed(state, AgentFamily::Codex).await;
    let after = codex_version(binary).await;
    let reached = |version: &Option<String>| {
        latest.zip(version.as_deref()).is_some_and(|(latest, now)| {
            wonder_app_server::compare_codex_versions(now, latest)
                .is_some_and(|order| order != std::cmp::Ordering::Less)
        })
    };
    let changed = after.is_some() && after != before;
    let _ = state.logger.record(
        "info",
        "provider_update_finished",
        json!({"agentFamily": AgentFamily::Codex, "owner": owner.as_str(), "changed": changed}),
    );
    if changed || reached(&after) && !reached(&before) {
        // The watcher restarts Codex on it once no agent is working.
        state.ingestion.set_update_pending(AgentFamily::Codex, true);
        let version = after.unwrap_or_default();
        return (
            "succeeded",
            format!("Codex {version} is installed. Cupola switches to it when agents finish."),
        );
    }
    match after {
        Some(version) if reached(&Some(version.clone())) => (
            "unchanged",
            format!("Codex {version} is already the newest version."),
        ),
        Some(version) => (
            "unchanged",
            format!("Codex is still {version}. Update it on your computer with {installer}."),
        ),
        None => (
            "failed",
            "Codex didn’t start after the update. Check it on your computer.".into(),
        ),
    }
}

async fn codex_version(binary: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    version_text(text.trim().trim_start_matches("codex-cli "))
}

/// Ask Cupola's Claude runtime to check now. It downloads and verifies a
/// compatible patch and switches to it between turns.
async fn update_claude(state: &AppState) -> (&'static str, String) {
    let Some(status) =
        claude_runtime_status(state, "runtime/update/check", CLAUDE_CHECK_TIMEOUT).await
    else {
        return (
            "failed",
            "Claude didn’t answer the update check. Try again.".into(),
        );
    };
    claude_outcome(&status)
}

fn claude_outcome(status: &Value) -> (&'static str, String) {
    let active = status["activeVersion"].as_str();
    if status["lastFailure"].is_string() {
        return (
            "failed",
            "Claude’s update couldn’t be downloaded or checked. Cupola keeps using the current version."
                .into(),
        );
    }
    if status["pendingVersion"].is_string() {
        return (
            "ready",
            "The Claude update is ready. Cupola switches to it when agents finish.".into(),
        );
    }
    if status["newestCompatibleVersion"].as_str().is_some()
        && status["newestCompatibleVersion"].as_str() == active
    {
        let mut message = "Claude is up to date.".to_owned();
        if status["adapterUpdateVersion"].is_string() {
            message.push_str(" A newer Claude needs the next Cupola update.");
        }
        return ("succeeded", message);
    }
    (
        "unchanged",
        "No Claude update was found. Cupola checks again tomorrow.".into(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A slow `brew info` (offline, or Homebrew busy) never holds up the
    /// provider list or Reconnect: the latest version is served from cache
    /// at once, looked up once in the background, then shown.
    #[tokio::test]
    async fn a_slow_latest_lookup_never_delays_the_provider_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::canonical_path::canonicalize(dir.path()).unwrap();
        let calls = root.join("calls");
        let release = root.join("release");
        executable(
            &root.join("bin/brew"),
            &format!(
                "#!/bin/sh\necho >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\n\
                 echo '{{\"formulae\":[{{\"versions\":{{\"stable\":\"0.170.0\"}}}}]}}'\n",
                calls.display(),
                release.display()
            ),
        );
        let codex = root.join("Cellar/codex/0.163.0/bin/codex");
        executable(&codex, "#!/bin/sh\n");
        let owner = codex_owner(&codex);
        assert!(matches!(owner, Owner::Homebrew { cask: false, .. }));

        // brew answers only once released below: a lookup that waited on it
        // would sit out the lookup limit on each call and run five of them.
        for _ in 0..5 {
            assert_eq!(codex_latest(&owner), None, "unknown while looking");
        }
        let lookups = || std::fs::read_to_string(&calls).map_or(0, |c| c.lines().count());
        let deadline = Instant::now() + Duration::from_secs(10);
        while lookups() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(lookups(), 1, "one lookup per owner at a time");

        std::fs::write(&release, "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while codex_latest(&owner).is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(codex_latest(&owner).as_deref(), Some("0.170.0"));
        assert_eq!(lookups(), 1, "a found version is cached");
    }

    #[test]
    fn a_start_failure_names_the_codex_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::canonical_path::canonicalize(dir.path()).unwrap();
        let chatgpt = root.join("ChatGPT.app/Contents/Resources/codex-cli/bin/codex");
        let cli = root.join("usr/local/bin/codex");
        executable(&chatgpt, "#!/bin/sh\n");
        executable(&cli, "#!/bin/sh\n");
        assert!(codex_start_failure(&chatgpt).contains("ChatGPT is installed"));
        let message = codex_start_failure(&cli);
        assert!(message.contains("Codex CLI is installed"), "{message}");
        assert!(!message.contains("ChatGPT"), "{message}");
    }

    #[test]
    fn the_owner_is_proven_from_the_real_install_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::canonical_path::canonicalize(dir.path()).unwrap();
        let npm_prefix = root.join("node");
        executable(&npm_prefix.join("bin/npm"), "#!/bin/sh\n");
        let npm_codex = npm_prefix.join("lib/node_modules/@openai/codex/bin/codex.js");
        executable(&npm_codex, "#!/bin/sh\n");
        // A PATH symlink resolves to the package that owns it.
        std::os::unix::fs::symlink(&npm_codex, npm_prefix.join("bin/codex")).unwrap();
        let brew = root.join("homebrew");
        executable(&brew.join("bin/brew"), "#!/bin/sh\n");
        let formula = brew.join("Cellar/codex/0.163.0/bin/codex");
        executable(&formula, "#!/bin/sh\n");
        let cask = brew.join("Caskroom/codex/0.163.0/codex-aarch64-apple-darwin");
        executable(&cask, "#!/bin/sh\n");
        let chatgpt = root.join("ChatGPT.app/Contents/Resources/codex-cli/bin/codex");
        executable(&chatgpt, "#!/bin/sh\n");
        // Without the installer beside it, a look-alike path is manual.
        let orphan = root.join("copy/lib/node_modules/@openai/codex/bin/codex.js");
        executable(&orphan, "#!/bin/sh\n");
        let stray_cellar = root.join("other/Cellar/codex/1/bin/codex");
        executable(&stray_cellar, "#!/bin/sh\n");
        for (binary, owner) in [
            (
                npm_prefix.join("bin/codex"),
                Owner::Npm {
                    npm: npm_prefix.join("bin/npm"),
                    prefix: npm_prefix.clone(),
                },
            ),
            (
                formula,
                Owner::Homebrew {
                    brew: brew.join("bin/brew"),
                    cask: false,
                },
            ),
            (
                cask,
                Owner::Homebrew {
                    brew: brew.join("bin/brew"),
                    cask: true,
                },
            ),
            (chatgpt, Owner::ChatGpt),
            (orphan, Owner::Manual),
            (stray_cellar, Owner::Manual),
            (root.join("missing/codex"), Owner::Manual),
        ] {
            assert_eq!(codex_owner(&binary), owner, "{}", binary.display());
        }
    }

    /// The installer runs, then the installed version decides the result:
    /// a real change is "succeeded" and switches over when Codex is idle;
    /// an installer that exits 0 without changing anything is "unchanged";
    /// a failing installer is "failed" in user language.
    #[tokio::test]
    async fn an_update_is_judged_by_the_version_installed_afterwards() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let prefix = dir.path().join("node");
        let version_file = dir.path().join("version");
        let codex = prefix.join("lib/node_modules/@openai/codex/bin/codex.js");
        executable(
            &codex,
            &format!(
                "#!/bin/sh\necho \"codex-cli $(cat '{}')\"\n",
                version_file.display()
            ),
        );
        let npm = prefix.join("bin/npm");
        let owner = Owner::Npm {
            npm: npm.clone(),
            prefix: prefix.clone(),
        };
        type Case = (&'static str, &'static str, &'static str, &'static str, bool);
        let cases: [Case; 4] = [
            // installer script, latest, status, message start, update pending
            (
                "echo 0.164.0 > \"$VERSION\"",
                "0.164.0",
                "succeeded",
                "Codex 0.164.0 is installed. Cupola switches to it when agents finish.",
                true,
            ),
            (
                "true",
                "0.164.0",
                "unchanged",
                "Codex is still 0.163.0. Update it on your computer with npm.",
                false,
            ),
            (
                "true",
                "0.163.0",
                "unchanged",
                "Codex 0.163.0 is already the newest version.",
                false,
            ),
            (
                "echo 'npm error code EACCES' >&2; exit 1",
                "0.164.0",
                "failed",
                "npm couldn’t update Codex. Cupola keeps using the current version.",
                false,
            ),
        ];
        for (script, latest, status, message, pending) in cases {
            std::fs::write(&version_file, "0.163.0").unwrap();
            executable(
                &npm,
                &format!(
                    "#!/bin/sh\nVERSION='{}'\n[ \"$1 $2 $5\" = 'install -g --prefix' ] || exit 9\n{script}\n",
                    version_file.display()
                ),
            );
            state
                .ingestion
                .set_update_pending(AgentFamily::Codex, false);
            assert_eq!(
                update_codex(&state, &owner, &codex, Some(latest)).await,
                (status, message.to_owned()),
                "{script}"
            );
            assert_eq!(
                state
                    .ingestion
                    .provider_runtime_state(AgentFamily::Codex)
                    .update_pending,
                pending,
                "{script}"
            );
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn claude_results_follow_the_runtime_status() {
        for (status, expected) in [
            (
                json!({"activeVersion":"0.3.283","pendingVersion":"0.3.290","newestCompatibleVersion":"0.3.290"}),
                "ready",
            ),
            (
                json!({"activeVersion":"0.3.290","newestCompatibleVersion":"0.3.290"}),
                "succeeded",
            ),
            (
                json!({"activeVersion":"0.3.283","lastFailure":"candidate_unavailable_or_incompatible"}),
                "failed",
            ),
            (json!({"activeVersion":"0.3.283"}), "unchanged"),
        ] {
            assert_eq!(claude_outcome(&status).0, expected, "{status}");
        }
        assert!(claude_outcome(
            &json!({"activeVersion":"0.3.290","newestCompatibleVersion":"0.3.290","adapterUpdateVersion":"0.4.0"})
        )
        .1
        .contains("next Cupola update"));
    }

    #[test]
    fn the_advisory_names_owner_status_and_whether_cupola_can_update() {
        let idle = advisory_json(
            &Owner::ChatGpt,
            false,
            (None, None),
            false,
            false,
            None,
            false,
        );
        assert_eq!(
            idle,
            json!({"owner":"chatgpt","available":false,"installedVersion":null,"latestVersion":null,"canUpdate":false,
                "cupolaUpdateNeeded":false,"status":"idle","message":null})
        );
        let failed = Run {
            status: "failed",
            message: Some("npm couldn’t update Codex.".into()),
        };
        let owner = Owner::Npm {
            npm: "/n/bin/npm".into(),
            prefix: "/n".into(),
        };
        let entry = advisory_json(
            &owner,
            true,
            (Some("0.163.0"), Some("0.164.0")),
            true,
            false,
            Some(&failed),
            false,
        );
        assert_eq!(entry["status"], "failed");
        assert_eq!(entry["available"], true);
        assert_eq!(entry["installedVersion"], "0.163.0");
        assert_eq!(entry["latestVersion"], "0.164.0");
        assert_eq!(
            advisory_json(&Owner::Cupola, false, (None, None), true, false, None, true)["status"],
            "updating"
        );
    }

    #[test]
    fn a_cask_upgrade_that_the_running_host_predates_asks_for_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let brew = root.path().join("bin/brew");
        let app = |version: &str| {
            let contents = root
                .path()
                .join(format!("Caskroom/cupola/{version}/Cupola.app/Contents"));
            std::fs::create_dir_all(contents.join("MacOS")).unwrap();
            std::fs::write(
                contents.join("Info.plist"),
                format!("<plist><dict><key>CFBundleVersion</key><string>{version}</string></dict></plist>"),
            )
            .unwrap();
            contents.join("MacOS/cupola")
        };
        // The running host started from a Caskroom folder Homebrew then removed.
        let removed = root
            .path()
            .join("Caskroom/cupola/0.0.1/Cupola.app/Contents/MacOS/cupola");
        assert_eq!(downloaded_bundle_version(&removed, &brew), None);
        let newer = app("999.0.0");
        assert_eq!(
            downloaded_bundle_version(&removed, &brew).as_deref(),
            Some("999.0.0")
        );
        // Its own bundle, replaced in place, tells the same.
        assert_eq!(
            downloaded_bundle_version(&newer, &brew).as_deref(),
            Some("999.0.0")
        );
        let entry = host_json_with(
            crate::RELEASE_VERSION,
            Some("999.0.0"),
            "cupola service restart",
        );
        assert_eq!(entry["available"], true);
        assert_eq!(entry["command"], "cupola service restart");
        assert_eq!(
            newer_bundle_version(
                "1.0.256",
                ["1.0.256", "1.0.300", "1.0.257"]
                    .map(String::from)
                    .into_iter()
            )
            .as_deref(),
            Some("1.0.300")
        );
        assert_eq!(
            newer_bundle_version("1.0.300", ["1.0.256"].map(String::from).into_iter()),
            None
        );
    }

    #[test]
    fn the_host_entry_names_versions_and_the_update_command() {
        let cask = (PathBuf::from("/opt/homebrew/bin/brew"), true);
        let formula = (PathBuf::from("/opt/homebrew/bin/brew"), false);
        let behind = host_json("1.0.256", Some("1.0.300"), Some(&cask));
        assert_eq!(behind["version"], "1.0.256");
        assert_eq!(behind["latestVersion"], "1.0.300");
        assert_eq!(behind["available"], true);
        assert_eq!(behind["command"], "brew upgrade --cask cupola");
        let current = host_json("1.0.300", Some("1.0.300"), Some(&formula));
        assert_eq!(current["available"], false);
        assert_eq!(current["command"], "brew upgrade cupola");
        // Unknown latest is not "available"; the command is still the cask's.
        let unknown = host_json("1.0.256", None, None);
        assert_eq!(unknown["available"], false);
        assert_eq!(unknown["command"], "brew upgrade --cask cupola");
        // An install.sh install updates with its own command, never brew.
        let receipt = host_json_with("1.0.257", Some("1.0.258"), "cupola update");
        assert_eq!(receipt["available"], true);
        assert_eq!(receipt["command"], "cupola update");
    }

    #[test]
    fn a_receipt_names_its_feed_only_for_its_own_executable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("install");
        let app = directory.path().join("Applications/Cupola.app");
        let exe = app.join("Contents/MacOS/cupola");
        let stranger = directory
            .path()
            .join("Applications/Other.app/Contents/MacOS/cupola");
        for path in [&exe, &stranger] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        let receipt = json!({"owner": "wonder-cli-v1", "prefix": root, "app": app,
                             "channel": "beta", "feed": "https://private.example/releases"});
        std::fs::write(root.join("install.json"), receipt.to_string()).unwrap();
        std::env::set_var("WONDER_INSTALL_DIR", &root);
        let found = host_receipt(&exe);
        let foreign = host_receipt(&stranger);
        std::env::remove_var("WONDER_INSTALL_DIR");
        assert_eq!(
            found,
            Some(("https://private.example/releases".into(), "beta".into()))
        );
        assert_eq!(foreign, None);
    }

    #[test]
    fn the_host_owner_is_proven_from_its_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        executable(&prefix.join("bin/brew"), "#!/bin/sh\n");
        let formula = prefix.join("Cellar/cupola/1.0.256/bin");
        std::fs::create_dir_all(&formula).unwrap();
        executable(&formula.join("cupola"), "#!/bin/sh\n");
        let found = host_homebrew(&formula.join("cupola")).expect("formula install");
        assert!(!found.1);
        let cask = prefix.join("Caskroom/cupola/1.0.256/Cupola.app/Contents/MacOS");
        std::fs::create_dir_all(&cask).unwrap();
        executable(&cask.join("cupola"), "#!/bin/sh\n");
        assert!(host_homebrew(&cask.join("cupola")).expect("cask install").1);
        let loose = dir.path().join("cupola");
        executable(&loose, "#!/bin/sh\n");
        assert!(host_homebrew(&loose).is_none());
    }
}
