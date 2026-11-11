//! Keep provider runtimes current with what is installed on the Mac, and let
//! Settings → Providers reconnect one.
//!
//! ChatGPT replaces its bundled Codex in place (Sparkle swaps the app). A
//! running app-server keeps executing the replaced binary until it exits, and
//! a runtime rejected as unsupported is only re-verified when something asks
//! for it, so an update was never picked up without restarting Wonder. The
//! watcher notices the replaced install, re-locates it and restarts each
//! Codex runtime once it is idle; startup verification then accepts or
//! rejects the new version. Claude's SDK updates are staged by the sidecar
//! and activate between requests on their own; a reconnect restarts the
//! sidecar, which also activates a staged version immediately.

use crate::{AppState, LocalOwnerAuthority, OwnerAuthority};
use axum::{
    extract::{Extension, Path as RoutePath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use wonder_store::AgentFamily;

const WATCH_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Installed-provider checks spawn the provider's CLI. Ordinary reads serve the
/// last answer and revalidate in the background once it reaches this age.
const INSTALLED_MAX_AGE: Duration = Duration::from_secs(30);
const INSTALLED_TIMEOUT: Duration = Duration::from_secs(20);

/// The Codex launcher ChatGPT ships, or `WONDER_CODEX_BIN`.
pub fn locate_codex() -> PathBuf {
    // The owner's chosen program (Settings > Codex > Program) comes first.
    if let Some(path) = wonder_app_server::provider_environment::runtime_settings("codex").binary {
        return path;
    }
    if let Some(path) = std::env::var_os("WONDER_CODEX_BIN") {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            for app in ["ChatGPT", "Codex"] {
                roots.extend([local.join("Programs").join(app), local.join(app)]);
            }
        }
        if let Some(programs) = std::env::var_os("ProgramFiles").map(PathBuf::from) {
            roots.push(programs.join("ChatGPT"));
            roots.push(programs.join("Codex"));
            if let Ok(packages) = std::fs::read_dir(programs.join("WindowsApps")) {
                roots.extend(
                    packages
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| windows_chatgpt_root(path)),
                );
            }
        }
        return roots
            .iter()
            .find_map(|root| windows_chatgpt_codex(root))
            .unwrap_or_else(|| "codex.exe".into());
    }
    // WONDER_CHATGPT_APP moves the app the locator looks in, so the nightly
    // can check a Mac where ChatGPT is not installed (scripts/check-cli-codex).
    #[cfg(not(windows))]
    mac_chatgpt_codex(
        &std::env::var_os("WONDER_CHATGPT_APP")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/Applications/ChatGPT.app".into()),
    )
}

#[cfg(not(windows))]
fn mac_chatgpt_codex(app: &Path) -> PathBuf {
    let resources = app.join("Contents/Resources");
    // The supported launcher preserves adjacency with codex-code-mode-host.
    // Do not select the nested Mach-O directly; its helper lives elsewhere.
    let current = resources.join("codex-cli/bin/codex");
    if current.is_file() {
        current
    } else {
        resources.join("codex")
    }
}

/// Recognize the installed Windows desktop app, including its renamed Store package.
/// A copied plugin cache or a directory merely called ChatGPT is insufficient.
pub(crate) fn windows_chatgpt_root(root: &Path) -> bool {
    let name = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    ["chatgpt", "codex"].iter().any(|app| {
        (name == *app
            || name.starts_with(&format!("openai.{app}_"))
            || name.starts_with(&format!("openai.{app}-")))
            && [
                "ChatGPT.exe",
                "app/ChatGPT.exe",
                "Codex.exe",
                "app/Codex.exe",
            ]
            .iter()
            .any(|exe| root.join(exe).is_file())
    })
}

#[cfg(any(windows, test))]
fn windows_chatgpt_codex(root: &Path) -> Option<PathBuf> {
    if !windows_chatgpt_root(root) {
        return None;
    }
    // App versions move their resources. Search only inside that install,
    // without following directory symlinks or traversing another user's files.
    // Codex.exe at the app root is the desktop GUI. Only the resources
    // directory contains the CLI and its adjacent code-mode helper.
    let mut pending = vec![(root.join("resources"), 0), (root.join("app/resources"), 0)];
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("codex.exe")
            {
                return Some(entry.path());
            }
            if kind.is_dir() && depth < 10 {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    None
}

/// Identity of an installed Codex: a replaced app bundle gives the launcher
/// and the binary it starts new inodes and modification times.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint(Vec<(PathBuf, u64, u64, i64)>);

fn fingerprint(launcher: &Path) -> Option<Fingerprint> {
    let launcher = crate::canonical_path::canonicalize(launcher).ok()?;
    let nested = launcher
        .parent()
        .map(|bin| bin.join("../CodexCLI.app/Contents/MacOS/codex"))
        .and_then(|path| crate::canonical_path::canonicalize(path).ok());
    let mut files = Vec::new();
    for path in std::iter::once(launcher).chain(nested) {
        let metadata = std::fs::metadata(&path).ok()?;
        let stamp = crate::platform::file_stamp(&metadata);
        files.push((path, stamp.inode, metadata.len(), stamp.modified.0));
    }
    Some(Fingerprint(files))
}

pub fn spawn_update_watcher(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut installed = fingerprint(&locate_codex());
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            let path = locate_codex();
            let current = fingerprint(&path);
            if current.is_some() && current != installed {
                installed = current;
                use_codex_install(&state, path).await;
                state.ingestion.set_update_pending(AgentFamily::Codex, true);
                let _ = state
                    .logger
                    .record("info", "codex_runtime_update_detected", json!({}));
            }
            if state
                .ingestion
                .provider_runtime_state(AgentFamily::Codex)
                .update_pending
                && replace_stale_codex(&state).await
            {
                state
                    .ingestion
                    .set_update_pending(AgentFamily::Codex, false);
                let _ = state.logger.record(
                    "info",
                    "codex_runtime_update_applied",
                    json!({"incompatible": state.ingestion.provider_runtime_state(AgentFamily::Codex).incompatible}),
                );
            }
        }
    })
}

async fn use_codex_install(state: &AppState, path: PathBuf) {
    state
        .projects
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .codex_bin = path.clone();
    state.projects.set_codex_bin(path);
}

/// Whether a Wonder message is running on `family`. Fails closed.
async fn provider_busy(state: &AppState, family: AgentFamily) -> bool {
    let Ok(messages) = state.store.active_runtime_messages().await else {
        return true;
    };
    for message in messages.iter().filter(|m| m.state != "uncertain") {
        match crate::claude::conversation_family(state, &message.conversation_id).await {
            Ok(found) if found != family => {}
            _ => return true,
        }
    }
    false
}

/// Restart every idle Codex runtime that runs a replaced binary. Returns
/// whether none is left running the old one.
async fn replace_stale_codex(state: &AppState) -> bool {
    let mut replaced = true;
    if state.projects.codex_alive().await {
        if crate::projects::release_codex_runtime_when_idle(state, true).await {
            // Start the new install now so readiness reflects it.
            let _ = crate::projects::codex_rpc(state).await;
        } else {
            replaced = false;
        }
    } else if state
        .ingestion
        .provider_runtime_state(AgentFamily::Codex)
        .incompatible
    {
        // An earlier version was rejected; verify the new one.
        let _ = crate::projects::codex_rpc(state).await;
    }
    replaced
}

fn family(id: &str) -> Option<AgentFamily> {
    match id {
        "codex" => Some(AgentFamily::Codex),
        "claude" => Some(AgentFamily::Claude),
        "opencode" => Some(AgentFamily::OpenCode),
        "cursor" => Some(AgentFamily::Cursor),
        _ => None,
    }
}

fn report(state: &AppState, family: AgentFamily) -> serde_json::Value {
    let runtime = state.ingestion.provider_runtime_state(family);
    let installed = crate::claude::available(state, family);
    let detail = if runtime.incompatible {
        Some(crate::ingestion::CODEX_UNSUPPORTED_DETAIL.to_owned())
    } else if !installed {
        Some(match family {
            AgentFamily::Claude => crate::claude::RUNTIME_MISSING.into(),
            _ => format!(
                "Cupola’s {} runtime is missing. Update Cupola on your computer.",
                family.provider().display_name
            ),
        })
    } else {
        runtime.setup.map(|issue| issue.detail(family).to_owned())
    };
    json!({
        "id": family.as_str(),
        "running": runtime.alive,
        "starting": runtime.starting && !runtime.alive && installed,
        "incompatible": runtime.incompatible,
        "updatePending": runtime.update_pending,
        "detail": detail,
    })
}

/// `GET /api/v1/host/providers`: each provider's runtime in this daemon.
pub(crate) async fn status(
    State(state): State<AppState>,
    local: Option<Extension<LocalOwnerAuthority>>,
) -> Response {
    if local.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut providers = vec![
        report(&state, AgentFamily::Codex),
        report(&state, AgentFamily::Claude),
    ];
    if state.opencode.is_some() {
        providers.push(report(&state, AgentFamily::OpenCode));
    }
    if state.cursor.is_some() {
        providers.push(report(&state, AgentFamily::Cursor));
    }
    Json(json!({ "providers": providers })).into_response()
}

/// `POST /api/v1/host/providers/{id}/reconnect`: re-locate and re-verify the
/// provider, restart its runtime and report the result.
pub(crate) async fn reconnect(
    State(state): State<AppState>,
    local: Option<Extension<LocalOwnerAuthority>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if local.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(family) = family(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match reconnect_family(&state, family).await {
        Ok(()) => Json(report(&state, family)).into_response(),
        Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
async fn reconnect_family(state: &AppState, family: AgentFamily) -> Result<(), Response> {
    restart_family(state, family, false)
        .await
        .map_err(|(status, detail)| (status, Json(json!({"detail": detail}))).into_response())
}

/// What happened to a running runtime after the owner saved its settings.
pub(crate) enum SettingsApplied {
    /// Not running: the next launch reads them.
    NextLaunch,
    /// Stopped and started again with them.
    Restarted,
    /// Work is still running; they apply once it finishes.
    Waiting,
    /// Stopped, but the runtime did not start with them. Says why.
    Failed(String),
}

async fn runtime_alive(state: &AppState, family: AgentFamily) -> bool {
    match family {
        AgentFamily::Codex => state.projects.codex_alive().await,
        family => match crate::claude::client(state, family) {
            Ok(client) => client.lock().await.health().is_alive(),
            Err(_) => false,
        },
    }
}

/// Restarts a running runtime that started before the owner's latest saved
/// settings, once nothing is running on it. A runtime that is not running
/// reads them at its next launch.
pub(crate) async fn apply_runtime_settings(
    state: &AppState,
    family: AgentFamily,
) -> SettingsApplied {
    if !runtime_alive(state, family).await {
        return SettingsApplied::NextLaunch;
    }
    if !wonder_app_server::provider_environment::settings_outdated(family.as_str()) {
        return SettingsApplied::NextLaunch;
    }
    // Look before taking the admission and dispatch locks: a runtime that is
    // still working must not hold up other chats' messages while we find out.
    if provider_busy(state, family).await || retains_work(state, family, true).await {
        return SettingsApplied::Waiting;
    }
    match restart_family(state, family, true).await {
        Ok(()) => SettingsApplied::Restarted,
        Err((StatusCode::CONFLICT, _)) => SettingsApplied::Waiting,
        Err((_, detail)) => SettingsApplied::Failed(detail),
    }
}

/// Whether a runtime is still running on settings saved before the latest ones.
pub(crate) async fn waiting_for_settings(state: &AppState, family: AgentFamily) -> bool {
    wonder_app_server::provider_environment::settings_outdated(family.as_str())
        && runtime_alive(state, family).await
}

/// Called from the recovery loop: applies saved settings to runtimes that
/// were busy when the owner saved them.
pub(crate) async fn apply_waiting_settings(state: &AppState) {
    // Tests share the launch registry across fixtures; one fixture's save must
    // not restart another's runtime.
    if cfg!(test) {
        return;
    }
    static LAST_TRY: std::sync::Mutex<Vec<(AgentFamily, Instant)>> =
        std::sync::Mutex::new(Vec::new());
    for family in AgentFamily::ALL {
        let recent = LAST_TRY.lock().is_ok_and(|tries| {
            tries
                .iter()
                .any(|(f, at)| *f == family && at.elapsed() < SETTINGS_RETRY_INTERVAL)
        });
        if recent || !waiting_for_settings(state, family).await {
            continue;
        }
        if let Ok(mut tries) = LAST_TRY.lock() {
            tries.retain(|(f, _)| *f != family);
            tries.push((family, Instant::now()));
        }
        let _ = apply_runtime_settings(state, family).await;
    }
}

/// How long a runtime that was still working waits before the recovery loop
/// asks again whether saved settings can be applied.
const SETTINGS_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// `automatic` restarts (applying saved settings) treat a runtime that does
/// not say what it still runs as busy; the owner's Reconnect treats it as
/// stuck, which is what Reconnect is for.
async fn restart_family(
    state: &AppState,
    family: AgentFamily,
    automatic: bool,
) -> Result<(), (StatusCode, String)> {
    let failure = |status: StatusCode, detail: &str| (status, detail.to_owned());
    const WORKING: &str = "An agent is still working. Reconnect after it finishes.";
    // Submissions share admission through receipt storage. Reconnect needs
    // exclusive admission: even an idle roster cannot authorize replacing a
    // generation whose turn/start has not answered yet. Do not queue a writer
    // behind that send and hold up unrelated chats' submissions.
    let Ok(_reconnecting) = state.provider_reconnect.try_write() else {
        return Err(failure(
            StatusCode::CONFLICT,
            "An agent is sending a message or reconnecting. Try again shortly.",
        ));
    };
    if provider_busy(state, family).await {
        return Err(failure(StatusCode::CONFLICT, WORKING));
    }
    {
        // The barrier dispatch admits messages under: no message is claimed or
        // started on this provider while it is held, so the check below holds
        // until the runtime is stopped. A message admitted after it starts the
        // new runtime instead.
        let Some(_admission) = state.update_admission.claim_guard().await else {
            return Err(failure(
                StatusCode::CONFLICT,
                "Cupola is getting ready to update. Reconnect after it finishes.",
            ));
        };
        let _dispatch = state.dispatch_lock.lock().await;
        if provider_busy(state, family).await || retains_work(state, family, automatic).await {
            return Err(failure(StatusCode::CONFLICT, WORKING));
        }
        match family {
            AgentFamily::Codex => {
                use_codex_install(state, installed_codex(state)).await;
                state.projects.stop_codex().await;
            }
            AgentFamily::Claude | AgentFamily::OpenCode | AgentFamily::Cursor => {
                if let Some(runtime) = crate::claude::bridge_runtime(state, family) {
                    let _ = runtime.client.lock().await.shutdown().await;
                }
            }
        }
    }
    let outcome = match family {
        AgentFamily::Codex => reconnect_codex(state).await,
        AgentFamily::Claude => reconnect_claude(state).await,
        AgentFamily::OpenCode | AgentFamily::Cursor => reconnect_acp(state, family).await,
    };
    forget_installed(state, family).await;
    let _ = state.logger.record(
        if outcome.is_ok() { "info" } else { "warn" },
        "provider_reconnect",
        json!({"agentFamily": family, "ok": outcome.is_ok()}),
    );
    outcome.map_err(|detail| failure(StatusCode::SERVICE_UNAVAILABLE, &detail))
}

/// The Codex Reconnect switches to: the installed one. Tests keep their
/// fixture's, so a reconnect never starts the Mac's real Codex in a test.
fn installed_codex(state: &AppState) -> PathBuf {
    #[cfg(test)]
    return state.projects.codex_bin();
    #[cfg(not(test))]
    {
        let _ = state;
        locate_codex()
    }
}

/// How long a running runtime gets to say what it still runs. One that does
/// not answer is stuck, which is what Reconnect is for.
const RETAINED_WORK_TIMEOUT: Duration = Duration::from_secs(5);

/// Work the provider's runtime keeps after Wonder's turns ended: Codex
/// threads still running or holding background terminals, and Claude
/// processes kept for background agents, commands or monitors. Stopping the
/// runtime would end it. A runtime that is not running keeps nothing; an
/// answer that cannot be read counts as work. One that does not answer in
/// time counts as work only when `silent_is_work` (an automatic restart must
/// not kill a slow runtime that is running threads).
async fn retains_work(state: &AppState, family: AgentFamily, silent_is_work: bool) -> bool {
    let Some(rpc) = crate::projects::live_rpc_for(state, family).await else {
        return false;
    };
    let check = async {
        let request = |method: &'static str, params: serde_json::Value| {
            let rpc = rpc.clone();
            async move {
                let response = rpc.request(method, params).await.ok()?;
                response.error.is_none().then_some(response.result?)
            }
        };
        let Some(loaded) = request("thread/loaded/list", json!({})).await else {
            return true;
        };
        let Some(threads) = loaded["data"].as_array() else {
            return true;
        };
        if family == AgentFamily::Claude {
            // The Claude runtime lists the sessions whose process it keeps.
            return !threads.is_empty();
        }
        for thread in threads {
            let Some(thread) = thread.as_str() else {
                return true;
            };
            let Some(read) = request("thread/read", json!({"threadId": thread})).await else {
                return true;
            };
            if read["thread"]["status"]["type"].as_str() != Some("idle") {
                return true;
            }
            let Some(background) = request(
                "thread/backgroundTerminals/list",
                json!({"threadId": thread, "limit": 1}),
            )
            .await
            else {
                return true;
            };
            if !background["data"].as_array().is_some_and(Vec::is_empty) {
                return true;
            }
        }
        false
    };
    tokio::time::timeout(RETAINED_WORK_TIMEOUT, check)
        .await
        .unwrap_or(silent_is_work)
}

// ---------------------------------------------------------------------------
// Paired devices: what is installed and signed in, and Reconnect.
// ---------------------------------------------------------------------------

/// The installed provider's native status, without starting model work or
/// exposing credentials. Bridge probes use the same launch paths as turns.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Installed {
    /// signedIn, signedOut, needsSubscription, unsupported, notInstalled or unavailable.
    state: &'static str,
    version: String,
    /// "ChatGPT account", "API key", "Claude subscription"; empty when unknown.
    account: String,
}

impl Installed {
    fn from_output(code: Option<i32>, stdout: &str, family: AgentFamily) -> Self {
        let state = match code {
            Some(0) => "signedIn",
            Some(42) => "signedOut",
            Some(43) => "unsupported",
            Some(44) => "notInstalled",
            Some(45) => "needsSubscription",
            Some(46) if family == AgentFamily::OpenCode => "installed",
            _ => "unavailable",
        };
        let line = stdout.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        let mut fields = line.splitn(2, '\t');
        let mut version = fields.next().unwrap_or("").trim().to_owned();
        if family == AgentFamily::Codex {
            version = version.trim_start_matches("codex-cli ").to_owned();
        }
        let account = fields.next().unwrap_or("").trim();
        let account =
            if family == AgentFamily::Claude && code == Some(0) && account.starts_with("Claude ") {
                "Claude subscription"
            } else {
                account
            };
        Self {
            state,
            version: version.chars().take(40).collect(),
            account: account.chars().take(60).collect(),
        }
    }
}

/// Only the small cache bookkeeping is locked; native probes always run outside
/// it. Invalidation advances the generation and discards the old answer, while
/// retaining the refresh owner until its probe finishes (and is discarded).
#[derive(Default)]
struct InstalledCache {
    generation: u64,
    value: Option<(Instant, Installed)>,
    refreshing: Option<tokio::sync::watch::Receiver<()>>,
}

/// Native status belongs to one host's runtime configuration. Cloned AppState
/// values share its caches; independent hosts/fixtures cannot share probe owners.
#[derive(Default)]
pub struct InstalledCaches {
    caches: [std::sync::Mutex<InstalledCache>; 4],
    #[cfg(test)]
    waiters: [std::sync::atomic::AtomicUsize; 4],
}

fn installed_index(family: AgentFamily) -> usize {
    match family {
        AgentFamily::Codex => 0,
        AgentFamily::Claude => 1,
        AgentFamily::OpenCode => 2,
        AgentFamily::Cursor => 3,
    }
}

fn installed_cache(state: &AppState, family: AgentFamily) -> &std::sync::Mutex<InstalledCache> {
    &state.installed_providers.caches[installed_index(family)]
}

#[cfg(test)]
struct InstalledWaiter<'a>(&'a std::sync::atomic::AtomicUsize);
#[cfg(test)]
impl Drop for InstalledWaiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The next read must wait for a check begun after this invalidation. A check
/// already running cannot publish its pre-sign-in/reconnect answer afterward.
pub(crate) async fn forget_installed(state: &AppState, family: AgentFamily) {
    let mut cache = installed_cache(state, family)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.generation = cache.generation.wrapping_add(1);
    cache.value = None;
}

/// Start one refresh or subscribe to the existing owner. The task owns the
/// probe independently of its requesting HTTP connection. Closing the channel
/// wakes waiters, including if the task is cancelled or panics; a later read
/// can then reclaim ownership rather than leaving the cache stuck refreshing.
fn refresh_installed(
    state: &AppState,
    family: AgentFamily,
    cache: &mut InstalledCache,
) -> tokio::sync::watch::Receiver<()> {
    if let Some(running) = &cache.refreshing {
        if running.has_changed().is_ok() {
            return running.clone();
        }
    }
    let (completed, waiting) = tokio::sync::watch::channel(());
    let generation = cache.generation;
    let state = state.clone();
    tokio::spawn(async move {
        let value = tokio::time::timeout(INSTALLED_TIMEOUT, native_status(&state, family))
            .await
            .unwrap_or_else(|_| Installed::from_output(None, "", family));
        let mut cache = installed_cache(&state, family)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.generation == generation {
            cache.value = Some((Instant::now(), value));
        }
        cache.refreshing = None;
        // Publish before waking readers; no probe or await holds the mutex.
        drop(cache);
        drop(completed);
    });
    cache.refreshing = Some(waiting.clone());
    waiting
}

struct InstalledSnapshot {
    value: Installed,
    current: bool,
}

async fn installed(state: &AppState, family: AgentFamily, refresh: bool) -> InstalledSnapshot {
    let asked = Instant::now();
    loop {
        let mut waiting = {
            let mut cache = installed_cache(state, family)
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((at, value)) = &cache.value {
                // Explicit refreshes share a check that finished after they
                // arrived; ordinary reads may serve stale, never invalidated.
                if !refresh || *at >= asked {
                    let snapshot = InstalledSnapshot {
                        value: value.clone(),
                        current: at.elapsed() < INSTALLED_MAX_AGE,
                    };
                    if !snapshot.current {
                        refresh_installed(state, family, &mut cache);
                    }
                    return snapshot;
                }
            }
            refresh_installed(state, family, &mut cache)
        };
        #[cfg(test)]
        let _waiter = {
            let counter = &state.installed_providers.waiters[installed_index(family)];
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            InstalledWaiter(counter)
        };
        let _ = waiting.changed().await;
        // An invalidated generation's result was discarded. Recheck and share
        // the next owner rather than returning that obsolete result.
    }
}

/// The last installation answer for pickers, not evidence of runtime readiness.
/// Preserve it while revalidating so a missing provider does not reappear on
/// every expiry. Invalidation discards it until a current check finishes.
pub(crate) fn known_state(state: &AppState, family: AgentFamily) -> Option<&'static str> {
    let mut cache = installed_cache(state, family)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let value = cache.value.as_ref().map(|(_, value)| value.state);
    if cache
        .value
        .as_ref()
        .is_none_or(|(at, _)| at.elapsed() >= INSTALLED_MAX_AGE)
    {
        refresh_installed(state, family, &mut cache);
    }
    value
}

async fn native_status(state: &AppState, family: AgentFamily) -> Installed {
    if family == AgentFamily::Codex {
        return codex_status(state).await;
    }
    let Some(runtime) = crate::claude::bridge_runtime(state, family) else {
        return Installed::from_output(Some(44), "", family);
    };
    let (script, action) = match family {
        AgentFamily::Claude => ("manage.mjs", "status"),
        AgentFamily::OpenCode => ("catalog.mjs", "status"),
        AgentFamily::Cursor => ("catalog.mjs", "cursor-status"),
        AgentFamily::Codex => unreachable!("Codex has its own native probe"),
    };
    let mut command = tokio::process::Command::new(&runtime.config.node_bin);
    command
        .arg(runtime.config.entrypoint.with_file_name(script))
        .arg(action)
        .env("WONDER_DATA_DIR", &state.data_dir);
    if let Some(home) = &runtime.config.claude_config_dir {
        command.env("CLAUDE_CONFIG_DIR", home);
    }
    // Status describes the program a launch would run, so it follows the
    // owner's Program setting.
    if let Some(binary) =
        wonder_app_server::provider_environment::runtime_settings(family.as_str()).binary
    {
        command.env(
            format!("WONDER_{}_BIN", family.as_str().to_ascii_uppercase()),
            binary,
        );
    }
    status_output(command, family).await
}

async fn codex_status(state: &AppState) -> Installed {
    use crate::account_cli::CliSignIn;
    let family = AgentFamily::Codex;
    let config = state.projects.launch_config();
    if !config.codex_bin.is_file() {
        return Installed::from_output(Some(44), "", family);
    }
    let mut version_command = tokio::process::Command::new(&config.codex_bin);
    version_command.arg("--version");
    let mut installed = status_output(version_command, family).await;
    // Preserve the existing protocol/code-mode verification and exit-code
    // semantics; merely finding a binary does not establish support.
    if let Err(error) = wonder_app_server::verify_runtime(&config.codex_bin).await {
        installed.state = if error.is_incompatible() {
            "unsupported"
        } else {
            "unavailable"
        };
        return installed;
    }
    let home = config.codex_home.as_deref().unwrap_or(&state.codex_home);
    match crate::account_cli::status(state, family, home).await {
        CliSignIn::SignedIn { auth_mode, .. } => {
            installed.state = "signedIn";
            installed.account = match auth_mode {
                Some("chatgpt") => "ChatGPT account",
                Some("apiKey") => "API key",
                _ => "",
            }
            .into();
        }
        CliSignIn::SignedOut => installed.state = "signedOut",
        _ => installed.state = "unavailable",
    }
    installed
}

async fn status_output(mut command: tokio::process::Command, family: AgentFamily) -> Installed {
    let unavailable = Installed {
        state: "unavailable",
        version: String::new(),
        account: String::new(),
    };
    let child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(INSTALLED_TIMEOUT, child).await {
        Ok(Ok(output)) => Installed::from_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            family,
        ),
        _ => unavailable,
    }
}

async fn provider_entry(state: &AppState, family: AgentFamily, refresh: bool) -> serde_json::Value {
    let snapshot = installed(state, family, refresh).await;
    let installed = snapshot.value;
    // Opening Providers to check again, or a status that no longer matches
    // why the provider was refused, re-checks it now instead of at its next
    // slow re-check.
    use crate::claude::SetupIssue;
    let changed = match state.ingestion.setup_issue(family) {
        Some(SetupIssue::NotInstalled) => {
            matches!(installed.state, "installed" | "signedIn" | "signedOut")
        }
        Some(SetupIssue::SignedOut) => installed.state == "signedIn",
        _ => false,
    };
    if snapshot.current && (refresh || changed) {
        state.ingestion.wake_setup_check(family);
    }
    let advisory = crate::provider_updates::advisory(state, family, &installed.version).await;
    if family == AgentFamily::Claude {
        // A downloaded SDK patch waits for the runtime to be idle: the
        // phone's "Update ready", whose Update now restarts the runtime on it.
        state.ingestion.set_update_pending(
            family,
            advisory.as_ref().is_some_and(|(_, pending)| *pending),
        );
    }
    let mut entry = report(state, family);
    if let Some((update, _)) = advisory {
        entry["update"] = update;
    }
    if family == AgentFamily::Codex
        && (installed.state == "unsupported" || entry["incompatible"] == true)
    {
        entry["detail"] = json!(unsupported_codex_detail(&installed.version));
    }
    entry["state"] = json!(installed.state);
    entry["version"] = json!(installed.version);
    entry["account"] = json!(installed.account);
    entry["working"] = json!(provider_busy(state, family).await);
    if family == AgentFamily::Codex && installed.state != "unsupported" {
        if let Some(warning) = codex_warning(
            &installed.version,
            &wonder_app_server::degraded_features(&state.projects.launch_config().codex_bin),
        ) {
            entry["warning"] = json!(warning);
        }
    }
    entry
}

/// What still works, and what does not, on a Codex Cupola runs but was not
/// tested with, or whose optional protocol moved. None when nothing is off.
fn codex_warning(version: &str, degraded: &[String]) -> Option<String> {
    let untested =
        wonder_app_server::codex_testing(version) == wonder_app_server::CodexTesting::Newer;
    match (untested, degraded.is_empty()) {
        (false, true) => None,
        (true, true) => Some(
            "This Codex is newer than the versions Cupola was tested with. Chats work; if something looks wrong, update Cupola on your computer.".into(),
        ),
        (_, false) => Some(format!(
            "Chats work. These features are off until Cupola is updated on your computer: {}.",
            degraded.join(", ")
        )),
    }
}

/// One piece of advice for a Codex Cupola refuses, by direction: older than
/// any Codex Cupola supports means updating Codex (ChatGPT, or the tool that
/// installed it); anything later means Cupola has not caught up yet.
fn unsupported_codex_detail(version: &str) -> &'static str {
    if wonder_app_server::codex_testing(version) == wonder_app_server::CodexTesting::Older {
        "This Codex is older than Cupola supports. Update ChatGPT (or the Codex CLI) on your computer."
    } else {
        crate::ingestion::CODEX_UNSUPPORTED_DETAIL
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct ProvidersQuery {
    #[serde(default)]
    refresh: bool,
}

/// `GET /api/v1/providers`: Codex and Claude for a paired device, as the
/// Mac's Settings → Providers shows them. `refresh=true` checks again.
pub(crate) async fn owner_status(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    Query(query): Query<ProvidersQuery>,
) -> Response {
    let (codex, claude, opencode, cursor) = tokio::join!(
        provider_entry(&state, AgentFamily::Codex, query.refresh),
        provider_entry(&state, AgentFamily::Claude, query.refresh),
        async {
            if state.opencode.is_some() {
                Some(provider_entry(&state, AgentFamily::OpenCode, query.refresh).await)
            } else {
                None
            }
        },
        async {
            if state.cursor.is_some() {
                Some(provider_entry(&state, AgentFamily::Cursor, query.refresh).await)
            } else {
                None
            }
        },
    );
    let mut providers = vec![codex, claude];
    // Only a packaged bridge is advertised. The status says whether its CLI is installed.
    if let Some(opencode) = opencode {
        providers.push(opencode);
    }
    if let Some(cursor) = cursor {
        providers.push(cursor);
    }
    Json(json!({ "providers": providers, "host": crate::provider_updates::host_entry() }))
        .into_response()
}

/// `POST /api/v1/providers/{id}/reconnect`: the Mac's Reconnect, from a
/// paired device. Refused while an agent of that provider is working.
pub(crate) async fn owner_reconnect(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    RoutePath(provider): RoutePath<String>,
) -> Response {
    let Some(family) = family(&provider) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match reconnect_family(&state, family).await {
        Ok(()) => Json(provider_entry(&state, family, true).await).into_response(),
        Err(response) => response,
    }
}

/// Starts the installed Codex after Reconnect stopped it. Starting the
/// Projects runtime verifies it and records whether it is supported, so this
/// reports on Codex's own new runtime without waiting for the recovery loop.
async fn reconnect_codex(state: &AppState) -> Result<(), String> {
    state.ingestion.wake_reconcile(AgentFamily::Codex);
    crate::projects::codex_rpc(state).await?;
    state
        .ingestion
        .set_update_pending(AgentFamily::Codex, false);
    Ok(())
}

async fn reconnect_claude(state: &AppState) -> Result<(), String> {
    if state.claude.is_none() {
        return Err(crate::claude::RUNTIME_MISSING.into());
    }
    let request = state.ingestion.request_reconcile(AgentFamily::Claude);
    reconnected(state, AgentFamily::Claude, request)
        .await
        .map(|_| ())
        .ok_or_else(|| "Claude didn’t restart. Check that it’s signed in, then try again.".into())
}

async fn reconnect_acp(state: &AppState, family: AgentFamily) -> Result<(), String> {
    let name = family.provider().display_name;
    if crate::claude::bridge_runtime(state, family).is_none() {
        return Err(format!(
            "Cupola’s {name} runtime is missing. Update Cupola on your computer."
        ));
    }
    let request = state.ingestion.request_reconcile(family);
    match reconnected(state, family, request).await {
        // Running, but the provider still is not set up: say what to fix.
        Some(setup) => setup.map_or(Ok(()), |issue| Err(issue.detail(family).into())),
        None => Err(match family {
            AgentFamily::OpenCode => {
                "OpenCode didn’t restart. Check that it runs on your computer, then try again."
                    .into()
            }
            _ => format!(
                "{name} didn’t restart. Check its sign-in on your computer, then try again."
            ),
        }),
    }
}

/// Waits for the recovery loop to finish this Reconnect's own request on the
/// provider's own runtime; it is woken at once rather than after its retry
/// sleep. None when that did not happen in time.
async fn reconnected(
    state: &AppState,
    family: AgentFamily,
    request: u64,
) -> Option<Option<crate::claude::SetupIssue>> {
    let deadline = tokio::time::Instant::now() + RECONNECT_TIMEOUT;
    loop {
        if let Some(setup) = state.ingestion.reconnected(family, request) {
            return Some(setup);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    static STATUS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(not(windows))]
    #[test]
    fn mac_locator_reads_only_the_named_chatgpt_app() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("ChatGPT.app");
        let resources = app.join("Contents/Resources");
        // An app without Codex names a path that does not exist, so the CLI
        // launcher falls back to PATH.
        assert!(!mac_chatgpt_codex(&app).is_file());
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("codex"), "").unwrap();
        assert_eq!(mac_chatgpt_codex(&app), resources.join("codex"));
        let launcher = resources.join("codex-cli/bin/codex");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::fs::write(&launcher, "").unwrap();
        assert_eq!(mac_chatgpt_codex(&app), launcher);
    }

    #[test]
    fn windows_chatgpt_discovery_requires_the_installed_app_and_bundled_codex() {
        let dir = tempfile::tempdir().unwrap();
        for (root, app) in [
            ("ChatGPT", "ChatGPT.exe"),
            ("OpenAI.ChatGPT-Desktop_1.0_x64__fixture", "app/ChatGPT.exe"),
            ("Codex", "Codex.exe"),
            ("OpenAI.Codex_26.1027.3384.0_x64__fixture", "app/Codex.exe"),
        ] {
            let root = dir.path().join(root);
            let binary = root.join("app/resources/codex-cli/bin/codex.exe");
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, "").unwrap();
            assert_eq!(windows_chatgpt_codex(&root), None);
            std::fs::write(root.join(app), "").unwrap();
            assert_eq!(windows_chatgpt_codex(&root), Some(binary.clone()));
            std::fs::remove_file(binary).unwrap();
            assert_eq!(windows_chatgpt_codex(&root), None);
        }
    }

    async fn call(
        state: &AppState,
        method: &str,
        path: &str,
        local: bool,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().method(method).uri(path);
        if local {
            request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
        }
        let response = crate::router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn project_runtime_id(state: &AppState) -> String {
        crate::projects::codex_rpc(state)
            .await
            .unwrap()
            .health()
            .id()
            .to_owned()
    }

    // Contract: sharing bridge discovery must preserve each provider's recovery
    // steps. OpenCode has no sign-in; Claude uses a subscription; Codex keeps
    // its own runtime path. Exercise the returned errors and status payload.
    #[tokio::test]
    async fn recovery_remains_specific_to_each_provider() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let cases = [
            (AgentFamily::Codex, None, None, None),
            (
                AgentFamily::Claude,
                Some(crate::claude::RUNTIME_MISSING),
                Some(crate::claude::RUNTIME_MISSING),
                Some((
                    vec![
                        (json!({"result":{"connected":false}}), "Sign in to your Claude subscription on your computer."),
                        (
                            json!({"error":{"code":-32000,"message":"unavailable"}}),
                            "Claude could not check your subscription. Sign in on your computer.",
                        ),
                    ],
                    "Claude didn’t restart. Check that it’s signed in, then try again.",
                )),
            ),
            (
                AgentFamily::OpenCode,
                Some("Cupola’s OpenCode runtime is missing. Update Cupola on your computer."),
                Some("Cupola’s OpenCode runtime is missing. Update Cupola on your computer."),
                Some((
                    vec![
                        (
                            json!({"result":{"connected":false,"availability":"notInstalled"}}),
                            "Install OpenCode’s command-line tool on your computer, then reconnect OpenCode.",
                        ),
                        (
                            json!({"result":{"connected":false,"availability":"noModels"}}),
                            "OpenCode has no models set up. Add a model provider in OpenCode on your computer.",
                        ),
                        (
                            json!({"result":{"connected":false,"availability":"unavailable"}}),
                            "OpenCode couldn’t list its models just now. Cupola will try again shortly.",
                        ),
                        (
                            json!({"error":{"code":-32000,"message":"unavailable"}}),
                            "OpenCode couldn’t list its models just now. Cupola will try again shortly.",
                        ),
                    ],
                    "OpenCode didn’t restart. Check that it runs on your computer, then try again.",
                )),
            ),
            (
                AgentFamily::Cursor,
                Some("Cupola’s Cursor runtime is missing. Update Cupola on your computer."),
                Some("Cupola’s Cursor runtime is missing. Update Cupola on your computer."),
                Some((
                    vec![
                        (
                            json!({"result":{"connected":false,"availability":"notInstalled"}}),
                            "Install Cursor’s command-line tool (agent) on your computer, then reconnect Cursor.",
                        ),
                        (
                            json!({"result":{"connected":false,"availability":"signedOut"}}),
                            "Sign in to Cursor on your computer (run agent login), then reconnect Cursor.",
                        ),
                        (
                            json!({"result":{"connected":false,"availability":"noModels"}}),
                            "Cursor listed no models. Check your Cursor account on your computer.",
                        ),
                        (
                            json!({"error":{"code":-32000,"message":"unavailable"}}),
                            "Cursor couldn’t be checked just now. Cupola will try again shortly.",
                        ),
                    ],
                    "Cursor didn’t restart. Check its sign-in on your computer, then try again.",
                )),
            ),
        ];
        let mut failures = Vec::new();
        for (family, missing, client_missing, discovery) in cases {
            let detail = report(&state, family)["detail"].as_str().map(str::to_owned);
            if detail.as_deref() != missing {
                failures.push(format!(
                    "{family:?} status: {detail:?}, expected {missing:?}"
                ));
            }
            let error = crate::claude::client(&state, family).err();
            if error.as_deref() != client_missing {
                failures.push(format!(
                    "{family:?} client: {error:?}, expected {client_missing:?}"
                ));
            }
            let Some((discovery, reconnect)) = discovery else {
                continue; // Codex uses its own account discovery, not a bridge.
            };
            let entrypoint = dir.path().join(format!("{}.py", family.as_str()));
            std::fs::write(
                &entrypoint,
                r#"import json, sys
from pathlib import Path
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request: continue
    if request['method'] == 'initialize':
        result = {'result': {'wonderBridge': {'protocolVersion': 1, 'family': Path(__file__).stem}, 'capabilities': {'experimentalApi': True}}}
    else:
        assert request['method'] == 'account/read'
        result = json.loads(Path(__file__).with_name('account.json').read_text())
    print(json.dumps({'id': request['id'], **result}), flush=True)
"#,
            )
            .unwrap();
            let config = wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint,
                state_dir: dir.path().join(family.as_str()),
                npm_cli: None,
                wonder_version: "test".into(),
                family: family.as_str(),
                claude_config_dir: None,
            };
            let mut runtime = wonder_app_server::AppServerClient::spawn_bridge(
                config.clone(),
                crate::ingestion::notification_sink(state.store.clone()),
            )
            .await
            .unwrap();
            for (response, expected) in discovery {
                std::fs::write(dir.path().join("account.json"), response.to_string()).unwrap();
                let error = crate::claude::discover_family(&state, &mut runtime, family)
                    .await
                    .unwrap_err();
                if error != expected {
                    failures.push(format!(
                        "{family:?} discovery: {error:?}, expected {expected:?}"
                    ));
                }
            }
            runtime.shutdown().await.unwrap();
            // A configured but stopped bridge never becomes ready. Pausing
            // only after child I/O exercises the actual reconnect deadline.
            let configured = Some(std::sync::Arc::new(crate::claude::Runtime {
                client: std::sync::Arc::new(tokio::sync::Mutex::new(runtime)),
                config,
            }));
            match family {
                AgentFamily::Claude => state.claude = configured,
                AgentFamily::OpenCode => state.opencode = configured,
                AgentFamily::Cursor => state.cursor = configured,
                AgentFamily::Codex => unreachable!("Codex has no bridge"),
            }
            tokio::time::pause();
            let error = if family == AgentFamily::Claude {
                reconnect_claude(&state).await
            } else {
                reconnect_acp(&state, family).await
            }
            .unwrap_err();
            tokio::time::resume();
            if error != reconnect {
                failures.push(format!(
                    "{family:?} reconnect: {error:?}, expected {reconnect:?}"
                ));
            }
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    // Contract: a replaced Codex install reaches every idle runtime without a
    // Wonder restart, and never interrupts a running turn.
    #[tokio::test]
    async fn stale_codex_runtimes_restart_once_idle() {
        let (_dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let before = project_runtime_id(&state).await;
        state
            .store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("thread"),
                Some("turn"),
            )
            .await
            .unwrap();
        assert!(state.projects.codex.lock().await.health().is_alive());
        assert!(
            !replace_stale_codex(&state).await,
            "A running Project turn keeps its runtime"
        );
        assert_eq!(project_runtime_id(&state).await, before);
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        assert!(replace_stale_codex(&state).await);
        let after = crate::projects::live_rpc_for(&state, AgentFamily::Codex)
            .await
            .expect("running")
            .health();
        assert!(after.is_alive(), "The installed version starts again");
        assert_ne!(after.id(), before);
        state.projects.shutdown().await;
    }

    #[tokio::test]
    async fn providers_report_runtime_state_and_reconnect_locally() {
        let (_dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let (status, _) = call(&state, "GET", "/api/v1/host/providers", false).await;
        assert_ne!(
            status,
            StatusCode::OK,
            "Only the Mac may read runtime state"
        );
        state
            .ingestion
            .set_runtime_incompatible(AgentFamily::Codex, true);
        let (status, body) = call(&state, "GET", "/api/v1/host/providers", true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["providers"][0]["id"], "codex");
        assert_eq!(body["providers"][0]["incompatible"], true);
        assert_eq!(
            body["providers"][0]["detail"],
            crate::ingestion::CODEX_UNSUPPORTED_DETAIL
        );
        assert_eq!(body["providers"][1]["id"], "claude");
        assert_eq!(body["providers"][1]["starting"], false);

        state
            .store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("thread"),
                Some("turn"),
            )
            .await
            .unwrap();
        let path = "/api/v1/host/providers/codex/reconnect";
        let (status, body) = call(&state, "POST", path, true).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let (status, body) = call(&state, "POST", path, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["running"], true);
        assert_eq!(body["starting"], false);
        assert_eq!(
            body["incompatible"], false,
            "Reconnecting re-verifies Codex"
        );
        let (status, _) = call(
            &state,
            "POST",
            "/api/v1/host/providers/other/reconnect",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(
            &state,
            "POST",
            "/api/v1/host/providers/claude/reconnect",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["detail"]
            .as_str()
            .unwrap()
            .contains("Reinstall Cupola"));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: Reconnect never stops a runtime under a dispatch that is
    // admitting a message. It waits for the dispatch barrier, then checks for
    // work again under it.
    #[tokio::test]
    async fn reconnect_rechecks_work_under_the_dispatch_barrier() {
        let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let before = project_runtime_id(&state).await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        // A dispatch holds the barrier while it claims and starts a message.
        let dispatch = state.dispatch_lock.lock().await;
        let reconnect = tokio::spawn({
            let state = state.clone();
            async move {
                call(
                    &state,
                    "POST",
                    "/api/v1/host/providers/codex/reconnect",
                    true,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let running = crate::projects::live_rpc_for(&state, AgentFamily::Codex)
            .await
            .map(|rpc| rpc.health().id().to_owned());
        assert_eq!(
            running.as_deref(),
            Some(before.as_str()),
            "Reconnect stopped the runtime while a dispatch was admitting work"
        );
        state
            .store
            .force_message_state(&message.id, "accepted_by_codex")
            .await
            .unwrap();
        drop(dispatch);
        let (status, body) = reconnect.await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(project_runtime_id(&state).await, before);

        // A finished turn that left a background terminal running keeps it.
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let idle = dir.path().join("idle-fixture.json");
        let terminal = json!({"thread": {"id": "thread", "status": {"type": "idle"},
            "background": [{"id": "terminal", "command": "npm run dev"}]}});
        std::fs::write(&idle, terminal.to_string()).unwrap();
        let path = "/api/v1/host/providers/codex/reconnect";
        let (status, body) = call(&state, "POST", path, true).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(project_runtime_id(&state).await, before);
        std::fs::write(
            &idle,
            json!({"thread": {"id": "thread", "status": {"type": "idle"}}}).to_string(),
        )
        .unwrap();
        let (status, body) = call(&state, "POST", path, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(project_runtime_id(&state).await, before);
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: the Mac's status script's exit codes and "version<TAB>account"
    // line become the states the phone words; nothing else is read.
    #[test]
    fn installed_status_follows_the_status_script() {
        for (native, label) in [
            ("API key", "API key"),
            ("Claude Max", "Claude subscription"),
        ] {
            let claude = Installed::from_output(
                Some(0),
                &format!("2.1.288\t{native}\n"),
                AgentFamily::Claude,
            );
            assert_eq!(claude.state, "signedIn");
            assert_eq!(claude.account, label);
        }
        for (code, expected) in [
            (0, "signedIn"),
            (42, "signedOut"),
            (44, "notInstalled"),
            (46, "unavailable"),
        ] {
            let cursor = Installed::from_output(
                Some(code),
                "2026.10.16\tCursor account\n",
                AgentFamily::Cursor,
            );
            assert_eq!(cursor.state, expected);
            assert_eq!(cursor.version, "2026.10.16");
        }
        let opencode = Installed::from_output(Some(46), "1.18.27\t\n", AgentFamily::OpenCode);
        assert_eq!(opencode.state, "installed");
        assert_eq!(opencode.version, "1.18.27");
        assert!(opencode.account.is_empty());
        let codex = Installed::from_output(
            Some(0),
            "noise\ncodex-cli 0.162.0\tChatGPT account\n",
            AgentFamily::Codex,
        );
        assert_eq!(
            (codex.state, codex.version.as_str(), codex.account.as_str()),
            ("signedIn", "0.162.0", "ChatGPT account")
        );
        let claude = Installed::from_output(Some(45), "2.1.0\tClaude Free\n", AgentFamily::Claude);
        assert_eq!(
            (claude.state, claude.account.as_str()),
            ("needsSubscription", "Claude Free")
        );
        for (code, expected) in [
            (Some(42), "signedOut"),
            (Some(43), "unsupported"),
            (Some(44), "notInstalled"),
            (Some(1), "unavailable"),
            (None, "unavailable"),
        ] {
            assert_eq!(
                Installed::from_output(code, "", AgentFamily::Codex).state,
                expected
            );
        }
    }

    fn claude_probe(state: &mut AppState, dir: &Path, script: &str) {
        let entrypoint = dir.join("main.mjs");
        std::fs::write(entrypoint.with_file_name("manage.mjs"), script).unwrap();
        state.claude = Some(std::sync::Arc::new(crate::claude::Runtime {
            client: std::sync::Arc::new(tokio::sync::Mutex::new(
                wonder_app_server::AppServerClient::unavailable(
                    "test".into(),
                    crate::ingestion::notification_sink(state.store.clone()),
                ),
            )),
            config: wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint,
                state_dir: dir.to_path_buf(),
                npm_cli: None,
                wonder_version: "test".into(),
                family: "claude",
                claude_config_dir: None,
            },
        }));
    }

    async fn probe_started(dir: &Path, run: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !dir.join(format!("started-{run}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("The owned native probe must start");
    }

    // Contract: a cold/invalidated cache waits for current evidence, an old
    // generation cannot publish after sign-in/reconnect, and abandoning an
    // HTTP request cannot cancel the refresh shared by other readers.
    #[tokio::test]
    async fn installed_refresh_owns_the_probe_and_rejects_invalidated_results() {
        use futures_util::FutureExt;
        let _status = STATUS_TEST_LOCK.lock().await;
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let family = AgentFamily::Claude;
        forget_installed(&state, family).await;
        claude_probe(
            &mut state,
            dir.path(),
            r#"import sys, time
from pathlib import Path
root = Path(__file__).parent
runs = root / 'status-runs'
run = len(runs.read_text().splitlines()) + 1 if runs.exists() else 1
with runs.open('a') as log: log.write('status\n')
code = int((root / 'status-code').read_text())
(root / f'started-{run}').touch()
while not (root / f'release-{run}').exists(): time.sleep(0.01)
print('2.0.0\tClaude account')
sys.exit(code)
"#,
        );
        std::fs::write(dir.path().join("status-code"), "42").unwrap();
        assert!(installed(&state, family, false).now_or_never().is_none());
        probe_started(dir.path(), 1).await;
        // A separate host configured for a different account must not join this
        // held probe or consume its result. Clones below still share this owner.
        let (other_dir, mut other) = crate::ingestion::tests::fixture().await;
        claude_probe(
            &mut other,
            other_dir.path(),
            "print('2.0.0\\tOther Claude account')\n",
        );
        let other_answer =
            tokio::time::timeout(Duration::from_secs(10), installed(&other, family, true))
                .await
                .expect("another host's status must not wait for this host's held probe");
        assert_eq!(other_answer.value.state, "signedIn");
        assert_eq!(other_answer.value.account, "Other Claude account");
        assert_eq!(known_state(&state, family), None);
        assert_eq!(known_state(&other, family), Some("signedIn"));

        // The first caller has already gone away; this request shares its
        // probe rather than launching another subprocess.
        let mut waiting = Box::pin(installed(&state, family, true));
        assert!(waiting.as_mut().now_or_never().is_none());
        std::fs::write(dir.path().join("release-1"), "").unwrap();
        let signed_out = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .unwrap();
        assert!(signed_out.current);
        assert_eq!(signed_out.value.state, "signedOut");
        assert_eq!(known_state(&state, family), Some("signedOut"));

        installed_cache(&state, family)
            .lock()
            .unwrap()
            .value
            .as_mut()
            .unwrap()
            .0 = Instant::now() - INSTALLED_MAX_AGE;
        let stale = installed(&state, family, false)
            .now_or_never()
            .expect("An expired read must not wait for its refresh");
        assert!(!stale.current);
        assert_eq!(stale.value.state, "signedOut");
        probe_started(dir.path(), 2).await;
        // Many reads, including model-picker reads, share that one owner.
        let started = Instant::now();
        for _ in 0..100 {
            assert!(installed(&state, family, false).now_or_never().is_some());
            assert_eq!(known_state(&state, family), Some("signedOut"));
        }
        eprintln!("100 stale reads plus picker reads: {:?}", started.elapsed());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("status-runs"))
                .unwrap()
                .lines()
                .count(),
            2
        );

        // Simulate a sign-in/reconnect while the old signed-out probe is
        // held. Invalidation itself must never wait for that subprocess.
        forget_installed(&state, family)
            .now_or_never()
            .expect("Invalidation must not wait for a native check");
        assert_eq!(
            known_state(&other, family),
            Some("signedIn"),
            "invalidating one host must not discard another host's answer"
        );
        std::fs::write(dir.path().join("status-code"), "0").unwrap();
        assert_eq!(known_state(&state, family), None);
        let mut invalidated = Box::pin(installed(&state, family, false));
        assert!(invalidated.as_mut().now_or_never().is_none());
        std::fs::write(dir.path().join("release-2"), "").unwrap();
        // Drive a surviving reader across the discarded result. It must
        // start probe 3, never return probe 2's now-obsolete signed-out answer.
        let waiter = tokio::spawn({
            let state = state.clone();
            async move { installed(&state, family, false).await }
        });
        probe_started(dir.path(), 3).await;
        assert!(invalidated.as_mut().now_or_never().is_none());
        assert!(!waiter.is_finished());
        assert_eq!(known_state(&state, family), None);
        std::fs::write(dir.path().join("release-3"), "").unwrap();
        let current = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(current.current);
        assert_eq!(current.value.state, "signedIn");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), invalidated)
                .await
                .unwrap()
                .value
                .state,
            "signedIn"
        );
        assert_eq!(known_state(&state, family), Some("signedIn"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("status-runs"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        other.projects.shutdown().await;
        state.projects.shutdown().await;
    }

    // Contract: a paired device reads what is installed and signed in plus what
    // wonderd runs, and drives the Mac's Reconnect, which is refused while an
    // agent works. Sign-out is not offered here.
    #[tokio::test]
    async fn paired_devices_read_providers_and_reconnect() {
        let _status = STATUS_TEST_LOCK.lock().await;
        let (dir, mut state, message) = crate::projects::tests::handoff_fixture().await;
        // Claude's native probe is released by the fixture, never by elapsed time.
        let runs = dir.path().join("status-runs");
        let gate = dir.path().join("gate");
        claude_probe(
            &mut state,
            dir.path(),
            "import sys, time\nfrom pathlib import Path\nroot = Path(__file__).parent\nruns = root / 'status-runs'\nrun = len(runs.read_text().splitlines()) + 1 if runs.exists() else 1\nwith runs.open('a') as log: log.write(sys.argv[1] + '\\n')\n(root / f'started-{run}').touch()\nwhile (root / 'gate').exists() and not (root / f'release-{run}').exists(): time.sleep(0.01)\nprint('2.0.0\\t')\nsys.exit(42)\n",
        );
        let (status, _) = call(&state, "GET", "/api/v1/providers", false).await;
        assert_ne!(status, StatusCode::OK, "Only an owner may read providers");
        let (status, body) = call(&state, "GET", "/api/v1/providers?refresh=true", true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        crate::tests::validate_http_contract("providersResponse", &body);
        // A paired phone learns this computer's Cupola version and how to update it;
        // an earlier host sends no `host`, which tells a newer app it is out of date.
        assert_eq!(body["host"]["version"], crate::RELEASE_VERSION);
        assert!(body["host"]["command"]
            .as_str()
            .unwrap()
            .starts_with("brew upgrade"));
        let codex = &body["providers"][0];
        assert_eq!(codex["id"], "codex");
        assert_eq!(codex["version"], "0.155.0-alpha.9");
        assert_eq!(
            codex["working"], true,
            "The fixture's message is still queued"
        );
        assert_eq!(body["providers"][1]["state"], "signedOut");

        state
            .store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("thread"),
                Some("turn"),
            )
            .await
            .unwrap();
        let (_, body) = call(&state, "GET", "/api/v1/providers", true).await;
        assert_eq!(body["providers"][0]["working"], true);
        let path = "/api/v1/providers/codex/reconnect";
        let (status, body) = call(&state, "POST", path, true).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("still working"));
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let (status, body) = call(&state, "POST", path, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["running"], true);
        assert_eq!(body["working"], false);
        crate::tests::validate_http_contract("providerStatus", &body);
        let (status, _) = call(&state, "POST", "/api/v1/providers/other/reconnect", true).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&state, "POST", "/api/v1/providers/codex/sign-out", true).await;
        assert!(status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED);

        // Ordinary reads of an expired cache must not wait for a native probe.
        std::fs::write(&gate, "").unwrap();
        installed_cache(&state, AgentFamily::Claude)
            .lock()
            .unwrap()
            .value
            .as_mut()
            .unwrap()
            .0 = Instant::now() - INSTALLED_MAX_AGE;
        let stale_snapshot =
            futures_util::FutureExt::now_or_never(installed(&state, AgentFamily::Claude, false))
                .expect("A stale cache read must complete without suspending for a probe");
        assert!(!stale_snapshot.current);
        assert_eq!(known_state(&state, AgentFamily::Claude), Some("signedOut"));
        let stale = tokio::time::timeout(
            Duration::from_secs(10),
            call(&state, "GET", "/api/v1/providers", true),
        )
        .await
        .expect("An expired provider cache must answer while the native check runs");
        assert_eq!(stale.0, StatusCode::OK);
        assert_eq!(stale.1["providers"][1]["state"], "signedOut");
        probe_started(dir.path(), 2).await;
        std::fs::write(dir.path().join("release-2"), "").unwrap();
        // Drain that background owner before the explicit-refresh fixture.
        assert!(installed(&state, AgentFamily::Claude, true).await.current);

        // Both real HTTP routes must have subscribed to this owner's probe
        // before it is released. A first poll may yield for an unrelated reason.
        let first = tokio::spawn({
            let state = state.clone();
            async move { call(&state, "GET", "/api/v1/providers?refresh=true", true).await }
        });
        probe_started(dir.path(), 3).await;
        let second = tokio::spawn({
            let state = state.clone();
            async move { call(&state, "GET", "/api/v1/providers?refresh=true", true).await }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while state.installed_providers.waiters[installed_index(AgentFamily::Claude)]
                .load(std::sync::atomic::Ordering::SeqCst)
                != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both device requests must join the held native probe");
        std::fs::write(dir.path().join("release-3"), "").unwrap();
        let (first, second) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(first, second)
        })
        .await
        .expect("both device refreshes must finish after releasing their shared probe");
        let (first, one) = first.unwrap();
        let (second, two) = second.unwrap();
        assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
        assert_eq!(one["providers"][1]["state"], "signedOut");
        assert_eq!(two["providers"][1]["state"], "signedOut");
        let ran = std::fs::read_to_string(&runs).unwrap();
        assert_eq!(
            ran.lines().filter(|l| *l == "status").count(),
            3,
            "initial, stale and shared explicit refresh each own one probe: {ran}"
        );
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// The phone and the daemon give one piece of advice per direction: a
    /// Codex older than Cupola supports needs a Codex update, anything later
    /// needs a Cupola update.
    #[test]
    fn unsupported_codex_advice_depends_on_which_side_is_behind() {
        for (version, advice) in [
            ("0.154.0", "Update ChatGPT (or the Codex CLI)"),
            ("0.155.0-alpha.8", "Update ChatGPT (or the Codex CLI)"),
            ("0.170.0", "Update Cupola"),
            ("0.155.0-alpha.9", "Update Cupola"),
            ("", "Update Cupola"),
        ] {
            assert!(
                unsupported_codex_detail(version).contains(advice),
                "{version}"
            );
        }
    }

    /// A Claude SDK patch the runtime downloaded is "Update ready" on the
    /// phone (its Update now restarts the runtime on it), and Update asks the
    /// runtime to check now. Codex installed some other way is refused with
    /// what to do instead.
    #[tokio::test]
    async fn claude_update_check_reaches_update_ready_and_manual_codex_is_refused() {
        let _status = STATUS_TEST_LOCK.lock().await;
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let entrypoint = dir.path().join("claude-bridge.py");
        std::fs::write(
            &entrypoint,
            r#"import json, sys, os
checked = os.path.join(os.path.dirname(__file__), 'checked')
def status():
    staged = os.path.exists(checked)
    return {'activeVersion': '0.3.283', 'bundledVersion': '0.3.283', 'pendingVersion': '0.3.290' if staged else None,
        'checking': False, 'lastFailure': None, 'newestCompatibleVersion': '0.3.290' if staged else '0.3.283',
        'adapterUpdateVersion': None}
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); result = {}
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'runtime/update/check':
        open(checked, 'w').close(); result = status()
    elif m == 'runtime/update/read': result = status()
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#,
        )
        .unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = std::sync::Arc::new(tokio::sync::Mutex::new(
            wonder_app_server::AppServerClient::unavailable(
                "test".into(),
                crate::ingestion::notification_sink(state.store.clone()),
            ),
        ));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(std::sync::Arc::new(crate::claude::Runtime {
            client,
            config,
        }));
        let claude = |body: &serde_json::Value| {
            body["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == "claude")
                .unwrap()
                .clone()
        };
        let (status, body) = call(&state, "GET", "/api/v1/providers", true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        crate::tests::validate_http_contract("providersResponse", &body);
        let before = claude(&body);
        assert_eq!(before["updatePending"], false);
        assert_eq!(before["update"]["owner"], "cupola");
        assert_eq!(before["update"]["canUpdate"], true, "{before}");
        assert_eq!(before["update"]["available"], false);

        let (status, body) = call(&state, "POST", "/api/v1/providers/claude/update", true).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let deadline = Instant::now() + Duration::from_secs(10);
        let after = loop {
            let (_, body) = call(&state, "GET", "/api/v1/providers", true).await;
            let entry = claude(&body);
            if entry["update"]["status"] != "updating" || Instant::now() > deadline {
                crate::tests::validate_http_contract("providersResponse", &body);
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(after["update"]["status"], "ready", "{after}");
        assert_eq!(
            after["update"]["message"],
            "The Claude update is ready. Cupola switches to it when agents finish."
        );
        assert_eq!(after["update"]["available"], true);
        assert_eq!(after["updatePending"], true, "the phone shows Update ready");

        let (status, body) = call(&state, "POST", "/api/v1/providers/codex/update", true).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body["detail"],
            "Update Codex on your computer with the app or tool that installed it."
        );
        if let Some(runtime) = &state.claude {
            let _ = runtime.client.lock().await.shutdown().await;
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: providers.list runs native status probes from the configured
    // runtime paths.
    #[tokio::test]
    async fn providers_list_reads_signed_in_native_runtimes_without_bundle_script() {
        let _status = STATUS_TEST_LOCK.lock().await;
        let mut fx = crate::provider_status_tests::Fixture::new().await;
        let entrypoint = fx.dir.path().join("main.mjs");
        std::fs::write(
            entrypoint.with_file_name("manage.mjs"),
            "import os, sys\nassert sys.argv[1] == 'status'\nassert os.environ['WONDER_DATA_DIR'] == os.path.dirname(__file__)\nprint('2.1.288\\tClaude Max')\n",
        )
        .unwrap();
        fx.state.claude = Some(std::sync::Arc::new(crate::claude::Runtime {
            client: std::sync::Arc::new(tokio::sync::Mutex::new(
                wonder_app_server::AppServerClient::unavailable(
                    "test".into(),
                    crate::ingestion::notification_sink(fx.state.store.clone()),
                ),
            )),
            config: wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint,
                state_dir: fx.dir.path().to_path_buf(),
                npm_cli: None,
                wonder_version: "test".into(),
                family: "claude",
                claude_config_dir: None,
            },
        }));
        std::fs::write(
            fx.dir.path().join("catalog.mjs"),
            "import sys\nassert sys.argv[1] == 'status'\nprint('1.18.27\\t')\nsys.exit(46)\n",
        )
        .unwrap();
        let claude = fx.state.claude.as_ref().unwrap();
        let mut config = claude.config.clone();
        config.family = "opencode";
        fx.state.opencode = Some(std::sync::Arc::new(crate::claude::Runtime {
            client: claude.client.clone(),
            config,
        }));
        // Fixture data_dir need not be the runtime directory in other tests.
        fx.state.data_dir = fx.dir.path().to_path_buf();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = crate::router(fx.state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = crate::rpc::client::Client::connect(
            &format!("ws://{address}/api/v1/rpc"),
            &[(
                "x-wonder-loopback-capability",
                &fx.state.loopback_capability,
            )],
        )
        .await
        .unwrap();
        client.initialize("native-status-contract").await.unwrap();
        let body = client
            .call("providers.list", json!({"refresh":true}))
            .await
            .unwrap();
        crate::tests::validate_http_contract("providersResponse", &body);
        assert_eq!(body["providers"][0]["state"], "signedIn", "{body}");
        assert_eq!(body["providers"][0]["account"], "ChatGPT account");
        assert_eq!(body["providers"][1]["state"], "signedIn", "{body}");
        assert_eq!(body["providers"][1]["account"], "Claude subscription");
        assert_eq!(body["providers"][2]["state"], "installed", "{body}");
        assert_eq!(body["providers"][2]["version"], "1.18.27");
        // Native CLI status reflects a sign-out even if a long-lived runtime
        // retained the former sign-in. Only this computer's home is probed.
        std::fs::remove_file(fx.state.codex_home.join("fake-sign-in.json")).unwrap();
        let signed_out = client
            .call("providers.list", json!({"refresh":true}))
            .await
            .unwrap();
        assert_eq!(signed_out["providers"][0]["state"], "signedOut");
        let homes = std::fs::read_to_string(fx.dir.path().join("codex-cli-status.log")).unwrap();
        assert!(homes
            .lines()
            .all(|home| std::path::Path::new(home) == fx.state.codex_home));
        drop(client);
        server.abort();
        fx.shutdown().await;
    }

    #[test]
    fn a_replaced_install_has_a_new_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("codex");
        std::fs::write(&launcher, "one").unwrap();
        let first = fingerprint(&launcher).unwrap();
        assert_eq!(fingerprint(&launcher), Some(first.clone()));
        // Sparkle swaps the bundle: a new file takes the old path.
        std::fs::remove_file(&launcher).unwrap();
        std::fs::write(&launcher, "two!").unwrap();
        assert_ne!(fingerprint(&launcher), Some(first));
        assert_eq!(fingerprint(&dir.path().join("missing")), None);
    }

    #[test]
    fn a_codex_that_runs_is_warned_about_not_refused() {
        assert_eq!(codex_warning("codex-cli 0.166.0-alpha.6", &[]), None);
        assert!(codex_warning("codex-cli 0.170.0", &[])
            .unwrap()
            .starts_with("This Codex is newer than the versions Cupola was tested with."));
        let off = codex_warning(
            "codex-cli 0.166.0-alpha.6",
            &["Message search".into(), "Reviews".into()],
        )
        .unwrap();
        assert_eq!(
            off,
            "Chats work. These features are off until Cupola is updated on your computer: Message search, Reviews."
        );
    }

    #[test]
    fn an_older_codex_is_told_to_update_codex_not_cupola() {
        assert!(unsupported_codex_detail("codex-cli 0.100.0").starts_with("This Codex is older"));
        assert_eq!(
            unsupported_codex_detail("codex-cli 0.170.0"),
            crate::ingestion::CODEX_UNSUPPORTED_DETAIL
        );
    }
}
