//! Owner-selected source-folder projects on this Mac. A project thread is the
//! owner's native Codex or Claude Code session in its normal provider store, so
//! it continues in the Codex app/CLI or Claude Code.
use crate::{
    agent_defaults, history, publish_message_state, AppState, AuthenticatedDevice, DeliveryState,
    OwnerAuthority,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use wonder_app_server::{AppServerClient, JsonRpcError, LaunchConfig, RpcClient};
use wonder_store::{
    AgentFamily, BackgroundWork, MessageInsert, ProjectConversationCreate,
    ProjectConversationInsert, ProjectConversationPatch, ProjectCreate, ProjectRootInput,
    StoredProject, StoredProjectConversation,
};

mod archive_state;
pub mod auto_archive;
mod task_summary;
use archive_state::ArchiveState;
pub(crate) mod identity;

pub const FEATURE: &str = "projects-v1";
/// Advertised in `GET /api/v1/projects`: the host accepts `claudeApproval` and
/// `planMode`, and lists pinned threads across included projects.
const MODES_VERSION: u8 = 1;
const MAX_PINNED_THREADS: i64 = 50;
pub(crate) const CODEX_SOURCE_KINDS: [&str; 4] = ["cli", "vscode", "exec", "appServer"];
/// How long after creating a thread Wonder lists it from its own record if the
/// provider does not list it yet.
const JUST_STARTED_SECONDS: i64 = 300;
const CURSOR_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_CURSORS: usize = 128;
const RECEIPT_PAGES_PER_CYCLE: usize = 20;
const RECEIPT_RESCAN_DELAY: Duration = Duration::from_secs(60);
const IDLE_FINAL_ROSTER_TIMEOUT: Duration = Duration::from_secs(2);
// The measured native idle poll is about 9 seconds. Allow ample scheduling
// slack, but never let a lost enqueue receipt indefinitely block runtime updates.
const NATIVE_QUEUE_RUNTIME_HOLD: Duration = Duration::from_secs(10 * 60);
const ACCESS_MODES: [&str; 3] = ["read_only", "workspace", "full_access"];

struct ReceiptScan {
    thread_id: String,
    client_message_id: String,
    /// A native-queue send reads history only after a definite event: first
    /// sight in this process, Wonder taking the thread over, the desktop
    /// finishing a turn in it, or hold expiry. Pending until a sweep ends.
    check: bool,
    /// The chat's event count this scan has consumed; a newer count, even
    /// one bumped while this scan was in flight, means check again.
    events: u64,
    expiry_checked: bool,
    /// The desktop's latest turn and status, from its local records.
    desktop_turn: Option<(String, &'static str)>,
    cursor: Option<String>,
    seen: HashSet<String>,
    pages_scanned: usize,
    resume_after: Instant,
    notice: Option<&'static str>,
}

impl ReceiptScan {
    fn new(message: &wonder_store::StoredMessage, events: u64) -> Self {
        Self {
            thread_id: message.codex_thread_id.clone().unwrap_or_default(),
            client_message_id: message.client_message_id.clone(),
            check: true,
            events,
            expiry_checked: false,
            desktop_turn: None,
            cursor: None,
            seen: HashSet::new(),
            pages_scanned: 0,
            resume_after: Instant::now(),
            notice: None,
        }
    }

    fn reset_after(&mut self, delay: Duration) {
        self.cursor = None;
        self.seen.clear();
        self.pages_scanned = 0;
        self.resume_after = Instant::now() + delay;
    }
}

enum ReceiptScanStep {
    Pending,
    Accepted(String),
    Notice(&'static str),
    /// History was read to its end and holds no turn for this message.
    Absent,
}

/// The normal-home Codex client serves project threads and shared discovery.
pub struct ProjectRuntime {
    pub(crate) codex: Arc<Mutex<AppServerClient>>,
    /// The Codex binary is re-located when ChatGPT moves or updates it.
    pub(crate) config: std::sync::Mutex<LaunchConfig>,
    start: Mutex<()>,
    pub codex_store: String,
    pub claude_store: String,
    cursors: std::sync::Mutex<HashMap<String, (Instant, CatalogCursor)>>,
    /// Held only to look up or store; never across a provider read.
    archives: std::sync::Mutex<HashMap<Vec<String>, CachedArchives>>,
    /// One provider read per folder selection; other selections run alongside.
    archive_reads: std::sync::Mutex<HashMap<Vec<String>, std::sync::Weak<Mutex<()>>>>,
    /// The last archive answer per (store, native thread), for display only
    /// (`archive_state::displayed`).
    archive_known: std::sync::Mutex<HashMap<(String, String), bool>>,
    /// Folder selections whose provider archive read runs for a list.
    archive_refreshes: std::sync::Mutex<HashSet<Vec<String>>>,
    visibility: Mutex<Option<(i64, Instant, HashSet<String>)>>,
    worktrees: Arc<std::sync::Mutex<HashMap<String, CachedWorktrees>>>,
    /// Orders `project_worktrees` writes so an older discovery never
    /// overwrites a newer one's rows.
    worktree_writes: Mutex<()>,
    /// The checked-out branch per working folder (`thread_branches`).
    branches: Arc<std::sync::Mutex<HashMap<String, CachedBranch>>>,
    /// Progress for uncertain Project sends. An interrupted host starts at the
    /// newest page again; it never retries the send from this cache.
    receipt_scans: std::sync::Mutex<HashMap<String, ReceiptScan>>,
    /// Ownership events per chat that settle its native-queue sends.
    native_queue_events: std::sync::Mutex<HashMap<String, u64>>,
    native_queue_capability: Mutex<Option<(String, bool)>>,
    /// Latest dispatch problem per conversation, in owner-facing language.
    notices: std::sync::Mutex<HashMap<String, String>>,
    /// Runtime generation that accepted each running turn, with when it was
    /// recorded. A changed or dead generation means the turn must be
    /// reconciled from native history. Recovery drops settled turns.
    generations: std::sync::Mutex<HashMap<String, (String, Instant)>>,
    /// Running turns that complete native history does not list, with the
    /// turn id and when recovery first found it missing (see `live_turn`).
    unlisted_turns: std::sync::Mutex<HashMap<String, (String, Instant)>>,
    /// Messages claimed by a dispatch that is preparing or submitting them.
    /// Preparation (taking the chat over from Claude on the Mac, reopening a
    /// thread) and the turn/start wait run outside `dispatch_lock`, so claim
    /// recovery skips these.
    dispatching: std::sync::Mutex<HashSet<String>>,
    /// Wakes waiters each time a dispatch releases its claim.
    dispatch_done: tokio::sync::Notify,
    /// Send/Guide/Stop ordering within a chat, including a provider wait that
    /// releases the shared lifecycle lock. Idle entries do not retain chats.
    conversation_dispatch: std::sync::Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    /// Archive, restore and an archive's follow-up after a send, per chat.
    /// Taken before any dispatch lock and never while holding one.
    archive_locks: std::sync::Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    /// Conversations being rewound (Edit from here). Their new messages stay
    /// queued until the rewind is saved (`crate::rewind`).
    rewinding: std::sync::Mutex<HashSet<String>>,
    /// Send now on a chat another app on the Mac holds: the message that
    /// asked, per conversation. Its waiting messages stop waiting for the Mac
    /// and the next dispatch ends that app's process for the chat first. Not
    /// durable and short-lived: after a restart, or once `TAKE_OVER_WINDOW`
    /// passes without a dispatch, the message waits as before.
    takeovers: std::sync::Mutex<HashMap<String, (String, Instant)>>,
    /// Send now on a thread T3 Code takes messages into: the message, with
    /// the turn it was sent into and when, steers that turn while it is the
    /// thread's newest (`deliver_through_t3_code`). Not durable: after a
    /// restart, or once `TAKE_OVER_WINDOW` passes, it is queued instead.
    t3_steers: std::sync::Mutex<HashMap<String, (String, Instant)>>,
    /// Take over on a thread T3 Code takes messages into, per conversation,
    /// with when: T3 Code ends its turn and starts one with the next waiting
    /// message. Kept apart from `takeovers`, which quits the app holding the
    /// chat: T3 Code changes its own turn and nothing on the Mac is stopped.
    t3_restarts: std::sync::Mutex<HashMap<String, Instant>>,
    /// Turns (thread, turn) the owner asked to stop, with when. The provider's
    /// own terminal status ends the turn; this only tells recovery that an
    /// interrupted turn was the owner's Stop rather than a lost runtime.
    stop_requests: std::sync::Mutex<HashMap<(String, String), Instant>>,
    /// Wonder's skill folder as set on the Codex runtime, with the runtime
    /// generation it was set on (see `wonder_skills`).
    wonder_skill_roots: std::sync::Mutex<Option<(String, Vec<String>)>>,
    /// Message search over native history (`crate::native_search`).
    pub(crate) native_search: Arc<crate::native_search::NativeSearch>,
    /// Whether an attached chat still runs tasks after its reply, for list
    /// rows (`task_summary`).
    task_summaries: task_summary::TaskSummaries,
}

/// What Stop answers when the request could not be saved (nothing was sent).
pub(crate) const STOP_NOT_SAVED: &str = "Cupola couldn't save your Stop. Try again.";

/// The owner's Stop on a turn, saved before the provider is asked to stop it,
/// so a host restart before the provider's terminal status still reads the
/// turn as stopped rather than lost. A Stop that cannot be saved is an error:
/// Stop must not answer for a turn recovery would then continue. `true` when
/// this call made the Stop, so a refused interrupt withdraws only its own and
/// never an earlier, accepted one.
pub(crate) async fn request_stop(
    state: &AppState,
    thread: &str,
    turn: &str,
) -> Result<bool, sqlx::Error> {
    let earlier = stop_requested(state, thread, turn).await?;
    state
        .store
        .record_turn_stop(thread, turn, Utc::now().timestamp())
        .await
        .inspect_err(|error| {
            let _ = state.logger.record(
                "error",
                "turn_stop_not_saved",
                json!({"threadId": thread, "turnId": turn, "error": error.to_string()}),
            );
        })?;
    state.projects.cache_stop(thread, turn);
    Ok(!earlier)
}

/// Whether the owner stopped this turn, including before a host restart. A
/// store that cannot answer is an error, never "no": callers defer.
pub(crate) async fn stop_requested(
    state: &AppState,
    thread: &str,
    turn: &str,
) -> Result<bool, sqlx::Error> {
    if state.projects.stop_cached(thread, turn) {
        return Ok(true);
    }
    state
        .store
        .turn_stop_recorded(thread, turn)
        .await
        .inspect_err(|error| {
            let _ = state.logger.record(
                "error",
                "turn_stop_unreadable",
                json!({"threadId": thread, "turnId": turn, "error": error.to_string()}),
            );
        })
}

/// A turn that ended, or a Stop the provider refused, has no Stop left to
/// remember.
pub(crate) async fn clear_stop(state: &AppState, thread: &str, turn: &str) {
    state
        .projects
        .stop_requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(thread.to_owned(), turn.to_owned()));
    let _ = state.store.clear_turn_stop(thread, turn).await;
}

/// A per-chat lock from `locks`; idle entries do not retain chats.
pub(crate) async fn keyed_lock(
    locks: &std::sync::Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    key: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.strong_count() != 0);
        match locks.get(key).and_then(std::sync::Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key.to_owned(), Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

impl ProjectRuntime {
    /// Take before `dispatch_lock`; never wait for a chat while holding the
    /// shared lock, since its pending receipt must reacquire that lock.
    pub(crate) async fn conversation_dispatch(
        &self,
        conversation: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        keyed_lock(&self.conversation_dispatch, conversation).await
    }

    async fn archive_lock(&self, conversation: &str) -> tokio::sync::OwnedMutexGuard<()> {
        keyed_lock(&self.archive_locks, conversation).await
    }

    /// Claimed messages whose dispatch is still running. Read under
    /// `dispatch_lock`, where claims are made, so the two agree.
    pub(crate) fn dispatching(&self) -> Vec<String> {
        self.dispatching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Records the runtime that accepted a turn, as dispatch does (test rigs).
    #[cfg(all(test, unix))]
    pub(crate) fn record_turn_generation(&self, message: &str, runtime: &str) {
        self.generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(message.into(), (runtime.into(), Instant::now()));
    }

    /// Notes whether complete native history lists a running turn and returns
    /// since when it has been missing. The one writer of `unlisted_turns`.
    fn observe_listing(
        &self,
        message: &str,
        turn: &str,
        listed: bool,
        now: Instant,
    ) -> Option<Instant> {
        let mut unlisted = self
            .unlisted_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if listed {
            unlisted.remove(message);
            return None;
        }
        let entry = unlisted
            .entry(message.to_owned())
            .or_insert_with(|| (turn.to_owned(), now));
        if entry.0 != turn {
            *entry = (turn.to_owned(), now);
        }
        Some(entry.1)
    }

    /// Starts a rewind of `conversation`; false when one is already running.
    /// Taken under `dispatch_lock`, where dispatch claims messages.
    pub(crate) fn begin_rewind(&self, conversation: &str) -> bool {
        self.rewinding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conversation.to_owned())
    }

    pub(crate) fn end_rewind(&self, conversation: &str) {
        self.rewinding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(conversation);
    }

    pub(crate) fn is_rewinding(&self, conversation: &str) -> bool {
        self.rewinding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(conversation)
    }

    fn cache_stop(&self, thread: &str, turn: &str) {
        let mut stops = self.stop_requests.lock().unwrap_or_else(|e| e.into_inner());
        // The store keeps the Stop across a restart; this only answers
        // without a read. An hour bounds it without a separate sweep.
        stops.retain(|_, at| at.elapsed() < Duration::from_secs(3600));
        stops.insert((thread.to_owned(), turn.to_owned()), Instant::now());
    }

    /// What a host restart does to the in-memory side of a Stop (test rigs).
    #[cfg(all(test, unix))]
    pub(crate) fn forget_cached_stops(&self) {
        self.stop_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn stop_cached(&self, thread: &str, turn: &str) -> bool {
        self.stop_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&(thread.to_owned(), turn.to_owned()))
    }

    pub fn configured(
        codex_bin: PathBuf,
        wonder_version: String,
        codex_home: &FsPath,
        claude_home: &FsPath,
        sink: wonder_app_server::NotificationSink,
    ) -> Arc<Self> {
        Arc::new(Self {
            codex: Arc::new(Mutex::new(AppServerClient::unavailable(
                wonder_version.clone(),
                sink,
            ))),
            config: std::sync::Mutex::new(LaunchConfig {
                codex_bin,
                wonder_version,
                codex_home: None,
            }),
            start: Mutex::new(()),
            codex_store: store_key("codex", codex_home),
            claude_store: store_key("claude", claude_home),
            cursors: std::sync::Mutex::new(HashMap::new()),
            archives: Default::default(),
            archive_reads: Default::default(),
            archive_known: Default::default(),
            archive_refreshes: Default::default(),
            visibility: Mutex::new(None),
            worktrees: Arc::default(),
            worktree_writes: Mutex::new(()),
            branches: Arc::default(),
            receipt_scans: std::sync::Mutex::new(HashMap::new()),
            native_queue_events: std::sync::Mutex::new(HashMap::new()),
            native_queue_capability: Mutex::new(None),
            notices: std::sync::Mutex::new(HashMap::new()),
            generations: std::sync::Mutex::new(HashMap::new()),
            unlisted_turns: std::sync::Mutex::new(HashMap::new()),
            dispatching: std::sync::Mutex::new(HashSet::new()),
            conversation_dispatch: std::sync::Mutex::new(HashMap::new()),
            dispatch_done: tokio::sync::Notify::new(),
            archive_locks: std::sync::Mutex::new(HashMap::new()),
            rewinding: std::sync::Mutex::new(HashSet::new()),
            takeovers: std::sync::Mutex::new(HashMap::new()),
            t3_steers: std::sync::Mutex::new(HashMap::new()),
            t3_restarts: std::sync::Mutex::new(HashMap::new()),
            stop_requests: std::sync::Mutex::new(HashMap::new()),
            wonder_skill_roots: std::sync::Mutex::new(None),
            native_search: crate::native_search::NativeSearch::new(claude_home.join("projects")),
            task_summaries: Default::default(),
        })
    }

    pub(crate) fn wonder_skill_roots(&self) -> Option<(String, Vec<String>)> {
        self.wonder_skill_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn set_wonder_skill_roots(&self, roots: Option<(String, Vec<String>)>) {
        *self
            .wonder_skill_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = roots;
    }

    pub async fn shutdown(&self) {
        let _ = self.codex.lock().await.shutdown().await;
    }

    /// Start the next Codex runtime from `codex_bin`.
    pub(crate) fn set_codex_bin(&self, codex_bin: PathBuf) {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .codex_bin = codex_bin;
    }

    /// The Codex the next runtime starts from.
    pub(crate) fn codex_bin(&self) -> PathBuf {
        self.launch_config().codex_bin
    }

    pub(crate) fn launch_config(&self) -> LaunchConfig {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) async fn codex_alive(&self) -> bool {
        self.codex.lock().await.health().is_alive()
    }

    /// Stop the Projects Codex runtime so the next request starts the
    /// installed one. Callers check that no Codex turn is running.
    pub(crate) async fn stop_codex(&self) {
        let _start = self.start.lock().await;
        let _ = self.codex.lock().await.shutdown().await;
    }
}

/// Opaque identity for a provider home. The path never leaves the host.
pub(crate) fn store_key(family: &str, home: &FsPath) -> String {
    // Hashes the path as it has always been spelled (`\\?\C:\…` on Windows)
    // so an existing home keeps its identity.
    let canonical =
        crate::canonical_path::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let digest = hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()));
    format!("{family}:{}", &digest[..16])
}

pub async fn codex_rpc(state: &AppState) -> Result<RpcClient, String> {
    let runtime = &state.projects;
    let _start = runtime.start.lock().await;
    let mut client = runtime.codex.lock().await;
    if !client.health().is_alive() {
        let config = runtime.launch_config();
        let identity = |metadata: std::fs::Metadata| (metadata.len(), metadata.modified().ok());
        let before = tokio::fs::metadata(&config.codex_bin)
            .await
            .ok()
            .map(identity);
        let queue_supported =
            wonder_app_server::supports_native_thread_queue(&config.codex_bin).await;
        if let Err(error) = client.restart(config.clone()).await {
            let incompatible = error.is_incompatible();
            state
                .ingestion
                .set_runtime_incompatible(AgentFamily::Codex, incompatible);
            return Err(if incompatible {
                // The technical reason (failing contract path) stays in the log.
                let _ = state.logger.record(
                    "error",
                    "codex_runtime_incompatible",
                    serde_json::json!({"error": error.to_string()}),
                );
                crate::ingestion::CODEX_UNSUPPORTED_DETAIL.to_owned()
            } else {
                crate::provider_updates::codex_start_failure(&config.codex_bin).to_owned()
            });
        }
        let after = tokio::fs::metadata(&config.codex_bin)
            .await
            .ok()
            .map(identity);
        // Bind the optional experimental contract to the process just launched.
        // A bundle update while it runs cannot enable it on an older process;
        // a binary replacement during this launch disables it until restart.
        *runtime.native_queue_capability.lock().await = Some((
            client.health().id().to_owned(),
            queue_supported && before.is_some() && before == after,
        ));
        state
            .ingestion
            .set_runtime_incompatible(AgentFamily::Codex, false);
        if let Err(error) = crate::rediscover_runtime(state, &mut client).await {
            let _ = client.shutdown().await;
            return Err(error);
        }
        state
            .ingestion
            .register_project_runtime(&runtime.codex, client.health());
    }
    Ok(client.rpc())
}

async fn claude_rpc(state: &AppState) -> Result<RpcClient, String> {
    bridge_rpc(state, AgentFamily::Claude).await
}

async fn bridge_rpc(state: &AppState, family: AgentFamily) -> Result<RpcClient, String> {
    let provider = crate::claude::bridge_runtime(state, family)
        .ok_or("This provider is not available. Run cupola providers on your computer.")?;
    let mut client = provider.client.lock().await;
    let started = !client.health().is_alive();
    if started {
        client
            .restart_bridge(provider.config.clone())
            .await
            .map_err(|e| e.to_string())?;
    }
    // A runtime is never used before its catalog is validated: once after it
    // starts, and again after a check found the catalog unusable.
    if started || state.ingestion.needs_validation(family) {
        match crate::claude::discover(state, &mut client, family).await {
            Ok(()) => {
                state.ingestion.set_setup_issue(family, None);
                state.ingestion.set_needs_validation(family, false);
            }
            // The runtime works; the provider is not set up. Restarting it
            // would only find the same answer, so it stays up, refusing work
            // until a re-check finds the provider usable.
            Err(crate::claude::DiscoveryFailure::NotSetUp { issue, .. }) => {
                state.ingestion.set_setup_issue(family, Some(issue));
                state.ingestion.set_needs_validation(family, false);
                state
                    .ingestion
                    .register_provider(family, &provider.client, client.health());
                return Err(issue.detail(family).into());
            }
            // The runtime answered, but its catalog or account is unusable
            // right now. Its transport is healthy, so it stays up, refusing
            // work with the real reason until recovery validates it again.
            Err(failure @ crate::claude::DiscoveryFailure::Runtime(_))
                if client.health().is_alive() =>
            {
                state.ingestion.set_needs_validation(family, true);
                state
                    .ingestion
                    .register_provider(family, &provider.client, client.health());
                return Err(failure.message(family));
            }
            Err(failure) => {
                let _ = client.shutdown().await;
                return Err(failure.message(family));
            }
        }
    } else if let Some(issue) = state.ingestion.setup_issue(family) {
        // A running runtime is never used before its provider is validated.
        return Err(issue.detail(family).into());
    }
    state
        .ingestion
        .register_provider(family, &provider.client, client.health());
    Ok(client.rpc())
}

/// The provider's RPC client only while it is already running, for reads that
/// must not start a runtime.
pub(crate) async fn live_rpc_for(state: &AppState, family: AgentFamily) -> Option<RpcClient> {
    match family {
        AgentFamily::Codex => {
            let client = state.projects.codex.lock().await;
            client.health().is_alive().then(|| client.rpc())
        }
        AgentFamily::Claude => {
            let client = crate::claude::client(state, family).ok()?;
            let client = client.lock().await;
            client.health().is_alive().then(|| client.rpc())
        }
        AgentFamily::OpenCode | AgentFamily::Cursor => acp_rpc(state, family).await.ok(),
    }
}

pub(crate) async fn rpc_for(state: &AppState, family: AgentFamily) -> Result<RpcClient, String> {
    match family {
        AgentFamily::Codex => codex_rpc(state).await,
        AgentFamily::Claude => claude_rpc(state).await,
        AgentFamily::OpenCode | AgentFamily::Cursor => bridge_rpc(state, family).await,
    }
}

async fn acp_rpc(state: &AppState, family: AgentFamily) -> Result<RpcClient, String> {
    let rpc = crate::claude::client(state, family)?.lock().await.rpc();
    if !rpc.health().is_alive() {
        return Err(format!(
            "{} is starting on your computer. Try again shortly.",
            family.provider().display_name
        ));
    }
    Ok(rpc)
}

// Recovery can restart a dead Codex runtime while obtaining its RPC client.
// Admit that setup before an update begins; native history reads stay outside
// the barrier so a long scan cannot delay the update handoff.
async fn recovery_rpc_for(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Option<RpcClient> {
    let _admission = state.update_admission.claim_guard().await?;
    crate::providers::rpc_for_store(state, conversation.family, &conversation.provider_store)
        .await
        .ok()
}

use crate::providers::provider_store;

pub(crate) fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

fn codex_active_writer_conflict(message: &str) -> bool {
    message.starts_with("thread ") && message.ends_with(" already has an active writer")
}

pub(crate) async fn result(rpc: &RpcClient, method: &str, params: Value) -> Result<Value, String> {
    let response = rpc
        .request(method, params)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(error) = response.error {
        return Err(error.message);
    }
    response
        .result
        .ok_or_else(|| format!("{method} returned no result"))
}

// Native protocol rejection codes prove the request was not accepted. The
// Claude bridge also identifies its own pre-admission refusals explicitly;
// arbitrary provider wording cannot make a submitted message safe to resend.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputRejection {
    Wait,
    Retry,
}

pub(crate) fn input_rejection(
    family: AgentFamily,
    method: &str,
    error: &JsonRpcError,
) -> Option<InputRejection> {
    if matches!(error.code, -32602..=-32600)
        || (family == AgentFamily::Codex && error.code == -32001)
    {
        return Some(InputRejection::Retry);
    }
    // Claude and the ACP bridges (OpenCode, Cursor) type their refusals alike.
    if family == AgentFamily::Codex || error.code != -32000 {
        return None;
    }
    let field = match method {
        "turn/start" => "turnStart",
        "turn/steer" => "turnSteer",
        _ => return None,
    };
    let admission = error.data.as_ref()?.get(field)?;
    if admission.get("accepted")?.as_bool()? {
        return None;
    }
    match admission.get("action")?.as_str()? {
        "wait" => Some(InputRejection::Wait),
        "retry" => Some(InputRejection::Retry),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Presentation contracts
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderSummary {
    id: String,
    path: String,
    name: String,
    is_primary: bool,
    is_available: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectSummary {
    id: String,
    name: String,
    is_included: bool,
    is_pinned: bool,
    roots_revision: i64,
    folders: Vec<FolderSummary>,
    last_family: Option<AgentFamily>,
    last_used_at: Option<String>,
    created_at: String,
    /// This Project's own auto-archive rule; omitted when it follows the
    /// computer's.
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_archive: Option<AutoArchiveSetting>,
    /// The model last chosen here for each provider: a new chat without a
    /// choice of its own starts from it, ahead of Settings → Default models.
    last_models: Vec<LastModelSummary>,
    /// The repository the primary folder belongs to, for showing the same
    /// repository on other computers together. Grouping only, never access.
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<identity::Repository>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastModelSummary {
    family: AgentFamily,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

/// An auto-archive rule as the phone reads and writes it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AutoArchiveSetting {
    idle_days: i64,
    on_merge: bool,
}

impl From<&wonder_store::AutoArchiveRule> for AutoArchiveSetting {
    fn from(rule: &wonder_store::AutoArchiveRule) -> Self {
        Self {
            idle_days: rule.idle_days,
            on_merge: rule.on_merge,
        }
    }
}

fn folder_name(path: &str) -> String {
    FsPath::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// A Project as the phone reads it, with its repository identity.
async fn summary(project: &StoredProject) -> ProjectSummary {
    project_summary(project, identity::repository(project).await)
}

/// Summaries in order, reading at most a few repository identities at once.
async fn summaries(projects: &[StoredProject]) -> Vec<ProjectSummary> {
    use futures_util::StreamExt;
    futures_util::stream::iter(projects)
        .map(summary)
        .buffered(identity::CONCURRENCY)
        .collect()
        .await
}

fn project_summary(
    project: &StoredProject,
    repository: Option<identity::Repository>,
) -> ProjectSummary {
    ProjectSummary {
        id: project.id.clone(),
        name: project.name.clone(),
        is_included: project.is_included,
        is_pinned: project.pin_order.is_some(),
        roots_revision: project.roots_revision,
        folders: project
            .roots
            .iter()
            .map(|root| FolderSummary {
                id: root.id.clone(),
                path: root.path.clone(),
                name: folder_name(&root.path),
                is_primary: root.id == project.primary_root_id,
                is_available: FsPath::new(&root.canonical_path).is_dir(),
            })
            .collect(),
        last_family: project.last_family,
        last_used_at: project.last_used_at.clone(),
        created_at: project.created_at.clone(),
        auto_archive: project.auto_archive.as_ref().map(AutoArchiveSetting::from),
        last_models: project
            .last_models
            .iter()
            .map(|last| LastModelSummary {
                family: last.family,
                model: last.model.clone(),
                effort: last.effort.clone(),
            })
            .collect(),
        repository,
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectThreadSummary {
    /// Opaque catalog reference, accepted only by this project's attach route.
    reference: String,
    conversation_id: Option<String>,
    title: String,
    family: AgentFamily,
    updated_at: i64,
    is_pinned: bool,
    has_unread: bool,
    is_working: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_computer_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_activity: Option<Value>,
    /// The agent waits on the Mac itself (a permission prompt or question
    /// shown in its desktop app), not on a request Wonder can answer.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    needs_input_on_mac: bool,
    /// A macOS permission, password or download dialog is open on the Mac
    /// while this thread's turn runs (`mac_dialogs`), or T3 Code's turn
    /// waits on a permission there (`t3_code::questions`). Only the person
    /// at the Mac can answer it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    waiting_on_mac_permission: bool,
    /// The branch checked out in the thread's working folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// The agent still runs something after its reply ended: background
    /// agents, commands or terminals. Omitted otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    background_work: bool,
    /// A T3 Code `delegate_task` child whose parent chat T3 Code no longer
    /// has. Children of a known parent are not listed: they show in the
    /// parent's agent tasks (`project_subagents`). Omitted otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_subagent: bool,
    /// When an automation last posted into this thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_automation_at: Option<String>,
    /// That latest automation run, including why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    automation_run: Option<AutomationRunMark>,
    /// A usage limit stopped the thread; Cupola continues it then (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    resumes_at: Option<String>,
    /// The working folder `branch` is read from; not sent.
    #[serde(skip)]
    cwd: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AutomationRunMark {
    automation_id: String,
    /// Null if the automation record is gone.
    name: Option<String>,
    /// running, completed or failed.
    status: String,
    started_at: String,
    finished_at: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectConversationDetail {
    /// T3 owns these modes; Cupola's access settings do not describe them.
    #[serde(skip_serializing_if = "Option::is_none")]
    t3_code_permissions: Option<crate::t3_code::Permissions>,
    conversation_id: String,
    project_id: String,
    project_name: String,
    title: String,
    family: AgentFamily,
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    access_mode: String,
    claude_approval: String,
    plan_mode: bool,
    working_folder: String,
    working_folder_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_label: Option<String>,
    is_pinned: bool,
    has_unread: bool,
    has_native_session: bool,
    is_archived: bool,
    folder_in_project: bool,
    /// History can be opened without granting its working folder to a Project.
    #[serde(skip_serializing_if = "Option::is_none")]
    read_only_reason: Option<String>,
    notice: Option<String>,
    /// A usage limit stopped the thread; Cupola continues it then (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    resumes_at: Option<String>,
}

pub(crate) async fn conversation_detail(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<ProjectConversationDetail, String> {
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("The project is unavailable")?;
    // The model and effort a send runs with, as dispatch resolves them: a
    // thread that never chose them shows the Mac's default, never a guess.
    let (model, effort) = dispatch_settings(
        &*state.runtime_catalog.read().await,
        conversation.family,
        conversation.model.as_deref(),
        conversation.effort.as_deref(),
        conversation.service_tier.as_deref(),
    )
    .map(|(model, effort, _)| (Some(model), effort))
    .unwrap_or_else(|_| (conversation.model.clone(), conversation.effort.clone()));
    let folders = project_folders(state, &project).await;
    // Opening a chat or saving its settings never fails on an archive check:
    // an unknown state shows the last one Cupola saw and says it is unchecked.
    let archive = if conversation.wonder_archived {
        ArchiveState::Archived
    } else {
        let current = archive_state::display_state(state, conversation).await;
        archive_state::displayed(state, conversation, current)
    };
    let notice = notice(state, &conversation.conversation_id).or_else(|| {
        (archive == ArchiveState::Unknown).then(|| ARCHIVE_UNCHECKED_NOTICE.to_owned())
    });
    Ok(ProjectConversationDetail {
        t3_code_permissions: match conversation.native_session_id.as_deref() {
            Some(native) => state.t3_code.permissions(conversation.family, native).await,
            // An OpenCode/Cursor chat started in T3 Code has no native session;
            // its T3 thread is its runtime binding.
            None => match crate::t3_code::feed::bound_thread(state, conversation)
                .await
                .and_then(|thread| crate::t3_code::feed::runtime_id(conversation.family, &thread))
            {
                Some(runtime) => {
                    state
                        .t3_code
                        .permissions(conversation.family, &runtime)
                        .await
                }
                None => None,
            },
        },
        conversation_id: conversation.conversation_id.clone(),
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        title: settled_title(state, conversation, None).await,
        family: conversation.family,
        model,
        effort,
        service_tier: conversation.service_tier.clone(),
        access_mode: conversation.access_mode.clone(),
        claude_approval: conversation.claude_approval.clone(),
        plan_mode: conversation.plan_mode,
        working_folder: conversation.cwd.clone(),
        working_folder_name: folder_name(&conversation.cwd),
        worktree_label: folders.label(&conversation.cwd),
        is_pinned: conversation.is_pinned,
        has_unread: conversation.has_unread,
        has_native_session: conversation.native_session_id.is_some(),
        is_archived: archive == ArchiveState::Archived,
        folder_in_project: folders.contains(&conversation.cwd),
        read_only_reason: if !crate::providers::is_default_store(
            state,
            conversation.family,
            &conversation.provider_store,
        ) {
            Some(crate::providers::other_history(conversation.family))
        } else {
            (!folders.contains(&conversation.cwd)).then(|| OUTSIDE_HISTORY_REASON.to_owned())
        },
        notice,
        resumes_at: state
            .store
            .usage_resumes()
            .await
            .ok()
            .and_then(|resumes| resumes.get(&conversation.conversation_id).copied())
            .and_then(crate::usage_resume::timestamp),
    })
}

/// The latest dispatch problem for a conversation, as the phone shows it.
pub(crate) fn notice(state: &AppState, conversation: &str) -> Option<String> {
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(conversation)
        .cloned()
}

struct CachedArchives {
    stamps: Vec<(PathBuf, FileStamp)>,
    read_at: Instant,
    ids: HashSet<String>,
}

// Cache only when the provider's state database can be observed. A new WAL,
// checkpoint, replacement or write invalidates it, including external archives.
async fn archive_stamps(home: PathBuf) -> Option<Vec<(PathBuf, FileStamp)>> {
    tokio::task::spawn_blocking(move || {
        let mut paths = std::fs::read_dir(home)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("state_")
                            && (name.ends_with(".sqlite") || name.ends_with(".sqlite-wal"))
                    })
            })
            .collect::<Vec<_>>();
        paths.sort();
        if paths.is_empty() {
            return None;
        }
        let stamps = file_stamps(&paths);
        if stamps.iter().any(Option::is_none) {
            return None;
        }
        Some(paths.into_iter().zip(stamps).collect())
    })
    .await
    .ok()
    .flatten()
}

/// Read provider-owned archive state without resuming a thread or relying on
/// its unstable rollout path. A failed/incomplete catalog never means active.
async fn archived_codex_ids(
    state: &AppState,
    conversations: &[StoredProjectConversation],
) -> Result<HashSet<String>, String> {
    let mut folders = HashSet::new();
    for conversation in conversations
        .iter()
        .filter(|c| c.family == AgentFamily::Codex && c.native_session_id.is_some())
    {
        if conversation.provider_store != state.projects.codex_store {
            return Err(
                "This thread belongs to a different Codex history on your computer.".into(),
            );
        }
        folders.insert(conversation.cwd.clone());
    }
    if folders.is_empty() {
        return Ok(HashSet::new());
    }
    // This read sits on Project and conversation loading. One deadline covers
    // runtime admission and every native page; a partial archive stays unknown.
    let mut key = folders.iter().cloned().collect::<Vec<_>>();
    key.sort();
    tokio::time::timeout(Duration::from_secs(3), async {
        // Concurrent reads of one selection wait for the first and reuse it.
        let flight = {
            let mut reads = state
                .projects
                .archive_reads
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            reads.retain(|_, read| read.strong_count() > 0);
            match reads.get(&key).and_then(std::sync::Weak::upgrade) {
                Some(read) => read,
                None => {
                    let read = Arc::new(Mutex::new(()));
                    reads.insert(key.clone(), Arc::downgrade(&read));
                    read
                }
            }
        };
        let _turn = flight.lock().await;
        let stamps = archive_stamps(state.codex_home.clone()).await;
        if let Some(found) = state
            .projects
            .archives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if stamps.as_ref() == Some(&found.stamps)
                && found.read_at.elapsed() < Duration::from_secs(30)
            {
                return Ok(found.ids.clone());
            }
        }
        let fetched = async {
            let rpc = codex_rpc(state).await?;
            let mut archived = HashSet::new();
            let mut cursor: Option<String> = None;
            let mut seen = HashSet::new();
            for _ in 0..200 {
                let page = result(
                    &rpc,
                    "thread/list",
                    json!({
                        "cwd": folders, "sourceKinds": CODEX_SOURCE_KINDS, "archived": true,
                        "useStateDbOnly": true, "limit": 100, "cursor": cursor,
                    }),
                )
                .await?;
                let rows = page["data"]
                    .as_array()
                    .ok_or("Codex archive state is unavailable.")?;
                for row in rows {
                    archived.insert(
                        row["id"]
                            .as_str()
                            .ok_or("Codex archive state is incomplete.")?
                            .to_owned(),
                    );
                }
                cursor = match page.get("nextCursor") {
                    Some(Value::Null) => None,
                    Some(Value::String(value)) => Some(value.clone()),
                    _ => return Err("Codex archive state is incomplete.".into()),
                };
                let Some(next) = &cursor else {
                    return Ok(archived);
                };
                if !seen.insert(next.clone()) {
                    break;
                }
            }
            Err("Codex archive state could not be fully refreshed. Try again.".into())
        }
        .await;
        if let (Ok(ids), Some(stamps)) = (&fetched, stamps) {
            if archive_stamps(state.codex_home.clone()).await.as_ref() == Some(&stamps) {
                let mut cache = state
                    .projects
                    .archives
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if cache.len() >= 32 {
                    cache.clear();
                }
                cache.insert(
                    key,
                    CachedArchives {
                        stamps,
                        read_at: Instant::now(),
                        ids: ids.clone(),
                    },
                );
            }
        }
        fetched
    })
    .await
    .unwrap_or_else(|_| Err("Codex archive state could not be fully refreshed. Try again.".into()))
}

/// Shown on a chat whose archive state in Codex could not be read.
const ARCHIVE_UNCHECKED_NOTICE: &str =
    "Cupola couldn’t check whether this chat is archived in Codex. It will check again shortly.";
/// Why a send, fork or restore was refused while that state is unknown.
const ARCHIVE_UNKNOWN: &str =
    "Cupola couldn’t check whether this chat is archived in Codex, so nothing was changed. Try again in a moment.";

/// A fresh answer for an action. Unknown is an error, never active.
async fn codex_is_archived(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<bool, String> {
    if !crate::providers::is_default_store(state, conversation.family, &conversation.provider_store)
    {
        return Err(crate::providers::other_history(conversation.family));
    }
    match archive_state::archive_state(state, conversation).await {
        ArchiveState::Archived => Ok(true),
        ArchiveState::Active => Ok(false),
        ArchiveState::Unknown => Err(ARCHIVE_UNKNOWN.into()),
    }
}

/// Archived in Cupola, or in Codex (from the Codex app).
pub(crate) async fn is_archived(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<bool, String> {
    if !crate::providers::is_default_store(state, conversation.family, &conversation.provider_store)
    {
        return Err(crate::providers::other_history(conversation.family));
    }
    if conversation.wonder_archived {
        return Ok(true);
    }
    codex_is_archived(state, conversation).await
}

/// Most time archive spends asking a running agent to stop, and separately
/// asking Codex to archive. Neither can refuse the archive itself.
const ARCHIVE_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const ARCHIVE_PROVIDER_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest archive follows a send that was mid-submission to stop its turn.
const ARCHIVE_SUBMISSION_WAIT: Duration = Duration::from_secs(60);

/// Every paired phone reads its Chats lists and pins again (the
/// `native_status` event its sidebar already follows), so an archive or
/// restore made on one device, the Mac or a script reaches the others at
/// once instead of at their next periodic refresh. One event per change set.
pub(crate) async fn announce_chats_changed(state: &AppState) {
    let _ = crate::publish_event_with_context(
        state,
        wonder_api::WonderEvent::Activity {
            category: "native_status".into(),
            state: "changed".into(),
            detail: None,
        },
        crate::EventContext::default(),
    )
    .await;
}

async fn set_archived(
    state: &AppState,
    conversation: &StoredProjectConversation,
    archived: bool,
) -> Result<(), (StatusCode, String)> {
    let _archive = state
        .projects
        .archive_lock(&conversation.conversation_id)
        .await;
    let current = state
        .store
        .project_conversation(&conversation.conversation_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The thread could not be checked. Try again.".to_owned(),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "This thread is no longer available.".to_owned(),
            )
        })?;
    if archived {
        archive(state, &current).await
    } else {
        restore(state, &current).await
    }
}

/// Archiving is never refused, whether the thread is stuck, running or
/// errored: ask its agent to stop, hide it in Cupola with its unfinished sends
/// settled, then archive it in Codex when Codex accepts. Other providers and
/// retained legacy homes are archived only in Cupola.
async fn archive(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<(), (StatusCode, String)> {
    let id = &conversation.conversation_id;
    if conversation.wonder_archived {
        reconcile_archive_cursors(state, conversation, true);
        return Ok(()); // A retry after a lost response.
    }
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return Err((
            StatusCode::CONFLICT,
            "Cupola is preparing to update. Try again shortly.".into(),
        ));
    };
    // A send to this chat holds its lock until the provider answers; its
    // provider is busy with it, so stop after it instead of waiting here.
    let sending = tokio::time::timeout(Duration::ZERO, state.projects.conversation_dispatch(id))
        .await
        .is_err();
    if !sending {
        // The Stop path takes the dispatch locks itself.
        let _ =
            tokio::time::timeout(ARCHIVE_STOP_TIMEOUT, stop_for_archive(state, conversation)).await;
    }
    // Like dispatch recovery, leave a send still being submitted to its
    // receipt rather than wait for its provider.
    let submitting = settle_archive(state, conversation).await.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The archive could not be saved. Try again.".to_owned(),
        )
    })?;
    reconcile_archive_cursors(state, conversation, true);
    if sending || submitting {
        let (state, id) = (state.clone(), id.clone());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                ARCHIVE_SUBMISSION_WAIT,
                finish_archive_after_send(&state, &id),
            )
            .await;
        });
    } else {
        archive_in_provider(state, conversation).await;
    }
    Ok(())
}

/// Hides the thread and settles its unfinished sends, except those a
/// dispatch has claimed, under the shared lock. Whether unfinished work remains; `None` if the
/// archive could not be saved.
async fn settle_archive(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Option<bool> {
    let id = &conversation.conversation_id;
    let (settled, remaining) = {
        let _dispatch = state.dispatch_lock.lock().await;
        // Claims are made under this lock: read them under it too.
        let claimed = state.projects.dispatching();
        let settled = state
            .store
            .archive_project_conversation(id, &claimed, &now_text())
            .await;
        let remaining = state
            .store
            .conversation_has_archive_blocking_work(id)
            .await
            .unwrap_or(true);
        (settled, remaining)
    };
    for message in settled.ok()?? {
        if let Ok(Some(message)) = state.store.message_by_id(&message).await {
            publish_message_state(
                state,
                &message,
                DeliveryState::Interrupted,
                message.codex_thread_id.as_deref(),
                message.codex_turn_id.as_deref(),
            )
            .await;
        }
    }
    Some(remaining)
}

/// A send that was claimed when the thread was archived: once it resolves,
/// unless the thread was restored meanwhile, ask the turn it started to stop,
/// settle whatever it left (accepted or uncertain, receipts kept), then
/// archive in the provider. A send still claimed (preparing runs before the
/// chat's lock) keeps its claim until its dispatch ends, so it ends in exactly
/// one terminal state; then this settles again.
async fn finish_archive_after_send(state: &AppState, id: &str) {
    loop {
        let released = state.projects.dispatch_done.notified();
        tokio::pin!(released);
        released.as_mut().enable();
        drop(state.projects.conversation_dispatch(id).await);
        {
            let _archive = state.projects.archive_lock(id).await;
            let Ok(Some(conversation)) = state.store.project_conversation(id).await else {
                return;
            };
            if !conversation.wonder_archived {
                return;
            }
            let _ =
                tokio::time::timeout(ARCHIVE_STOP_TIMEOUT, stop_for_archive(state, &conversation))
                    .await;
            match settle_archive(state, &conversation).await {
                Some(false) => {
                    archive_in_provider(state, &conversation).await;
                    return;
                }
                Some(true) => {}
                None => return,
            }
        }
        released.await;
    }
}

/// Only Codex keeps its own archive; a refusal leaves Cupola's archive
/// standing while the Codex app keeps showing the thread.
async fn archive_in_provider(state: &AppState, conversation: &StoredProjectConversation) {
    if conversation.family != AgentFamily::Codex
        || conversation.native_session_id.is_none()
        || !crate::providers::is_default_store(
            state,
            conversation.family,
            &conversation.provider_store,
        )
    {
        return;
    }
    let outcome = tokio::time::timeout(
        ARCHIVE_PROVIDER_TIMEOUT,
        archive_in_codex(state, conversation),
    )
    .await
    .unwrap_or_else(|_| Err("timed out".into()));
    if let Err(reason) = outcome {
        let _ = state.logger.record(
            "info",
            "project_archive_provider_skipped",
            json!({"conversationId": conversation.conversation_id, "reason": reason}),
        );
    }
}

/// Best effort: every turn Cupola started here goes through the normal Stop
/// path, then a turn the provider still reports in progress is interrupted.
async fn stop_for_archive(state: &AppState, conversation: &StoredProjectConversation) {
    let id = &conversation.conversation_id;
    for turn in state
        .store
        .conversation_unsettled_turns(id)
        .await
        .unwrap_or_default()
    {
        let _ = crate::interrupt_turn(State(state.clone()), Path((id.clone(), turn))).await;
    }
    let thread = match state.store.runtime_binding(id).await {
        Ok(Some(binding)) => Some(binding.thread_id),
        _ => conversation.native_session_id.clone(),
    };
    let Some(thread) = thread else {
        return;
    };
    let Some(rpc) = crate::providers::live_rpc_for_store(
        state,
        conversation.family,
        &conversation.provider_store,
    )
    .await
    else {
        return;
    };
    let Ok(turns) = result(
        &rpc,
        "thread/turns/list",
        json!({
            "threadId": thread, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded",
        }),
    )
    .await
    else {
        return;
    };
    // Claude Code reports a reply still running on the thread, not the turn.
    let active = turns.pointer("/data/0/status").and_then(Value::as_str) == Some("inProgress")
        || result(&rpc, "thread/read", json!({"threadId": thread}))
            .await
            .is_ok_and(|read| {
                read.pointer("/thread/status/type").and_then(Value::as_str) == Some("active")
            });
    if !active {
        return;
    }
    let Some(turn) = turns.pointer("/data/0/id").and_then(Value::as_str) else {
        return;
    };
    if stop_requested(state, &thread, turn).await.unwrap_or(true) {
        return;
    }
    let Ok(own) = request_stop(state, &thread, turn).await else {
        return;
    };
    let stopped = rpc
        .request(
            "turn/interrupt",
            json!({"threadId": thread, "turnId": turn}),
        )
        .await
        .is_ok_and(|response| response.error.is_none());
    if !stopped && own {
        clear_stop(state, &thread, turn).await;
    }
}

/// Codex's own archive, for the desktop app's thread list. Never archives a
/// thread whose identity on the Mac changed.
async fn archive_in_codex(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<(), String> {
    let native = conversation
        .native_session_id
        .as_deref()
        .ok_or("no native thread")?;
    if codex_is_archived(state, conversation).await? {
        return Ok(());
    }
    let rpc = codex_rpc(state).await?;
    let read = result(&rpc, "thread/read", json!({"threadId": native})).await?;
    if read["thread"]["id"].as_str() != Some(native)
        || read["thread"]["cwd"].as_str() != Some(conversation.cwd.as_str())
    {
        return Err("thread identity changed".into());
    }
    let response = rpc
        .request("thread/archive", json!({"threadId": native}))
        .await
        .map_err(|e| e.to_string())?;
    match response.error {
        // A lost response can follow a committed archive.
        Some(error) if codex_is_archived(state, conversation).await.ok() != Some(true) => {
            Err(format!("{}: {}", error.code, error.message))
        }
        _ => {
            archive_state::record(state, native, true);
            Ok(())
        }
    }
}

/// Restore clears Cupola's archive and, for a Codex thread archived in Codex,
/// unarchives it there first.
async fn restore(
    state: &AppState,
    conversation: &StoredProjectConversation,
) -> Result<(), (StatusCode, String)> {
    let conflict = |message: &str| (StatusCode::CONFLICT, message.to_owned());
    let unavailable = |message: String| (StatusCode::SERVICE_UNAVAILABLE, message);
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return Err(conflict(
            "Cupola is preparing to update. Try again shortly.",
        ));
    };
    crate::settled_elsewhere::restore(state, conversation)
        .await
        .map_err(unavailable)?;
    // Provider reads and unarchive can be slow. The caller's archive lock
    // orders this chat; shared admission keeps its runtime alive without
    // holding other chats' sends or streamed events behind the provider.
    let _provider = state.provider_reconnect.read().await;
    let codex = conversation.family.provider().capabilities.archive
        == wonder_store::ArchiveStyle::Provider
        && crate::providers::is_default_store(
            state,
            conversation.family,
            &conversation.provider_store,
        );
    if let (true, Some(native)) = (codex, conversation.native_session_id.as_deref()) {
        if codex_is_archived(state, conversation)
            .await
            .map_err(unavailable)?
        {
            let rpc = codex_rpc(state).await.map_err(unavailable)?;
            let read = result(&rpc, "thread/read", json!({"threadId": native}))
                .await
                .map_err(unavailable)?;
            if read["thread"]["id"].as_str() != Some(native)
                || read["thread"]["cwd"].as_str() != Some(conversation.cwd.as_str())
            {
                return Err(conflict(
                    "This thread's identity on your computer changed. Reopen it before restoring.",
                ));
            }
            let response = rpc
                .request("thread/unarchive", json!({"threadId": native}))
                .await;
            let failed = !matches!(&response, Ok(r) if r.error.is_none() && r.result.is_some());
            // A transport response can be lost after Codex has already
            // committed the change. Confirm provider state before failing.
            if failed && codex_is_archived(state, conversation).await.ok() != Some(false) {
                return Err(unavailable(
                    "The archive change could not be confirmed. Refresh before trying again."
                        .into(),
                ));
            }
            archive_state::record(state, native, false);
        }
    }
    let _guard = state.dispatch_lock.lock().await;
    if conversation.wonder_archived
        && !matches!(
            state
                .store
                .set_project_conversation_wonder_archived(
                    &conversation.conversation_id,
                    false,
                    &now_text(),
                )
                .await,
            Ok(true)
        )
    {
        return Err(unavailable(
            "The archive change could not be saved. Try again.".into(),
        ));
    }
    reconcile_archive_cursors(state, conversation, false);
    Ok(())
}

fn reconcile_archive_cursors(
    state: &AppState,
    conversation: &StoredProjectConversation,
    archived: bool,
) {
    let mut cursors = state
        .projects
        .cursors
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !archived {
        // Restore already refreshes the first page so the thread is sorted at
        // its authoritative position, rather than inserted into an old page.
        cursors.clear();
        return;
    }
    let Some(native) = conversation.native_session_id.as_deref() else {
        return;
    };
    let key = reference(conversation.family, native);
    for (_, cursor) in cursors.values_mut() {
        // Preserve the continuation and its expiry. Skip this identity even
        // if a provider page read before the archive returns it later.
        cursor.emitted.insert(key.clone());
        for page in [&mut cursor.codex, &mut cursor.claude, &mut cursor.accounts] {
            page.buffer.retain(|row| {
                !(row.family == conversation.family
                    && row.store == conversation.provider_store
                    && row.id == native)
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Folder validation
// ---------------------------------------------------------------------------

/// Existing, readable directories only. A project folder may not be, contain,
/// or sit inside a protected location such as provider credentials or
/// Wonder's own data; it is metadata and never widens a Mac access grant.
fn validate_folder(path: &str, denied: &[String]) -> Result<ProjectRootInput, &'static str> {
    if path.is_empty()
        || !is_native_folder_path(path, cfg!(windows))
        || path.len() > 4096
        || path.contains('\0')
    {
        return Err("Choose an existing folder on your computer.");
    }
    let lexical = FsPath::new(path);
    let canonical = crate::canonical_path::canonicalize(lexical)
        .map_err(|_| "A selected folder is no longer available on your computer.")?;
    if !std::fs::metadata(&canonical).is_ok_and(|m| m.is_dir()) {
        return Err("Choose a folder, not a file.");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if canonical.parent().is_none()
        || home
            .as_ref()
            .and_then(|home| crate::canonical_path::canonicalize(home).ok())
            .is_some_and(|home| home == canonical)
    {
        return Err("Choose a specific project folder, not your whole computer or home folder.");
    }
    for root in denied {
        let root = FsPath::new(root.strip_suffix("/**").unwrap_or(root));
        let Ok(protected) = crate::filesystem::protected_root(root) else {
            return Err("Protected computer locations could not be checked.");
        };
        if canonical.starts_with(&protected) || protected.starts_with(&canonical) {
            return Err("This folder contains or is inside protected computer settings. Choose a more specific project folder.");
        }
    }
    let canonical = canonical
        .to_str()
        .ok_or("Folder names must be valid text.")?;
    let canonical = canonical.to_owned();
    let trimmed = path.trim_end_matches(std::path::is_separator);
    Ok(ProjectRootInput {
        path: if trimmed.is_empty() {
            "/".into()
        } else {
            trimmed.to_owned()
        },
        canonical_path: canonical,
    })
}

/// Whether `path` names a folder by its full location in this computer's own
/// path style: `/…` on macOS and Linux, a drive path such as `C:\Work` or
/// `C:/Work` on Windows. Drive-relative (`C:Work`), drive-less (`\Work`),
/// network share (`\\server\share`) and device (`\\?\…`, `\\.\…`) paths are
/// refused on Windows, as POSIX paths are.
fn is_native_folder_path(path: &str, windows: bool) -> bool {
    if !windows {
        return path.starts_with('/');
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// Recheck persisted paths before every execution. A replaced symlink must
/// never redirect the agent to a folder the owner did not select.
pub(crate) fn validate_execution_roots(
    project: &StoredProject,
    denied: &[String],
) -> Result<(), &'static str> {
    for root in &project.roots {
        let current = validate_folder(&root.path, denied)?;
        if current.canonical_path != root.canonical_path {
            return Err(
                "A project folder now points somewhere else. Edit the project before continuing.",
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_folders(
    paths: &[String],
    denied: &[String],
) -> Result<Vec<ProjectRootInput>, &'static str> {
    if paths.is_empty() || paths.len() > wonder_store::MAX_PROJECT_ROOTS {
        return Err("Choose between one and sixteen folders.");
    }
    let mut roots: Vec<ProjectRootInput> = Vec::with_capacity(paths.len());
    for path in paths {
        let root = validate_folder(path, denied)?;
        // Symlinks and trailing slashes may name one folder twice.
        if !roots
            .iter()
            .any(|r| r.canonical_path == root.canonical_path)
        {
            roots.push(root);
        }
    }
    Ok(roots)
}

// ---------------------------------------------------------------------------
// Library routes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectsResponse {
    projects: Vec<ProjectSummary>,
    families: Vec<FamilyAvailability>,
    modes_version: u8,
    skills_version: u8,
    archive_version: u8,
    delete_version: u8,
    create_version: u8,
    /// Version of each family's `capabilities` record.
    capabilities_version: u8,
    /// Owner terminals (`crate::terminal`) are available on this host.
    terminal_version: u8,
    pinned: Vec<PinnedThread>,
    partial: Vec<PartialFailure>,
}

/// A pinned, attached thread and the project it belongs to.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PinnedThread {
    project_id: String,
    thread: ProjectThreadSummary,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FamilyAvailability {
    family: AgentFamily,
    available: bool,
    /// False only when the last check found the provider not installed on
    /// this Mac, so the phone leaves it out of new-chat pickers. A signed-out,
    /// recovering or unchecked provider is not hidden. Sent for the providers
    /// the owner installs separately (OpenCode, Cursor).
    #[serde(skip_serializing_if = "Option::is_none")]
    installed: Option<bool>,
    /// True only when the last check found the provider signed out, so the
    /// phone does not start a new chat there unless the owner picks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_out: Option<bool>,
    capabilities: FamilyCapabilities,
}

fn family_installed(state: &AppState, family: AgentFamily) -> Option<bool> {
    if !matches!(family, AgentFamily::OpenCode | AgentFamily::Cursor) {
        return None;
    }
    installed_from(crate::provider_runtime::known_state(state, family))
}

fn family_signed_out(state: &AppState, family: AgentFamily) -> Option<bool> {
    signed_out_from(crate::provider_runtime::known_state(state, family))
}

/// Only a finished check that found the account signed out says so.
fn signed_out_from(known: Option<&str>) -> Option<bool> {
    (known? == "signedOut").then_some(true)
}

/// Only a finished check that found the provider missing hides it; a
/// check that could not tell ("unavailable") says nothing.
fn installed_from(known: Option<&str>) -> Option<bool> {
    match known? {
        "notInstalled" => Some(false),
        "unavailable" => None,
        _ => Some(true),
    }
}

/// A provider's capability record as the phone reads it: the descriptor's
/// capabilities plus the approval modes a thread may choose.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FamilyCapabilities {
    #[serde(flatten)]
    capabilities: wonder_store::ProviderCapabilities,
    approval_modes: &'static [&'static str],
}

impl FamilyCapabilities {
    pub(crate) fn of(family: AgentFamily) -> Self {
        let provider = family.provider();
        Self {
            capabilities: provider.capabilities,
            approval_modes: provider.approval_modes,
        }
    }
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    let mut projects = match state.store.list_projects().await {
        Ok(projects) => projects,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Projects are temporarily unavailable. Try again.",
            )
        }
    };
    rename_legacy_scratch(&state, &mut projects).await;
    let pinned = match state
        .store
        .pinned_project_conversations(MAX_PINNED_THREADS)
        .await
    {
        Ok(rows) => rows,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Projects are temporarily unavailable. Try again.",
            )
        }
    };
    let visible = match visible_conversations(&state).await {
        Ok(ids) => ids,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Projects are temporarily unavailable. Try again.",
            )
        }
    };
    let pinned: Vec<_> = pinned
        .into_iter()
        .filter(|c| visible.contains(&c.conversation_id))
        .collect();
    let mut pinned_threads = Vec::with_capacity(pinned.len());
    let checked = pinned
        .iter()
        .filter(|c| !c.wonder_archived)
        .cloned()
        .collect::<Vec<_>>();
    let archive = archive_state::display_states(&state, &checked).await;
    // A pin whose archive state cannot be read stays as last seen, or waits
    // for the next refresh; it never takes the rest of the list down.
    let mut pins_unchecked = false;
    for conversation in &pinned {
        if conversation.wonder_archived || !visible.contains(&conversation.conversation_id) {
            continue;
        }
        if let Some(found) = conversation
            .native_session_id
            .as_ref()
            .and_then(|id| archive.get(id))
        {
            match archive_state::displayed(&state, conversation, *found) {
                ArchiveState::Active => {}
                ArchiveState::Archived => continue,
                ArchiveState::Unknown => {
                    pins_unchecked = true;
                    continue;
                }
            }
        }
        pinned_threads.push(PinnedThread {
            project_id: conversation.project_id.clone(),
            thread: attached_summary(
                &state,
                conversation,
                activity_seconds(&conversation.last_activity_at),
                None,
            )
            .await,
        });
    }
    mark_computer_sessions(&state, pinned_threads.iter_mut().map(|pin| &mut pin.thread)).await;
    let (claude_desktop, activity_complete, claude_threads) = claude_activity(
        &state,
        claude_session_ids(pinned_threads.iter().map(|pin| &pin.thread)),
    )
    .await;
    for pin in &mut pinned_threads {
        mark_desktop_working(&state, &mut pin.thread, &claude_desktop).await;
    }
    decorate(
        &state,
        pinned_threads
            .iter_mut()
            .map(|pin| &mut pin.thread)
            .collect(),
        &claude_desktop,
        claude_threads.as_ref(),
    )
    .await;
    let projects = summaries(&projects).await;
    let catalog = state.runtime_catalog.read().await;
    let families = AgentFamily::ALL
        .into_iter()
        .filter(|family| {
            !matches!(family, AgentFamily::OpenCode | AgentFamily::Cursor)
                || crate::claude::available(&state, *family)
        })
        .map(|family| FamilyAvailability {
            family,
            available: catalog
                .models
                .iter()
                .any(|m| m.agent_family == family && !m.hidden)
                && crate::claude::available(&state, family),
            installed: family_installed(&state, family),
            signed_out: family_signed_out(&state, family),
            capabilities: FamilyCapabilities::of(family),
        })
        .collect();
    Json(ProjectsResponse {
        projects,
        families,
        modes_version: MODES_VERSION,
        skills_version: 1,
        // 3: bounded, ordered archive batches with per-thread outcomes.
        archive_version: 3,
        delete_version: 1,
        create_version: CREATE_VERSION,
        capabilities_version: wonder_store::CAPABILITIES_VERSION,
        terminal_version: crate::terminal::VERSION,
        pinned: pinned_threads,
        partial: claude_activity_partial(activity_complete)
            .into_iter()
            .chain(pins_unchecked.then(|| PartialFailure {
                family: AgentFamily::Codex,
                detail:
                    "Some pinned Codex chats couldn’t be checked and are hidden for now.".into(),
            }))
            .collect(),
    })
    .into_response()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    name: String,
    folders: Vec<String>,
    sources: Vec<AgentFamily>,
    updated_at: Option<i64>,
    is_included: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidatesResponse {
    candidates: Vec<Candidate>,
    partial: Vec<PartialFailure>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PartialFailure {
    family: AgentFamily,
    detail: String,
}

/// Whether this Mac has a Codex the owner could be using: its program exists
/// and the last provider check did not find it signed out.
fn codex_present(state: &AppState) -> bool {
    state.projects.codex_bin().is_file()
        && !matches!(
            crate::provider_runtime::known_state(state, AgentFamily::Codex),
            Some("notInstalled" | "signedOut")
        )
}

/// Bounded suggestions from native provider metadata. Nothing is included,
/// scanned or started until the owner explicitly selects a suggestion.
pub(crate) async fn candidates(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    let included: Vec<HashSet<String>> = match state.store.list_projects().await {
        Ok(projects) => projects
            .iter()
            .map(|p| p.roots.iter().map(|r| r.canonical_path.clone()).collect())
            .collect(),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Projects are temporarily unavailable. Try again.",
            )
        }
    };
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut partial = Vec::new();
    let codex = async {
        let rpc = codex_rpc(&state).await?;
        result(
            &rpc,
            "project/list",
            json!({"limit": 50, "sortKey": "recencyAt", "sortDirection": "desc"}),
        )
        .await
    }
    .await;
    match codex {
        Ok(value) => {
            for project in value
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(50)
            {
                let folders = project
                    .get("roots")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.get("path").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let name = project
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if folders.is_empty() || name.is_empty() {
                    continue;
                }
                candidates.push(Candidate {
                    name,
                    folders,
                    sources: vec![AgentFamily::Codex],
                    updated_at: project
                        .get("recencyAt")
                        .and_then(Value::as_i64)
                        .or_else(|| project.get("updatedAt").and_then(Value::as_i64)),
                    is_included: false,
                });
            }
        }
        Err(detail)
            if detail.contains("not allowlisted")
                || detail.to_ascii_lowercase().contains("method") => {}
        // A Codex that is not installed or signed in is not a failure.
        Err(_) if !codex_present(&state) => {}
        Err(_) => partial.push(PartialFailure {
            family: AgentFamily::Codex,
            detail: "Codex projects could not be loaded.".into(),
        }),
    }
    if state.claude.is_some() {
        match async {
            result(
                &claude_rpc(&state).await?,
                "project/folders/list",
                json!({"limit": 50}),
            )
            .await
        }
        .await
        {
            Ok(value) => {
                for folder in value
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(cwd) = folder.get("cwd").and_then(Value::as_str) else {
                        continue;
                    };
                    let updated = folder.get("updatedAt").and_then(Value::as_i64);
                    if let Some(existing) = candidates
                        .iter_mut()
                        .find(|c| c.folders.len() == 1 && c.folders[0] == cwd)
                    {
                        existing.sources.push(AgentFamily::Claude);
                        existing.updated_at = existing.updated_at.max(updated);
                    } else {
                        candidates.push(Candidate {
                            name: folder_name(cwd),
                            folders: vec![cwd.to_owned()],
                            sources: vec![AgentFamily::Claude],
                            updated_at: updated,
                            is_included: false,
                        });
                    }
                }
            }
            Err(_) => partial.push(PartialFailure {
                family: AgentFamily::Claude,
                detail: "Claude Code folders could not be loaded.".into(),
            }),
        }
    }
    // Suggest only folders that can actually become a project on this Mac;
    // app-managed scratch folders under ~/Library are not owner projects.
    let library = std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library"));
    candidates.retain(|c| {
        !c.folders.iter().any(|f| {
            library
                .as_ref()
                .is_some_and(|l| FsPath::new(f).starts_with(l))
        })
    });
    let denied = state.denied_roots.clone();
    let candidates = tokio::task::spawn_blocking(move || {
        let mut kept = Vec::new();
        for mut candidate in candidates {
            let Ok(roots) = validate_folders(&candidate.folders, &denied) else {
                continue;
            };
            let set: HashSet<String> = roots.iter().map(|r| r.canonical_path.clone()).collect();
            candidate.is_included = included.contains(&set);
            kept.push(candidate);
        }
        kept.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        kept.truncate(40);
        kept
    })
    .await
    .unwrap_or_default();
    Json(CandidatesResponse {
        candidates,
        partial,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateProjectRequest {
    request_id: String,
    name: String,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    primary_index: usize,
    /// Creates an empty folder named after the project instead of using
    /// existing `folders`.
    new_folder: Option<NewFolderRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NewFolderRequest {
    /// Where the folder is created; the Mac's home folder when absent.
    parent: Option<String>,
}

/// Version of `POST /api/v1/projects`: 1 accepts `newFolder`.
const CREATE_VERSION: u8 = 1;

/// Where a new project's folder goes: `parent/name`, checked without touching
/// the disk, as the canonical path the project will record.
fn plan_project_folder(
    parent: Option<&str>,
    name: &str,
    denied: &[String],
) -> Result<PathBuf, (StatusCode, &'static str)> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || name.contains(['/', ':', '\0'])
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Folder names can’t start with a period or contain / or :.",
        ));
    }
    let parent = match parent {
        Some(parent) => PathBuf::from(parent),
        None => std::env::var_os("HOME").map(PathBuf::from).ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "Your computer’s home folder could not be found.",
        ))?,
    };
    if !parent.is_absolute() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a location on your computer.",
        ));
    }
    let parent = crate::canonical_path::canonicalize(&parent)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or((
            StatusCode::UNPROCESSABLE_ENTITY,
            "That location is no longer available on your computer.",
        ))?;
    for root in denied {
        let root = FsPath::new(root.strip_suffix("/**").unwrap_or(root));
        let Ok(protected) = crate::filesystem::protected_root(root) else {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Protected computer locations could not be checked.",
            ));
        };
        if parent.starts_with(&protected) {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "That location holds protected computer settings. Choose another location.",
            ));
        }
    }
    let folder = parent.join(name);
    if folder.to_str().is_none() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Folder names must be valid text.",
        ));
    }
    Ok(folder)
}

/// Creates a planned project folder. An empty folder already at that path is
/// reused (a create that failed after making it); a folder with contents is
/// refused rather than adopted silently.
fn create_project_folder(folder: &FsPath) -> Result<String, (StatusCode, &'static str)> {
    match std::fs::create_dir(folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let empty = std::fs::symlink_metadata(folder).is_ok_and(|m| m.is_dir())
                && std::fs::read_dir(folder).is_ok_and(|mut entries| entries.next().is_none());
            if !empty {
                return Err((
                    StatusCode::CONFLICT,
                    "A folder with this name already exists there. Choose another name, or add it as an existing folder.",
                ));
            }
        }
        Err(_) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "The folder couldn’t be created there. Choose another location.",
            ))
        }
    }
    folder.to_str().map(str::to_owned).ok_or((
        StatusCode::UNPROCESSABLE_ENTITY,
        "Folder names must be valid text.",
    ))
}

const REUSED_REQUEST: &str = "This request was already used for a different project.";

/// What identifies a create request's input, so a retry is recognized and a
/// reused request ID with different input is refused.
fn creation_payload(name: &str, canonical_roots: &[String], primary: usize) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(&json!([name.trim(), canonical_roots, primary])).unwrap_or_default(),
    ))
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Json(request): Json<CreateProjectRequest>,
) -> Response {
    if uuid::Uuid::parse_str(&request.request_id).is_err() {
        return error(StatusCode::BAD_REQUEST, "requestId must be a UUID");
    }
    let mut request = request;
    if let Some(new_folder) = request.new_folder.take() {
        if !request.folders.is_empty() {
            return error(
                StatusCode::BAD_REQUEST,
                "Send either folders or newFolder, not both.",
            );
        }
        let denied = state.denied_roots.clone();
        let name = request.name.clone();
        let planned = match tokio::task::spawn_blocking(move || {
            plan_project_folder(new_folder.parent.as_deref(), &name, &denied)
        })
        .await
        {
            Ok(Ok(folder)) => folder,
            Ok(Err((status, message))) => return error(status, message),
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The folder could not be created. Try again.",
                )
            }
        };
        // The request is resolved before anything is made on disk: a retry of
        // a completed create returns its project whatever the folder holds
        // now, and a reused request never leaves a folder behind.
        let payload = creation_payload(&request.name, &[planned.to_string_lossy().into_owned()], 0);
        match state.store.project_creation(&request.request_id).await {
            Ok(Some((project, recorded))) if recorded.as_deref() == Some(payload.as_str()) => {
                return Json(summary(&project).await).into_response()
            }
            Ok(Some(_)) => return error(StatusCode::CONFLICT, REUSED_REQUEST),
            Ok(None) => {}
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The project could not be saved. Try again.",
                )
            }
        }
        match tokio::task::spawn_blocking(move || create_project_folder(&planned)).await {
            Ok(Ok(path)) => {
                request.folders = vec![path];
                request.primary_index = 0;
            }
            Ok(Err((status, message))) => return error(status, message),
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The folder could not be created. Try again.",
                )
            }
        }
    }
    let denied = state.denied_roots.clone();
    let folders = request.folders.clone();
    let roots = match tokio::task::spawn_blocking(move || validate_folders(&folders, &denied)).await
    {
        Ok(Ok(roots)) => roots,
        Ok(Err(message)) => return error(StatusCode::UNPROCESSABLE_ENTITY, message),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Folders could not be checked. Try again.",
            )
        }
    };
    if request.primary_index >= request.folders.len() {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "Choose a primary folder.");
    }
    // The primary is named by the caller's folder; de-duplication can shift indices.
    let primary_canonical =
        crate::canonical_path::canonicalize(&request.folders[request.primary_index]).ok();
    let primary = roots
        .iter()
        .position(|r| {
            primary_canonical
                .as_deref()
                .is_some_and(|p| p == FsPath::new(&r.canonical_path))
        })
        .unwrap_or(0);
    let canonical: Vec<String> = roots.iter().map(|r| r.canonical_path.clone()).collect();
    let payload = creation_payload(&request.name, &canonical, primary);
    match state
        .store
        .create_project(
            &uuid::Uuid::new_v4().to_string(),
            &request.request_id,
            &payload,
            &request.name,
            &roots,
            primary,
            &now_text(),
        )
        .await
    {
        Ok(ProjectCreate::Created(project)) => {
            (StatusCode::CREATED, Json(summary(&project).await)).into_response()
        }
        Ok(ProjectCreate::Existing(project)) => Json(summary(&project).await).into_response(),
        Ok(ProjectCreate::Conflict) => error(StatusCode::CONFLICT, REUSED_REQUEST),
        Err(sqlx::Error::Protocol(message)) => error(StatusCode::UNPROCESSABLE_ENTITY, message),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The project could not be saved. Try again.",
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdateProjectRequest {
    name: Option<String>,
    is_included: Option<bool>,
    is_pinned: Option<bool>,
    folders: Option<Vec<String>>,
    primary_index: Option<usize>,
    roots_revision: Option<i64>,
    /// A rule, or null to follow the computer's again.
    #[serde(default, deserialize_with = "present")]
    auto_archive: Option<Option<AutoArchiveSetting>>,
}

/// Tells a field sent as null (`Some(None)`) from one left out (`None`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<UpdateProjectRequest>,
) -> Response {
    let now = now_text();
    let root_update = if let Some(folders) = request.folders.clone() {
        let Some(revision) = request.roots_revision else {
            return error(
                StatusCode::BAD_REQUEST,
                "rootsRevision is required when folders change",
            );
        };
        let primary_path = folders.get(request.primary_index.unwrap_or(0)).cloned();
        let denied = state.denied_roots.clone();
        let roots =
            match tokio::task::spawn_blocking(move || validate_folders(&folders, &denied)).await {
                Ok(Ok(roots)) => roots,
                Ok(Err(message)) => return error(StatusCode::UNPROCESSABLE_ENTITY, message),
                Err(_) => {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Folders could not be checked. Try again.",
                    )
                }
            };
        let primary_canonical =
            primary_path.and_then(|p| crate::canonical_path::canonicalize(p).ok());
        let primary = roots
            .iter()
            .position(|r| {
                primary_canonical
                    .as_deref()
                    .is_some_and(|p| p == FsPath::new(&r.canonical_path))
            })
            .unwrap_or(0);
        Some((revision, roots, primary))
    } else {
        None
    };
    // An automatic archive reads the rule under this lock just before it
    // commits, so a rule change lands wholly before or after it. Taken before
    // the approval lock, in the order the notification owner takes them.
    let _dispatch = if request.auto_archive.is_some() || request.is_included.is_some() {
        Some(state.dispatch_lock.lock().await)
    } else {
        None
    };
    // A removal must either precede approval admission or wait until its
    // response is sent. Folder validation above does not hold this lock.
    let _approval_guard = if root_update.is_some() || request.is_included.is_some() {
        Some(state.approval_lock.lock().await)
    } else {
        None
    };
    let rule = request
        .auto_archive
        .map(|rule| rule.map(|rule| (rule.idle_days, rule.on_merge)));
    // With folders, the rule is saved in the same step, so a refused folder
    // change leaves the rule as it was.
    if let (Some(rule), None) = (rule, &root_update) {
        if !matches!(state.store.project(&id).await, Ok(Some(_))) {
            return StatusCode::NOT_FOUND.into_response();
        }
        match state
            .store
            .set_project_auto_archive_rule(&id, rule, &now)
            .await
        {
            Ok(()) => {}
            Err(sqlx::Error::Protocol(message)) => {
                return error(StatusCode::UNPROCESSABLE_ENTITY, message)
            }
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Auto-archive could not be saved. Try again.",
                )
            }
        }
    }
    if let Some((revision, roots, primary)) = root_update {
        match state
            .store
            .update_project_roots_and_rule(&id, revision, &roots, primary, rule, &now)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(sqlx::Error::Protocol(message)) if message == "roots_revision_changed" => {
                return error(
                    StatusCode::CONFLICT,
                    "These folders changed on another device. Reload and try again.",
                )
            }
            Err(sqlx::Error::Protocol(message)) => {
                return error(StatusCode::UNPROCESSABLE_ENTITY, message)
            }
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The folders could not be saved. Try again.",
                )
            }
        }
    }
    match state
        .store
        .update_project_metadata(
            &id,
            request.name.as_deref(),
            request.is_included,
            request.is_pinned,
            &now,
        )
        .await
    {
        Ok(Some(project)) => Json(summary(&project).await).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(sqlx::Error::Protocol(message)) => error(StatusCode::UNPROCESSABLE_ENTITY, message),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The project could not be saved. Try again.",
        ),
    }
}

// ---------------------------------------------------------------------------
// Thread catalog
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct NativeThread {
    family: AgentFamily,
    /// The provider home that lists it (`provider_store`).
    store: String,
    id: String,
    /// The row's title when no Wonder thread owns the session.
    title: String,
    /// The provider's name or first message, for `display_title`.
    source: Option<String>,
    updated_at: i64,
    is_working: bool,
    is_computer_session: bool,
    worktree_label: Option<String>,
    /// The thread's working folder, when the provider reports it.
    cwd: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ProviderPage {
    cursor: Option<String>,
    done: bool,
    buffer: VecDeque<NativeThread>,
    failed: bool,
    deferred: bool,
}

#[derive(Clone, Debug)]
struct CatalogCursor {
    project_id: String,
    roots_revision: i64,
    folders_key: String,
    codex: ProviderPage,
    claude: ProviderPage,
    /// Retained threads from other homes, from Cupola's own record: only the
    /// Mac's own home is listed natively.
    accounts: ProviderPage,
    emitted: HashSet<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ThreadsQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsPage {
    threads: Vec<ProjectThreadSummary>,
    next_cursor: Option<String>,
    partial: Vec<PartialFailure>,
    /// A provider is still reading its catalog; the cursor continues it.
    /// Slowness is not a failure, so it never adds a `partial` entry.
    loading: bool,
}

pub(crate) fn thread_title(value: &Value) -> String {
    display_title(None, native_title_source(value))
}

/// A Codex thread's own name, else its first message.
fn native_title_source(value: &Value) -> Option<&str> {
    value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| value.get("preview").and_then(Value::as_str))
}

/// A title from a provider's name or first message, or from a title Cupola
/// stored: never Wonder's briefing markup, and a scripted agent run is named
/// by its task, else its goal.
fn title_text(raw: &str) -> String {
    let text = crate::provider_switch::without_handoff(raw);
    // A first message that mentions a T3 thread or file is named by the
    // owner's words, not the mention markup.
    let owned = crate::injected_context::owner_text(text);
    let text = owned.as_str();
    if let Some(brief) = brief_title(text) {
        return brief;
    }
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Untitled thread");
    line.chars().take(120).collect()
}

/// The one title every surface shows for a thread: its Chats row, pinned and
/// archived rows, attach and create receipts, its header, and (through the
/// stored title) search. A thread's stored title is written once: the owner's
/// rename, a title Cupola gave it (a fork's, a child task's), or one Cupola
/// took the first time it had the thread's first message or the provider's
/// name (a scripted run is named by the task its brief names). After that no
/// provider text replaces it: a later preview may be cut short (Claude lists
/// 200 characters) and a session a thread moved to opens with Cupola's
/// briefing. Only a thread with no title yet takes one from `provider`, the
/// provider's name or first message for the session the thread uses; so does
/// a session no thread owns. Otherwise "Untitled thread".
pub(crate) fn display_title(
    conversation: Option<&StoredProjectConversation>,
    provider: Option<&str>,
) -> String {
    match conversation {
        Some(c) if c.title_chosen => return c.title.clone(),
        // Only shown without the briefing markup; the stored title stays.
        Some(c) if !is_untitled(&c.title) => return title_text(&c.title),
        _ => {}
    }
    provider
        .filter(|raw| conversation.is_none() || !crate::provider_switch::is_handoff_text(raw))
        .map(title_text)
        .filter(|title| !is_placeholder_title(title))
        .unwrap_or_else(|| "Untitled thread".into())
}

/// A title that names nothing: a bare "GOAL" heading, reviewer boilerplate
/// without its task, or a briefing with no message after it.
fn is_placeholder_title(title: &str) -> bool {
    let label = title.trim_matches(|c: char| c == '#' || c == ':' || c.is_whitespace());
    label.is_empty()
        || label.eq_ignore_ascii_case("GOAL")
        || matches!(title, "Review" | "Untitled thread")
}

/// A taken title written before Cupola had anything to name the thread by:
/// a thread created without words, or attached before its provider listed a
/// name or first message.
fn is_untitled(title: &str) -> bool {
    matches!(
        title.trim(),
        "" | "Untitled thread" | "New thread" | "Claude session"
    )
}

/// `display_title` for a thread. A taken title that is still untitled is
/// stored the first time the provider names the thread, so rows that read no
/// provider (pinned, archived in Cupola, search) show the same. A thread T3
/// Code has takes T3 Code's title whenever that changes
/// (`t3_code::titles`); no other stored title is ever replaced here.
pub(crate) async fn settled_title(
    state: &AppState,
    conversation: &StoredProjectConversation,
    provider: Option<&str>,
) -> String {
    if let Some(title) = crate::t3_code::titles::follow_title(state, conversation).await {
        return title;
    }
    let title = display_title(Some(conversation), provider);
    if !conversation.title_chosen && is_untitled(&conversation.title) && !is_untitled(&title) {
        // A failed write only means the next read derives it again.
        let _ = state
            .store
            .name_untitled_project_conversation(
                &conversation.conversation_id,
                &conversation.title,
                &title,
            )
            .await;
    }
    title
}

/// A Claude session's title from the bridge's session list.
fn claude_title(session: &Value) -> String {
    match claude_title_source(session) {
        Some(title) => display_title(None, Some(title)),
        None => "Claude session".into(),
    }
}

fn claude_title_source(session: &Value) -> Option<&str> {
    session
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
}

/// Agent runs started from a written brief (scripts/orch) open with the same
/// boilerplate, so their first line ("GOAL", "You are reviewing the Wonder
/// change…") says nothing. They work on an `agent/<task>` branch; that task
/// names them, else the goal's words do. Claude previews arrive on one line,
/// so this reads words.
fn brief_title(text: &str) -> Option<String> {
    let text = text.trim_start();
    let task = || {
        text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '`' | '"' | '\''))
            .find_map(|word| {
                let word = word.strip_prefix("branch:").unwrap_or(word);
                let task = word
                    .strip_prefix("agent/")?
                    .trim_end_matches(['.', ':', ')']);
                (!task.is_empty()).then(|| task.chars().take(100).collect::<String>())
            })
    };
    if text.starts_with("You are reviewing ")
        || text.starts_with("You are the one independent verifier")
    {
        return Some(match task() {
            Some(task) => format!("Review {task}"),
            None => "Review".into(),
        });
    }
    let label = text.trim_start_matches('#').trim_start();
    let goal = label.strip_prefix("GOAL")?;
    if !goal.starts_with([':', ' ', '\n', '\r', '\t']) && !goal.is_empty() {
        return None;
    }
    task().or_else(|| {
        // No branch named: the goal itself, without its label.
        let goal = goal.trim_start_matches([':', ' ', '\t']).trim_start();
        goal.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(120).collect())
    })
}

#[derive(Clone, Debug)]
struct LinkedWorktree {
    path: String,
    label: String,
}

type FileStamp = Option<(std::time::SystemTime, u64)>;
struct CachedWorktrees {
    watches: Vec<PathBuf>,
    stamps: Vec<FileStamp>,
    rows: Vec<LinkedWorktree>,
    /// These rows were written to the store's `project_worktrees`, which
    /// decides Project membership in SQL. Each discovery writes them again.
    stored: bool,
    /// Which discovery this is; only the newest one's rows are written.
    generation: u64,
}

static WORKTREE_DISCOVERIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A Git pointer file's target (`gitdir: <path>` in a worktree's `.git`,
/// `<path>` in `commondir`/`gitdir`), resolved against `base`. Bounded read.
fn git_pointer(file: &FsPath, prefix: &str, base: &FsPath) -> Option<PathBuf> {
    // Never open a FIFO or device: discovery holds the worktree cache lock.
    if !std::fs::symlink_metadata(file).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(std::fs::File::open(file).ok()?, 4096),
        &mut text,
    )
    .ok()?;
    let target = text.strip_prefix(prefix)?.trim_end_matches(['\r', '\n']);
    (!target.is_empty() && !target.contains(['\r', '\n'])).then(|| base.join(target))
}

/// Whether `path` is a linked worktree registered in `common`, the Git folder
/// its Project folder shares, proven from Git's own files rather than from
/// the path: its `.git` is a regular file naming an admin folder directly in
/// `<common>/worktrees`, whose `commondir` leads back to `common` and whose
/// `gitdir` points back at this checkout. Paths are compared canonicalized and
/// whole, so `..` segments, symlinks and look-alike prefixes do not pass.
/// A thread whose recorded folder is a symlink to a worktree stays unowned
/// (fails closed). Provider-home worktrees (~/.codex/worktrees) can belong;
/// Files and Terminal still refuse denied roots on their own.
fn registered_worktree(common: &FsPath, path: &FsPath) -> bool {
    let verify = || -> Option<()> {
        let common = crate::canonical_path::canonicalize(common).ok()?;
        let path = crate::canonical_path::canonicalize(path).ok()?;
        let dot_git = path.join(".git");
        std::fs::symlink_metadata(&dot_git)
            .ok()?
            .is_file()
            .then_some(())?;
        let admin =
            crate::canonical_path::canonicalize(git_pointer(&dot_git, "gitdir: ", &path)?).ok()?;
        (admin.parent()? == common.join("worktrees")).then_some(())?;
        let shared = git_pointer(&admin.join("commondir"), "", &admin)?;
        (crate::canonical_path::canonicalize(shared).ok()? == common).then_some(())?;
        let back = git_pointer(&admin.join("gitdir"), "", &admin)?;
        (crate::canonical_path::canonicalize(back).ok()? == dot_git).then_some(())
    };
    verify().is_some()
}

fn file_stamps(paths: &[PathBuf]) -> Vec<FileStamp> {
    paths.iter().map(|path| file_stamp(path)).collect()
}

fn git_output(root: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(["--no-optional-locks", "-C", root])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

impl CachedWorktrees {
    fn discover(root: &str) -> Self {
        // Watching ancestors also notices a newly initialized repository or a
        // replaced .git indirection without periodically running Git.
        let mut watches: Vec<_> = FsPath::new(root)
            .ancestors()
            .map(|p| p.join(".git"))
            .collect();
        let mut rows = Vec::new();
        // Only a folder that is its checkout's top level has worktrees: a
        // folder inside a larger repository (a monorepo package, a home
        // folder kept in Git) must not gain that repository's checkouts.
        let line = |args: &[&str]| {
            git_output(root, args)
                .and_then(|b| String::from_utf8(b).ok())
                .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
        };
        let layout = (|| {
            let root = crate::canonical_path::canonicalize(root).ok()?;
            let common = line(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
            let common = crate::canonical_path::canonicalize(common).ok()?;
            let top_level = match line(&["rev-parse", "--path-format=absolute", "--show-toplevel"])
            {
                Some(top) => crate::canonical_path::canonicalize(top).ok()? == root,
                // A bare repository saved directly, or a folder whose `.git`
                // file points at one (`.bare` layouts), has no work tree.
                None => {
                    line(&["rev-parse", "--is-bare-repository"]).as_deref() == Some("true")
                        && (root == common
                            || git_pointer(&root.join(".git"), "gitdir: ", &root)
                                .and_then(|bare| crate::canonical_path::canonicalize(bare).ok())
                                == Some(common.clone()))
                }
            };
            top_level.then_some(common)
        })();
        if let Some(common) = layout {
            watches.push(common.join("worktrees"));
            if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
                for entry in entries.flatten() {
                    watches.extend([entry.path().join("HEAD"), entry.path().join("gitdir")]);
                }
            }
            if let Some(output) = git_output(root, &["worktree", "list", "--porcelain", "-z"]) {
                let mut path = None;
                let mut branch = None;
                let mut excluded = false;
                for field in output.split(|b| *b == 0) {
                    if field.is_empty() {
                        if let Some(path) = path.take() {
                            let path: String = path;
                            // Only linked worktrees, not the primary checkout,
                            // bare repositories or stale/prunable registrations.
                            if !excluded && registered_worktree(&common, FsPath::new(&path)) {
                                if let Ok(canonical) = crate::canonical_path::canonicalize(&path) {
                                    let path = canonical.to_string_lossy().into_owned();
                                    watches.push(FsPath::new(&path).join(".git"));
                                    rows.push(LinkedWorktree {
                                        label: branch.take().unwrap_or_else(|| folder_name(&path)),
                                        path,
                                    });
                                }
                            }
                        }
                        branch = None;
                        excluded = false;
                    } else if let Some(value) = field.strip_prefix(b"worktree ") {
                        path = String::from_utf8(value.to_vec()).ok();
                    } else if let Some(value) = field.strip_prefix(b"branch refs/heads/") {
                        branch = String::from_utf8(value.to_vec()).ok();
                    } else if field == b"bare" || field.starts_with(b"prunable") {
                        excluded = true;
                    }
                }
            }
        }
        let stamps = file_stamps(&watches);
        Self {
            watches,
            stamps,
            rows,
            stored: false,
            generation: WORKTREE_DISCOVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// A working folder's checked-out branch, read from Git's HEAD file rather
/// than a Git process. The entry is re-read only when that file changes; a
/// folder outside any repository is looked at again after `NO_REPOSITORY_TTL`.
struct CachedBranch {
    head: Option<PathBuf>,
    stamp: FileStamp,
    checked: Instant,
    branch: Option<String>,
}

const NO_REPOSITORY_TTL: Duration = Duration::from_secs(30);

#[cfg(test)]
static HEAD_DISCOVERIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl CachedBranch {
    fn discover(cwd: &str) -> Self {
        #[cfg(test)]
        HEAD_DISCOVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let head = head_file(FsPath::new(cwd));
        let stamp = head.as_deref().and_then(file_stamp);
        Self {
            branch: head.as_deref().and_then(head_branch),
            head,
            stamp,
            checked: Instant::now(),
        }
    }

    fn current(&self) -> bool {
        match &self.head {
            Some(head) => file_stamp(head) == self.stamp && self.stamp.is_some(),
            None => self.checked.elapsed() < NO_REPOSITORY_TTL,
        }
    }
}

fn file_stamp(path: &FsPath) -> FileStamp {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

/// The HEAD file of the repository or linked worktree containing `cwd`.
fn head_file(cwd: &FsPath) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        let Ok(metadata) = std::fs::metadata(&git) else {
            continue;
        };
        if metadata.is_dir() {
            return Some(git.join("HEAD"));
        }
        // A linked worktree's `.git` names its own Git folder.
        if metadata.len() > 4096 {
            return None;
        }
        let text = std::fs::read_to_string(&git).ok()?;
        let target = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?;
        return Some(dir.join(target.trim()).join("HEAD"));
    }
    None
}

/// The branch HEAD points at; a detached HEAD has none.
fn head_branch(head: &FsPath) -> Option<String> {
    if std::fs::metadata(head).ok()?.len() > 4096 {
        return None;
    }
    let text = std::fs::read_to_string(head).ok()?;
    text.trim_end()
        .strip_prefix("ref: refs/heads/")
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned)
}

/// Fill each row's `branch` from its working folder. One blocking pass per
/// list page, one cached entry per folder, so many threads in one folder
/// cost one HEAD check.
async fn thread_branches(state: &AppState, threads: &mut [&mut ProjectThreadSummary]) {
    let folders: HashSet<String> = threads
        .iter()
        .filter(|thread| thread.branch.is_none())
        .filter_map(|thread| thread.cwd.clone())
        .collect();
    if folders.is_empty() {
        return;
    }
    let cache = state.projects.branches.clone();
    let Ok(branches) = tokio::task::spawn_blocking(move || {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 1024 {
            cache.clear();
        }
        folders
            .into_iter()
            .map(|cwd| {
                let entry = cache
                    .entry(cwd.clone())
                    .or_insert_with(|| CachedBranch::discover(&cwd));
                if !entry.current() {
                    *entry = CachedBranch::discover(&cwd);
                }
                (cwd, entry.branch.clone())
            })
            .collect::<HashMap<_, _>>()
    })
    .await
    else {
        return;
    };
    for thread in threads.iter_mut() {
        if thread.branch.is_none() {
            thread.branch = thread
                .cwd
                .as_ref()
                .and_then(|cwd| branches.get(cwd).cloned().flatten());
        }
    }
}

/// How long one list page may spend asking runtimes about background work.
const BACKGROUND_PROBE: Duration = Duration::from_millis(300);
/// Native IDs of idle threads whose runtime keeps background terminals
/// (`BackgroundWork::Terminals`). Only threads loaded in Wonder's runtime can
/// hold them, so one roster read bounds the per-thread questions.
async fn terminal_background(
    state: &AppState,
    family: AgentFamily,
    ids: HashSet<String>,
) -> HashSet<String> {
    if ids.is_empty() {
        return HashSet::new();
    }
    let Some(rpc) = live_rpc_for(state, family).await else {
        return HashSet::new();
    };
    tokio::time::timeout(BACKGROUND_PROBE, async {
        let Ok(loaded) = result(&rpc, "thread/loaded/list", json!({})).await else {
            return HashSet::new();
        };
        let loaded = loaded["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row.as_str().or_else(|| row["id"].as_str()))
            .filter(|id| ids.contains(*id))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        futures_util::future::join_all(loaded.into_iter().map(|id| {
            let rpc = rpc.clone();
            async move {
                let page = result(
                    &rpc,
                    "thread/backgroundTerminals/list",
                    json!({"threadId": id, "limit": 1}),
                )
                .await
                .ok()?;
                page["data"]
                    .as_array()
                    .is_some_and(|rows| !rows.is_empty())
                    .then_some(id)
            }
        }))
        .await
        .into_iter()
        .flatten()
        .collect()
    })
    .await
    .unwrap_or_default()
}

/// Native IDs of idle attached sessions with work still running
/// (`BackgroundWork::Tasks`): tasks in a process Cupola retains or the
/// provider's app still runs, from their shared cached summary.
async fn task_background(
    state: &AppState,
    family: AgentFamily,
    sessions: HashMap<String, (i64, Option<String>)>,
    known: Option<&ListedThreads>,
) -> HashSet<String> {
    if sessions.is_empty() {
        return HashSet::new();
    }
    let Some(rpc) = live_rpc_for(state, family).await else {
        return HashSet::new();
    };
    let deadline = tokio::time::Instant::now() + BACKGROUND_PROBE;
    let read = tokio::time::timeout_at(deadline, async {
        // The page's activity read already listed these sessions' threads
        // (`claude_activity`); ask again only when it could not.
        let listed = match known.filter(|known| known.family == family) {
            Some(known) => sessions
                .keys()
                .filter_map(|native| Some((known.by_native.get(native)?.clone(), native.clone())))
                .collect(),
            None => {
                let mut ids = sessions.keys().cloned().collect::<Vec<_>>();
                ids.sort();
                ids.truncate(200);
                let listed = result(
                    &rpc,
                    "thread/list",
                    json!({"sessionIds": ids, "limit": 200}),
                )
                .await
                .ok()?;
                listed["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|row| row.get("parentThreadId").and_then(Value::as_str).is_none())
                    .filter_map(|row| {
                        Some((
                            row["id"].as_str()?.to_owned(),
                            row["sessionId"].as_str()?.to_owned(),
                        ))
                    })
                    .collect::<Vec<_>>()
            }
        };
        Some(listed)
    })
    .await;
    let Ok(Some(listed)) = read else {
        return HashSet::new();
    };
    // Both retained and external processes use the same cached answer. A
    // slow session cannot block a roster for the page or clear a sibling's
    // verified badge. Refreshes run once per runtime generation and session.
    let asked = listed
        .into_iter()
        .filter_map(|(thread, native)| {
            let (_, conversation) = sessions.get(&native)?;
            Some(task_summary::Asked {
                thread,
                native,
                conversation: conversation.clone(),
            })
        })
        .collect();
    task_summary::running(state, &rpc, asked, deadline).await
}

/// The native ID of a row that has one (a draft has none).
fn native_id(thread: &ProjectThreadSummary) -> Option<&str> {
    thread
        .reference
        .strip_prefix(thread.family.as_str())?
        .strip_prefix(':')
}

/// Mark rows whose agent still works after its reply ended, by what the
/// provider reports (`ProviderCapabilities::background_work`). A row already
/// working needs no mark; an unanswered question leaves the row unmarked.
/// Sessions open in the provider's app on the Mac, attached or only
/// discovered, are marked from its live session records (`desktop`);
/// Wonder's runtime knows only the sessions it attached.
async fn mark_background_work(
    state: &AppState,
    threads: &mut [&mut ProjectThreadSummary],
    desktop: &crate::desktop_activity::ClaudeDesktopSessions,
    claude_threads: Option<&ListedThreads>,
) {
    let mut terminals = HashMap::<AgentFamily, HashSet<String>>::new();
    let mut tasks = HashMap::<AgentFamily, HashMap<String, (i64, Option<String>)>>::new();
    for thread in threads.iter().filter(|thread| !thread.is_working) {
        let Some(native) = native_id(thread) else {
            continue;
        };
        match thread.family.provider().capabilities.background_work {
            BackgroundWork::Terminals => {
                terminals
                    .entry(thread.family)
                    .or_default()
                    .insert(native.to_owned());
            }
            BackgroundWork::Tasks if thread.conversation_id.is_some() => {
                tasks.entry(thread.family).or_default().insert(
                    native.to_owned(),
                    (thread.updated_at, thread.conversation_id.clone()),
                );
            }
            BackgroundWork::Tasks | BackgroundWork::None => {}
        }
    }
    let (terminals, tasks) = tokio::join!(
        futures_util::future::join_all(terminals.into_iter().map(|(family, ids)| async move {
            (family, terminal_background(state, family, ids).await)
        })),
        futures_util::future::join_all(tasks.into_iter().map(|(family, sessions)| async move {
            (
                family,
                task_background(state, family, sessions, claude_threads).await,
            )
        })),
    );
    let running: HashMap<AgentFamily, HashSet<String>> =
        terminals.into_iter().chain(tasks).collect();
    for thread in threads.iter_mut().filter(|thread| !thread.is_working) {
        let Some(native) = native_id(thread).map(str::to_owned) else {
            continue;
        };
        let reported = running
            .get(&thread.family)
            .is_some_and(|ids| ids.contains(&native));
        thread.background_work = match thread.family.provider().capabilities.background_work {
            BackgroundWork::Terminals => reported,
            BackgroundWork::Tasks => reported || desktop.background.contains(&native),
            BackgroundWork::None => false,
        };
    }
}

/// What a list row shows beyond its own record: the folder's branch, work
/// still running after the reply, and the latest automation run. Each is one
/// batch per page, so a long list costs per row what a short one does.
async fn decorate(
    state: &AppState,
    mut threads: Vec<&mut ProjectThreadSummary>,
    desktop: &crate::desktop_activity::ClaudeDesktopSessions,
    claude_threads: Option<&ListedThreads>,
) {
    thread_branches(state, &mut threads).await;
    let ids = threads
        .iter()
        .filter_map(|thread| thread.conversation_id.clone())
        .collect::<Vec<_>>();
    if let Ok(mut runs) = state.store.latest_automation_runs(&ids).await {
        for thread in threads.iter_mut() {
            let Some(run) = thread
                .conversation_id
                .as_ref()
                .and_then(|id| runs.remove(id))
            else {
                continue;
            };
            thread.last_automation_at = Some(run.started_at.clone());
            thread.automation_run = Some(AutomationRunMark {
                automation_id: run.automation_id,
                name: run.automation_name,
                status: run.status,
                started_at: run.started_at,
                finished_at: run.finished_at,
                error: run.error,
            });
        }
    }
    if let Ok(resumes) = state.store.usage_resumes().await {
        for thread in threads.iter_mut() {
            thread.resumes_at = thread
                .conversation_id
                .as_ref()
                .and_then(|id| resumes.get(id))
                .and_then(|at| crate::usage_resume::timestamp(*at));
        }
    }
    mark_background_work(state, &mut threads, desktop, claude_threads).await;
}

/// Shared folder membership for native catalogs, attached rows and Live
/// Activity inputs. Configuration stays unchanged; only discovery expands.
struct ProjectFolders {
    project: StoredProject,
    labels: HashMap<String, String>,
}
impl ProjectFolders {
    fn label(&self, cwd: &str) -> Option<String> {
        self.labels.get(cwd).cloned()
    }
    fn contains(&self, cwd: &str) -> bool {
        self.project.root_for(cwd).is_some()
    }
    // Saved chats outlive removed Project folders. Only discovered worktree
    // chats are hidden when their path is absent from the effective folder set.
    fn shows_saved_chat(&self, cwd: &str) -> bool {
        !self.labels.contains_key(cwd) || self.contains(cwd)
    }
}

async fn project_folders(state: &AppState, project: &StoredProject) -> ProjectFolders {
    let include = state.store.include_worktrees().await.unwrap_or(false);
    let project = project.clone();
    let fallback = project.clone();
    let cache = state.projects.worktrees.clone();
    let (folders, unstored) = tokio::task::spawn_blocking(move || {
        let mut expanded = project.clone();
        let mut labels = HashMap::new();
        let mut unstored = Vec::new();
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() > 256 {
            cache.clear();
        }
        for root in &project.roots {
            let entry = cache
                .entry(root.canonical_path.clone())
                .or_insert_with(|| CachedWorktrees::discover(&root.canonical_path));
            if file_stamps(&entry.watches) != entry.stamps {
                *entry = CachedWorktrees::discover(&root.canonical_path);
            }
            let mut members = Vec::new();
            for row in &entry.rows {
                // Native titles already live in the provider's history. A
                // linked checkout under ~/.codex/worktrees or ~/.claude/worktrees
                // may be discovered without granting file/execution access to
                // its protected parent. Those flows still validate denied roots.
                let Ok(valid) = validate_folder(&row.path, &[]) else {
                    continue;
                };
                members.push(row.path.clone());
                labels.insert(row.path.clone(), row.label.clone());
                if include && expanded.root_for(&row.path).is_none() {
                    expanded.roots.push(wonder_store::StoredProjectRoot {
                        id: format!(
                            "worktree:{}",
                            hex::encode(&Sha256::digest(row.path.as_bytes())[..12])
                        ),
                        path: row.path.clone(),
                        canonical_path: valid.canonical_path,
                        ordinal: root.ordinal,
                    });
                }
            }
            if !entry.stored {
                unstored.push((root.canonical_path.clone(), entry.generation, members));
            }
        }
        (
            ProjectFolders {
                project: expanded,
                labels,
            },
            unstored,
        )
    })
    .await
    .unwrap_or_else(|_| {
        (
            ProjectFolders {
                project: fallback,
                labels: HashMap::new(),
            },
            Vec::new(),
        )
    });
    // Membership checks in SQL (approvals, questions, push, Live Activity,
    // previews) see the same worktrees this listing does.
    let current = |root: &str, generation: u64| {
        let cache = state
            .projects
            .worktrees
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.get(root).is_some_and(|e| e.generation == generation)
    };
    if !unstored.is_empty() {
        let _order = state.projects.worktree_writes.lock().await;
        for (root, generation, members) in unstored {
            // A newer discovery of this folder writes its own rows.
            if !current(&root, generation) {
                continue;
            }
            // Marked only once written: a dropped request or failed write
            // leaves the next listing to write them.
            if state
                .store
                .replace_project_worktrees(&root, &members)
                .await
                .is_ok()
            {
                let mut cache = state
                    .projects
                    .worktrees
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = cache.get_mut(&root).filter(|e| e.generation == generation) {
                    entry.stored = true;
                }
            }
        }
    }
    folders
}

/// Workspace access follows this checkout instead of its parent checkout;
/// other explicitly configured Project folders retain their grants. Discovery
/// never gives a thread write access to sibling worktrees.
fn execution_project(
    project: &StoredProject,
    root: &wonder_store::StoredProjectRoot,
) -> StoredProject {
    let mut project = project.clone();
    if project.root_for(&root.path).is_none() {
        if let Some(parent) = project
            .roots
            .iter_mut()
            .find(|parent| parent.ordinal == root.ordinal)
        {
            *parent = root.clone();
        }
    }
    project
}

/// The Project as a thread working in `cwd` sees it: the same expanded
/// folder set the thread lists use, with a discovered worktree standing in
/// for its parent checkout. Unchanged when `cwd` is not in that set, so
/// callers keep their own `root_for` membership checks.
pub(crate) async fn thread_project(
    state: &AppState,
    project: StoredProject,
    cwd: &str,
) -> StoredProject {
    if project.root_for(cwd).is_some() {
        return project;
    }
    let folders = project_folders(state, &project).await;
    let resolved = folders
        .project
        .root_for(cwd)
        .map(|root| execution_project(&project, root));
    resolved.unwrap_or(project)
}

/// Every checkout of the Project: its saved folders and each linked worktree
/// Git registered for them, whether or not worktree chats are on. Paths are
/// as discovered (not all canonical). A feature that follows one thread's
/// folder uses this to leave out checkouts nested inside it, such as
/// `.local/wt-*` or `.claude/worktrees/*` under the main checkout.
pub(crate) async fn checkout_paths(state: &AppState, project: &StoredProject) -> Vec<String> {
    let folders = project_folders(state, project).await;
    let mut paths: Vec<String> = folders
        .project
        .roots
        .iter()
        .map(|root| root.canonical_path.clone())
        .collect();
    paths.extend(folders.labels.into_keys());
    paths.sort();
    paths.dedup();
    paths
}

/// The same resolution by folder ID: a discovered worktree's ID (as thread
/// lists and catalogs report it) names that checkout. `None` when the ID is
/// in neither the saved nor the discovered folder set.
pub(crate) async fn folder_project(
    state: &AppState,
    project: StoredProject,
    folder_id: &str,
) -> Option<(StoredProject, wonder_store::StoredProjectRoot)> {
    if let Some(root) = project.roots.iter().find(|root| root.id == folder_id) {
        let root = root.clone();
        return Some((project, root));
    }
    let folders = project_folders(state, &project).await;
    let root = folders
        .project
        .roots
        .into_iter()
        .find(|root| root.id == folder_id)?;
    Some((execution_project(&project, &root), root))
}

/// A linked checkout of one of the Project's folders that the effective
/// folder set leaves out (worktree chats are turned off). Lets a refusal say
/// "add this worktree" instead of "access changed".
pub(crate) async fn is_unlisted_worktree(
    state: &AppState,
    project: &StoredProject,
    cwd: &str,
) -> bool {
    let folders = project_folders(state, project).await;
    folders.labels.contains_key(cwd) && !folders.contains(cwd)
}

/// Response header naming why a thread's folder was refused, for clients
/// that offer a recovery action rather than Try Again.
pub(crate) const ERROR_CODE_HEADER: &str = "x-wonder-error";
pub(crate) const WORKTREE_NOT_IN_PROJECT: &str = "worktree-not-in-project";

/// Only nonarchived conversations whose Project can show them. Both owned
/// and desktop activity pass through this projection before being counted.
pub(crate) async fn visible_conversations(
    state: &AppState,
) -> Result<HashSet<String>, sqlx::Error> {
    let revision = state.store.project_visibility_revision().await?;
    let mut cache = state.projects.visibility.lock().await;
    if let Some((saved_revision, at, visible)) = cache.as_ref() {
        // Git worktree membership can change outside the database. Revisit
        // disk at least every five seconds even without a stored change.
        if *saved_revision == revision && at.elapsed() < Duration::from_secs(5) {
            return Ok(visible.clone());
        }
    }
    let mut visible = HashSet::new();
    for project in state
        .store
        .list_projects()
        .await?
        .into_iter()
        .filter(|p| p.is_included)
    {
        let folders = project_folders(state, &project).await;
        for (conversation, cwd) in state
            .store
            .visible_project_conversations(&project.id)
            .await?
        {
            if folders.shows_saved_chat(&cwd) {
                visible.insert(conversation);
            }
        }
    }
    if state.store.project_visibility_revision().await? == revision {
        *cache = Some((revision, Instant::now(), visible.clone()));
    }

    Ok(visible)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProjectSettings {
    include_worktrees: Option<bool>,
    auto_archive: Option<AutoArchiveSetting>,
    /// Resume a thread a usage limit stopped once the limit resets.
    resume_after_limit: Option<bool>,
    /// Continue a reply a crash or restart of Cupola or the computer cut off.
    continue_after_restart: Option<bool>,
}

async fn settings_response(state: &AppState) -> Response {
    match tokio::try_join!(
        state.store.include_worktrees(),
        state.store.auto_archive_rule(),
        state.store.continuation_settings()
    ) {
        Ok((enabled, rule, continuation)) => Json(json!({
            "includeWorktrees": enabled,
            "autoArchive": AutoArchiveSetting::from(&rule),
            "resumeAfterLimit": continuation.resume_after_limit,
            "continueAfterRestart": continuation.continue_after_restart,
        }))
        .into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Project settings could not be read. Try again.",
        ),
    }
}

pub(crate) async fn settings(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
) -> Response {
    settings_response(&state).await
}

pub(crate) async fn set_settings(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
    Json(request): Json<ProjectSettings>,
) -> Response {
    if let Some(rule) = request.auto_archive {
        // As for a Project's rule: never between an archive's read and write.
        let _dispatch = state.dispatch_lock.lock().await;
        match state
            .store
            .set_auto_archive_rule(rule.idle_days, rule.on_merge, &now_text())
            .await
        {
            Ok(_) => {}
            Err(sqlx::Error::Protocol(message)) => {
                return error(StatusCode::UNPROCESSABLE_ENTITY, message)
            }
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Auto-archive could not be saved. Try again.",
                )
            }
        }
    }
    let continuation_changed =
        request.resume_after_limit.is_some() || request.continue_after_restart.is_some();
    if continuation_changed
        && crate::update_handoff::set_continuation_settings(
            &state,
            request.resume_after_limit,
            request.continue_after_restart,
        )
        .await
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Continuing work could not be saved. Try again.",
        );
    }
    let Some(include_worktrees) = request.include_worktrees else {
        return settings_response(&state).await;
    };
    if state
        .store
        .set_include_worktrees(include_worktrees)
        .await
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Project settings could not be saved. Try again.",
        );
    }
    state
        .projects
        .cursors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    announce_chats_changed(&state).await;
    settings_response(&state).await
}

fn root_paths(project: &StoredProject) -> Vec<String> {
    let mut paths = Vec::new();
    for root in &project.roots {
        for path in [&root.path, &root.canonical_path] {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
    }
    paths
}

/// `codex exec` runs are one-shot commands, not chats: they never enter the
/// list. Key on the source too: other hosts (T3 Code) also launch `exec` runs
/// and some rollouts carry no originator.
fn codex_one_shot(thread: &Value) -> bool {
    thread.get("originator").and_then(Value::as_str) == Some("codex_exec")
        || thread.get("source").and_then(Value::as_str) == Some("exec")
}

/// Creation metadata, not App Server membership or the presence of a native ID.
/// Require the producer's fork field too: a copied origin is not a new origin.
fn codex_computer_session(thread: &Value) -> bool {
    thread.get("forkedFromId").is_some_and(Value::is_null)
        && matches!(
            (
                thread.get("source").and_then(Value::as_str),
                thread.get("originator").and_then(Value::as_str)
            ),
            (Some("vscode"), Some("Codex Desktop")) | (Some("cli" | "vscode"), Some("codex-tui"))
        )
}

/// Pins live outside native paging. Read only exact metadata (no resume or turn),
/// with four requests at once and one 750 ms budget for the entire decoration.
/// A timeout leaves unknown rows unbadged; it never blocks the catalog itself.
async fn mark_computer_sessions<'a>(
    state: &AppState,
    threads: impl Iterator<Item = &'a mut ProjectThreadSummary>,
) {
    use futures_util::{stream, StreamExt};
    let mut threads = threads
        .filter(|row| !row.is_computer_session)
        .collect::<Vec<_>>();
    let requests = threads
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.conversation_id.clone().map(|id| (index, id)))
        .collect::<Vec<_>>();
    let reads = stream::iter(requests.into_iter().map(|(index, id)| async move {
        let evidence = async {
            let conversation = state.store.project_conversation(&id).await.ok()??;
            let native = conversation.native_session_id.as_deref()?;
            let provider = conversation.family.provider();
            if !provider.capabilities.native_sessions {
                return None;
            }
            let rpc = crate::providers::rpc_for_store(
                state,
                conversation.family,
                &conversation.provider_store,
            )
            .await
            .ok()?;
            match provider.native_session {
                wonder_store::NativeSessionKey::ThreadId => {
                    if conversation.creation_request_id.is_some() {
                        return None;
                    }
                    let read = result(
                        &rpc,
                        "thread/read",
                        json!({"threadId":native,"includeTurns":false}),
                    )
                    .await
                    .ok()?;
                    let thread = read.get("thread")?;
                    (thread.get("id").and_then(Value::as_str) == Some(native))
                        .then(|| codex_computer_session(thread))
                }
                wonder_store::NativeSessionKey::BridgeSessionId => {
                    let page = result(
                        &rpc,
                        "project/sessions/list",
                        json!({"dirs":[conversation.cwd],"sessionId":native,"limit":1}),
                    )
                    .await
                    .ok()?;
                    let row =
                        page.get("data")?.as_array()?.iter().find(|row| {
                            row.get("sessionId").and_then(Value::as_str) == Some(native)
                        })?;
                    row.get("isComputerSession").and_then(Value::as_bool)
                }
            }
        };
        (
            index,
            tokio::time::timeout(Duration::from_millis(250), evidence)
                .await
                .ok()
                .flatten()
                == Some(true),
        )
    }))
    .buffer_unordered(4);
    tokio::pin!(reads);
    let batch = async {
        while let Some((index, positive)) = reads.next().await {
            threads[index].is_computer_session |= positive;
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(750), batch).await;
}

async fn fill_codex(
    state: &AppState,
    folders: &ProjectFolders,
    page: &mut ProviderPage,
    limit: usize,
    deadline: tokio::time::Instant,
) {
    let project = &folders.project;
    if page.done || page.failed || page.deferred || !page.buffer.is_empty() {
        return;
    }
    let fetched = tokio::time::timeout_at(deadline, async {
        let rpc = codex_rpc(state).await?;
        // Codex's thread index, not a scan of every saved rollout: the scan took
        // 13-24 s per folder (measured on 945 threads) and outlasted the phone's
        // request, while the index answered in 10 ms with the same threads and
        // titles (it also kept a 141 MB thread the scan dropped).
        result(&rpc, "thread/list", json!({
            "cwd": root_paths(project), "sourceKinds": CODEX_SOURCE_KINDS, "archived": false,
            "useStateDbOnly": true, "limit": limit.max(30), "sortKey": "updated_at", "sortDirection": "desc", "cursor": page.cursor,
        }))
        .await
    })
    .await
    .unwrap_or_else(|_| Err("Thread discovery timed out.".into()));
    match fetched {
        Ok(value) => {
            for thread in value
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(id) = thread.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // Helper threads remain under their parent.
                if thread
                    .get("parentThreadId")
                    .and_then(Value::as_str)
                    .is_some()
                    || codex_one_shot(thread)
                {
                    continue;
                }
                if let Some(path) = thread.get("path").and_then(Value::as_str) {
                    crate::desktop_activity::remember_codex_rollout(id, path);
                }
                page.buffer.push_back(NativeThread {
                    family: AgentFamily::Codex,
                    store: state.projects.codex_store.clone(),
                    is_computer_session: codex_computer_session(thread),
                    id: id.to_owned(),
                    worktree_label: thread
                        .get("cwd")
                        .and_then(Value::as_str)
                        .and_then(|cwd| folders.label(cwd)),
                    cwd: thread.get("cwd").and_then(Value::as_str).map(str::to_owned),
                    title: thread_title(thread),
                    source: native_title_source(thread).map(str::to_owned),
                    is_working: thread.pointer("/status/type").and_then(Value::as_str)
                        == Some("active"),
                    updated_at: thread
                        .get("updatedAt")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                });
            }
            page.cursor = value
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            page.done = page.cursor.is_none();
        }
        Err(_) => page.failed = true,
    }
}

async fn fill_claude(
    state: &AppState,
    folders: &ProjectFolders,
    page: &mut ProviderPage,
    limit: usize,
    deadline: tokio::time::Instant,
) {
    let project = &folders.project;
    if page.done || page.failed || page.deferred || !page.buffer.is_empty() {
        return;
    }
    if state.claude.is_none() {
        page.done = true;
        return;
    }
    let fetched: Result<Result<Value, String>, _> = tokio::time::timeout_at(deadline, async {
        let rpc = claude_rpc(state).await?;
        let mut cursor = page.cursor.clone();
        // A cold folder can outlast the sidecar's one-second page budget.
        // Give its retained read one continuation within this same deadline,
        // so the first HTTP page can contain its rows. Later/filtered pages
        // still defer normally; never turn this into an unbounded scan.
        for attempt in 0..2 {
            let value = result(
                &rpc,
                "project/sessions/list",
                json!({
                    "dirs": root_paths(project), "limit": limit.max(30), "cursor": cursor,
                }),
            )
            .await?;
            if attempt == 0
                && page.cursor.is_none()
                && value.get("partial").and_then(Value::as_bool) == Some(true)
                && value
                    .get("data")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            {
                if let Some(next) = value
                    .get("nextCursor")
                    .and_then(Value::as_str)
                    .filter(|next| next.len() <= 256 * 1024)
                {
                    cursor = Some(next.to_owned());
                    continue;
                }
            }
            return Ok(value);
        }
        unreachable!("The second native page always returns")
    })
    .await;
    // A slow catalog or runtime is still loading, not missing chats: keep the
    // cursor so the next page resumes it.
    let Ok(fetched) = fetched else {
        page.deferred = true;
        return;
    };
    match fetched {
        Ok(value) => {
            let Some(data) = value
                .get("data")
                .and_then(Value::as_array)
                .filter(|rows| rows.len() <= 100)
            else {
                page.failed = true;
                return;
            };
            let next = match value.get("nextCursor") {
                Some(Value::Null) => None,
                Some(Value::String(next)) if next.len() <= 256 * 1024 => Some(next.clone()),
                _ => {
                    page.failed = true;
                    return;
                }
            };
            let Some(partial) = value.get("partial").and_then(Value::as_bool) else {
                page.failed = true;
                return;
            };
            if !partial && data.is_empty() && next.is_some() && next == page.cursor {
                page.failed = true;
                return;
            }
            for session in data {
                let Some(id) = session.get("sessionId").and_then(Value::as_str) else {
                    continue;
                };
                page.buffer.push_back(NativeThread {
                    family: AgentFamily::Claude,
                    store: state.projects.claude_store.clone(),
                    is_computer_session: session.get("isComputerSession").and_then(Value::as_bool)
                        == Some(true),
                    id: id.to_owned(),
                    worktree_label: session
                        .get("cwd")
                        .and_then(Value::as_str)
                        .and_then(|cwd| folders.label(cwd)),
                    cwd: session
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    is_working: false,
                    title: claude_title(session),
                    source: claude_title_source(session).map(str::to_owned),
                    updated_at: session
                        .get("updatedAt")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                });
            }
            page.cursor = next;
            page.done = page.cursor.is_none();
            page.deferred = partial;
        }
        Err(_) => page.failed = true,
    }
}

/// How a provider session belongs to a Wonder thread.
enum Owner {
    /// The thread uses it now.
    Current,
    /// The thread moved to another session and keeps this one as history.
    Left,
}

async fn owning_conversation(
    state: &AppState,
    family: AgentFamily,
    store: &str,
    native: &str,
) -> Result<Option<(StoredProjectConversation, Owner)>, sqlx::Error> {
    if let Some(current) = state
        .store
        .project_conversation_by_native(family, store, native)
        .await?
    {
        return Ok(Some((current, Owner::Current)));
    }
    Ok(state
        .store
        .project_conversation_by_left_session(family, store, native)
        .await?
        .map(|left| (left, Owner::Left)))
}

fn reference(family: AgentFamily, id: &str) -> String {
    format!("{}:{id}", family.as_str())
}

pub(crate) async fn attached_summary(
    state: &AppState,
    conversation: &StoredProjectConversation,
    updated_at: i64,
    provider: Option<&str>,
) -> ProjectThreadSummary {
    let folders = match state.store.project(&conversation.project_id).await {
        Ok(Some(project)) => Some(project_folders(state, &project).await),
        _ => None,
    };
    attached_summary_with_folders(state, conversation, updated_at, provider, folders.as_ref()).await
}

async fn attached_summary_with_folders(
    state: &AppState,
    conversation: &StoredProjectConversation,
    updated_at: i64,
    provider: Option<&str>,
    folders: Option<&ProjectFolders>,
) -> ProjectThreadSummary {
    let title = settled_title(state, conversation, provider).await;
    let is_working = state
        .store
        .conversation_has_active_turn(&conversation.conversation_id)
        .await
        .unwrap_or(false);
    let current_activity = if is_working {
        history::current_activity(state, &conversation.conversation_id).await
    } else {
        None
    };
    let worktree_label = folders.and_then(|folders| folders.label(&conversation.cwd));
    ProjectThreadSummary {
        // A draft has no provider session yet; it opens by conversation ID.
        reference: match conversation.native_session_id.as_deref() {
            Some(native) => reference(conversation.family, native),
            None => format!("wonder:{}", conversation.conversation_id),
        },
        conversation_id: Some(conversation.conversation_id.clone()),
        title,
        family: conversation.family,
        updated_at,
        is_pinned: conversation.is_pinned,
        has_unread: conversation.has_unread,
        is_working,
        current_activity,
        needs_input_on_mac: false,
        is_computer_session: false,
        worktree_label,
        // Only a chat-specific permission T3 Code asks for on the Mac.
        waiting_on_mac_permission: state
            .t3_code
            .approves_on_computer(&conversation.conversation_id),
        branch: None,
        background_work: false,
        is_subagent: false,
        last_automation_at: None,
        automation_run: None,
        resumes_at: None,
        cwd: Some(conversation.cwd.clone()),
    }
}

/// One thread's row for a create, attach or fork receipt, with its branch.
/// A new row has no automation runs, and its background work shows on the
/// next list read.
async fn single_summary(
    state: &AppState,
    conversation: &StoredProjectConversation,
    updated_at: i64,
    provider: Option<&str>,
) -> ProjectThreadSummary {
    let mut summary = attached_summary(state, conversation, updated_at, provider).await;
    thread_branches(state, &mut [&mut summary]).await;
    summary
}

#[cfg(test)]
thread_local! {
    /// A test's budget for the owned Claude read in place of the page's 250 ms,
    /// so its assertions do not race the wall clock. Thread-local: only the
    /// test that sets it (its current-thread runtime) sees it.
    pub(crate) static CLAUDE_READ_BUDGET: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

fn claude_read_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = CLAUDE_READ_BUDGET.with(std::cell::Cell::get) {
        return budget;
    }
    Duration::from_millis(250)
}

/// The native process can continue after its prompt receipt completed. Read
/// its state for this page; missing/idle state cannot erase other evidence.
/// The boolean describes the owned lookup, not exhaustive external liveness:
/// absent IDs are unowned, and the external fallback reports positive state only.
#[cfg(test)]
async fn claude_working_sessions(
    state: &AppState,
    native_ids: HashSet<String>,
) -> (crate::desktop_activity::ClaudeDesktopSessions, bool) {
    let (sessions, complete, _) = claude_activity(state, native_ids).await;
    (sessions, complete)
}

/// The provider that supplied this page's native-session to thread mapping.
/// Carry its identity with the data so another task provider cannot reuse it.
struct ListedThreads {
    family: AgentFamily,
    by_native: HashMap<String, String>,
}

/// `claude_working_sessions`, with the Claude thread of each top-level
/// session the read listed. That map is None unless the read answered for
/// every requested session, so a session missing from it has no thread.
async fn claude_activity(
    state: &AppState,
    native_ids: HashSet<String>,
) -> (
    crate::desktop_activity::ClaudeDesktopSessions,
    bool,
    Option<ListedThreads>,
) {
    let family = AgentFamily::Claude;
    if native_ids.is_empty() {
        return (
            Default::default(),
            true,
            Some(ListedThreads {
                family,
                by_native: HashMap::new(),
            }),
        );
    }
    // The global pinned list is capped at 50. A Project can also include all
    // recent creations and pins, so its visible set can exceed a native page.
    let mut ids = native_ids.iter().collect::<Vec<_>>();
    ids.sort();
    let mut busy = HashSet::new();
    let mut threads = HashMap::new();
    // A row without its thread leaves the map incomplete, not the activity read.
    let mut threads_complete = true;
    let mut owned_read_complete = ids.len() <= 1600;
    let owned = async {
        let rpc = live_rpc_for(state, family).await?;
        for ids in ids.chunks(200).take(8) {
            let page = result(
                &rpc,
                "thread/list",
                json!({"sessionIds": ids, "limit": 200}),
            )
            .await
            .ok()?;
            let rows = page.get("data")?.as_array()?;
            if page
                .get("nextCursor")
                .is_none_or(|cursor| !cursor.is_null())
            {
                owned_read_complete = false;
            }
            for row in rows {
                let Some(native) = row.get("sessionId").and_then(Value::as_str) else {
                    owned_read_complete = false;
                    continue;
                };
                if !ids.iter().any(|id| id.as_str() == native)
                    || row.get("parentThreadId").and_then(Value::as_str).is_some()
                {
                    continue;
                }
                match row.get("id").and_then(Value::as_str) {
                    Some(thread) => {
                        threads.insert(native.to_owned(), thread.to_owned());
                    }
                    None => threads_complete = false,
                }
                match row.pointer("/status/type").and_then(Value::as_str) {
                    Some("active") => {
                        busy.insert(native.to_owned());
                    }
                    Some("idle") => {}
                    _ => owned_read_complete = false,
                }
            }
        }
        Some(())
    };
    let (read, external) = tokio::join!(
        tokio::time::timeout(claude_read_budget(), owned),
        crate::desktop_activity::claude_busy_sessions(),
    );
    // A later batch timing out must not discard the earlier affirmative state.
    let mut sessions = external;
    sessions.busy.extend(busy);
    let complete = owned_read_complete && matches!(read, Ok(Some(())));
    (
        sessions,
        complete,
        (complete && threads_complete).then_some(ListedThreads {
            family,
            by_native: threads,
        }),
    )
}

fn claude_activity_partial(complete: bool) -> Vec<PartialFailure> {
    if complete {
        Vec::new()
    } else {
        vec![PartialFailure {
            family: AgentFamily::Claude,
            detail: "Some Claude Code activity could not be refreshed.".into(),
        }]
    }
}

fn claude_session_ids<'a>(
    threads: impl Iterator<Item = &'a ProjectThreadSummary>,
) -> HashSet<String> {
    threads
        .filter(|thread| thread.family == AgentFamily::Claude)
        .filter_map(|thread| thread.reference.strip_prefix("claude:").map(str::to_owned))
        .collect()
}

/// Work running in another app on the Mac. Codex's own rollout recency and
/// index decide, not when Wonder last used the thread.
async fn mark_desktop_working(
    state: &AppState,
    thread: &mut ProjectThreadSummary,
    claude: &crate::desktop_activity::ClaudeDesktopSessions,
) {
    let Some(native) = thread.reference.split_once(':').map(|(_, id)| id) else {
        return;
    };
    thread.needs_input_on_mac =
        thread.family == AgentFamily::Claude && claude.waiting.contains(native);
    if thread.is_working {
        return;
    }
    thread.is_working = match thread.family {
        AgentFamily::Claude => claude.busy.contains(native),
        AgentFamily::OpenCode | AgentFamily::Cursor => false,
        AgentFamily::Codex => {
            crate::desktop_activity::codex_thread_working(&state.codex_home, native).await
        }
    };
}

fn activity_seconds(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|t| t.timestamp())
        .unwrap_or_default()
}

/// Merged native Codex/Claude threads. Each provider keeps its own cursor and
/// buffer, so the combined page is globally ordered by recent activity.
pub(crate) async fn threads(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Query(query): Query<ThreadsQuery>,
) -> Response {
    match thread_page(&state, &id, query, MAX_PROJECT_THREAD_PAGE).await {
        Ok(page) => Json(page).into_response(),
        Err(response) => response,
    }
}

/// Most native rows one Project page returns to the Chats list.
const MAX_PROJECT_THREAD_PAGE: usize = 30;

/// One Project's page: pinned rows and drafts first (first page only), then
/// up to `limit` native rows (at most `max_limit`) by recent activity.
async fn thread_page(
    state: &AppState,
    id: &str,
    query: ThreadsQuery,
    max_limit: usize,
) -> Result<ThreadsPage, Response> {
    let state = state.clone();
    let limit = query.limit.unwrap_or(5).clamp(1, max_limit);
    let Ok(Some(project)) = state.store.project(id).await else {
        return Err(StatusCode::NOT_FOUND.into_response());
    };
    let folders = project_folders(&state, &project).await;
    let mut labels: Vec<_> = folders.labels.iter().collect();
    labels.sort();
    let folders_key = format!("{:?}{:?}", root_paths(&folders.project), labels);
    let (mut cursor, first) = match query.cursor.as_deref() {
        Some(token) => {
            let cursors = state
                .projects
                .cursors
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Cursors are immutable snapshots, so retrying a page after a
            // dropped response returns the same page instead of skipping it.
            match cursors.get(token).cloned() {
                Some((created, cursor))
                    if created.elapsed() < CURSOR_TTL
                        && cursor.project_id == project.id
                        && cursor.roots_revision == project.roots_revision
                        && cursor.folders_key == folders_key =>
                {
                    (cursor, false)
                }
                _ => {
                    return Err(error(
                        StatusCode::GONE,
                        "This list changed. Refresh to see the latest threads.",
                    ))
                }
            }
        }
        None => (
            CatalogCursor {
                project_id: project.id.clone(),
                roots_revision: project.roots_revision,
                folders_key,
                codex: ProviderPage::default(),
                claude: ProviderPage::default(),
                accounts: ProviderPage {
                    done: true,
                    ..ProviderPage::default()
                },
                emitted: HashSet::new(),
            },
            true,
        ),
    };
    cursor.codex.failed = false;
    cursor.codex.deferred = false;
    cursor.claude.failed = false;
    cursor.claude.deferred = false;
    let mut threads = Vec::new();
    // Threads Wonder just started that the provider may not list yet.
    let mut just_started = Vec::new();
    let mut pinned_conversations = HashMap::new();
    let mut archive_unchecked = false;
    if first {
        // Pinned threads lead the first page; Wonder drafts without a native
        // session yet remain visible until the provider reports them.
        let mut attached = state
            .store
            .project_conversations(&project.id)
            .await
            .unwrap_or_default();
        attached.retain(|c| folders.shows_saved_chat(&c.cwd));
        // Codex lists a new thread only once its first turn has started (about
        // 1 s after; measured), and a Claude session once its transcript is
        // written. Until then Wonder's own record keeps it in the list, so a
        // refresh right after creating it does not drop it.
        let now = chrono::Utc::now().timestamp();
        // Retained history from older provider homes stays visible alongside
        // native lists, but opens read-only.
        let retained_history = |c: &StoredProjectConversation| {
            !crate::providers::is_default_store(&state, c.family, &c.provider_store)
        };
        let mut accounts = attached
            .iter()
            .filter(|c| {
                (retained_history(c) || !c.family.provider().capabilities.native_sessions)
                    && c.native_session_id.is_some()
                    && !c.is_pinned
                    && !c.wonder_archived
            })
            .map(|c| NativeThread {
                family: c.family,
                store: c.provider_store.clone(),
                is_computer_session: false,
                worktree_label: folders.label(&c.cwd),
                cwd: Some(c.cwd.clone()),
                id: c.native_session_id.clone().unwrap_or_default(),
                title: display_title(Some(c), None),
                source: None,
                updated_at: activity_seconds(&c.last_activity_at),
                is_working: false,
            })
            .collect::<Vec<_>>();
        accounts.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
        cursor.accounts.buffer = accounts.into();
        // Only rows this page leads with need their archive state: pins and
        // threads too new for Codex's list. Codex's own list is unarchived.
        let led = attached
            .iter()
            .filter(|c| !c.wonder_archived)
            .filter(|c| {
                c.is_pinned
                    || (0..=JUST_STARTED_SECONDS).contains(&(now - activity_seconds(&c.created_at)))
            })
            .cloned()
            .collect::<Vec<_>>();
        let archive = archive_state::display_states(&state, &led).await;
        let shown = |c: &StoredProjectConversation| {
            c.native_session_id
                .as_ref()
                .and_then(|id| archive.get(id))
                .map(|found| archive_state::displayed(&state, c, *found))
        };
        // Unknown with no earlier answer hides a chat until it is checked,
        // except one started moments ago: Codex cannot have archived a thread
        // it does not list yet.
        let unchecked = |c: &StoredProjectConversation| {
            shown(c) == Some(ArchiveState::Unknown)
                && !(0..=JUST_STARTED_SECONDS).contains(&(now - activity_seconds(&c.created_at)))
        };
        let hidden = |c: &StoredProjectConversation| {
            shown(c) == Some(ArchiveState::Archived) || unchecked(c)
        };
        archive_unchecked = attached.iter().any(unchecked);
        just_started = attached
            .iter()
            .filter(|c| c.native_session_id.is_some() && !c.is_pinned && !c.wonder_archived)
            .filter(|c| !retained_history(c))
            .filter(|c| {
                (0..=JUST_STARTED_SECONDS).contains(&(now - activity_seconds(&c.created_at)))
            })
            .filter(|c| !hidden(c))
            .cloned()
            .collect::<Vec<_>>();
        for conversation in attached
            .iter()
            .filter(|c| (c.is_pinned || c.native_session_id.is_none()) && !c.wonder_archived)
        {
            if hidden(conversation) {
                continue;
            }
            if let Some(native) = &conversation.native_session_id {
                let key = reference(conversation.family, native);
                cursor.emitted.insert(key.clone());
                pinned_conversations.insert(key, conversation.clone());
            }
            threads.push(
                attached_summary_with_folders(
                    &state,
                    conversation,
                    activity_seconds(&conversation.last_activity_at),
                    None,
                    Some(&folders),
                )
                .await,
            );
        }
    }
    let lead = threads.len();
    let mut native_rows = 0;
    // One budget covers the native merge, including runtime locks and empty
    // filtered pages. Provider offsets survive a partial page for retry.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut owners = HashMap::new();
    let mut deleted = HashSet::new();
    let mut checked = HashSet::new();
    let mut kept = None;
    while native_rows < limit {
        tokio::join!(
            fill_codex(&state, &folders, &mut cursor.codex, limit, deadline),
            fill_claude(&state, &folders, &mut cursor.claude, limit, deadline),
        );
        if (cursor.codex.deferred && cursor.codex.buffer.is_empty())
            || (cursor.claude.deferred && cursor.claude.buffer.is_empty())
        {
            break;
        }
        let keys = [&cursor.codex, &cursor.claude, &cursor.accounts]
            .iter()
            .flat_map(|page| page.buffer.iter())
            .map(|row| (row.family, row.store.clone(), row.id.clone()))
            .filter(|key| checked.insert(key.clone()))
            .collect::<Vec<_>>();
        if !keys.is_empty() {
            // Owners and deleted chats of the whole page, one query each; a
            // row is never checked on its own.
            deleted.extend(
                state
                    .store
                    .project_sessions_deleted(&keys)
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?,
            );
            let found = state
                .store
                .project_conversation_owners(&keys)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
            for owner in found {
                owners.insert(
                    (owner.family, owner.provider_store, owner.native_session_id),
                    (
                        owner.conversation,
                        if owner.is_current {
                            Owner::Current
                        } else {
                            Owner::Left
                        },
                    ),
                );
            }
        }
        // The most recent front; on a tie the earlier list (Codex) goes first.
        let newest = [&cursor.codex, &cursor.claude, &cursor.accounts]
            .iter()
            .enumerate()
            .filter_map(|(index, page)| page.buffer.front().map(|t| (index, t.updated_at)))
            .fold(None, |best: Option<(usize, i64)>, (index, at)| match best {
                Some((_, newest)) if at <= newest => best,
                _ => Some((index, at)),
            });
        let next = match newest {
            Some((0, _)) => cursor.codex.buffer.pop_front(),
            Some((1, _)) => cursor.claude.buffer.pop_front(),
            Some(_) => cursor.accounts.buffer.pop_front(),
            None => break,
        };
        let Some(native) = next else { break };
        if deleted.contains(&(native.family, native.store.clone(), native.id.clone())) {
            continue;
        }
        let key = reference(native.family, &native.id);
        if !cursor.emitted.insert(key.clone()) {
            if let Some(pinned) = threads.iter_mut().find(|thread| thread.reference == key) {
                // Pinned rows lead the page before the provider is read.
                if let Some(conversation) = pinned_conversations.get(&key) {
                    pinned.title =
                        settled_title(&state, conversation, native.source.as_deref()).await;
                }
                // Use on the Mac's own app is newer than Wonder's saved activity.
                pinned.updated_at = pinned.updated_at.max(native.updated_at);
                pinned.is_working |= native.is_working;
                pinned.is_computer_session |= native.is_computer_session;
            }
            continue;
        }
        // Settled, snoozed or archived in its home app: listed under Archived.
        let owner = match owners.get(&(native.family, native.store.clone(), native.id.clone())) {
            Some((conversation, Owner::Current)) => Some(conversation),
            _ => None,
        };
        // A T3 Code subagent belongs to its parent's agent tasks, not to
        // Chats; one whose parent T3 Code no longer has is listed, marked.
        let orphan = match t3_subagent(&state, &native).await {
            Some(true) if !owner.is_some_and(|owner| owner.is_pinned) => continue,
            Some(known) => !known,
            None => false,
        };
        if kept.is_none() {
            kept = Some(crate::settled_elsewhere::kept(&state).await);
        }
        if crate::settled_elsewhere::hidden(
            &state,
            native.family,
            &native.store,
            &native.id,
            owner,
            native.is_working,
            kept.as_ref().unwrap_or(&HashMap::new()),
        )
        .await
        .is_some()
        {
            continue;
        }
        let summary = match owners.remove(&(native.family, native.store.clone(), native.id.clone()))
        {
            // A session the thread switched away from is part of that thread,
            // which is listed by the session it uses now.
            Some((_, Owner::Left)) => continue,
            // Archived in Wonder: the provider still lists it, Wonder does not.
            Some((conversation, _)) if conversation.wonder_archived => continue,
            Some((conversation, Owner::Current)) => {
                attached_summary_with_folders(
                    &state,
                    &conversation,
                    native.updated_at,
                    native.source.as_deref(),
                    Some(&folders),
                )
                .await
            }
            _ => ProjectThreadSummary {
                reference: key,
                conversation_id: None,
                title: crate::t3_code::titles::listed_title(
                    &state,
                    native.family,
                    &native.id,
                    native.title,
                )
                .await,
                family: native.family,
                updated_at: native.updated_at,
                is_pinned: false,
                has_unread: false,
                is_working: false,
                current_activity: None,
                needs_input_on_mac: false,
                waiting_on_mac_permission: false,
                is_computer_session: native.is_computer_session,
                worktree_label: native.worktree_label.clone(),
                branch: None,
                background_work: false,
                is_subagent: false,
                last_automation_at: None,
                automation_run: None,
                resumes_at: None,
                cwd: native.cwd.clone(),
            },
        };
        let mut summary = summary;
        summary.is_subagent = orphan;
        summary.worktree_label = native.worktree_label.or(summary.worktree_label);
        // Native activity is affirmative evidence even when the last saved
        // timestamp is old. Foreign App Server idle is not idle authority.
        summary.is_working |= native.is_working;
        summary.is_computer_session |= native.is_computer_session;
        threads.push(summary);
        native_rows += 1;
    }
    let mut missing = Vec::new();
    for conversation in &just_started {
        let native = conversation
            .native_session_id
            .as_deref()
            .unwrap_or_default();
        if cursor
            .emitted
            .insert(reference(conversation.family, native))
        {
            let summary = attached_summary_with_folders(
                &state,
                conversation,
                activity_seconds(&conversation.last_activity_at),
                None,
                Some(&folders),
            )
            .await;
            missing.push(summary);
        }
    }
    threads.splice(lead..lead, missing);
    mark_computer_sessions(&state, threads.iter_mut().filter(|row| row.is_pinned)).await;
    // Rows never depend on the activity refresh; the Projects library reports
    // an incomplete one. A slow refresh must not read as missing chats.
    let (claude_desktop, _, claude_threads) =
        claude_activity(&state, claude_session_ids(threads.iter())).await;
    for thread in &mut threads {
        mark_desktop_working(&state, thread, &claude_desktop).await;
    }
    decorate(
        &state,
        threads.iter_mut().collect(),
        &claude_desktop,
        claude_threads.as_ref(),
    )
    .await;
    // A page still reading continues through its cursor, not a failure.
    let mut partial = Vec::new();
    if cursor.codex.failed {
        partial.push(PartialFailure {
            family: AgentFamily::Codex,
            detail: "Codex threads could not be loaded.".into(),
        });
    } else if archive_unchecked {
        partial.push(PartialFailure {
            family: AgentFamily::Codex,
            detail: "Some Codex chats couldn’t be checked and are hidden for now.".into(),
        });
    }
    let loading = (cursor.codex.deferred && !cursor.codex.failed)
        || (cursor.claude.deferred && !cursor.claude.failed);
    if cursor.claude.failed {
        partial.push(PartialFailure {
            family: AgentFamily::Claude,
            detail: "Claude Code threads could not be loaded.".into(),
        });
    }
    let more = |page: &ProviderPage| !page.buffer.is_empty() || !page.done;
    let next_cursor = if more(&cursor.codex) || more(&cursor.claude) || more(&cursor.accounts) {
        let token = uuid::Uuid::new_v4().to_string();
        let mut cursors = state
            .projects
            .cursors
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cursors.retain(|_, (created, _)| created.elapsed() < CURSOR_TTL);
        while cursors.len() >= MAX_CURSORS {
            let Some(oldest) = cursors
                .iter()
                .min_by_key(|(_, (created, _))| *created)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            cursors.remove(&oldest);
        }
        cursors.insert(token.clone(), (Instant::now(), cursor));
        Some(token)
    } else {
        None
    };
    Ok(ThreadsPage {
        threads,
        next_cursor,
        partial,
        loading,
    })
}

/// Most threads `GET /api/v1/threads/recent` returns.
const MAX_RECENT_THREADS: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecentThreadsQuery {
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecentThread {
    project_id: String,
    project_name: String,
    thread: ProjectThreadSummary,
}

/// The most recently active threads across every included project, newest
/// first, each with its project. One first page per project, merged here so
/// the phone (voice mode) gets an answer that does not depend on which
/// projects it happened to load.
pub(crate) async fn recent_threads(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(query): Query<RecentThreadsQuery>,
) -> Response {
    use futures_util::{stream, StreamExt};
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_RECENT_THREADS);
    let Ok(projects) = state.store.list_projects().await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Projects are temporarily unavailable. Try again.",
        );
    };
    let pages = stream::iter(
        projects
            .into_iter()
            .filter(|p| p.is_included)
            .map(|project| {
                let state = state.clone();
                async move {
                    // Every Project gets the whole global limit: one Project
                    // may hold all of the newest threads.
                    let query = ThreadsQuery {
                        cursor: None,
                        limit: Some(limit),
                    };
                    let page = thread_page(&state, &project.id, query, MAX_RECENT_THREADS)
                        .await
                        .ok();
                    (project, page)
                }
            }),
    )
    .buffer_unordered(6)
    .collect::<Vec<_>>()
    .await;
    let mut threads = Vec::new();
    let mut partial: Vec<PartialFailure> = Vec::new();
    for (project, page) in pages {
        let Some(page) = page else {
            continue;
        };
        for failure in page.partial {
            if !partial.iter().any(|known| known.family == failure.family) {
                partial.push(failure);
            }
        }
        threads.extend(page.threads.into_iter().map(|thread| RecentThread {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            thread,
        }));
    }
    // Pinned rows lead each project's page; here only activity orders them.
    threads.sort_by(|a, b| {
        b.thread
            .updated_at
            .cmp(&a.thread.updated_at)
            .then_with(|| a.project_id.cmp(&b.project_id))
            .then_with(|| a.thread.reference.cmp(&b.thread.reference))
    });
    // Overlapping Project folders list one thread twice; name it once.
    let mut seen = HashSet::new();
    threads.retain(|row| seen.insert(row.thread.reference.clone()));
    threads.truncate(limit);
    Json(json!({"threads": threads, "partial": partial})).into_response()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivedThreadsPage {
    threads: Vec<ArchivedThread>,
    partial: Vec<PartialFailure>,
}

/// An archived thread, and where it was closed when that was its home app
/// rather than Cupola or Codex (`settled_elsewhere`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchivedThread {
    #[serde(flatten)]
    thread: ProjectThreadSummary,
    /// "Settled in T3 Code", "Archived in the Claude app" and the like.
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_in: Option<String>,
    /// When a snoozed thread returns on its own (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    returns_at: Option<String>,
    /// Restore reopens it in its home app too, when Cupola is connected there.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    reopens_there: bool,
}

/// Most native threads one Archived list reads for threads closed in their
/// home app, and how long it may take.
const MAX_ELSEWHERE_SCAN: usize = 3000;
// Test builds share a loaded machine with the whole suite: a scan that is slow
// there is not the timeout this bound exists to test.
const ELSEWHERE_SCAN_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 60 } else { 6 });

/// Whether a native session is a T3 Code subagent: `Some(true)` while T3 Code
/// still has its parent thread, `Some(false)` once it does not.
async fn t3_subagent(state: &AppState, native: &NativeThread) -> Option<bool> {
    if !crate::providers::is_default_store(state, native.family, &native.store) {
        return None;
    }
    state
        .t3_code
        .subagent_of(native.family, &native.id)
        .await
        .map(|subagent| subagent.parent_known)
}

/// The project's native threads that their home app closed and Chats leaves
/// out (`settled_elsewhere::hidden`), with their owning Cupola thread, and
/// the providers whose list could not be read to the end.
async fn closed_elsewhere(
    state: &AppState,
    folders: &ProjectFolders,
) -> (
    Vec<(
        NativeThread,
        Option<StoredProjectConversation>,
        crate::settled_elsewhere::Elsewhere,
    )>,
    Vec<AgentFamily>,
) {
    let deadline = tokio::time::Instant::now() + ELSEWHERE_SCAN_TIMEOUT;
    let (mut codex, mut claude) = (ProviderPage::default(), ProviderPage::default());
    let mut rows = Vec::new();
    while rows.len() < MAX_ELSEWHERE_SCAN && tokio::time::Instant::now() < deadline {
        tokio::join!(
            fill_codex(state, folders, &mut codex, 100, deadline),
            fill_claude(state, folders, &mut claude, 100, deadline),
        );
        // A sidecar page still reading continues within the deadline.
        claude.deferred = false;
        let before = rows.len();
        rows.extend(codex.buffer.drain(..));
        rows.extend(claude.buffer.drain(..));
        let stuck = rows.len() == before && claude.cursor.is_some() && !claude.done;
        if (codex.done || codex.failed) && (claude.done || claude.failed) || stuck {
            break;
        }
    }
    let mut unread = Vec::new();
    if !codex.done {
        unread.push(AgentFamily::Codex);
    }
    if !claude.done {
        unread.push(AgentFamily::Claude);
    }
    let keys = rows
        .iter()
        .map(|row| (row.family, row.store.clone(), row.id.clone()))
        .collect::<Vec<_>>();
    let mut owners = HashMap::new();
    for owner in state
        .store
        .project_conversation_owners(&keys)
        .await
        .unwrap_or_default()
    {
        owners.insert(
            (owner.family, owner.provider_store, owner.native_session_id),
            (owner.conversation, owner.is_current),
        );
    }
    let kept = crate::settled_elsewhere::kept(state).await;
    let mut closed = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        if state
            .store
            .project_session_deleted(row.family, &row.store, &row.id)
            .await
            .unwrap_or(true)
        {
            continue;
        }
        if !seen.insert((row.family, row.id.clone())) {
            continue;
        }
        // A T3 Code subagent is never filed as a chat of its own, closed or not.
        if t3_subagent(state, &row).await == Some(true) {
            continue;
        }
        let owner = match owners.remove(&(row.family, row.store.clone(), row.id.clone())) {
            // A session the thread switched away from is part of that thread.
            Some((_, false)) => continue,
            Some((conversation, true)) => Some(conversation),
            None => None,
        };
        if let Some(elsewhere) = crate::settled_elsewhere::hidden(
            state,
            row.family,
            &row.store,
            &row.id,
            owner.as_ref(),
            row.is_working,
            &kept,
        )
        .await
        {
            closed.push((row, owner, elsewhere));
        }
    }
    (closed, unread)
}

/// Most archived Codex threads Wonder reads for one project.
const MAX_ARCHIVED_CODEX: usize = 200;

/// A project's archived threads, newest first: threads archived in Cupola and
/// Codex threads archived in the Codex app.
/// Restoring one is the same conversation PATCH as archiving it.
pub(crate) async fn archived_threads(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
) -> Response {
    let Ok(Some(project)) = state.store.project(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let folders = project_folders(&state, &project).await;
    let project = &folders.project;
    let Ok(mut attached) = state.store.project_conversations(&project.id).await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Archived threads are temporarily unavailable. Try again.",
        );
    };
    attached.retain(|c| folders.shows_saved_chat(&c.cwd));
    let mut threads = Vec::new();
    for conversation in attached.iter().filter(|c| c.wonder_archived) {
        threads.push(
            attached_summary(
                &state,
                conversation,
                activity_seconds(&conversation.last_activity_at),
                None,
            )
            .await,
        );
    }
    let mut partial = Vec::new();
    let mut listed = HashSet::new();
    let mut elsewhere = Vec::new();
    let (closed, unread) = closed_elsewhere(&state, &folders).await;
    for (native, owner, closing) in closed {
        // Already listed as archived in Cupola.
        if owner.as_ref().is_some_and(|owner| owner.wonder_archived) {
            continue;
        }
        let thread = match &owner {
            Some(conversation) => {
                attached_summary(
                    &state,
                    conversation,
                    native.updated_at,
                    native.source.as_deref(),
                )
                .await
            }
            None => ProjectThreadSummary {
                reference: reference(native.family, &native.id),
                conversation_id: None,
                title: crate::t3_code::titles::listed_title(
                    &state,
                    native.family,
                    &native.id,
                    native.title.clone(),
                )
                .await,
                family: native.family,
                updated_at: native.updated_at,
                is_pinned: false,
                has_unread: false,
                is_working: false,
                current_activity: None,
                needs_input_on_mac: false,
                waiting_on_mac_permission: false,
                is_computer_session: native.is_computer_session,
                worktree_label: native.worktree_label.clone(),
                branch: None,
                background_work: false,
                is_subagent: false,
                last_automation_at: None,
                automation_run: None,
                resumes_at: None,
                cwd: native.cwd.clone(),
            },
        };
        listed.insert(thread.reference.clone());
        elsewhere.push(ArchivedThread {
            thread,
            closed_in: Some(closing.label()),
            returns_at: closing.until(),
            reopens_there: closing.reopens_there(),
        });
    }
    for family in unread {
        partial.push(PartialFailure {
            family,
            detail: "Some threads closed in T3 Code or the Claude app could not be listed.".into(),
        });
    }
    match archived_codex_rows(&state, project).await {
        Ok(rows) => {
            let store = provider_store(&state, AgentFamily::Codex).to_owned();
            for native in rows {
                match state
                    .store
                    .project_session_deleted(native.family, &store, &native.id)
                    .await
                {
                    Ok(false) => {}
                    Ok(true) => continue,
                    Err(_) => {
                        return error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Deleted chats could not be checked. Try again.",
                        )
                    }
                }
                let summary =
                    match owning_conversation(&state, AgentFamily::Codex, &store, &native.id).await
                    {
                        // Archiving a session the thread switched away from archives
                        // only its history; the thread itself is not archived, and
                        // that session's first message never names it.
                        Ok(Some((_, Owner::Left))) => continue,
                        // Already listed above as archived in Cupola; Codex still has
                        // its first message.
                        Ok(Some((conversation, Owner::Current)))
                            if conversation.wonder_archived =>
                        {
                            if let Some(row) = threads.iter_mut().find(|row| {
                                row.conversation_id.as_deref()
                                    == Some(conversation.conversation_id.as_str())
                            }) {
                                row.title =
                                    settled_title(&state, &conversation, native.source.as_deref())
                                        .await;
                            }
                            continue;
                        }
                        Ok(Some((conversation, Owner::Current))) => {
                            attached_summary(
                                &state,
                                &conversation,
                                native.updated_at,
                                native.source.as_deref(),
                            )
                            .await
                        }
                        _ => ProjectThreadSummary {
                            reference: reference(AgentFamily::Codex, &native.id),
                            conversation_id: None,
                            title: native.title,
                            family: AgentFamily::Codex,
                            updated_at: native.updated_at,
                            is_pinned: false,
                            has_unread: false,
                            is_working: false,
                            current_activity: None,
                            needs_input_on_mac: false,
                            waiting_on_mac_permission: false,
                            is_computer_session: native.is_computer_session,
                            worktree_label: native.worktree_label.clone(),
                            branch: None,
                            background_work: false,
                            is_subagent: false,
                            last_automation_at: None,
                            automation_run: None,
                            resumes_at: None,
                            cwd: native.cwd.clone(),
                        },
                    };
                let mut summary = summary;
                summary.is_computer_session |= native.is_computer_session;
                threads.push(summary);
            }
        }
        Err(_) => partial.push(PartialFailure {
            family: AgentFamily::Codex,
            detail: "Archived Codex threads could not be loaded.".into(),
        }),
    }
    let mut threads = threads
        .into_iter()
        .filter(|thread| !listed.contains(&thread.reference))
        .map(|thread| ArchivedThread {
            thread,
            closed_in: None,
            returns_at: None,
            reopens_there: false,
        })
        .chain(elsewhere)
        .collect::<Vec<_>>();
    threads.sort_by_key(|row| std::cmp::Reverse(row.thread.updated_at));
    // Archived threads are not running anything worth a desktop scan.
    decorate(
        &state,
        threads.iter_mut().map(|row| &mut row.thread).collect(),
        &Default::default(),
        None,
    )
    .await;
    Json(ArchivedThreadsPage { threads, partial }).into_response()
}

/// Codex's own archived threads in the project's folders, helpers excluded.
async fn archived_codex_rows(
    state: &AppState,
    project: &StoredProject,
) -> Result<Vec<NativeThread>, String> {
    let folders = project_folders(state, project).await;
    let project = &folders.project;
    let rpc = codex_rpc(state).await?;
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let page = result(
            &rpc,
            "thread/list",
            json!({
                "cwd": root_paths(project), "sourceKinds": CODEX_SOURCE_KINDS, "archived": true,
                "limit": 100, "sortKey": "updated_at", "sortDirection": "desc", "cursor": cursor,
            }),
        )
        .await?;
        for thread in page["data"]
            .as_array()
            .ok_or("Codex archive state is unavailable.")?
        {
            let Some(id) = thread.get("id").and_then(Value::as_str) else {
                continue;
            };
            if thread
                .get("parentThreadId")
                .and_then(Value::as_str)
                .is_some()
                || codex_one_shot(thread)
            {
                continue;
            }
            rows.push(NativeThread {
                family: AgentFamily::Codex,
                store: state.projects.codex_store.clone(),
                is_computer_session: codex_computer_session(thread),
                id: id.to_owned(),
                worktree_label: thread
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|cwd| folders.label(cwd)),
                cwd: thread.get("cwd").and_then(Value::as_str).map(str::to_owned),
                title: thread_title(thread),
                source: native_title_source(thread).map(str::to_owned),
                is_working: false,
                updated_at: thread
                    .get("updatedAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
            });
            if rows.len() >= MAX_ARCHIVED_CODEX {
                return Ok(rows);
            }
        }
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match &cursor {
            Some(next) if seen.insert(next.clone()) => {}
            _ => return Ok(rows),
        }
    }
}

// ---------------------------------------------------------------------------
// Attaching an existing native thread
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AttachRequest {
    reference: String,
}

const MAX_ARCHIVE_BATCH: usize = 50;
const ARCHIVE_BATCH_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArchiveBatchItem {
    reference: String,
    conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArchiveBatchRequest {
    threads: Vec<ArchiveBatchItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveBatchResult {
    reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Archive in request order, sharing the single-item safety checks. A failed
/// item never prevents a later item from being attempted within the deadline.
pub(crate) async fn archive_threads(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<ArchiveBatchRequest>,
) -> Response {
    let mut seen = HashSet::new();
    if request.threads.is_empty()
        || request.threads.len() > MAX_ARCHIVE_BATCH
        || request.threads.iter().any(|item| {
            item.reference.is_empty()
                || item.reference.len() > 200
                || !seen.insert(item.reference.as_str())
        })
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose between 1 and 50 different threads to archive.",
        );
    }
    let Ok(Some(project)) = state.store.project(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let deadline = tokio::time::Instant::now() + ARCHIVE_BATCH_TIMEOUT;
    let results = archive_batch(&state, &project, request.threads, deadline).await;
    if results.iter().any(|result| result.error.is_none()) {
        announce_chats_changed(&state).await;
    }
    Json(json!({"results": results})).into_response()
}

async fn archive_batch(
    state: &AppState,
    project: &StoredProject,
    items: Vec<ArchiveBatchItem>,
    deadline: tokio::time::Instant,
) -> Vec<ArchiveBatchResult> {
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let outcome = if tokio::time::Instant::now() >= deadline {
            None
        } else {
            tokio::time::timeout_at(deadline, archive_batch_item(state, project, &item))
                .await
                .ok()
        };
        let (conversation_id, failure) = match outcome {
            Some(Ok(id)) => (Some(id), None),
            Some(Err((_, message))) => (None, Some(message)),
            None => (
                None,
                Some("The archive could not be confirmed in time. Try again.".into()),
            ),
        };
        results.push(ArchiveBatchResult {
            reference: item.reference,
            conversation_id,
            error: failure,
        });
    }
    results
}

async fn archive_batch_item(
    state: &AppState,
    project: &StoredProject,
    item: &ArchiveBatchItem,
) -> Result<String, (StatusCode, String)> {
    let conversation = archive_target(state, project, item).await?;
    set_archived(state, &conversation, true).await?;
    Ok(conversation.conversation_id)
}

/// The saved conversation an archive request names, attaching a native
/// thread first.
async fn archive_target(
    state: &AppState,
    project: &StoredProject,
    item: &ArchiveBatchItem,
) -> Result<StoredProjectConversation, (StatusCode, String)> {
    let conversation = if let Some(id) = &item.conversation_id {
        let existing = state
            .store
            .project_conversation(id)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The thread could not be checked. Try again.".into(),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    "This thread is no longer available.".into(),
                )
            })?;
        let expected = existing
            .native_session_id
            .as_deref()
            .map(|native| reference(existing.family, native))
            .unwrap_or_else(|| format!("wonder:{}", existing.conversation_id));
        if item.reference != expected {
            return Err((
                StatusCode::CONFLICT,
                "This thread's identity changed. Reopen it before archiving.".into(),
            ));
        }
        let folders = project_folders(state, project).await;
        // Saved conversations stay in their own Project after a folder is
        // removed. Overlapping Projects may share a thread only by folder.
        if existing.project_id != project.id && !folders.contains(&existing.cwd) {
            return Err((
                StatusCode::FORBIDDEN,
                "This thread is outside the project's folders.".into(),
            ));
        }
        existing
    } else {
        let (family, native) = native_reference(&item.reference)
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "Invalid thread reference.".into()))?;
        attach_native(state, project, family, &native).await?
    };
    Ok(conversation)
}

fn native_reference(value: &str) -> Option<(AgentFamily, String)> {
    value.split_once(':').and_then(|(family, native)| {
        let family = match family {
            "codex" => AgentFamily::Codex,
            "claude" => AgentFamily::Claude,
            _ => return None,
        };
        (!native.is_empty()
            && native.len() <= 128
            && native.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .then(|| (family, native.to_owned()))
    })
}

/// `cupola delegate`: the owner starts a task as a child of a thread. The
/// body is `wonder_delegate`'s arguments.
pub(crate) async fn delegate(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(args): Json<Value>,
) -> Response {
    match crate::thread_tools::owner_delegate(&state, &id, args).await {
        Ok(result) => Json(result).into_response(),
        Err((status, message)) => error(status, message),
    }
}

pub(crate) async fn attach(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<AttachRequest>,
) -> Response {
    let started = Instant::now();
    let outcome = attach_thread(&state, &id, &request.reference).await;
    let (status, conversation, message) = match &outcome {
        Ok(summary) => (StatusCode::OK, summary.conversation_id.as_deref(), None),
        Err((status, message)) => (*status, None, Some(message.as_str())),
    };
    let _ = state.logger.record(
        if status.is_success() { "info" } else { "warn" },
        "project_thread_attach",
        json!({"projectId":id, "conversationId":conversation,
            "readOnly":conversation.is_some_and(is_outside_history),
            "reference":native_reference(&request.reference).map(|(family, native)| reference(family, &native)),
            "status":status.as_u16(), "elapsedMs":started.elapsed().as_millis(), "error":message}),
    );
    match outcome {
        Ok(summary) => Json(summary).into_response(),
        Err((status, message)) => error(status, message),
    }
}

async fn attach_thread(
    state: &AppState,
    project_id: &str,
    reference: &str,
) -> Result<ProjectThreadSummary, (StatusCode, String)> {
    let project = state
        .store
        .project(project_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "This chat's project could not be checked. Try again.".into(),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "This chat's project is no longer available.".into(),
            )
        })?;
    let (family, native) = native_reference(reference)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Invalid thread reference.".into()))?;
    let conversation = match attach_native(state, &project, family, &native).await {
        Ok(conversation) => conversation,
        Err((StatusCode::FORBIDDEN, _)) if family == AgentFamily::Codex => {
            // Opening history is distinct from admitting a thread. Keep the
            // membership guard in attach_native for all mutating callers.
            let id = outside_history_id(project_id, reference);
            let (detail, _, _) = outside_history_detail(state, &id).await?;
            return Ok(ProjectThreadSummary {
                reference: reference.to_owned(),
                conversation_id: Some(id),
                title: detail.title,
                family,
                updated_at: 0,
                is_pinned: false,
                has_unread: false,
                is_working: false,
                is_computer_session: false,
                worktree_label: None,
                current_activity: None,
                needs_input_on_mac: false,
                waiting_on_mac_permission: false,
                branch: None,
                background_work: false,
                is_subagent: false,
                last_automation_at: None,
                automation_run: None,
                resumes_at: None,
                cwd: Some(detail.working_folder),
            });
        }
        Err(error) => return Err(error),
    };
    // A session the thread left needs no desktop project: it is only history.
    if family == AgentFamily::Codex
        && conversation.native_session_id.as_deref() == Some(native.as_str())
    {
        associate_codex_project(state, &project, &native)
            .await
            .map_err(|message| (StatusCode::SERVICE_UNAVAILABLE, message))?;
    }
    let mut summary = single_summary(
        state,
        &conversation,
        activity_seconds(&conversation.last_activity_at),
        None,
    )
    .await;
    // The receipt replaces the native row on the phone. Preserve its badge.
    mark_computer_sessions(state, std::iter::once(&mut summary)).await;
    // The receipt admits the thread, not its history. Even refresh setup
    // belongs off this path; the existing refresh owner deduplicates reads,
    // persists status and streams saved items without starting model work.
    let state = state.clone();
    tokio::spawn(async move {
        let response = crate::refresh_history(
            State(state.clone()),
            Path(conversation.conversation_id.clone()),
        )
        .await;
        if !response.status().is_success() {
            let _ = state.logger.record("warn", "project_thread_history_start_failed",
                json!({"conversationId":conversation.conversation_id,"status":response.status().as_u16()}));
        }
    });
    Ok(summary)
}

const OUTSIDE_HISTORY_PREFIX: &str = "history-";
const OUTSIDE_HISTORY_REASON: &str =
    "This chat ran outside the Project's folders, so it can't be continued here. Open it on your computer to continue.";

/// A Project-labelled history address, never a stored conversation or folder
/// grant. Per the owner's every-conversation-accessible decision, paired owner
/// devices can read native history across folders; Project membership limits
/// writes, not these reads. Mutating routes accept stored conversations only.
fn outside_history_id(project: &str, reference: &str) -> String {
    use base64::Engine as _;
    format!(
        "{OUTSIDE_HISTORY_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&(project, reference)).unwrap())
    )
}

pub(crate) fn is_outside_history(id: &str) -> bool {
    id.starts_with(OUTSIDE_HISTORY_PREFIX)
}

async fn outside_history_detail(
    state: &AppState,
    id: &str,
) -> Result<(ProjectConversationDetail, RpcClient, String), (StatusCode, String)> {
    let invalid = || {
        (
            StatusCode::BAD_REQUEST,
            "This chat's history address is invalid. Open it again from Chats.".to_owned(),
        )
    };
    let encoded = id
        .strip_prefix(OUTSIDE_HISTORY_PREFIX)
        .filter(|value| value.len() <= 2048)
        .ok_or_else(invalid)?;
    use base64::Engine as _;
    let (project_id, reference): (String, String) = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid())?,
    )
    .map_err(|_| invalid())?;
    let (family, native) = native_reference(&reference)
        // This route parses and pages Codex's native thread/read and
        // thread/items/list formats; it is not a provider capability switch.
        .filter(|(family, _)| *family == AgentFamily::Codex)
        .ok_or_else(invalid)?;
    require_not_deleted(state, family, provider_store(state, family), &native).await?;
    let project = state
        .store
        .project(&project_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "This chat's Project could not be checked. Try again.".into(),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "This chat's Project is no longer available. Open the chat on your computer."
                    .into(),
            )
        })?;
    let rpc = codex_rpc(state)
        .await
        .map_err(|message| (StatusCode::SERVICE_UNAVAILABLE, message))?;
    let read = result(
        &rpc,
        "thread/read",
        json!({"threadId":native,"includeTurns":false}),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            "This Codex thread is no longer available on your computer.".to_owned(),
        )
    })?;
    let thread = &read["thread"];
    if thread["id"].as_str() != Some(native.as_str()) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "This chat could not be checked. Open it again from Chats.".into(),
        ));
    }
    if thread["parentThreadId"].as_str().is_some() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Open this helper thread from its parent.".into(),
        ));
    }
    let cwd = thread["cwd"]
        .as_str()
        .filter(|cwd| !cwd.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "This chat's folder could not be read. Try again.".to_owned(),
            )
        })?;
    let detail = ProjectConversationDetail {
        t3_code_permissions: state.t3_code.permissions(family, &native).await,
        conversation_id: id.to_owned(),
        project_id: project.id,
        project_name: project.name,
        title: thread_title(thread),
        family,
        model: None,
        effort: None,
        service_tier: None,
        access_mode: "read_only".into(),
        claude_approval: "ask".into(),
        plan_mode: false,
        working_folder: cwd.to_owned(),
        working_folder_name: folder_name(cwd),
        worktree_label: None,
        is_pinned: false,
        has_unread: false,
        has_native_session: true,
        is_archived: false,
        folder_in_project: false,
        read_only_reason: Some(OUTSIDE_HISTORY_REASON.into()),
        notice: None,
        resumes_at: None,
    };
    Ok((detail, rpc, native))
}

/// Page native history directly: no resume, project association, import, or
/// conversation creation. Scope opaque provider cursors to this exact address.
pub(crate) async fn outside_history_snapshot(
    state: &AppState,
    id: &str,
    before: Option<&str>,
    limit: Option<u32>,
    compact: bool,
) -> Response {
    let read = async {
        let (detail, rpc, native) = outside_history_detail(state, id).await?;
        let bad_cursor = || {
            (
                StatusCode::BAD_REQUEST,
                "Open this chat again to read its earlier messages.".to_owned(),
            )
        };
        let cursor = match before {
            None => None,
            Some(value) => {
                if value.len() > 16384 {
                    return Err(bad_cursor());
                }
                let (scope, cursor): (String, String) =
                    serde_json::from_slice(&hex::decode(value).map_err(|_| bad_cursor())?)
                        .map_err(|_| bad_cursor())?;
                if scope != id {
                    return Err(bad_cursor());
                }
                Some(cursor)
            }
        };
        let limit = limit.unwrap_or(100);
        if limit == 0 || limit > 100 {
            return Err((
                StatusCode::BAD_REQUEST,
                "History page size must be between 1 and 100.".into(),
            ));
        }
        let page = result(
            &rpc,
            "thread/items/list",
            json!({"threadId":native,"limit":limit,"sortDirection":"desc","cursor":cursor}),
        )
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "This chat's history could not be read. Try again.".to_owned(),
            )
        })?;
        let invalid_page = || {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "This chat's history could not be read. Try again.".to_owned(),
            )
        };
        let entries = page["data"]
            .as_array()
            .filter(|entries| entries.len() <= limit as usize)
            .ok_or_else(invalid_page)?;
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries.iter().rev() {
            let turn = entry["turnId"].as_str().ok_or_else(invalid_page)?;
            let item = entry
                .get("item")
                .filter(|item| item.is_object())
                .ok_or_else(invalid_page)?;
            // Reuse the normal media/sanitization boundary. Without preview
            // storage, media says unavailable rather than inventing a file.
            let item = crate::tool_media::normalize(state, id, turn, item)
                .await
                .map_err(|_| invalid_page())?;
            items.push(history::app_server_entry_item(turn, entry, &item));
        }
        let mut thread = history::conversation_thread_projection_with_items(
            Some(native.clone()),
            &[],
            &[],
            &[],
            &items,
            Some(&detail.working_folder),
        );
        thread.hydrated = true;
        // A history address is not a stored chat, so it has no override.
        if !state
            .store
            .agent_family_settings(detail.family)
            .await
            .is_ok_and(|settings| settings.chat_detail == wonder_store::ChatDetail::Thinking)
        {
            history::hide_thinking(&mut thread);
        }
        if compact {
            history::compact_thread(&mut thread);
        }
        thread.next_cursor = match page.get("nextCursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) if cursor.len() <= 4096 => {
                Some(hex::encode(serde_json::to_vec(&(id, cursor)).unwrap()))
            }
            _ => return Err(invalid_page()),
        };
        let sequence = state
            .store
            .committed_sequence(&state.host_epoch)
            .await
            .map_err(|_| invalid_page())?;
        Ok(history::ConversationSnapshot {
            conversation_id: id.to_owned(),
            host_epoch: state.host_epoch.clone(),
            last_sequence: sequence,
            history_revision: 0,
            codex_thread_id: Some(native),
            messages: vec![],
            assistant_messages: vec![],
            thread,
            events: vec![],
            message_records: vec![],
        })
    }
    .await;
    match read {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err((status, message)) => error(status, message),
    }
}

async fn attach_native(
    state: &AppState,
    project: &StoredProject,
    family: AgentFamily,
    native: &str,
) -> Result<StoredProjectConversation, (StatusCode, String)> {
    attach_native_into(state, project, family, native, None).await
}

/// A chat with no Project works in a folder of T3 Code's own (`scratch`),
/// outside the Project's one folder. This view of the Project adds that
/// thread's folder, so its session can be opened; any other folder, or one
/// outside T3 Code's scratch folder, stays outside it.
fn t3_scratch_view(
    state: &AppState,
    project: &StoredProject,
    adopt: Option<&StoredProjectConversation>,
) -> Option<StoredProject> {
    let conversation = adopt?;
    if project.id != SCRATCH_PROJECT_ID || project.root_for(&conversation.cwd).is_some() {
        return None;
    }
    let scratch = crate::canonical_path::canonicalize(state.t3_code.scratch_dir()?).ok()?;
    let folder = crate::canonical_path::canonicalize(&conversation.cwd).ok()?;
    if folder == scratch || !folder.starts_with(&scratch) {
        return None;
    }
    let mut view = project.clone();
    view.roots.push(wonder_store::StoredProjectRoot {
        id: format!(
            "t3-scratch:{}",
            hex::encode(&Sha256::digest(conversation.cwd.as_bytes())[..12])
        ),
        path: conversation.cwd.clone(),
        canonical_path: folder.to_string_lossy().into_owned(),
        ordinal: 0,
    });
    Some(view)
}

/// Attaches a native session as a conversation: a new one, or `adopt`, a
/// conversation made for it before the session existed (Start in T3 Code),
/// which keeps its ID, title and model.
async fn attach_native_into(
    state: &AppState,
    project: &StoredProject,
    family: AgentFamily,
    native: &str,
    adopt: Option<&StoredProjectConversation>,
) -> Result<StoredProjectConversation, (StatusCode, String)> {
    let configured_project = project;
    let folders = project_folders(state, project).await;
    let scratch_view = t3_scratch_view(state, &folders.project, adopt);
    let project = scratch_view.as_ref().unwrap_or(&folders.project);
    let store = provider_store(state, family).to_owned();
    require_not_deleted(state, family, &store, native).await?;
    // A session the thread switched away from opens that thread: renaming or
    // opening its row never makes a second thread of one conversation.
    if let Ok(Some((existing, _))) = owning_conversation(state, family, &store, native).await {
        // Overlapping folder groups share one conversation; a caller-supplied
        // ID alone never opens a thread outside this project's folders.
        return if project.root_for(&existing.cwd).is_some() {
            Ok(existing)
        } else {
            Err((
                StatusCode::FORBIDDEN,
                "This thread is outside the project's folders.".into(),
            ))
        };
    }
    let unavailable = |_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "This thread could not be opened. Try again.".to_owned(),
        )
    };
    // The host decides model, effort and speed once, here, and stores them.
    let defaults = agent_defaults::resolve(
        &*state.runtime_catalog.read().await,
        family,
        None,
        None,
        None,
    );
    let (cwd, title, thread_id, session) = match family {
        AgentFamily::OpenCode | AgentFamily::Cursor => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "Opening {} sessions from your computer is not available yet.",
                    family.provider().display_name
                ),
            ))
        }
        AgentFamily::Codex => {
            let rpc = codex_rpc(state)
                .await
                .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;
            let thread = result(&rpc, "thread/read", json!({"threadId": native}))
                .await
                .map_err(|_| {
                    (
                        StatusCode::NOT_FOUND,
                        "This Codex thread is no longer available on your computer.".to_owned(),
                    )
                })?;
            let thread = thread.get("thread").cloned().unwrap_or_default();
            let cwd = thread
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if thread
                .get("parentThreadId")
                .and_then(Value::as_str)
                .is_some()
            {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Open this helper thread from its parent.".into(),
                ));
            }
            (cwd, thread_title(&thread), native.to_owned(), None)
        }
        AgentFamily::Claude => {
            // The runtime answers each exact lookup within about a second and
            // returns a cursor while folders remain; follow it for a bounded total.
            let listed = tokio::time::timeout(Duration::from_secs(10), async {
                let rpc = claude_rpc(state).await?;
                let mut cursor = Value::Null;
                loop {
                    let page = result(
                        &rpc,
                        "project/sessions/list",
                        json!({"dirs": root_paths(project), "sessionId": native, "cursor": cursor}),
                    )
                    .await?;
                    match (
                        page.get("partial").and_then(Value::as_bool),
                        page.get("nextCursor").filter(|c| c.is_string()),
                    ) {
                        (Some(true), Some(next)) => cursor = next.clone(),
                        _ => return Ok(page),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| Err("Claude Code sessions could not be loaded. Try again.".into()))
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;
            if listed.get("partial").and_then(Value::as_bool) != Some(false)
                || listed.get("data").and_then(Value::as_array).is_none()
            {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Claude Code sessions could not be loaded. Try again.".into(),
                ));
            }
            let Some(session) = listed
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|s| s.get("sessionId").and_then(Value::as_str) == Some(native))
                .cloned()
            else {
                return Err((
                    StatusCode::NOT_FOUND,
                    "This Claude Code session is no longer in this project.".into(),
                ));
            };
            let cwd = session
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let model = defaults.model.clone().ok_or((
                StatusCode::SERVICE_UNAVAILABLE,
                "Claude is not available on your computer.".to_owned(),
            ))?;
            let root = project.root_for(&cwd).ok_or((
                StatusCode::FORBIDDEN,
                "This session is outside the project's folders.".to_owned(),
            ))?;
            let attached = result(
                &claude_rpc(state)
                    .await
                    .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?,
                "project/session/attach",
                json!({"sessionId": native, "model": model, "cwd": root.path,
                    "wonderProject": claude_project(&execution_project(configured_project, root), &root.path),
                    "wonderPolicy": claude_policy(state, &execution_project(configured_project, root), ClaudeModes::default(), &root.path)}),
            )
            .await
            .map_err(|e| (StatusCode::NOT_FOUND, e))?;
            let thread = attached
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let title = claude_title(&session);
            (root.path.clone(), title, thread, Some(native.to_owned()))
        }
    };
    // Exact folder membership; a sibling with a shared prefix is not a match.
    let Some(root) = project.root_for(&cwd) else {
        return Err((
            StatusCode::FORBIDDEN,
            "This thread is outside the project's folders.".into(),
        ));
    };
    let now = now_text();
    if let Some(adopted) = adopt {
        let id = &adopted.conversation_id;
        state
            .store
            .set_project_native_session(id, native, &now)
            .await
            .map_err(unavailable)?;
        state
            .store
            .bind_project_runtime(id, family, &store, &thread_id, session.as_deref(), &now)
            .await
            .map_err(unavailable)?;
        return state
            .store
            .project_conversation(id)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| unavailable(sqlx::Error::RowNotFound));
    }
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let created = state
        .store
        .create_project_conversation(ProjectConversationInsert {
            conversation_id: &conversation_id,
            project_id: &project.id,
            family,
            provider_store: &store,
            native_session_id: Some(native),
            cwd: &root.path,
            roots_revision: project.roots_revision,
            title: &title,
            model: defaults.model.as_deref(),
            effort: defaults.effort.as_deref(),
            service_tier: defaults.service_tier.as_deref(),
            access_mode: "workspace",
            claude_approval: agent_defaults::claude_approval(family),
            plan_mode: false,
            creation_request_id: None,
            now: &now,
        })
        .await
        .map_err(unavailable)?;
    let conversation = match created {
        // Its title is the provider's name or first message.
        ProjectConversationCreate::Created(mut c) => {
            state
                .store
                .mark_project_conversation_title_taken(&c.conversation_id)
                .await
                .map_err(unavailable)?;
            c.title_chosen = false;
            c
        }
        ProjectConversationCreate::Existing(c) => c,
    };
    if conversation.conversation_id == conversation_id {
        state
            .store
            .bind_project_runtime(
                &conversation_id,
                family,
                &store,
                &thread_id,
                session.as_deref(),
                &now,
            )
            .await
            .map_err(unavailable)?;
    }
    Ok(conversation)
}

// ---------------------------------------------------------------------------
// Fork
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ForkRequest {
    /// The reply to fork from; its turn is the last one kept. Omitted: the
    /// whole conversation.
    last_turn_id: Option<String>,
}

fn fork_title(title: &str) -> String {
    let base: String = title.chars().take(300).collect();
    format!("{} (fork)", base.trim_end())
}

/// Copies a started thread, through `last_turn`, into a new native session and
/// returns its identity: the native session, the runtime thread and the Claude
/// session. The source is only read.
async fn fork_native(
    state: &AppState,
    source: &StoredProjectConversation,
    project: &StoredProject,
    native: &str,
    last_turn: Option<&str>,
    title: &str,
) -> Result<(String, String, Option<String>), (StatusCode, String)> {
    let unavailable = |message: String| (StatusCode::SERVICE_UNAVAILABLE, message);
    let failed = |_| {
        (
            StatusCode::BAD_GATEWAY,
            format!(
                "{} couldn’t fork this chat. Try again.",
                source.family.provider().display_name
            ),
        )
    };
    match source.family {
        AgentFamily::OpenCode | AgentFamily::Cursor => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "Forking is not available for {} yet.",
                source.family.provider().display_name
            ),
        )),
        AgentFamily::Codex => {
            let rpc = crate::providers::rpc_for_store(state, source.family, &source.provider_store)
                .await
                .map_err(unavailable)?;
            let mut params = json!({"threadId": native, "excludeTurns": true, "cwd": source.cwd});
            if let Some(turn) = last_turn {
                params["lastTurnId"] = json!(turn);
            }
            let forked = result(&rpc, "thread/fork", params).await.map_err(failed)?;
            let id = forked
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != native)
                .ok_or_else(|| failed(String::new()))?;
            Ok((id.to_owned(), id.to_owned(), None))
        }
        AgentFamily::Claude => {
            let rpc = claude_rpc(state).await.map_err(unavailable)?;
            let binding = state
                .store
                .runtime_binding(&source.conversation_id)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| failed(String::new()))?;
            let mut params = json!({"threadId": binding.thread_id, "title": title});
            if let Some(turn) = last_turn {
                params["lastTurnId"] = json!(turn);
            }
            let forked = result(&rpc, "project/session/fork", params)
                .await
                .map_err(|message| (StatusCode::CONFLICT, message))?;
            let session = forked
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| failed(String::new()))?
                .to_owned();
            let model = source.model.clone().ok_or_else(|| failed(String::new()))?;
            let attached = result(
                &rpc,
                "project/session/attach",
                json!({"sessionId": session, "model": model, "cwd": source.cwd,
                    "wonderProject": claude_project(project, &source.cwd),
                    "wonderPolicy": claude_policy(state, project, ClaudeModes::from(source), &source.cwd)}),
            )
            .await
            .map_err(failed)?;
            let thread = attached
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or_else(|| failed(String::new()))?
                .to_owned();
            Ok((session.clone(), thread, Some(session)))
        }
    }
}

/// Starts a new thread in the same project and folder with the same settings
/// and the source's history up to a chosen reply. Only a finished conversation
/// can be forked; the source is never changed.
pub(crate) async fn fork(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<ForkRequest>,
) -> Response {
    let last_turn = request.last_turn_id.as_deref();
    if last_turn.is_some_and(|turn| turn.is_empty() || turn.len() > 128) {
        return error(StatusCode::BAD_REQUEST, "Choose a reply to fork from.");
    }
    let Ok(Some(source)) = state.store.project_conversation(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let provider = source.family.provider();
    if !provider.capabilities.native_fork {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{} chats can’t be forked yet.", provider.display_name),
        );
    }
    let Some(native) = source.native_session_id.clone() else {
        return error(
            StatusCode::CONFLICT,
            "Send a message first. There is nothing to fork yet.",
        );
    };
    let busy = state
        .store
        .conversation_has_active_turn(&id)
        .await
        .unwrap_or(true)
        || (source.family == AgentFamily::Codex
            && codex_held(&state, &native, &source.provider_store).await);
    if busy {
        return error(
            StatusCode::CONFLICT,
            format!(
                "{} is still working in this chat. Fork it when the reply finishes.",
                provider.display_name
            ),
        );
    }
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return error(
            StatusCode::CONFLICT,
            "Cupola is preparing to update. Try again shortly.",
        );
    };
    let Ok(Some(project)) = state.store.project(&source.project_id).await else {
        return error(StatusCode::NOT_FOUND, "This chat’s project is unavailable.");
    };
    let project = thread_project(&state, project, &source.cwd).await;
    if !project.is_included || project.root_for(&source.cwd).is_none() {
        return error(
            StatusCode::CONFLICT,
            "This chat’s folder is no longer in the project. Add it back to fork.",
        );
    }
    match is_archived(&state, &source).await {
        Ok(false) => {}
        Ok(true) => {
            return error(
                StatusCode::CONFLICT,
                "This chat is archived. Restore it before forking.",
            )
        }
        Err(message) => return error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
    let title = fork_title(&source.title);
    let (forked_native, thread_id, session) =
        match fork_native(&state, &source, &project, &native, last_turn, &title).await {
            Ok(forked) => forked,
            Err((status, message)) => return error(status, message),
        };
    let not_saved = || {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The fork was made but could not be saved. Open this chat’s project to find it.",
        )
    };
    let fork_id = uuid::Uuid::new_v4().to_string();
    let now = now_text();
    let created = state
        .store
        .create_project_conversation(ProjectConversationInsert {
            conversation_id: &fork_id,
            project_id: &project.id,
            family: source.family,
            provider_store: &source.provider_store,
            native_session_id: Some(&forked_native),
            cwd: &source.cwd,
            roots_revision: project.roots_revision,
            title: &title,
            model: source.model.as_deref(),
            effort: source.effort.as_deref(),
            service_tier: source.service_tier.as_deref(),
            access_mode: &source.access_mode,
            claude_approval: &source.claude_approval,
            plan_mode: source.plan_mode,
            creation_request_id: None,
            now: &now,
        })
        .await;
    let Ok(ProjectConversationCreate::Created(fork)) = created else {
        return not_saved();
    };
    if state
        .store
        .bind_project_runtime(
            &fork.conversation_id,
            source.family,
            &source.provider_store,
            &thread_id,
            session.as_deref(),
            &now,
        )
        .await
        .is_err()
        || state
            .store
            .record_project_fork(&fork.conversation_id, &source.conversation_id, last_turn)
            .await
            .is_err()
    {
        return not_saved();
    }
    let _ = state
        .store
        .touch_project(&project.id, source.family, &now)
        .await;
    if source.family == AgentFamily::Codex
        && crate::providers::is_default_store(&state, source.family, &source.provider_store)
    {
        // Best effort: opening the thread repairs its desktop project later.
        let _ = associate_codex_project(&state, &project, &forked_native).await;
    }
    let summary = single_summary(&state, &fork, activity_seconds(&now), None).await;
    // Load the copied history so the new thread opens on it; this starts no
    // model work.
    let _ = crate::refresh_history(State(state.clone()), Path(fork.conversation_id.clone())).await;
    (StatusCode::CREATED, Json(summary)).into_response()
}

// ---------------------------------------------------------------------------
// Conversation metadata
// ---------------------------------------------------------------------------

async fn require_not_deleted(
    state: &AppState,
    family: AgentFamily,
    store: &str,
    native: &str,
) -> Result<(), (StatusCode, String)> {
    match state
        .store
        .project_session_deleted(family, store, native)
        .await
    {
        Ok(false) => Ok(()),
        Ok(true) => Err((
            StatusCode::GONE,
            "This chat was deleted from Cupola.".into(),
        )),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Deleted chats could not be checked. Try again.".into(),
        )),
    }
}

/// Deletes only Cupola's copy. Provider history and project files are untouched.
/// Dispatch and archive share these gates; the store checks queued work again
/// inside the deletion transaction to close the race with another device's send.
pub(crate) async fn delete_conversation(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
) -> Response {
    let _archive = state.projects.archive_lock(&id).await;
    let _sending = state.projects.conversation_dispatch(&id).await;
    if let Ok(Some(conversation)) = state.store.project_conversation(&id).await {
        let summary = attached_summary(&state, &conversation, 0, None).await;
        if summary.is_working {
            return error(
                StatusCode::CONFLICT,
                "The chat is working. Stop it or wait for it to finish before deleting.",
            );
        }
    }
    let outcome = {
        let _dispatch = state.dispatch_lock.lock().await;
        state.store.delete_project_chat(&id, &now_text()).await
    };
    match outcome {
        Ok(wonder_store::DeleteProjectChat::Deleted) => {
            if state
                .projects
                .native_search
                .purge_deleted(&state)
                .await
                .is_err()
            {
                return error(StatusCode::SERVICE_UNAVAILABLE, "The chat was removed, but its search cache could not be cleared. Try Delete again.");
            }
            state
                .projects
                .cursors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            announce_chats_changed(&state).await;
            Json(json!({"conversationId":id,"deleted":true})).into_response()
        }
        Ok(wonder_store::DeleteProjectChat::Missing) => StatusCode::NOT_FOUND.into_response(),
        Ok(wonder_store::DeleteProjectChat::Working) => error(
            StatusCode::CONFLICT,
            "Finish active work and close this chat’s Computer view before deleting.",
        ),
        Err(error) => {
            let _ = state.logger.record(
                "warn",
                "project_chat_delete_failed",
                json!({"error": error.to_string()}),
            );
            crate::projects::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The chat couldn’t be deleted. Try again.",
            )
        }
    }
}

pub(crate) async fn conversation(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
) -> Response {
    if is_outside_history(&id) {
        return match outside_history_detail(&state, &id).await {
            Ok((detail, _, _)) => Json(detail).into_response(),
            Err((status, message)) => error(status, message),
        };
    }
    // A turn made in the desktop app comes first, so the composer shows it.
    crate::native_settings::sync(&state, &id).await;
    let Ok(Some(conversation)) = state.store.project_conversation(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match conversation_detail(&state, &conversation).await {
        Ok(detail) => Json(detail).into_response(),
        Err(message) => error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdateConversationRequest {
    title: Option<String>,
    is_pinned: Option<bool>,
    is_archived: Option<bool>,
    has_unread: Option<bool>,
    model: Option<String>,
    #[serde(default, deserialize_with = "patch_effort")]
    effort: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_effort")]
    service_tier: Option<Option<String>>,
    access_mode: Option<String>,
    claude_approval: Option<String>,
    plan_mode: Option<bool>,
}

// Serde's ordinary nested Option treats both null and an omitted field as None.
// A present field must retain its null so PATCH can clear an old model's effort.
fn patch_effort<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

/// Approval modes exist only for Claude threads, and only with known values.
fn validate_claude_approval(
    family: AgentFamily,
    approval: Option<&str>,
) -> Result<(), &'static str> {
    match approval {
        None => Ok(()),
        Some(_) if !family.provider().capabilities.access_choices => {
            Err("Change permissions in OpenCode settings on your computer.")
        }
        Some(_) if !family.allows_approval_choice() => {
            Err("Approval modes are available for Claude threads.")
        }
        Some(approval) if !family.provider().approval_modes.contains(&approval) => {
            Err("Choose Ask, Accept edits or Auto.")
        }
        Some(_) => Ok(()),
    }
}

/// Checks a model someone is choosing now: a model the owner hid is refused.
pub(crate) fn validate_model(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: Option<&str>,
) -> Result<(), &'static str> {
    validate_model_choice(catalog, family, model, effort, service_tier, false)
}

/// Whether the provider itself hides the model, apart from the owner hiding it.
fn provider_hides(option: &crate::ModelOption) -> bool {
    option
        .provider_default
        .as_ref()
        .map_or(option.hidden, |d| d.hidden)
}

/// `keep_owner_hidden` lets a model the owner hid still run for a thread that
/// already uses it; hiding only stops new choices.
pub(crate) fn validate_model_choice(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: Option<&str>,
    keep_owner_hidden: bool,
) -> Result<(), &'static str> {
    let Some(model) = model else {
        return if effort.is_some() || service_tier.is_some() {
            Err("Choose a model before changing its settings.")
        } else {
            Ok(())
        };
    };
    let option = catalog
        .models
        .iter()
        .find(|m| {
            m.id == model
                && m.agent_family == family
                && if keep_owner_hidden {
                    !provider_hides(m)
                } else {
                    !m.hidden
                }
        })
        .ok_or("This model is not available for this conversation.")?;
    if let Some(effort) = effort {
        if !option.reasoning_efforts.iter().any(|e| e.id == effort) {
            return Err("This model does not support that effort.");
        }
    }
    if let Some(tier) = service_tier {
        if !agent_defaults::offers_tier(option, tier) {
            return Err("This model does not support that speed.");
        }
    }
    Ok(())
}

fn supports_tier(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    model: &str,
    tier: &str,
) -> bool {
    catalog
        .models
        .iter()
        .find(|option| option.agent_family == family && option.id == model)
        .is_some_and(|option| agent_defaults::offers_tier(option, tier))
}

pub(crate) async fn update_conversation(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<UpdateConversationRequest>,
) -> Response {
    let Ok(Some(existing)) = state.store.project_conversation(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(archived) = request.is_archived {
        if request.title.is_some()
            || request.is_pinned.is_some()
            || request.has_unread.is_some()
            || request.model.is_some()
            || request.effort.is_some()
            || request.service_tier.is_some()
            || request.access_mode.is_some()
            || request.claude_approval.is_some()
            || request.plan_mode.is_some()
        {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Save the archive change separately from other thread settings.",
            );
        }
        if let Err((status, message)) = set_archived(&state, &existing, archived).await {
            return error(status, message);
        }
        announce_chats_changed(&state).await;
        let Ok(Some(existing)) = state.store.project_conversation(&id).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        return match conversation_detail(&state, &existing).await {
            Ok(detail) if detail.is_archived == archived => Json(detail).into_response(),
            _ => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The archive change could not be confirmed. Refresh before trying again.",
            ),
        };
    }
    let catalog = state.runtime_catalog.read().await;
    let model = request.model.as_deref().or(existing.model.as_deref());
    let service_tier = request
        .service_tier
        .as_ref()
        .map_or(existing.service_tier.as_deref(), |tier| tier.as_deref());
    let clear_incompatible_tier = request.model.is_some()
        && request.service_tier.is_none()
        && service_tier.is_some_and(|tier| {
            !supports_tier(&catalog, existing.family, model.unwrap_or_default(), tier)
        });
    // An effort the user names must fit the model. One the model no longer
    // offers, or an explicit clear, takes the host's resolved default instead.
    let named_effort = request.effort.as_ref().and_then(|effort| effort.as_deref());
    let settled_effort = if request.model.is_some() || request.effort.is_some() {
        let carried = match &request.effort {
            None => existing.effort.as_deref(),
            Some(_) => named_effort,
        };
        Some(agent_defaults::resolve(&catalog, existing.family, model, carried, None).effort)
    } else {
        None
    };
    if request.model.is_some() || request.effort.is_some() || request.service_tier.is_some() {
        if let Err(message) = validate_model_choice(
            &catalog,
            existing.family,
            model,
            match (&request.effort, &settled_effort) {
                (Some(_), _) => named_effort,
                (None, Some(settled)) => settled.as_deref(),
                (None, None) => existing.effort.as_deref(),
            },
            if clear_incompatible_tier {
                None
            } else {
                service_tier
            },
            // Effort and speed on the thread's own model stay changeable
            // after the owner hides that model; picking another is a choice.
            request.model.is_none(),
        ) {
            return error(StatusCode::UNPROCESSABLE_ENTITY, message);
        }
    }
    drop(catalog);
    if request.access_mode.is_some() && !existing.family.provider().capabilities.access_choices {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Change permissions in OpenCode settings on your computer.",
        );
    }
    if request
        .access_mode
        .as_deref()
        .is_some_and(|mode| !ACCESS_MODES.contains(&mode))
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose Read only, Workspace or Full access.",
        );
    }
    if let Err(message) =
        validate_claude_approval(existing.family, request.claude_approval.as_deref())
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, message);
    }
    let patch = ProjectConversationPatch {
        title: request.title.as_deref(),
        pinned: request.is_pinned,
        unread: request.has_unread,
        model: request.model.as_deref(),
        effort: settled_effort.as_ref().map(|effort| effort.as_deref()),
        service_tier: if clear_incompatible_tier {
            Some(None)
        } else {
            request.service_tier.as_ref().map(|tier| tier.as_deref())
        },
        access_mode: request.access_mode.as_deref(),
        claude_approval: request.claude_approval.as_deref(),
        plan_mode: request.plan_mode,
        native_settings_turn: None,
    };
    let now = now_text();
    let updated =
        if request.model.is_some() || request.effort.is_some() || request.service_tier.is_some() {
            state
                .store
                .update_project_conversation_if_settings_match(
                    &id,
                    patch,
                    existing.model.as_deref(),
                    existing.effort.as_deref(),
                    existing.service_tier.as_deref(),
                    &now,
                )
                .await
        } else {
            state
                .store
                .update_project_conversation(&id, patch, &now)
                .await
        };
    if let (Ok(Some(conversation)), true) = (
        &updated,
        request.model.is_some() || request.effort.is_some(),
    ) {
        if let Some(model) = conversation.model.as_deref() {
            let _ = state
                .store
                .record_project_model(
                    &conversation.project_id,
                    conversation.family,
                    model,
                    conversation.effort.as_deref(),
                )
                .await;
        }
    }
    match updated {
        Ok(Some(conversation)) => match conversation_detail(&state, &conversation).await {
            Ok(detail) => Json(detail).into_response(),
            Err(message) => error(StatusCode::SERVICE_UNAVAILABLE, message),
        },
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(sqlx::Error::Protocol(message)) if message == "project_settings_changed" => error(
            StatusCode::CONFLICT,
            "The thread settings changed on another device. Refresh and try again.",
        ),
        Err(sqlx::Error::Protocol(message)) => error(StatusCode::UNPROCESSABLE_ENTITY, message),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The conversation could not be saved. Try again.",
        ),
    }
}

// ---------------------------------------------------------------------------
// New thread: the first message is the only thing that starts provider work
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateThreadRequest {
    device_id: String,
    client_message_id: String,
    family: AgentFamily,
    model: String,
    effort: Option<String>,
    service_tier: Option<String>,
    #[serde(default = "default_access")]
    access_mode: String,
    /// Claude threads only; part of the frozen creation request.
    claude_approval: Option<String>,
    #[serde(default)]
    plan_mode: bool,
    folder_id: Option<String>,
    roots_revision: Option<i64>,
    body: String,
    /// Reserve Wonder metadata for authenticated attachment uploads. This does
    /// not start a provider session or enqueue a message.
    #[serde(default)]
    prepare_only: bool,
    #[serde(default)]
    attachment_ids: Vec<String>,
    #[serde(default)]
    codex_skills: Vec<wonder_store::CodexSkillSelection>,
    /// Compatibility with older clients: only `default` is accepted.
    /// New threads always use this computer's own provider sign-in.
    account_id: Option<String>,
}

fn default_access() -> String {
    "workspace".into()
}

// Frozen request identity excludes the draft body and staged attachments:
// prepareOnly reserves a chat before either exists. The accepted message row
// compares both when replaying a submitted first message.
fn creation_request_digest(project_id: &str, request: &CreateThreadRequest) -> String {
    let mut intent = json!({
        "projectId": project_id,
        "deviceId": request.device_id,
        "clientMessageId": request.client_message_id,
        "family": request.family,
        "model": request.model,
        "effort": request.effort,
        "serviceTier": request.service_tier,
        "accessMode": request.access_mode,
        "claudeApproval": request.claude_approval.as_deref().unwrap_or(agent_defaults::claude_approval(request.family)),
        "planMode": request.plan_mode,
        "folderId": request.folder_id,
        "rootsRevision": request.roots_revision,
    });
    // Added after the first digests were stored; absent keeps them equal.
    if let Some(account) = &request.account_id {
        intent["accountId"] = json!(account);
    }
    hex::encode(Sha256::digest(
        serde_json::to_vec(&intent).unwrap_or_default(),
    ))
}

fn legacy_creation_matches(
    payload: &Value,
    project_id: &str,
    request: &CreateThreadRequest,
    conversation: &StoredProjectConversation,
) -> bool {
    let Some(fields) = payload.as_array() else {
        return false;
    };
    fields.len()
        == if request.service_tier.is_some() {
            11
        } else {
            10
        }
        && fields[0] == json!(project_id)
        && fields[1] == json!(request.family.as_str())
        && fields[3] == json!(conversation.cwd)
        && fields[4] == json!(conversation.roots_revision)
        && fields[5] == json!(request.model)
        && fields[6] == json!(request.effort)
        && fields[7] == json!(request.access_mode)
        && fields[8]
            == json!(request
                .claude_approval
                .as_deref()
                .unwrap_or(agent_defaults::claude_approval(request.family)))
        && fields[9] == json!(i64::from(request.plan_mode))
        && request
            .roots_revision
            .is_none_or(|value| value == conversation.roots_revision)
        && request
            .service_tier
            .as_ref()
            .is_none_or(|tier| fields[10] == json!(tier))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateThreadResponse {
    conversation: ProjectThreadSummary,
    receipt: Option<wonder_api::ClientMessageReceipt>,
}

/// The Project ID a chat started without a Project goes into.
pub(crate) const SCRATCH_PROJECT_ID: &str = "scratch";
/// What the owner sees for it: a chat in no Project of their own.
const NO_PROJECT_NAME: &str = "No project";
/// The name it had before; a name the owner chose is left alone.
const LEGACY_SCRATCH_NAME: &str = "Scratch";
const SCRATCH_REQUEST_ID: &str = "5c2a7d1e-0000-4000-8000-5c4a7c400001";

/// Projects made before the rename keep "Scratch" in their store; the list
/// renames that one once, so every phone shows "No project". The write only
/// lands while the stored name is still "Scratch"; when the owner renamed it
/// after the list was read, the list shows their name instead.
async fn rename_legacy_scratch(state: &AppState, projects: &mut [wonder_store::StoredProject]) {
    let Some(project) = projects
        .iter_mut()
        .find(|p| p.id == SCRATCH_PROJECT_ID && p.name == LEGACY_SCRATCH_NAME)
    else {
        return;
    };
    match state
        .store
        .rename_project_if_named(
            SCRATCH_PROJECT_ID,
            LEGACY_SCRATCH_NAME,
            NO_PROJECT_NAME,
            &now_text(),
        )
        .await
    {
        Ok(true) => project.name = NO_PROJECT_NAME.to_owned(),
        Ok(false) => {
            if let Ok(Some(current)) = state.store.project(SCRATCH_PROJECT_ID).await {
                project.name = current.name;
            }
        }
        Err(_) => {}
    }
}

/// Makes the no-Project Project on first use: one folder, `~/Cupola Scratch`
/// (the data directory is off limits to agents), so a chat needs no Project
/// of the owner's.
async fn ensure_scratch_project(state: &AppState) -> Result<(), (StatusCode, &'static str)> {
    if matches!(state.store.project(SCRATCH_PROJECT_ID).await, Ok(Some(_))) {
        return Ok(());
    }
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Your computer’s home folder could not be found.",
        ));
    };
    let folder = PathBuf::from(home).join("Cupola Scratch");
    let denied = state.denied_roots.clone();
    let roots = tokio::task::spawn_blocking(move || {
        // An existing folder (even with files) is reused as is.
        if !std::fs::symlink_metadata(&folder).is_ok_and(|m| m.is_dir()) {
            create_project_folder(&folder).map_err(|e| e.1)?;
        }
        folder
            .to_str()
            .map(str::to_owned)
            .ok_or("Folder names must be valid text.")
            .and_then(|path| validate_folders(&[path], &denied))
    })
    .await
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The no-project folder could not be set up.",
        )
    })?
    .map_err(|m| (StatusCode::UNPROCESSABLE_ENTITY, m))?;
    let canonical: Vec<String> = roots.iter().map(|r| r.canonical_path.clone()).collect();
    match state
        .store
        .create_project(
            SCRATCH_PROJECT_ID,
            SCRATCH_REQUEST_ID,
            &creation_payload(NO_PROJECT_NAME, &canonical, 0),
            NO_PROJECT_NAME,
            &roots,
            0,
            &now_text(),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The no-project folder could not be saved. Try again.",
        )),
    }
}

pub(crate) async fn create_thread(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<CreateThreadRequest>,
) -> Response {
    if id == SCRATCH_PROJECT_ID {
        if let Err((status, message)) = ensure_scratch_project(&state).await {
            return error(status, message);
        }
    }
    if request
        .account_id
        .as_deref()
        .is_some_and(|id| id != "default")
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "New chats use this computer's own sign-in. Start a new chat without an added account.",
        );
    }
    if let Some(Extension(device)) = authenticated_device {
        if device.device_id != request.device_id {
            return error(StatusCode::FORBIDDEN, "device identity mismatch");
        }
    } else if request.device_id == crate::rpc::LOCAL_SENDER
        // The local owner starts threads as the desktop identity, as its
        // sends do; its device row may not exist before its first message.
        && state.store.ensure_local_desktop(&now_text()).await.is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Desktop identity could not be saved",
        );
    }
    if uuid::Uuid::parse_str(&request.client_message_id).is_err() {
        return error(StatusCode::BAD_REQUEST, "clientMessageId must be a UUID");
    }
    if (!request.prepare_only
        && request.body.trim().is_empty()
        && request.attachment_ids.is_empty())
        || request.body.len() > 64 * 1024
    {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "message body must be between 1 and 65536 bytes",
        );
    }
    if !crate::valid_attachment_ids(&request.attachment_ids) {
        return error(StatusCode::BAD_REQUEST, "invalid attachmentIds");
    }
    if !request.family.provider().capabilities.access_choices && request.access_mode != "workspace"
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Change permissions in OpenCode settings on your computer.",
        );
    }
    if !ACCESS_MODES.contains(&request.access_mode.as_str()) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose Read only, Workspace or Full access.",
        );
    }
    if let Err(message) =
        validate_claude_approval(request.family, request.claude_approval.as_deref())
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, message);
    }
    let request_digest = creation_request_digest(&id, &request);
    let body_sha256 = hex::encode(Sha256::digest(request.body.as_bytes()));
    let existing_creation = match state
        .store
        .project_creation_by_request(&request.client_message_id)
        .await
    {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The thread could not be checked. Try again.",
            )
        }
    };
    let existing_conversation = if let Some((conversation, payload)) = existing_creation {
        if conversation.project_id != id {
            return error(
                StatusCode::CONFLICT,
                "This request belongs to another Project.",
            );
        }
        let matches = if payload["version"] == 2 {
            payload["requestDigest"].as_str() == Some(request_digest.as_str())
        } else {
            let legacy = legacy_creation_matches(&payload, &id, &request, &conversation);
            if legacy {
                // Old records stored only the resolved folder path. When an
                // explicit folder ID is gone, its original identity cannot
                // be proven; do not silently treat a changed ID as exact.
                if let Some(folder_id) = request.folder_id.as_deref() {
                    let project = match state.store.project(&id).await {
                        Ok(Some(project)) => project,
                        _ => return error(StatusCode::CONFLICT, "The original Project folder could not be verified for this older request."),
                    };
                    project
                        .roots
                        .iter()
                        .any(|root| root.id == folder_id && root.path == conversation.cwd)
                } else {
                    true
                }
            } else {
                false
            }
        };
        if !matches {
            return error(
                StatusCode::CONFLICT,
                "This thread was already created with different settings or a different folder.",
            );
        }
        let now = now_text();
        if request.prepare_only {
            return Json(CreateThreadResponse {
                conversation: single_summary(&state, &conversation, activity_seconds(&now), None)
                    .await,
                receipt: None,
            })
            .into_response();
        }
        let prior = match state
            .store
            .message_by_device_and_client_message_id(&request.device_id, &request.client_message_id)
            .await
        {
            Ok(value) => value,
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The message could not be checked. Try again.",
                )
            }
        };
        if let Some(message) = prior {
            let saved_ids = match state.store.attachment_ids_for_message(&message.id).await {
                Ok(value) => value,
                Err(_) => {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "The message attachments could not be checked. Try again.",
                    )
                }
            };
            let mut requested_ids = request.attachment_ids.clone();
            requested_ids.sort();
            if message.conversation_id != conversation.conversation_id
                || message.body_sha256 != body_sha256
                || saved_ids != requested_ids
                || state
                    .store
                    .original_message_codex_skills(&message.id)
                    .await
                    .ok()
                    .as_deref()
                    != Some(request.codex_skills.as_slice())
            {
                return error(
                    StatusCode::CONFLICT,
                    "This message was already sent differently.",
                );
            }
            let Some(receipt) = crate::queue::receipt(&state, message).await else {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Stored message has an unknown delivery state.",
                );
            };
            return (
                StatusCode::ACCEPTED,
                Json(CreateThreadResponse {
                    conversation: single_summary(
                        &state,
                        &conversation,
                        activity_seconds(&now),
                        None,
                    )
                    .await,
                    receipt: Some(receipt),
                }),
            )
                .into_response();
        }
        Some(conversation)
    } else {
        None
    };
    // The update lease fences first acceptance and fresh metadata creation.
    // A stored receipt (or metadata-only reservation replay) above is a read,
    // so it remains recoverable while the host prepares an update.
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return error(
            StatusCode::CONFLICT,
            "Cupola is preparing to update. Try again shortly.",
        );
    };
    if request.family == AgentFamily::Claude && state.claude.is_none() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            crate::claude::RUNTIME_MISSING,
        );
    }
    if matches!(request.family, AgentFamily::OpenCode | AgentFamily::Cursor)
        && !crate::claude::available(&state, request.family)
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "Cupola’s {} runtime is missing. Update Cupola on your computer.",
                request.family.provider().display_name
            ),
        );
    }
    let readiness = state.ingestion.project_readiness(&state.store).await;
    if !readiness.ready {
        return error(StatusCode::SERVICE_UNAVAILABLE, readiness.detail);
    }
    let Ok(Some(project)) = state.store.project(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !project.is_included {
        return error(
            StatusCode::CONFLICT,
            "This Project is not included in Cupola. Restore it before starting work.",
        );
    }
    if request
        .roots_revision
        .is_some_and(|revision| revision != project.roots_revision)
    {
        return error(
            StatusCode::PRECONDITION_FAILED,
            "The project's folders changed. Review them before sending.",
        );
    }
    let root = match request.folder_id.as_deref() {
        Some(folder) => project.roots.iter().find(|r| r.id == folder),
        None => Some(project.primary_root()),
    };
    let Some(root) = root.cloned() else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose one of this project's folders.",
        );
    };
    let checked_project = project.clone();
    let denied = state.denied_roots.clone();
    let folders_valid =
        tokio::task::spawn_blocking(move || validate_execution_roots(&checked_project, &denied))
            .await;
    match folders_valid {
        Ok(Ok(())) => {}
        Ok(Err(detail)) => return error(StatusCode::CONFLICT, detail),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Project folders could not be checked. Try again.",
            )
        }
    }
    if let Some(existing) = &existing_conversation {
        if existing.cwd != root.path || existing.roots_revision != project.roots_revision {
            return error(
                StatusCode::PRECONDITION_FAILED,
                "The original Project folder changed. Review it before sending.",
            );
        }
    }
    let now = now_text();
    // Taken once from the whole first message, so a scripted run keeps the
    // task its brief names even where a provider lists only the start.
    let title: String = match request.body.trim() {
        "" => "New thread".into(),
        body => display_title(None, Some(body)).chars().take(80).collect(),
    };
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let store = provider_store(&state, request.family).to_owned();
    let resolved = agent_defaults::resolve(
        &*state.runtime_catalog.read().await,
        request.family,
        Some(&request.model),
        request.effort.as_deref(),
        request.service_tier.as_deref(),
    );
    let insert = ProjectConversationInsert {
        conversation_id: &conversation_id,
        project_id: &project.id,
        family: request.family,
        provider_store: &store,
        native_session_id: None,
        cwd: &root.path,
        roots_revision: project.roots_revision,
        title: &title,
        model: Some(&request.model),
        effort: resolved.effort.as_deref(),
        service_tier: resolved.service_tier.as_deref(),
        access_mode: &request.access_mode,
        claude_approval: request
            .claude_approval
            .as_deref()
            .unwrap_or(agent_defaults::claude_approval(request.family)),
        plan_mode: request.plan_mode,
        creation_request_id: Some(&request.client_message_id),
        now: &now,
    };
    if existing_conversation.is_none() {
        if let Err(message) = validate_model(
            &*state.runtime_catalog.read().await,
            request.family,
            Some(&request.model),
            request.effort.as_deref(),
            request.service_tier.as_deref(),
        ) {
            return error(StatusCode::UNPROCESSABLE_ENTITY, message);
        }
    }
    let created = match existing_conversation {
        Some(conversation) => Ok(ProjectConversationCreate::Existing(conversation)),
        None => {
            state
                .store
                .create_project_conversation_with_request_digest(insert, &request_digest)
                .await
        }
    };
    let conversation = match created {
        // Its title is the first message's first line.
        Ok(ProjectConversationCreate::Created(mut c)) => {
            if state
                .store
                .mark_project_conversation_title_taken(&c.conversation_id)
                .await
                .is_ok()
            {
                c.title_chosen = false;
            }
            c
        }
        Ok(ProjectConversationCreate::Existing(c)) => c,
        Err(sqlx::Error::Protocol(message)) => return error(StatusCode::CONFLICT, message),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The thread could not be saved. Try again.",
            )
        }
    };
    // The owner chose this model for the thread: new chats in the Project
    // start from it.
    let _ = state
        .store
        .record_project_model(
            &project.id,
            request.family,
            &request.model,
            conversation.effort.as_deref(),
        )
        .await;
    if request.prepare_only {
        return Json(CreateThreadResponse {
            conversation: single_summary(&state, &conversation, activity_seconds(&now), None).await,
            receipt: None,
        })
        .into_response();
    }
    if let Err((status, detail)) = crate::artifact_annotations::validate_first_acceptance(
        &state,
        &conversation.conversation_id,
        &request.device_id,
        &request.client_message_id,
        &request.attachment_ids,
    )
    .await
    {
        return error(status, detail);
    }
    if let Err(error) = crate::codex_skills::validate_acceptance(
        &state,
        &conversation.conversation_id,
        &request.device_id,
        &request.client_message_id,
        Some(request.family),
        &request.body,
        &request.codex_skills,
    )
    .await
    {
        return crate::codex_skills::rejection_response(error);
    }
    let stored = match state
        .store
        .insert_message_with_skills(
            &request.device_id,
            &request.client_message_id,
            &request.body,
            &body_sha256,
            &conversation.conversation_id,
            &request.attachment_ids,
            &now,
            None,
            &request.codex_skills,
        )
        .await
    {
        Ok(MessageInsert::Inserted(m) | MessageInsert::Existing(m)) => m,
        Ok(MessageInsert::Conflict) => {
            return error(
                StatusCode::CONFLICT,
                "This message was already sent differently.",
            )
        }
        Err(sqlx::Error::Protocol(message)) if message == "annotation_already_sent" => {
            return error(
                StatusCode::CONFLICT,
                "This annotation was already sent. Add a new annotation to send another note.",
            )
        }
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Message saved state is unknown. Retry to finish sending.",
            )
        }
    };
    let _ = state
        .store
        .touch_project(&project.id, request.family, &now)
        .await;
    let Some(receipt) = crate::queue::receipt(&state, stored).await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "stored message has an unknown delivery state",
        );
    };
    state.dispatch_wake.notify_one();
    let conversation = single_summary(&state, &conversation, activity_seconds(&now), None).await;
    (
        StatusCode::ACCEPTED,
        Json(CreateThreadResponse {
            conversation,
            receipt: Some(receipt),
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Codex sandbox/approval for a Wonder turn. Always explicit: a resumed thread
/// otherwise inherits whatever the last desktop client used (P0 finding).
fn codex_policy(mode: &str, roots: &[String]) -> (&'static str, &'static str, Value) {
    match mode {
        "read_only" => (
            "read-only",
            "on-request",
            json!({"type": "readOnly", "networkAccess": false}),
        ),
        "full_access" => (
            "danger-full-access",
            "never",
            json!({"type": "dangerFullAccess"}),
        ),
        _ => (
            "workspace-write",
            "on-request",
            json!({"type": "workspaceWrite", "writableRoots": roots, "networkAccess": false}),
        ),
    }
}

/// The model, effort and speed one turn sends. Threads saved before the host
/// stored defaults resolve here with the rule creation uses, so a model that
/// offers efforts never runs with a nil one, and a stored speed such as
/// priority reaches the provider while the model still offers it.
///
/// This is for a model someone is choosing now (a delegated thread): a model
/// the owner hid is refused. `dispatch_settings` is for threads that already
/// use a model.
pub(crate) fn execution_settings(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: Option<&str>,
) -> Result<(String, Option<String>, Option<String>), String> {
    settings_for(catalog, family, model, effort, service_tier, false)
}

/// Like `execution_settings` for a thread that already uses its model: a model
/// the owner hid afterwards keeps working there, only the provider's own
/// hiding stops it.
pub(crate) fn dispatch_settings(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: Option<&str>,
) -> Result<(String, Option<String>, Option<String>), String> {
    settings_for(catalog, family, model, effort, service_tier, true)
}

fn settings_for(
    catalog: &crate::RuntimeCatalog,
    family: AgentFamily,
    model: Option<&str>,
    effort: Option<&str>,
    service_tier: Option<&str>,
    keep_owner_hidden: bool,
) -> Result<(String, Option<String>, Option<String>), String> {
    let resolved = agent_defaults::resolve(catalog, family, model, effort, service_tier);
    let model = resolved.model.ok_or("Choose a model for this thread.")?;
    validate_model_choice(
        catalog,
        family,
        Some(&model),
        resolved.effort.as_deref(),
        resolved.service_tier.as_deref(),
        keep_owner_hidden,
    )
    .map_err(str::to_owned)?;
    Ok((model, resolved.effort, resolved.service_tier))
}

/// The folders a Claude session in `cwd` works in, and the fingerprints of
/// the Project's own Claude configuration the owner trusted: the runtime loads
/// a folder's settings, hooks and MCP servers only when it still matches one.
pub(crate) fn claude_project(project: &StoredProject, cwd: &str) -> Value {
    json!({"cwd": cwd, "additionalDirectories": project.roots.iter().map(|r| r.path.clone()).filter(|p| p != cwd).collect::<Vec<_>>(),
        "trustedConfigurations": project.claude_trust})
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaudeTrustQuery {
    /// The folder a chat works in; the Project's main folder when absent.
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaudeTrustRequest {
    /// The fingerprint the owner reviewed, from a read of the same folder.
    fingerprint: String,
    cwd: Option<String>,
}

/// What a Project folder's own Claude Code configuration would run (hooks,
/// tool servers, permission rules, helpers), read by the Claude runtime now.
async fn claude_configuration(
    state: &AppState,
    id: &str,
    cwd: Option<String>,
) -> Result<(StoredProject, Value), Response> {
    let project = match state.store.project(id).await {
        Ok(Some(project)) => project,
        Ok(None) => return Err(error(StatusCode::NOT_FOUND, "Project not found.")),
        Err(_) => {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The project could not be loaded. Try again.",
            ))
        }
    };
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => match project
            .roots
            .iter()
            .find(|r| r.id == project.primary_root_id)
        {
            Some(root) => root.path.clone(),
            None => return Err(error(StatusCode::NOT_FOUND, "Project not found.")),
        },
    };
    let folders = thread_project(state, project.clone(), &cwd).await;
    let Some(root) = folders.root_for(&cwd) else {
        return Err(error(
            StatusCode::FORBIDDEN,
            "This folder is outside the project's folders.",
        ));
    };
    let rpc = claude_rpc(state)
        .await
        .map_err(|message| error(StatusCode::SERVICE_UNAVAILABLE, message))?;
    let configuration = result(
        &rpc,
        "project/configuration/read",
        json!({"wonderProject": claude_project(&execution_project(&folders, root), &root.path)}),
    )
    .await
    .map_err(|message| error(StatusCode::SERVICE_UNAVAILABLE, message))?;
    Ok((project, configuration))
}

pub(crate) async fn claude_trust(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Query(query): Query<ClaudeTrustQuery>,
) -> Response {
    match claude_configuration(&state, &id, query.cwd).await {
        Ok((_, configuration)) => Json(configuration).into_response(),
        Err(response) => response,
    }
}

/// Trusts the folder's configuration exactly as the owner reviewed it: a
/// change since that read is refused, so the owner reviews it again.
pub(crate) async fn trust_claude(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<ClaudeTrustRequest>,
) -> Response {
    let (project, configuration) = match claude_configuration(&state, &id, request.cwd).await {
        Ok(read) => read,
        Err(response) => return response,
    };
    if configuration["empty"] == true {
        return Json(configuration).into_response();
    }
    if configuration["tooLarge"] == true {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "This folder's Claude settings are too large for Cupola to check, so they stay off.",
        );
    }
    let Some(current) = configuration["fingerprint"].as_str() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude couldn't read this folder's settings. Try again.",
        );
    };
    if current != request.fingerprint {
        return error(
            StatusCode::CONFLICT,
            "This folder's Claude settings changed since you reviewed them. Review them again.",
        );
    }
    if state
        .store
        .trust_project_claude_configuration(&project.id, current, &now_text())
        .await
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cupola couldn't save your choice. Try again.",
        );
    }
    let mut configuration = configuration;
    configuration["trusted"] = Value::Bool(true);
    Json(configuration).into_response()
}

/// Stops trusting every configuration of the Project's folders. Claude
/// processes started after this load only the owner's own settings.
pub(crate) async fn revoke_claude_trust(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
) -> Response {
    match state.store.project(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::NOT_FOUND, "Project not found."),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The project could not be loaded. Try again.",
            )
        }
    }
    match state.store.revoke_project_claude_configuration(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cupola couldn't save your choice. Try again.",
        ),
    }
}

/// A Claude thread's stored access, approval and plan settings.
#[derive(Clone, Copy, Debug)]
struct ClaudeModes<'a> {
    access_mode: &'a str,
    approval: &'a str,
    plan: bool,
}

impl Default for ClaudeModes<'_> {
    fn default() -> Self {
        Self {
            access_mode: "workspace",
            approval: agent_defaults::claude_approval(AgentFamily::Claude),
            plan: false,
        }
    }
}

impl<'a> From<&'a StoredProjectConversation> for ClaudeModes<'a> {
    fn from(conversation: &'a StoredProjectConversation) -> Self {
        Self {
            access_mode: &conversation.access_mode,
            approval: &conversation.claude_approval,
            plan: conversation.plan_mode,
        }
    }
}

/// The bridge policy for one turn. Access decides the file scope; Claude's
/// approval mode only refines Workspace, so Read only always asks and Full
/// access never does. The bridge derives the command sandbox from the same
/// policy: Full access outside Plan has none. Unknown stored values fall back
/// to the safest mode.
fn claude_policy_value(
    roots: Vec<String>,
    denied_roots: &[String],
    modes: ClaudeModes<'_>,
    cwd: &str,
) -> Value {
    let reads = claude_read_roots(&roots);
    let (mode, approval, writes) = match modes.access_mode {
        "read_only" => ("read_only", "ask", Vec::new()),
        "full_access" => ("full_access", "full_access", roots),
        _ => (
            "workspace",
            match modes.approval {
                "accept_edits" => "accept_edits",
                "auto" => "auto",
                _ => "ask",
            },
            roots,
        ),
    };
    json!({"mode": mode, "approvalMode": approval, "planMode": modes.plan, "workspace": cwd,
        "readRoots": reads, "writeRoots": writes, "deniedRoots": denied_roots})
}

// `/` means the whole filesystem on Unix, but only the current drive on
// Windows. Name drive roots explicitly, plus any Project network shares.
// Inaccessible/missing roots remain closed by the bridge's canonical check.
fn claude_read_roots(_project_roots: &[String]) -> Vec<String> {
    #[cfg(not(windows))]
    {
        vec!["/".to_owned()]
    }
    #[cfg(windows)]
    {
        let mut roots: Vec<String> = (b'A'..=b'Z')
            .map(|drive| format!("{}:\\", char::from(drive)))
            .collect();
        for path in _project_roots {
            if let Some(root) = std::path::Path::new(path).ancestors().last() {
                if root.is_absolute() {
                    let root = root.to_string_lossy().into_owned();
                    if !roots.iter().any(|value| value.eq_ignore_ascii_case(&root)) {
                        roots.push(root);
                    }
                }
            }
        }
        roots
    }
}

// Contract: a saved Project folder is written as the plain path the
// browser, the execution recheck and the protected-folder checks all
// compare with (on Windows `C:\\…`, never `\\?\C:\\…`), and a saved root
// validates to itself again.
#[cfg(test)]
#[test]
fn saved_project_folder_uses_the_one_canonical_spelling() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("app");
    std::fs::create_dir_all(&project).unwrap();
    let denied = vec![temp.path().join("secrets").to_string_lossy().into_owned()];
    let root = validate_folder(project.to_str().unwrap(), &denied).unwrap();
    assert_eq!(
        root.canonical_path,
        crate::canonical_path::canonicalize(&project)
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert!(!root.canonical_path.starts_with(r"\\?\"));
    // The stored spelling is a fixed point, so the recheck before every
    // run finds the same folder.
    let again = validate_folder(&root.canonical_path, &denied).unwrap();
    assert_eq!(again.canonical_path, root.canonical_path);
    // A protected folder is still found when it is spelled the other way.
    #[cfg(windows)]
    {
        let verbatim = format!(r"\\?\{}", root.canonical_path);
        let secrets = vec![verbatim];
        assert!(validate_folder(&root.canonical_path, &secrets).is_err());
    }
}

#[test]
#[cfg(all(test, windows))]
fn claude_windows_reads_name_drives_and_project_shares_explicitly() {
    let roots = claude_read_roots(&[
        r"D:\Work\App".to_owned(),
        r"\\server\share\Project".to_owned(),
    ]);
    for path in [r"C:\", r"D:\", r"Z:\", r"\\server\share\"] {
        assert!(roots.iter().any(|root| root == path), "missing {path}");
    }
    assert!(roots
        .iter()
        .all(|root| std::path::Path::new(root).is_absolute()));
    assert!(!roots.iter().any(|root| root == "/"));
}

fn claude_policy(
    state: &AppState,
    project: &StoredProject,
    modes: ClaudeModes<'_>,
    cwd: &str,
) -> Value {
    claude_policy_value(root_paths(project), &state.denied_roots, modes, cwd)
}

/// Codex `turn/start` parameters. Sandbox and approval stay explicit, and the
/// collaboration mode is always sent so a thread that left plan mode does not
/// keep the previous turn's mode.
fn codex_turn_params(
    thread_id: &str,
    client_message_id: &str,
    input: &[Value],
    turn: &CodexTurn<'_>,
) -> Value {
    let (_, approval, sandbox_policy) = codex_policy(turn.access_mode, turn.roots);
    json!({"threadId": thread_id, "clientUserMessageId": client_message_id, "input": input,
        "model": turn.model, "effort": turn.effort, "serviceTier": turn.service_tier, "cwd": turn.cwd,
        "approvalPolicy": approval, "sandboxPolicy": sandbox_policy, "runtimeWorkspaceRoots": turn.roots,
        "collaborationMode": {"mode": if turn.plan { "plan" } else { "default" },
            "settings": {"model": turn.model, "reasoning_effort": turn.effort, "developer_instructions": null}}})
}

struct CodexTurn<'a> {
    access_mode: &'a str,
    roots: &'a [String],
    model: &'a str,
    effort: Option<&'a str>,
    service_tier: Option<&'a str>,
    cwd: &'a str,
    plan: bool,
}

/// Reuse normal execution's root checks before restoring frozen update policy.
/// Returns the current bridge policy to resume with.
pub(crate) async fn validate_update_policy(
    state: &AppState,
    conversation: &StoredProjectConversation,
    resume: &Value,
    turn: &Value,
) -> Result<Option<Value>, String> {
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("The paused project was removed")?;
    let project = thread_project(state, project, &conversation.cwd).await;
    let roots = root_paths(&project);
    let checked = project.clone();
    let denied = state.denied_roots.clone();
    tokio::task::spawn_blocking(move || validate_execution_roots(&checked, &denied))
        .await
        .map_err(|e| e.to_string())?
        .map_err(str::to_owned)?;
    if project.root_for(&conversation.cwd).is_none()
        || resume["cwd"].as_str() != Some(&conversation.cwd)
    {
        return Err("The paused project folder is no longer authorized".into());
    }
    let (compatible, fresh) = match conversation.family {
        AgentFamily::Codex => {
            let (_, approval, policy) = codex_policy(&conversation.access_mode, &roots);
            (
                resume["runtimeWorkspaceRoots"] == json!(roots)
                    && turn["sandboxPolicy"] == policy
                    && turn["approvalPolicy"] == approval,
                None,
            )
        }
        AgentFamily::Claude => {
            let mut policy = claude_policy(
                state,
                &project,
                ClaudeModes::from(conversation),
                &conversation.cwd,
            );
            policy["workspace"] = json!(media_workspace(state, &conversation.conversation_id)
                .await
                .ok_or("The paused attachment folder is unavailable")?);
            (
                crate::update_handoff::same_claude_access(&resume["wonderPolicy"], &policy)
                    && crate::update_handoff::same_claude_access(&turn["wonderPolicy"], &policy),
                Some(policy),
            )
        }
        AgentFamily::OpenCode | AgentFamily::Cursor => {
            let policy = json!({"planMode": conversation.plan_mode});
            (
                resume["wonderPolicy"] == policy && turn["wonderPolicy"] == policy,
                Some(policy),
            )
        }
    };
    if !compatible {
        return Err("Project access changed while work was paused.".into());
    }
    Ok(fresh)
}

pub(crate) async fn is_project(state: &AppState, conversation: &str) -> bool {
    matches!(
        state.store.project_conversation(conversation).await,
        Ok(Some(_))
    )
}

// Queue acceptance is not a turn receipt. Keep it non-retryable until native
// history supplies one; even an empty queue/history read can race consumption.
const CODEX_NATIVE_QUEUED_NOTICE: &str = "Your message is queued in Codex on your computer. It will run when that thread is free. Codex will use this thread's model and permissions; approvals appear there. Cupola will show the reply here.";
const CODEX_NATIVE_QUEUE_UNCONFIRMED_NOTICE: &str = "Cupola is checking whether Codex queued your message. It won't send it again while delivery is unconfirmed. Check this thread in Codex on your computer.";
const CODEX_DESKTOP_RECOVERY_NOTICE: &str = "The Codex app on your computer is using this thread. Your message is saved. Let any reply finish, then quit Codex and retry in Cupola.";
// The queue cannot prove a lost input, so an expired send is never retried.
const CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE: &str =
    "Cupola could not confirm Codex received this. Open the thread in Codex on your computer to check.";

async fn native_queue_supported(state: &AppState, rpc: &RpcClient) -> bool {
    let generation = rpc.health().id().to_owned();
    state
        .projects
        .native_queue_capability
        .lock()
        .await
        .as_ref()
        .is_some_and(|(id, supported)| *id == generation && *supported)
}

const CODEX_WORKING_NOTICE: &str = "Your message is queued while Codex is working on your computer. Let the reply finish; Cupola will try again automatically.";
const CODEX_UNCONFIRMED_NOTICE: &str = "Your message is queued because Cupola can't confirm this thread is idle. Open it in Codex on your computer and let any reply finish. If it stays queued, quit and reopen Codex; Cupola will check again automatically.";

/// Whether a Wonder message must wait for the Mac app: the Claude chat is open
/// in Claude Code in a terminal or another Agent SDK app (T3 Code), or Codex's
/// desktop app is running a turn in it. Reads native thread state. With
/// Claude app sync on, Claude's desktop app never holds it, even mid-reply:
/// the last sender wins, and dispatch stops that process before continuing
/// the chat (see `take_over_from_desktop`); off (the default), it holds it
/// like a terminal. Send now takes over any holder.
/// `waiting` is the waiting message when dispatching it. That admission may
/// start Wonder's stopped runtime to recover native evidence.
pub(crate) async fn held_on_mac(
    state: &AppState,
    conversation: &str,
    waiting: Option<&wonder_store::StoredMessage>,
) -> bool {
    let sent_at = waiting.map(|message| message.created_at.as_str());
    let Ok(Some(stored)) = state.store.project_conversation(conversation).await else {
        return false;
    };
    let Some(native) = stored.native_session_id.as_deref() else {
        return false;
    };
    let held = match stored.family {
        AgentFamily::Claude => {
            // Send now: dispatch takes the chat over (`take_over_from_desktop`).
            !take_over_requested(state, conversation)
                && crate::desktop_activity::claude_holds_session(
                    native,
                    sent_at.map(sent_at_ms),
                    crate::agent_settings::claude_app_sync(state).await,
                )
                .await
                .is_some()
        }
        AgentFamily::Codex => {
            let busy = codex_busy(state, native, &stored.provider_store, sent_at.is_some()).await;
            // ChatGPT's desktop app running a turn in it takes a waiting
            // message into that turn now (`desktop_relay`), not after it.
            if let Some(message) = waiting {
                if busy == Some(true)
                    && relay_carries(state, message, stored.family).await
                    && relays_to_desktop(state, stored.family, native, &stored.provider_store).await
                {
                    return false;
                }
            }
            // The native queue serializes with its existing writer, so an
            // unconfirmed private index need not block preparing a queued send.
            // Known active work still waits. Older runtimes keep the idle gate.
            let queue_waits_for_owner = if busy.is_none()
                && sent_at.is_some()
                && crate::providers::is_default_store(
                    state,
                    AgentFamily::Codex,
                    &stored.provider_store,
                ) {
                match live_rpc_for(state, AgentFamily::Codex).await {
                    Some(rpc) => native_queue_supported(state, &rpc).await,
                    None => false,
                }
            } else {
                false
            };
            // Send now: dispatch stops the codex CLI that has it open first.
            let held =
                !take_over_requested(state, conversation) && busy.unwrap_or(!queue_waits_for_owner);
            let mut notices = state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if sent_at.is_some() && held {
                // These are observations of this queued send, not a permanent
                // failure: every dispatch pass checks the evidence again.
                notices.insert(
                    conversation.to_owned(),
                    if busy == Some(true) {
                        CODEX_WORKING_NOTICE
                    } else {
                        CODEX_UNCONFIRMED_NOTICE
                    }
                    .to_owned(),
                );
            } else if !held
                && notices.get(conversation).is_some_and(|notice| {
                    notice == CODEX_WORKING_NOTICE || notice == CODEX_UNCONFIRMED_NOTICE
                })
            {
                notices.remove(conversation);
            }
            held
        }
        // Wonder cannot see OpenCode's own app or TUI working in a session.
        AgentFamily::OpenCode | AgentFamily::Cursor => false,
    };
    if held && waiting.is_some() {
        note_t3_reconnect(state, &stored).await;
    }
    held
}

/// A background watcher can learn revocation before the send does. Called
/// only for a held send; the actual holder must be T3 Code for that global
/// connection state to explain why this message is queued.
async fn note_t3_reconnect(state: &AppState, stored: &StoredProjectConversation) {
    if state.t3_code.connected() {
        return;
    }
    if !matches!(
        state.t3_code.unconnected(),
        crate::t3_code::Unconnected::Revoked | crate::t3_code::Unconnected::Expired
    ) {
        return;
    }
    let Some(holder) = crate::t3_code::holder(stored).await else {
        return;
    };
    let delivery = crate::t3_code::delivery_seen(
        state,
        stored,
        None,
        crate::t3_code::Seen::Read(Some(&holder)),
    )
    .await;
    if matches!(
        delivery,
        crate::t3_code::Delivery::Connect(
            crate::t3_code::Unconnected::Revoked | crate::t3_code::Unconnected::Expired
        )
    ) {
        state
            .projects
            .notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                stored.conversation_id.clone(),
                t3_code_reconnect_notice(holder.can_take_over),
            );
    }
}

/// Whether Codex is running a turn in `native`, which lives in `store`: the
/// computer's own home. Other histories are read-only.
async fn codex_held(state: &AppState, native: &str, store: &str) -> bool {
    codex_busy(state, native, store, false)
        .await
        .unwrap_or(true)
}

async fn codex_busy(
    state: &AppState,
    native: &str,
    store: &str,
    recover_runtime: bool,
) -> Option<bool> {
    let family = AgentFamily::Codex;
    // A home Wonder no longer has holds nothing; dispatch refuses it and says why.
    let Some(home) = crate::providers::home_for_store(state, family, store).await else {
        return Some(false);
    };
    let mut runtime = crate::providers::live_rpc_for_store(state, family, store).await;
    if runtime.is_none() && recover_runtime {
        // The queue gate runs before prepare_dispatch, which normally starts
        // the runtime. A stopped runtime is not evidence of a desktop writer
        // and must not prevent that normal startup/discovery indefinitely.
        runtime = crate::providers::rpc_for_store(state, family, store)
            .await
            .ok();
    }
    crate::desktop_activity::codex_thread_busy(&home, native, runtime.as_ref()).await
}

/// Whether a message to this Codex thread goes through ChatGPT's desktop app,
/// which has it open (see `codex_desktop_ipc`). Only the Mac's own home:
/// ChatGPT never runs an added account's threads.
pub(crate) async fn relays_to_desktop(
    state: &AppState,
    family: AgentFamily,
    native: &str,
    store: &str,
) -> bool {
    if !crate::providers::is_default_store(state, family, store) {
        return false;
    }
    match crate::providers::home_for_store(state, family, store).await {
        Some(home) => crate::codex_desktop_ipc::may_deliver(&home, native).await,
        None => false,
    }
}

/// Whether the relay can take `message` into the thread ChatGPT's desktop app
/// has open. A move to another provider leaves that thread for a
/// session Wonder's runtime starts from its history, which must not begin
/// while ChatGPT runs a turn in it: those wait for that turn (`held_on_mac`).
async fn relay_carries(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    family: AgentFamily,
) -> bool {
    !state
        .store
        .project_message_target(&message.id)
        .await
        .ok()
        .flatten()
        .and_then(|target| target.family)
        .is_some_and(|target| target != family)
}

/// Phone sends to a Codex thread ChatGPT's desktop app has open go into it
/// there: a connection to its owner, and the turn it is running, if any.
/// `None` when the relay does not apply or found no owner while the thread
/// is idle: the usual path decides. While ChatGPT runs a turn the relay could
/// not reach, the message waits for that turn as before (`HELD_ON_MAC`).
async fn desktop_relay(
    state: &AppState,
    family: AgentFamily,
    store: &str,
    thread: &str,
) -> Result<Option<DesktopRelay>, String> {
    if !relays_to_desktop(state, family, thread, store).await {
        return Ok(None);
    }
    let Some(home) = crate::providers::home_for_store(state, family, store).await else {
        return Ok(None);
    };
    let running = crate::desktop_activity::codex_thread_activity(&home, thread)
        .await
        .filter(|activity| activity.working())
        .map(|activity| activity.turn);
    match crate::codex_desktop_ipc::open(&home, thread).await {
        Ok(session) => Ok(Some(DesktopRelay { session, running })),
        Err(refusal) => {
            crate::codex_desktop_ipc::refuse(thread);
            let _ = state.logger.record(
                "info",
                "codex_desktop_relay_unavailable",
                json!({"threadId": thread, "refusal": format!("{refusal:?}")}),
            );
            if running.is_some() {
                Err(HELD_ON_MAC.into())
            } else {
                Ok(None)
            }
        }
    }
}

pub(crate) struct DesktopRelay {
    session: crate::codex_desktop_ipc::Session,
    /// The turn ChatGPT is running, which the message joins.
    running: Option<String>,
}

/// The generation recorded for a turn ChatGPT's desktop app runs: never a
/// live runtime of Wonder's, so recovery reads that app's own lifecycle.
const DESKTOP_RELAY_GENERATION: &str = "chatgpt-desktop";
const DESKTOP_RELAY_UNCONFIRMED: &str = "Cupola couldn't confirm ChatGPT on your computer received this, so it won't send it again. Check this thread in ChatGPT.";

const DESKTOP_RELAY_STALE: &str = "ChatGPT on your computer stopped reporting on this reply, so Cupola can't tell how it ended and won't send it again. Check this thread in ChatGPT.";

/// A message that went through ChatGPT's desktop app (its journal says so),
/// which then runs its turn.
struct DesktopRelayed {
    /// When it was handed to ChatGPT, in ms since the epoch.
    at_ms: Option<i64>,
}

async fn desktop_relayed(state: &AppState, message: &str) -> Option<DesktopRelayed> {
    let context = state
        .store
        .dispatch_context(message)
        .await
        .ok()
        .flatten()
        .and_then(|context| serde_json::from_str::<Value>(&context).ok())?;
    (context["deliveryMethod"] == DESKTOP_RELAY_METHOD).then(|| DesktopRelayed {
        at_ms: context["relayedAt"]
            .as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp_millis()),
    })
}
const DESKTOP_RELAY_METHOD: &str = "chatgpt-desktop-ipc";

/// How long a relayed turn may go with no word from ChatGPT (no lifecycle
/// record, no rollout write, nothing since the relay) before it counts as
/// lost: ChatGPT quit mid-turn, or never wrote the turn it acknowledged.
const DESKTOP_RELAY_QUIET: Duration = Duration::from_secs(15 * 60);

/// Since ChatGPT last showed signs of the turn: its rollout's last write or
/// the relay itself, whichever is later. Neither known: no sign at all.
fn desktop_relay_quiet(thread: &str, relayed: &DesktopRelayed) -> Duration {
    let rollout = crate::desktop_activity::codex_rollout(thread)
        .and_then(|path| std::fs::metadata(path).ok()?.modified().ok())
        .map(|modified| modified.elapsed().unwrap_or_default());
    let relay = relayed
        .at_ms
        .map(|at| Duration::from_millis((Utc::now().timestamp_millis() - at).max(0) as u64));
    rollout
        .into_iter()
        .chain(relay)
        .min()
        .unwrap_or(Duration::MAX)
}

/// What recovery does with a turn ChatGPT's desktop app runs.
#[derive(Debug, PartialEq, Eq)]
enum RelayedTurn {
    Running,
    Ended(&'static str),
    /// No word from ChatGPT for `DESKTOP_RELAY_QUIET`: how it ended is unknown.
    Lost,
}

/// ChatGPT's lifecycle record decides first: it reads `inProgress` past a
/// quiet spell only while ChatGPT's Codex process still writes the rollout.
/// Without it, Wonder's own reading of the shared rollout (`native`: status,
/// and whether its end was recorded) settles a finished turn. Wonder reads
/// another process's unfinished turn as interrupted with no recorded end, so
/// that ends it only after the owner's Stop; an interruption with its abort
/// recorded was a Stop there. Otherwise the turn runs until ChatGPT has been
/// quiet too long.
fn relayed_turn(
    desktop: Option<&'static str>,
    native: Option<(&str, bool)>,
    stop_requested: bool,
    quiet: Duration,
) -> RelayedTurn {
    match (desktop, native) {
        (Some("inProgress"), _) => RelayedTurn::Running,
        (Some(status @ ("completed" | "failed" | "interrupted")), _) => RelayedTurn::Ended(status),
        (_, Some(("completed", _))) => RelayedTurn::Ended("completed"),
        (_, Some(("failed", _))) => RelayedTurn::Ended("failed"),
        (_, Some(("interrupted", ended))) if ended || stop_requested => {
            RelayedTurn::Ended("interrupted")
        }
        _ if quiet < DESKTOP_RELAY_QUIET => RelayedTurn::Running,
        _ => RelayedTurn::Lost,
    }
}

/// Stop for a turn ChatGPT's desktop app runs goes to that app, which
/// interrupts only that turn; Wonder's own runtime is not running it. `None`
/// for any other turn.
pub(crate) async fn stop_desktop_relayed(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    thread: &str,
    turn: &str,
) -> Option<Response> {
    use crate::codex_desktop_ipc::Stop;
    desktop_relayed(state, &message.id).await?;
    const UNREACHABLE: &str =
        "Cupola can't reach ChatGPT on your computer right now. Stop it in ChatGPT there.";
    let conversation = state
        .store
        .project_conversation(&message.conversation_id)
        .await
        .ok()
        .flatten();
    let home = match conversation {
        Some(conversation) => {
            crate::providers::home_for_store(
                state,
                conversation.family,
                &conversation.provider_store,
            )
            .await
        }
        None => None,
    };
    let Some(home) = home else {
        return Some(error(StatusCode::CONFLICT, UNREACHABLE));
    };
    // Saved before ChatGPT is asked, so a restart in between still reads the
    // turn as the owner's Stop.
    let Ok(own) = request_stop(state, thread, turn).await else {
        return Some(error(StatusCode::SERVICE_UNAVAILABLE, STOP_NOT_SAVED));
    };
    let stopped = match crate::codex_desktop_ipc::open(&home, thread).await {
        Ok(mut session) => session.interrupt(turn).await,
        Err(refusal) => Stop::NotSent(refusal),
    };
    if own && !matches!(stopped, Stop::Stopping) {
        clear_stop(state, thread, turn).await;
    }
    let _ = state.logger.record(
        "info",
        "codex_desktop_relay_stop",
        json!({"messageId": message.id, "threadId": thread, "outcome": format!("{stopped:?}")}),
    );
    Some(match stopped {
        // ChatGPT's own record of the interrupted turn settles the message.
        Stop::Stopping => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(json!({ "stopConfirmed": false })),
        )
            .into_response(),
        Stop::NotRunning => StatusCode::NO_CONTENT.into_response(),
        Stop::NotSent(_) => error(StatusCode::CONFLICT, UNREACHABLE),
        Stop::Unknown(_) => error(
            StatusCode::BAD_GATEWAY,
            "ChatGPT on your computer didn't confirm Stop. Check this thread in ChatGPT there.",
        ),
    })
}

/// The status ChatGPT's desktop app records for a turn it ran, when its
/// lifecycle record is readable and names that turn.
async fn desktop_turn_status(
    state: &AppState,
    conversation: &StoredProjectConversation,
    thread: &str,
    turn: &str,
) -> Option<&'static str> {
    let home =
        crate::providers::home_for_store(state, conversation.family, &conversation.provider_store)
            .await?;
    crate::desktop_activity::codex_thread_activity(&home, thread)
        .await
        .filter(|activity| activity.turn == turn)
        .map(|activity| activity.status)
}

/// Whether a waiting message must stay queued: the Mac holds the chat (see
/// `held_on_mac`), or Wonder's own Claude runtime is still running a reply in
/// it. The runtime is the authority for the second: a reply it runs can
/// outlive the message that started it (a Guide answered after the reply it
/// steered), and sending into it would be refused. Production sends ask
/// `send_waits`; tests read the whole hold here.
#[cfg(test)]
pub(crate) async fn held(
    state: &AppState,
    conversation: &str,
    waiting: Option<&wonder_store::StoredMessage>,
) -> bool {
    state.projects.is_rewinding(conversation)
        || held_on_mac(state, conversation, waiting).await
        || claude_reply_running(state, conversation).await
}

/// Whether a waiting message stays queued when sent: `held`, unless T3 Code
/// has the thread open and takes the message into it instead
/// (`deliver_through_t3_code`). Only the send path (the dispatch loop, the
/// send receipt and the queue list) asks this; the thread stays held for
/// everything else. A thread nothing on the Mac holds waits only when T3
/// Code owns it and runs but can't take the message now
/// (`Delivery::waits_unheld`); any other goes at once.
/// `waiting` is the message being dispatched, or the conversation's next
/// one when `None`.
pub(crate) async fn send_waits(
    state: &AppState,
    conversation: &str,
    waiting: Option<&wonder_store::StoredMessage>,
) -> bool {
    if state.projects.is_rewinding(conversation) || claude_reply_running(state, conversation).await
    {
        return true;
    }
    let held = held_on_mac(state, conversation, waiting).await;
    let Ok(Some(stored)) = state.store.project_conversation(conversation).await else {
        return held;
    };
    let t3_only = crate::t3_code::feed::bound_thread(state, &stored)
        .await
        .is_some();
    if !held && !t3_only && !state.t3_code.connected() {
        return false;
    }
    let delivery =
        crate::t3_code::delivery(state, &stored, waiting.map(|message| message.id.as_str())).await;
    if held || t3_only {
        !delivery.sends()
    } else {
        delivery.waits_unheld()
    }
}

/// Wonder's Claude runtime reports a reply running in this chat's session.
/// Reads only, and only from a runtime that is already running.
pub(crate) async fn claude_reply_running(state: &AppState, conversation: &str) -> bool {
    let Ok(Some(binding)) = state.store.runtime_binding(conversation).await else {
        return false;
    };
    if binding.family != AgentFamily::Claude
        || binding.execution_scope != wonder_store::EXECUTION_SCOPE_PROJECTS
    {
        return false;
    }
    // Only this computer's current history can have a live runtime.
    let rpc = match state.store.project_conversation(conversation).await {
        Ok(Some(stored)) => {
            crate::providers::live_rpc_for_store(state, AgentFamily::Claude, &stored.provider_store)
                .await
        }
        _ => live_rpc_for(state, AgentFamily::Claude).await,
    };
    let Some(rpc) = rpc else {
        return false;
    };
    result(&rpc, "thread/read", json!({"threadId": binding.thread_id}))
        .await
        .is_ok_and(|read| {
            read.pointer("/thread/id").and_then(Value::as_str) == Some(binding.thread_id.as_str())
                && read.pointer("/thread/status/type").and_then(Value::as_str) == Some("active")
        })
}

/// A message's send time in ms since the epoch. An unreadable one counts as
/// now, so a reply already running on the Mac is older than it.
fn sent_at_ms(created_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|at| at.timestamp_millis())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis())
}

/// A dispatch that found the chat held on the Mac after all (Claude there
/// started a reply after the message was sent, or the chat opened in a
/// terminal). The message is not failed: its claim is released and it waits
/// like any held message.
const HELD_ON_MAC: &str = "\u{0}held-on-mac";

/// Send now on a chat another app on the Mac holds (`takeOver` on a send):
/// its waiting messages stop waiting for the Mac, and the next dispatch ends
/// that app's process for the chat before sending.
pub(crate) fn request_take_over(state: &AppState, conversation: &str, message: &str) {
    state
        .projects
        .takeovers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            conversation.to_owned(),
            (message.to_owned(), Instant::now()),
        );
    // A failed earlier attempt's explanation no longer applies.
    let mut notices = state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if notices
        .get(conversation)
        .is_some_and(|notice| notice.ends_with(TAKE_OVER_FAILED_SUFFIX))
    {
        notices.remove(conversation);
    }
}

/// Send now on a thread T3 Code takes messages into: `message` goes into
/// `turn` (the phone's turn ID) while it runs, rather than after it.
pub(crate) fn request_t3_steer(state: &AppState, message: &str, turn: &str) {
    let mut steers = state
        .projects
        .t3_steers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    steers.retain(|_, (_, at)| at.elapsed() < TAKE_OVER_WINDOW);
    steers.insert(message.to_owned(), (turn.to_owned(), Instant::now()));
}

/// The turn `message` asked to go into, recently enough to still mean it.
/// Asking clears it.
fn take_t3_steer(state: &AppState, message: &str) -> Option<String> {
    state
        .projects
        .t3_steers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(message)
        .filter(|(_, at)| at.elapsed() < TAKE_OVER_WINDOW)
        .map(|(turn, _)| turn)
}

/// Take over on a thread T3 Code takes messages into: the conversation's
/// next waiting message restarts T3 Code's turn there.
pub(crate) fn request_t3_restart(state: &AppState, conversation: &str) {
    let mut restarts = state
        .projects
        .t3_restarts
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    restarts.retain(|_, at| at.elapsed() < TAKE_OVER_WINDOW);
    restarts.insert(conversation.to_owned(), Instant::now());
}

/// Whether the conversation's Take over still waits for a message to
/// restart T3 Code's turn with. Asking clears it.
fn take_t3_restart(state: &AppState, conversation: &str) -> bool {
    state
        .projects
        .t3_restarts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(conversation)
        .is_some_and(|at| at.elapsed() < TAKE_OVER_WINDOW)
}

fn take_over_requested(state: &AppState, conversation: &str) -> bool {
    take_over_request(state, conversation).is_some()
}

/// The conversation's pending Send now, as `end_take_over` matches it.
fn take_over_request(state: &AppState, conversation: &str) -> Option<(String, Instant)> {
    state
        .projects
        .takeovers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(conversation)
        .filter(|(_, at)| at.elapsed() < TAKE_OVER_WINDOW)
        .cloned()
}

/// A Send now is a decision about the app holding the chat when it was made;
/// one that could not be acted on soon is dropped rather than applied later.
const TAKE_OVER_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Clears `request` once a dispatch acted on it. A newer Send now made
/// meanwhile is kept for the dispatch that follows.
fn end_take_over(state: &AppState, conversation: &str, request: &(String, Instant)) {
    let mut takeovers = state
        .projects
        .takeovers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if takeovers.get(conversation) == Some(request) {
        takeovers.remove(conversation);
    }
}

const TAKE_OVER_FAILED_SUFFIX: &str = "Your message is still queued.";

/// Claude Code keeps an open chat's conversation in memory: it neither sees a
/// turn written by Wonder nor continues after it, and two processes writing
/// one session fork it. So with Claude app sync on, Wonder ends Claude's
/// desktop app process for the chat before sending, even mid-reply (its text so far is kept); the app
/// continues from the transcript, with Wonder's turn, when the chat is next
/// used there. Background work, a terminal, another Agent SDK app (T3 Code)
/// and a reply the Mac started after the message hold the message until the
/// owner chooses Send now, which ends them too.
/// Claude's runtime reads the transcript again before continuing a chat
/// another process wrote meanwhile (see `transcriptMoved` in the bridge).
async fn take_over_from_desktop(
    state: &AppState,
    native: &str,
    message: &wonder_store::StoredMessage,
) -> Result<(), String> {
    use crate::desktop_activity::StopRefusal;
    if let Some(request) = take_over_request(state, &message.conversation_id) {
        let holder = crate::desktop_activity::claude_holder(native).await;
        let result = crate::desktop_activity::take_over_claude_session(native).await;
        return taken_over(state, message, &request, holder, "Claude", result);
    }
    let sent_at = sent_at_ms(&message.created_at);
    // Claude app sync off: Cupola never stops the Claude app on its own, so a
    // chat open there waits like one open in a terminal.
    if !crate::agent_settings::claude_app_sync(state).await {
        return match crate::desktop_activity::claude_holds_session(native, Some(sent_at), false)
            .await
        {
            None => Ok(()),
            Some(_) => Err(HELD_ON_MAC.into()),
        };
    }
    match crate::desktop_activity::end_desktop_session(native, sent_at).await {
        Ok(()) => Ok(()),
        Err(
            StopRefusal::Busy
            | StopRefusal::Protected
            | StopRefusal::Unknown
            | StopRefusal::Background
            | StopRefusal::Unverified
            | StopRefusal::Shared,
        ) => Err(HELD_ON_MAC.into()),
        Err(StopRefusal::StillRunning) => Err(
            "Claude on your computer didn't close this chat, so its running tasks are still going. Stop them or close the chat in Claude on your computer, then send again."
                .into(),
        ),
    }
}

/// The end of a Send now: the dispatch continues once the app on the Mac
/// that held the chat stopped. Otherwise the message waits as before, and
/// the phone says why (in the chat's notice) without naming internals.
fn taken_over(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    request: &(String, Instant),
    holder: Option<crate::desktop_activity::Holder>,
    provider: &str,
    result: Result<(), crate::desktop_activity::StopRefusal>,
) -> Result<(), String> {
    use crate::desktop_activity::StopRefusal;
    end_take_over(state, &message.conversation_id, request);
    let app = holder.map_or_else(|| provider.to_owned(), |holder| holder.app);
    let reason = match result {
        Ok(()) => {
            let _ = state.logger.record(
                "info",
                "desktop_take_over",
                json!({"conversationId": message.conversation_id}),
            );
            return Ok(());
        }
        Err(StopRefusal::Unverified) => format!(
            "Cupola couldn't confirm which process on your computer has this chat open in {app}, so it stopped nothing."
        ),
        Err(StopRefusal::Shared) => format!(
            "{app} on your computer runs other threads in the same process, so Cupola can't stop just this one. Stop the reply in {app}."
        ),
        Err(StopRefusal::Busy) => format!("{app} opened this chat again on your computer."),
        Err(StopRefusal::Unknown) => {
            "Cupola couldn't read which apps on your computer have this chat open.".to_owned()
        }
        Err(StopRefusal::StillRunning | StopRefusal::Protected | StopRefusal::Background) => {
            format!("{app} on your computer didn't stop.")
        }
    };
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            message.conversation_id.clone(),
            format!("{reason} {TAKE_OVER_FAILED_SUFFIX}"),
        );
    let _ = state.logger.record(
        "info",
        "desktop_take_over_refused",
        json!({"conversationId": message.conversation_id, "refusal": format!("{result:?}")}),
    );
    Err(HELD_ON_MAC.into())
}

/// With Claude app sync on: after Wonder's turn, a Claude desktop process
/// opened for the chat during it still holds the conversation from before; closing it while idle lets
/// the app continue from the transcript instead of branching.
pub(crate) fn release_to_desktop(state: &AppState, conversation: &str) {
    let state = state.clone();
    let conversation = conversation.to_owned();
    tokio::spawn(async move {
        let Ok(Some(stored)) = state.store.project_conversation(&conversation).await else {
            return;
        };
        if stored.family != AgentFamily::Claude
            || !crate::agent_settings::claude_app_sync(&state).await
        {
            return;
        }
        if let Some(native) = stored.native_session_id.as_deref() {
            crate::desktop_activity::close_idle_desktop_session(native).await;
        }
    });
}

/// `deliveryMethod` of a dispatch intent sent with T3 Code's `t3_thread_send`.
const T3_CODE_SEND: &str = "t3_thread_send";
const T3_CODE_UNCONFIRMED_NOTICE: &str =
    "Cupola is checking whether T3 Code on your computer got your message. It won't send it twice.";
const T3_CODE_RECHECK_NOTICE: &str = "Connect T3 Code again in Settings so Cupola can check whether T3 Code got your message. It won't send it twice.";
const T3_CODE_UNRESOLVED_NOTICE: &str =
    "Cupola couldn't confirm T3 Code got your message. Check this thread in T3 Code on your computer.";

fn t3_code_reconnect_notice(can_take_over: bool) -> String {
    let take_over = if can_take_over {
        ", or touch and hold Send to take over"
    } else {
        ""
    };
    format!("T3 Code no longer accepts Cupola. Connect T3 Code again in Settings{take_over}. Your message is queued.")
}

/// A thread T3 Code knows (open there or not), with Cupola connected to T3
/// Code and T3 Code running: the message goes into the T3 thread as plain
/// text (with its files), and nothing on the Mac is stopped. It queues behind a turn running there,
/// steers it (Send now, while the turn it was sent into is still the
/// thread's newest; otherwise it queues, `request_t3_steer`) or restarts it
/// (Take over: the next waiting message goes with it, `request_t3_restart`).
/// The intent (the T3 thread, the mode, the file names and the message's ID
/// as T3 Code's `clientRequestId`) is durable before the call, like any
/// submission: a restart or a lost answer leaves the message uncertain,
/// never sent again blindly, and recovery reads the T3 thread for it
/// (`reconcile_t3_code`); an answer adds T3 Code's own ID for the message
/// (`t3_code::reconcile_sends`). When T3 Code refuses it, the message waits as
/// without T3 Code; when T3 Code isn't running and nothing on the Mac has
/// the thread open, Cupola runs the reply itself. Returns whether this
/// dispatch is finished; `false` sends it the usual way.
async fn deliver_through_t3_code(state: &AppState, message: &wonder_store::StoredMessage) -> bool {
    let Ok(Some(stored)) = state
        .store
        .project_conversation(&message.conversation_id)
        .await
    else {
        return false;
    };
    let t3_only = crate::t3_code::feed::bound_thread(state, &stored)
        .await
        .is_some();
    // T3-only chats keep their queue even after sign-out or an ownership
    // reset. Their runtime ID is a history identity, never a host session.
    if !state.t3_code.connected() {
        if t3_only {
            t3_only_waiting(
                state,
                &message.conversation_id,
                "Connect T3 Code in Settings to send this message. Your message is queued.",
            );
        }
        return t3_only;
    }
    if !crate::providers::is_default_store(state, stored.family, &stored.provider_store) {
        return t3_only;
    }
    // OpenCode and Cursor chats have no native session: T3 Code's thread is
    // the conversation (recorded as `t3:<thread>`), and Cupola can't run the
    // reply itself.
    let t3_native = crate::t3_code::native_thread(state, &stored).await;
    let Some(native) = stored
        .native_session_id
        .clone()
        .or_else(|| t3_native.as_ref().map(|thread| format!("t3:{thread}")))
    else {
        if t3_only {
            t3_only_waiting(state, &message.conversation_id, "T3 Code no longer has this chat. Check it in T3 Code on your computer. Your message is queued.");
        }
        return t3_only;
    };
    let thread = match crate::t3_code::delivery(state, &stored, Some(&message.id)).await {
        crate::t3_code::Delivery::Sends(thread) => thread,
        // T3 Code can't take it after all (a file, or it just refused one):
        // a thread it holds keeps the message waiting, as does one nothing
        // holds that T3 Code owns while it runs (`Delivery::waits_unheld`);
        // the claim stays and the dispatch loop releases it.
        delivery => {
            if t3_only {
                let notice = match delivery {
                    crate::t3_code::Delivery::NotFound(crate::t3_code::Unrouted::Unmatched) =>
                        "T3 Code no longer has this chat. Check it in T3 Code on your computer. Your message is queued.",
                    _ => "T3 Code can't take this message right now. Your message is queued; Cupola tries again when T3 Code is available.",
                };
                t3_only_waiting(state, &message.conversation_id, notice);
                return true;
            }
            return match crate::t3_code::holder(&stored).await {
                Some(_) => held_on_mac(state, &message.conversation_id, Some(message)).await,
                None => delivery.waits_unheld(),
            };
        }
    };
    // The model and effort it runs with, which T3 Code switches to first
    // where its thread differs.
    let chosen = crate::t3_code::chosen(state, &stored, &message.id).await;
    let steer = take_t3_steer(state, &message.id);
    let mode = if take_t3_restart(state, &message.conversation_id) {
        crate::t3_code::SendMode::Restart
    } else if let Some(turn) = &steer {
        // Guidance for a reply that ended meanwhile steers neither it nor the
        // next one: the message goes after it instead.
        if latest_native_turn(state, &message.conversation_id)
            .await
            .as_deref()
            == Some(turn)
        {
            crate::t3_code::SendMode::Steer
        } else {
            crate::t3_code::SendMode::Queue
        }
    } else {
        crate::t3_code::SendMode::Queue
    };
    let files = state
        .store
        .attachments_for_message(&message.id)
        .await
        .unwrap_or_default();
    // T3 Code takes files only into the running turn: a message with files
    // sent normally waits here for that turn to end, as Send promises.
    if !files.is_empty()
        && mode == crate::t3_code::SendMode::Queue
        && t3_turn_running(state, &stored, &native).await
    {
        return t3_only || held_on_mac(state, &message.conversation_id, Some(message)).await;
    }
    // Uploads come before the intent: until the message itself is sent,
    // nothing can be in the T3 thread, and the IDs T3 Code gives these
    // uploads are what recovery looks for there.
    let uploaded = if files.is_empty() {
        Ok(Vec::new())
    } else {
        match t3_code_uploads(state, &message.conversation_id, files).await {
            Ok(uploads) => state.t3_code.upload(&native, uploads).await,
            Err(detail) => Err(crate::t3_code::CallError::Refused {
                code: "file_unreadable".into(),
                message: detail,
            }),
        }
    };
    // Uploads take a while and T3 Code's reply may have changed meanwhile,
    // so the turn is checked again right before the send: guidance whose
    // reply ended goes after it, and files sent normally wait for a reply
    // that started. T3 Code has no send conditional on its turn, so one T3
    // Code round trip remains between this check and the send.
    let mode = match (&uploaded, &steer) {
        (Ok(attachments), Some(turn))
            if !attachments.is_empty()
                && mode == crate::t3_code::SendMode::Steer
                && latest_native_turn(state, &message.conversation_id)
                    .await
                    .as_deref()
                    != Some(turn.as_str()) =>
        {
            crate::t3_code::SendMode::Queue
        }
        _ => mode,
    };
    if matches!(&uploaded, Ok(attachments) if !attachments.is_empty())
        && mode == crate::t3_code::SendMode::Queue
        && t3_turn_running(state, &stored, &native).await
    {
        return t3_only || held_on_mac(state, &message.conversation_id, Some(message)).await;
    }
    let provider_text = match crate::composer_mentions::provider_text(state, message).await {
        Ok(text) => text,
        Err(detail) => {
            let _ = state
                .store
                .update_message_delivery(&message.id, "failed", None, None)
                .await;
            state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(message.conversation_id.clone(), detail);
            publish_message_state(state, message, DeliveryState::Failed, None, None).await;
            return true;
        }
    };
    // A thread launched with a native command never got Cupola's briefing;
    // the first message after it that is not itself a command carries it.
    let briefed = t3_briefing_owed(state, message).await
        && crate::preamble::t3_first_message(&provider_text) != provider_text;
    let provider_text = if briefed {
        crate::preamble::t3_first_message(&provider_text)
    } else {
        provider_text
    };
    let sent = match uploaded {
        Ok(attachments) => {
            // No history row saved before the send is its agent's record of
            // it (`t3_code::echoes`).
            let after = state
                .store
                .conversation_history_mark(&message.conversation_id)
                .await
                .ok()
                .map(|(_, newest)| newest);
            let intent = json!({
                "deliveryMethod": T3_CODE_SEND,
                "providerText": (provider_text != message.body).then_some(&provider_text),
                "t3Briefed": briefed.then_some(true),
                "t3ThreadId": thread,
                "clientRequestId": message.id,
                "mode": mode.as_str(),
                "attachmentIds": attachments.iter().map(|file| file["id"].clone()).collect::<Vec<_>>(),
                "t3Files": attachments.iter().map(|file| json!({"type": file["type"], "name": file["name"]})).collect::<Vec<_>>(),
                "sentAt": now_text(),
                "t3After": after,
            });
            {
                let _dispatch = state
                    .update_admission
                    .dispatch_guard(&state.dispatch_lock)
                    .await;
                if state
                    .store
                    .begin_dispatch_submission_with_context(
                        &message.id,
                        &native,
                        Some(&intent.to_string()),
                    )
                    .await
                    .is_err()
                {
                    // The claim was released meanwhile; the dispatch loop decides again.
                    return true;
                }
            }
            // T3 Code runs the turn: the thread's own changes start from
            // the tree before this send and end when T3 Code's run ends.
            crate::filesystem::thread_changes::t3_sending(
                state,
                &message.conversation_id,
                &message.id,
            )
            .await;
            let sent = state
                .t3_code
                .send(
                    &native,
                    &thread,
                    &provider_text,
                    &message.id,
                    mode,
                    &attachments,
                    chosen.as_ref(),
                )
                .await;
            crate::filesystem::thread_changes::t3_sent(
                state,
                &message.conversation_id,
                &message.id,
                &thread,
                &sent,
            )
            .await;
            sent
        }
        Err(error) => Err(error),
    };
    // Send now stops T3 Code's Claude thread, not the codex app-server it
    // runs (read before taking the lock).
    let holder = match &sent {
        Err(error) if !error.unconfirmed() => Some(crate::t3_code::holder(&stored).await),
        _ => None,
    };
    let can_take_over = holder
        .as_ref()
        .is_some_and(|holder| holder.as_ref().is_some_and(|holder| holder.can_take_over));
    let unheld = matches!(holder, Some(None)) && !t3_only;
    let take_over = if can_take_over {
        ", or touch and hold Send to take over"
    } else {
        ""
    };
    let _dispatch = state.dispatch_lock.lock().await;
    let notice = match sent {
        Ok(result) => {
            // T3 Code's ID for the message: its timeline names it so once
            // T3 Code starts it (`t3_code::reconcile_sends`). One T3 Code
            // queued behind its running turn shows as queued until then
            // (`t3_code::queue`); saved before the phone hears it was sent.
            let mut learned = serde_json::Map::new();
            if let Some(id) = result.get("messageId").filter(|id| id.is_string()) {
                learned.insert("t3MessageId".into(), id.clone());
            }
            if let Some(run) = crate::t3_code::queue::queued_run(&result) {
                learned.insert("t3QueuedRunId".into(), run.into());
            }
            if !learned.is_empty() {
                let _ = state
                    .store
                    .update_t3_code_intent(&message.id, &Value::Object(learned))
                    .await;
            }
            t3_code_delivered(state, message).await;
            // What its turn asks reaches the phone (`t3_code::questions`).
            crate::t3_code::questions::watch(state, &message.conversation_id, &thread);
            let _ = state.logger.record(
                "info",
                "t3_code_delivered",
                json!({"conversationId": message.conversation_id, "messageId": message.id,
                    "delivery": result.get("delivery"), "status": result.get("status")}),
            );
            return true;
        }
        Err(error) if error.unconfirmed() => {
            // T3 Code may have it: recovery reads its thread before anything
            // else is sent.
            if state
                .store
                .update_message_delivery(&message.id, "uncertain", None, None)
                .await
                .is_ok()
            {
                publish_message_state(state, message, DeliveryState::Uncertain, None, None).await;
            }
            let _ = state.logger.record(
                "info",
                "t3_code_send_unconfirmed",
                json!({"conversationId": message.conversation_id, "error": format!("{error:?}")}),
            );
            T3_CODE_UNCONFIRMED_NOTICE.to_owned()
        }
        Err(error) => {
            // Refused before it reached the thread: it waits its turn again.
            let _ = state.store.requeue_refused_send(&message.id).await;
            // T3 Code no longer has the thread (deleted there): it isn't
            // T3 Code's any more unless T3 Code's own store says so.
            if matches!(&error, crate::t3_code::CallError::Refused { code, .. } if code == "thread_not_found")
            {
                let _ = state
                    .store
                    .forget_t3_code_thread(&message.conversation_id)
                    .await;
            }
            let _ = state.logger.record(
                "info",
                "t3_code_send_failed",
                json!({"conversationId": message.conversation_id, "error": format!("{error:?}")}),
            );
            // T3 Code isn't running and nothing on the Mac has the thread
            // open: Cupola runs the reply itself, at once (`t3_code::find`).
            if error == crate::t3_code::CallError::Offline && unheld {
                state.dispatch_wake.notify_one();
                return true;
            }
            match error {
                crate::t3_code::CallError::Unauthorized | crate::t3_code::CallError::NotConnected => {
                    t3_code_reconnect_notice(can_take_over)
                }
                crate::t3_code::CallError::Offline => {
                    "T3 Code on your computer didn't answer. Your message is queued; Cupola tries again shortly.".to_owned()
                }
                crate::t3_code::CallError::Refused { code, .. } if code == "model_unavailable" || code == "configure_not_applied" => {
                    format!("T3 Code couldn't switch this thread to the model you chose. Choose another model{take_over}. Your message is queued.")
                }
                crate::t3_code::CallError::Refused { code, .. } if code == crate::t3_code::RESTART_TIMED_OUT => {
                    "T3 Code's reply didn't stop in time, so your files weren't sent into it. Your message goes when that reply ends.".to_owned()
                }
                _ => format!("T3 Code didn't take this message. It's queued; Cupola tries again shortly{take_over}."),
            }
        }
    };
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(message.conversation_id.clone(), notice);
    true
}

/// Explain a T3-only chat's waiting send without handing it to a host runtime.
fn t3_only_waiting(state: &AppState, conversation: &str, notice: &str) {
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation.to_owned(), notice.to_owned());
}

/// Whether a turn runs in the thread T3 Code has open: its Claude session
/// is writing a reply, or its Codex thread is busy (unknown counts as busy).
/// An unreadable T3 run state is not running.
async fn t3_turn_running(
    state: &AppState,
    stored: &StoredProjectConversation,
    native: &str,
) -> bool {
    match stored.family {
        AgentFamily::Claude => crate::t3_code::holder(stored)
            .await
            .is_some_and(|holder| holder.replying),
        AgentFamily::Codex => codex_held(state, native, &stored.provider_store).await,
        AgentFamily::OpenCode | AgentFamily::Cursor => {
            match crate::t3_code::native_thread(state, stored).await {
                Some(thread) => state.t3_code.run_in_progress(&thread).await,
                None => false,
            }
        }
    }
}

/// The T3 thread a message to this conversation goes into now, when T3 Code
/// has it open and Cupola is connected (`t3_code::route`).
pub(crate) async fn t3_code_thread(state: &AppState, conversation: &str) -> Option<String> {
    if !state.t3_code.connected() {
        return None;
    }
    let stored = state
        .store
        .project_conversation(conversation)
        .await
        .ok()??;
    if !crate::providers::is_default_store(state, stored.family, &stored.provider_store) {
        return None;
    }
    crate::t3_code::route(state, &stored).await
}

/// Whether the conversation's next waiting message goes into the T3 thread
/// when dispatched (`delivery`): T3 Code knows the thread and runs, Cupola
/// is connected, and T3 Code can carry that message.
pub(crate) async fn t3_code_takes_next(state: &AppState, conversation: &str) -> bool {
    let Ok(Some(stored)) = state.store.project_conversation(conversation).await else {
        return false;
    };
    if !crate::providers::is_default_store(state, stored.family, &stored.provider_store) {
        return false;
    }
    crate::t3_code::delivery(state, &stored, None).await.sends()
}

/// Stop on a thread whose turn runs in T3 Code: T3 Code interrupts it
/// (`t3_thread_interrupt`). `None` when T3 Code does not have the thread.
/// 202 says T3 Code took the request (the turn ends there a moment later),
/// 204 that nothing was running. The Stop names T3 Code's run that is
/// active when it arrives, read before its turn is checked to still be the
/// thread's newest (409 when another turn followed, 503 when either can't
/// be read), so a run that replaced it meanwhile is never stopped.
pub(crate) async fn interrupt_through_t3_code(
    state: &AppState,
    conversation: &str,
    turn: &str,
) -> Option<Response> {
    const ENDED: &str = "That reply already ended, so nothing was stopped.";
    let thread = match t3_code_thread(state, conversation).await {
        Some(thread) => thread,
        None => {
            let stored = state
                .store
                .project_conversation(conversation)
                .await
                .ok()??;
            if crate::t3_code::feed::bound_thread(state, &stored)
                .await
                .is_some()
            {
                return Some(error(StatusCode::SERVICE_UNAVAILABLE,
                    "T3 Code can't stop this reply right now. Open T3 Code on your computer or reconnect it in Settings."));
            }
            return None;
        }
    };
    let run = match state.t3_code.active_run(&thread).await {
        Ok(Some(run)) => run,
        Ok(None) => return Some(StatusCode::NO_CONTENT.into_response()),
        Err(_) => {
            return Some(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Cupola couldn't confirm which reply is running, so it stopped nothing. Stop it in T3 Code on your computer.",
            ))
        }
    };
    match latest_native_turn(state, conversation).await {
        Some(latest) if latest == turn => {}
        Some(_) => {
            return Some(error(StatusCode::CONFLICT, ENDED))
        }
        None => {
            return Some(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Cupola couldn't confirm which reply is running, so it stopped nothing. Stop it in T3 Code on your computer.",
            ))
        }
    }
    let response = match state
        .t3_code
        .interrupt(&thread, Some(&run), &format!("stop:{turn}"))
        .await
    {
        Ok(result) if result["status"] == "interrupt_requested" => {
            (StatusCode::ACCEPTED, Json(json!({"stopConfirmed": false}))).into_response()
        }
        Ok(result) if result["status"] == "no_active_run" => StatusCode::NO_CONTENT.into_response(),
        // The run ended on its own, and another may run now.
        Ok(_) => error(StatusCode::CONFLICT, ENDED),
        Err(crate::t3_code::CallError::Refused { code, .. })
            if code == "thread_not_interruptible" || code == "run_not_found" =>
        {
            error(StatusCode::CONFLICT, ENDED)
        }
        Err(crate::t3_code::CallError::NotConnected | crate::t3_code::CallError::Unauthorized) => {
            error(
                StatusCode::CONFLICT,
                "Connect T3 Code again in Settings, or stop the reply in T3 Code on your computer.",
            )
        }
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "T3 Code didn't stop the reply. Stop it in T3 Code on your computer.",
        ),
    };
    Some(response)
}

/// The newest turn of the conversation's thread, as the phone names it
/// (`history_activity`'s `latestTurnId`), when it can be read.
pub(crate) async fn latest_native_turn(state: &AppState, conversation: &str) -> Option<String> {
    let thread = state.store.conversation_thread(conversation).await.ok()??;
    if let Some(t3_thread) = crate::t3_code::feed::t3_thread(&thread) {
        // Read T3 again after capturing its active run: a newer user turn
        // must make Stop/Guide refuse the stale phone turn, not target it.
        let items = state.t3_code.activity_items(t3_thread).await.ok()?;
        return crate::t3_code::feed::entries(&items).last()?["turnId"]
            .as_str()
            .map(str::to_owned);
    }
    let runtime = crate::claude::for_thread(state, &thread).await.ok()?;
    let response = runtime
        .request(
            "thread/turns/list",
            json!({"threadId": thread, "limit": 1, "itemsView": "notLoaded"}),
        )
        .await
        .ok()?;
    response
        .result?
        .pointer("/data/0/id")?
        .as_str()
        .map(str::to_owned)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LaunchInT3CodeRequest {
    title: String,
    #[serde(default)]
    message: String,
    folder_id: Option<String>,
    /// Start the thread in T3 Code's plan mode (`interactionMode: plan`).
    #[serde(default)]
    plan_mode: bool,
    /// Run it on exactly this provider (and `model`), or refuse.
    provider: Option<AgentFamily>,
    model: Option<String>,
    /// With `provider`: the phone's New chat request. The launched thread
    /// gets its Cupola conversation at once (`start_t3_conversation`), with
    /// the message as this device's, and a launch already made for it is
    /// answered with that conversation instead of starting another.
    device_id: Option<String>,
    client_message_id: Option<String>,
    provider_instance_id: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    runtime_mode: Option<String>,
}

/// How long a launched thread's conversation waits for its agent's session
/// to show in T3 Code's store before saying so (`bind_t3_conversation`).
const T3_BIND_WAIT: Duration = Duration::from_secs(300);

/// New thread in T3 Code: starts a thread in T3 Code on the Mac
/// (`t3_thread_launch`) in this Project's folder, on exactly the requested
/// provider and model: one Cupola can't follow there (`launch_families`),
/// that T3 Code doesn't run, or a model it doesn't offer is refused with the
/// reason before anything starts. Without a request T3 Code's default model
/// runs; a T3 project without one gets the Project's default provider (or
/// Codex) as T3 Code has it. With the phone's request (`clientMessageId`)
/// the thread's Cupola conversation exists at once and its agent's session
/// joins it when it shows (`start_t3_conversation`); otherwise the session
/// shows in the Project like any thread started on the Mac. T3 Code has no
/// retry key for a launch, so a lost answer is reported rather than retried.
pub(crate) async fn launch_in_t3_code(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<LaunchInT3CodeRequest>,
) -> Response {
    let title = request.title.trim();
    if title.is_empty() || title.len() > 200 || request.message.len() > 64 * 1024 {
        return error(
            StatusCode::BAD_REQUEST,
            "A title of up to 200 bytes and a message of up to 65536 bytes",
        );
    }
    let has_choices = request.provider_instance_id.is_some()
        || request.effort.is_some()
        || request.service_tier.is_some()
        || request.runtime_mode.is_some();
    if has_choices
        && (request.provider.is_none()
            || request
                .model
                .as_deref()
                .is_none_or(|model| model.trim().is_empty())
            || request.runtime_mode.is_none())
    {
        return error(
            StatusCode::BAD_REQUEST,
            "T3 Code choices require a provider, model and permission mode.",
        );
    }
    let sender = match (&request.device_id, &request.client_message_id) {
        (Some(device), Some(client)) => {
            if uuid::Uuid::parse_str(client).is_err() {
                return error(StatusCode::BAD_REQUEST, "clientMessageId must be a UUID");
            }
            if authenticated_device.is_some_and(|Extension(known)| &known.device_id != device) {
                return error(StatusCode::FORBIDDEN, "device identity mismatch");
            }
            Some((device.as_str(), client.as_str()))
        }
        (None, None) => None,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "deviceId and clientMessageId go together",
            )
        }
    };
    // Launched for this request already: its conversation, never a second
    // thread in T3 Code.
    if let Some((_, client)) = sender {
        match state.store.project_creation_by_request(client).await {
            Ok(Some((conversation, _))) if conversation.project_id == id => {
                return t3_launch_answer(&state, None, &conversation).await;
            }
            Ok(Some(_)) => {
                return error(
                    StatusCode::CONFLICT,
                    "This request belongs to another Project.",
                )
            }
            Ok(None) => {}
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The thread could not be checked. Try again.",
                )
            }
        }
    }
    let project = match state.store.project(&id).await {
        Ok(Some(project)) => project,
        Ok(None) => return error(StatusCode::NOT_FOUND, "Project not found"),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Project lookup failed"),
    };
    let folder = match request.folder_id.as_deref() {
        Some(folder) => project.roots.iter().find(|root| root.id == folder),
        None => Some(project.primary_root()),
    };
    let Some(folder) = folder else {
        return error(
            StatusCode::NOT_FOUND,
            "That folder is no longer in this Project.",
        );
    };
    if !state.t3_code.connected() {
        return error(
            StatusCode::CONFLICT,
            "Connect T3 Code in Settings to start threads there.",
        );
    }
    let model = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    // Exactly the chosen provider and model, or a reason it can't start.
    let provider_name = |family: AgentFamily| family.provider().display_name;
    if let Some(family) = request
        .provider
        .filter(|family| !crate::t3_code::launch_families().contains(family))
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "Cupola can't show {} threads started in T3 Code yet. Turn off Start in T3 Code to start it in Cupola, or choose Claude or Codex.",
                provider_name(family)
            ),
        );
    }
    let (model_ids, model_name) = match model {
        Some(model) => {
            let catalog = state.runtime_catalog.read().await;
            let name = catalog
                .models
                .iter()
                .find(|option| option.id == model)
                .map_or_else(|| model.to_owned(), |option| option.display_name.clone());
            (crate::t3_code::t3_model_ids(&catalog, model), Some(name))
        }
        None => (Vec::new(), None),
    };
    let requested = request
        .provider
        .map(|family| (family, model_ids.as_slice()));
    let referenced_message = match crate::composer_mentions::draft_provider_text(
        &state,
        &format!("project-files:{}:{}", project.id, folder.id),
        &request.message,
    )
    .await
    {
        Ok(text) => text,
        Err(detail) => return error(StatusCode::UNPROCESSABLE_ENTITY, detail),
    };
    // T3 Code takes no system prompt: Cupola's preamble opens the message.
    let message = if request.message.trim().is_empty() {
        request.message.clone()
    } else {
        crate::preamble::t3_first_message(&referenced_message)
    };
    // No request: the Project's last provider and the model last chosen
    // for it here, else T3 Code's own default.
    let family = project.last_family.unwrap_or_default();
    let fallback = (
        family,
        project
            .last_models
            .iter()
            .find(|last| last.family == family)
            .map(|last| last.model.as_str()),
    );
    let choices = has_choices.then_some(crate::t3_code::LaunchChoices {
        instance: request.provider_instance_id.as_deref(),
        effort: request.effort.as_deref(),
        tier: request.service_tier.as_deref(),
        runtime_mode: request.runtime_mode.as_deref(),
    });
    // A chat with no Project works in T3 Code's own scratch folder, which T3
    // Code makes (`scratch`), not in Cupola's.
    let in_scratch = project.id == SCRATCH_PROJECT_ID;
    let workspace = if in_scratch {
        crate::t3_code::LaunchWorkspace::Scratch
    } else {
        crate::t3_code::LaunchWorkspace::Folder(&folder.path)
    };
    match state
        .t3_code
        .launch(
            workspace,
            title,
            &message,
            request.plan_mode,
            requested,
            fallback,
            choices,
        )
        .await
    {
        Ok(result) => {
            let thread = result.get("threadId").and_then(Value::as_str);
            // Where T3 Code put the thread: its scratch folder when the chat
            // has no Project, else the chosen folder.
            let cwd = match (in_scratch, result.get("projectId").and_then(Value::as_str)) {
                (true, Some(scratch)) => state.t3_code.workspace_root(scratch).await,
                _ => None,
            };
            let started = match (sender, request.provider, thread) {
                (Some(sender), Some(family), Some(thread)) => {
                    start_t3_conversation(
                        &state,
                        &project,
                        T3Launched {
                            cwd: cwd.as_deref().unwrap_or(&folder.path),
                            family,
                            model,
                            choices,
                            plan: request.plan_mode,
                            title,
                            body: &request.message,
                            provider_text: (referenced_message != request.message).then_some(message.as_str()),
                            briefing_owed: !request.message.trim().is_empty()
                                && message == referenced_message,
                            sender,
                            thread,
                        },
                    )
                    .await
                }
                _ => None,
            };
            match started {
                Some(conversation) => t3_launch_answer(&state, thread, &conversation).await,
                None => (
                    StatusCode::CREATED,
                    Json(json!({"threadId": result.get("threadId")})),
                )
                    .into_response(),
            }
        }
        Err(crate::t3_code::CallError::Refused { code, .. }) if code == "project_not_found" => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "T3 Code doesn't have this folder as a project. Add it in T3 Code on your computer first.",
        ),
        Err(crate::t3_code::CallError::Refused { code, .. }) if code == "provider_unavailable" => {
            let name = request.provider.map_or("This provider", provider_name);
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("T3 Code on your computer doesn't run {name}. Turn it on in T3 Code's settings, or start this chat in Cupola."),
            )
        }
        Err(crate::t3_code::CallError::Refused { code, .. }) if code == "model_unavailable" => {
            let name = request.provider.map_or("this provider", provider_name);
            let model = model_name.as_deref().unwrap_or("the chosen model");
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("T3 Code doesn't offer {model} for {name}. Choose another model, or start this chat in Cupola."),
            )
        }
        Err(crate::t3_code::CallError::Refused { code, .. }) if code == "no_provider" => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "T3 Code has no provider set up. Set one up in T3 Code on your computer first.",
        ),
        Err(crate::t3_code::CallError::NotConnected | crate::t3_code::CallError::Unauthorized) => {
            error(
                StatusCode::CONFLICT,
                "T3 Code no longer accepts Cupola. Connect T3 Code again in Settings.",
            )
        }
        Err(crate::t3_code::CallError::Offline) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "T3 Code isn't running on your computer. Open it and try again.",
        ),
        Err(error) if error.unconfirmed() => self::error(
            StatusCode::GATEWAY_TIMEOUT,
            "T3 Code may have started the thread. Check T3 Code on your computer before trying again.",
        ),
        Err(crate::t3_code::CallError::Refused { message, .. }) if !message.is_empty() => {
            error(StatusCode::UNPROCESSABLE_ENTITY, format!("T3 Code didn't start the thread: {message}"))
        }
        Err(_) => error(
            StatusCode::BAD_GATEWAY,
            "T3 Code didn't start the thread. Try again.",
        ),
    }
}

/// Shown on a launched thread's conversation until its agent's session
/// joins it (`bind_t3_conversation`).
const T3_STARTING_NOTICE: &str = "Starting in T3 Code on your computer…";
const T3_UNBOUND_NOTICE: &str = "T3 Code started this thread, but its session hasn't shown up here yet. Check it in T3 Code on your computer.";
const T3_ELSEWHERE_NOTICE: &str =
    "T3 Code's replies for this thread are in another chat in this Project, opened from Chats.";

/// Whether the thread's launch message went without Cupola's briefing (a
/// native command had to lead it) and no later message has carried it yet.
/// A message's own earlier attempt does not count, so a retry sends the
/// same text.
pub(crate) async fn t3_briefing_owed(
    state: &AppState,
    message: &wonder_store::StoredMessage,
) -> bool {
    let Ok(sends) = state
        .store
        .t3_code_sent_messages(&message.conversation_id)
        .await
    else {
        return false;
    };
    let flag = |send: &wonder_store::T3CodeSend, key: &str| {
        serde_json::from_str::<Value>(&send.intent)
            .is_ok_and(|intent| intent[key].as_bool() == Some(true))
    };
    sends.iter().any(|send| flag(send, "t3BriefingOwed"))
        && !sends
            .iter()
            .any(|send| send.id != message.id && flag(send, "t3Briefed"))
}

/// A thread T3 Code launched for the phone's New chat request.
struct T3Launched<'a> {
    /// The thread's working folder: the Project's folder, or T3 Code's
    /// scratch folder for a chat with no Project.
    cwd: &'a str,
    provider_text: Option<&'a str>,
    /// The first message was a native command, so it went without Cupola's
    /// briefing (`t3_briefing_owed`).
    briefing_owed: bool,
    family: AgentFamily,
    model: Option<&'a str>,
    choices: Option<crate::t3_code::LaunchChoices<'a>>,
    plan: bool,
    title: &'a str,
    body: &'a str,
    /// The phone's (device, client message ID).
    sender: (&'a str, &'a str),
    thread: &'a str,
}

/// The launch's answer: T3 Code's thread and the conversation the phone opens.
async fn t3_launch_answer(
    state: &AppState,
    thread: Option<&str>,
    conversation: &StoredProjectConversation,
) -> Response {
    let summary = single_summary(
        state,
        conversation,
        activity_seconds(&conversation.last_activity_at),
        None,
    )
    .await;
    (
        StatusCode::CREATED,
        Json(
            json!({"threadId": thread, "conversationId": conversation.conversation_id,
            "conversation": summary}),
        ),
    )
        .into_response()
}

/// The launched thread's Cupola conversation, made at once so the phone
/// opens it: the chosen provider and model, the typed message as delivered
/// to T3 Code (its intent pairs it with the agent's record of it, as for any
/// message sent through T3 Code), and "Starting in T3 Code…" until the
/// agent's session joins it (`bind_t3_conversation`). Keyed by the phone's
/// request, so asking again answers this conversation. None when it can't
/// be saved: the thread then shows in Chats once its agent starts.
async fn start_t3_conversation(
    state: &AppState,
    project: &StoredProject,
    launched: T3Launched<'_>,
) -> Option<StoredProjectConversation> {
    let family = launched.family;
    let (device, client) = launched.sender;
    let now = now_text();
    // T3 already validated these choices against its own catalog. Native
    // host defaults must not replace or fill them when recording the chat.
    let resolved = if let Some(choices) = launched.choices {
        // The phone names the model by T3 Code's id ("claude-opus-5-5");
        // the chat records Cupola's id for it ("claude:opus") so every
        // surface shows the name Cupola's own chats show.
        let catalog = state.runtime_catalog.read().await;
        agent_defaults::Resolved {
            model: launched.model.map(|native| {
                crate::native_settings::catalog_model(&catalog, family, native)
                    .unwrap_or(native)
                    .to_owned()
            }),
            effort: choices.effort.map(str::to_owned),
            service_tier: choices.tier.map(str::to_owned),
        }
    } else {
        agent_defaults::resolve(
            &*state.runtime_catalog.read().await,
            family,
            launched.model,
            None,
            None,
        )
    };
    let runtime_mode = launched.choices.and_then(|choices| choices.runtime_mode);
    let approval = match runtime_mode {
        Some("approval-required") => "ask",
        Some("auto-accept-edits") => "accept_edits",
        Some("auto") => "auto",
        _ => agent_defaults::claude_approval(family),
    };
    let approval = if family.provider().approval_modes.contains(&approval) {
        approval
    } else {
        agent_defaults::claude_approval(family)
    };
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let store = provider_store(state, family).to_owned();
    let created = state
        .store
        .create_project_conversation(ProjectConversationInsert {
            conversation_id: &conversation_id,
            project_id: &project.id,
            family,
            provider_store: &store,
            native_session_id: None,
            cwd: launched.cwd,
            roots_revision: project.roots_revision,
            title: launched.title,
            model: resolved.model.as_deref(),
            effort: resolved.effort.as_deref(),
            service_tier: resolved.service_tier.as_deref(),
            access_mode: if runtime_mode == Some("full-access") {
                "full_access"
            } else {
                "workspace"
            },
            claude_approval: approval,
            plan_mode: launched.plan,
            creation_request_id: Some(client),
            now: &now,
        })
        .await
        .ok()?;
    let conversation = match created {
        ProjectConversationCreate::Created(c) | ProjectConversationCreate::Existing(c) => c,
    };
    let id = conversation.conversation_id.clone();
    if !launched.body.trim().is_empty() {
        let after = state
            .store
            .conversation_history_mark(&id)
            .await
            .map_or(0, |(_, newest)| newest);
        let intent = json!({
            "deliveryMethod": T3_CODE_SEND,
            "t3ThreadId": launched.thread,
            "clientRequestId": client,
            "mode": "queue",
            "attachmentIds": [],
            "t3Files": [],
            "sentAt": now,
            "t3After": after,
            "t3Started": true,
            "t3Launch": true,
            "t3BriefingOwed": launched.briefing_owed.then_some(true),
            "providerText": launched.provider_text,
        });
        let sha = hex::encode(Sha256::digest(launched.body.as_bytes()));
        if let Err(error) = state
            .store
            .record_t3_code_launch_message(
                device,
                client,
                launched.body,
                &sha,
                &id,
                launched.thread,
                &intent,
                &now,
            )
            .await
        {
            let _ = state.logger.record(
                "info",
                "t3_code_launch_message_failed",
                json!({"conversationId": id, "error": error.to_string()}),
            );
        }
    }
    if let Some(model) = resolved.model.as_deref() {
        let _ = state
            .store
            .record_project_model(&project.id, family, model, resolved.effort.as_deref())
            .await;
    }
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), T3_STARTING_NOTICE.into());
    let _ = state.logger.record(
        "info",
        "t3_code_launch_conversation",
        json!({"conversationId": id, "t3ThreadId": launched.thread, "agentFamily": family}),
    );
    bind_t3_conversation(
        state,
        project.clone(),
        conversation.clone(),
        launched.thread,
    )
    .await;
    Some(conversation)
}

/// Joins a launched thread's agent session to its conversation once T3
/// Code's store names it, as opening it from Chats would attach it, but
/// keeping the conversation (and its ID on the phone). Until then, and up
/// to `T3_BIND_WAIT`, the conversation says it is starting; past that it
/// says where to look.
async fn bind_t3_conversation(
    state: &AppState,
    project: StoredProject,
    conversation: StoredProjectConversation,
    thread: &str,
) {
    let state = state.clone();
    let thread = thread.to_owned();
    let id = conversation.conversation_id.clone();
    let family = conversation.family;
    let notice = |text: Option<&str>| {
        let mut notices = state
            .projects
            .notices
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match text {
            Some(text) => notices.insert(id.clone(), text.to_owned()),
            None => notices.remove(&id),
        };
    };
    // No session to wait for: T3 Code's thread is the conversation.
    if matches!(family, AgentFamily::OpenCode | AgentFamily::Cursor) {
        let now = now_text();
        let bound = match crate::t3_code::feed::runtime_id(family, &thread) {
            Some(runtime) => {
                let store = provider_store(&state, family).to_owned();
                let kept = state
                    .store
                    .bind_project_runtime(&id, family, &store, &runtime, None, &now)
                    .await;
                kept.is_ok()
                    && state
                        .store
                        .set_t3_code_thread(&id, &format!("t3:{thread}"), &thread, &now)
                        .await
                        .is_ok()
            }
            None => false,
        };
        notice((!bound).then_some(T3_UNBOUND_NOTICE));
        if bound {
            tokio::spawn(async move {
                let _ = crate::refresh_history(State(state.clone()), Path(id.clone())).await;
                crate::t3_code::stream::watch(&state, &id, &thread);
            });
        }
        return;
    }
    // T3 Code's own word on the reply reaches the open chat from launch, not
    // only once the session is bound (the transcript is found a few seconds
    // in); the stream ends with the run, and idles nothing.
    crate::t3_code::stream::watch(&state, &id, &thread);
    tokio::spawn(async move {
        let id = conversation.conversation_id.clone();
        let family = conversation.family;
        let deadline = tokio::time::Instant::now() + T3_BIND_WAIT;
        let notice = |text: Option<&str>| {
            let mut notices = state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match text {
                Some(text) => notices.insert(id.clone(), text.to_owned()),
                None => notices.remove(&id),
            };
        };
        loop {
            // A session not listed by its provider yet is looked for again.
            if let Some(native) = state.t3_code.native_for_thread(&thread, family).await {
                if let Ok(bound) =
                    attach_native_into(&state, &project, family, &native, Some(&conversation)).await
                {
                    // Opened from Chats meanwhile, the session is another
                    // chat's: this one says where it went.
                    let joined = bound.conversation_id == id;
                    if joined {
                        let _ = state
                            .store
                            .set_t3_code_thread(&id, &native, &thread, &now_text())
                            .await;
                        if family == AgentFamily::Codex {
                            let _ = associate_codex_project(&state, &project, &native).await;
                        }
                    }
                    notice((!joined).then_some(T3_ELSEWHERE_NOTICE));
                    let _ = state.logger.record(
                        "info",
                        "t3_code_launch_bound",
                        json!({"conversationId": id, "t3ThreadId": thread, "joined": joined}),
                    );
                    let _ = crate::refresh_history(State(state.clone()), Path(id.clone())).await;
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                notice(Some(T3_UNBOUND_NOTICE));
                let _ = state.logger.record(
                    "info",
                    "t3_code_launch_unbound",
                    json!({"conversationId": id, "t3ThreadId": thread}),
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// A message's files as T3 Code receives them, converted as for any agent
/// (`agent_images::for_agent`). Small files carry verified bytes; videos
/// stream their private verified snapshot, never reopen the stored original.
pub(super) async fn t3_code_uploads(
    state: &AppState,
    conversation: &str,
    files: Vec<wonder_store::StoredConversationFile>,
) -> Result<Vec<crate::t3_code::Upload>, String> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let missing = || "An attached file is missing. Attach it again.".to_owned();
    let workspace = media_workspace(state, conversation)
        .await
        .ok_or_else(missing)?;
    let mut uploads = Vec::new();
    for (file, bytes) in crate::agent_images::for_agent(&workspace, files).await? {
        let body = match bytes {
            Some(bytes) => crate::t3_code::UploadBody::Bytes(bytes),
            None if crate::uploads::is_video(file.mime_type.as_deref().unwrap_or_default()) => {
                let path = FsPath::new(&workspace)
                    .join(file.relative_path.as_deref().ok_or_else(missing)?);
                let input = crate::platform::open_regular_nofollow(&path).map_err(|_| missing())?;
                crate::t3_code::UploadBody::File {
                    file: input,
                    size: file.byte_size.ok_or_else(missing)? as u64,
                }
            }
            None => return Err(missing()),
        };
        uploads.push(crate::t3_code::Upload {
            name: file.name,
            mime_type: file
                .mime_type
                .unwrap_or_else(|| "application/octet-stream".into()),
            body,
        });
    }
    Ok(uploads)
}

/// T3 Code has the message: it is delivered there (its turn runs in T3 Code,
/// which Cupola does not follow), and a notice about it no longer applies.
async fn t3_code_delivered(state: &AppState, message: &wonder_store::StoredMessage) {
    if state
        .store
        .update_message_delivery(&message.id, "completed", None, None)
        .await
        .is_ok()
    {
        publish_message_state(state, message, DeliveryState::Completed, None, None).await;
    }
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&message.conversation_id);
    let _ = state
        .store
        .touch_project_conversation(&message.conversation_id, &now_text())
        .await;
}

/// The T3 thread and `clientRequestId` an uncertain message was sent with,
/// when it went through T3 Code.
async fn t3_code_intent(state: &AppState, message: &str) -> Option<T3CodeIntent> {
    let context = state.store.dispatch_context(message).await.ok().flatten()?;
    let context: Value = serde_json::from_str(&context).ok()?;
    if context["deliveryMethod"] != T3_CODE_SEND {
        return None;
    }
    Some(T3CodeIntent {
        thread: context["t3ThreadId"].as_str()?.to_owned(),
        request: context["clientRequestId"].as_str()?.to_owned(),
        provider_text: context["providerText"].as_str().map(str::to_owned),
        attachments: context["attachmentIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect(),
    })
}

/// What a message sent through T3 Code went with, as its intent saved it.
struct T3CodeIntent {
    provider_text: Option<String>,
    thread: String,
    request: String,
    /// The IDs T3 Code gave this send's uploads.
    attachments: Vec<String>,
}

/// An uncertain message sent through T3 Code (a restart or a lost answer
/// after its intent was saved) is settled by reading the T3 thread, not by
/// sending it again: T3 Code has it (completed), or it never arrived and it
/// waits its turn again, to go with the same `clientRequestId`. Until T3
/// Code can be read it stays uncertain and the phone says why.
async fn reconcile_t3_code(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    intent: &T3CodeIntent,
) {
    let found = state
        .t3_code
        .has_sent(
            &intent.thread,
            &intent.request,
            intent.provider_text.as_deref().unwrap_or(&message.body),
            &intent.attachments,
        )
        .await;
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return;
    };
    let _dispatch = state.dispatch_lock.lock().await;
    match state.store.message_by_id(&message.id).await {
        Ok(Some(current)) if current.state == "uncertain" => {}
        _ => return,
    }
    let notice = match found {
        Ok(found @ (crate::t3_code::Found::Started | crate::t3_code::Found::Queued(_))) => {
            // Waiting in T3 Code's queue: queued on the phone until T3 Code
            // starts it (`t3_code::queue`).
            if let crate::t3_code::Found::Queued(run) = &found {
                let _ = state
                    .store
                    .update_t3_code_intent(&message.id, &json!({ "t3QueuedRunId": run }))
                    .await;
            }
            t3_code_delivered(state, message).await;
            let _ = state.logger.record(
                "info",
                "t3_code_reconciled",
                json!({"messageId": message.id, "found": true,
                    "queued": matches!(found, crate::t3_code::Found::Queued(_))}),
            );
            return;
        }
        Ok(crate::t3_code::Found::Missing) => {
            if state
                .store
                .update_message_delivery(&message.id, "accepted_by_wonder", None, None)
                .await
                .is_ok()
            {
                publish_message_state(state, message, DeliveryState::AcceptedByWonder, None, None)
                    .await;
                state.dispatch_wake.notify_one();
            }
            let _ = state.logger.record(
                "info",
                "t3_code_reconciled",
                json!({"messageId": message.id, "found": false}),
            );
            let mut notices = state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if notices.get(&message.conversation_id).is_some_and(|notice| {
                notice == T3_CODE_UNCONFIRMED_NOTICE || notice == T3_CODE_RECHECK_NOTICE
            }) {
                notices.remove(&message.conversation_id);
            }
            return;
        }
        Err(crate::t3_code::CallError::NotConnected | crate::t3_code::CallError::Unauthorized) => {
            T3_CODE_RECHECK_NOTICE
        }
        // The T3 thread is gone: what reached it can no longer be read.
        Err(crate::t3_code::CallError::Refused { code, .. }) if code == "thread_not_found" => {
            T3_CODE_UNRESOLVED_NOTICE
        }
        Err(_) => T3_CODE_UNCONFIRMED_NOTICE,
    };
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(message.conversation_id.clone(), notice.to_owned());
}

pub(crate) async fn ready(state: &AppState) -> bool {
    state.ingestion.project_readiness(&state.store).await.ready
}

/// Durable Project send: claim, resolve, persist the
/// receipt before turn/start, and never blindly resend an uncertain request.
pub(crate) async fn dispatch(state: AppState, message: wonder_store::StoredMessage) {
    // The claim is atomic under the shared lock. Preparation, which can take
    // seconds (ending Claude on the Mac's process, reopening a Codex thread),
    // runs without it. Submission intent and receipt writes take it again;
    // the provider wait retains only this conversation's ordering gate.
    {
        let _claim = state
            .update_admission
            .dispatch_guard(&state.dispatch_lock)
            .await;
        // A rewind of this chat is cutting its history; the dispatch loop
        // sends the message once that is saved.
        if state.projects.is_rewinding(&message.conversation_id) {
            return;
        }
        match state.store.claim_message_for_dispatch(&message.id).await {
            Ok(true) => {}
            _ => return,
        }
        state
            .projects
            .dispatching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(message.id.clone());
    }
    struct Dispatching<'a>(&'a AppState, String);
    impl Drop for Dispatching<'_> {
        fn drop(&mut self) {
            self.0
                .projects
                .dispatching
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.1);
            self.0.projects.dispatch_done.notify_waiters();
        }
    }
    let _dispatching = Dispatching(&state, message.id.clone());
    let Ok(Some(message)) = state.store.message_by_id(&message.id).await else {
        return;
    };
    let mut submitting = false;
    // A Send now made before this dispatch applies to it, taken over or not
    // (one it acted on is already cleared); one made while it was being
    // prepared is left for the next dispatch.
    let take_over = take_over_request(&state, &message.conversation_id);
    let t3_only = match state
        .store
        .project_conversation(&message.conversation_id)
        .await
    {
        Ok(Some(stored)) => crate::t3_code::feed::bound_thread(&state, &stored)
            .await
            .is_some(),
        _ => false,
    };
    if (take_over.is_none() || t3_only) && deliver_through_t3_code(&state, &message).await {
        if let Some(request) = &take_over {
            end_take_over(&state, &message.conversation_id, request);
        }
        return;
    }
    let prepared = prepare_dispatch(&state, &message).await;
    if let Some(request) = &take_over {
        end_take_over(&state, &message.conversation_id, request);
    }
    let _conversation = state
        .projects
        .conversation_dispatch(&message.conversation_id)
        .await;
    // Reconnect's busy probe only sees turns with receipts. Share admission
    // across chats, retaining it through submission and receipt storage.
    // Wait for this chat before admission so an ordered follow-up does not
    // retain a generation it has not submitted to yet.
    let _provider_submission = state.provider_reconnect.read().await;
    let prepared = match prepared {
        Ok(prepared) => prepared.revalidate(&state).await,
        Err(failure) => Err(failure),
    };
    let mut guard = Some(
        state
            .update_admission
            .dispatch_guard(&state.dispatch_lock)
            .await,
    );
    let outcome = match prepared {
        Ok(prepared) => {
            submit_dispatch(&state, &message, prepared, &mut submitting, &mut guard).await
        }
        Err(failure) => Err(failure),
    };
    match outcome {
        Ok(Submission::Started(thread_id, turn_id, generation)) => {
            record_generation(&state, &message.id, &generation);
            if state
                .store
                .update_message_delivery(
                    &message.id,
                    "accepted_by_codex",
                    Some(&thread_id),
                    Some(&turn_id),
                )
                .await
                .is_err()
            {
                return;
            }
            publish_message_state(
                &state,
                &message,
                DeliveryState::AcceptedByCodex,
                Some(&thread_id),
                Some(&turn_id),
            )
            .await;
            // A buffered completion must remain the last delivery event.
            crate::drain_pending_with_dispatch(&state, &thread_id, &turn_id, &mut guard).await;
            // Wonder now owns the thread, so its history settles earlier queued sends.
            check_native_queue(&state, &message.conversation_id);
            state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&message.conversation_id);
            let _ = state
                .store
                .touch_project_conversation(&message.conversation_id, &now_text())
                .await;
            drop(guard);
        }
        Ok(Submission::CommandAccepted(thread)) => {
            if state
                .store
                .update_message_delivery(&message.id, "completed", Some(&thread), None)
                .await
                .is_ok()
            {
                publish_message_state(
                    &state,
                    &message,
                    DeliveryState::Completed,
                    Some(&thread),
                    None,
                )
                .await;
                state
                    .projects
                    .notices
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&message.conversation_id);
                let _ = state
                    .store
                    .touch_project_conversation(&message.conversation_id, &now_text())
                    .await;
            }
        }
        Ok(Submission::NativeQueued(confirmed)) => {
            // The native queue owns the input but has not issued a turn receipt.
            // `uncertain` is durable and the retry API refuses it, including
            // after restart. Recovery consults the queue and native history.
            if state
                .store
                .update_message_delivery(&message.id, "uncertain", None, None)
                .await
                .is_err()
            {
                return;
            }
            publish_message_state(&state, &message, DeliveryState::Uncertain, None, None).await;
            state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    message.conversation_id.clone(),
                    if confirmed {
                        CODEX_NATIVE_QUEUED_NOTICE
                    } else {
                        CODEX_NATIVE_QUEUE_UNCONFIRMED_NOTICE
                    }
                    .to_owned(),
                );
        }
        // The claim stays; the next dispatch pass releases it and the message
        // waits, unchanged, until the Mac lets go of the chat.
        Err(failure) if failure == HELD_ON_MAC => {
            // Direct dispatch can discover a desktop holder during preparation
            // without the queue loop's send_waits check. Release global admission
            // before reading the holder; other chats must keep dispatching.
            drop(guard.take());
            if let Ok(Some(stored)) = state
                .store
                .project_conversation(&message.conversation_id)
                .await
            {
                note_t3_reconnect(&state, &stored).await;
            }
        }
        Err(failure) => {
            let (status, delivery) = if submitting {
                ("uncertain", DeliveryState::Uncertain)
            } else {
                ("safe_to_retry", DeliveryState::SafeToRetry)
            };
            if state
                .store
                .fail_message_delivery(&message.id, status, &failure)
                .await
                .is_ok()
            {
                publish_message_state(&state, &message, delivery, None, None).await;
            }
            // Kept for the conversation's recovery notice; never a raw payload.
            state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(message.conversation_id.clone(), failure.clone());
            let _ = state.logger.record(
                "error",
                "project_dispatch_failed",
                json!({"messageId": message.id, "error": failure}),
            );
        }
    }
}

/// Prepares and submits in one call (tests; `dispatch` splits the phases).
#[cfg(all(test, unix))]
pub(crate) async fn dispatch_inner(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    submitting: &mut bool,
) -> Result<(String, String, String), String> {
    let prepared = prepare_dispatch(state, message).await?;
    let _conversation = state
        .projects
        .conversation_dispatch(&message.conversation_id)
        .await;
    let _provider_submission = state.provider_reconnect.read().await;
    let prepared = prepared.revalidate(state).await?;
    match submit_dispatch(state, message, prepared, submitting, &mut None).await? {
        Submission::Started(thread, turn, generation) => Ok((thread, turn, generation)),
        Submission::CommandAccepted(_) => Err("Command accepted without a model turn".into()),
        Submission::NativeQueued(confirmed) => Err(if confirmed {
            CODEX_NATIVE_QUEUED_NOTICE
        } else {
            CODEX_NATIVE_QUEUE_UNCONFIRMED_NOTICE
        }
        .into()),
    }
}

/// Everything a dispatch resolves before its first durable submission write.
pub(crate) struct PreparedDispatch {
    family: AgentFamily,
    provider_store: String,
    command: Option<(&'static str, Value)>,
    rpc: RpcClient,
    thread_id: String,
    params: Value,
    resume: Value,
    context: String,
    handoff: bool,
    native_queue: bool,
    /// Sent through ChatGPT's desktop app, which has the thread open.
    relay: Option<DesktopRelay>,
}

impl PreparedDispatch {
    /// Preparation precedes admission and may span a reconnect. Validate
    /// under shared admission, before taking dispatch_lock: capturing the
    /// current client must not hold every chat behind a runtime's start lock.
    async fn revalidate(self, state: &AppState) -> Result<Self, String> {
        if self.rpc.health().is_alive()
            && crate::providers::live_rpc_for_store(state, self.family, &self.provider_store)
                .await
                .is_some_and(|current| current.health().id() == self.rpc.health().id())
        {
            Ok(self)
        } else {
            Err("The agent restarted while preparing this message. Try again.".into())
        }
    }
}

/// Messages whose preparation waits until a test removes them.
#[cfg(test)]
pub(crate) static PREPARE_HOLD: std::sync::Mutex<Option<HashSet<String>>> =
    std::sync::Mutex::new(None);

/// Resolves a claimed message's thread, settings and input without holding
/// `dispatch_lock`. It may end Claude on the Mac's idle process for the chat
/// or reopen a Codex thread; it writes no submission state.
async fn prepare_dispatch(
    state: &AppState,
    message: &wonder_store::StoredMessage,
) -> Result<PreparedDispatch, String> {
    #[cfg(test)]
    {
        while PREPARE_HOLD
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|held| held.contains(&message.id))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    let conversation = state
        .store
        .project_conversation(&message.conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("This project conversation is unavailable.")?;
    if !crate::providers::is_default_store(state, conversation.family, &conversation.provider_store)
    {
        return Err(crate::providers::other_history(conversation.family));
    }
    if conversation.wonder_archived {
        return Err(
            "This thread is archived in Cupola. Restore it from Archived threads before sending."
                .into(),
        );
    }
    if codex_is_archived(state, &conversation).await? {
        return Err("This thread is archived. Restore it on your computer before sending.".into());
    }
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("This project is unavailable.")?;
    let project = thread_project(state, project, &conversation.cwd).await;
    if !project.is_included {
        return Err("This Project is not included in Cupola. Restore it before continuing.".into());
    }
    let validation_project = project.clone();
    let denied = state.denied_roots.clone();
    tokio::task::spawn_blocking(move || validate_execution_roots(&validation_project, &denied))
        .await
        .map_err(|_| "Project folders could not be checked.")?
        .map_err(str::to_owned)?;
    // Execution needs the recorded folder to still belong to the project and
    // exist; Wonder never rebases a session onto a different folder.
    let root = project
        .root_for(&conversation.cwd)
        .ok_or("This thread's folder was removed from the project. Add it back to continue.")?;
    if !FsPath::new(&root.canonical_path).is_dir() {
        return Err("This thread's folder is no longer available on your computer.".into());
    }
    let target_family = state
        .store
        .project_message_target(&message.id)
        .await
        .map_err(|e| e.to_string())?
        .and_then(|target| target.family)
        .unwrap_or(conversation.family);
    let skill_inputs =
        crate::codex_skills::message_input(state, message, Some(target_family)).await?;
    // The model this message carries decides whether the thread keeps its
    // session or moves to the other provider, in delivery order.
    let conversation =
        crate::provider_switch::apply_transition(state, conversation, message).await?;
    let catalog = state.runtime_catalog.read().await.clone();
    let (selected_model, selected_effort, selected_tier) = state
        .store
        .project_message_execution_settings(&message.id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| {
            (
                conversation.model.clone(),
                conversation.effort.clone(),
                conversation.service_tier.clone(),
            )
        });
    let (model, effort, service_tier) = dispatch_settings(
        &catalog,
        conversation.family,
        selected_model.as_deref(),
        selected_effort.as_deref(),
        selected_tier.as_deref(),
    )?;
    let roots = project
        .roots
        .iter()
        .map(|r| r.path.clone())
        .collect::<Vec<_>>();
    let files = state
        .store
        .attachments_for_message(&message.id)
        .await
        .map_err(|e| e.to_string())?;
    let media = media_workspace(state, &message.conversation_id)
        .await
        .ok_or("The attachment folder is unavailable.")?;
    for file in &files {
        if file.conversation_id != message.conversation_id {
            return Err("The attachment belongs to another conversation.".into());
        }
        // Refuse a changed file before any thread work; the bytes are read
        // and verified again by the same owner when the input is built.
        crate::attachments::validate_stored(&media, file).await?;
    }
    // Reuse the bridge's existing owned-workspace exception for this exact
    // conversation's media. Other Wonder data remains denied.
    let permission = if matches!(
        conversation.family,
        AgentFamily::OpenCode | AgentFamily::Cursor
    ) {
        json!({ "planMode": conversation.plan_mode })
    } else {
        let mut permission = claude_policy(
            state,
            &project,
            ClaudeModes::from(&conversation),
            &conversation.cwd,
        );
        permission["workspace"] = json!(media);
        permission
    };
    let binding = state
        .store
        .runtime_binding(&message.conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    // A retained legacy home can never be resumed with this computer's sign-in.
    let rpc =
        crate::providers::rpc_for_store(state, conversation.family, &conversation.provider_store)
            .await?;
    // Off by default; while every skill is off this sends nothing.
    let wonder_skills =
        crate::wonder_skills::TurnSkills::for_message(state, conversation.family, message).await;
    wonder_skills.prepare_runtime(state, &rpc).await;
    // The owner's per-provider additions: thread tools (on by default) and a
    // custom instruction (off by default).
    let additions =
        crate::agent_settings::TurnAdditions::for_family(state, conversation.family).await;
    let store = conversation.provider_store.clone();
    let now = now_text();
    let mut native_queue = false;
    let mut relay = None;
    let mut computer_notice = None;
    // A session created for this turn, whose first prompt carries Cupola's
    // preamble where the harness has no other way to take it.
    let mut new_session = false;
    let mut codex_config = None;
    let thread_id = match (conversation.family, binding) {
        (_, Some(binding)) if binding.execution_scope != wonder_store::EXECUTION_SCOPE_PROJECTS => {
            return Err("This conversation has no valid Project runtime binding.".into());
        }
        (AgentFamily::Codex, Some(binding)) => {
            // Send now: a codex CLI that has the thread open is stopped first,
            // so Wonder's runtime becomes its writer instead of queueing there.
            if let Some(request) = take_over_request(state, &message.conversation_id) {
                let holder = crate::desktop_activity::codex_holder(&binding.thread_id).await;
                let result =
                    crate::desktop_activity::take_over_codex_thread(&binding.thread_id).await;
                taken_over(state, message, &request, holder, "Codex", result)?;
            }
            // ChatGPT's desktop app has the thread open: the message goes to
            // it there, with that thread's settings, like its own queue.
            if relay_carries(state, message, conversation.family).await {
                relay =
                    desktop_relay(state, conversation.family, &store, &binding.thread_id).await?;
            }
            if relay.is_some() {
                native_queue = true;
            } else {
                // Admission without private idle evidence is for the native queue
                // alone. Even if the writer has just exited and resume succeeds,
                // do not turn that admission into a direct turn or settings change.
                let queue_only = native_queue_supported(state, &rpc).await
                    && codex_busy(state, &binding.thread_id, &store, false)
                        .await
                        .is_none();
                let (sandbox, approval, _) = codex_policy(&conversation.access_mode, &roots);
                // Read-only check first: a desktop client may be running this thread.
                let turns = result(&rpc, "thread/turns/list", json!({"threadId": binding.thread_id, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"})).await
                .map_err(|_| "This Codex thread is no longer available on your computer.".to_owned())?;
                if turns.pointer("/data/0/status").and_then(Value::as_str) == Some("inProgress") {
                    return Err(
                    "This thread is still working on your computer. Send again after it finishes."
                        .into(),
                );
                }
                let resume_params = if queue_only {
                    json!({"threadId":binding.thread_id,"excludeTurns":true})
                } else {
                    let mut params = json!({"threadId": binding.thread_id, "excludeTurns": true, "cwd": conversation.cwd, "model": model,
                    "serviceTier": service_tier, "sandbox": sandbox, "approvalPolicy": approval, "runtimeWorkspaceRoots": roots});
                    computer_notice =
                        crate::computer_runtime::configure_codex(state, &mut params).await;
                    codex_config = params.get("config").cloned();
                    params
                };
                let resumed = result(&rpc, "thread/resume", resume_params).await;
                match resumed {
                Err(error) if codex_active_writer_conflict(&error) => {
                    if native_queue_supported(state, &rpc).await {
                        // Do not resume, change settings or acquire a second writer.
                        // The existing owner drains its own queue with its settings.
                        native_queue = true;
                    } else {
                        return Err(CODEX_DESKTOP_RECOVERY_NOTICE.into());
                    }
                }
                Err(_) => return Err("This Codex thread could not be reopened. Its history on your computer is unchanged.".into()),
                Ok(resumed) => {
                    if resumed.pointer("/thread/id").and_then(Value::as_str)
                        != Some(binding.thread_id.as_str())
                    {
                        return Err("Codex returned a different thread. Nothing was sent.".into());
                    }
                    native_queue = queue_only;
                    if !native_queue {
                        associate_codex_project(state, &project, &binding.thread_id).await?;
                    }
                }
            }
            }
            binding.thread_id
        }
        (AgentFamily::Codex, None) => {
            let project_id = native_codex_project(state, &rpc, &project, &store).await;
            let (sandbox, approval, _) = codex_policy(&conversation.access_mode, &roots);
            let mut start = json!({
                "cwd": conversation.cwd, "projectId": project_id, "model": model, "serviceTier": service_tier, "sandbox": sandbox,
                "approvalPolicy": approval, "runtimeWorkspaceRoots": roots,
            });
            // Codex keeps a thread's dynamic tools from its start.
            if additions.thread_tools() {
                start["dynamicTools"] = json!(additions.tool_specs());
            }
            computer_notice = crate::computer_runtime::configure_codex(state, &mut start).await;
            codex_config = start.get("config").cloned();
            let started = result(&rpc, "thread/start", start).await?;
            let thread = started
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or("Codex returned no thread")?
                .to_owned();
            state
                .store
                .set_project_native_session(&conversation.conversation_id, &thread, &now)
                .await
                .map_err(|e| e.to_string())?;
            state
                .store
                .bind_project_runtime(
                    &conversation.conversation_id,
                    AgentFamily::Codex,
                    &store,
                    &thread,
                    None,
                    &now,
                )
                .await
                .map_err(|e| e.to_string())?;
            thread
        }
        (AgentFamily::Claude, Some(binding)) => {
            if let Some(native) = conversation.native_session_id.as_deref() {
                take_over_from_desktop(state, native, message).await?;
            }
            binding.thread_id
        }
        (AgentFamily::OpenCode | AgentFamily::Cursor, Some(binding)) => binding.thread_id,
        (AgentFamily::OpenCode | AgentFamily::Cursor, None) => {
            let mut start = json!({
                "requestId": conversation.conversation_id,
                "cwd": conversation.cwd, "model": model,
                "wonderPolicy": permission,
                // ACP adds MCP servers when the session is created.
                "dynamicTools": additions.tool_specs(),
                "wonderProject": claude_project(&project, &conversation.cwd),
            });
            // The host's Cua Driver is one of those servers, so it is named
            // at creation; `turn/start` repeats it for later changes.
            crate::computer_runtime::configure_project(state, !conversation.plan_mode, &mut start)
                .await?;
            let started = result(&rpc, "thread/start", start).await?;
            let thread = started
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or("The provider returned no session")?
                .to_owned();
            new_session = true;
            let session = started
                .pointer("/thread/sessionId")
                .and_then(Value::as_str)
                .ok_or("The provider returned no session")?
                .to_owned();
            state
                .store
                .set_project_native_session(&conversation.conversation_id, &session, &now)
                .await
                .map_err(|e| e.to_string())?;
            state
                .store
                .bind_project_runtime(
                    &conversation.conversation_id,
                    conversation.family,
                    &store,
                    &thread,
                    Some(&session),
                    &now,
                )
                .await
                .map_err(|e| e.to_string())?;
            thread
        }
        (AgentFamily::Claude, None) => {
            let started = result(
                &rpc,
                "thread/start",
                json!({
                    "cwd": conversation.cwd, "model": model, "serviceTier": service_tier,
                    "wonderPolicy": permission,
                    "wonderProject": claude_project(&project, &conversation.cwd),
                }),
            )
            .await?;
            let thread = started
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or("Claude returned no session")?
                .to_owned();
            let session = started
                .pointer("/thread/sessionId")
                .and_then(Value::as_str)
                .ok_or("Claude returned no session")?
                .to_owned();
            state
                .store
                .set_project_native_session(&conversation.conversation_id, &session, &now)
                .await
                .map_err(|e| e.to_string())?;
            crate::desktop_activity::schedule_desktop_import(
                state.clone(),
                conversation.conversation_id.clone(),
                session.clone(),
            );
            state
                .store
                .bind_project_runtime(
                    &conversation.conversation_id,
                    AgentFamily::Claude,
                    &store,
                    &thread,
                    Some(&session),
                    &now,
                )
                .await
                .map_err(|e| e.to_string())?;
            thread
        }
    };
    let annotation_inputs = crate::artifact_annotations::selected_inputs(
        state,
        &message.conversation_id,
        &files,
        false,
    )
    .await
    .map_err(|(_, detail)| detail)?;
    let ordinary_files = files
        .into_iter()
        .filter(|file| file.mime_type.as_deref() != Some(crate::artifact_annotations::MIME))
        .collect::<Vec<_>>();
    // An agent's message says who wrote it; the stored body stays as sent.
    let source = state
        .store
        .message_source(&message.id)
        .await
        .map_err(|e| e.to_string())?;
    let referenced_body = crate::composer_mentions::provider_text(state, message).await?;
    let body = crate::thread_tools::provider_text(source.as_ref(), &referenced_body);
    let command = if conversation.family == AgentFamily::Codex {
        crate::composer_commands::codex_request(
            &body,
            &thread_id,
            !ordinary_files.is_empty() || !annotation_inputs.is_empty() || !skill_inputs.is_empty(),
        )?
    } else {
        None
    };
    if command.is_some() && (native_queue || relay.is_some()) {
        return Err(HELD_ON_MAC.into());
    }
    // Images in a format the agent rejects (an iPhone's HEIC) go as JPEG.
    let ordinary_files = crate::agent_images::for_agent(&media, ordinary_files).await?;
    let mut input = if matches!(
        conversation.family,
        AgentFamily::OpenCode | AgentFamily::Cursor
    ) {
        let files = ordinary_files
            .iter()
            .map(|(file, _)| file.clone())
            .collect::<Vec<_>>();
        crate::attachments::acp_input(&body, &media, &files)?
    } else {
        crate::turn_input(&body, &media, &ordinary_files).await?
    };
    input.extend(annotation_inputs);
    input.extend(skill_inputs);
    // After a provider switch the first turn also carries the earlier
    // conversation, as labelled background in its own leading part.
    let attachment_kinds = ordinary_files
        .iter()
        .map(|(file, _)| {
            if file
                .mime_type
                .as_deref()
                .is_some_and(|m| m.starts_with("image/"))
            {
                crate::provider_switch::Attachment::Image
            } else {
                crate::provider_switch::Attachment::File
            }
        })
        .collect::<Vec<_>>();
    let handoff =
        crate::provider_switch::prepare(state, &conversation, &thread_id, &body, &attachment_kinds)
            .await?;
    if command.is_some() && handoff.is_some() {
        return Err("Send a regular message first to bring this conversation into the selected provider, then use its command.".into());
    }
    if let Some(text) = &handoff {
        input.insert(0, json!({"type": "text", "text": text}));
    }
    if new_session {
        if let Some(preamble) = additions.first_prompt() {
            input.insert(0, preamble);
        }
    }
    let context = json!({
        "schemaVersion": 1, "scope": "projects", "projectId": project.id, "rootsRevision": project.roots_revision,
        "deliveryMethod": if let Some((method, _)) = &command { *method } else if relay.is_some() { DESKTOP_RELAY_METHOD } else if native_queue { "thread/queue/add" } else { "turn/start" },
        "clientUserMessageId": message.client_message_id,
        "nativeOwnerSettings": native_queue,
        "workingDirectory": conversation.cwd, "model": model, "effort": effort, "serviceTier": service_tier,
        "accessMode": conversation.access_mode, "family": conversation.family,
        "claudeApproval": conversation.claude_approval, "planMode": conversation.plan_mode,
        "inputSha256": hex::encode(Sha256::digest(serde_json::to_vec(&input).unwrap_or_default())),
    })
    .to_string();
    let mut params = match conversation.family {
        AgentFamily::Codex => codex_turn_params(
            &thread_id,
            &message.client_message_id,
            &input,
            &CodexTurn {
                access_mode: &conversation.access_mode,
                roots: &roots,
                model: &model,
                effort: effort.as_deref(),
                service_tier: service_tier.as_deref(),
                cwd: &conversation.cwd,
                plan: conversation.plan_mode,
            },
        ),
        AgentFamily::Claude => {
            json!({"threadId": thread_id, "clientUserMessageId": message.client_message_id,
            "input": input, "model": model, "effort": effort, "serviceTier": service_tier,
            "wonderPolicy": permission, "dynamicTools": additions.tool_specs(),
            "wonderProject": claude_project(&project, &conversation.cwd)})
        }
        // The ACP bridge offers the thread tools through an MCP server it adds
        // to the session (ACP `mcpServers`), scoped to this thread.
        AgentFamily::OpenCode | AgentFamily::Cursor => {
            json!({"threadId": thread_id, "clientUserMessageId": message.client_message_id,
            "input": input, "model": model, "wonderPolicy": permission,
            "dynamicTools": additions.tool_specs(),
            "wonderProject": claude_project(&project, &conversation.cwd)})
        }
    };
    let mut resume = match conversation.family {
        AgentFamily::Codex => {
            let (sandbox, approval, _) = codex_policy(&conversation.access_mode, &roots);
            json!({"threadId":thread_id,"excludeTurns":true,"cwd":conversation.cwd,"model":model,"serviceTier":service_tier,
                "sandbox":sandbox,"approvalPolicy":approval,"runtimeWorkspaceRoots":roots})
        }
        AgentFamily::Claude | AgentFamily::OpenCode | AgentFamily::Cursor => {
            json!({"threadId":thread_id,"excludeTurns":true,
            "cwd":conversation.cwd,"model":model,"serviceTier":service_tier,"wonderPolicy":permission,
            "wonderProject":claude_project(&project, &conversation.cwd)})
        }
    };
    wonder_skills.apply(&mut params);
    additions.apply(&mut params);
    if let Some(notice) = computer_notice {
        if !params["additionalContext"].is_object() {
            params["additionalContext"] = json!({});
        }
        params["additionalContext"][crate::computer_runtime::CUA_DRIVER_CONTEXT_KEY] =
            json!({"kind": "application", "value": notice});
    }
    // File backups for Edit from here, once the owner turned them on.
    if crate::rewind::checkpointing(state, conversation.family).await {
        params["fileCheckpointing"] = Value::Bool(true);
    }
    // Routing: bundled Codex uses ChatGPT's native computer use. Every other
    // provider gets only the computer's enabled, installed Cua Driver.
    if conversation.family != AgentFamily::Codex {
        crate::computer_runtime::configure_project(state, !conversation.plan_mode, &mut params)
            .await?;
        resume["config"] = params["config"].clone();
    }
    // A later resume (an update, a restarted runtime) keeps the computer use
    // this Codex thread was loaded with.
    if let Some(config) = codex_config.filter(|_| !native_queue) {
        resume["config"] = config;
    }
    Ok(PreparedDispatch {
        family: conversation.family,
        provider_store: store,
        command,
        rpc,
        thread_id,
        params,
        resume,
        context,
        handoff: handoff.is_some(),
        native_queue,
        relay,
    })
}

enum Submission {
    CommandAccepted(String),
    Started(String, String, String),
    NativeQueued(bool),
}

/// Submits a prepared dispatch: the durable submission record, then
/// turn/start. Release the shared lock only after durable submission intent;
/// reacquire it before applying any provider outcome or publishing the receipt.
async fn submit_dispatch<'a>(
    state: &'a AppState,
    message: &wonder_store::StoredMessage,
    prepared: PreparedDispatch,
    submitting: &mut bool,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) -> Result<Submission, String> {
    let PreparedDispatch {
        family,
        provider_store: _,
        command,
        rpc,
        thread_id,
        params,
        resume,
        mut context,
        handoff,
        native_queue,
        relay,
    } = prepared;
    // A peer queue does not grant Wonder settings/approval ownership.
    if !native_queue {
        crate::update_handoff::remember_settings(state, &thread_id, &resume, &params).await?;
    } else {
        // Persist the hold's (or relay's) origin with the intent, immediately
        // before its transaction. Restart/reconciliation must never restart
        // this deadline.
        let mut journal: Value = serde_json::from_str(&context).map_err(|e| e.to_string())?;
        let origin = match relay {
            Some(_) => "relayedAt",
            None => "nativeQueueEnqueuedAt",
        };
        journal[origin] = json!(now_text());
        context = journal.to_string();
    }
    state
        .store
        .begin_dispatch_submission_with_context(&message.id, &thread_id, Some(&context))
        .await
        .map_err(|e| e.to_string())?;
    // Pending before submission: if the outcome is lost, this session is
    // replaced rather than given the same context twice.
    if handoff {
        crate::provider_switch::delivery_started(state, &message.conversation_id, &thread_id).await;
    }
    *submitting = true;
    // The claimed message remains dispatching_to_codex and owned by this
    // dispatch. Recovery skips its claim; archive/rewind refuse its work;
    // update handoff's unsafe-work check refuses a submit without a receipt.
    // Early terminal events are buffered until the receipt is bound below.
    let shared = dispatch.take();
    let relock = shared.is_some();
    drop(shared);
    if let Some(DesktopRelay {
        mut session,
        running,
    }) = relay
    {
        let outcome = session
            .deliver(
                &message.client_message_id,
                &params["input"],
                running.as_deref(),
            )
            .await;
        if relock {
            *dispatch = Some(state.dispatch_lock.lock().await);
        }
        if handoff {
            match &outcome {
                crate::codex_desktop_ipc::Outcome::Delivered { .. } => {
                    crate::provider_switch::delivery_accepted(
                        state,
                        &message.conversation_id,
                        &thread_id,
                    )
                    .await
                }
                crate::codex_desktop_ipc::Outcome::NotSent(_) => {
                    crate::provider_switch::delivery_refused(
                        state,
                        &message.conversation_id,
                        &thread_id,
                    )
                    .await
                }
                crate::codex_desktop_ipc::Outcome::Unknown(_) => {}
            }
        }
        return desktop_relay_outcome(state, message, &thread_id, outcome).await;
    }
    let response = if native_queue {
        rpc.request(
            "thread/queue/add",
            json!({
                "threadId": thread_id, "clientUserMessageId": message.client_message_id,
                "input": params["input"],
            }),
        )
        .await
    } else {
        // The turn's changes start from the tree as it is before it runs.
        crate::filesystem::thread_changes::before_runtime_turn(state, &thread_id).await;
        let response = if let Some((method, command_params)) = &command {
            rpc.request(method, command_params.clone()).await
        } else {
            rpc.request("turn/start", params).await
        };
        let turn = response
            .as_ref()
            .ok()
            .filter(|r| r.error.is_none())
            .and_then(|r| r.result.as_ref())
            .and_then(|started| {
                started
                    .pointer("/turn/id")
                    .or_else(|| started.get("turnId"))
            })
            .and_then(Value::as_str);
        crate::filesystem::thread_changes::runtime_turn_sent(state, &thread_id, turn);
        response
    };
    if relock {
        // Finish an already submitted intent even if update admission changed
        // while it waited. The updater checks that intent under this lock.
        *dispatch = Some(state.dispatch_lock.lock().await);
    }
    if native_queue {
        // No native enqueue error authorizes another add: a timeout, malformed
        // receipt or error after commit can all hide an accepted submission.
        let confirmed = response
            .ok()
            .filter(|r| r.error.is_none())
            .and_then(|r| r.result)
            .is_some_and(|r| {
                r.pointer("/queuedSubmission/clientUserMessageId")
                    .and_then(Value::as_str)
                    == Some(message.client_message_id.as_str())
                    && r.pointer("/queuedSubmission/id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
            });
        return Ok(Submission::NativeQueued(confirmed));
    }
    let response = response.map_err(|e| e.to_string())?;
    if let Some(error) = response.error {
        // Claude refused before reading it because a reply still runs there
        // (Wonder's own, or the chat reopened on the Mac): it waits its turn.
        let rejection = input_rejection(family, "turn/start", &error);
        if rejection == Some(InputRejection::Wait)
            && state
                .store
                .requeue_refused_send(&message.id)
                .await
                .unwrap_or(false)
        {
            *submitting = false;
            if handoff {
                crate::provider_switch::delivery_refused(
                    state,
                    &message.conversation_id,
                    &thread_id,
                )
                .await;
            }
            return Err(HELD_ON_MAC.into());
        }
        if rejection.is_some() {
            *submitting = false;
            if handoff {
                crate::provider_switch::delivery_refused(
                    state,
                    &message.conversation_id,
                    &thread_id,
                )
                .await;
            }
        }
        return Err(error.message);
    }
    if command
        .as_ref()
        .is_some_and(|(method, _)| *method == "thread/compact/start")
    {
        response
            .result
            .ok_or("The provider returned no command receipt")?;
        return Ok(Submission::CommandAccepted(thread_id));
    }
    let started = response.result.ok_or("turn/start returned no result")?;
    let turn = started
        .pointer("/turn/id")
        .or_else(|| started.get("turnId"))
        .and_then(Value::as_str)
        .ok_or("The provider returned no message receipt")?
        .to_owned();
    // The context reached the session; it is never sent again.
    if handoff {
        crate::provider_switch::delivery_accepted(state, &message.conversation_id, &thread_id)
            .await;
    }
    Ok(Submission::Started(
        thread_id,
        turn,
        rpc.health().id().to_owned(),
    ))
}

/// What ChatGPT's desktop app did with a relayed message. Nothing sent: the
/// message waits again and the next dispatch goes the usual way (the relay
/// rests for this thread). A lost answer may hide a started turn, so the
/// message stays unconfirmed and is never sent again; native history settles
/// it by its client id.
async fn desktop_relay_outcome(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    thread_id: &str,
    outcome: crate::codex_desktop_ipc::Outcome,
) -> Result<Submission, String> {
    use crate::codex_desktop_ipc::Outcome;
    let log = |event: &str, detail: Value| {
        let _ = state.logger.record(
            "info",
            event,
            json!({"messageId": message.id, "threadId": thread_id, "detail": detail}),
        );
    };
    match outcome {
        Outcome::Delivered { turn, steered } => {
            log("codex_desktop_relay_sent", json!({"steered": steered}));
            Ok(Submission::Started(
                thread_id.to_owned(),
                turn,
                DESKTOP_RELAY_GENERATION.to_owned(),
            ))
        }
        Outcome::NotSent(refusal) => {
            crate::codex_desktop_ipc::refuse(thread_id);
            log("codex_desktop_relay_refused", json!(format!("{refusal:?}")));
            if state
                .store
                .requeue_refused_send(&message.id)
                .await
                .unwrap_or(false)
            {
                Err(HELD_ON_MAC.into())
            } else {
                Err(DESKTOP_RELAY_UNCONFIRMED.into())
            }
        }
        Outcome::Unknown(why) => {
            log("codex_desktop_relay_unconfirmed", json!(why));
            Err(DESKTOP_RELAY_UNCONFIRMED.into())
        }
    }
}

/// The runtime generation that accepted a message; recovery trusts a running
/// turn only while that same runtime is live.
pub(crate) fn record_generation(state: &AppState, message_id: &str, generation: &str) {
    state
        .projects
        .generations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            message_id.to_owned(),
            (generation.to_owned(), Instant::now()),
        );
}

/// Dispatch records a generation just before it stores the turn receipt, so a
/// recent entry may belong to a message that is not active yet.
const GENERATION_SETTLE_GRACE: Duration = Duration::from_secs(600);

/// Forget generations of turns that settled without passing through recovery
/// (the normal case), so the map holds only running turns.
fn retain_running_generations(
    generations: &mut HashMap<String, (String, Instant)>,
    active: &HashSet<&str>,
    now: Instant,
) {
    generations.retain(|message, (_, recorded)| {
        active.contains(message.as_str())
            || now.saturating_duration_since(*recorded) < GENERATION_SETTLE_GRACE
    });
}

/// How long a live runtime's turn may stay missing from complete native
/// history before recovery stops trusting it. A started turn is written to
/// history at once, so a longer absence means the stored turn id is gone
/// (compaction or a lost `turn/completed`) and nothing will ever settle it.
const UNLISTED_TURN_TTL: Duration = Duration::from_secs(5 * 60);

/// What recovery makes of a turn whose accepting runtime is still live and
/// whose native end it has not seen.
#[derive(Debug, PartialEq, Eq)]
enum LiveTurn {
    /// The runtime may still be running it: wait for its own completion.
    Running,
    /// Complete native history has not listed the turn for the whole TTL:
    /// it is settled like a turn whose runtime is gone.
    Unlisted,
}

fn live_turn(unlisted_since: Option<Instant>, now: Instant, ttl: Duration) -> LiveTurn {
    match unlisted_since {
        Some(since) if now.saturating_duration_since(since) >= ttl => LiveTurn::Unlisted,
        _ => LiveTurn::Running,
    }
}

/// Native history also settles a missed completion from a still-live runtime.
/// An uncertain receipt is completed only with proof for its exact native turn;
/// incomplete history never authorizes resubmission.
pub(crate) async fn recover(state: &AppState) {
    recover_with_ttl(state, UNLISTED_TURN_TTL).await;
}

async fn recover_with_ttl(state: &AppState, unlisted_ttl: Duration) {
    if state.update_admission.work_paused() {
        return;
    }
    crate::rewind::recover(state).await;
    recover_uncertain_receipts(state).await;
    crate::provider_runtime::apply_waiting_settings(state).await;
    let Ok(messages) = state.store.active_runtime_messages().await else {
        return;
    };
    retain_running_generations(
        &mut state
            .projects
            .generations
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
        &messages.iter().map(|message| message.id.as_str()).collect(),
        Instant::now(),
    );
    state
        .projects
        .unlisted_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|id, _| messages.iter().any(|message| &message.id == id));
    for message in messages {
        if state.update_admission.work_paused() {
            return;
        }
        let Ok(Some(conversation)) = state
            .store
            .project_conversation(&message.conversation_id)
            .await
        else {
            continue;
        };
        let (Some(thread), Some(turn)) = (
            message.codex_thread_id.clone(),
            message.codex_turn_id.clone(),
        ) else {
            continue;
        };
        let Some(rpc) = recovery_rpc_for(state, &conversation).await else {
            continue;
        };
        let Ok(native) = native_turn_status(&rpc, &thread, &turn).await else {
            continue;
        };
        let status = native.as_ref().map(|(status, _)| status.clone());
        let unlisted_since =
            state
                .projects
                .observe_listing(&message.id, &turn, native.is_some(), Instant::now());
        // ChatGPT's desktop app runs a relayed turn: its own lifecycle record
        // and rollout decide (see `relayed_turn`). A lost one falls through
        // to the "runtime is gone" settlement below.
        let relayed = desktop_relayed(state, &message.id).await;
        let status = match &relayed {
            Some(relay) => match relayed_turn(
                desktop_turn_status(state, &conversation, &thread, &turn).await,
                native
                    .as_ref()
                    .map(|(status, ended)| (status.as_str(), *ended)),
                match stop_requested(state, &thread, &turn).await {
                    Ok(stopped) => stopped,
                    Err(_) => continue,
                },
                desktop_relay_quiet(&thread, relay),
            ) {
                RelayedTurn::Running => continue,
                RelayedTurn::Ended(status) => Some(status.to_owned()),
                RelayedTurn::Lost => None,
            },
            None => status,
        };
        // A turn another Codex process runs (the desktop app that owns the
        // thread drains Cupola's native queue) reads interrupted or running
        // here with no recorded end. That process's own lifecycle decides, so
        // a turn still running there is never settled as lost, and one the
        // owner stopped there is not a failure.
        let mut stopped_elsewhere = false;
        let status = if relayed.is_none()
            && conversation.family == AgentFamily::Codex
            && matches!(status.as_deref(), None | Some("inProgress" | "interrupted"))
            && !native.as_ref().is_some_and(|(_, ended)| *ended)
        {
            match desktop_turn_status(state, &conversation, &thread, &turn).await {
                Some("inProgress") => continue,
                Some(ended @ ("completed" | "failed")) => Some(ended.to_owned()),
                Some("interrupted") => {
                    stopped_elsewhere = true;
                    Some("interrupted".to_owned())
                }
                _ => status,
            }
        } else {
            status
        };
        let items = if matches!(
            status.as_deref(),
            Some("completed" | "failed" | "interrupted")
        ) {
            crate::hydrate_app_server_turn_items(state, &thread, &turn).await
        } else {
            None
        };
        // An update may have paused/resumed this turn while native history was
        // loading. Re-enter the admission barrier and verify the exact receipt
        // before any persisted state or client event changes.
        let Some(_admission) = state.update_admission.claim_guard().await else {
            return;
        };
        let _dispatch = state.dispatch_lock.lock().await;
        let Ok(active) = state.store.active_runtime_messages().await else {
            continue;
        };
        let Some(current_message) = active.iter().find(|current| {
            current.id == message.id
                && current.codex_thread_id.as_deref() == Some(&thread)
                && current.codex_turn_id.as_deref() == Some(&turn)
        }) else {
            continue;
        };
        let accepted = state
            .projects
            .generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&message.id)
            .map(|(generation, _)| generation.clone());
        let current = if !crate::providers::is_default_store(
            state,
            conversation.family,
            &conversation.provider_store,
        ) {
            // Legacy history has no live runtime.
            crate::providers::live_rpc_for_store(
                state,
                conversation.family,
                &conversation.provider_store,
            )
            .await
            .map(|rpc| rpc.health())
        } else {
            match conversation.family {
                AgentFamily::Codex => Some(state.projects.codex.lock().await.health()),
                AgentFamily::Claude | AgentFamily::OpenCode | AgentFamily::Cursor => {
                    match crate::claude::client(state, conversation.family) {
                        Ok(client) => Some(client.lock().await.health()),
                        Err(_) => None,
                    }
                }
            }
        };
        let same_live_runtime = accepted.is_some()
            && current
                .as_ref()
                .is_some_and(|h| h.is_alive() && Some(h.id()) == accepted.as_deref());
        // A live runtime's Codex turn that history never lists is not left
        // running forever; a relayed turn has its own quiet limit.
        let turn_unlisted = relayed.is_none()
            && conversation.family == AgentFamily::Codex
            && live_turn(unlisted_since, Instant::now(), unlisted_ttl) == LiveTurn::Unlisted;
        // A store that cannot say whether the owner stopped it defers the turn
        // to the next pass; it is never guessed to be a lost turn.
        let Ok(stopped_by_owner) = stop_requested(state, &thread, &turn).await else {
            continue;
        };
        // A provider marks a turn interrupted when its process ended mid-turn
        // (Codex on restart, the Claude bridge's stale turns). Unless the owner
        // pressed Stop, the turn did not finish: it failed and asks for a look.
        // A relayed turn ends interrupted only by ChatGPT's record or the
        // owner's Stop: the owner's own Stop there or here, not a failure.
        let stopped_there = relayed.is_some() || stopped_elsewhere;
        let status = match status.as_deref() {
            Some("interrupted") if !same_live_runtime && !stopped_by_owner && !stopped_there => {
                // "Continue threads after restarts" continues it instead,
                // through the update continuation path.
                if crate::update_handoff::continue_after_restart(
                    state,
                    current_message,
                    &thread,
                    &turn,
                )
                .await
                {
                    continue;
                }
                Some("failed".to_owned())
            }
            _ => status,
        };
        match status.as_deref() {
            Some("completed" | "failed" | "interrupted") => {
                if let Some(items) = items {
                    for entry in items {
                        if entry.item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                            crate::process_app_server_notification(state, json!({"method": "item/completed",
                                "params": {"threadId": thread, "turnId": turn, "item": entry.item}}), false).await;
                        }
                    }
                }
                if !crate::process_app_server_notification(state, json!({"method": "turn/completed",
                    "params": {"threadId": thread, "turnId": turn, "turn": {"id": turn, "status": status}}}), false).await {
                    continue;
                }
            }
            _ if (same_live_runtime && !turn_unlisted) || current_message.state == "uncertain" => {
                continue
            }
            _ => {
                // The runtime that ran the turn is gone, so the turn cannot
                // finish there: the owner sees it failed and is told. The
                // receipt stays uncertain, so native proof of a completed turn
                // still settles it and a resend never duplicates the work.
                if state
                    .store
                    .mark_runtime_turn_uncertain(
                        &thread,
                        &turn,
                        relayed
                            .as_ref()
                            .map(|_| (message.id.as_str(), DESKTOP_RELAY_STALE)),
                    )
                    .await
                    .is_ok()
                {
                    let _ = crate::publish_message_state_checked(
                        state,
                        current_message,
                        DeliveryState::Failed,
                        Some(&thread),
                        Some(&turn),
                    )
                    .await;
                }
            }
        }
        state
            .projects
            .generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&message.id);
        state
            .projects
            .unlisted_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&message.id);
        clear_stop(state, &thread, &turn).await;
    }
    release_idle_codex_runtime(state).await;
}

async fn recover_uncertain_receipts(state: &AppState) {
    recover_uncertain_receipts_with_budget(
        state,
        RECEIPT_HISTORY_PAGE_LIMIT,
        RECEIPT_PAGES_PER_CYCLE,
        RECEIPT_RESCAN_DELAY,
    )
    .await;
}

#[cfg(all(test, unix))]
async fn recover_uncertain_receipts_with_limit(state: &AppState, page_limit: usize) {
    recover_uncertain_receipts_with_budget(state, page_limit, page_limit, Duration::ZERO).await;
}

async fn recover_uncertain_receipts_with_budget(
    state: &AppState,
    page_limit: usize,
    pages_per_cycle: usize,
    rescan_delay: Duration,
) {
    let Ok(messages) = state.store.ambiguous_dispatch_messages().await else {
        return;
    };
    let ids = messages
        .iter()
        .map(|message| message.id.as_str())
        .collect::<HashSet<_>>();
    state
        .projects
        .receipt_scans
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|id, _| ids.contains(id.as_str()));
    let chats = messages
        .iter()
        .map(|message| message.conversation_id.as_str())
        .collect::<HashSet<_>>();
    state
        .projects
        .native_queue_events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|chat, _| chats.contains(chat.as_str()));
    for message in messages {
        if state.update_admission.work_paused() {
            return;
        }
        if !is_project(state, &message.conversation_id).await || !ready(state).await {
            continue;
        }
        if let Some(intent) = t3_code_intent(state, &message.id).await {
            reconcile_t3_code(state, &message, &intent).await;
            continue;
        }
        // A native compact leaves no user message in history to find, so a
        // lost receipt can never settle by scanning. It is safe to run again.
        if is_compact_command(state, &message.id).await {
            let _ = state
                .store
                .update_message_delivery(&message.id, "failed", None, None)
                .await;
            state
                .projects
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    message.conversation_id.clone(),
                    COMPACT_UNCONFIRMED_NOTICE.to_owned(),
                );
            publish_message_state(state, &message, DeliveryState::Failed, None, None).await;
            continue;
        }
        let scan =
            scan_ambiguous_receipt(state, &message, page_limit, pages_per_cycle, rescan_delay)
                .await;
        if matches!(scan, ReceiptScanStep::Pending) {
            continue;
        }
        let Some(_admission) = state.update_admission.claim_guard().await else {
            return;
        };
        let _dispatch = state.dispatch_lock.lock().await;
        let Ok(current) = state.store.ambiguous_dispatch_messages().await else {
            continue;
        };
        if !current.iter().any(|current| {
            current.id == message.id
                && current.conversation_id == message.conversation_id
                && current.client_message_id == message.client_message_id
                && current.codex_thread_id == message.codex_thread_id
        }) {
            continue;
        }
        let turn = match scan {
            ReceiptScanStep::Accepted(turn) => turn,
            ReceiptScanStep::Notice(error) => {
                state
                    .projects
                    .notices
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(message.conversation_id.clone(), error.to_owned());
                continue;
            }
            ReceiptScanStep::Pending => continue,
            ReceiptScanStep::Absent => {
                settle_absent_receipt(state, &message).await;
                continue;
            }
        };
        if state
            .store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                message.codex_thread_id.as_deref(),
                Some(&turn),
            )
            .await
            .is_err()
        {
            continue;
        }
        crate::ingestion::request_conversation_reconcile(state, &message.conversation_id).await;
        crate::publish_message_state(
            state,
            &message,
            DeliveryState::AcceptedByCodex,
            message.codex_thread_id.as_deref(),
            Some(&turn),
        )
        .await;
        let mut notices = state
            .projects
            .notices
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if notices.get(&message.conversation_id).is_some_and(|notice| {
            notice == RECEIPT_HISTORY_LIMIT_NOTICE
                || notice == RECEIPT_HISTORY_INVALID_NOTICE
                || notice == CODEX_NATIVE_QUEUED_NOTICE
                || notice == CODEX_NATIVE_QUEUE_UNCONFIRMED_NOTICE
                || notice == CODEX_DESKTOP_RECOVERY_NOTICE
                || notice == CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE
        }) {
            notices.remove(&message.conversation_id);
        }
    }
}

struct NativeQueueIntent {
    enqueued_at_ms: Option<i64>,
}

impl NativeQueueIntent {
    fn holds_runtime_at(&self, now_ms: i64) -> bool {
        // Old/malformed journals and clock rollback cannot grant an unbounded
        // hold. Expiry only permits idle shutdown; it never permits re-enqueue.
        self.enqueued_at_ms.is_some_and(|enqueued| {
            (0..NATIVE_QUEUE_RUNTIME_HOLD.as_millis() as i64)
                .contains(&now_ms.saturating_sub(enqueued))
        })
    }
}

const RECEIPT_ABSENT_REASON: &str = "The provider never received this message. Send it again.";

const COMPACT_UNCONFIRMED_NOTICE: &str =
    "Cupola couldn't confirm the thread was compacted. Send /compact again.";

async fn is_compact_command(state: &AppState, message: &str) -> bool {
    state
        .store
        .dispatch_context(message)
        .await
        .ok()
        .flatten()
        .and_then(|context| serde_json::from_str::<Value>(&context).ok())
        .is_some_and(|context| context["deliveryMethod"] == "thread/compact/start")
}

async fn native_queue_intent(state: &AppState, message: &str) -> Option<NativeQueueIntent> {
    let context = state
        .store
        .dispatch_context(message)
        .await
        .ok()
        .flatten()
        .and_then(|context| serde_json::from_str::<Value>(&context).ok())?;
    (context["deliveryMethod"] == "thread/queue/add").then(|| NativeQueueIntent {
        enqueued_at_ms: context["nativeQueueEnqueuedAt"]
            .as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp_millis()),
    })
}

async fn native_queue_holds_runtime(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    now_ms: i64,
) -> bool {
    let Some(intent) = native_queue_intent(state, &message.id).await else {
        return false;
    };
    if intent.holds_runtime_at(now_ms) {
        return true;
    }
    state
        .projects
        .notices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            message.conversation_id.clone(),
            CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE.to_owned(),
        );
    false
}

/// Wonder took this chat's thread over: check its queued sends' history once.
fn check_native_queue(state: &AppState, conversation: &str) {
    *state
        .projects
        .native_queue_events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(conversation.to_owned())
        .or_default() += 1;
}

async fn scan_ambiguous_receipt(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    page_limit: usize,
    pages_per_cycle: usize,
    rescan_delay: Duration,
) -> ReceiptScanStep {
    scan_ambiguous_receipt_at(
        state,
        message,
        page_limit,
        pages_per_cycle,
        rescan_delay,
        Utc::now().timestamp_millis(),
    )
    .await
}

async fn scan_ambiguous_receipt_at(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    page_limit: usize,
    pages_per_cycle: usize,
    rescan_delay: Duration,
    now_ms: i64,
) -> ReceiptScanStep {
    let Some(thread) = message.codex_thread_id.as_deref() else {
        return ReceiptScanStep::Pending;
    };
    let queue_intent = native_queue_intent(state, &message.id).await;
    let events = state
        .projects
        .native_queue_events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&message.conversation_id)
        .copied()
        .unwrap_or_default();
    let mut scan = {
        let mut scans = state
            .projects
            .receipt_scans
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        scans
            .remove(&message.id)
            .filter(|scan| {
                scan.thread_id == thread && scan.client_message_id == message.client_message_id
            })
            .unwrap_or_else(|| ReceiptScan::new(message, events))
    };
    let conversation = state
        .store
        .project_conversation(&message.conversation_id)
        .await;
    if let Some(intent) = &queue_intent {
        let mut event = scan.events != events;
        scan.events = events;
        if !intent.holds_runtime_at(now_ms) && !scan.expiry_checked {
            scan.expiry_checked = true;
            event = true;
        }
        // A desktop turn that finished may have drained the queue into history.
        if let Ok(Some(conversation)) = &conversation {
            if let Some(home) = crate::providers::home_for_store(
                state,
                AgentFamily::Codex,
                &conversation.provider_store,
            )
            .await
            {
                if let Some(activity) =
                    crate::desktop_activity::codex_thread_activity(&home, thread).await
                {
                    // A first readable finished state counts too: the turn
                    // may have ended after this scan's last history read.
                    let seen = Some((activity.turn.clone(), activity.status));
                    event |= !activity.working() && scan.desktop_turn != seen;
                    scan.desktop_turn = seen;
                }
            }
        }
        if event {
            // Start from the newest page: an input accepted since this
            // sweep began lands above the pages it has already read.
            scan.reset_after(Duration::ZERO);
            scan.check = true;
        }
    }
    let waiting = match queue_intent {
        Some(_) => !scan.check || Instant::now() < scan.resume_after,
        None => Instant::now() < scan.resume_after,
    };
    if waiting {
        let notice = scan.notice;
        state
            .projects
            .receipt_scans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(message.id.clone(), scan);
        return notice.map_or(ReceiptScanStep::Pending, ReceiptScanStep::Notice);
    }
    let rpc = match &conversation {
        Ok(Some(conversation)) => recovery_rpc_for(state, conversation).await,
        _ => None,
    };
    let mut outcome = ReceiptScanStep::Pending;
    let mut absent = false;
    // Whether this check ended: read to the end of history or failed,
    // including a runtime that could not start. An update pause only defers it.
    let mut swept = rpc.is_none() && !state.update_admission.work_paused();
    // Expiry is honest without a runtime: no check can confirm delivery now.
    let mut queue_notice = queue_intent
        .as_ref()
        .filter(|intent| !intent.holds_runtime_at(now_ms))
        .map(|_| CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE);
    if let Some(rpc) = rpc {
        if let Some(intent) = &queue_intent {
            // Read both sources. Removal from the queue alone is never proof
            // of non-delivery: Codex may be moving it into history right now.
            let queued = native_queue_supported(state, &rpc).await
                && native_queue_contains(&rpc, thread, &message.client_message_id).await
                    == Ok(true);
            scan.notice = Some(if !intent.holds_runtime_at(now_ms) {
                CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE
            } else if queued {
                CODEX_NATIVE_QUEUED_NOTICE
            } else {
                CODEX_NATIVE_QUEUE_UNCONFIRMED_NOTICE
            });
            queue_notice = scan.notice;
            outcome = ReceiptScanStep::Notice(scan.notice.unwrap());
        }
        for _ in 0..pages_per_cycle.min(page_limit.saturating_sub(scan.pages_scanned)) {
            if state.update_admission.work_paused() {
                break;
            }
            let page = result(
                &rpc,
                "thread/items/list",
                json!({"threadId": thread, "limit": 100, "sortDirection": "desc", "cursor": scan.cursor}),
            )
            .await;
            let Ok(page) = page else {
                // Opaque cursors may expire after native history changes. A
                // fresh sweep is safe because this path never resends intent.
                // A native-queue check ends here and waits for the next event.
                scan.reset_after(rescan_delay);
                swept = true;
                break;
            };
            if page.get("data").and_then(Value::as_array).is_none() {
                scan.reset_after(rescan_delay);
                scan.notice = Some(RECEIPT_HISTORY_INVALID_NOTICE);
                outcome = ReceiptScanStep::Notice(RECEIPT_HISTORY_INVALID_NOTICE);
                swept = true;
                break;
            }
            if let Some(turn) = crate::find_client_message(&page, &message.client_message_id) {
                return ReceiptScanStep::Accepted(turn);
            }
            scan.pages_scanned += 1;
            let next = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match next {
                None => {
                    scan.reset_after(rescan_delay);
                    if queue_intent.is_none() {
                        scan.notice = None;
                        absent = absent_history_settles(&conversation);
                    }
                    swept = true;
                    break;
                }
                Some(cursor) if !scan.seen.insert(cursor.clone()) => {
                    scan.reset_after(rescan_delay);
                    scan.notice = Some(RECEIPT_HISTORY_INVALID_NOTICE);
                    outcome = ReceiptScanStep::Notice(RECEIPT_HISTORY_INVALID_NOTICE);
                    swept = true;
                    break;
                }
                Some(cursor) => scan.cursor = Some(cursor),
            }
        }
    }
    if scan.pages_scanned >= page_limit {
        scan.reset_after(rescan_delay);
        scan.notice = Some(RECEIPT_HISTORY_LIMIT_NOTICE);
        outcome = ReceiptScanStep::Notice(RECEIPT_HISTORY_LIMIT_NOTICE);
        swept = true;
    }
    if queue_intent.is_some() {
        // An event stays pending through later cycles until its check ends;
        // only a new event starts another.
        scan.check = !swept;
        // Generic history warnings promise a retry; these sends never get one.
        if let Some(notice) = queue_notice {
            scan.notice = Some(notice);
            outcome = ReceiptScanStep::Notice(notice);
        }
    }
    state
        .projects
        .receipt_scans
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(message.id.clone(), scan);
    if absent && matches!(outcome, ReceiptScanStep::Pending) {
        return ReceiptScanStep::Absent;
    }
    outcome
}

/// A complete history without the message settles it for the providers that
/// record a turn before they answer (Claude, OpenCode, Cursor). Codex keeps it
/// uncertain: its native queue and desktop can still deliver it later.
fn absent_history_settles(
    conversation: &Result<Option<wonder_store::StoredProjectConversation>, sqlx::Error>,
) -> bool {
    matches!(conversation, Ok(Some(c)) if c.family != AgentFamily::Codex)
}

/// The provider never took the message: failed and retryable, so the chat
/// stops spinning and a person can send it again.
async fn settle_absent_receipt(state: &AppState, message: &wonder_store::StoredMessage) {
    if state
        .store
        .fail_message_delivery(&message.id, "failed", RECEIPT_ABSENT_REASON)
        .await
        .is_ok()
    {
        state
            .projects
            .receipt_scans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&message.id);
        publish_message_state(state, message, DeliveryState::Failed, None, None).await;
    }
}

async fn native_queue_contains(
    rpc: &RpcClient,
    thread: &str,
    client: &str,
) -> Result<bool, String> {
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..RECEIPT_PAGES_PER_CYCLE {
        let page = result(
            rpc,
            "thread/queue/list",
            json!({"threadId":thread,"cursor":cursor,"limit":100}),
        )
        .await?;
        let entries = page["data"].as_array().ok_or("Missing native queue page")?;
        if entries
            .iter()
            .any(|entry| entry["clientUserMessageId"].as_str() == Some(client))
        {
            return Ok(true);
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        let Some(next) = cursor.as_ref() else {
            return Ok(false);
        };
        if !seen.insert(next.clone()) {
            return Err("Native queue repeated a cursor".into());
        }
    }
    Err("Native queue history is incomplete".into())
}

/// A turn's status in native history, and whether its end was recorded
/// (`completedAt`): an unfinished turn of another process reads interrupted
/// without one.
async fn native_turn_status(
    rpc: &RpcClient,
    thread: &str,
    turn: &str,
) -> Result<Option<(String, bool)>, String> {
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..100 {
        let page = result(
            rpc,
            "thread/turns/list",
            json!({"threadId": thread,
            "limit": 20, "sortDirection": "desc", "itemsView": "notLoaded", "cursor": cursor}),
        )
        .await?;
        let turns = page["data"].as_array().ok_or("Missing native turns")?;
        if let Some(found) = turns.iter().find(|t| t["id"].as_str() == Some(turn)) {
            let ended = found["completedAt"].as_i64().is_some_and(|at| at >= 0);
            return found["status"]
                .as_str()
                .map(|s| Some((s.to_owned(), ended)))
                .ok_or_else(|| "Missing native turn status".into());
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        let Some(next) = cursor.as_ref() else {
            return Ok(None);
        };
        if !seen.insert(next.clone()) {
            return Err("Native turns repeated a cursor".into());
        }
    }
    Err("Native turn history is incomplete".into())
}

/// Unsubscribe retains Codex's writer lock for an inactivity grace period.
/// Close our normal-home process once all of its loaded work is idle instead.
/// Other project work, native children, goals, terminals and captured read/RPC
/// handles retain the process until their owned work has finished.
async fn release_idle_codex_runtime(state: &AppState) {
    release_codex_runtime_when_idle(state, false).await;
}

/// Shut the Projects Codex runtime down once nothing uses it. A `stale`
/// runtime (ChatGPT installed a newer Codex) is released even when it holds
/// only discovery state, so the next request starts the installed version.
/// Returns whether it was shut down.
pub(crate) async fn release_codex_runtime_when_idle(state: &AppState, stale: bool) -> bool {
    release_codex_runtime_when_idle_at(state, stale, Utc::now().timestamp_millis()).await
}

async fn release_codex_runtime_when_idle_at(state: &AppState, stale: bool, now_ms: i64) -> bool {
    let runtime = &state.projects;
    let (rpc, generation) = {
        let _start = runtime.start.lock().await;
        let client = runtime.codex.lock().await;
        if !client.health().is_alive()
            || client.health().storage_blocked()
            || client.has_rpc_handles()
        {
            return false;
        }
        (client.rpc(), client.health().id().to_owned())
    };
    // Native idle checks may take several RPCs per loaded thread. An update
    // can proceed while they run; the final shutdown has its own admission.
    if state.update_admission.work_paused() {
        return false;
    }
    let Ok(loaded) = result(&rpc, "thread/loaded/list", json!({})).await else {
        return false;
    };
    if state.update_admission.work_paused() {
        return false;
    }
    let Some(threads) = loaded["data"].as_array() else {
        return false;
    };
    // Discovery alone holds no writer and needs no restart unless it runs a
    // replaced binary. Incomplete or unsupported lifecycle responses must
    // never authorize process shutdown.
    if (threads.is_empty() && !stale) || !loaded["nextCursor"].is_null() {
        return false;
    }
    let Ok(messages) = state.store.active_runtime_messages().await else {
        return false;
    };
    // Queued user intent remains in SQLite for the next dispatch tick. An
    // admitted turn with unconfirmed completion keeps its runtime alive.
    for message in &messages {
        if threads
            .iter()
            .any(|thread| thread.as_str() == message.codex_thread_id.as_deref())
            && (message.state != "uncertain"
                || native_queue_holds_runtime(state, message, now_ms).await)
        {
            // A loaded native queue needs this owner to remain alive through
            // its idle polling interval, bounded by the durable enqueue time.
            return false;
        }
    }
    let Ok(queued) = state.store.ambiguous_dispatch_messages().await else {
        return false;
    };
    for message in queued {
        if threads
            .iter()
            .any(|thread| thread.as_str() == message.codex_thread_id.as_deref())
            && native_queue_holds_runtime(state, &message, now_ms).await
        {
            return false;
        }
    }
    for thread in threads {
        if state.update_admission.work_paused() {
            return false;
        }
        let Some(thread) = thread.as_str() else {
            return false;
        };
        let Ok(read) = result(&rpc, "thread/read", json!({"threadId": thread})).await else {
            return false;
        };
        if state.update_admission.work_paused() {
            return false;
        }
        if read["thread"]["id"].as_str() != Some(thread)
            || read["thread"]["status"]["type"].as_str() != Some("idle")
        {
            return false;
        }
        let Ok(goal) = result(&rpc, "thread/goal/get", json!({"threadId": thread})).await else {
            return false;
        };
        if state.update_admission.work_paused() {
            return false;
        }
        if !goal.get("goal").is_some_and(|g| {
            g.is_null()
                || matches!(
                    g["status"].as_str(),
                    Some("complete" | "blocked" | "paused" | "budgetLimited" | "usageLimited")
                )
        }) {
            return false;
        }
        let Ok(background) = result(
            &rpc,
            "thread/backgroundTerminals/list",
            json!({"threadId": thread, "limit": 1}),
        )
        .await
        else {
            return false;
        };
        if !background["data"].as_array().is_some_and(Vec::is_empty)
            || !background["nextCursor"].is_null()
        {
            return false;
        }
    }
    drop(rpc);
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return false;
    };
    let _start = runtime.start.lock().await;
    let mut client = runtime.codex.lock().await;
    if !client.health().is_alive()
        || client.health().storage_blocked()
        || client.health().id() != generation
        || client.has_rpc_handles()
    {
        return false;
    }
    // A native child may have appeared after the unlocked probe without a
    // Wonder message. Recheck the roster once while shutdown is fenced.
    let final_rpc = client.rpc();
    let final_loaded = tokio::time::timeout(
        IDLE_FINAL_ROSTER_TIMEOUT,
        result(&final_rpc, "thread/loaded/list", json!({})),
    )
    .await;
    drop(final_rpc);
    let Ok(Ok(final_loaded)) = final_loaded else {
        return false;
    };
    let Some(final_threads) = final_loaded["data"].as_array() else {
        return false;
    };
    let probed = threads
        .iter()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let current = final_threads
        .iter()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    if !final_loaded["nextCursor"].is_null()
        || probed.len() != threads.len()
        || current.len() != final_threads.len()
        || current != probed
    {
        return false;
    }
    // No global dispatch lock is held while probing or waiting for this
    // runtime's client. Retire only when no submission still owes a receipt;
    // a contending send makes this optional cleanup yield immediately.
    let Ok(retiring) = state.provider_reconnect.try_write() else {
        return false;
    };
    let Ok(current) = state.store.active_runtime_messages().await else {
        return false;
    };
    // A queue intent may have been committed during the unlocked idle probe.
    // Recheck its bounded hold under the same admission fence as direct sends.
    for message in current
        .iter()
        .filter(|message| message.state == "uncertain")
    {
        if native_queue_holds_runtime(state, message, now_ms).await {
            return false;
        }
    }
    let Ok(queued) = state.store.ambiguous_dispatch_messages().await else {
        return false;
    };
    for message in queued {
        if native_queue_holds_runtime(state, &message, now_ms).await {
            return false;
        }
    }
    for message in current
        .iter()
        .filter(|message| message.state != "uncertain")
    {
        if threads
            .iter()
            .any(|thread| thread.as_str() == message.codex_thread_id.as_deref())
        {
            return false;
        }
        // A new Project turn may have started after the idle snapshot on a
        // thread that was not in that snapshot. Fail closed on store errors.
        match state
            .store
            .project_conversation(&message.conversation_id)
            .await
        {
            Ok(Some(conversation)) if conversation.family == AgentFamily::Codex => return false,
            Err(_) => return false,
            _ => {}
        }
    }
    // The client/start locks prevent new handles until shutdown completes.
    // Other providers can submit while this child takes time to exit.
    drop(retiring);
    let _ = client.shutdown().await;
    true
}

/// Reuse a native Codex project with the same folders, otherwise create one
/// with an idempotency key. Unsupported runtimes simply omit project identity.
async fn native_codex_project(
    state: &AppState,
    rpc: &RpcClient,
    project: &StoredProject,
    store: &str,
) -> Option<String> {
    let store = store.to_owned();
    let existing = state
        .store
        .project_provider_ref(&project.id, AgentFamily::Codex, &store)
        .await
        .ok()?;
    let wanted: HashSet<String> = project
        .roots
        .iter()
        .map(|r| r.canonical_path.clone())
        .collect();
    if let Some(id) = existing.as_deref() {
        if let Ok(read) = result(rpc, "project/read", json!({"projectId": id})).await {
            if read.pointer("/project/id").and_then(Value::as_str) == Some(id)
                && native_project_roots_match(&read["project"], &wanted)
            {
                return Some(id.to_owned());
            }
        }
    }
    let listed = result(rpc, "project/list", json!({"limit": 100}))
        .await
        .ok()?;
    let matched = listed
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|native| {
            native_project_roots_match(native, &wanted)
                .then(|| native.get("id").and_then(Value::as_str).map(str::to_owned))
                .flatten()
        });
    let id = match matched {
        Some(id) => id,
        None => {
            let primary = project.primary_root();
            let mut roots = vec![json!({"path": primary.path})];
            roots.extend(
                project
                    .roots
                    .iter()
                    .filter(|r| r.id != primary.id)
                    .map(|r| json!({"path": r.path})),
            );
            // A folder edit needs a new identity even when an older project
            // creation key still points at the previous folders.
            let mut identity_roots = wanted.iter().collect::<Vec<_>>();
            identity_roots.sort();
            let roots_hash = hex::encode(Sha256::digest(serde_json::to_vec(&identity_roots).ok()?));
            let created = result(rpc, "project/create", json!({"idempotencyKey": format!("wonder-{}-{roots_hash}", project.id), "name": project.name, "roots": roots})).await.ok()?;
            if !native_project_roots_match(&created["project"], &wanted) {
                return None;
            }
            created
                .pointer("/project/id")
                .and_then(Value::as_str)?
                .to_owned()
        }
    };
    let winner = match existing {
        Some(previous) => {
            state
                .store
                .replace_project_provider_ref(
                    &project.id,
                    AgentFamily::Codex,
                    &store,
                    &previous,
                    &id,
                )
                .await
        }
        None => {
            state
                .store
                .set_project_provider_ref(&project.id, AgentFamily::Codex, &store, &id)
                .await
        }
    }
    .ok()?;
    if winner == id {
        return Some(winner);
    }
    // Another caller won the first insert or stale-reference replacement.
    // Never associate a thread using that unchecked destination.
    let read = result(rpc, "project/read", json!({"projectId": winner}))
        .await
        .ok()?;
    (read.pointer("/project/id").and_then(Value::as_str) == Some(winner.as_str())
        && native_project_roots_match(&read["project"], &wanted))
    .then_some(winner)
}

fn native_project_roots_match(native: &Value, wanted: &HashSet<String>) -> bool {
    let Some(roots) = native.get("roots").and_then(Value::as_array) else {
        return false;
    };
    let Some(paths) = roots
        .iter()
        .map(|root| root.get("path").and_then(Value::as_str))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let canonical: HashSet<String> = paths
        .into_iter()
        .map(|path| {
            crate::canonical_path::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_owned())
        })
        .collect();
    canonical == *wanted
}

/// Imported or older native threads can have a cwd without a desktop project.
/// Repair only unassigned, in-project roots; preserve an owner's existing choice.
async fn associate_codex_project(
    state: &AppState,
    project: &StoredProject,
    native: &str,
) -> Result<(), String> {
    let rpc = codex_rpc(state).await?;
    let read = result(&rpc, "thread/read", json!({"threadId": native}))
        .await
        .map_err(|_| "This Codex thread could not be checked on your computer.".to_owned())?;
    let thread = read.get("thread").ok_or("Codex returned no thread.")?;
    if thread.get("id").and_then(Value::as_str) != Some(native) {
        return Err("Codex returned a different thread. Nothing was changed.".into());
    }
    let cwd = thread
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !project_folders(state, project).await.contains(cwd)
        || thread
            .get("parentThreadId")
            .and_then(Value::as_str)
            .is_some()
    {
        return Err("This thread is outside the project's folders.".into());
    }
    if thread
        .get("projectId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        return Ok(());
    }
    let Some(id) = native_codex_project(state, &rpc, project, &state.projects.codex_store).await
    else {
        // Older runtimes can still continue a thread by cwd alone.
        return Ok(());
    };
    let response = rpc
        .request(
            "thread/metadata/update",
            json!({"threadId": native, "projectId": id}),
        )
        .await
        .map_err(|_| {
            "This thread's desktop project could not be saved. Try opening it again.".to_owned()
        })?;
    if let Some(error) = response.error {
        return if error.code == -32601 {
            Ok(())
        } else {
            Err("This thread's desktop project could not be saved. Try opening it again.".into())
        };
    }
    let saved = response.result.ok_or("Codex returned no saved project.")?;
    if saved.pointer("/thread/id").and_then(Value::as_str) != Some(native)
        || saved.pointer("/thread/projectId").and_then(Value::as_str) != Some(id.as_str())
    {
        return Err("This thread's desktop project was not saved. Try opening it again.".into());
    }
    Ok(())
}

const RECEIPT_HISTORY_PAGE_LIMIT: usize = 10_000;
const RECEIPT_HISTORY_LIMIT_NOTICE: &str = "This Project's history is too long to verify the message automatically. Check this thread on your computer before sending it again; Cupola will retry verification.";
const RECEIPT_HISTORY_INVALID_NOTICE: &str = "This Project's history could not be verified automatically. Check this thread on your computer before sending it again; Cupola will retry verification.";

/// Resolve an uncertain project send by its client message identity.
pub(crate) async fn find_accepted_turn(
    state: &AppState,
    message: &wonder_store::StoredMessage,
) -> Result<Option<String>, String> {
    find_accepted_turn_with_limit(state, message, RECEIPT_HISTORY_PAGE_LIMIT).await
}

async fn find_accepted_turn_with_limit(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    page_limit: usize,
) -> Result<Option<String>, String> {
    let Some(thread) = message.codex_thread_id.as_deref() else {
        return Ok(None);
    };
    let Some(conversation) = state
        .store
        .project_conversation(&message.conversation_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let rpc = {
        let Some(_admission) = state.update_admission.claim_guard().await else {
            return Ok(None);
        };
        crate::providers::rpc_for_store(state, conversation.family, &conversation.provider_store)
            .await?
    };
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..page_limit {
        let page = result(
            &rpc,
            "thread/items/list",
            json!({"threadId": thread, "limit": 100, "sortDirection": "desc", "cursor": cursor}),
        )
        .await?;
        if page.get("data").and_then(Value::as_array).is_none() {
            return Err("Project history returned no items page".into());
        }
        if let Some(turn) = crate::find_client_message(&page, &message.client_message_id) {
            return Ok(Some(turn));
        }
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(None);
        }
        if !seen.insert(cursor.as_ref().unwrap().clone()) {
            return Err("Project history repeated a continuation cursor".into());
        }
    }
    Err(RECEIPT_HISTORY_LIMIT_NOTICE.into())
}

/// Wonder-owned storage for media a project tool returned. It is never the
/// owner's project folder.
pub(crate) async fn media_workspace(state: &AppState, conversation: &str) -> Option<String> {
    let owner = state
        .store
        .owning_project_conversation(conversation)
        .await
        .ok()??;
    // Child preview storage is kept with its verified Project ancestor.
    let conversation = owner.conversation_id.as_str();
    // A local symlink must never redirect an authenticated upload or grant the
    // provider access to a different conversation's private data.
    uuid::Uuid::parse_str(conversation).ok()?;
    let parent = crate::canonical_path::canonicalize_async(&state.data_dir)
        .await
        .ok()?;
    let mut root = parent.clone();
    for component in ["project-media", conversation] {
        root.push(component);
        match tokio::fs::create_dir(&root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        let metadata = tokio::fs::symlink_metadata(&root).await.ok()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return None;
        }
    }
    let root = crate::canonical_path::canonicalize_async(root).await.ok()?;
    if !root.starts_with(&parent) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).await;
    }
    root.to_str().map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Continue on Mac
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContinuationOption {
    id: &'static str,
    title: String,
    detail: String,
    command: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContinuationResponse {
    options: Vec<ContinuationOption>,
    notes: Vec<String>,
    /// Why there is no option, when the provider has no Terminal resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<String>,
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Exact-ID terminal commands generated from validated metadata. Copying a
/// command never executes it; Wonder does not open desktop apps remotely.
pub(crate) async fn continuation(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
) -> Response {
    let Ok(Some(conversation)) = state.store.project_conversation(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !crate::providers::is_default_store(
        &state,
        conversation.family,
        &conversation.provider_store,
    ) {
        return error(
            StatusCode::CONFLICT,
            crate::providers::other_history(conversation.family),
        );
    }
    let Some(native) = conversation.native_session_id.clone() else {
        return error(
            StatusCode::CONFLICT,
            "Send the first message before continuing on your computer.",
        );
    };
    let Ok(Some(project)) = state.store.project(&conversation.project_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let extra = project
        .roots
        .iter()
        .filter(|r| r.path != conversation.cwd)
        .flat_map(|r| ["--add-dir".to_owned(), shell_quote(&r.path)])
        .collect::<Vec<_>>();
    let cd = format!("cd {}", shell_quote(&conversation.cwd));
    let mut notes = Vec::new();
    let options = match conversation.family {
        AgentFamily::Codex => vec![ContinuationOption {
            id: "codex-cli",
            title: "Continue in Codex CLI".into(),
            detail: "Resumes this exact thread in Terminal with your usual Codex settings.".into(),
            command: [
                vec![
                    cd,
                    "&&".into(),
                    "codex".into(),
                    "resume".into(),
                    shell_quote(&native),
                ],
                extra,
            ]
            .concat()
            .join(" "),
        }],
        AgentFamily::Claude => {
            notes.push("Use a current Claude Code version. Older versions can split the conversation when resuming.".into());
            vec![ContinuationOption {
                id: "claude-code",
                title: "Continue in Claude Code".into(),
                detail: "Resumes this exact session in Terminal. Type /desktop there to move it to the Claude app.".into(),
                command: [vec![cd, "&&".into(), "claude".into(), "--resume".into(), shell_quote(&native)], extra].concat().join(" "),
            }]
        }
        // Whether Cursor Agent's own resume accepts these session IDs is
        // unverified, so no command is offered.
        AgentFamily::Cursor => Vec::new(),
        AgentFamily::OpenCode => vec![ContinuationOption {
            id: "opencode",
            title: "Continue in OpenCode".into(),
            detail: "Opens this exact session in OpenCode in Terminal.".into(),
            command: [
                cd,
                "&&".into(),
                "opencode".into(),
                "--session".into(),
                shell_quote(&native),
            ]
            .join(" "),
        }],
    };
    if state
        .store
        .conversation_has_active_turn(&id)
        .await
        .unwrap_or(false)
    {
        notes.push("Cupola is still working in this thread. Wait for it to finish, or stop it, before continuing on your computer.".into());
    }
    let unavailable = options.is_empty().then(|| {
        format!(
            "{} chats can’t be continued in Terminal yet.",
            conversation.family.provider().display_name
        )
    });
    if unavailable.is_some() {
        notes.clear();
    } else {
        notes.push("When you come back, Cupola shows the turns you added on your computer.".into());
    }
    Json(ContinuationResponse {
        options,
        notes,
        unavailable,
    })
    .into_response()
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn live_turn_runs_until_history_has_missed_it_for_the_whole_ttl() {
        let start = Instant::now();
        let ttl = Duration::from_secs(60);
        assert_eq!(live_turn(None, start, ttl), LiveTurn::Running);
        assert_eq!(live_turn(Some(start), start, ttl), LiveTurn::Running);
        assert_eq!(
            live_turn(Some(start), start + ttl - Duration::from_secs(1), ttl),
            LiveTurn::Running
        );
        assert_eq!(live_turn(Some(start), start + ttl, ttl), LiveTurn::Unlisted);
    }

    /// A Mac without Codex has nothing to report about it; a Codex that is
    /// installed but fails to answer still says so.
    #[tokio::test]
    async fn candidates_stay_silent_about_a_codex_that_is_not_installed() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let partial = |state: AppState| async move {
            let response = candidates(State(state), Extension(OwnerAuthority)).await;
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            value["partial"].clone()
        };
        // The fixture's stub runtime is stopped so the next request must start Codex.
        let _ = state.projects.codex.lock().await.shutdown().await;
        state
            .projects
            .set_codex_bin(dir.path().join("no-such-codex"));
        assert_eq!(partial(state.clone()).await, json!([]));
        // An installed program that cannot serve the request is a real failure.
        let broken = dir.path().join("broken-codex");
        std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        state.projects.set_codex_bin(broken);
        assert_eq!(
            partial(state).await,
            json!([{"family": "codex", "detail": "Codex projects could not be loaded."}])
        );
    }

    #[tokio::test]
    async fn a_listed_turn_resets_the_unlisted_clock() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let runtime = &state.projects;
        let t0 = Instant::now();
        assert_eq!(runtime.observe_listing("m", "turn", false, t0), Some(t0));
        let later = t0 + Duration::from_secs(30);
        assert_eq!(runtime.observe_listing("m", "turn", false, later), Some(t0));
        assert_eq!(runtime.observe_listing("m", "turn", true, later), None);
        assert_eq!(
            runtime.observe_listing("m", "turn", false, later),
            Some(later)
        );
        // A new turn id for the same message starts its own clock.
        let newer = later + Duration::from_secs(1);
        assert_eq!(
            runtime.observe_listing("m", "turn-2", false, newer),
            Some(newer)
        );
    }

    // SM-01: a stored turn id that native history never lists, while the
    // accepting runtime stays alive, used to leave the message streaming and
    // the row working through every recover() pass.
    #[tokio::test]
    async fn live_runtime_turn_missing_from_native_history_settles_after_the_ttl() {
        let (dir, state, message) = handoff_fixture().await;
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id": "other-turn", "status": "completed", "completedAt": 1}]]).to_string(),
        )
        .unwrap();
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        // Inside the TTL the live runtime is trusted.
        for _ in 0..3 {
            recover_with_ttl(&state, Duration::from_secs(3600)).await;
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "streaming");
        assert!(
            attached_summary(&state, &conversation, 0, None)
                .await
                .is_working
        );
        // Past it, the turn settles like one whose runtime is gone.
        for _ in 0..3 {
            recover_with_ttl(&state, Duration::ZERO).await;
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        // The runtime-gone settlement: shown failed, receipt kept uncertain.
        assert_eq!(saved.state, "uncertain");
        assert!(
            !attached_summary(&state, &conversation, 0, None)
                .await
                .is_working
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // A turn native history lists as running stays running, however long.
    #[tokio::test]
    async fn live_runtime_turn_native_history_lists_as_running_is_never_timed_out() {
        let (dir, state, message) = handoff_fixture().await;
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id": "turn", "status": "inProgress"}]]).to_string(),
        )
        .unwrap();
        for _ in 0..3 {
            recover_with_ttl(&state, Duration::ZERO).await;
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "streaming");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn settled_turn_generations_are_forgotten_after_the_grace() {
        let recorded = Instant::now();
        let mut generations: HashMap<String, (String, Instant)> =
            ["running", "settled", "just-sent"]
                .into_iter()
                .map(|id| (id.to_owned(), ("runtime".to_owned(), recorded)))
                .collect();
        generations.get_mut("just-sent").unwrap().1 = recorded + GENERATION_SETTLE_GRACE;
        let active = HashSet::from(["running"]);
        // A receipt being stored right now is not active yet; keep it.
        retain_running_generations(&mut generations, &active, recorded + Duration::from_secs(1));
        assert_eq!(generations.len(), 3);
        retain_running_generations(
            &mut generations,
            &active,
            recorded + GENERATION_SETTLE_GRACE,
        );
        let mut kept = generations.keys().map(String::as_str).collect::<Vec<_>>();
        kept.sort_unstable();
        assert_eq!(kept, ["just-sent", "running"]);
    }

    #[test]
    fn only_native_rejection_data_proves_a_turn_was_not_accepted() {
        let error = |code, data| JsonRpcError {
            code,
            message: "arbitrary display text".into(),
            data,
        };
        for (data, expected) in [
            (
                json!({"turnStart":{"accepted":false,"action":"wait"}}),
                Some(InputRejection::Wait),
            ),
            (
                json!({"turnStart":{"accepted":false,"action":"retry"}}),
                Some(InputRejection::Retry),
            ),
            (json!({"turnStart":{"accepted":true,"action":"wait"}}), None),
            (
                json!({"turnStart":{"accepted":"false","action":"wait"}}),
                None,
            ),
            (
                json!({"turnStart":{"accepted":false,"action":"future"}}),
                None,
            ),
            (json!({"turnStart":{"action":"wait"}}), None),
            (Value::Null, None),
        ] {
            let e = error(-32000, Some(data));
            assert_eq!(
                input_rejection(AgentFamily::Claude, "turn/start", &e),
                expected
            );
            assert_eq!(input_rejection(AgentFamily::Codex, "turn/start", &e), None);
            for acp in [AgentFamily::OpenCode, AgentFamily::Cursor] {
                assert_eq!(input_rejection(acp, "turn/start", &e), expected);
            }
        }
        assert_eq!(
            input_rejection(AgentFamily::Codex, "turn/start", &error(-32001, None)),
            Some(InputRejection::Retry)
        );
        assert_eq!(
            input_rejection(AgentFamily::Claude, "turn/start", &error(-32001, None)),
            None
        );
    }

    // Blank, structurally valid two-page PDF for provider-input tests. Offsets
    // are calculated from the emitted objects so a PDF parser can check page 2.
    #[cfg(target_os = "macos")]
    fn two_page_pdf() -> Vec<u8> {
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>",
        ];
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = bytes.len();
        bytes.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(
            format!("trailer\n<< /Root 1 0 R /Size 5 >>\nstartxref\n{xref}\n").as_bytes(),
        );
        bytes.extend_from_slice(b"%%EOF\n");
        bytes
    }

    pub(crate) async fn handoff_fixture(
    ) -> (tempfile::TempDir, AppState, wonder_store::StoredMessage) {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        state.projects = ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "project-chat",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: Some("thread"),
                cwd: source.to_str().unwrap(),
                roots_revision: 1,
                title: "Recovery",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                "project-chat",
                AgentFamily::Codex,
                &state.projects.codex_store,
                "thread",
                None,
                "now",
            )
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                "receipt",
                "work",
                "hash",
                "project-chat",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{
            "id":"thread", "status":{"type":"idle"}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("lock-fixture"), "").unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        state.projects.generations.lock().unwrap().insert(
            message.id.clone(),
            (rpc.health().id().into(), Instant::now()),
        );
        drop(rpc);
        (dir, state, message)
    }

    pub(crate) async fn dispatch_failure_fixture(
    ) -> (tempfile::TempDir, AppState, wonder_store::StoredMessage) {
        let (dir, mut state) = crate::test_support::fixture().await;
        state
            .runtime_catalog
            .write()
            .await
            .models
            .iter_mut()
            .find(|model| model.id == "fake")
            .unwrap()
            .default_service_tier = Some("default".into());
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        // The home exists in production; create it before deriving its store
        // identity so macOS /var canonicalization stays stable on restart.
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        state.projects = ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let conversation = uuid::Uuid::new_v4().to_string();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: &conversation,
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd: source.to_str().unwrap(),
                roots_revision: 1,
                title: "Recovery",
                model: Some("fake"),
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                &uuid::Uuid::new_v4().to_string(),
                "Do the task",
                "hash",
                &conversation,
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        (dir, state, message)
    }

    /// A Codex chat whose thread ChatGPT's desktop app holds, with a stand-in
    /// for that app's router answering start-turn with `reply`.
    #[cfg(target_os = "macos")]
    async fn desktop_relay_fixture(
        reply: crate::codex_desktop_ipc::tests::Reply,
    ) -> (
        tempfile::TempDir,
        AppState,
        wonder_store::StoredMessage,
        crate::codex_desktop_ipc::tests::FakeRouter,
    ) {
        let (dir, state, message) = dispatch_failure_fixture().await;
        // Its own thread: a refusal rests the relay for that thread only.
        let thread = uuid::Uuid::new_v4().to_string();
        crate::test_support::bind_project_thread(
            &state,
            &message.conversation_id,
            AgentFamily::Codex,
            &thread,
            None,
            "now",
        )
        .await
        .unwrap();
        std::fs::write(dir.path().join("resume-active-writer"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({thread.clone():{"id":thread,"status":{"type":"idle"}}}).to_string(),
        )
        .unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        state.runtime_catalog.write().await.apply_models_page(
            AgentFamily::Codex,
            &json!({"data":[{"id":"fake","model":"fake","displayName":"Fake","isDefault":true}]}),
        );
        *state.projects.native_queue_capability.lock().await =
            Some((rpc.health().id().into(), true));
        let router = crate::codex_desktop_ipc::tests::FakeRouter::start(
            true,
            HashMap::from([("thread-follower-start-turn", reply)]),
        );
        router.serve_thread(&thread);
        (dir, state, message, router)
    }

    #[cfg(target_os = "macos")]
    fn runtime_methods(dir: &tempfile::TempDir) -> Vec<String> {
        std::fs::read_to_string(dir.path().join("requests-jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|r| r["method"].as_str().map(str::to_owned))
            .collect()
    }

    // Contract: a thread ChatGPT's desktop app holds gets the message through
    // that app, exactly once, keyed by its client id; Wonder neither resumes
    // the thread nor queues it in Codex.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn desktop_held_codex_thread_gets_the_message_through_chatgpt_once() {
        use crate::codex_desktop_ipc::tests::Reply;
        let (dir, state, message, router) =
            desktop_relay_fixture(Reply::Turn("desktop-turn")).await;
        dispatch(state.clone(), message.clone()).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (saved.state.as_str(), saved.codex_turn_id.as_deref()),
            ("accepted_by_codex", Some("desktop-turn")),
            "{:?}",
            state.store.message_delivery_error(&message.id).await
        );
        let starts = router
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "thread-follower-start-turn")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1);
        assert_eq!(
            starts[0]["params"]["turnStart"]["request"]["clientUserMessageId"],
            message.client_message_id.as_str()
        );
        assert!(desktop_relayed(&state, &message.id).await.is_some());
        let runtime = runtime_methods(&dir);
        assert!(
            !runtime
                .iter()
                .any(|m| m == "thread/resume" || m == "turn/start" || m == "thread/queue/add"),
            "{runtime:?}"
        );
        // The accepted message is never claimed again.
        state.store.recover_dispatch_claims().await.unwrap();
        dispatch(state.clone(), message.clone()).await;
        assert_eq!(
            router
                .methods()
                .iter()
                .filter(|m| *m == "thread-follower-start-turn")
                .count(),
            1
        );
    }

    // Contract: an owner that refuses the method version sends nothing, so
    // the message waits and then goes to Codex's own queue, once.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn chatgpt_refusing_the_relay_version_falls_back_to_codex_queue() {
        use crate::codex_desktop_ipc::tests::Reply;
        let (dir, state, message, router) =
            desktop_relay_fixture(Reply::Error("no-client-found")).await;
        dispatch(state.clone(), message.clone()).await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder",
            "nothing was sent, so it waits"
        );
        assert!(!runtime_methods(&dir)
            .iter()
            .any(|m| m == "thread/queue/add"));
        dispatch(state.clone(), message.clone()).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "uncertain");
        assert!(desktop_relayed(&state, &message.id).await.is_none());
        assert_eq!(
            runtime_methods(&dir)
                .iter()
                .filter(|m| *m == "thread/queue/add")
                .count(),
            1
        );
        assert_eq!(
            router
                .methods()
                .iter()
                .filter(|m| *m == "thread-follower-start-turn")
                .count(),
            1
        );
    }

    // Contract: an answer lost after the request reached ChatGPT may hide a
    // started turn; the message is unconfirmed and never sent again.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_lost_chatgpt_answer_is_never_resent() {
        use crate::codex_desktop_ipc::tests::Reply;
        let (dir, state, message, router) = desktop_relay_fixture(Reply::Drop).await;
        dispatch(state.clone(), message.clone()).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "uncertain");
        assert_eq!(
            state
                .store
                .message_delivery_error(&message.id)
                .await
                .unwrap()
                .as_deref(),
            Some(DESKTOP_RELAY_UNCONFIRMED)
        );
        assert!(!state
            .store
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
        state.store.recover_dispatch_claims().await.unwrap();
        dispatch(state.clone(), message.clone()).await;
        assert_eq!(
            router
                .methods()
                .iter()
                .filter(|m| *m == "thread-follower-start-turn")
                .count(),
            1
        );
        assert!(!runtime_methods(&dir)
            .iter()
            .any(|m| m == "thread/queue/add" || m == "turn/start"));
    }

    // Contract: Stop on a relayed turn reaches ChatGPT, which interrupts only
    // that turn; Wonder's own runtime, which is not running it, is not asked.
    // A turn already ended there reads as ended, and a ChatGPT that can't be
    // reached is refused with where to stop it instead.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn stop_on_a_relayed_turn_goes_to_chatgpt() {
        use crate::codex_desktop_ipc::tests::{FakeRouter, Reply};
        let (dir, state, message, _router) =
            desktop_relay_fixture(Reply::Turn("desktop-turn")).await;
        dispatch(state.clone(), message.clone()).await;
        let thread = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap()
            .codex_thread_id
            .unwrap();
        for (reply, status, body) in [
            (
                Some(Reply::Interrupted(Some("desktop-turn"))),
                StatusCode::ACCEPTED,
                "stopConfirmed",
            ),
            (Some(Reply::Interrupted(None)), StatusCode::NO_CONTENT, ""),
            // An answer naming no turn: the turn may still run there.
            (
                Some(Reply::Ok),
                StatusCode::BAD_GATEWAY,
                "didn't confirm Stop",
            ),
            // ChatGPT no longer has the thread open.
            (None, StatusCode::CONFLICT, "Stop it in ChatGPT there."),
        ] {
            let owner = reply.is_some();
            let router = FakeRouter::start(
                owner,
                reply
                    .map(|reply| HashMap::from([("thread-follower-interrupt-turn", reply)]))
                    .unwrap_or_default(),
            );
            router.serve_thread(&thread);
            let response = crate::interrupt_turn(
                State(state.clone()),
                Path((message.conversation_id.clone(), "desktop-turn".into())),
            )
            .await;
            assert_eq!(response.status(), status);
            let text = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&text).contains(body),
                "{status}: {text:?}"
            );
            if owner {
                let request = router.requests.lock().unwrap().last().unwrap().clone();
                assert_eq!(request["method"], "thread-follower-interrupt-turn");
                assert_eq!(request["params"]["expectedTurnId"], "desktop-turn");
            }
        }
        // The accepted Stop survives the later refused ones: ChatGPT's
        // interrupted record settles it as the owner's Stop.
        assert!(stop_requested(&state, &thread, "desktop-turn")
            .await
            .unwrap());
        assert!(!runtime_methods(&dir).iter().any(|m| m == "turn/interrupt"));
    }

    // Contract: submission cannot precede its journal, and a missing/malformed
    // queue receipt never becomes retry permission even after claim recovery.
    #[tokio::test]
    async fn codex_native_queue_journal_failure_and_unknown_receipt_never_duplicate() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        crate::test_support::bind_project_thread(
            &state,
            &message.conversation_id,
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        std::fs::write(dir.path().join("resume-active-writer"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{"id":"thread","status":{"type":"idle"}}}).to_string(),
        )
        .unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        state.runtime_catalog.write().await.apply_models_page(
            AgentFamily::Codex,
            &json!({"data":[{"id":"fake","model":"fake","displayName":"Fake","isDefault":true}]}),
        );
        // Capability probing is tested against the real binary separately;
        // this fixture deliberately returns a malformed queue acknowledgement.
        *state.projects.native_queue_capability.lock().await =
            Some((rpc.health().id().into(), true));
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER fail_enqueue_journal BEFORE UPDATE OF phase ON dispatch_attempts WHEN NEW.phase='submitting' BEGIN SELECT RAISE(ABORT, 'enqueue journal unavailable'); END")
            .execute(&pool).await.unwrap();
        dispatch(state.clone(), message.clone()).await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "safe_to_retry"
        );
        let count_adds = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|r| r["method"] == "thread/queue/add")
                .count()
        };
        assert_eq!(count_adds(), 0);
        assert!(state
            .store
            .message_delivery_error(&message.id)
            .await
            .unwrap()
            .unwrap()
            .contains("enqueue journal unavailable"));
        sqlx::query("DROP TRIGGER fail_enqueue_journal")
            .execute(&pool)
            .await
            .unwrap();
        assert!(state
            .store
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
        dispatch(state.clone(), message.clone()).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.state,
            "uncertain",
            "{:?}",
            state
                .store
                .message_delivery_error(&message.id)
                .await
                .unwrap()
        );
        assert!(saved.codex_turn_id.is_none());
        assert_eq!(count_adds(), 1);
        assert!(!state
            .store
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
        state.store.recover_dispatch_claims().await.unwrap();
        dispatch(state.clone(), message.clone()).await;
        assert_eq!(count_adds(), 1);
        assert!(matches!(
            scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await,
            ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNCONFIRMED_NOTICE)
        ));
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        std::fs::write(
            dir.path().join("accepted-client"),
            &message.client_message_id,
        )
        .unwrap();
        // Proof alone is not read until a definite event; then it settles.
        assert!(!matches!(
            scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await,
            ReceiptScanStep::Accepted(_)
        ));
        // The desktop finishes and lets the thread go; Wonder takes it over and
        // its own send in this chat starts a turn.
        std::fs::remove_file(dir.path().join("resume-active-writer")).unwrap();
        let cwd = state
            .store
            .project_conversation(&message.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .cwd;
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{"id":"thread","cwd":cwd,"projectId":"native-project","status":{"type":"idle"}}})
                .to_string(),
        )
        .unwrap();
        let script = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&script).unwrap();
        std::fs::write(
            &script,
            source.replace(
                "else {'thread':{'status':{'type':'idle'}}}",
                "else {'thread':{'id':r['params']['threadId'],'status':{'type':'idle'}}}",
            ),
        )
        .unwrap();
        // The restarted runtime rediscovers its catalog from this list.
        std::fs::write(
            dir.path().join("models.json"),
            json!({"data":[{"id":"fake","model":"fake","displayName":"Fake","isDefault":true}]})
                .to_string(),
        )
        .unwrap();
        state.projects.shutdown().await;
        let MessageInsert::Inserted(next) = state
            .store
            .insert_dispatch_message(
                "owner",
                &uuid::Uuid::new_v4().to_string(),
                "Next task",
                "hash-next",
                &message.conversation_id,
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        dispatch(state.clone(), next.clone()).await;
        assert_eq!(
            state
                .store
                .message_by_id(&next.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_codex",
            "{:?}",
            state.store.message_delivery_error(&next.id).await.unwrap()
        );
        // History now holds Wonder's turn and the queued send it followed.
        std::fs::write(
            dir.path().join("accepted-client"),
            &message.client_message_id,
        )
        .unwrap();
        assert!(
            matches!(scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await,
            ReceiptScanStep::Accepted(turn) if turn == "turn")
        );
        assert_eq!(count_adds(), 1);
        state.projects.shutdown().await;
    }

    // Contract: the real pinned server's idle owner consumes exactly one
    // durable Wonder enqueue, including when the peer dies before its receipt.
    // Run explicitly on this Mac with bundled Codex and python3.14 on PATH
    // (compression.zstd is in Python 3.14+); no live provider is contacted.
    // Package tests must not require ChatGPT or Python 3.14.
    #[cfg(unix)]
    const NATIVE_QUEUE_FIXTURE: &str = r###""""Local-only deterministic provider and a crash-before-receipt stdio proxy."""
import compression.zstd
import json
import os
from pathlib import Path
import queue
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE, ROOT, CLI = sys.argv[1], Path(sys.argv[2]), sys.argv[3]
ARGS = sys.argv[4:]
HOME = ROOT / 'home'
WORK = ROOT / 'project-source'
LOCK = threading.Lock()

def record(name, value):
    with LOCK:
        with (ROOT / name).open('a') as f:
            f.write(json.dumps(value) + '\n')

if MODE == 'proxy':
    if ARGS == ['--version'] or 'generate-json-schema' in ARGS:
        os.execv(CLI, [CLI] + ARGS)
    child = subprocess.Popen([CLI] + ARGS, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=sys.stderr, text=True, bufsize=1)
    adds = set()
    def read():
        for line in child.stdout:
            frame = json.loads(line)
            record('peer-replies.jsonl', frame)
            if frame.get('id') in adds and 'result' in frame and (ROOT/'crash-mode').exists() and not (ROOT/'lost-receipt.json').exists():
                (ROOT/'lost-receipt.json').write_text(json.dumps(frame))
                child.kill()
                killed = child.wait()
                (ROOT/'peer-kill.json').write_text(json.dumps({'pid':child.pid,'returncode':killed,'before_receipt_forward':True}))
                # Keep transport open: the test kills the Wonder dispatch task
                # while its committed intent still has no receipt whatsoever.
                threading.Event().wait()
            print(line, end='', flush=True)
    threading.Thread(target=read, daemon=True).start()
    try:
        for line in sys.stdin:
            frame = json.loads(line)
            record('peer-requests.jsonl', frame)
            if frame.get('method') == 'thread/queue/add':
                with sqlite3.connect(ROOT/'state.db') as db:
                    phase, context = db.execute('SELECT phase,context_json FROM dispatch_attempts ORDER BY id DESC LIMIT 1').fetchone()
                assert phase == 'submitting' and json.loads(context)['deliveryMethod'] == 'thread/queue/add', (phase, context)
                record('journal-before-rpc.jsonl', {'phase':phase,'context':json.loads(context)})
                adds.add(frame['id'])
            child.stdin.write(line)
            child.stdin.flush()
    finally:
        child.stdin.close()
        child.wait(timeout=15)
    sys.exit()

REQUESTS, ACTIVE, MAX_ACTIVE = [], 0, 0
MODEL_LOCK = threading.Lock()
class Model(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *args): pass
    def do_POST(self):
        global ACTIVE, MAX_ACTIVE
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd': raw = compression.zstd.decompress(raw)
        body = json.loads(raw)
        with MODEL_LOCK:
            ACTIVE += 1
            MAX_ACTIVE = max(MAX_ACTIVE, ACTIVE)
            REQUESTS.append(body)
            n = len(REQUESTS)
        item = {'type':'message','id':'msg_'+str(n),'role':'assistant','status':'completed',
                'content':[{'type':'output_text','text':'Offline fixture reply '+str(n)+'.','annotations':[]}]}
        response = {'id':'resp_'+str(n),'object':'response','created_at':int(time.time()),'model':'fixture',
                    'status':'completed','output':[item],'usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}
        events = [{'type':'response.created','response':response|{'status':'in_progress','output':[]}},
                  {'type':'response.output_item.done','output_index':0,'item':item},
                  {'type':'response.completed','response':response}]
        data = ''.join('event: '+e['type']+'\ndata: '+json.dumps(e)+'\n\n' for e in events).encode()
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.send_header('Content-Length',str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()
        with MODEL_LOCK:
            ACTIVE -= 1
            (ROOT/'model.json').write_text(json.dumps({'requests':REQUESTS,'max_active':MAX_ACTIVE}))

HOME.mkdir(exist_ok=True)
http = ThreadingHTTPServer(('127.0.0.1',0),Model)
threading.Thread(target=http.serve_forever,daemon=True).start()
(HOME/'config.toml').write_text(f'''model = "fixture"
model_provider = "fixture"
[model_providers.fixture]
name = "Offline fixture"
base_url = "http://127.0.0.1:{http.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
[features]
shell_snapshot = false
''')
owner = subprocess.Popen([CLI,'app-server','--stdio'],cwd=WORK,env=os.environ|{'CODEX_HOME':str(HOME)},
                         stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=sys.stderr,text=True,bufsize=1)
REPLIES, COMPLETED = queue.Queue(), threading.Event()
def read_owner():
    for line in owner.stdout:
        frame=json.loads(line)
        record('owner-rpc.jsonl',frame)
        if frame.get('method')=='turn/completed': COMPLETED.set()
        REPLIES.put(frame)
threading.Thread(target=read_owner,daemon=True).start()
NEXT=0
def call(method,params):
    global NEXT
    NEXT+=1
    owner.stdin.write(json.dumps({'id':NEXT,'method':method,'params':params})+'\n')
    owner.stdin.flush()
    while True:
        frame=REPLIES.get(timeout=30)
        if frame.get('id')==NEXT:
            assert 'error' not in frame,frame
            return frame['result']
try:
    call('initialize',{'clientInfo':{'name':'overnight_fixture_owner','version':'1'},'capabilities':{'experimentalApi':True}})
    owner.stdin.write('{"method":"initialized","params":{}}\n'); owner.stdin.flush()
    project=call('project/create',{'name':'Overnight fixtures 2026-10-16','idempotencyKey':str(uuid.uuid4()),'roots':[{'path':str(WORK)}]})['project']
    thread=call('thread/start',{'projectId':project['id'],'cwd':str(WORK),'model':'fixture','modelProvider':'fixture',
                              'historyMode':'paginated','approvalPolicy':'never','sandbox':'read-only'})['thread']
    call('turn/start',{'threadId':thread['id'],'clientUserMessageId':'owner-seed','input':[{'type':'text','text':'Offline owner seed.'}]})
    assert COMPLETED.wait(30),'seed never completed'
    (ROOT/'native-thread.json').write_text(json.dumps(thread))
    sys.stdin.read()
finally:
    owner.stdin.close()
    owner.wait(timeout=15)
    http.shutdown()
"###;

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires bundled Codex and python3.14 (compression.zstd); local deterministic provider only"]
    async fn codex_native_queue_real_idle_owner_receives_once() {
        real_native_queue(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires bundled Codex and python3.14 (compression.zstd); kills only its disposable peer"]
    async fn codex_native_queue_real_killed_peer_reconciles_without_resend() {
        real_native_queue(true).await;
    }

    #[cfg(unix)]
    async fn real_native_queue(crash: bool) {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let binary = std::env::var_os("WONDER_NATIVE_QUEUE_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex")
            });
        assert!(
            wonder_app_server::supports_native_thread_queue(&binary).await,
            "pinned queue unavailable"
        );
        let (dir, mut state, message) = dispatch_failure_fixture().await;
        state.projects.shutdown().await;
        let root = dir.path().to_path_buf();
        // Keep optional debug evidence across a failed assertion as well.
        let kept = std::env::var_os("WONDER_NATIVE_QUEUE_KEEP").is_some();
        let _dir = if kept {
            eprintln!("native queue fixture: {}", root.display());
            let _ = dir.keep();
            None
        } else {
            Some(dir)
        };
        let script = root.join("native_fixture.py");
        std::fs::write(&script, NATIVE_QUEUE_FIXTURE).unwrap();
        let stderr = std::fs::File::create(root.join("native-stderr.log")).unwrap();
        let mut owner = tokio::process::Command::new("python3.14")
            .args(["-u"])
            .arg(&script)
            .arg("owner")
            .arg(&root)
            .arg(&binary)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("native queue fixture requires python3.14 on PATH (compression.zstd)");
        tokio::time::timeout(Duration::from_secs(45), async {
            while !root.join("native-thread.json").exists() {
                assert!(
                    owner.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(root.join("native-stderr.log")).unwrap()
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("native fixture owner ready");
        let thread: Value =
            serde_json::from_slice(&std::fs::read(root.join("native-thread.json")).unwrap())
                .unwrap();
        let thread_id = thread["id"].as_str().unwrap();
        let home = root.join("home");
        state.codex_home = home.clone();
        let wrapper = root.join("codex");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec python3.14 -u {} proxy {} {} \"$@\"\n",
                shell_quote(script.to_str().unwrap()),
                shell_quote(root.to_str().unwrap()),
                shell_quote(binary.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let helper = root.join("codex-code-mode-host");
        std::fs::remove_file(&helper).unwrap();
        symlink(binary.with_file_name("codex-code-mode-host"), &helper).unwrap();
        state.projects.config.lock().unwrap().codex_home = Some(home);
        state
            .store
            .set_project_native_session(&message.conversation_id, thread_id, "now")
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                &message.conversation_id,
                AgentFamily::Codex,
                &state.projects.codex_store,
                thread_id,
                None,
                "now",
            )
            .await
            .unwrap();
        crate::desktop_activity::remember_codex_rollout(
            thread_id,
            thread["path"].as_str().unwrap(),
        );
        let client = AppServerClient::spawn_with_notification_sink(
            state.projects.launch_config(),
            crate::ingestion::notification_sink(state.store.clone()),
        )
        .await
        .unwrap();
        let health = client.health();
        *state.projects.native_queue_capability.lock().await = Some((
            health.id().to_owned(),
            wonder_app_server::supports_native_thread_queue(&binary).await,
        ));
        *state.projects.codex.lock().await = client;
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, health);
        let rpc = codex_rpc(&state).await.unwrap();
        // Wonder's selected model deliberately differs from the native owner.
        state.runtime_catalog.write().await.apply_models_page(
            AgentFamily::Codex,
            &json!({"data":[{"id":"fake","model":"fake","displayName":"Fake","isDefault":true}]}),
        );
        assert!(native_queue_supported(&state, &rpc).await);
        if crash {
            std::fs::write(root.join("crash-mode"), "").unwrap();
        }
        assert!(!held_on_mac(&state, &message.conversation_id, Some(&message)).await);
        let before = Instant::now();
        if crash {
            let pending = tokio::spawn(dispatch(state.clone(), message.clone()));
            tokio::time::timeout(Duration::from_secs(10), async {
                while !root.join("peer-kill.json").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("peer killed after enqueue, before receipt");
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
        } else {
            dispatch(state.clone(), message.clone()).await;
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.state,
            if crash {
                "dispatching_to_codex"
            } else {
                "uncertain"
            }
        );
        assert!(saved.codex_turn_id.is_none());
        let context: Value = serde_json::from_str(
            &state
                .store
                .dispatch_context(&message.id)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(context["deliveryMethod"], "thread/queue/add");
        assert_eq!(context["clientUserMessageId"], message.client_message_id);
        assert_eq!(context["nativeOwnerSettings"], true);
        assert!(chrono::DateTime::parse_from_rfc3339(
            context["nativeQueueEnqueuedAt"].as_str().unwrap()
        )
        .is_ok());
        assert!(root.join("journal-before-rpc.jsonl").exists());
        let notice = state
            .projects
            .notices
            .lock()
            .unwrap()
            .get(&message.conversation_id)
            .cloned();
        assert_eq!(
            notice.as_deref(),
            if crash {
                None
            } else {
                Some(CODEX_NATIVE_QUEUED_NOTICE)
            }
        );
        if !crash {
            let (status, detail) = crate::test_support::call(
                &state,
                "GET",
                &format!("/api/v1/project-conversations/{}", message.conversation_id),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{detail}");
            crate::test_support::assert_http_contract("projectConversationDetail", &detail);
            assert_eq!(detail["notice"], CODEX_NATIVE_QUEUED_NOTICE);
        }
        assert!(!state
            .store
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
        if crash {
            assert!(root.join("lost-receipt.json").exists());
            let killed: Value =
                serde_json::from_slice(&std::fs::read(root.join("peer-kill.json")).unwrap())
                    .unwrap();
            assert_eq!(killed["returncode"], -9);
            assert_eq!(killed["before_receipt_forward"], true);
            assert!(
                rpc.health().is_alive(),
                "proxy withheld receipt and EOF until dispatch was cancelled"
            );
        }
        drop(rpc);
        // Lose all volatile send/reconciliation state, retaining only SQLite.
        state.projects.shutdown().await;
        let config = state.projects.launch_config();
        state.projects = ProjectRuntime::configured(
            config.codex_bin.clone(),
            "test".into(),
            &root.join("home"),
            &root.join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        *state.projects.config.lock().unwrap() = config;
        state.store = wonder_store::Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            root.join("state.db").display()
        ))
        .await
        .unwrap();
        state.store.recover_dispatch_claims().await.unwrap();
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "uncertain");
        let client = AppServerClient::spawn_with_notification_sink(
            state.projects.launch_config(),
            crate::ingestion::notification_sink(state.store.clone()),
        )
        .await
        .unwrap();
        let health = client.health();
        *state.projects.native_queue_capability.lock().await = Some((
            health.id().to_owned(),
            wonder_app_server::supports_native_thread_queue(&binary).await,
        ));
        *state.projects.codex.lock().await = client;
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, health);
        let rpc = codex_rpc(&state).await.unwrap();
        assert!(
            native_queue_contains(&rpc, thread_id, &message.client_message_id)
                .await
                .unwrap(),
            "queued entry survives peer restart"
        );
        // A queued match is a queue receipt, never a fabricated native turn.
        assert!(matches!(
            scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await,
            ReceiptScanStep::Notice(CODEX_NATIVE_QUEUED_NOTICE)
        ));
        tokio::time::timeout(Duration::from_secs(35), async {
            loop {
                // Recovery reads history only on a definite event; stand in
                // for one per attempt until the peer drains its queue.
                check_native_queue(&state, &message.conversation_id);
                if let ReceiptScanStep::Accepted(turn) =
                    scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await
                {
                    let turns = result(
                        &rpc,
                        "thread/turns/list",
                        json!({"threadId":thread_id,"itemsView":"full","sortDirection":"asc"}),
                    )
                    .await
                    .unwrap();
                    if turns["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["id"] == turn && t["status"] == "completed")
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("native owner consumed queue");
        let _ingestion = crate::ingestion::spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready(&state).await {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("durable ingestion ready for receipt recovery");
        recover_uncertain_receipts_with_limit(&state, 100).await;
        let accepted = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.state, "accepted_by_codex");
        assert!(accepted.codex_turn_id.is_some());
        assert!(!state
            .projects
            .notices
            .lock()
            .unwrap()
            .contains_key(&message.conversation_id));
        recover(&state).await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        let turns = result(
            &rpc,
            "thread/turns/list",
            json!({"threadId":thread_id,"itemsView":"full","sortDirection":"asc"}),
        )
        .await
        .unwrap();
        let matches = turns["data"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap())
            .filter(|item| item["clientId"] == message.client_message_id)
            .count();
        assert_eq!(matches, 1);
        let settings = result(
            &rpc,
            "thread/read",
            json!({"threadId":thread_id,"includeTurns":false}),
        )
        .await
        .unwrap();
        assert_eq!(settings["thread"]["model"], "fixture");
        let rollout = std::fs::read_to_string(thread["path"].as_str().unwrap()).unwrap();
        let contexts: Vec<Value> = rollout
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["type"] == "turn_context")
            .map(|record| record["payload"].clone())
            .collect();
        assert_eq!(contexts.len(), 2);
        for context in contexts {
            assert_eq!(context["model"], "fixture");
            assert_eq!(context["approval_policy"], "never");
            assert_eq!(context["sandbox_policy"]["type"], "read-only");
        }
        let requests: Vec<Value> = std::fs::read_to_string(root.join("peer-requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "thread/queue/add")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "turn/start")
                .count(),
            0
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "thread/metadata/update")
                .count(),
            0
        );
        let add = requests
            .iter()
            .find(|r| r["method"] == "thread/queue/add")
            .unwrap();
        assert_eq!(add["params"].as_object().unwrap().len(), 3);
        let model: Value =
            serde_json::from_slice(&std::fs::read(root.join("model.json")).unwrap()).unwrap();
        assert_eq!(model["max_active"], 1);
        assert_eq!(model["requests"].as_array().unwrap().len(), 2); // seed + Wonder
        assert!(owner.try_wait().unwrap().is_none());
        eprintln!("native queue crash={crash}: exactly one add, zero peer turns, one history match, model=fixture, max concurrency=1, elapsed={:?}, evidence={}", before.elapsed(), root.display());
        std::fs::write(
            root.join("verified-turns.json"),
            serde_json::to_vec_pretty(&turns).unwrap(),
        )
        .unwrap();
        drop(rpc);
        state.projects.shutdown().await;
        drop(owner.stdin.take());
        tokio::time::timeout(Duration::from_secs(20), owner.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn project_settings_write_failure_is_safe_before_turn_submission() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER reject_update_settings BEFORE INSERT ON update_turn_settings BEGIN SELECT RAISE(FAIL, 'settings unavailable'); END")
            .execute(&pool)
            .await
            .unwrap();

        dispatch(state.clone(), message.clone()).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "safe_to_retry");
        assert!(state
            .projects
            .notices
            .lock()
            .unwrap()
            .get(&message.conversation_id)
            .is_some_and(|notice| notice.contains("settings unavailable")));
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert!(requests.lines().any(|line| {
            serde_json::from_str::<Value>(line).unwrap()["method"] == "thread/start"
        }));
        assert!(requests.lines().all(|line| {
            serde_json::from_str::<Value>(line).unwrap()["method"] != "turn/start"
        }));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_native_rejection_is_safe_but_text_only_errors_are_uncertain() {
        for (code, message_text, data, expected) in [
            (-32602, "Invalid input", Value::Null, "safe_to_retry"),
            (-32001, "Overloaded", Value::Null, "safe_to_retry"),
            (
                -32000,
                "Thread already has an active turn",
                Value::Null,
                "uncertain",
            ),
            (
                -32000,
                "Failure after submission: thread is busy",
                Value::Null,
                "uncertain",
            ),
            (
                -32000,
                "This conversation already has an active turn. Diagnostic text only.",
                Value::Null,
                "uncertain",
            ),
            (
                -32000,
                "Provider failed after starting",
                Value::Null,
                "uncertain",
            ),
            (
                -32000,
                "Claude metadata on a Codex error",
                json!({"turnStart":{"accepted":false,"action":"wait"}}),
                "uncertain",
            ),
        ] {
            let (dir, state, message) = dispatch_failure_fixture().await;
            let script_path = dir.path().join("runtime.py");
            let script = std::fs::read_to_string(&script_path).unwrap();
            let original =
                "elif method == 'turn/start':\n        with open(root + '/accepted-client'";
            assert!(script.contains(original));
            let replacement = format!(
                "elif method == 'turn/start':\n        print(json.dumps({{'id':r['id'],'error':json.loads({})}}),flush=True)\n        continue\n        with open(root + '/accepted-client'",
                serde_json::to_string(&json!({"code":code,"message":message_text,"data":data}).to_string()).unwrap()
            );
            std::fs::write(&script_path, script.replacen(original, &replacement, 1)).unwrap();

            dispatch(state.clone(), message.clone()).await;
            let saved = state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap();
            let notice = state
                .projects
                .notices
                .lock()
                .unwrap()
                .get(&message.conversation_id)
                .cloned();
            assert_eq!(saved.state, expected, "{message_text}: {notice:?}");
            // The message itself keeps the reason, for the phone and for an
            // agent that sent it (owner, Oct 12: failures looked sent).
            assert_eq!(
                state
                    .store
                    .message_delivery_error(&message.id)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(message_text)
            );
            assert!(!dir.path().join("accepted-client").exists());
            let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
            assert_eq!(
                requests
                    .lines()
                    .filter(
                        |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                            == "turn/start"
                    )
                    .count(),
                1,
                "{message_text}: {notice:?}; {requests}"
            );
            state.projects.shutdown().await;
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn project_receipt_recovery_rejects_repeated_history_cursor() {
        let (dir, state, message) = handoff_fixture().await;
        state.projects.shutdown().await;
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/items/list':\n        print(json.dumps({'id':r['id'],'result':{'data':[],'nextCursor':'same'}}),flush=True)\n        continue\n        if os.path.exists(root + '/long-history'):";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();

        let message = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        let error = find_accepted_turn(&state, &message).await.unwrap_err();
        assert!(error.contains("repeated a continuation cursor"), "{error}");
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert_eq!(
            requests
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                        == "thread/items/list"
                )
                .count(),
            2
        );
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lost_steer_receipt_does_not_start_project_runtime_during_update() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        let message = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(
            dir.path().join("accepted-client"),
            &message.client_message_id,
        )
        .unwrap();
        assert_eq!(
            state
                .update_admission
                .prepare_update("lost-steer", &state)
                .await,
            Ok(())
        );
        assert_eq!(find_accepted_turn(&state, &message).await.unwrap(), None);
        assert!(!state.projects.codex.lock().await.health().is_alive());
        assert!(state.update_admission.cancel_lease("lost-steer").await);
        assert_eq!(
            find_accepted_turn(&state, &message).await.unwrap(),
            Some("turn".into())
        );
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_receipt_recovery_scans_newest_first_and_past_twenty_thousand_items() {
        for target_page in [0, 200] {
            let (dir, state, message) = dispatch_failure_fixture().await;
            state
                .store
                .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
                .await
                .unwrap();
            let message = state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap();
            let script_path = dir.path().join("runtime.py");
            let script = std::fs::read_to_string(&script_path).unwrap();
            let original = "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
            assert!(script.contains(original));
            let replacement = format!(
                "elif method == 'thread/items/list':\n        if r['params'].get('sortDirection') != 'desc':\n            print(json.dumps({{'id':r['id'],'error':{{'code':-32602,'message':'Expected newest-first history'}}}}),flush=True)\n            continue\n        page = int(r['params'].get('cursor') or '0')\n        data = [{{'turnId':'other','item':{{'id':str(page)+'-'+str(i),'type':'agentMessage'}}}} for i in range(100)]\n        if page == {}: data[-1] = {{'turnId':'accepted-turn','item':{{'type':'userMessage','clientId':{}}}}}\n        result = {{'data':data,'nextCursor':str(page+1) if page < 200 else None}}\n        print(json.dumps({{'id':r['id'],'result':result}}),flush=True)\n        continue\n        if os.path.exists(root + '/long-history'):",
                target_page,
                serde_json::to_string(&message.client_message_id).unwrap()
            );
            std::fs::write(&script_path, script.replacen(original, &replacement, 1)).unwrap();

            assert_eq!(
                find_accepted_turn(&state, &message).await.unwrap(),
                Some("accepted-turn".into())
            );
            let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
            let pages = requests
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|request| request["method"] == "thread/items/list")
                .collect::<Vec<_>>();
            assert_eq!(pages.len(), target_page + 1);
            assert!(pages
                .iter()
                .all(|request| request["params"]["sortDirection"] == "desc"));
            assert_eq!(
                pages.last().unwrap()["params"]["cursor"],
                if target_page == 0 {
                    Value::Null
                } else {
                    json!("200")
                }
            );
            state.projects.shutdown().await;
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn project_receipt_recovery_advances_in_slices_and_backs_off_after_absent_proof() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
        assert!(script.contains(original));
        let replacement = format!(
            "elif method == 'thread/items/list':\n        if r['params'].get('sortDirection') != 'desc':\n            print(json.dumps({{'id':r['id'],'error':{{'code':-32602,'message':'Expected newest-first history'}}}}),flush=True)\n            continue\n        page = int(r['params'].get('cursor') or '0')\n        data = [{{'turnId':'other','item':{{'id':str(page)+'-'+str(i),'type':'agentMessage'}}}} for i in range(100)]\n        if page == 200 and os.path.exists(root + '/accepted-client'): data[-1] = {{'turnId':'accepted-turn','item':{{'type':'userMessage','clientId':{}}}}}\n        result = {{'data':data,'nextCursor':str(page+1) if page < 200 else None}}\n        print(json.dumps({{'id':r['id'],'result':result}}),flush=True)\n        continue\n        if os.path.exists(root + '/long-history'):",
            serde_json::to_string(&message.client_message_id).unwrap()
        );
        std::fs::write(&script_path, script.replacen(original, &replacement, 1)).unwrap();
        let _service = crate::ingestion::spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while !ready(&state).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let request_count = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["method"] == "thread/items/list"
                })
                .count()
        };

        // 201 pages represent 20,100 newer items. Each recovery pass must
        // release the provider after at most 20 pages, retaining its cursor.
        for cycle in 0..11 {
            recover_uncertain_receipts_with_budget(
                &state,
                RECEIPT_HISTORY_PAGE_LIMIT,
                RECEIPT_PAGES_PER_CYCLE,
                RECEIPT_RESCAN_DELAY,
            )
            .await;
            assert_eq!(request_count(), ((cycle + 1) * 20).min(201));
        }
        for _ in 0..3 {
            recover_uncertain_receipts(&state).await;
        }
        assert_eq!(
            request_count(),
            201,
            "absent proof must not rescan every tick"
        );
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );

        // Once the quiet period expires, an old accepted item remains
        // reachable without starting a second provider turn.
        std::fs::write(
            dir.path().join("accepted-client"),
            &message.client_message_id,
        )
        .unwrap();
        state
            .projects
            .receipt_scans
            .lock()
            .unwrap()
            .get_mut(&message.id)
            .unwrap()
            .resume_after = Instant::now();
        for cycle in 0..11 {
            recover_uncertain_receipts(&state).await;
            assert_eq!(request_count(), 201 + ((cycle + 1) * 20).min(201));
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "accepted_by_codex");
        assert_eq!(saved.codex_turn_id.as_deref(), Some("accepted-turn"));
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert!(requests.lines().all(|line| {
            serde_json::from_str::<Value>(line).unwrap()["method"] != "turn/start"
        }));
        assert!(state
            .projects
            .receipt_scans
            .lock()
            .unwrap()
            .get(&message.id)
            .is_none());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_receipt_cursor_cycle_across_passes_resets_on_restart() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/items/list':\n        print(json.dumps({'id':r['id'],'result':{'data':[],'nextCursor':'same'}}),flush=True)\n        continue\n        if os.path.exists(root + '/long-history'):";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();
        let _service = crate::ingestion::spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while !ready(&state).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let count = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["method"] == "thread/items/list"
                })
                .count()
        };
        for expected in [1, 2] {
            recover_uncertain_receipts_with_budget(&state, 10, 1, RECEIPT_RESCAN_DELAY).await;
            assert_eq!(count(), expected);
        }
        assert_eq!(
            state
                .projects
                .notices
                .lock()
                .unwrap()
                .get(&message.conversation_id)
                .map(String::as_str),
            Some(RECEIPT_HISTORY_INVALID_NOTICE)
        );
        recover_uncertain_receipts_with_budget(&state, 10, 1, RECEIPT_RESCAN_DELAY).await;
        assert_eq!(count(), 2, "cycle must back off");
        // Process restart discards cursor progress; a new process rechecks the
        // newest page without resending the message.
        state.projects.receipt_scans.lock().unwrap().clear();
        recover_uncertain_receipts_with_budget(&state, 10, 1, RECEIPT_RESCAN_DELAY).await;
        assert_eq!(count(), 3);
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        let cursors = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| request["method"] == "thread/items/list")
            .map(|request| request["params"]["cursor"].clone())
            .collect::<Vec<_>>();
        assert_eq!(cursors, vec![Value::Null, json!("same"), Value::Null]);
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_uncertain_message_absent_from_complete_history_settles_failed() {
        let (_dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        let conversation = state
            .store
            .project_conversation(&message.conversation_id)
            .await;
        // Codex can still deliver late; the other providers cannot.
        assert!(!absent_history_settles(&conversation));
        let mut other = conversation.unwrap().unwrap();
        other.family = AgentFamily::OpenCode;
        assert!(absent_history_settles(&Ok(Some(other))));

        settle_absent_receipt(&state, &message).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "failed");
        // The retry API accepts what recovery settled.
        assert!(state
            .store
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn project_receipt_history_limit_keeps_uncertain_and_shows_recovery_notice() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/items/list':\n        page = int(r['params'].get('cursor') or '0')\n        print(json.dumps({'id':r['id'],'result':{'data':[],'nextCursor':str(page+1)}}),flush=True)\n        continue\n        if os.path.exists(root + '/long-history'):";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();
        let _service = crate::ingestion::spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while !ready(&state).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        recover_uncertain_receipts_with_limit(&state, 2).await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        assert_eq!(
            state
                .projects
                .notices
                .lock()
                .unwrap()
                .get(&message.conversation_id)
                .map(String::as_str),
            Some(RECEIPT_HISTORY_LIMIT_NOTICE)
        );
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert_eq!(
            requests
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["method"]
                        == "thread/items/list"
                )
                .count(),
            2
        );
        state.projects.shutdown().await;
        std::fs::write(&script_path, script).unwrap();
        std::fs::write(
            dir.path().join("accepted-client"),
            &message.client_message_id,
        )
        .unwrap();
        recover_uncertain_receipts_with_limit(&state, 2).await;
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "accepted_by_codex");
        assert_eq!(saved.codex_turn_id.as_deref(), Some("turn"));
        assert!(!state
            .projects
            .notices
            .lock()
            .unwrap()
            .contains_key(&message.conversation_id));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    async fn history_scan_allows_update(with_turn: bool) {
        let (dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(
                &message.id,
                "uncertain",
                Some("thread"),
                with_turn.then_some("turn"),
            )
            .await
            .unwrap();
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = if with_turn {
            "elif method == 'thread/turns/list':\n        if os.path.exists(root + '/turn-pages.json'):"
        } else {
            "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):"
        };
        assert!(script.contains(original));
        let entry = if with_turn {
            "{'id':'turn','status':'completed'}".to_owned()
        } else {
            format!(
                "{{'turnId':'turn','item':{{'type':'userMessage','clientId':{}}}}}",
                serde_json::to_string(&message.client_message_id).unwrap()
            )
        };
        let replacement = format!(
            "elif method == '{}':\n        page = int(r.get('params',{{}}).get('cursor') or '0')\n        open(root + '/history-waiting','w').close()\n        while os.path.exists(root + '/delay-history'): time.sleep(0.01)\n        result = {{'data':[{}] if page == 20 else [], 'nextCursor':str(page+1) if page < 20 else None}}\n        if page == 20: open(root + '/history-finished','w').close()\n        print(json.dumps({{'id':r['id'],'result':result}}),flush=True)\n        continue\n        if os.path.exists(root + '/{}'):",
            if with_turn { "thread/turns/list" } else { "thread/items/list" },
            entry,
            if with_turn { "turn-pages.json" } else { "long-history" }
        );
        std::fs::write(&script_path, script.replacen(original, &replacement, 1)).unwrap();
        std::fs::write(dir.path().join("delay-history"), "").unwrap();

        let _service = crate::ingestion::spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while !ready(&state).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let scheduler = crate::dispatch::spawn(state.clone()).await.unwrap();
        let waiting = dir.path().join("history-waiting");
        tokio::time::timeout(Duration::from_secs(60), async {
            while !waiting.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let prepare_state = state.clone();
        let preparing = tokio::spawn(async move {
            prepare_state
                .update_admission
                .prepare_update("history-update", &prepare_state)
                .await
        });
        let admitted = tokio::time::timeout(Duration::from_secs(60), async {
            while !state.update_admission.work_paused() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        std::fs::remove_file(dir.path().join("delay-history")).unwrap();
        if admitted.is_err() {
            preparing.abort();
            scheduler.abort();
            panic!("Project history blocked update admission (turn={with_turn})");
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), preparing)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            while if with_turn {
                !dir.path().join("history-finished").exists()
            } else {
                !state
                    .projects
                    .receipt_scans
                    .lock()
                    .unwrap()
                    .contains_key(&message.id)
            } {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        let history_method = if with_turn {
            "thread/turns/list"
        } else {
            "thread/items/list"
        };
        let history_requests = requests
            .lines()
            .filter(|line| serde_json::from_str::<Value>(line).unwrap()["method"] == history_method)
            .count();
        if with_turn {
            assert!(history_requests >= 21);
        } else {
            assert_eq!(
                history_requests, 1,
                "Project scan should pause after update admission"
            );
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "uncertain");
        assert_eq!(saved.codex_turn_id.as_deref(), with_turn.then_some("turn"));
        assert!(state.update_admission.cancel_lease("history-update").await);
        scheduler.abort();
        let _ = scheduler.await;
        if !with_turn {
            recover_uncertain_receipts(&state).await;
            let saved = state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.state, "accepted_by_codex");
            assert_eq!(saved.codex_turn_id.as_deref(), Some("turn"));
        }
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert!(requests.lines().all(|line| {
            serde_json::from_str::<Value>(line).unwrap()["method"] != "turn/start"
        }));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_history_scans_do_not_hold_update_admission_or_publish_during_update() {
        history_scan_allows_update(false).await;
        history_scan_allows_update(true).await;
    }

    #[tokio::test]
    async fn receipt_recovery_waits_for_runtime_setup_before_admitting_update() {
        let (_dir, state, message) = dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        let message = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        let start = state.projects.start.lock().await;
        let scan_state = state.clone();
        let scanning = tokio::spawn(async move {
            scan_ambiguous_receipt(
                &scan_state,
                &message,
                RECEIPT_HISTORY_PAGE_LIMIT,
                RECEIPT_PAGES_PER_CYCLE,
                RECEIPT_RESCAN_DELAY,
            )
            .await
        });
        // The scan is waiting to start the dead normal-home runtime. An update
        // must queue behind that admitted setup, even though history reads are
        // free to overlap the update once setup has finished.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let update_state = state.clone();
        let preparing = tokio::spawn(async move {
            update_state
                .update_admission
                .prepare_update("recovery-setup", &update_state)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), async {
                while !state.update_admission.work_paused() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_err(),
            "Update admission overtook recovery runtime setup"
        );
        drop(start);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), preparing)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        tokio::time::timeout(Duration::from_secs(60), scanning)
            .await
            .unwrap()
            .unwrap();
        assert!(state.projects.codex.lock().await.health().is_alive());
        assert!(state.update_admission.cancel_lease("recovery-setup").await);
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_turn_recovery_waits_for_runtime_setup_before_admitting_update() {
        let (_dir, state, _) = handoff_fixture().await;
        state.projects.shutdown().await;
        let start = state.projects.start.lock().await;
        let recovery_state = state.clone();
        let recovering = tokio::spawn(async move { recover(&recovery_state).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let update_state = state.clone();
        let preparing = tokio::spawn(async move {
            update_state
                .update_admission
                .prepare_update("active-recovery-setup", &update_state)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), async {
                while !state.update_admission.work_paused() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_err(),
            "Update admission overtook active-turn runtime setup"
        );
        drop(start);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), preparing)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        tokio::time::timeout(Duration::from_secs(60), recovering)
            .await
            .unwrap()
            .unwrap();
        assert!(
            state
                .update_admission
                .cancel_lease("active-recovery-setup")
                .await
        );
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    fn writer_available(dir: &FsPath) -> bool {
        std::fs::File::open(dir.join("writer.lock"))
            .unwrap()
            .try_lock()
            .is_ok()
    }

    // One active row with a long saved turn: list decoration reads only the
    // latest 16 history entries and never asks a runtime to hydrate history.
    #[tokio::test]
    async fn current_activity_is_bounded_and_newer_terminal_turn_clears_it() {
        use wonder_api::{HostEventEnvelope, WonderEvent};
        let (_dir, state, _message) = handoff_fixture().await;
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        let mut event = HostEventEnvelope {
            event_id: "start".into(),
            host_epoch: state.host_epoch.clone(),
            sequence: 0,
            occurred_at: "1700000000000".into(),
            conversation_id: Some("project-chat".into()),
            thread_id: Some("thread".into()),
            turn_id: Some("turn".into()),
            request_id: None,
            device_id: None,
            message_id: None,
            item_id: None,
            approval_id: None,
            event: WonderEvent::MessageState {
                state: DeliveryState::AcceptedByCodex,
            },
        };
        state.store.commit_event(&mut event).await.unwrap();
        for index in 0..200 {
            event.event_id = format!("item-{index}");
            event.occurred_at = (1700000000001_i64 + index).to_string();
            let status = if index == 199 { "started" } else { "completed" };
            event.event = WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: status.into(),
                detail: crate::thread_item_upsert_detail_with_authority(
                    "turn",
                    &json!({
                        "id":format!("item-{index}"), "type":"commandExecution", "status":status,
                        "command":"cat Sources/AppShell.swift", "aggregatedOutput":"private output",
                        "commandActions":[{"type":"read","path":"Sources/AppShell.swift"}]
                    }),
                    status,
                    2,
                    Some(&conversation.cwd),
                ),
            };
            state.store.commit_event(&mut event).await.unwrap();
        }
        let summary = attached_summary(&state, &conversation, 1, None).await;
        let activity = summary.current_activity.as_ref().unwrap();
        assert_eq!(activity["payload"]["commandActions"][0]["type"], "read");
        assert!(activity["payload"].get("command").is_none());
        assert!(activity["payload"].get("aggregatedOutput").is_none());
        assert!(activity["text"].is_null());
        crate::tests::validate_http_contract(
            "projectThreadSummary",
            &serde_json::to_value(summary).unwrap(),
        );
        let page = state
            .store
            .conversation_history_page(&state.host_epoch, "project-chat", None, 16)
            .await
            .unwrap()
            .unwrap();
        let read_items = page
            .events
            .iter()
            .filter(|event| matches!(event.event, WonderEvent::Activity { .. }))
            .count();
        assert!((1..=16).contains(&read_items));
        assert!(page.next_cursor.is_some());
        let mut before = Vec::new();
        let mut after = Vec::new();
        for _ in 0..100 {
            let start = std::time::Instant::now();
            assert!(state
                .store
                .conversation_has_active_turn("project-chat")
                .await
                .unwrap());
            before.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            assert!(attached_summary(&state, &conversation, 1, None)
                .await
                .current_activity
                .is_some());
            after.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        before.sort_by(f64::total_cmp);
        after.sort_by(f64::total_cmp);
        println!("current-activity: samples=100 working_rows=1 retained_items=200 page_limit=16 activity_items_read={read_items} baseline_p50_ms={:.3} baseline_p95_ms={:.3} current_p50_ms={:.3} current_p95_ms={:.3}", before[49], before[94], after[49], after[94]);
        // An old active message receipt is not authority over a newer terminal
        // provider turn, even if that turn left an unfinished tool item behind.
        event.event_id = "newer-terminal".into();
        event.turn_id = Some("newer".into());
        event.occurred_at = "1700000001000".into();
        event.event = WonderEvent::MessageState {
            state: DeliveryState::Completed,
        };
        state.store.commit_event(&mut event).await.unwrap();
        assert!(attached_summary(&state, &conversation, 2, None)
            .await
            .current_activity
            .is_none());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // The computer's continuation choices are saved and read back through
    // the phone's routes; explicit Project thread defaults are gone (a new
    // chat follows the Project's last-used model instead).
    #[tokio::test]
    async fn continuation_settings_round_trip_and_thread_defaults_are_refused() {
        use crate::test_support::{assert_http_contract, call, fixture};
        let (dir, state) = fixture().await;
        let roots = validate_folders(&[dir.path().to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("app", "app", "hash", "app", &roots, 0, "now")
            .await
            .unwrap();
        let (status, refused) = call(
            &state,
            "PATCH",
            "/api/v1/projects/app",
            json!({"threadDefaults": {"family": "claude", "model": "claude-opus"}}),
        )
        .await;
        assert!(status.is_client_error(), "{status} {refused}");
        let (status, project) = call(
            &state,
            "PATCH",
            "/api/v1/projects/app",
            json!({"name": "App"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{project}");
        assert_eq!(project["lastModels"], json!([]));

        let (status, settings) =
            call(&state, "GET", "/api/v1/settings/projects", json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_http_contract("projectDiscoverySettings", &settings);
        assert_eq!(
            (
                settings["resumeAfterLimit"].clone(),
                settings["continueAfterRestart"].clone()
            ),
            (json!(true), json!(true))
        );
        let (status, settings) = call(
            &state,
            "PUT",
            "/api/v1/settings/projects",
            json!({"continueAfterRestart": false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{settings}");
        assert_eq!(
            (
                settings["resumeAfterLimit"].clone(),
                settings["continueAfterRestart"].clone()
            ),
            (json!(true), json!(false))
        );
        state.projects.shutdown().await;
    }

    // A store from before the rename lists "No project"; a name the owner chose stays.
    #[tokio::test]
    async fn legacy_scratch_project_is_renamed_once_and_owner_names_stay() {
        let (dir, state, _) = handoff_fixture().await;
        let folder = dir.path().join("scratch-folder");
        std::fs::create_dir(&folder).unwrap();
        let roots = validate_folders(&[folder.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                SCRATCH_PROJECT_ID,
                "scratch-request",
                "hash",
                LEGACY_SCRATCH_NAME,
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        let mut projects = state.store.list_projects().await.unwrap();
        rename_legacy_scratch(&state, &mut projects).await;
        let named = |projects: &[wonder_store::StoredProject]| {
            projects
                .iter()
                .find(|p| p.id == SCRATCH_PROJECT_ID)
                .unwrap()
                .name
                .clone()
        };
        assert_eq!(named(&projects), NO_PROJECT_NAME);
        assert_eq!(
            named(&state.store.list_projects().await.unwrap()),
            NO_PROJECT_NAME
        );
        state
            .store
            .update_project_metadata(SCRATCH_PROJECT_ID, Some("Errands"), None, None, "later")
            .await
            .unwrap();
        let mut projects = state.store.list_projects().await.unwrap();
        rename_legacy_scratch(&state, &mut projects).await;
        assert_eq!(
            named(&state.store.list_projects().await.unwrap()),
            "Errands"
        );
        state.projects.shutdown().await;
    }

    // The owner renames between the list read and the legacy rename: their
    // name survives in the store and in the response.
    #[tokio::test]
    async fn legacy_scratch_rename_does_not_overwrite_a_concurrent_owner_rename() {
        let (dir, state, _) = handoff_fixture().await;
        let folder = dir.path().join("scratch-folder");
        std::fs::create_dir(&folder).unwrap();
        let roots = validate_folders(&[folder.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                SCRATCH_PROJECT_ID,
                "scratch-request",
                "hash",
                LEGACY_SCRATCH_NAME,
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        let mut projects = state.store.list_projects().await.unwrap();
        state
            .store
            .update_project_metadata(SCRATCH_PROJECT_ID, Some("Errands"), None, None, "later")
            .await
            .unwrap();
        rename_legacy_scratch(&state, &mut projects).await;
        let listed = projects
            .iter()
            .find(|p| p.id == SCRATCH_PROJECT_ID)
            .unwrap();
        assert_eq!(listed.name, "Errands");
        let stored = state
            .store
            .project(SCRATCH_PROJECT_ID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.name, "Errands");
        state.projects.shutdown().await;
    }

    // Removing a configured folder preserves its saved chats and their pins,
    // archive access and activity; only execution membership becomes false.
    #[tokio::test]
    async fn project_chats_in_removed_folders_remain_listed() {
        use crate::test_support::call;
        let (dir, state, _) = handoff_fixture().await;
        let original = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        for (id, family, native, archived) in [
            ("removed-draft", AgentFamily::Claude, None, false),
            (
                "removed-opencode",
                AgentFamily::OpenCode,
                Some("removed-native"),
                false,
            ),
            (
                "removed-archive",
                AgentFamily::OpenCode,
                Some("archived-native"),
                true,
            ),
        ] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: "project",
                    family,
                    provider_store: provider_store(&state, family),
                    native_session_id: native,
                    cwd: &original.cwd,
                    roots_revision: 1,
                    title: id,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: family.provider().default_approval,
                    plan_mode: false,
                    creation_request_id: None,
                    now: "2000-01-01T00:00:00Z",
                })
                .await
                .unwrap();
            if archived {
                state
                    .store
                    .set_project_conversation_wonder_archived(id, true, "now")
                    .await
                    .unwrap();
            }
        }
        // Checked before, as by its last send; lists never wait for a check.
        assert_eq!(
            archive_state::archive_state(&state, &original).await,
            ArchiveState::Active
        );
        let expected: HashSet<_> = ["project-chat", "removed-draft", "removed-opencode"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for removed in [false, true] {
            if removed {
                let replacement = dir.path().join("replacement");
                std::fs::create_dir(&replacement).unwrap();
                let roots = validate_folders(&[replacement.to_str().unwrap().into()], &[]).unwrap();
                state
                    .store
                    .update_project_roots("project", 1, &roots, 0, "now")
                    .await
                    .unwrap();
            }
            for include in [true, false] {
                state.store.set_include_worktrees(include).await.unwrap();
                let (status, page) =
                    call(&state, "GET", "/api/v1/projects/project/threads", json!({})).await;
                assert_eq!(status, StatusCode::OK, "{page}");
                let ids: HashSet<_> = page["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["conversationId"].as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(
                    ids, expected,
                    "removed={removed}, include={include}: {page}"
                );
                let (_, projects) = call(&state, "GET", "/api/v1/projects", json!({})).await;
                assert!(
                    projects["pinned"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|p| p["thread"]["conversationId"] == "project-chat"),
                    "{projects}"
                );
                let (_, archive) = call(
                    &state,
                    "GET",
                    "/api/v1/projects/project/threads/archived",
                    json!({}),
                )
                .await;
                assert!(
                    archive["threads"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["conversationId"] == "removed-archive"),
                    "{archive}"
                );
                let visible = visible_conversations(&state).await.unwrap();
                assert!(expected.is_subset(&visible));
                assert!(!visible.contains("removed-archive"));
                assert!(crate::live_activity::read_inputs(&state)
                    .await
                    .unwrap()
                    .owned
                    .contains("project-chat"));
                for id in expected
                    .iter()
                    .chain(std::iter::once(&"removed-archive".to_owned()))
                {
                    let (status, detail) = call(
                        &state,
                        "GET",
                        &format!("/api/v1/project-conversations/{id}"),
                        json!({}),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK, "{detail}");
                    assert_eq!(detail["folderInProject"], !removed, "{detail}");
                }
            }
        }
        state.projects.shutdown().await;
    }

    fn fixture_repository(repo: &FsPath, linked: &FsPath) {
        std::fs::create_dir(repo).unwrap();
        let git = |args: &[&str]| {
            let output = crate::test_git::git()
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "trunk"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ]);
        git(&[
            "worktree",
            "add",
            "-qb",
            "agent/fixture",
            linked.to_str().unwrap(),
        ]);
    }

    // Contract: list rows carry the branch of their working folder and the
    // latest automation run (with its failure), and a Project with 240 threads
    // pages through them with one HEAD read per folder, not per row.
    #[tokio::test]
    async fn thread_rows_carry_branch_and_automation_across_240_threads() {
        use crate::test_support::{assert_http_contract, call, fixture};
        let (dir, state) = fixture().await;
        fixture_repository(&dir.path().join("repo"), &dir.path().join("worker"));
        let repo = crate::canonical_path::canonicalize(dir.path().join("repo")).unwrap();
        let linked = crate::canonical_path::canonicalize(dir.path().join("worker")).unwrap();
        let roots = validate_folders(&[repo.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("many", "many", "hash", "Many", &roots, 0, "now")
            .await
            .unwrap();
        const THREADS: usize = 240;
        for index in 0..THREADS {
            let id = format!("many-{index}");
            let native = format!("native-{index}");
            // Wonder's own record lists these, so no provider catalog is involved.
            let at = chrono::DateTime::from_timestamp(1_790_000_000 + index as i64, 0)
                .unwrap()
                .to_rfc3339_opts(SecondsFormat::Secs, true);
            let cwd = if index.is_multiple_of(3) {
                &linked
            } else {
                &repo
            };
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: &id,
                    project_id: "many",
                    family: AgentFamily::OpenCode,
                    provider_store: provider_store(&state, AgentFamily::OpenCode),
                    native_session_id: Some(&native),
                    cwd: cwd.to_str().unwrap(),
                    roots_revision: 1,
                    title: &id,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: AgentFamily::OpenCode.provider().default_approval,
                    plan_mode: false,
                    creation_request_id: None,
                    now: &at,
                })
                .await
                .unwrap();
        }
        // An automation that posted into many-7 twice; the newer run failed.
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("INSERT INTO automations(id,name,kind,conversation_id,scope_type,scope_id,prompt,rrule,timezone,status,notification_policy,created_at,updated_at) VALUES ('nightly','Nightly check','continuation','many-7','project_thread','many-7','Check','FREQ=DAILY','UTC','active','all_runs','now','now')")
            .execute(&pool).await.unwrap();
        for (id, status, started, error) in [
            ("run-1", "completed", "2026-10-19T08:00:00.000Z", None),
            (
                "run-2",
                "failed",
                "2026-10-20T08:00:00.000Z",
                Some("Cupola needs an active paired owner device to run this automation."),
            ),
        ] {
            sqlx::query("INSERT INTO automation_runs(id,automation_id,scheduled_for,status,started_at,finished_at,error,conversation_id) VALUES (?,'nightly',?,?,?,?,?,'many-7')")
                .bind(id).bind(started).bind(status).bind(started).bind(started).bind(error)
                .execute(&pool).await.unwrap();
        }
        pool.close().await;

        let branch_checks = |state: &AppState| {
            state
                .projects
                .branches
                .lock()
                .unwrap()
                .iter()
                .map(|(cwd, entry)| (cwd.clone(), entry.checked))
                .collect::<HashMap<_, _>>()
        };
        let mut rows = HashMap::new();
        let mut cursor: Option<String> = None;
        let mut first_checks = None;
        let mut slowest = Duration::ZERO;
        let started = Instant::now();
        for _ in 0..THREADS {
            let path = match &cursor {
                Some(cursor) => format!("/api/v1/projects/many/threads?limit=30&cursor={cursor}"),
                None => "/api/v1/projects/many/threads?limit=30".to_owned(),
            };
            let page_started = Instant::now();
            let (status, page) = call(&state, "GET", &path, json!({})).await;
            slowest = slowest.max(page_started.elapsed());
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_http_contract("projectThreadsPage", &page);
            for row in page["threads"].as_array().unwrap() {
                let id = row["conversationId"].as_str().unwrap().to_owned();
                assert!(rows.insert(id, row.clone()).is_none(), "{row}");
            }
            first_checks.get_or_insert_with(|| branch_checks(&state));
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        eprintln!(
            "{THREADS} threads listed in {:?}, slowest page {slowest:?}",
            started.elapsed()
        );
        assert_eq!(rows.len(), THREADS);
        for (id, row) in &rows {
            let index: usize = id.trim_start_matches("many-").parse().unwrap();
            let expected = if index.is_multiple_of(3) {
                "agent/fixture"
            } else {
                "trunk"
            };
            assert_eq!(row["branch"], expected, "{row}");
            assert!(row.get("backgroundWork").is_none(), "{row}");
            if index != 7 {
                assert!(row.get("lastAutomationAt").is_none(), "{row}");
                assert!(row.get("automationRun").is_none(), "{row}");
            }
        }
        let automated = &rows["many-7"];
        assert_eq!(automated["lastAutomationAt"], "2026-10-20T08:00:00.000Z");
        assert_eq!(
            automated["automationRun"],
            json!({"automationId":"nightly","name":"Nightly check","status":"failed",
                "startedAt":"2026-10-20T08:00:00.000Z","finishedAt":"2026-10-20T08:00:00.000Z",
                "error":"Cupola needs an active paired owner device to run this automation."})
        );
        // Listing cost: two folders, two cached HEAD reads, none repeated while
        // paging; each page stays well inside the phone's request budget.
        let checks = branch_checks(&state);
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(Some(checks), first_checks);
        assert!(slowest < Duration::from_secs(3), "slowest page {slowest:?}");

        // Switching the worktree's branch shows on the next read.
        let output = crate::test_git::git()
            .arg("-C")
            .arg(&linked)
            .args(["checkout", "-q", "-b", "renamed"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let (_, page) = call(
            &state,
            "GET",
            "/api/v1/projects/many/threads?limit=30",
            json!({}),
        )
        .await;
        for row in page["threads"].as_array().unwrap() {
            let index: usize = row["conversationId"].as_str().unwrap()["many-".len()..]
                .parse()
                .unwrap();
            let expected = if index.is_multiple_of(3) {
                "renamed"
            } else {
                "trunk"
            };
            assert_eq!(row["branch"], expected, "{row}");
        }

        // The pinned list in Chats carries the same row fields.
        state
            .store
            .update_project_conversation(
                "many-7",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let (status, library) = call(&state, "GET", "/api/v1/projects", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{library}");
        let pinned = &library["pinned"][0]["thread"];
        assert_eq!(pinned["conversationId"], "many-7", "{library}");
        assert_eq!(pinned["branch"], "trunk");
        assert_eq!(pinned["automationRun"]["status"], "failed");
        assert_http_contract("projectThreadSummary", pinned);
        state.projects.shutdown().await;
    }

    // Contract: a Codex thread whose reply ended but whose runtime still keeps
    // a background terminal is marked, in the Project list and in Chats' pins.
    #[tokio::test]
    async fn codex_rows_mark_background_terminals_that_outlive_the_reply() {
        use crate::test_support::{assert_http_contract, call, fixture};
        let (dir, state) = fixture().await;
        let folder = dir.path().join("plain");
        std::fs::create_dir(&folder).unwrap();
        let roots = validate_folders(&[folder.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project(
                "background",
                "background",
                "hash",
                "Background",
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        for native in ["bg-thread", "quiet-thread"] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: native,
                    project_id: "background",
                    family: AgentFamily::Codex,
                    provider_store: provider_store(&state, AgentFamily::Codex),
                    native_session_id: Some(native),
                    cwd: roots[0].canonical_path.as_str(),
                    roots_revision: 1,
                    title: native,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: AgentFamily::Codex.provider().default_approval,
                    plan_mode: false,
                    creation_request_id: None,
                    now: "2000-01-01T00:00:00Z",
                })
                .await
                .unwrap();
            state
                .store
                .update_project_conversation(
                    native,
                    ProjectConversationPatch {
                        pinned: Some(true),
                        ..Default::default()
                    },
                    "now",
                )
                .await
                .unwrap();
            // Checked before, as by its last send; lists never wait for one.
            let stored = state
                .store
                .project_conversation(native)
                .await
                .unwrap()
                .unwrap();
            archive_state::archive_state(&state, &stored).await;
        }
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({
                "bg-thread": {"id":"bg-thread","status":{"type":"idle"},"background":[{"id":"terminal"}]},
                "quiet-thread": {"id":"quiet-thread","status":{"type":"idle"},"background":[]},
            })
            .to_string(),
        )
        .unwrap();
        let (status, page) = call(
            &state,
            "GET",
            "/api/v1/projects/background/threads",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let (_, library) = call(&state, "GET", "/api/v1/projects", json!({})).await;
        let pins = library["pinned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pin| pin["thread"].clone());
        for rows in [page["threads"].as_array().unwrap().clone(), pins.collect()] {
            let marked = |id: &str| {
                let row = rows
                    .iter()
                    .find(|row| row["conversationId"] == id)
                    .unwrap_or_else(|| panic!("{id} in {rows:?}"));
                assert_http_contract("projectThreadSummary", row);
                assert_eq!(row["isWorking"], false, "{row}");
                row.get("backgroundWork").cloned()
            };
            assert_eq!(marked("bg-thread"), Some(json!(true)));
            assert_eq!(marked("quiet-thread"), None);
        }
        assert!(
            page["threads"][0].get("branch").is_none(),
            "not a repository"
        );
        state.projects.shutdown().await;
    }

    // Contract: Claude rows are marked when work outlives the reply: a session
    // Cupola's runtime keeps for background tasks, tasks Claude on the Mac
    // still runs in an attached chat, and a session only discovered on the
    // Mac whose app still runs background work. Idle sessions stay unmarked.
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_rows_mark_background_work_for_attached_and_discovered_sessions() {
        use crate::provider_switch_tests::{CLAUDE_SESSION, CLAUDE_THREAD};
        use crate::test_support::{assert_http_contract, call};
        let fx = crate::provider_switch_tests::fx().await;
        let first = fx
            .queue(
                "start in Claude",
                Some((AgentFamily::Claude, "claude:sonnet")),
            )
            .await;
        fx.release(&first).await.unwrap();
        fx.finish(&first).await;
        let cwd = fx
            .state
            .store
            .project_conversation(&fx.chat)
            .await
            .unwrap()
            .unwrap()
            .cwd;
        let discovered = uuid::Uuid::new_v4().to_string();
        let idle = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let reply = |method: &str, value: Value| {
            let name = format!("claude-reply-{}.json", method.replace('/', "-"));
            std::fs::write(fx.dir.path().join(name), value.to_string()).unwrap();
        };
        let unreply = |method: &str| {
            let name = format!("claude-reply-{}.json", method.replace('/', "-"));
            std::fs::remove_file(fx.dir.path().join(name)).unwrap();
        };
        let sessions = [CLAUDE_SESSION, &discovered, &idle]
            .iter()
            .enumerate()
            .map(|(age, id)| json!({"sessionId": id, "cwd": cwd, "title": id, "updatedAt": now - age as i64}))
            .collect::<Vec<_>>();
        reply(
            "project/sessions/list",
            json!({"data": sessions, "nextCursor": null, "partial": false}),
        );
        reply(
            "thread/list",
            json!({"data": [{"id": CLAUDE_THREAD, "sessionId": CLAUDE_SESSION,
                "status": {"type": "idle", "activeFlags": []}}], "nextCursor": null}),
        );
        reply(
            "thread/loaded/list",
            json!({"data": [CLAUDE_THREAD], "nextCursor": null}),
        );
        reply(
            "thread/backgroundTasks/list",
            json!({"data": [{"id": "task", "kind": "agent", "status": "running", "runningElsewhere": false}]}),
        );
        // The discovered session's app on the Mac runs a background command.
        let records = tempfile::tempdir().unwrap();
        let transcripts = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let started = crate::desktop_activity::record_start_label(pid as i32);
        std::fs::write(
            records.path().join(format!("{pid}.json")),
            json!({"pid": pid, "procStart": started, "sessionId": discovered, "status": "shell", "statusUpdatedAt": 1,
                "cwd": cwd, "entrypoint": "claude-desktop"})
            .to_string(),
        )
        .unwrap();
        let dirs = (records.path().to_owned(), transcripts.path().to_owned());
        crate::desktop_activity::tests::BUSY_DIRS
            .lock()
            .unwrap()
            .push(dirs.clone());
        let marks = || async {
            let (status, page) = call(
                &fx.state,
                "GET",
                "/api/v1/projects/project/threads?limit=10",
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let rows = page["threads"].as_array().unwrap().clone();
            [CLAUDE_SESSION, &discovered, &idle].map(|native| {
                let row = rows
                    .iter()
                    .find(|row| row["reference"] == format!("claude:{native}"))
                    .unwrap_or_else(|| panic!("{native} in {page}"));
                assert_http_contract("projectThreadSummary", row);
                assert_eq!(row["isWorking"], false, "{row}");
                row.get("backgroundWork") == Some(&json!(true))
            })
        };
        // Cupola's runtime keeps the attached session for its background task.
        assert_eq!(marks().await, [true, true, false]);
        // This is work in Cupola's own retained process. A roster blocked
        // by another selected chat must preserve this verified badge too.
        std::fs::write(fx.dir.path().join("claude-hold-roster"), "").unwrap();
        assert_eq!(
            marks().await,
            [true, true, false],
            "a roster timeout keeps owned badges"
        );
        std::fs::remove_file(fx.dir.path().join("claude-hold-roster")).unwrap();
        // One read of the page's Claude threads serves both its activity and
        // its background marks (A8-13).
        let lists = || {
            std::fs::read_to_string(fx.dir.path().join("claude-requests"))
                .unwrap()
                .lines()
                .filter(|line| line.contains("\"method\": \"thread/list\""))
                .count()
        };
        let before = lists();
        assert_eq!(marks().await, [true, true, false]);
        assert_eq!(lists() - before, 1, "one thread list read per page");
        // Task summaries are scoped per session; listing never waits on a
        // roster that could hide another session's independently verified work.
        let requests = std::fs::read_to_string(fx.dir.path().join("claude-requests")).unwrap();
        assert!(!requests.contains("thread/loaded/list"), "{requests}");
        assert!(
            requests
                .lines()
                .any(|line| line.contains("thread/backgroundTasks/list")
                    && line.contains("\"runningOnly\": true")
                    && line.contains(&format!("\"threadId\": \"{CLAUDE_THREAD}\""))),
            "{requests}"
        );
        // Tasks Claude on the Mac runs use the same summary owner.
        unreply("thread/loaded/list");
        reply(
            "thread/backgroundTasks/list",
            json!({"data": [{"id": "task", "kind": "agent", "status": "running", "runningElsewhere": true}]}),
        );
        assert_eq!(marks().await, [true, true, false]);
        // A selected lingering chat can hold the roster beyond the list's
        // budget. Its timeout must not discard this verified sibling answer.
        std::fs::write(fx.dir.path().join("claude-hold-roster"), "").unwrap();
        assert_eq!(
            marks().await,
            [true, true, false],
            "a roster timeout keeps verified badges"
        );
        std::fs::remove_file(fx.dir.path().join("claude-hold-roster")).unwrap();
        // Nothing left running: only the discovered session's app still works.
        // The list shows the verified answer while it reads the new one, then
        // the new one (`task_summary`).
        reply("thread/backgroundTasks/list", json!({"data": []}));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while marks().await != [false, true, false] {
            assert!(
                std::time::Instant::now() < deadline,
                "the finished tasks unmark the row"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        crate::desktop_activity::tests::BUSY_DIRS
            .lock()
            .unwrap()
            .retain(|entry| entry != &dirs);
        assert_eq!(marks().await, [false, false, false]);
    }

    // Contract: a chat deleted in Cupola stays out of the Project's list while
    // the provider still reports its session, on the first page and after it.
    // The page's rows are checked against the deletion list together
    // (`project_sessions_deleted`), not with one query per row.
    #[cfg(unix)]
    #[tokio::test]
    async fn native_rows_of_deleted_chats_stay_out_of_the_list_page_after_page() {
        use crate::provider_switch_tests::CLAUDE_SESSION;
        use crate::test_support::call;
        let fx = crate::provider_switch_tests::fx().await;
        let first = fx
            .queue(
                "start in Claude",
                Some((AgentFamily::Claude, "claude:sonnet")),
            )
            .await;
        fx.release(&first).await.unwrap();
        fx.finish(&first).await;
        let cwd = fx
            .state
            .store
            .project_conversation(&fx.chat)
            .await
            .unwrap()
            .unwrap()
            .cwd;
        let now = chrono::Utc::now().timestamp();
        let ids = (0..6)
            .map(|n| {
                if n == 2 {
                    CLAUDE_SESSION.to_owned()
                } else {
                    uuid::Uuid::new_v4().to_string()
                }
            })
            .collect::<Vec<_>>();
        let sessions = ids
            .iter()
            .enumerate()
            .map(|(age, id)| json!({"sessionId": id, "cwd": cwd, "title": id, "updatedAt": now - age as i64}))
            .collect::<Vec<_>>();
        std::fs::write(
            fx.dir
                .path()
                .join("claude-reply-project-sessions-list.json"),
            json!({"data": sessions, "nextCursor": null, "partial": false}).to_string(),
        )
        .unwrap();
        let listed = || async {
            let mut seen = Vec::new();
            let mut cursor = None::<String>;
            loop {
                let uri = match &cursor {
                    Some(cursor) => {
                        format!("/api/v1/projects/project/threads?limit=2&cursor={cursor}")
                    }
                    None => "/api/v1/projects/project/threads?limit=2".to_owned(),
                };
                let (status, page) = call(&fx.state, "GET", &uri, json!({})).await;
                assert_eq!(status, StatusCode::OK, "{page}");
                seen.extend(
                    page["threads"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|row| row["reference"].as_str().map(str::to_owned)),
                );
                match page["nextCursor"].as_str() {
                    Some(next) => cursor = Some(next.to_owned()),
                    None => break seen,
                }
            }
        };
        let claude = |rows: Vec<String>| {
            rows.into_iter()
                .filter(|row| row.starts_with("claude:"))
                .collect::<Vec<_>>()
        };
        assert!(
            claude(listed().await).contains(&format!("claude:{CLAUDE_SESSION}")),
            "the session is listed before its chat is deleted"
        );
        fx.state
            .store
            .delete_project_chat(&fx.chat, "now")
            .await
            .unwrap();
        let expected = ids
            .iter()
            .filter(|id| id.as_str() != CLAUDE_SESSION)
            .map(|id| format!("claude:{id}"))
            .collect::<Vec<_>>();
        assert_eq!(claude(listed().await), expected);
    }

    // Benchmark: threads.list at several limits over a real Claude catalog
    // reader and a synthetic project folder of 300 large transcripts, 30 of them
    // chats, 13 of which were deleted in Cupola: the Project lists 17, all within
    // the runtime's first page of 30, so a limit above 17 must ask for the
    // page after it (the shape of the slow live Projects). It needs `node` and
    // `npm ci` in services/claude-runtime, so it only runs when asked:
    //   cargo test -p wonderd --lib thread_page_limit_sweep -- --ignored --nocapture
    // Set WONDER_BENCH_RUNTIME to another checkout's services/claude-runtime to
    // measure that runtime ("before") with this checkout's fixture and harness.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "benchmark: needs node and services/claude-runtime/node_modules"]
    async fn thread_page_limit_sweep_over_a_real_claude_catalog() {
        use crate::test_support::call;
        let runtime = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../services/claude-runtime"
        ));
        let measured = std::env::var("WONDER_BENCH_RUNTIME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| runtime.clone());
        let fx = crate::provider_switch_tests::fx().await;
        let cwd = fx
            .state
            .store
            .project("project")
            .await
            .unwrap()
            .unwrap()
            .roots[0]
            .path
            .clone();
        let config = fx.dir.path().join("claude-config");
        let written = std::process::Command::new("node")
            .arg(runtime.join("bench/write-fixture.mjs"))
            .arg(&config)
            .arg(&cwd)
            .args(["300", "10"])
            .output()
            .unwrap();
        assert!(written.status.success(), "{written:?}");
        let all_chats: Vec<String> = serde_json::from_slice(&written.stdout).unwrap();
        assert_eq!(all_chats.len(), 30);
        // Delete every other one of the newest 26.
        let claude_store = fx.state.projects.claude_store.clone();
        let project = fx.state.store.project("project").await.unwrap().unwrap();
        let mut chats = Vec::new();
        for (index, native) in all_chats.iter().enumerate() {
            if index >= 26 || index % 2 == 0 {
                chats.push(native.clone());
                continue;
            }
            let id = format!("bench-deleted-{index}");
            fx.state
                .store
                .create_project_conversation(wonder_store::ProjectConversationInsert {
                    conversation_id: &id,
                    project_id: "project",
                    family: AgentFamily::Claude,
                    provider_store: &claude_store,
                    native_session_id: Some(native),
                    cwd: &project.roots[0].path,
                    roots_revision: project.roots_revision,
                    title: "Deleted",
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: "ask",
                    plan_mode: false,
                    creation_request_id: None,
                    now: "now",
                })
                .await
                .unwrap();
            fx.state
                .store
                .delete_project_chat(&id, "now")
                .await
                .unwrap();
        }
        assert_eq!(chats.len(), 17);
        std::fs::write(
            fx.dir.path().join("claude-native-list"),
            json!({"argv": ["node", runtime.join("bench/list-rpc.mjs"), measured],
                "env": {"CLAUDE_CONFIG_DIR": config}})
            .to_string(),
        )
        .unwrap();
        let rows = |limit: usize| {
            let fx = &fx;
            async move {
                let started = std::time::Instant::now();
                let (status, page) = call(
                    &fx.state,
                    "GET",
                    &format!("/api/v1/projects/project/threads?limit={limit}"),
                    json!({}),
                )
                .await;
                let took = started.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(status, StatusCode::OK, "{page}");
                let refs = page["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|row| row["reference"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                (took, refs)
            }
        };
        let cold = rows(30).await;
        println!(
            "runtime {}\nfirst threads.list (limit 30, cold): {:.0} ms, {} rows",
            measured.display(),
            cold.0,
            cold.1.len()
        );
        let mut p50 = std::collections::HashMap::new();
        let mut lists = std::collections::HashMap::new();
        for limit in [5, 10, 15, 16, 17, 18, 30] {
            let mut times = Vec::new();
            let mut refs = Vec::new();
            for _ in 0..20 {
                let (took, found) = rows(limit).await;
                times.push(took);
                refs = found;
            }
            times.sort_by(f64::total_cmp);
            println!(
                "limit {limit:>2}: p50 {:>7.1} ms  p90 {:>7.1} ms  min {:>7.1} ms  ({} rows)",
                times[10],
                times[18],
                times[0],
                refs.len()
            );
            p50.insert(limit, times[10]);
            lists.insert(limit, refs);
        }
        // Same rows in the same order whatever the limit.
        let all = &lists[&30];
        for limit in [5, 10, 15, 16, 17, 18] {
            assert_eq!(
                &all[..lists[&limit].len()],
                &lists[&limit][..],
                "limit {limit}"
            );
        }
        let claude = all
            .iter()
            .filter(|row| row.starts_with("claude:"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            claude,
            chats
                .iter()
                .map(|chat| format!("claude:{chat}"))
                .collect::<Vec<_>>(),
            "the 17 chats that were not deleted, newest first"
        );
        assert!(
            p50[&30] <= 2.0 * p50[&17] + 5.0,
            "limit 30 ({:.1} ms) is within 2x of limit 17 ({:.1} ms)",
            p50[&30],
            p50[&17]
        );
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = crate::test_git::git()
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    /// A repository at `repo` with one commit and, when named, one linked
    /// worktree at `linked`.
    fn git_repo(repo: &std::path::Path, linked: Option<&std::path::Path>) {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q"]);
        git(repo, &["commit", "--allow-empty", "-qm", "fixture"]);
        if let Some(linked) = linked {
            git(repo, &["worktree", "add", "-qd", linked.to_str().unwrap()]);
        }
    }

    // Contract: one membership rule decides which Project owns a thread for
    // previews, approvals, questions, push and Live Activity: the saved folder
    // exactly, or a linked worktree Git registered for it (while worktree chats
    // are on). Subfolders, look-alike prefixes, other repositories and their
    // worktrees, symlink aliases, worktrees of a repository the Project folder
    // only sits inside, removed worktrees and worktrees whose repository is
    // gone do not belong. Forged or climbing `.git` files are never listed by
    // Git; `registered_worktree` rejects them on its own at the end.
    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_threads_belong_to_their_project_and_nothing_else_does() {
        use crate::test_support::fixture;
        let (dir, state) = fixture().await;
        let base = crate::canonical_path::canonicalize(dir.path()).unwrap();
        let repo = base.join("Wonder");
        let linked = base.join("wt-feature");
        git_repo(&repo, Some(&linked));
        std::fs::create_dir(repo.join("sub")).unwrap();
        git_repo(&base.join("other"), Some(&base.join("other-wt")));
        git_repo(&base.join("Wonder-evil"), None);
        git_repo(&base.join("gone"), Some(&base.join("gone-wt")));
        git_repo(&base.join("mono"), Some(&base.join("mono-wt")));
        // A `.bare` layout: the folder's `.git` file points at a bare clone.
        let bare = base.join("bare-proj");
        std::fs::create_dir(&bare).unwrap();
        git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                repo.to_str().unwrap(),
                bare.join(".bare").to_str().unwrap(),
            ],
        );
        std::fs::write(bare.join(".git"), "gitdir: ./.bare\n").unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-qd",
                bare.join("main-wt").to_str().unwrap(),
            ],
        );
        // A bare repository saved as the Project folder itself.
        let naked = base.join("naked.git");
        git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                repo.to_str().unwrap(),
                naked.to_str().unwrap(),
            ],
        );
        git(
            &naked,
            &[
                "worktree",
                "add",
                "-qd",
                base.join("naked-wt").to_str().unwrap(),
            ],
        );
        std::fs::create_dir(base.join("mono/pkg")).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-qd",
                base.join("wt-done").to_str().unwrap(),
            ],
        );
        // A copy of a real worktree's `.git` file, never registered by Git.
        std::fs::create_dir(base.join("forged")).unwrap();
        std::fs::copy(linked.join(".git"), base.join("forged/.git")).unwrap();
        // A pointer that climbs out of the repository's worktrees folder.
        std::fs::create_dir(base.join("climber")).unwrap();
        std::fs::write(
            base.join("climber/.git"),
            format!(
                "gitdir: {}/.git/worktrees/../../../other/.git\n",
                repo.display()
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink(&repo, base.join("alias")).unwrap();
        std::os::unix::fs::symlink(&linked, base.join("wt-alias")).unwrap();
        // The Project is saved through a symlink; both spellings are its folder.
        let saved = validate_folders(&[base.join("alias").to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("p", "p", "hash", "Wonder", &saved, 0, "now")
            .await
            .unwrap();
        let gone_roots =
            validate_folders(&[base.join("gone").to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("g", "g", "hash-g", "Gone", &gone_roots, 0, "now")
            .await
            .unwrap();
        let package =
            validate_folders(&[base.join("mono/pkg").to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("m", "m", "hash-m", "Package", &package, 0, "now")
            .await
            .unwrap();
        for (id, folder) in [("b", &bare), ("n", &naked)] {
            let roots = validate_folders(&[folder.to_str().unwrap().into()], &[]).unwrap();
            state
                .store
                .create_project(id, id, &format!("hash-{id}"), id, &roots, 0, "now")
                .await
                .unwrap();
        }
        let cases: &[(&str, &str, std::path::PathBuf, bool)] = &[
            ("folder", "p", repo.clone(), true),
            ("saved-symlink-spelling", "p", base.join("alias"), true),
            ("linked-worktree", "p", linked.clone(), true),
            ("subfolder", "p", repo.join("sub"), false),
            ("prefix-look-alike", "p", base.join("Wonder-evil"), false),
            ("unrelated-repo", "p", base.join("other"), false),
            ("unrelated-worktree", "p", base.join("other-wt"), false),
            ("symlink-into-worktree", "p", base.join("wt-alias"), false),
            ("forged-git-file", "p", base.join("forged"), false),
            ("climbing-git-file", "p", base.join("climber"), false),
            ("worktree-of-removed-repo", "g", base.join("gone-wt"), true),
            ("removed-worktree", "p", base.join("wt-done"), true),
            ("package-in-larger-repo", "m", base.join("mono/pkg"), true),
            (
                "worktree-of-enclosing-repo",
                "m",
                base.join("mono-wt"),
                false,
            ),
            ("bare-layout-worktree", "b", bare.join("main-wt"), true),
            ("bare-repo-worktree", "n", base.join("naked-wt"), true),
        ];
        for (id, project, cwd, _) in cases {
            create_conversation(&state, id, project, cwd.to_str().unwrap()).await;
        }
        let owner = |id: &'static str| {
            let state = state.clone();
            async move {
                state
                    .store
                    .owning_project_conversation(id)
                    .await
                    .unwrap()
                    .map(|owner| owner.project_id)
            }
        };
        // Before discovery nothing vouches for a worktree.
        assert_eq!(owner("linked-worktree").await, None);
        for project in ["p", "g", "m", "b", "n"] {
            let project = state.store.project(project).await.unwrap().unwrap();
            project_folders(&state, &project).await;
        }
        for (id, project, cwd, owned) in cases {
            assert_eq!(
                owner(id).await.as_deref(),
                owned.then_some(*project),
                "{id}: {}",
                cwd.display()
            );
        }

        // Worktree chats off: only the saved folder itself belongs.
        state.store.set_include_worktrees(false).await.unwrap();
        assert_eq!(owner("linked-worktree").await, None);
        assert_eq!(owner("folder").await.as_deref(), Some("p"));
        state.store.set_include_worktrees(true).await.unwrap();
        assert_eq!(owner("linked-worktree").await.as_deref(), Some("p"));

        // `git worktree remove` while the repository stays.
        git(
            &repo,
            &["worktree", "remove", base.join("wt-done").to_str().unwrap()],
        );
        let wonder = state.store.project("p").await.unwrap().unwrap();
        project_folders(&state, &wonder).await;
        assert_eq!(owner("removed-worktree").await, None);
        assert_eq!(owner("linked-worktree").await.as_deref(), Some("p"));

        // The repository goes away; its worktree's thread no longer belongs.
        std::fs::remove_dir_all(base.join("gone")).unwrap();
        let gone = state.store.project("g").await.unwrap().unwrap();
        project_folders(&state, &gone).await;
        assert_eq!(owner("worktree-of-removed-repo").await, None);

        // The file-level proof on its own.
        let common = repo.join(".git");
        assert!(registered_worktree(&common, &linked));
        for stranger in ["forged", "climber", "other-wt", "gone-wt", "Wonder-evil"] {
            assert!(
                !registered_worktree(&common, &base.join(stranger)),
                "{stranger}"
            );
        }
    }

    async fn create_conversation(state: &AppState, id: &str, project: &str, cwd: &str) {
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: id,
                project_id: project,
                family: AgentFamily::Codex,
                provider_store: provider_store(state, AgentFamily::Codex),
                native_session_id: None,
                cwd,
                roots_revision: 1,
                title: id,
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "workspace",
                claude_approval: AgentFamily::Codex.provider().default_approval,
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
    }

    // Contract: a thread whose folder is a linked worktree of a saved folder
    // gets the same Files, Terminal, goals, commands, skills and pull request
    // access as one in the saved folder; with worktree chats off, Files and
    // Terminal refuse it with the distinct "worktree not in project" code.
    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_threads_resolve_their_folder_everywhere() {
        use crate::test_support::{call, fixture};
        use tower::ServiceExt;
        let (dir, state) = fixture().await;
        let state = AppState {
            terminals: Arc::new(crate::terminal::TerminalHost::with_shell(
                Default::default(),
                std::path::PathBuf::from("/bin/sh"),
            )),
            ..state
        };
        let repo = dir.path().join("repo");
        let linked = dir.path().join("worker");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            &["init", "-q"][..],
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
            &[
                "worktree",
                "add",
                "-qb",
                "agent/fixture",
                linked.to_str().unwrap(),
            ],
        ] {
            let output = crate::test_git::git()
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        let linked = crate::canonical_path::canonicalize(linked).unwrap();
        let cwd = linked.to_str().unwrap();
        let roots = validate_folders(&[repo.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project("wt", "wt", "hash", "Worktrees", &roots, 0, "now")
            .await
            .unwrap();
        for (id, family) in [
            ("wt-codex", AgentFamily::Codex),
            ("wt-claude", AgentFamily::Claude),
        ] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: "wt",
                    family,
                    provider_store: provider_store(&state, family),
                    native_session_id: None,
                    cwd,
                    roots_revision: 1,
                    title: id,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: family.provider().default_approval,
                    plan_mode: false,
                    creation_request_id: None,
                    now: "now",
                })
                .await
                .unwrap();
        }
        let file = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&file).unwrap().replace(
            "    elif method == 'thread/start':",
            "    elif method == 'skills/list':\n        result = {'data':[{'cwd':r['params']['cwds'][0],'skills':[],'errors':[]}]}\n    elif method == 'thread/start':",
        );
        state.projects.shutdown().await;
        std::fs::write(&file, source).unwrap();
        let config = state.projects.launch_config();
        state
            .projects
            .codex
            .lock()
            .await
            .restart(config)
            .await
            .unwrap();

        let (status, files) = call(
            &state,
            "GET",
            "/api/v1/conversations/wt-codex/workspace/roots",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{files}");
        assert_eq!(files["roots"][0]["id"], "workspace");
        assert_eq!(
            files["roots"][0]["path"], cwd,
            "Files open the worktree, not its parent"
        );
        let (status, terminal) = call(
            &state,
            "POST",
            "/api/v1/conversations/wt-codex/terminals",
            json!({"cols": 80, "rows": 24}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{terminal}");
        state.terminals.close_all();
        let (status, goal) = call(
            &state,
            "GET",
            "/api/v1/conversations/wt-codex/goal",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{goal}");
        let (status, commands) = call(
            &state,
            "GET",
            "/api/v1/conversations/wt-claude/commands",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{commands}");
        let (status, skills) = call(
            &state,
            "GET",
            "/api/v1/project-conversations/wt-codex/skills",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{skills}");
        assert!(
            skills["folderId"]
                .as_str()
                .unwrap()
                .starts_with("worktree:"),
            "{skills}"
        );
        // The folder ID a worktree catalog reports opens the same catalog.
        let (status, again) = call(
            &state,
            "GET",
            &format!(
                "/api/v1/projects/wt/skills?folderId={}&rootsRevision=1",
                skills["folderId"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{again}");
        // The thread's branch PR and the PR it opened both come back for the
        // worktree, read in the worktree through a stand-in signed-in `gh`.
        // Its addresses are this test's own: PR states are cached host-wide.
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
echo "$PWD $@" >> "$(dirname "$0")/gh-calls"
case "$1 $2" in
  "pr list") echo '[{"number":31,"title":"Worktree branch","state":"OPEN","isDraft":false,"url":"https://github.com/o/worktree-fixture/pull/31","statusCheckRollup":[]}]' ;;
  "pr view") echo '{"number":32,"title":"Opened here","state":"MERGED","isDraft":false,"url":"https://github.com/o/worktree-fixture/pull/32","statusCheckRollup":[]}' ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::pull_requests::use_test_gh(gh);
        state
            .store
            .link_thread_pull_request(
                "wt-codex",
                "https://github.com/o/worktree-fixture/pull/32",
                cwd,
                "now",
            )
            .await
            .unwrap();
        let (status, pulls) = call(
            &state,
            "GET",
            "/api/v1/project-conversations/wt-codex/pull-requests",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{pulls}");
        assert_eq!(pulls["available"], true, "{pulls}");
        let numbers: Vec<_> = pulls["pullRequests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pr| pr["number"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [31, 32], "{pulls}");
        let calls = std::fs::read_to_string(dir.path().join("gh-calls")).unwrap();
        assert!(
            calls.contains(&format!("{cwd} pr list --head agent/fixture")),
            "{calls}"
        );

        state.store.set_include_worktrees(false).await.unwrap();
        for (method, path) in [
            ("GET", "/api/v1/conversations/wt-codex/workspace/roots"),
            ("POST", "/api/v1/conversations/wt-codex/terminals"),
        ] {
            let response = crate::router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .header("x-wonder-loopback-capability", &state.loopback_capability)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            json!({"cols": 80, "rows": 24}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            assert_eq!(
                response.headers().get(ERROR_CODE_HEADER).unwrap(),
                WORKTREE_NOT_IN_PROJECT,
                "{path}"
            );
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(body, crate::filesystem::WORKTREE_NOT_IN_PROJECT.as_bytes());
        }
        let (status, _) = call(
            &state,
            "GET",
            "/api/v1/conversations/wt-claude/commands",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // Contract: one Project discovers a linked worktree through the real Git
    // registry, without a second configured folder or provider-specific matching.
    #[tokio::test]
    async fn project_worktree_catalog_uses_linked_repository_folders() {
        use crate::test_support::{call, fixture};
        let (dir, mut state) = fixture().await;
        let repo = dir.path().join("repo");
        let linked = dir.path().join("worker");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = crate::test_git::git()
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ]);
        git(&[
            "worktree",
            "add",
            "-qb",
            "agent/fixture",
            linked.to_str().unwrap(),
        ]);
        let linked = crate::canonical_path::canonicalize(linked).unwrap();
        let roots = validate_folders(&[repo.to_str().unwrap().into()], &[]).unwrap();
        state
            .store
            .create_project(
                "worktrees",
                "worktrees",
                "hash",
                "Worktrees",
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        let file = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&file).unwrap().replace(
            "\n    elif method == 'thread/list':",
            "\n    elif method == 'thread/list':\n        cwd = json.load(open(root + '/worktree-cwd.json'))\n        result = {'data': [{'id':'worker-thread','cwd':cwd,'name':'Worktree worker','updatedAt':1,'parentThreadId':None}] if cwd in r.get('params',{}).get('cwd',[]) and not r.get('params',{}).get('archived',False) else [], 'nextCursor':None}\n        print(json.dumps({'id':r['id'],'result':result}),flush=True)\n        continue\n    elif False:");
        std::fs::write(&file, source).unwrap();
        std::fs::write(
            dir.path().join("worktree-cwd.json"),
            json!(linked).to_string(),
        )
        .unwrap();
        let config = state.projects.launch_config();
        state
            .projects
            .codex
            .lock()
            .await
            .restart(config)
            .await
            .unwrap();
        let (status, page) = call(
            &state,
            "GET",
            "/api/v1/projects/worktrees/threads",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["threads"].as_array().unwrap().len(), 1, "{page}");
        assert_eq!(page["threads"][0]["worktreeLabel"], "agent/fixture");
        let (status, setting) = call(&state, "GET", "/api/v1/settings/projects", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(setting["includeWorktrees"], true);
        // Stored rows (including OpenCode, which has no external importer)
        // obey the same membership and do not duplicate the native Codex row.
        let mut working = HashMap::new();
        for (family, native) in [
            (AgentFamily::Codex, "worker-thread"),
            (AgentFamily::Claude, "claude-worker"),
            (AgentFamily::OpenCode, "opencode-worker"),
        ] {
            let id = format!("worker-{}", family.as_str());
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: &id,
                    project_id: "worktrees",
                    family,
                    provider_store: provider_store(&state, family),
                    native_session_id: Some(native),
                    cwd: linked.to_str().unwrap(),
                    roots_revision: 1,
                    title: &id,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: family.provider().default_approval,
                    plan_mode: false,
                    creation_request_id: None,
                    now: "2000-01-01T00:00:00Z",
                })
                .await
                .unwrap();
            if family != AgentFamily::OpenCode {
                state
                    .store
                    .update_project_conversation(
                        &id,
                        ProjectConversationPatch {
                            pinned: Some(true),
                            ..Default::default()
                        },
                        "now",
                    )
                    .await
                    .unwrap();
            }
            working.insert(id, crate::live_activity::Desktop::Working);
        }
        state.live_activity.desktop_reading(working, vec![]);
        for enabled in [true, false, true] {
            let (status, saved) = call(
                &state,
                "PUT",
                "/api/v1/settings/projects",
                json!({"includeWorktrees":enabled}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{saved}");
            assert_eq!(saved["includeWorktrees"], enabled);
            let reopened = wonder_store::Store::connect(&format!(
                "sqlite://{}",
                dir.path().join("state.db").display()
            ))
            .await
            .unwrap();
            assert_eq!(
                reopened.include_worktrees().await.unwrap(),
                enabled,
                "host setting survives reopening storage"
            );
            let (_, page) = call(
                &state,
                "GET",
                "/api/v1/projects/worktrees/threads",
                json!({}),
            )
            .await;
            let rows = page["threads"].as_array().unwrap();
            assert_eq!(rows.len(), if enabled { 3 } else { 0 }, "{page}");
            assert!(rows
                .iter()
                .all(|row| row["worktreeLabel"] == "agent/fixture"));
            let visible = visible_conversations(&state).await.unwrap();
            let activity = crate::live_activity::read_inputs(&state).await.unwrap();
            assert_eq!(
                activity.desktop.len(),
                rows.len(),
                "activity counts precisely the displayable worker conversations"
            );
            for row in rows {
                assert!(visible.contains(row["conversationId"].as_str().unwrap()));
            }
            let (_, projects) = call(&state, "GET", "/api/v1/projects", json!({})).await;
            let pins = projects["pinned"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["projectId"] == "worktrees")
                .count();
            assert_eq!(pins, if enabled { 2 } else { 0 });
        }
        // Owned work uses the same visibility contract, and a desktop reading
        // for the same conversations must not count those threads twice.
        for family in [
            AgentFamily::Codex,
            AgentFamily::Claude,
            AgentFamily::OpenCode,
        ] {
            let id = format!("worker-{}", family.as_str());
            let receipt = format!("worktree-owned-{}", family.as_str());
            let wonder_store::MessageInsert::Inserted(message) = state
                .store
                .insert_message("owner", &receipt, "Synthetic work", "hash", &id, "now")
                .await
                .unwrap()
            else {
                panic!("new fixture message")
            };
            state
                .store
                .update_message_delivery(&message.id, "streaming", Some(&receipt), Some("turn"))
                .await
                .unwrap();
        }
        for enabled in [false, true] {
            state.store.set_include_worktrees(enabled).await.unwrap();
            let activity = crate::live_activity::read_inputs(&state).await.unwrap();
            assert_eq!(activity.owned.len(), if enabled { 3 } else { 0 });
            assert!(
                activity.desktop.is_empty(),
                "owned and native readings count once"
            );
            let (_, page) = call(
                &state,
                "GET",
                "/api/v1/projects/worktrees/threads",
                json!({}),
            )
            .await;
            assert_eq!(
                page["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| row["isWorking"] == true)
                    .count(),
                activity.owned.len()
            );
        }
        // Watching the registry and per-worktree HEAD notices additions,
        // detached HEAD labels, branch changes and deleted/prunable worktrees.
        let detached = dir.path().join("detached-folder");
        git(&["worktree", "add", "--detach", detached.to_str().unwrap()]);
        let project = state.store.project("worktrees").await.unwrap().unwrap();
        let folders = project_folders(&state, &project).await;
        let detached = crate::canonical_path::canonicalize(detached).unwrap();
        let execution = execution_project(
            &project,
            folders.project.root_for(linked.to_str().unwrap()).unwrap(),
        );
        assert_eq!(execution.roots.len(), project.roots.len());
        assert!(execution.root_for(linked.to_str().unwrap()).is_some());
        assert!(execution.root_for(detached.to_str().unwrap()).is_none());

        assert_eq!(
            folders.label(detached.to_str().unwrap()).as_deref(),
            Some("detached-folder")
        );
        let rename = crate::test_git::git()
            .arg("-C")
            .arg(&linked)
            .args(["branch", "-m", "agent/renamed"])
            .output()
            .unwrap();
        assert!(rename.status.success());
        assert_eq!(
            project_folders(&state, &project)
                .await
                .label(linked.to_str().unwrap())
                .as_deref(),
            Some("agent/renamed")
        );
        std::fs::remove_dir_all(&detached).unwrap();
        assert!(!project_folders(&state, &project)
            .await
            .contains(detached.to_str().unwrap()));
        let private_home = dir.path().join("private-provider-home");
        let private_worktree = private_home.join("worktrees/codex");
        std::fs::create_dir_all(private_worktree.parent().unwrap()).unwrap();
        git(&[
            "worktree",
            "add",
            "-qb",
            "agent/desktop",
            private_worktree.to_str().unwrap(),
        ]);
        let private_worktree = crate::canonical_path::canonicalize(private_worktree).unwrap();
        state
            .denied_roots
            .push(private_home.to_str().unwrap().into());
        assert!(validate_folder(private_worktree.to_str().unwrap(), &state.denied_roots).is_err());
        assert!(project_folders(&state, &project).await.contains(private_worktree.to_str().unwrap()),
            "provider-home privacy must not hide the native chat; execution remains separately checked");
        // An explicitly configured worktree stays eligible when automatic
        // inclusion is off, and overlapping discovery still emits it once.
        let both = validate_folders(
            &[
                repo.to_str().unwrap().into(),
                linked.to_str().unwrap().into(),
            ],
            &[],
        )
        .unwrap();
        state
            .store
            .update_project_roots("worktrees", 1, &both, 0, "now")
            .await
            .unwrap();
        state.store.set_include_worktrees(false).await.unwrap();
        let (_, page) = call(
            &state,
            "GET",
            "/api/v1/projects/worktrees/threads",
            json!({}),
        )
        .await;
        assert_eq!(page["threads"].as_array().unwrap().len(), 3, "{page}");
        assert_eq!(
            crate::live_activity::read_inputs(&state)
                .await
                .unwrap()
                .owned
                .len(),
            3
        );
        state.projects.shutdown().await;
    }

    // The provider cursor advances through empty filtered windows. Retrying an
    // HTTP cursor keeps the same rows, and changing roots invalidates it.
    #[tokio::test]
    async fn claude_catalog_continuation_survives_partial_pages_and_retry() {
        let (dir, mut state, _) = handoff_fixture().await;
        let entrypoint = dir.path().join("claude-catalog.py");
        std::fs::write(&entrypoint, r#"import json, sys, os
root = os.path.dirname(__file__)
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'thread/list': result = {'data': [], 'nextCursor': None}
    elif m == 'project/sessions/list':
        assert 'offset' not in p
        with open(root + '/catalog-calls', 'a') as log: log.write(json.dumps(p) + '\n')
        cursor = p.get('cursor')
        rows = lambda ids: [dict({'sessionId': i, 'title': i, 'updatedAt': 100}, **({'isComputerSession': i == 'a'} if i in ['a', 'b'] else {})) for i in ids]
        if os.path.exists(root + '/malformed-catalog'): result = {'nextCursor': None}
        elif os.path.exists(root + '/pending-first') and (cursor is None or os.path.exists(root + '/held-first')):
            import time
            time.sleep(1.05)
            result = {'data': [], 'nextCursor': 'pending-first', 'partial': True}
        elif cursor == 'pending-first': result = {'data': rows(['a', 'b']), 'nextCursor': 'after-b', 'partial': False}
        elif p.get('sessionId'): result = {'data': [{'sessionId': p['sessionId'], 'cwd': p['dirs'][0], 'title': 'Older exact session', 'updatedAt': 1, 'isComputerSession': True}], 'nextCursor': None, 'partial': False}
        elif cursor is None: result = {'data': rows(['a', 'b']), 'nextCursor': 'after-b', 'partial': False}
        elif cursor == 'after-b': result = {'data': [], 'nextCursor': 'after-filtered', 'partial': True}
        else: result = {'data': rows(['a', 'c']), 'nextCursor': None, 'partial': False}
    elif m == 'project/session/attach': result = {'thread': {'id': 'claude-' + p['sessionId']}}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#).unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime {
            client: client.clone(),
            config,
        }));
        let fetch = |cursor| {
            threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
                Query(ThreadsQuery {
                    cursor,
                    limit: Some(1),
                }),
            )
        };
        let body = |response: Response| async {
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            crate::tests::validate_http_contract("projectThreadsPage", &value);
            value
        };
        let token = |page: &Value| page["nextCursor"].as_str().map(str::to_owned);
        std::fs::write(dir.path().join("pending-first"), "").unwrap();
        std::fs::write(dir.path().join("held-first"), "").unwrap();
        let started = Instant::now();
        let held = body(fetch(None).await).await;
        assert_eq!(held["threads"], json!([]));
        // A slow catalog is still loading: a cursor, never a failure entry.
        assert_eq!(held["partial"], json!([]), "{held}");
        assert_eq!(held["loading"], true);
        assert!(token(&held).is_some());
        assert!(started.elapsed() < Duration::from_secs(3));
        let requests = std::fs::read_to_string(dir.path().join("catalog-calls")).unwrap();
        assert_eq!(
            requests.lines().count(),
            2,
            "Still-pending reads must defer after one continuation"
        );
        std::fs::remove_file(dir.path().join("held-first")).unwrap();
        std::fs::write(dir.path().join("catalog-calls"), "").unwrap();
        let started = Instant::now();
        let cold = body(fetch(None).await).await;
        assert_eq!(cold["threads"][0]["reference"], "claude:a", "{cold}");
        assert_eq!(cold["partial"], json!([]));
        assert_eq!(cold["loading"], false);
        assert!(started.elapsed() < Duration::from_secs(3));
        let requests = std::fs::read_to_string(dir.path().join("catalog-calls")).unwrap();
        assert_eq!(
            requests.lines().count(),
            2,
            "One bounded cold-start continuation"
        );
        std::fs::remove_file(dir.path().join("pending-first")).unwrap();
        std::fs::write(dir.path().join("catalog-calls"), "").unwrap();
        let first = body(fetch(None).await).await;
        assert_eq!(first["threads"][0]["reference"], "claude:a");
        assert_eq!(first["threads"][0]["isComputerSession"], true);
        let second = body(fetch(token(&first)).await).await;
        let retry = body(fetch(token(&first)).await).await;
        assert_eq!(second["threads"], retry["threads"]);
        assert_eq!(second["threads"][0]["reference"], "claude:b");
        assert!(second["threads"][0].get("isComputerSession").is_none());
        let partial = body(fetch(token(&second)).await).await;
        assert_eq!(partial["threads"], json!([]));
        assert_eq!(partial["partial"], json!([]));
        assert_eq!(partial["loading"], true);
        assert!(token(&partial).is_some());
        let final_page = body(fetch(token(&partial)).await).await;
        assert_eq!(final_page["threads"].as_array().unwrap().len(), 1);
        assert_eq!(final_page["threads"][0]["reference"], "claude:c");
        assert!(final_page["threads"][0].get("isComputerSession").is_none());
        assert!(token(&final_page).is_none());
        let requests = std::fs::read_to_string(dir.path().join("catalog-calls")).unwrap();
        assert_eq!(
            requests.lines().count(),
            3,
            "Buffered/retried rows do not fetch another native prefix"
        );
        let project = state.store.project("project").await.unwrap().unwrap();
        {
            let mut catalog = state.runtime_catalog.write().await;
            catalog.models.push(crate::agent_defaults::tests::model(
                "claude:opus",
                AgentFamily::Claude,
                &["low"],
                None,
            ));
            catalog.resolve_defaults();
        }
        let old_id = "00000000-0000-4000-8000-000000005010";
        let attached = attach_native(&state, &project, AgentFamily::Claude, old_id)
            .await
            .unwrap();
        assert_eq!(attached.native_session_id.as_deref(), Some(old_id));
        assert_eq!(attached.title, "Older exact session");
        let mut pinned = attached_summary(&state, &attached, 1, None).await;
        pinned.is_pinned = true;
        mark_computer_sessions(&state, std::iter::once(&mut pinned)).await;
        assert!(
            pinned.is_computer_session,
            "A pin outside the native page uses exact producer evidence"
        );
        let receipt = attach(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path("project".into()),
            Json(AttachRequest {
                reference: format!("claude:{old_id}"),
            }),
        )
        .await;
        assert_eq!(receipt.status(), StatusCode::OK);
        let receipt: Value = serde_json::from_slice(
            &axum::body::to_bytes(receipt.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            receipt["isComputerSession"], true,
            "Attaching must preserve the catalog badge"
        );
        crate::tests::validate_http_contract("projectThreadSummary", &receipt);
        let held = client.lock().await;
        let mut blocked = ProviderPage::default();
        let start = Instant::now();
        fill_claude(
            &state,
            &project_folders(&state, &project).await,
            &mut blocked,
            10,
            tokio::time::Instant::now() + Duration::from_millis(30),
        )
        .await;
        assert!(blocked.deferred && !blocked.failed && !blocked.done);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "Native discovery deadline includes the runtime lock"
        );
        drop(held);
        std::fs::write(dir.path().join("malformed-catalog"), "").unwrap();
        let malformed = body(fetch(None).await).await;
        assert_eq!(malformed["partial"][0]["family"], "claude");
        assert_eq!(malformed["loading"], false);
        assert!(token(&malformed).is_some());
        let missing = attach_native(
            &state,
            &project,
            AgentFamily::Claude,
            "00000000-0000-4000-8000-000000009999",
        )
        .await
        .unwrap_err();
        assert_eq!(
            missing.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "An incomplete native lookup cannot establish not found"
        );
        let extra = dir.path().join("other-root");
        std::fs::create_dir(&extra).unwrap();
        let roots = validate_folders(&[extra.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .update_project_roots("project", 1, &roots, 0, "later")
            .await
            .unwrap();
        assert_eq!(fetch(token(&first)).await.status(), StatusCode::GONE);
        client.lock().await.shutdown().await.unwrap();
        state.projects.shutdown().await;
    }

    // Contract: a thread Wonder just started stays in the project's list while
    // the provider does not list it yet (Codex lists a new thread only once its
    // first turn has started), appears once when the provider does, and an
    // older thread the provider no longer lists is not revived.
    #[tokio::test]
    async fn just_started_threads_stay_listed_until_the_provider_lists_them() {
        let (dir, state, _message) = handoff_fixture().await;
        let cwd = dir.path().join("project-source");
        for (id, native, at) in [
            (
                "fresh-chat",
                "fresh-thread",
                chrono::Utc::now().to_rfc3339(),
            ),
            ("old-chat", "old-thread", "2026-01-01T00:00:00Z".to_owned()),
        ] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: "project",
                    family: AgentFamily::Codex,
                    provider_store: &state.projects.codex_store,
                    native_session_id: Some(native),
                    cwd: cwd.to_str().unwrap(),
                    roots_revision: 1,
                    title: id,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "read_only",
                    claude_approval: "ask",
                    plan_mode: false,
                    creation_request_id: None,
                    now: &at,
                })
                .await
                .unwrap();
        }
        let listed = || async {
            let response = threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
                Query(ThreadsQuery {
                    cursor: None,
                    limit: None,
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let page: Value = serde_json::from_slice(&bytes).unwrap();
            crate::tests::validate_http_contract("projectThreadsPage", &page);

            page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["conversationId"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(listed().await, ["fresh-chat"]);
        // Codex now lists it: one row, not two.
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread": {"id": "fresh-thread", "cwd": cwd, "name": "fresh-chat",
                "updatedAt": 1, "status": {"type": "active"}, "parentThreadId": null}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(listed().await, ["fresh-chat"]);
        let page = threads(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path("project".into()),
            Query(ThreadsQuery {
                cursor: None,
                limit: None,
            }),
        )
        .await;
        let bytes = axum::body::to_bytes(page.into_body(), 1_000_000)
            .await
            .unwrap();
        let page: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            page["threads"][0]["isWorking"], true,
            "App Server already reports activity; an old timestamp cannot override it"
        );
    }

    // Contract: one Project holding all of the newest threads fills the
    // global list; a per-Project cap must not let older Projects in.
    #[tokio::test]
    async fn recent_threads_take_the_newest_even_from_one_project() {
        let (dir, mut state, _) = handoff_fixture().await;
        let source = dir.path().join("second");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project("second", "second", "hash", "Second", &roots, 0, "now")
            .await
            .unwrap();
        let entrypoint = dir.path().join("claude-recent.py");
        std::fs::write(&entrypoint, r#"import json, sys
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'thread/list': result = {'data': [], 'nextCursor': None}
    elif m == 'project/sessions/list':
        busy = any(d.endswith('project-source') for d in p['dirs'])
        quiet = any(d.endswith('second') for d in p['dirs'])
        rows = [{'sessionId': ('busy-%02d' if busy else 'quiet-%02d') % i, 'title': 't', 'updatedAt': (2000 if busy else 100) - i} for i in range(60 if busy else 10 if quiet else 0)]
        if quiet: rows[0]['updatedAt'] = 5000
        start = int(p.get('cursor') or 0); end = start + min(p['limit'], 100)
        result = {'data': rows[start:end], 'nextCursor': str(end) if end < len(rows) else None, 'partial': False}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#).unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
        // quiet-00 is pinned in Wonder long ago and just used on the desktop.
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "pinned-quiet",
                project_id: "second",
                family: AgentFamily::Claude,
                provider_store: &state.projects.claude_store,
                native_session_id: Some("quiet-00"),
                cwd: source.to_str().unwrap(),
                roots_revision: 1,
                title: "pinned",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "1970-01-01T00:00:01Z",
            })
            .await
            .unwrap();
        state
            .store
            .update_project_conversation(
                "pinned-quiet",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                "1970-01-01T00:00:01Z",
            )
            .await
            .unwrap();
        let recent = |limit| {
            let state = state.clone();
            async move {
                let response = recent_threads(
                    State(state),
                    Extension(OwnerAuthority),
                    Query(RecentThreadsQuery { limit: Some(limit) }),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                    .await
                    .unwrap();
                let page: Value = serde_json::from_slice(&bytes).unwrap();
                crate::tests::validate_http_contract("recentThreadsPage", &page);
                page["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|t| t["thread"]["reference"].as_str())
                    .filter(|r| r.starts_with("claude:"))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            }
        };
        let busy = |n: usize| {
            (0..n)
                .map(|i| format!("claude:busy-{i:02}"))
                .collect::<Vec<_>>()
        };
        let mut newest = vec!["claude:quiet-00".to_owned()];
        newest.extend(busy(49));
        assert_eq!(
            recent(50).await,
            newest,
            "A pinned thread used on the desktop sorts by its newer activity"
        );
        let mut all = vec!["claude:quiet-00".to_owned()];
        all.extend(busy(60));
        all.extend((1..10).map(|i| format!("claude:quiet-{i:02}")));
        assert_eq!(recent(100).await, all);
    }

    // Contract: recent threads span every included project, newest first
    // whatever the project order, each named with its project; the limit
    // applies to the merged list and excluded projects stay out.
    #[tokio::test]
    async fn recent_threads_merge_included_projects_by_activity() {
        let (dir, state, _message) = handoff_fixture().await;
        for (id, name) in [("second", "Second"), ("hidden", "Hidden")] {
            let source = dir.path().join(id);
            std::fs::create_dir(&source).unwrap();
            let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
            state
                .store
                .create_project(id, id, "hash", name, &roots, 0, "now")
                .await
                .unwrap();
        }
        state
            .store
            .update_project_metadata("hidden", None, Some(false), None, "now")
            .await
            .unwrap();
        for (id, project, at) in [
            ("old-draft", "project", "2026-01-01T00:00:00Z"),
            ("new-draft", "second", "2026-06-01T00:00:00Z"),
            ("mid-draft", "project", "2026-03-01T00:00:00Z"),
            ("hidden-draft", "hidden", "2026-09-01T00:00:00Z"),
        ] {
            let cwd = dir.path().join(if project == "project" {
                "project-source"
            } else {
                project
            });
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: project,
                    family: AgentFamily::Claude,
                    provider_store: &state.projects.claude_store,
                    native_session_id: None,
                    cwd: cwd.to_str().unwrap(),
                    roots_revision: 1,
                    title: id,
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "read_only",
                    claude_approval: "ask",
                    plan_mode: false,
                    creation_request_id: None,
                    now: at,
                })
                .await
                .unwrap();
        }
        let recent = |limit| {
            let state = state.clone();
            async move {
                let response = recent_threads(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Query(RecentThreadsQuery { limit: Some(limit) }),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                    .await
                    .unwrap();
                let page: Value = serde_json::from_slice(&bytes).unwrap();
                crate::tests::validate_http_contract("recentThreadsPage", &page);
                page["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|t| {
                        t["thread"]["conversationId"]
                            .as_str()
                            .is_some_and(|id| id.ends_with("-draft"))
                    })
                    .map(|t| {
                        (
                            t["thread"]["conversationId"].as_str().unwrap().to_owned(),
                            t["projectName"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            }
        };
        let named = |rows: &[(&str, &str)]| {
            rows.iter()
                .map(|(id, project)| (id.to_string(), project.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            recent(50).await,
            named(&[
                ("new-draft", "Second"),
                ("mid-draft", "Test"),
                ("old-draft", "Test")
            ])
        );
        assert_eq!(recent(1).await, named(&[("new-draft", "Second")]));
    }

    // Contract: one provider-owned archive hides even pinned chats, failures
    // preserve visibility, restart/retry retain identity, and desktop changes
    // reconcile both ways without resuming archived work.
    // Contract: a Claude Code thread archives in Wonder only. It leaves the
    // pinned list and the thread list, appears under archived threads, refuses
    // sends, survives a re-read from the store (relaunch), and restores with the
    // same PATCH. Claude Code's own session is never touched.
    // Owner (Oct 12): the composer showed High while the session ran Medium,
    // so a plain send counted as a settings change. A thread that never chose
    // an effort shows the one the Mac runs, including the owner's default.
    #[tokio::test]
    async fn the_detail_shows_the_settings_a_send_runs_with() {
        let (dir, state, _message) = handoff_fixture().await;
        {
            let mut catalog = state.runtime_catalog.write().await;
            catalog.models.push(crate::agent_defaults::tests::model(
                "claude:opus",
                AgentFamily::Claude,
                &["low", "medium", "high"],
                None,
            ));
            catalog.preferences.insert(
                AgentFamily::Claude,
                wonder_store::AgentDefaultPreference {
                    model: Some("claude:opus".into()),
                    effort: Some("medium".into()),
                    service_tier: None,
                },
            );
            catalog.resolve_defaults();
        }
        let cwd = dir.path().join("project-source");
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "claude-settings",
                project_id: "project",
                family: AgentFamily::Claude,
                provider_store: &state.projects.claude_store,
                native_session_id: Some("5e55-10a2"),
                cwd: cwd.to_str().unwrap(),
                roots_revision: 1,
                title: "Claude thread",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "workspace",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        let stored = state
            .store
            .project_conversation("claude-settings")
            .await
            .unwrap()
            .unwrap();
        let detail =
            serde_json::to_value(conversation_detail(&state, &stored).await.unwrap()).unwrap();
        assert_eq!(detail["model"], "claude:opus");
        assert_eq!(
            detail["effort"], "medium",
            "the owner's default, not the phone's guess"
        );
        crate::tests::validate_http_contract("projectConversationDetail", &detail);
    }

    // API callers cannot select permissions ACP cannot enforce. Validate before
    // provider availability or any create side effect; Plan is independent.
    #[tokio::test]
    async fn opencode_rejects_unsupported_access_controls() {
        let (dir, state, _) = handoff_fixture().await;
        let cwd = dir.path().join("project-source");
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "opencode-chat",
                project_id: "project",
                family: AgentFamily::OpenCode,
                provider_store: "opencode:home",
                native_session_id: None,
                cwd: cwd.to_str().unwrap(),
                roots_revision: 1,
                title: "OpenCode thread",
                model: Some("opencode:local/fixture"),
                effort: None,
                service_tier: None,
                access_mode: "workspace",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        for fields in [
            json!({"accessMode":"read_only"}),
            json!({"accessMode":"workspace"}),
            json!({"accessMode":"full_access"}),
            json!({"claudeApproval":"ask"}),
            json!({"claudeApproval":"auto"}),
        ] {
            let (status, body) = fork_response(
                update_conversation(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path("opencode-chat".into()),
                    Json(serde_json::from_value(fields).unwrap()),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.to_string().contains("OpenCode settings"), "{body}");
        }
        for fields in [
            json!({"accessMode":"read_only"}),
            json!({"accessMode":"full_access"}),
            json!({"claudeApproval":"ask"}),
            json!({"claudeApproval":"auto"}),
        ] {
            let mut request = json!({"deviceId":"test", "clientMessageId":uuid::Uuid::new_v4().to_string(),
                "family":"opencode", "model":"opencode:opencode/free-model", "body":"", "prepareOnly":true, "accessMode":"workspace"});
            request
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let (status, body) = fork_response(
                create_thread(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    None,
                    Path("project".into()),
                    Json(serde_json::from_value(request).unwrap()),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.to_string().contains("OpenCode settings"), "{body}");
        }
        let (status, body) = fork_response(
            update_conversation(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("opencode-chat".into()),
                Json(serde_json::from_value(json!({"planMode":true})).unwrap()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["planMode"], true);
        let stored = state
            .store
            .project_conversation("opencode-chat")
            .await
            .unwrap()
            .unwrap();
        let resume = json!({"cwd":stored.cwd, "wonderPolicy":{"planMode":true}});
        let turn = json!({"wonderPolicy":{"planMode":true}});
        assert_eq!(
            validate_update_policy(&state, &stored, &resume, &turn)
                .await
                .unwrap(),
            Some(json!({"planMode":true}))
        );
        assert!(validate_update_policy(
            &state,
            &stored,
            &resume,
            &json!({"wonderPolicy":{"planMode":false}})
        )
        .await
        .is_err());
        assert!(validate_update_policy(
            &state,
            &stored,
            &resume,
            &json!({"wonderPolicy":{"planMode":true,"mode":"read_only"}})
        )
        .await
        .is_err());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: the authenticated batch is bounded before any writes, returns
    // results in request order, and keeps successful changes around failures.
    #[tokio::test]
    async fn archive_batch_bounds_orders_and_reports_independent_failures() {
        let (dir, state, _message) = handoff_fixture().await;
        let source = dir.path().join("project-source");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let other_roots = validate_folders(&[outside.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                "other-project",
                "other-request",
                "other-hash",
                "Other",
                &other_roots,
                0,
                "now",
            )
            .await
            .unwrap();
        for index in 0..52 {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: &format!("batch-{index}"),
                    project_id: if index == 50 {
                        "other-project"
                    } else {
                        "project"
                    },
                    family: AgentFamily::Claude,
                    provider_store: &state.projects.claude_store,
                    native_session_id: if index == 0 { Some("abc0") } else { None },
                    cwd: if index >= 50 {
                        outside.to_str().unwrap()
                    } else {
                        source.to_str().unwrap()
                    },
                    roots_revision: 1,
                    title: "Batch fixture",
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "read_only",
                    claude_approval: "ask",
                    plan_mode: false,
                    creation_request_id: None,
                    now: "now",
                })
                .await
                .unwrap();
        }
        let item = |index| json!({"reference": if index == 0 { "claude:abc0".to_owned() } else { format!("wonder:batch-{index}") }, "conversationId": format!("batch-{index}")});
        let path = "/api/v1/projects/project/threads/archive";
        // Other phones learn of the archive through the event their sidebar
        // follows: one per batch that archived anything, none for a refusal.
        let mut events = state.events.subscribe();
        let mut chats_changed = move || {
            let mut count = 0;
            while let Ok(event) = events.try_recv() {
                if matches!(event.event, wonder_api::WonderEvent::Activity { ref category, .. } if category == "native_status")
                {
                    count += 1;
                }
            }
            count
        };
        for items in [vec![], vec![item(0), item(0)], (0..51).map(item).collect()] {
            let (status, _) =
                crate::test_support::call(&state, "POST", path, json!({"threads": items})).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(
                !state
                    .store
                    .project_conversation("batch-0")
                    .await
                    .unwrap()
                    .unwrap()
                    .wonder_archived
            );
        }
        assert_eq!(chats_changed(), 0, "A refused batch changes no list");
        let project = state.store.project("project").await.unwrap().unwrap();
        let folders = project_folders(&state, &project).await;
        let mut labels: Vec<_> = folders.labels.iter().collect();
        labels.sort();
        let native = |id: &str| NativeThread {
            family: AgentFamily::Claude,
            store: state.projects.claude_store.clone(),
            id: id.into(),
            title: id.into(),
            source: None,
            updated_at: 1,
            is_working: false,
            is_computer_session: false,
            worktree_label: None,
            cwd: None,
        };
        state.projects.cursors.lock().unwrap().insert(
            "batch-cursor".into(),
            (
                Instant::now(),
                CatalogCursor {
                    project_id: project.id,
                    roots_revision: project.roots_revision,
                    folders_key: format!("{:?}{:?}", root_paths(&folders.project), labels),
                    codex: ProviderPage {
                        done: true,
                        ..Default::default()
                    },
                    claude: ProviderPage {
                        cursor: Some("next-native".into()),
                        done: true,
                        buffer: VecDeque::from([native("abc0"), native("beef")]),
                        ..Default::default()
                    },
                    accounts: ProviderPage {
                        done: true,
                        ..Default::default()
                    },
                    emitted: HashSet::new(),
                },
            ),
        );
        let mut items: Vec<_> = (0..47).rev().map(item).collect();
        // A running Codex thread archives like any other (archive is never
        // refused); a stale identity and a sibling folder fail independently
        // without touching their provider or conversation.
        items.insert(
            1,
            json!({"reference": "codex:thread", "conversationId": "project-chat"}),
        );
        items.insert(
            3,
            json!({"reference": "wonder:stale", "conversationId": "batch-48"}),
        );
        items.insert(5, item(50));
        crate::tests::validate_http_contract(
            "archiveProjectThreadsRequest",
            &json!({"threads": items}),
        );
        let (status, body) =
            crate::test_support::call(&state, "POST", path, json!({"threads": items})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        crate::tests::validate_http_contract("archiveProjectThreadsResponse", &body);
        assert_eq!(
            chats_changed(),
            1,
            "One Chats refresh event per archiving batch"
        );
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), MAX_ARCHIVE_BATCH);
        assert_eq!(
            results.iter().map(|r| &r["reference"]).collect::<Vec<_>>(),
            items.iter().map(|r| &r["reference"]).collect::<Vec<_>>()
        );
        for index in [3, 5] {
            assert!(results[index]["error"].is_string(), "{body}");
        }
        assert_eq!(results[1]["conversationId"], "project-chat", "{body}");
        assert_eq!(results.iter().filter(|r| r["error"].is_null()).count(), 48);
        assert!(
            state
                .store
                .project_conversation("project-chat")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
        assert!(
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap()
                .lines()
                .any(|line| line.contains("\"turn/interrupt\"") && line.contains("\"turn\"")),
            "Its running turn was asked to stop"
        );
        {
            let cursors = state.projects.cursors.lock().unwrap();
            let (_, cursor) = cursors
                .get("batch-cursor")
                .expect("Archive must preserve Show more");
            assert_eq!(cursor.claude.cursor.as_deref(), Some("next-native"));
            assert_eq!(
                cursor
                    .claude
                    .buffer
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>(),
                ["beef"]
            );
            assert!(cursor.emitted.contains("claude:abc0"));
        }
        let (status, more) = crate::test_support::call(
            &state,
            "GET",
            "/api/v1/projects/project/threads?cursor=batch-cursor&limit=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{more}");
        assert_eq!(more["threads"][0]["reference"], "claude:beef");
        for index in 0..51 {
            assert_eq!(
                state
                    .store
                    .project_conversation(&format!("batch-{index}"))
                    .await
                    .unwrap()
                    .unwrap()
                    .wonder_archived,
                index < 47
            );
        }
        // Retrying confirmed successes is idempotent; repairing one failed
        // identity can archive it without disturbing the other failed rows.
        let (_, retried) = crate::test_support::call(
            &state,
            "POST",
            path,
            json!({"threads": [item(46), item(48)]}),
        )
        .await;
        assert!(
            retried["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["error"].is_null()),
            "{retried}"
        );
        let restored = state
            .store
            .project_conversation("batch-48")
            .await
            .unwrap()
            .unwrap();
        set_archived(&state, &restored, false).await.unwrap();
        assert!(
            !state
                .store
                .project_conversation("batch-48")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
        // A blocked first item consumes the whole-operation budget. Dropping
        // it and releasing the lock later must not execute any expired writes.
        let provider_reads =
            std::fs::read_to_string(dir.path().join("requests")).unwrap_or_default();
        // A short real budget, not a paused clock: auto-advance fired the
        // store's own timeouts while SQLite worked, which made this flaky.
        let dispatch = state.dispatch_lock.lock().await;
        let project = state.store.project("project").await.unwrap().unwrap();
        let expired = archive_batch(
            &state,
            &project,
            serde_json::from_value(json!([item(49), {"reference": "codex:beef"}])).unwrap(),
            tokio::time::Instant::now() + Duration::from_millis(300),
        )
        .await;
        assert_eq!(expired.len(), 2);
        assert!(
            expired.iter().all(|r| r
                .error
                .as_deref()
                .is_some_and(|error| error.contains("in time"))),
            "{expired:?}"
        );
        drop(dispatch);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("requests")).unwrap_or_default(),
            provider_reads,
            "Later items must not even attach before the earlier item settles"
        );
        for index in [47, 49] {
            assert!(
                !state
                    .store
                    .project_conversation(&format!("batch-{index}"))
                    .await
                    .unwrap()
                    .unwrap()
                    .wonder_archived
            );
        }
        // A saved conversation still belongs to its Project after its old
        // folder is removed; the list continues to show it, so archive works.
        let (status, saved) =
            crate::test_support::call(&state, "POST", path, json!({"threads": [item(51)]})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["results"][0]["conversationId"], "batch-51");
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // A send still being submitted when its thread is archived keeps its
    // receipt. Archive answers at once; once the send resolves, its turn is
    // asked to stop and whatever it left (accepted or uncertain) is settled,
    // unless the thread was restored first.
    #[tokio::test]
    async fn archive_during_a_submission_settles_it_once_it_resolves() {
        for outcome in ["accepted", "lost", "restored"] {
            let (dir, state, sending) = dispatch_failure_fixture().await;
            let id = sending.conversation_id.clone();
            crate::phone_approval_tests::hold_provider_send(dir.path());
            if outcome == "lost" {
                // The provider dies before answering: the send is uncertain.
                let script = dir.path().join("runtime.py");
                let source = std::fs::read_to_string(&script).unwrap().replacen(
                    "            while not os.path.exists(root + '/release-send'): time.sleep(0.01)\n",
                    "            while not os.path.exists(root + '/release-send'): time.sleep(0.01)\n            os._exit(1)\n",
                    1,
                );
                assert!(source.contains("os._exit(1)"));
                std::fs::write(&script, source).unwrap();
            }
            let send = tokio::spawn(dispatch(state.clone(), sending.clone()));
            tokio::time::timeout(Duration::from_secs(10), async {
                while !dir.path().join("send-waiting").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the provider holds the send");
            let patch = |archived: bool| {
                tokio::time::timeout(
                    Duration::from_secs(60),
                    update_conversation(
                        State(state.clone()),
                        Extension(OwnerAuthority),
                        Path(id.clone()),
                        Json(serde_json::from_value(json!({"isArchived": archived})).unwrap()),
                    ),
                )
            };
            let archived = patch(true)
                .await
                .expect("archive does not wait for the provider");
            assert_eq!(archived.status(), StatusCode::OK, "{outcome}");
            let stored = || async {
                state
                    .store
                    .project_conversation(&id)
                    .await
                    .unwrap()
                    .unwrap()
            };
            assert!(stored().await.wonder_archived, "{outcome}");
            assert_eq!(
                state
                    .store
                    .message_by_id(&sending.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "dispatching_to_codex",
                "{outcome}: the send keeps its receipt"
            );
            if outcome == "restored" {
                let restored = patch(false).await.expect("restore does not wait either");
                assert_eq!(restored.status(), StatusCode::OK);
                assert!(!stored().await.wonder_archived);
            }
            std::fs::write(dir.path().join("release-send"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(10), send)
                .await
                .unwrap()
                .unwrap();
            let interrupts = || {
                std::fs::read_to_string(dir.path().join("requests-jsonl"))
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|r| r["method"] == "turn/interrupt")
                    .count()
            };
            let message = || async {
                state
                    .store
                    .message_by_id(&sending.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state
            };
            if outcome == "restored" {
                // The follow-up finds the thread restored and leaves it alone.
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert_eq!(message().await, "accepted_by_codex");
                assert_eq!(interrupts(), 0);
                assert!(!stored().await.wonder_archived);
            } else {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while message().await != "interrupted" {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{outcome}: left {}", "unsettled"));
                assert!(!state
                    .store
                    .conversation_has_archive_blocking_work(&id)
                    .await
                    .unwrap());
                assert_eq!(
                    interrupts(),
                    usize::from(outcome == "accepted"),
                    "{outcome}: only a started turn is asked to stop"
                );
                assert!(stored().await.wonder_archived);
            }
            state.projects.shutdown().await;
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    // A send claimed but still preparing (before it takes the chat's lock)
    // when its thread is archived ends in exactly one terminal state: the
    // archive's follow-up leaves the claim to its dispatch, then settles.
    #[tokio::test]
    async fn archive_leaves_a_send_preparing_to_its_dispatch() {
        let (_dir, state, sending) = dispatch_failure_fixture().await;
        let id = sending.conversation_id.clone();
        PREPARE_HOLD
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(sending.id.clone());
        let send = tokio::spawn(dispatch(state.clone(), sending.clone()));
        let message = || async {
            state
                .store
                .message_by_id(&sending.id)
                .await
                .unwrap()
                .unwrap()
                .state
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while !state.projects.dispatching().contains(&sending.id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the send is claimed and preparing");
        assert_eq!(message().await, "dispatching_to_codex");
        let archived = tokio::time::timeout(
            Duration::from_secs(60),
            update_conversation(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(id.clone()),
                Json(serde_json::from_value(json!({"isArchived": true})).unwrap()),
            ),
        )
        .await
        .expect("archive does not wait for the preparing send");
        assert_eq!(archived.status(), StatusCode::OK);
        // The follow-up runs now; it must leave the claimed send alone.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(message().await, "dispatching_to_codex");
        PREPARE_HOLD
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .remove(&sending.id);
        tokio::time::timeout(Duration::from_secs(10), send)
            .await
            .unwrap()
            .unwrap();
        // Preparing found the thread archived: the send was never submitted.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(message().await, "safe_to_retry");
        assert!(!state
            .store
            .conversation_has_archive_blocking_work(&id)
            .await
            .unwrap());
        let states = state
            .store
            .events_for_conversation(&id)
            .await
            .unwrap()
            .into_iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .filter(|event| event.to_string().contains(&sending.id))
            .collect::<Vec<_>>();
        assert!(
            !states
                .iter()
                .any(|event| event.to_string().contains("\"interrupted\"")),
            "one terminal state only: {states:?}"
        );
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // A claim that lands after archive starts but before it settles (both
    // wait on the shared lock, the claim first) is still left to its
    // dispatch: archive reads claims under the lock it settles under.
    #[tokio::test]
    async fn archive_reads_claims_under_the_lock_it_settles_under() {
        let (_dir, state, sending) = dispatch_failure_fixture().await;
        let id = sending.conversation_id.clone();
        PREPARE_HOLD
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(sending.id.clone());
        let held = state.dispatch_lock.lock().await;
        // Queue the claim on the shared lock, then the archive behind it.
        // Neither has anything slow to do before the lock.
        let send = tokio::spawn(dispatch(state.clone(), sending.clone()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let archive = tokio::spawn(update_conversation(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path(id.clone()),
            Json(serde_json::from_value(json!({"isArchived": true})).unwrap()),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!archive.is_finished() && !send.is_finished());
        assert!(state.projects.dispatching().is_empty(), "not claimed yet");
        drop(held);
        let archived = tokio::time::timeout(Duration::from_secs(60), archive)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.status(), StatusCode::OK);
        assert!(state.projects.dispatching().contains(&sending.id));
        let message = || async {
            state
                .store
                .message_by_id(&sending.id)
                .await
                .unwrap()
                .unwrap()
                .state
        };
        assert_eq!(message().await, "dispatching_to_codex");
        PREPARE_HOLD
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .remove(&sending.id);
        tokio::time::timeout(Duration::from_secs(10), send)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(message().await, "safe_to_retry");
        let events = state
            .store
            .events_for_conversation(&id)
            .await
            .unwrap()
            .into_iter()
            .map(|event| serde_json::to_value(event).unwrap().to_string())
            .filter(|event| event.contains(&sending.id))
            .collect::<Vec<_>>();
        assert!(
            !events.iter().any(|event| event.contains("\"interrupted\"")),
            "one terminal state only: {events:?}"
        );
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn claude_archive_is_wonder_local_listed_and_restorable() {
        // Reads that must complete get a hang guard instead of the page's 250 ms.
        CLAUDE_READ_BUDGET.with(|budget| budget.set(Some(Duration::from_secs(60))));
        let (dir, mut state, _message) = handoff_fixture().await;
        let cwd = dir.path().join("project-source");
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "claude-chat",
                project_id: "project",
                family: AgentFamily::Claude,
                provider_store: &state.projects.claude_store,
                native_session_id: Some("5e55-10a1"),
                cwd: cwd.to_str().unwrap(),
                roots_revision: 1,
                title: "Claude thread",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        state
            .store
            .update_project_conversation(
                "claude-chat",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        // A native process can work after its prompt receipt completed.
        // The same native level must reach activity reads and block archive.
        let entrypoint = dir.path().join("claude-state.py");
        std::fs::write(&entrypoint, r#"import json, sys, os, time
root = os.path.dirname(__file__)
with open(root + "/claude-starts", "a") as log: log.write("start\n")
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    with open(root + '/claude-requests.jsonl', 'a') as log: log.write(json.dumps(r) + '\n')
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'thread/list':
        while os.path.exists(root + '/slow-batch') and '5e55-10a1' not in p.get('sessionIds', []): time.sleep(0.05)
        result = {'data': [
            {'id': 'claude-5e55-10a1', 'sessionId': '5e55-10a1', 'status': json.load(open(root + '/claude-status.json'))},
            {'id': 'foreign', 'sessionId': 'unrequested', 'status': {'type': 'active'}},
            {'id': 'helper', 'sessionId': '5e55-10a1', 'parentThreadId': 'parent', 'status': {'type': 'active'}}], 'nextCursor': None}
        if os.path.exists(root + '/claude-list-page.json'): result = json.load(open(root + '/claude-list-page.json'))
        if os.path.exists(root + '/claude-list-error'):
            print(json.dumps({'id': r['id'], 'error': {'code': -32603, 'message': 'Synthetic list failure'}}), flush=True)
            continue
    elif m == 'thread/read': result = {'thread': {'id': p['threadId'], 'status': json.load(open(root + '/claude-status.json'))}}; result.update({'contextUsage': json.load(open(root + '/claude-context.json'))} if p.get('includeContextUsage') else {})
    elif m == 'thread/turns/list': result = {'data': [{'id': 'answered', 'status': 'completed', 'itemCount': 2}], 'nextCursor': None}
    elif m in ['thread/backgroundTasks/list', 'project/sessions/list']: result = {'data': [], 'nextCursor': None, 'partial': False}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#).unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
        state
            .store
            .bind_project_runtime(
                "claude-chat",
                AgentFamily::Claude,
                &state.projects.claude_store,
                "claude-5e55-10a1",
                Some("5e55-10a1"),
                "now",
            )
            .await
            .unwrap();
        let native_state = |status: Value| {
            std::fs::write(dir.path().join("claude-status.json"), status.to_string()).unwrap()
        };
        native_state(json!({"type":"active", "activeFlags":["waitingOnUserInput"]}));
        std::fs::write(
            dir.path().join("claude-context.json"),
            json!({
                "observedAtMs": 1_000, "tokenUsage": {"last": {"totalTokens": 1234},
                "modelContextWindow": 200_000, "model": "claude-native"}
            })
            .to_string(),
        )
        .unwrap();
        let patch = |archived: bool| {
            update_conversation(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("claude-chat".into()),
                Json(serde_json::from_value(json!({"isArchived": archived})).unwrap()),
            )
        };
        let body = |response: Response| async move {
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };
        let pinned_ids = |library: Value| {
            library["pinned"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    p["thread"]["conversationId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        let library = body(list(State(state.clone()), Extension(OwnerAuthority)).await).await;
        assert_eq!(library["archiveVersion"], 3);
        crate::tests::validate_http_contract("projectsResponse", &library);
        assert_eq!(library["capabilitiesVersion"], 1);
        for family in library["families"].as_array().unwrap() {
            let name: AgentFamily = serde_json::from_value(family["family"].clone()).unwrap();
            assert_eq!(
                family["capabilities"],
                json!(FamilyCapabilities::of(name)),
                "{name:?}"
            );
        }
        assert_eq!(
            library["pinned"][0]["thread"]["isWorking"], true,
            "The native process remains active after the saved prompt completed"
        );
        assert!(pinned_ids(library).contains(&"claude-chat".to_owned()));
        let working_page = body(
            threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
                Query(ThreadsQuery {
                    cursor: None,
                    limit: None,
                }),
            )
            .await,
        )
        .await;
        assert!(working_page["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["conversationId"] == "claude-chat" && row["isWorking"] == true));
        let native_lists = std::fs::read_to_string(dir.path().join("claude-requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["method"] == "thread/list")
            .collect::<Vec<_>>();
        assert_eq!(
            native_lists.len(),
            2,
            "One batch per catalog, never one read per row"
        );
        assert!(native_lists
            .iter()
            .all(|row| row["params"]["sessionIds"] == json!(["5e55-10a1"])));
        // Contract: this lookup measures activity freshness, not catalog
        // coverage. Settled or absent (including archived) IDs are complete;
        // unknown process state, malformed paging, and RPC failure are not.
        let list_page = dir.path().join("claude-list-page.json");
        for (case, page, expected) in [
            (
                "settled",
                json!({"data": [{"sessionId": "5e55-10a1", "status": {"type": "idle"}}], "nextCursor": null}),
                true,
            ),
            (
                "absent archived sessions",
                json!({"data": [], "nextCursor": null}),
                true,
            ),
            (
                "unknown retained process",
                json!({"data": [{"sessionId": "5e55-10a1", "status": {"type": "notLoaded"}}], "nextCursor": null}),
                false,
            ),
            (
                "continued page",
                json!({"data": [], "nextCursor": "more"}),
                false,
            ),
            (
                "missing session identity",
                json!({"data": [{"status": {"type": "idle"}}], "nextCursor": null}),
                false,
            ),
            ("missing cursor", json!({"data": []}), false),
        ] {
            std::fs::write(&list_page, page.to_string()).unwrap();
            // A large visible set of settled/archived sessions alone must not
            // make this partial; the library's pinned set is smaller still.
            let mut ids = (0..900)
                .map(|i| format!("archived-{i}"))
                .collect::<HashSet<_>>();
            ids.insert("5e55-10a1".into());
            let (sessions, complete) = claude_working_sessions(&state, ids).await;
            assert_eq!(complete, expected, "{case}");
            assert!(!sessions.busy.contains("5e55-10a1"), "{case}");
            let library = body(list(State(state.clone()), Extension(OwnerAuthority)).await).await;
            assert_eq!(
                library["partial"].as_array().unwrap().is_empty(),
                expected,
                "{case}"
            );
            assert!(
                pinned_ids(library).contains(&"claude-chat".to_owned()),
                "{case}: activity freshness cannot remove chats"
            );
        }
        std::fs::remove_file(list_page).unwrap();
        let list_error = dir.path().join("claude-list-error");
        std::fs::write(&list_error, "").unwrap();
        let (_, complete) =
            claude_working_sessions(&state, HashSet::from(["5e55-10a1".into()])).await;
        assert!(!complete, "An RPC failure leaves working status incomplete");
        std::fs::remove_file(list_error).unwrap();
        native_state(json!({"type":"notLoaded"}));
        let (unknown, complete) =
            claude_working_sessions(&state, HashSet::from(["5e55-10a1".into()])).await;
        assert!(
            !unknown.busy.contains("5e55-10a1"),
            "Unknown main state cannot borrow a helper's active state"
        );
        assert!(!unknown.busy.contains("unrequested"));
        assert!(!complete);
        let unknown_library =
            body(list(State(state.clone()), Extension(OwnerAuthority)).await).await;
        assert_eq!(unknown_library["partial"][0]["family"], "claude");
        // More than one page still preserves native state; over-budget sets
        // explicitly report partial rather than dropping every active result.
        for count in [201, 1601] {
            let mut ids = (0..count)
                .map(|i| format!("extra-{i}"))
                .collect::<HashSet<_>>();
            ids.insert("5e55-10a1".into());
            native_state(json!({"type":"active"}));
            let (busy, complete) = claude_working_sessions(&state, ids).await;
            assert!(busy.busy.contains("5e55-10a1"));
            assert_eq!(complete, count == 201);
        }
        // The later batch is held until the budget ends; the first one has
        // that long to answer.
        std::fs::write(dir.path().join("slow-batch"), "").unwrap();
        let mut ids = (0..201)
            .map(|i| format!("extra-{i}"))
            .collect::<HashSet<_>>();
        ids.insert("5e55-10a1".into());
        CLAUDE_READ_BUDGET.with(|budget| budget.set(Some(Duration::from_secs(10))));
        let (busy, complete) = claude_working_sessions(&state, ids).await;
        CLAUDE_READ_BUDGET.with(|budget| budget.set(Some(Duration::from_secs(60))));
        std::fs::remove_file(dir.path().join("slow-batch")).unwrap();
        assert!(
            busy.busy.contains("5e55-10a1"),
            "A later batch timeout cannot erase a confirmed active session"
        );
        assert!(!complete);
        native_state(json!({"type":"active", "activeFlags":["waitingOnUserInput"]}));

        let activity = body(
            crate::history::history_activity(State(state.clone()), Path("claude-chat".into()))
                .await,
        )
        .await;
        crate::tests::validate_http_contract("historyActivity", &activity);
        assert_eq!(activity["latestTurnStatus"], "completed");
        assert_eq!(
            activity["contextUsage"],
            json!({"usedTokens": 1234, "windowTokens": 200_000, "atMs": 1000})
        );
        assert_eq!(
            activity["threadStatus"],
            json!({"type":"active", "activeFlags":["waitingOnUserInput"]})
        );
        native_state(json!({"type":"idle"}));
        let idle_list = body(list(State(state.clone()), Extension(OwnerAuthority)).await).await;
        assert_eq!(idle_list["pinned"][0]["thread"]["isWorking"], false);
        // The page's own 250 ms budget, with the runtime lock held by the test:
        // a deadline that excluded the lock would wait as long as it is held.
        CLAUDE_READ_BUDGET.with(|budget| budget.set(None));
        let client_lock = state.claude.as_ref().unwrap().client.lock().await;
        let started = Instant::now();
        let (blocked, complete) =
            claude_working_sessions(&state, HashSet::from(["5e55-10a1".into()])).await;
        assert!(!blocked.busy.contains("5e55-10a1"));
        assert!(!complete);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "Batch deadline includes acquiring the runtime lock"
        );
        drop(client_lock);
        CLAUDE_READ_BUDGET.with(|budget| budget.set(Some(Duration::from_secs(60))));
        std::fs::write(dir.path().join("claude-context.json"), "null").unwrap();
        let idle = body(
            crate::history::history_activity(State(state.clone()), Path("claude-chat".into()))
                .await,
        )
        .await;
        assert_eq!(idle["threadStatus"]["type"], "idle");
        assert_eq!(idle["latestTurnStatus"], "completed");
        assert_eq!(
            idle["contextUsage"],
            json!({"usedTokens": 1234, "windowTokens": 200_000, "atMs": 1000}),
            "a failed native read keeps the last real reading (the phone dims it with its age)"
        );
        // Archiving while Claude is still replying asks it to stop and still
        // archives (archive is never refused).
        native_state(json!({"type":"active"}));
        let archived = body(patch(true).await).await;
        assert_eq!(archived["isArchived"], true);
        crate::tests::validate_http_contract("projectConversationDetail", &archived);
        assert!(
            std::fs::read_to_string(dir.path().join("claude-requests.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .any(|r| r["method"] == "turn/interrupt"
                    && r["params"]
                        == json!({"threadId": "claude-5e55-10a1", "turnId": "answered"})),
            "A running Claude reply is asked to stop"
        );
        native_state(json!({"type":"idle"}));
        // Relaunch: the flag comes back from the store, not from memory.
        let stored = state
            .store
            .project_conversation("claude-chat")
            .await
            .unwrap()
            .unwrap();
        assert!(stored.wonder_archived);
        assert!(
            conversation_detail(&state, &stored)
                .await
                .unwrap()
                .is_archived
        );
        let library = body(list(State(state.clone()), Extension(OwnerAuthority)).await).await;
        assert!(!pinned_ids(library).contains(&"claude-chat".to_owned()));
        let page = body(
            threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
                Query(ThreadsQuery {
                    cursor: None,
                    limit: None,
                }),
            )
            .await,
        )
        .await;
        assert!(!page["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["conversationId"] == "claude-chat"));
        // The list reads Codex's thread index: a scan of every saved rollout took
        // 13-24 s and outlasted the phone's request ("Threads could not be loaded").
        let codex_lists = std::fs::read_to_string(dir.path().join("requests-jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|r| r["method"] == "thread/list" && r["params"]["cwd"].is_array())
            .filter(|r| r["params"]["archived"] == false)
            .map(|r| r["params"]["useStateDbOnly"].clone())
            .collect::<Vec<_>>();
        assert!(!codex_lists.is_empty());
        assert!(codex_lists.iter().all(|flag| flag == &json!(true)));
        let listed = body(
            archived_threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
            )
            .await,
        )
        .await;
        crate::tests::validate_http_contract("archivedProjectThreadsPage", &listed);
        let row = listed["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["conversationId"] == "claude-chat")
            .cloned()
            .expect("archived Claude thread listed");
        assert_eq!(row["reference"], "claude:5e55-10a1");
        assert_eq!(row["family"], "claude");

        let restored = body(patch(false).await).await;
        assert_eq!(restored["isArchived"], false);
        assert!(
            !state
                .store
                .project_conversation("claude-chat")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
        let library = body(list(State(state.clone()), Extension(OwnerAuthority)).await).await;
        assert!(pinned_ids(library).contains(&"claude-chat".to_owned()));
        let listed = body(
            archived_threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
            )
            .await,
        )
        .await;
        assert!(!listed["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["conversationId"] == "claude-chat"));
        let client = state.claude.as_ref().unwrap().client.clone();
        client.lock().await.shutdown().await.unwrap();
        assert!(
            live_rpc_for(&state, AgentFamily::Claude).await.is_none(),
            "Read-only activity admission must not restart a stopped runtime"
        );
        assert!(!client.lock().await.health().is_alive());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("claude-starts"))
                .unwrap()
                .lines()
                .count(),
            1,
            "A failed discovery after spawning is still an unwanted restart"
        );
    }

    #[tokio::test]
    async fn archive_reads_share_a_cache_until_the_provider_store_changes() {
        let (dir, state, _) = handoff_fixture().await;
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::create_dir_all(&state.codex_home).unwrap();
        let database = state.codex_home.join("state_5.sqlite");
        std::fs::write(&database, "synthetic provider generation").unwrap();
        let calls = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("thread/list") && line.contains("archived"))
                .count()
        };
        let before = calls();
        for _ in 0..5 {
            assert!(
                archived_codex_ids(&state, std::slice::from_ref(&conversation))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            calls() - before,
            1,
            "five same-generation reads use one provider catalog"
        );
        std::fs::write(dir.path().join("archived-thread"), "").unwrap();
        // A WAL appearing invalidates even while the main database is unchanged.
        std::fs::write(
            state.codex_home.join("state_5.sqlite-wal"),
            "archive commit",
        )
        .unwrap();
        assert!(
            archived_codex_ids(&state, std::slice::from_ref(&conversation))
                .await
                .unwrap()
                .contains("thread")
        );
        assert_eq!(calls() - before, 2);
        std::fs::remove_file(dir.path().join("archived-thread")).unwrap();
        std::fs::remove_file(state.codex_home.join("state_5.sqlite-wal")).unwrap();
        std::fs::write(&database, "checkpoint and external restore").unwrap();
        assert!(
            archived_codex_ids(&state, std::slice::from_ref(&conversation))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls() - before, 3);

        // With Codex's real state database, a check reads just the thread's
        // row: no provider catalog, however often Codex writes, and every
        // answer is current (no snapshot for a send to trust).
        std::fs::remove_file(&database).unwrap();
        let writer = |path: &std::path::Path| {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
            async move {
                <sqlx::SqliteConnection as sqlx::Connection>::connect_with(&options)
                    .await
                    .unwrap()
            }
        };
        let mut codex = writer(&database).await;
        sqlx::query(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(&mut codex)
        .await
        .unwrap();
        sqlx::query("INSERT INTO threads(id, cwd) VALUES ('thread', ?)")
            .bind(&conversation.cwd)
            .execute(&mut codex)
            .await
            .unwrap();
        // The provider's catalog would now say archived; the database wins.
        std::fs::write(dir.path().join("archived-thread"), "").unwrap();
        let before = calls();
        for write in 0..5 {
            sqlx::query("INSERT INTO threads(id, cwd, archived) VALUES (?, '/elsewhere', 1)")
                .bind(format!("unrelated-{write}"))
                .execute(&mut codex)
                .await
                .unwrap();
            assert_eq!(
                archive_state::archive_state(&state, &conversation).await,
                ArchiveState::Active
            );
        }
        assert_eq!(calls(), before, "the database answered every check");
        sqlx::query("UPDATE threads SET archived=1 WHERE id='thread'")
            .execute(&mut codex)
            .await
            .unwrap();
        assert_eq!(codex_is_archived(&state, &conversation).await, Ok(true));
        sqlx::query("UPDATE threads SET archived=0 WHERE id='thread'")
            .execute(&mut codex)
            .await
            .unwrap();
        assert_eq!(codex_is_archived(&state, &conversation).await, Ok(false));
        assert_eq!(calls(), before, "an archive made in Codex is read at once");
        // A thread the database has no row for yet asks the provider about
        // its own folder only.
        let mut unindexed = conversation.clone();
        unindexed.native_session_id = Some("unindexed".into());
        let both = archive_state::archive_states(&state, &[conversation.clone(), unindexed]).await;
        assert_eq!(both["thread"], ArchiveState::Active);
        assert_eq!(both["unindexed"], ArchiveState::Active);
        assert_eq!(calls() - before, 1);
        // A newer database Cupola cannot read falls back the same way.
        let mut newer = writer(&state.codex_home.join("state_6.sqlite")).await;
        sqlx::query("CREATE TABLE threads (id TEXT PRIMARY KEY)")
            .execute(&mut newer)
            .await
            .unwrap();
        assert_eq!(codex_is_archived(&state, &conversation).await, Ok(true));
        assert_eq!(calls() - before, 2);
        state.projects.shutdown().await;
    }

    /// Retained legacy account history stays visible and opens read-only;
    /// no send, fork, or continuation can bind it to the computer's sign-in.
    #[tokio::test]
    async fn legacy_account_history_opens_read_only_without_rebinding() {
        let (dir, state, _) = handoff_fixture().await;
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        let healthy = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "gone-chat",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: "codex:/a-history-cupola-no-longer-serves",
                native_session_id: Some("gone-thread"),
                cwd: &healthy.cwd,
                roots_revision: 1,
                title: "Gone",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        for id in ["project-chat", "gone-chat"] {
            state
                .store
                .update_project_conversation(
                    id,
                    ProjectConversationPatch {
                        pinned: Some(true),
                        ..Default::default()
                    },
                    "now",
                )
                .await
                .unwrap();
        }
        let gone = state
            .store
            .project_conversation("gone-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            archive_state::archive_state(&state, &gone).await,
            ArchiveState::Unknown
        );
        // The healthy chat has been checked before (by its last send).
        assert_eq!(
            archive_state::archive_state(&state, &healthy).await,
            ArchiveState::Active
        );

        let library = list(State(state.clone()), Extension(OwnerAuthority)).await;
        assert_eq!(library.status(), StatusCode::OK);
        let library: Value = serde_json::from_slice(
            &axum::body::to_bytes(library.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        let pinned = library["pinned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["thread"]["conversationId"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(pinned.contains(&"project-chat".to_owned()), "{library}");
        assert!(pinned.contains(&"gone-chat".to_owned()), "{library}");
        assert_eq!(
            library["partial"],
            json!([]),
            "Legacy history does not make current history unavailable"
        );

        let saved = update_conversation(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path("gone-chat".into()),
            Json(serde_json::from_value(json!({"title": "Renamed"})).unwrap()),
        )
        .await;
        assert_eq!(saved.status(), StatusCode::OK);
        let saved: Value = serde_json::from_slice(
            &axum::body::to_bytes(saved.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        crate::tests::validate_http_contract("projectConversationDetail", &saved);
        assert_eq!(saved["title"], "Renamed");
        assert_eq!(
            saved["readOnlyReason"],
            crate::providers::other_history(AgentFamily::Codex)
        );
        assert!(saved["notice"].is_null());
        let original_store = gone.provider_store.clone();
        let home = state.data_dir.join("provider-accounts/codex/legacy");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("history.jsonl"), "untouched").unwrap();
        let kept = state
            .store
            .insert_message(
                "owner",
                "kept-client",
                "Saved legacy text",
                "hash",
                "gone-chat",
                "2026-10-27T12:00:00Z",
            )
            .await
            .unwrap();
        let kept = match kept {
            wonder_store::MessageInsert::Inserted(message) => message,
            _ => panic!("fixture message exists"),
        };
        state
            .store
            .update_message_delivery(&kept.id, "completed", None, None)
            .await
            .unwrap();
        let (status, history) = crate::test_support::call(
            &state,
            "GET",
            "/api/v1/conversations/gone-chat/history",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "Saved history remains readable");
        assert!(
            history.to_string().contains("Saved legacy text"),
            "{history}"
        );

        // Exercise real HTTP admission with the fixture's durable projector
        // ready, so a generic reconnect gate cannot mask the legacy refusal.
        let service = crate::ingestion::spawn(state.clone()).await;
        crate::ingestion::tests::ready(&state).await;
        let (status, _) = crate::test_support::call(&state, "POST", "/api/v1/conversations/gone-chat/messages",
            json!({"deviceId":"owner","clientMessageId":uuid::Uuid::new_v4().to_string(),"body":"Must never be sent"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, refused) = crate::test_support::call(&state, "POST", "/api/v1/conversations/gone-chat/turns/old-turn/steer",
            json!({"deviceId":"owner","clientMessageId":uuid::Uuid::new_v4().to_string(),"body":"Must never be steered","expectedTurnId":"old-turn"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            refused["error"],
            crate::providers::other_history(AgentFamily::Codex)
        );
        assert!(
            matches!(crate::provider_switch::apply_transition(&state, gone.clone(), &kept).await,
            Err(detail) if detail == crate::providers::other_history(AgentFamily::Codex))
        );
        let messages = state
            .store
            .messages_for_conversation("gone-chat")
            .await
            .unwrap();
        assert_eq!(messages.len(), 1, "The rejected send inserts no content");
        assert_eq!(messages[0].body, "Saved legacy text");
        let response = continuation(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path("gone-chat".into()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            crate::providers::rpc_for_store(&state, gone.family, &gone.provider_store)
                .await
                .is_err()
        );
        assert!(
            crate::providers::live_rpc_for_store(&state, gone.family, &gone.provider_store)
                .await
                .is_none()
        );
        assert_eq!(
            state
                .store
                .project_conversation("gone-chat")
                .await
                .unwrap()
                .unwrap()
                .provider_store,
            original_store
        );
        assert_eq!(
            std::fs::read_to_string(home.join("history.jsonl")).unwrap(),
            "untouched"
        );

        // Admission fails closed, in words a person can act on.
        assert_eq!(
            codex_is_archived(&state, &gone).await,
            Err(crate::providers::other_history(AgentFamily::Codex))
        );
        assert_eq!(
            is_archived(&state, &gone).await,
            Err(crate::providers::other_history(AgentFamily::Codex))
        );
        drop(service);
        state.projects.shutdown().await;
    }

    /// When Codex's database cannot answer, a chat open or the Projects list
    /// shows the last answer at once and asks the provider in the background,
    /// once per folder selection; an action still waits for a fresh answer.
    #[tokio::test]
    async fn lists_and_chat_opens_never_wait_for_the_archived_history() {
        let (dir, state, _) = handoff_fixture().await;
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        let calls = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("thread/list") && line.contains("archived"))
                .count()
        };
        // Archived in the Codex app, with the provider's list stalled.
        std::fs::write(dir.path().join("archived-thread"), "").unwrap();
        let delay = dir.path().join("delay-archive-list");
        std::fs::write(&delay, "").unwrap();
        let before = calls();
        let opened = tokio::time::timeout(
            Duration::from_secs(10),
            conversation_detail(&state, &conversation),
        )
        .await
        .expect("the chat opened without waiting for the archived history")
        .unwrap();
        assert!(!opened.is_archived);
        assert_eq!(opened.notice.as_deref(), Some(ARCHIVE_UNCHECKED_NOTICE));
        let library = tokio::time::timeout(
            Duration::from_secs(10),
            list(State(state.clone()), Extension(OwnerAuthority)),
        )
        .await
        .expect("the Projects list did not wait for the archived history");
        assert_eq!(library.status(), StatusCode::OK);
        let library: Value = serde_json::from_slice(
            &axum::body::to_bytes(library.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            library["pinned"].as_array().unwrap().is_empty(),
            "{library}"
        );
        while calls() == before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(calls() - before, 1, "one background read per selection");

        std::fs::remove_file(&delay).unwrap();
        let shown = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let detail = conversation_detail(&state, &conversation).await.unwrap();
                if detail.is_archived {
                    break detail;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the background read reached the chat");
        assert_eq!(shown.notice, None);
        // An action never takes the displayed answer: it reads Codex again.
        std::fs::remove_file(dir.path().join("archived-thread")).unwrap();
        assert_eq!(codex_is_archived(&state, &conversation).await, Ok(false));
        state.projects.shutdown().await;
    }

    /// A slow provider read for one folder selection holds neither the cache
    /// nor other selections, and concurrent reads of it share one request.
    #[tokio::test]
    async fn a_slow_archive_read_does_not_hold_other_folders() {
        let (dir, state, _) = handoff_fixture().await;
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        let mut elsewhere = conversation.clone();
        elsewhere.cwd = dir.path().join("elsewhere").to_string_lossy().into_owned();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::create_dir_all(&state.codex_home).unwrap();
        std::fs::write(state.codex_home.join("state_5.sqlite"), "generation").unwrap();
        let calls = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("thread/list") && line.contains("archived"))
                .count()
        };
        // Warm the other selection, then stall the provider.
        archived_codex_ids(&state, std::slice::from_ref(&elsewhere))
            .await
            .unwrap();
        let before = calls();
        let delay = dir.path().join("delay-archive-list");
        std::fs::write(&delay, "").unwrap();
        let slow: Vec<_> = (0..3)
            .map(|_| {
                let (state, conversation) = (state.clone(), conversation.clone());
                tokio::spawn(async move {
                    archived_codex_ids(&state, std::slice::from_ref(&conversation)).await
                })
            })
            .collect();
        while calls() == before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let cached = tokio::time::timeout(
            Duration::from_secs(10),
            archived_codex_ids(&state, std::slice::from_ref(&elsewhere)),
        )
        .await
        .expect("cached read finished");
        assert!(cached.unwrap().is_empty());
        assert!(
            slow.iter().all(|read| !read.is_finished()),
            "the cached selection waited for the slow provider read"
        );
        std::fs::remove_file(&delay).unwrap();
        for read in slow {
            assert!(read.await.unwrap().unwrap().is_empty());
        }
        assert_eq!(calls() - before, 1, "concurrent reads share one request");
        state.projects.shutdown().await;
    }

    // Contract (owner, Oct 23): a thread T3 Code settled, snoozed or archived
    // leaves Chats and is listed under Archived saying where. Restore reopens
    // it in T3 Code (its own tool) and shows it in Cupola at once; a later
    // closing in T3 Code hides it again, a snooze only until it ends. Pinned
    // in Cupola, or used in Cupola after the closing, it stays in Chats.
    #[tokio::test]
    async fn threads_closed_in_t3_code_leave_chats_until_reopened() {
        let (dir, mut state, _message) = handoff_fixture().await;
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread": {
                "id": "thread", "cwd": conversation.cwd, "name": "Recovery", "updatedAt": 1,
                "status": {"type": "idle"}, "parentThreadId": null,
            }})
            .to_string(),
        )
        .unwrap();
        let fake = crate::t3_code::tests::Fake::start().await;
        let (t3, _grants) = crate::t3_code::tests::connected(&fake).await;
        state.t3_code = t3.clone();
        fake.keeps("t3-a", "codex", "thread").await;
        let key = reference(AgentFamily::Codex, "thread");
        // T3 Code files a change to its thread.
        let files = |set: &'static str| {
            let (fake, t3) = (fake.clone(), t3.clone());
            async move {
                let mut store = fake.store().await;
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE orchestration_v2_projection_threads SET payload_json = \
                     json_set(payload_json, {set}) WHERE thread_id = 't3-a'"
                )))
                .execute(&mut store)
                .await
                .unwrap();
                sqlx::Connection::close(store).await.unwrap();
                t3.reread_store().await;
            }
        };
        let shown = || async {
            let (status, page) = crate::test_support::call(
                &state,
                "GET",
                "/api/v1/projects/project/threads",
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["reference"] == key.as_str())
        };
        let archived = || async {
            let (status, page) = crate::test_support::call(
                &state,
                "GET",
                "/api/v1/projects/project/threads/archived",
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            crate::tests::validate_http_contract("archivedProjectThreadsPage", &page);
            page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["reference"] == key.as_str())
                .cloned()
        };
        let patch = |fields: Value| {
            update_conversation(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Json(serde_json::from_value(fields).unwrap()),
            )
        };
        assert!(shown().await, "open in T3 Code");
        assert_eq!(archived().await, None);

        files("'$.settledOverride', 'settled', '$.settledAt', '2026-10-22T10:00:00.000Z'").await;
        assert!(!shown().await, "settled in T3 Code");
        let row = archived().await.expect("listed under Archived");
        assert_eq!(row["closedIn"], "Settled in T3 Code");
        assert_eq!(row["reopensThere"], true);
        assert_eq!(row["conversationId"], "project-chat");

        // Restore asks T3 Code to unsettle it and shows it in Cupola at once,
        // before T3 Code files the change.
        assert_eq!(
            patch(json!({"isArchived": false})).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            *fake.organized.lock().unwrap(),
            [json!({"action": "unsettle", "threadId": "t3-a"})]
        );
        assert!(shown().await, "restored");
        assert_eq!(archived().await, None);
        files("'$.settledOverride', 'active', '$.settledAt', NULL").await;
        assert!(shown().await, "unsettled in T3 Code");

        // Settled again later: hidden again.
        files("'$.settledOverride', 'settled', '$.settledAt', '2026-10-22T12:00:00.000Z'").await;
        assert!(!shown().await, "settled anew");

        // Pinned in Cupola, as pinning in T3 Code unsettles a thread.
        assert_eq!(
            patch(json!({"isPinned": true})).await.status(),
            StatusCode::OK
        );
        assert!(shown().await, "pinned in Cupola");
        assert_eq!(
            patch(json!({"isPinned": false})).await.status(),
            StatusCode::OK
        );
        assert!(!shown().await);

        // Snoozed: hidden until it ends.
        files(
            "'$.settledOverride', NULL, '$.settledAt', NULL, \
             '$.snoozedUntil', '2999-01-01T09:00:00.000Z', '$.snoozedAt', '2026-10-22T13:00:00.000Z'",
        )
        .await;
        assert!(!shown().await, "snoozed");
        let row = archived().await.expect("snoozed is listed");
        assert_eq!(row["closedIn"], "Snoozed in T3 Code");
        assert_eq!(row["returnsAt"], "2999-01-01T09:00:00Z");
        files("'$.snoozedUntil', '2026-10-22T14:00:00.000Z'").await;
        assert!(shown().await, "the snooze ended");

        // Archived in T3 Code; then used in Cupola after that.
        fake.archive("t3-a").await;
        t3.reread_store().await;
        assert!(!shown().await, "archived in T3 Code");
        assert_eq!(
            archived().await.expect("listed")["closedIn"],
            "Archived in T3 Code"
        );
        state
            .store
            .touch_project_conversation("project-chat", &now_text())
            .await
            .unwrap();
        assert!(shown().await, "used in Cupola since");
        state.projects.shutdown().await;
    }

    // Contract (owner, Nov 4): a T3 Code `delegate_task` child is a session
    // the native lists also report, but it belongs to its parent chat's agent
    // tasks: it is in neither Chats nor Archived while T3 Code has the
    // parent, closed or not. Once T3 Code no longer has the parent it is
    // listed, marked as a subagent. Pinned in Cupola, it stays in Chats.
    #[tokio::test]
    async fn t3_code_delegate_children_leave_chats_for_their_parent() {
        let (dir, mut state, _message) = handoff_fixture().await;
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread": {
                "id": "thread", "cwd": conversation.cwd, "name": "Recovery", "updatedAt": 1,
                "status": {"type": "idle"}, "parentThreadId": null,
            }})
            .to_string(),
        )
        .unwrap();
        let fake = crate::t3_code::tests::Fake::start().await;
        let (t3, _grants) = crate::t3_code::tests::connected(&fake).await;
        state.t3_code = t3.clone();
        fake.keeps("t3-parent", "codex", "parent-native").await;
        fake.keeps("t3-child", "codex", "thread").await;
        let key = reference(AgentFamily::Codex, "thread");
        let row = || async {
            let (status, page) = crate::test_support::call(
                &state,
                "GET",
                "/api/v1/projects/project/threads",
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["reference"] == key.as_str())
                .cloned()
        };
        let archived = || async {
            let (_, page) = crate::test_support::call(
                &state,
                "GET",
                "/api/v1/projects/project/threads/archived",
                Value::Null,
            )
            .await;
            page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["reference"] == key.as_str())
        };
        assert!(row().await.is_some(), "a plain T3 Code thread is a chat");

        fake.child_of("t3-child", "t3-parent").await;
        t3.reread_store().await;
        assert!(
            row().await.is_none(),
            "a child of a known parent is no chat"
        );

        // Settled or archived in T3 Code, it is still not filed as a chat.
        let mut store = fake.store().await;
        sqlx::query(
            "UPDATE orchestration_v2_projection_threads SET archived_at = ? WHERE thread_id = 't3-child'",
        )
        .bind("2026-10-21T11:00:00.000Z")
        .execute(&mut store)
        .await
        .unwrap();
        sqlx::Connection::close(store).await.unwrap();
        t3.reread_store().await;
        assert!(
            row().await.is_none() && !archived().await,
            "not under Archived"
        );
        let mut store = fake.store().await;
        sqlx::query(
            "UPDATE orchestration_v2_projection_threads SET archived_at = NULL WHERE thread_id = 't3-child'",
        )
        .execute(&mut store)
        .await
        .unwrap();
        sqlx::Connection::close(store).await.unwrap();

        // The parent chat is gone from T3 Code: the child stands on its own.
        fake.remove("t3-parent").await;
        t3.reread_store().await;
        let orphan = row().await.expect("an orphaned child is listed");
        assert_eq!(orphan["isSubagent"], true);
        state.projects.shutdown().await;
    }

    // Owner decision: archive is never refused. Each case that used to be
    // refused (a running turn, an uncertain send, Codex refusing or failing,
    // no first message) archives in Cupola, asks running work to stop, and
    // drops the thread from every list. Restore undoes both archives.
    #[tokio::test]
    async fn codex_archive_is_never_refused_stops_work_and_restores() {
        let (dir, state, message) = handoff_fixture().await;
        let conversation = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread": {
                "id": "thread", "cwd": conversation.cwd, "name": "Recovery", "updatedAt": 1,
                "status": {"type": "idle"}, "parentThreadId": null,
            }})
            .to_string(),
        )
        .unwrap();
        // Codex still reports the turn running.
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id": "turn", "status": "inProgress"}]]).to_string(),
        )
        .unwrap();
        let MessageInsert::Inserted(stuck) = state
            .store
            .insert_dispatch_message(
                "owner",
                "stuck-receipt",
                "stuck",
                "stuck-hash",
                "project-chat",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        state
            .store
            .update_message_delivery(&stuck.id, "uncertain", None, None)
            .await
            .unwrap();
        state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let requests = || {
            std::fs::read_to_string(dir.path().join("requests-jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        };
        let count = |method: &str| requests().iter().filter(|r| r["method"] == method).count();
        let patch = |archived: bool| {
            update_conversation(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Json(serde_json::from_value(json!({"isArchived": archived})).unwrap()),
            )
        };
        let listed = || async {
            let library = list(State(state.clone()), Extension(OwnerAuthority)).await;
            let library: Value = serde_json::from_slice(
                &axum::body::to_bytes(library.into_body(), 1_000_000)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let (status, page) = crate::test_support::call(
                &state,
                "GET",
                "/api/v1/projects/project/threads",
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let pinned = library["pinned"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["thread"]["conversationId"] == "project-chat");
            let threads = page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["conversationId"] == "project-chat");
            (pinned, threads)
        };
        let archived_rows = || async {
            let page = archived_threads(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project".into()),
            )
            .await;
            let page: Value = serde_json::from_slice(
                &axum::body::to_bytes(page.into_body(), 1_000_000)
                    .await
                    .unwrap(),
            )
            .unwrap();
            page["threads"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| t["conversationId"] == "project-chat")
                .count()
        };
        let is_archived = || async {
            let stored = state
                .store
                .project_conversation("project-chat")
                .await
                .unwrap()
                .unwrap();
            conversation_detail(&state, &stored)
                .await
                .unwrap()
                .is_archived
        };
        // Lists show the last answer; an action records the first one.
        assert_eq!(codex_is_archived(&state, &conversation).await, Ok(false));
        assert_eq!(listed().await, (true, true));

        // Running, uncertain, and the Codex app holding the thread.
        std::fs::write(dir.path().join("archive-active-writer"), "").unwrap();
        let response = patch(true).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["isArchived"], true);
        crate::tests::validate_http_contract("projectConversationDetail", &body);
        assert!(requests().iter().any(|r| r["method"] == "turn/interrupt"
            && r["params"]["threadId"] == "thread"
            && r["params"]["turnId"] == "turn"));
        assert_eq!(count("thread/archive"), 1, "Codex was still asked");
        assert!(!dir.path().join("archived-thread").exists());
        for id in [&message.id, &stuck.id] {
            assert_eq!(
                state.store.message_by_id(id).await.unwrap().unwrap().state,
                "interrupted",
                "No send stays in flight or uncertain"
            );
        }
        assert!(!state
            .store
            .conversation_has_archive_blocking_work("project-chat")
            .await
            .unwrap());
        assert_eq!(listed().await, (false, false));
        assert_eq!(archived_rows().await, 1);
        let mut submitting = false;
        assert!(dispatch_inner(&state, &message, &mut submitting)
            .await
            .unwrap_err()
            .contains("archived"));
        assert!(!submitting);
        // A retry is a no-op; a restart reads the archive from the store.
        set_archived(&state, &conversation, true).await.unwrap();
        assert_eq!(count("thread/archive"), 1);
        state.projects.shutdown().await;
        assert!(is_archived().await);
        std::fs::remove_file(dir.path().join("archive-active-writer")).unwrap();
        std::fs::remove_file(dir.path().join("turn-pages.json")).unwrap();

        // Restore brings it back, still pinned.
        let restored = patch(false).await;
        assert_eq!(restored.status(), StatusCode::OK);
        assert!(!is_archived().await);
        assert_eq!(listed().await, (true, true));

        // An update handoff is the one short wait: nothing is touched.
        state
            .update_admission
            .prepare_update("archive-update", &state)
            .await
            .unwrap();
        let (archives, interrupts) = (count("thread/archive"), count("turn/interrupt"));
        assert_eq!(patch(true).await.status(), StatusCode::CONFLICT);
        assert_eq!(count("thread/archive"), archives);
        assert_eq!(count("turn/interrupt"), interrupts);
        assert!(!is_archived().await);
        assert!(state.update_admission.cancel_lease("archive-update").await);

        // Codex accepts: archived in both places and listed once.
        set_archived(&state, &conversation, true).await.unwrap();
        assert!(dir.path().join("archived-thread").exists());
        assert!(is_archived().await);
        assert_eq!(archived_rows().await, 1);
        set_archived(&state, &conversation, false).await.unwrap();
        assert!(!dir.path().join("archived-thread").exists());
        assert!(!is_archived().await);

        // Codex fails outright, or loses its response.
        for fixture in ["archive-fail", "archive-lost-response"] {
            std::fs::write(dir.path().join(fixture), "").unwrap();
            set_archived(&state, &conversation, true).await.unwrap();
            assert!(is_archived().await, "{fixture}");
            assert_eq!(listed().await, (false, false), "{fixture}");
            std::fs::remove_file(dir.path().join(fixture)).unwrap();
            set_archived(&state, &conversation, false).await.unwrap();
            assert!(!is_archived().await, "{fixture}");
        }

        // An archive or restore in the Codex app is read without a Cupola
        // write, in the background of the chat's next open.
        let eventually = |archived: bool| async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                while is_archived().await != archived {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok()
        };
        std::fs::write(dir.path().join("archived-thread"), "").unwrap();
        assert!(eventually(true).await);
        std::fs::remove_file(dir.path().join("archived-thread")).unwrap();
        assert!(eventually(false).await);

        // A thread with no first message has nothing to stop.
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "draft-chat",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd: &conversation.cwd,
                roots_revision: 1,
                title: "Draft",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        let draft = state
            .store
            .project_conversation("draft-chat")
            .await
            .unwrap()
            .unwrap();
        set_archived(&state, &draft, true).await.unwrap();
        assert!(
            state
                .store
                .project_conversation("draft-chat")
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );

        // A stalled provider lock must not stall every Project/library read.
        let held = state.projects.codex.lock().await;
        let bounded = tokio::time::timeout(
            Duration::from_secs(60),
            archived_codex_ids(&state, std::slice::from_ref(&conversation)),
        )
        .await;
        drop(held);
        state.projects.shutdown().await;
        assert!(
            bounded.is_ok(),
            "Archive reconciliation must include the runtime lock in its deadline"
        );
        assert!(
            bounded.unwrap().is_err(),
            "Incomplete native state cannot become an empty archive"
        );
        // Claude archives in Cupola only: claude_archive_is_wonder_local_listed_and_restorable.
    }

    // A slow handover (Claude on the Mac taking seconds to let go, a Codex
    // thread reopening) must not hold the lock every other chat's send and
    // every streamed event need; the claim made under it still holds.
    #[tokio::test]
    async fn dispatch_prepares_outside_the_shared_lock_and_keeps_its_claim() {
        let (_dir, state, message) = dispatch_failure_fixture().await;
        PREPARE_HOLD
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(message.id.clone());
        let running = tokio::spawn(dispatch(state.clone(), message.clone()));
        let claimed = async {
            loop {
                if state
                    .store
                    .message_by_id(&message.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state
                    == "dispatching_to_codex"
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(60), claimed)
            .await
            .unwrap();
        // While it prepares: the lock is free, and claim recovery (as the
        // dispatch tick runs it) leaves this live claim alone.
        let started = std::time::Instant::now();
        let guard = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            state.dispatch_lock.lock(),
        )
        .await
        .expect("dispatch_lock is free while a dispatch prepares");
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
        assert_eq!(state.projects.dispatching(), vec![message.id.clone()]);
        state
            .store
            .recover_direct_dispatch_claims_except(&state.projects.dispatching())
            .await
            .unwrap();
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "dispatching_to_codex"
        );
        drop(guard);
        PREPARE_HOLD
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .remove(&message.id);
        running.await.unwrap();
        assert!(state.projects.dispatching().is_empty());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    async fn wait_for_dispatch_fixture(path: &FsPath) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fixture never reached {}", path.display()));
    }

    async fn another_dispatch_chat(
        state: &AppState,
        family: AgentFamily,
    ) -> wonder_store::StoredMessage {
        // Codex's first start refreshes the catalog. Keep that provider-owned
        // answer consistent with the model selected in these chats.
        std::fs::write(
            state.data_dir.join("models.json"),
            json!({"data":[{"id":"fake","model":"fake","displayName":"Fixture Codex","isDefault":true}]}).to_string(),
        ).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let project = state.store.project("project").await.unwrap().unwrap();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: &id,
                project_id: &project.id,
                family,
                provider_store: provider_store(state, family),
                native_session_id: None,
                cwd: &project.roots[0].canonical_path,
                roots_revision: 1,
                title: "Independent chat",
                model: Some(if family == AgentFamily::Claude {
                    "claude:haiku"
                } else {
                    "fake"
                }),
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message("owner", &id, "Other task", "hash", &id, &[], "now", true)
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        message
    }

    async fn independent_claude_fixture(dir: &FsPath, state: &mut AppState) {
        let entrypoint = dir.join("independent-claude.py");
        std::fs::write(&entrypoint, r#"import json, sys, os
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    with open(os.path.join(os.path.dirname(__file__), 'claude-composer-payloads'), 'a') as log: log.write(json.dumps(r) + '\n')
    m = r.get('method'); result = {}
    if m == 'initialize': result = {'wonderBridge':{'protocolVersion':1,'family':'claude'},'capabilities':{'experimentalApi':True}}
    elif m == 'account/read': result = {'connected':True}
    elif m == 'model/list': result = {'data':[{'id':'claude:haiku','displayName':'Fixture Haiku'}]}
    elif m == 'thread/start': result = {'thread':{'id':'claude-thread','sessionId':'claude-session'}}
    elif m == 'turn/start': result = {'turn':{'id':'claude-turn'}}
    print(json.dumps({'id':r['id'],'result':result}),flush=True)
"#).unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime {
            client: Arc::new(Mutex::new(AppServerClient::unavailable(
                "test".into(),
                crate::ingestion::notification_sink(state.store.clone()),
            ))),
            config: wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint,
                state_dir: dir.join("independent-claude-state"),
                npm_cli: None,
                claude_config_dir: None,
                wonder_version: "test".into(),
                family: "claude",
            },
        }));
        drop(claude_rpc(state).await.unwrap());
    }

    // Hold A's actual provider response, leaving its transport responsive.
    // B must persist its own receipt before A is released, both on the same
    // provider and across providers. A reconnect must leave A's generation alone.
    #[tokio::test]
    async fn held_fake_provider_start_does_not_block_another_chat() {
        for family in [AgentFamily::Codex, AgentFamily::Claude] {
            let (dir, mut state, first) = dispatch_failure_fixture().await;
            independent_claude_fixture(dir.path(), &mut state).await;
            let second = another_dispatch_chat(&state, family).await;
            crate::phone_approval_tests::hold_provider_send(dir.path());
            let script = dir.path().join("runtime.py");
            let source = std::fs::read_to_string(&script).unwrap()
                .replace("elif method == 'thread/start': result = {'thread':{'id':'thread'}}",
                    "elif method == 'thread/start': result = {'thread':{'id':'thread-' + str(r['id'])}}")
                .replace("elif method == 'turn/start':\n        open(root + '/send-waiting'",
                    &format!("elif method == 'turn/start' and r['params']['clientUserMessageId'] == {}:\n        open(root + '/send-waiting'", json!(first.client_message_id)))
                .replace("        continue\n        with open(root + '/accepted-client'",
                    "        continue\n    elif method == 'turn/start':\n        with open(root + '/accepted-client'");
            std::fs::write(&script, source).unwrap();
            let sending = tokio::spawn(dispatch(state.clone(), first.clone()));
            wait_for_dispatch_fixture(&dir.path().join("send-waiting")).await;
            let generation = state.projects.codex.lock().await.health().id().to_owned();
            let reconnect = crate::provider_runtime::reconnect(
                State(state.clone()),
                Some(Extension(crate::LocalOwnerAuthority)),
                Path("codex".into()),
            )
            .await;
            assert_eq!(reconnect.status(), StatusCode::CONFLICT);
            assert_eq!(state.projects.codex.lock().await.health().id(), generation);
            let independent = tokio::time::timeout(
                Duration::from_secs(10),
                dispatch(state.clone(), second.clone()),
            )
            .await;
            let second_state = state
                .store
                .message_by_id(&second.id)
                .await
                .unwrap()
                .unwrap();
            let first_state = state.store.message_by_id(&first.id).await.unwrap().unwrap();
            let second_error = state
                .store
                .message_delivery_error(&second.id)
                .await
                .unwrap();
            let pending = !sending.is_finished();
            std::fs::write(dir.path().join("release-send"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(10), sending)
                .await
                .unwrap()
                .unwrap();
            state.projects.shutdown().await;
            state
                .claude
                .as_ref()
                .unwrap()
                .client
                .lock()
                .await
                .shutdown()
                .await
                .unwrap();
            assert!(
                independent.is_ok(),
                "{family:?}: B waited for A's provider response"
            );
            assert_eq!(
                second_state.state, "accepted_by_codex",
                "{family:?}: {second_error:?}"
            );
            assert_eq!(first_state.state, "dispatching_to_codex");
            assert!(pending, "A must still be held when B finishes");
        }
    }

    // Preparation runs before reconnect admission. Reconnect may replace that
    // generation, but submission must refuse it before writing durable intent.
    #[tokio::test]
    async fn held_fake_provider_preparation_is_revalidated_after_reconnect() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        let ordered = state
            .projects
            .conversation_dispatch(&message.conversation_id)
            .await;
        let sending = tokio::spawn(dispatch(state.clone(), message.clone()));
        // The waiting guard's second owner proves preparation finished and
        // dispatch reached the chat's ordering gate, before runtime admission.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting = state
                    .projects
                    .conversation_dispatch
                    .lock()
                    .unwrap()
                    .get(&message.conversation_id)
                    .is_some_and(|lock| lock.strong_count() > 1);
                if waiting {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("send finished preparation and is waiting for its chat");
        let generation = state.projects.codex.lock().await.health().id().to_owned();
        let response = crate::provider_runtime::reconnect(
            State(state.clone()),
            Some(Extension(crate::LocalOwnerAuthority)),
            Path("codex".into()),
        )
        .await;
        let after = state.projects.codex.lock().await.health().id().to_owned();
        drop(ordered);
        tokio::time::timeout(Duration::from_secs(10), sending)
            .await
            .unwrap()
            .unwrap();
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        state.projects.shutdown().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(after, generation);
        assert_eq!(
            saved.state, "safe_to_retry",
            "a replaced generation must not submit"
        );
        assert!(state
            .store
            .message_delivery_error(&message.id)
            .await
            .unwrap()
            .as_deref()
            .is_some_and(|error| error.contains("Try again")));
        assert!(state
            .store
            .dispatch_context(&message.id)
            .await
            .unwrap()
            .is_none());
        assert!(!std::fs::read_to_string(dir.path().join("requests-jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .any(|r| r["method"] == "turn/start"));
    }

    #[tokio::test]
    async fn held_fake_provider_restore_does_not_block_another_chat() {
        let (dir, state, first) = dispatch_failure_fixture().await;
        let second = another_dispatch_chat(&state, AgentFamily::Codex).await;
        crate::test_support::bind_project_thread(
            &state,
            &first.conversation_id,
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        state
            .store
            .set_project_conversation_wonder_archived(&first.conversation_id, true, "now")
            .await
            .unwrap();
        let conversation = state
            .store
            .project_conversation(&first.conversation_id)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("archive-fixture"), "").unwrap();
        std::fs::write(dir.path().join("archived-thread"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread": {
                "id":"thread", "cwd":conversation.cwd, "status":{"type":"idle"},
            }})
            .to_string(),
        )
        .unwrap();
        let script = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&script).unwrap().replace(
            "elif method == 'thread/start': result = {'thread':{'id':'thread'}}",
            "elif method == 'thread/start': result = {'thread':{'id':'thread-' + str(r['id'])}}",
        );
        let marker = "    elif method in ('thread/archive','thread/unarchive') and os.path.exists(root + '/archive-fixture'):";
        assert!(source.contains(marker));
        std::fs::write(&script, source.replacen(marker, &format!(r#"    elif method == 'thread/unarchive' and os.path.exists(root + '/hold-restore'):
        open(root + '/restore-waiting','w').close()
        def restore_later(request_id):
            while os.path.exists(root + '/hold-restore'): time.sleep(0.01)
            os.remove(root + '/archived-thread')
            print(json.dumps({{'id':request_id,'result':{{'thread':idle['thread']}}}}),flush=True)
        threading.Thread(target=restore_later,args=(r['id'],),daemon=True).start()
        continue
{marker}"#), 1)).unwrap();
        std::fs::write(dir.path().join("hold-restore"), "").unwrap();
        let restoring = tokio::spawn({
            let state = state.clone();
            let conversation = conversation.clone();
            async move { set_archived(&state, &conversation, false).await }
        });
        wait_for_dispatch_fixture(&dir.path().join("restore-waiting")).await;
        let independent = tokio::time::timeout(
            Duration::from_secs(10),
            dispatch(state.clone(), second.clone()),
        )
        .await;
        let saved = state
            .store
            .message_by_id(&second.id)
            .await
            .unwrap()
            .unwrap();
        let pending = !restoring.is_finished();
        let error = state
            .store
            .message_delivery_error(&second.id)
            .await
            .unwrap();
        std::fs::remove_file(dir.path().join("hold-restore")).unwrap();
        let restored = tokio::time::timeout(Duration::from_secs(10), restoring)
            .await
            .unwrap()
            .unwrap();
        state.projects.shutdown().await;
        assert!(independent.is_ok(), "B waited for A's restore response");
        assert_eq!(saved.state, "accepted_by_codex", "{error:?}");
        assert!(pending);
        assert_eq!(restored, Ok(()));
        assert!(
            !state
                .store
                .project_conversation(&first.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .wonder_archived
        );
    }

    // The final idle probe can wait for its runtime's client lock, but must
    // leave other providers free. Hold that lock after the native probe has
    // started, then observe the probe owning `start` while it waits for us.
    #[tokio::test]
    async fn held_fake_provider_idle_release_does_not_block_another_provider() {
        let (dir, mut state, first) = handoff_fixture().await;
        independent_claude_fixture(dir.path(), &mut state).await;
        let second = another_dispatch_chat(&state, AgentFamily::Claude).await;
        state
            .store
            .update_message_delivery(&first.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        state.projects.shutdown().await;
        let script = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&script).unwrap();
        let marker = "    elif method == 'thread/backgroundTerminals/list':\n        background =";
        assert!(source.contains(marker));
        std::fs::write(&script, source.replacen(marker, r#"    elif method == 'thread/backgroundTerminals/list':
        if os.path.exists(root + '/hold-idle-probe'):
            open(root + '/idle-probe-waiting','w').close()
            def idle_later(request_id):
                while os.path.exists(root + '/hold-idle-probe'): time.sleep(0.01)
                print(json.dumps({'id':request_id,'result':{'data':[],'nextCursor':None}}),flush=True)
            threading.Thread(target=idle_later,args=(r['id'],),daemon=True).start()
            continue
        background ="#, 1)).unwrap();
        drop(codex_rpc(&state).await.unwrap());
        std::fs::write(dir.path().join("hold-idle-probe"), "").unwrap();
        let probing = tokio::spawn({
            let state = state.clone();
            async move { release_idle_codex_runtime(&state).await }
        });
        wait_for_dispatch_fixture(&dir.path().join("idle-probe-waiting")).await;
        let client = state.projects.codex.lock().await;
        std::fs::remove_file(dir.path().join("hold-idle-probe")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while state.projects.start.try_lock().is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("idle release reached the final runtime check");
        let independent = tokio::time::timeout(
            Duration::from_secs(10),
            dispatch(state.clone(), second.clone()),
        )
        .await;
        let saved = state
            .store
            .message_by_id(&second.id)
            .await
            .unwrap()
            .unwrap();
        let pending = !probing.is_finished();
        drop(client);
        tokio::time::timeout(Duration::from_secs(10), probing)
            .await
            .unwrap()
            .unwrap();
        state.projects.shutdown().await;
        state
            .claude
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .shutdown()
            .await
            .unwrap();
        assert!(independent.is_ok(), "B waited for Codex's idle release");
        assert_eq!(saved.state, "accepted_by_codex");
        assert!(pending);
    }

    // A desktop-owned thread can be notLoaded in Wonder's separate App Server
    // while its last persisted turn is interrupted. The provider writer lock
    // must reject resume before Wonder starts another turn, with useful retry copy.
    #[tokio::test]
    async fn project_send_reports_desktop_writer_conflict_without_starting_turn() {
        let (dir, state, message) = dispatch_failure_fixture().await;
        let rpc = codex_rpc(&state).await.unwrap();
        drop(rpc);
        state.runtime_catalog.write().await.models = vec![crate::ModelOption {
            agent_family: AgentFamily::Codex,
            capabilities: crate::ModelCapabilities::for_family(AgentFamily::Codex),
            id: "fake".into(),
            display_name: "Fake model".into(),
            description: None,
            model_specialty: None,
            hidden: false,
            reasoning_efforts: vec![],
            default_reasoning_effort: None,
            service_tiers: vec![crate::ChoiceOption {
                id: "default".into(),
                label: "Standard".into(),
                description: None,
            }],
            default_service_tier: Some("default".into()),
            is_default: false,
            provider_default: None,
            native_ids: vec![],
        }];
        state
            .store
            .set_project_native_session(&message.conversation_id, "thread", "now")
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                &message.conversation_id,
                AgentFamily::Codex,
                &state.projects.codex_store,
                "thread",
                None,
                "now",
            )
            .await
            .unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{"id":"thread","cwd":dir.path().join("project-source"),
                "projectId":"native-project","status":{"type":"notLoaded"}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id":"old-turn","status":"interrupted"}]]).to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("resume-active-writer"), "").unwrap();

        let mut submitting = false;
        let failure = dispatch_inner(&state, &message, &mut submitting)
            .await
            .unwrap_err();
        assert!(
            failure.contains("The Codex app on your computer is using this thread"),
            "{failure}"
        );
        assert!(!submitting);
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert!(requests.lines().any(|line| {
            serde_json::from_str::<Value>(line).unwrap()["method"] == "thread/resume"
        }));
        assert!(!requests.lines().any(|line| {
            serde_json::from_str::<Value>(line).unwrap()["method"] == "turn/start"
        }));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a lost terminal notification cannot leave a live-generation
    // receipt busy forever. The existing transport/SQLite owner checks exact
    // turn proof, paged history, durable intent and the actual OS writer lock.
    #[tokio::test]
    async fn project_recovery_settles_live_and_uncertain_receipts_without_resending() {
        let (dir, state, message) = handoff_fixture().await;
        assert!(!writer_available(dir.path()));
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{
            "id":"thread", "status":{"type":"active"}}})
            .to_string(),
        )
        .unwrap();
        for (delivery, native) in [("streaming", "inProgress"), ("uncertain", "inProgress")] {
            state
                .store
                .update_message_delivery(&message.id, delivery, Some("thread"), Some("turn"))
                .await
                .unwrap();
            std::fs::write(
                dir.path().join("turn-pages.json"),
                json!([[{"id":"turn", "status":native}]]).to_string(),
            )
            .unwrap();
            recover(&state).await;
            assert_eq!(
                state
                    .store
                    .message_by_id(&message.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                delivery
            );
        }
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id":"other", "status":"completed"}]]).to_string(),
        )
        .unwrap();
        recover(&state).await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        // Restore the original same-live-generation case. A terminal turn on
        // the second page settles it and an uncertain receipt for the same turn.
        assert!(state.projects.codex.lock().await.health().is_alive());
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let MessageInsert::Inserted(uncertain) = state
            .store
            .insert_message("owner", "uncertain", "work", "hash", "project-chat", "now")
            .await
            .unwrap()
        else {
            panic!("new uncertain receipt")
        };
        state
            .store
            .update_message_delivery(&uncertain.id, "uncertain", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let MessageInsert::Inserted(queued) = state
            .store
            .insert_dispatch_message(
                "owner",
                "queued",
                "next",
                "hash",
                "project-chat",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new queued receipt")
        };
        std::fs::write(dir.path().join("completed"), "").unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{
            "id":"thread", "status":{"type":"idle"}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([
                [{"id":"other", "status":"completed"}], [{"id":"turn", "status":"completed"}]
            ])
            .to_string(),
        )
        .unwrap();
        recover(&state).await;
        for receipt in [&message, &uncertain] {
            assert_eq!(
                state
                    .store
                    .message_by_id(&receipt.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "completed"
            );
        }
        assert_eq!(
            state
                .store
                .message_by_id(&queued.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder"
        );
        assert!(writer_available(dir.path()));
        assert!(!state.projects.codex.lock().await.health().is_alive());
        let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        for method in ["thread/start", "thread/resume", "turn/start"] {
            assert!(
                !requests.lines().any(|line| line == method),
                "{method} must not run during recovery"
            );
        }
        assert!(state
            .store
            .assistant_messages_for_conversation("project-chat")
            .await
            .unwrap()
            .iter()
            .any(|m| m.text == "recovered"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a turn whose runtime was lost reads failed (the owner is told),
    // not interrupted, unless the owner's Stop was waiting on it. A turn native
    // history still shows running keeps an uncertain receipt (no resend).
    #[tokio::test]
    async fn lost_runtime_turn_reads_failed_unless_the_owner_stopped_it() {
        for (native, stopped, expected, receipt) in [
            ("interrupted", false, DeliveryState::Failed, "completed"),
            ("interrupted", true, DeliveryState::Interrupted, "completed"),
            ("inProgress", false, DeliveryState::Failed, "uncertain"),
        ] {
            let (dir, state, message) = handoff_fixture().await;
            std::fs::write(
                dir.path().join("turn-pages.json"),
                json!([[{"id":"turn", "status":native}]]).to_string(),
            )
            .unwrap();
            if stopped {
                request_stop(&state, "thread", "turn").await.unwrap();
            }
            // The runtime that accepted the turn is gone (a host restart).
            state.projects.generations.lock().unwrap().clear();
            let mut events = state.events.subscribe();
            recover(&state).await;
            let mut published = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let wonder_api::WonderEvent::MessageState { state } = event.event {
                    published.push(state);
                }
            }
            assert_eq!(published, [expected], "{native} stopped={stopped}");
            let saved = state.store.message_by_id(&message.id).await.unwrap();
            assert_eq!(saved.unwrap().state, receipt, "{native} stopped={stopped}");
            // Settled once: the next pass publishes nothing new.
            recover(&state).await;
            assert!(events.try_recv().is_err(), "{native} replayed");
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    // Contract: Stop answers only for a Stop that is saved, and a store that
    // cannot say whether the owner stopped a turn defers recovery: the turn is
    // neither failed nor continued, and settles once the store answers.
    #[tokio::test]
    async fn an_unsaved_or_unreadable_stop_never_reads_as_no_stop() {
        let (dir, state, message) = handoff_fixture().await;
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id":"turn", "status":"interrupted"}]]).to_string(),
        )
        .unwrap();
        request_stop(&state, "thread", "turn").await.unwrap();
        state.projects.forget_cached_stops();
        state.projects.generations.lock().unwrap().clear();
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("ALTER TABLE turn_stop_requests RENAME TO turn_stop_requests_away")
            .execute(&pool)
            .await
            .unwrap();
        assert!(request_stop(&state, "thread", "other").await.is_err());
        assert!(stop_requested(&state, "thread", "turn").await.is_err());
        let mut events = state.events.subscribe();
        recover(&state).await;
        assert!(
            events.try_recv().is_err(),
            "settled without knowing the Stop"
        );
        let saved = state.store.message_by_id(&message.id).await.unwrap();
        assert_ne!(saved.unwrap().state, "interrupted");
        sqlx::query("ALTER TABLE turn_stop_requests_away RENAME TO turn_stop_requests")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        recover(&state).await;
        let Ok(event) = events.try_recv() else {
            panic!("the next pass settles the turn")
        };
        assert!(matches!(
            event.event,
            wonder_api::WonderEvent::MessageState {
                state: DeliveryState::Interrupted
            }
        ));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a turn the desktop app runs for a message in Cupola's native
    // queue reads interrupted here (another process's unfinished turn), but its
    // own lifecycle says it is running. Recovery leaves it running, even when it
    // compacts its context mid-turn, and settles it from that lifecycle when it
    // ends; it never shows a failed reply that went on to finish (a started
    // compaction inherits its turn's failure).
    #[tokio::test]
    async fn native_queue_turn_running_in_the_desktop_app_is_not_settled_as_lost() {
        let (dir, mut state, message) = handoff_fixture().await;
        let mut index = crate::codex_activity::tests::Fixture::new_for_thread("thread").await;
        state.codex_home = index.dir.path().to_path_buf();
        crate::desktop_activity::remember_codex_rollout("thread", index.path.to_str().unwrap());
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([[{"id":"turn", "status":"interrupted"}]]).to_string(),
        )
        .unwrap();
        // No generation: the desktop app, not this runtime, accepted the turn.
        state.projects.generations.lock().unwrap().clear();
        let before = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap()
            .state;
        let mut events = state.events.subscribe();
        let mut published = || {
            let mut states = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let wonder_api::WonderEvent::MessageState { state } = event.event {
                    states.push(state);
                }
            }
            states
        };
        recover(&state).await;
        recover(&state).await;
        assert_eq!(published(), [], "a running desktop turn is not lost");
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, before, "the receipt is untouched");
        index.finish("completed").await;
        recover(&state).await;
        assert_eq!(published(), [DeliveryState::Completed]);
        crate::desktop_activity::forget_codex_rollout("thread");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a turn ChatGPT's desktop app runs for a relayed message always
    // ends. A finished turn in the shared rollout settles it, the owner's Stop
    // reads interrupted, and with no word from ChatGPT for DESKTOP_RELAY_QUIET
    // (it acknowledged the turn, then quit) the receipt is unconfirmed: the
    // owner is told plainly and it is never resent, rather than running forever.
    #[tokio::test]
    async fn relayed_turn_settles_or_is_told_lost_never_runs_forever() {
        let long_ago = (Utc::now() - chrono::Duration::minutes(20))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let ended = |status: &str| json!({"id":"desktop-turn","status":status,"completedAt":1});
        let open = |status: &str| json!({"id":"desktop-turn","status":status});
        for (native, stopped, relayed_at, expected, receipt) in [
            (None, false, now_text(), None, "accepted_by_codex"),
            (
                Some(open("inProgress")),
                false,
                now_text(),
                None,
                "accepted_by_codex",
            ),
            // Wonder reads another process's unfinished turn as interrupted.
            (
                Some(open("interrupted")),
                false,
                now_text(),
                None,
                "accepted_by_codex",
            ),
            (
                None,
                false,
                long_ago.clone(),
                Some(DeliveryState::Failed),
                "uncertain",
            ),
            (
                Some(open("inProgress")),
                false,
                long_ago.clone(),
                Some(DeliveryState::Failed),
                "uncertain",
            ),
            (
                Some(open("interrupted")),
                false,
                long_ago.clone(),
                Some(DeliveryState::Failed),
                "uncertain",
            ),
            // The phone's Stop, or the owner's Stop in ChatGPT (abort recorded).
            (
                Some(open("interrupted")),
                true,
                now_text(),
                Some(DeliveryState::Interrupted),
                "completed",
            ),
            (
                Some(ended("interrupted")),
                false,
                now_text(),
                Some(DeliveryState::Interrupted),
                "completed",
            ),
            (
                Some(ended("completed")),
                false,
                now_text(),
                Some(DeliveryState::Completed),
                "completed",
            ),
        ] {
            let case = format!("{native:?} stopped={stopped} at={relayed_at}");
            let (dir, state, fixture) = handoff_fixture().await;
            state
                .store
                .update_message_delivery(&fixture.id, "completed", None, None)
                .await
                .unwrap();
            // The journal a relay writes, then ChatGPT's turn id as the receipt.
            let thread = uuid::Uuid::new_v4().to_string();
            let MessageInsert::Inserted(message) = state
                .store
                .insert_dispatch_message(
                    "owner",
                    "relayed",
                    "work",
                    "hash",
                    "project-chat",
                    &[],
                    "now",
                    true,
                )
                .await
                .unwrap()
            else {
                panic!("new relayed receipt")
            };
            assert!(state
                .store
                .claim_message_for_dispatch(&message.id)
                .await
                .unwrap());
            state
                .store
                .begin_dispatch_submission_with_context(
                    &message.id,
                    &thread,
                    Some(
                        &json!({"deliveryMethod": DESKTOP_RELAY_METHOD, "relayedAt": relayed_at})
                            .to_string(),
                    ),
                )
                .await
                .unwrap();
            state
                .store
                .update_message_delivery(
                    &message.id,
                    "accepted_by_codex",
                    Some(&thread),
                    Some("desktop-turn"),
                )
                .await
                .unwrap();
            record_generation(&state, &message.id, DESKTOP_RELAY_GENERATION);
            let turns = native.into_iter().collect::<Vec<_>>();
            std::fs::write(
                dir.path().join("turn-pages.json"),
                json!([turns]).to_string(),
            )
            .unwrap();
            if stopped {
                request_stop(&state, &thread, "desktop-turn").await.unwrap();
            }
            let mut events = state.events.subscribe();
            recover(&state).await;
            let mut published = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let wonder_api::WonderEvent::MessageState { state } = event.event {
                    published.push(state);
                }
            }
            assert_eq!(
                published,
                expected.into_iter().collect::<Vec<_>>(),
                "{case}"
            );
            let saved = state.store.message_by_id(&message.id).await.unwrap();
            assert_eq!(saved.unwrap().state, receipt, "{case}");
            if receipt == "uncertain" {
                assert_eq!(
                    state
                        .store
                        .message_delivery_error(&message.id)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(DESKTOP_RELAY_STALE),
                    "{case}"
                );
                assert!(!state
                    .store
                    .requeue_message_for_retry(&message.id)
                    .await
                    .unwrap());
            }
            // Settled (or told) once: the next pass publishes nothing new.
            recover(&state).await;
            assert!(events.try_recv().is_err(), "{case} replayed");
            let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap_or_default();
            for method in ["thread/resume", "turn/start", "turn/interrupt"] {
                assert!(
                    !requests.lines().any(|line| line == method),
                    "{case}: {method} must not run for a relayed turn"
                );
            }
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    // Contract: handing the writer back cannot stop another loaded thread,
    // autonomous goal, background command or a captured history/approval RPC.
    // Unmapped native children are checked directly, rather than assumed idle.
    #[tokio::test]
    async fn project_handoff_preserves_other_native_work_and_readers() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        release_idle_codex_runtime(&state).await;
        assert!(!writer_available(dir.path()));
        drop(rpc);
        let mut idle = json!({"thread":{"id":"thread", "status":{"type":"idle"}},
            "child-thread":{"id":"child-thread", "status":{"type":"idle"}}});
        for (path, value) in [
            ("/child-thread/status/type", json!("active")),
            ("/thread/goal", json!({"status":"active"})),
            ("/thread/goal", json!({"status":"futureStatus"})),
            ("/thread/background", json!([{"processId":"running"}])),
        ] {
            // Goal/background keys are optional in native snapshots.
            if path == "/thread/goal" {
                idle["thread"]["goal"] = value;
            } else if path == "/thread/background" {
                idle["thread"]["background"] = value;
            } else {
                *idle.pointer_mut(path).unwrap() = value;
            }
            std::fs::write(dir.path().join("idle-fixture.json"), idle.to_string()).unwrap();
            release_idle_codex_runtime(&state).await;
            assert!(
                state.projects.codex.lock().await.health().is_alive(),
                "{path}"
            );
            assert!(!writer_available(dir.path()));
            idle = json!({"thread":{"id":"thread", "status":{"type":"idle"}},
                "child-thread":{"id":"child-thread", "status":{"type":"idle"}}});
        }
        let MessageInsert::Inserted(queued) = state
            .store
            .insert_dispatch_message(
                "owner",
                "native-queued",
                "next",
                "hash",
                "project-chat",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("queued receipt");
        };
        assert!(state
            .store
            .claim_message_for_dispatch(&queued.id)
            .await
            .unwrap());
        state
            .store
            .begin_dispatch_submission_with_context(
                &queued.id,
                "thread",
                Some(&json!({"deliveryMethod":"thread/queue/add", "nativeQueueEnqueuedAt":now_text()}).to_string()),
            )
            .await
            .unwrap();
        state
            .store
            .update_message_delivery(&queued.id, "uncertain", Some("thread"), None)
            .await
            .unwrap();
        release_idle_codex_runtime(&state).await;
        assert!(
            state.projects.codex.lock().await.health().is_alive(),
            "native queue keeps its owner until a turn receipt"
        );
        assert!(!writer_available(dir.path()));
        state
            .store
            .update_message_delivery(&queued.id, "completed", Some("thread"), None)
            .await
            .unwrap();
        idle["thread"]["goal"] = json!({"status":"complete"});
        std::fs::write(dir.path().join("idle-fixture.json"), idle.to_string()).unwrap();
        release_idle_codex_runtime(&state).await;
        assert!(writer_available(dir.path()));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// A Project send whose native enqueue was journaled but never confirmed.
    async fn uncertain_native_queue_send(
        state: &AppState,
        client: &str,
        thread: &str,
    ) -> wonder_store::StoredMessage {
        let MessageInsert::Inserted(queued) = state
            .store
            .insert_dispatch_message(
                "owner",
                client,
                "next",
                "hash",
                "project-chat",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("queued receipt");
        };
        assert!(state
            .store
            .claim_message_for_dispatch(&queued.id)
            .await
            .unwrap());
        let context = json!({
            "deliveryMethod":"thread/queue/add",
            "nativeQueueEnqueuedAt": Utc::now().to_rfc3339(),
        });
        state
            .store
            .begin_dispatch_submission_with_context(&queued.id, thread, Some(&context.to_string()))
            .await
            .unwrap();
        state
            .store
            .update_message_delivery(&queued.id, "uncertain", Some(thread), None)
            .await
            .unwrap();
        state
            .store
            .message_by_id(&queued.id)
            .await
            .unwrap()
            .unwrap()
    }

    // Contract: Wonder taking the thread over while a native-queue history
    // check is in flight still triggers a follow-up check; it is not lost.
    #[tokio::test]
    async fn codex_native_queue_event_during_scan_triggers_follow_up_check() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let saved = uncertain_native_queue_send(&state, "in-flight-native-queue", "thread").await;
        // Hold the first-sight history read open, fire the event inside it,
        // then let Codex show the input only after that read has answered.
        std::fs::write(dir.path().join("delay-history"), "").unwrap();
        let scan = tokio::spawn({
            let (state, saved) = (state.clone(), saved.clone());
            async move { scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !dir.path().join("history-waiting").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        check_native_queue(&state, "project-chat");
        std::fs::remove_file(dir.path().join("delay-history")).unwrap();
        assert!(!matches!(scan.await.unwrap(), ReceiptScanStep::Accepted(_)));
        std::fs::write(dir.path().join("accepted-client"), &saved.client_message_id).unwrap();
        assert!(
            matches!(scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await,
            ReceiptScanStep::Accepted(turn) if turn == "turn")
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: the desktop finishing a turn in the thread checks a queued
    // send's history once; until then Codex history is not read again.
    #[tokio::test]
    async fn codex_native_queue_desktop_turn_completion_triggers_check() {
        let (dir, mut state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let thread = "native-queue-desktop-turn";
        let mut index = crate::codex_activity::tests::Fixture::new_for_thread(thread).await;
        state.codex_home = index.dir.path().to_path_buf();
        crate::desktop_activity::remember_codex_rollout(thread, index.path.to_str().unwrap());
        let saved = uncertain_native_queue_send(&state, "desktop-turn-native-queue", thread).await;
        let reads = || fork_requests(dir.path(), "requests-jsonl", "thread/items/list").len();
        let scan = || scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO);
        assert!(!matches!(scan().await, ReceiptScanStep::Accepted(_)));
        let first = reads();
        assert!(first > 0, "first sight checks history");
        std::fs::write(dir.path().join("accepted-client"), &saved.client_message_id).unwrap();
        for _ in 0..3 {
            assert!(!matches!(scan().await, ReceiptScanStep::Accepted(_)));
        }
        assert_eq!(reads(), first, "no reads while the desktop turn runs");
        index.finish("completed").await;
        assert!(matches!(scan().await, ReceiptScanStep::Accepted(turn) if turn == "turn"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a check that spans pages stays pending until its sweep ends;
    // an event mid-sweep restarts it at the newest page, where a newly
    // accepted input lands; reaching the page limit ends the check for good.
    #[tokio::test]
    async fn codex_native_queue_check_spans_pages_and_stops_at_the_limit() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let saved = uncertain_native_queue_send(&state, "paged-native-queue", "thread").await;
        std::fs::write(dir.path().join("long-history"), "").unwrap();
        let cursors = || {
            fork_requests(dir.path(), "requests-jsonl", "thread/items/list")
                .iter()
                .map(|request| request["params"]["cursor"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let scan = || scan_ambiguous_receipt(&state, &saved, 3, 1, Duration::ZERO);
        scan().await;
        scan().await;
        check_native_queue(&state, "project-chat");
        for _ in 0..5 {
            scan().await;
        }
        assert_eq!(
            cursors(),
            [
                None,
                Some("1".into()),
                None,
                Some("1".into()),
                Some("2".into())
            ],
            "the event restarts the sweep, which ends at the page limit"
        );
        check_native_queue(&state, "project-chat");
        scan().await;
        assert_eq!(cursors().len(), 6, "only a new event checks again");
        assert_eq!(cursors()[5], None);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a failed history read ends that check; Codex history is not
    // read again until the next definite event (here hold expiry), and once
    // after it.
    #[tokio::test]
    async fn codex_native_queue_failed_read_waits_for_the_next_event() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        state.projects.shutdown().await;
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = "elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/items/list':\n        print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'History unavailable'}}),flush=True)\n        continue\n        if os.path.exists(root + '/long-history'):";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();
        let saved = uncertain_native_queue_send(&state, "failed-read-native-queue", "thread").await;
        let reads = || fork_requests(dir.path(), "requests-jsonl", "thread/items/list").len();
        let expired_ms = Utc::now().timestamp_millis() + 601_000;
        let scan =
            |now_ms| scan_ambiguous_receipt_at(&state, &saved, 100, 20, Duration::ZERO, now_ms);
        scan(Utc::now().timestamp_millis()).await;
        assert_eq!(reads(), 1, "first sight reads once");
        for _ in 0..3 {
            scan(Utc::now().timestamp_millis()).await;
        }
        assert_eq!(reads(), 1, "a failed read is not retried on later ticks");
        assert!(matches!(
            scan(expired_ms).await,
            ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
        ));
        assert_eq!(reads(), 2, "one check when the hold expires");
        for _ in 0..3 {
            assert!(matches!(
                scan(expired_ms + 60_000).await,
                ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
            ));
        }
        assert_eq!(reads(), 2, "nothing after expiry");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: the desktop finishing a turn still triggers one check when its
    // activity was unreadable before (the rollout became known only later).
    #[tokio::test]
    async fn codex_native_queue_completion_after_unknown_activity_triggers_check() {
        let (dir, mut state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let thread = "native-queue-unknown-activity";
        let mut index = crate::codex_activity::tests::Fixture::new_for_thread(thread).await;
        state.codex_home = index.dir.path().to_path_buf();
        let saved =
            uncertain_native_queue_send(&state, "unknown-activity-native-queue", thread).await;
        let scan = || scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO);
        assert!(!matches!(scan().await, ReceiptScanStep::Accepted(_)));
        std::fs::write(dir.path().join("accepted-client"), &saved.client_message_id).unwrap();
        index.finish("completed").await;
        assert!(!matches!(scan().await, ReceiptScanStep::Accepted(_)));
        crate::desktop_activity::remember_codex_rollout(thread, index.path.to_str().unwrap());
        assert!(matches!(scan().await, ReceiptScanStep::Accepted(turn) if turn == "turn"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: the expiry outcome keeps the honest native-queue notice even
    // when history is too long to finish; it never invites a resend or
    // promises that Wonder will retry.
    #[tokio::test]
    async fn codex_native_queue_expiry_at_page_limit_keeps_the_honest_notice() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let saved = uncertain_native_queue_send(&state, "limit-native-queue", "thread").await;
        std::fs::write(dir.path().join("long-history"), "").unwrap();
        let expired_ms = Utc::now().timestamp_millis() + 601_000;
        for _ in 0..3 {
            assert!(matches!(
                scan_ambiguous_receipt_at(&state, &saved, 2, 2, Duration::ZERO, expired_ms).await,
                ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
            ));
        }
        assert_eq!(
            fork_requests(dir.path(), "requests-jsonl", "thread/items/list").len(),
            2,
            "one expiry check, stopped at the page limit"
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: an expired send shows the honest notice even when the Codex
    // runtime cannot start for its expiry check, and that failed check ends:
    // a runtime that starts later does not read history without an event.
    #[tokio::test]
    async fn codex_native_queue_expiry_without_runtime_keeps_the_honest_notice() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let saved = uncertain_native_queue_send(&state, "no-runtime-native-queue", "thread").await;
        assert!(!matches!(
            scan_ambiguous_receipt(&state, &saved, 100, 20, Duration::ZERO).await,
            ReceiptScanStep::Accepted(_)
        ));
        state.projects.shutdown().await;
        let (codex, hidden) = (dir.path().join("codex"), dir.path().join("codex-hidden"));
        std::fs::rename(&codex, &hidden).unwrap();
        let expired_ms = Utc::now().timestamp_millis() + 601_000;
        let scan =
            || scan_ambiguous_receipt_at(&state, &saved, 100, 20, Duration::ZERO, expired_ms);
        assert!(matches!(
            scan().await,
            ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
        ));
        std::fs::rename(&hidden, &codex).unwrap();
        std::fs::write(dir.path().join("accepted-client"), &saved.client_message_id).unwrap();
        let reads = fork_requests(dir.path(), "requests-jsonl", "thread/items/list").len();
        assert!(matches!(
            scan().await,
            ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
        ));
        assert_eq!(
            fork_requests(dir.path(), "requests-jsonl", "thread/items/list").len(),
            reads,
            "no history read without a new event"
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: an unresolved native enqueue cannot retain an otherwise idle
    // runtime forever, including when a replaced binary needs to be released.
    #[tokio::test]
    async fn codex_native_queue_expired_intent_releases_idle_runtime() {
        for (stale, turn) in [
            (false, None),
            (true, None),
            (false, Some("turn")),
            (true, Some("turn")),
        ] {
            let (dir, state, message) = handoff_fixture().await;
            state
                .store
                .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
                .await
                .unwrap();
            let MessageInsert::Inserted(queued) = state
                .store
                .insert_dispatch_message(
                    "owner",
                    "expired-native-queue",
                    "next",
                    "hash",
                    "project-chat",
                    &[],
                    "now",
                    true,
                )
                .await
                .unwrap()
            else {
                panic!("queued receipt");
            };
            assert!(state
                .store
                .claim_message_for_dispatch(&queued.id)
                .await
                .unwrap());
            let enqueued_at = Utc::now();
            // Fake wall clock: advance to the last retained millisecond and
            // the exact deadline without sleeping or rewriting the journal.
            let deadline_ms = enqueued_at.timestamp_millis() + 600_000;
            let context = json!({
                "deliveryMethod":"thread/queue/add",
                "nativeQueueEnqueuedAt": enqueued_at.to_rfc3339(),
            });
            state
                .store
                .begin_dispatch_submission_with_context(
                    &queued.id,
                    "thread",
                    Some(&context.to_string()),
                )
                .await
                .unwrap();
            state
                .store
                .update_message_delivery(&queued.id, "uncertain", Some("thread"), turn)
                .await
                .unwrap();
            state.store.recover_dispatch_claims().await.unwrap();
            assert!(!release_codex_runtime_when_idle_at(&state, stale, deadline_ms - 1).await);
            assert!(!writer_available(dir.path()));
            // Expiry must not bypass real work, readers or goals. The existing
            // handoff owner covers the latter two; exercise native work here.
            std::fs::write(
                dir.path().join("idle-fixture.json"),
                json!({"thread":{"id":"thread","status":{"type":"active"}}}).to_string(),
            )
            .unwrap();
            assert!(!release_codex_runtime_when_idle_at(&state, stale, deadline_ms).await);
            std::fs::write(
                dir.path().join("idle-fixture.json"),
                json!({"thread":{"id":"thread","status":{"type":"idle"}}}).to_string(),
            )
            .unwrap();
            assert!(
                release_codex_runtime_when_idle_at(&state, stale, deadline_ms).await,
                "expired native enqueue must permit idle release (stale={stale}, turn={turn:?})"
            );
            assert!(writer_available(dir.path()));
            assert!(!state.projects.codex.lock().await.health().is_alive());
            assert_eq!(
                state
                    .projects
                    .notices
                    .lock()
                    .unwrap()
                    .get("project-chat")
                    .map(String::as_str),
                Some(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
            );
            let saved = state
                .store
                .message_by_id(&queued.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.state, "uncertain");
            assert!(!state
                .store
                .requeue_message_for_retry(&queued.id)
                .await
                .unwrap());
            assert_eq!(
                state
                    .store
                    .dispatch_context(&queued.id)
                    .await
                    .unwrap()
                    .unwrap(),
                context.to_string()
            );
            if turn.is_none() {
                // History is read on definite events only: first sight, hold
                // expiry and Wonder taking the thread over; never per tick.
                let reads = || {
                    std::fs::read_to_string(dir.path().join("requests"))
                        .unwrap()
                        .lines()
                        .filter(|method| *method == "thread/items/list")
                        .count()
                };
                let scan = |now_ms| {
                    scan_ambiguous_receipt_at(&state, &saved, 100, 20, Duration::ZERO, now_ms)
                };
                assert!(matches!(
                    scan(deadline_ms - 1).await,
                    ReceiptScanStep::Notice(_)
                ));
                let first = reads();
                assert!(first > 0);
                for _ in 0..3 {
                    scan(deadline_ms - 1).await;
                }
                assert_eq!(reads(), first, "no rescans inside the hold");
                assert!(matches!(
                    scan(deadline_ms).await,
                    ReceiptScanStep::Notice(CODEX_NATIVE_QUEUE_UNRESOLVED_NOTICE)
                ));
                let expired = reads();
                assert!(expired > first, "one check when the hold expires");
                std::fs::write(
                    dir.path().join("accepted-client"),
                    &queued.client_message_id,
                )
                .unwrap();
                for _ in 0..3 {
                    assert!(matches!(
                        scan(deadline_ms + 60_000).await,
                        ReceiptScanStep::Pending | ReceiptScanStep::Notice(_)
                    ));
                }
                assert_eq!(reads(), expired, "no rescans after expiry");
            }
            let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
            assert!(!requests
                .lines()
                .any(|method| matches!(method, "thread/queue/add" | "turn/start")));
            state.projects.shutdown().await;
        }
    }

    #[tokio::test]
    async fn idle_runtime_probe_allows_update_and_does_not_shutdown_during_lease() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        state.projects.shutdown().await;
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original =
            "elif method == 'thread/loaded/list': result = {'data':list(idle), 'nextCursor':None}";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/loaded/list':\n        if os.path.exists(root + '/delay-loaded') and not os.path.exists(root + '/loaded-waiting'):\n            open(root + '/loaded-waiting','w').close()\n            def delayed_loaded(request_id):\n                while os.path.exists(root + '/delay-loaded'): time.sleep(0.01)\n                print(json.dumps({'id':request_id,'result':{'data':list(idle),'nextCursor':None}}),flush=True)\n            threading.Thread(target=delayed_loaded,args=(r['id'],),daemon=True).start()\n            continue\n        result = {'data':list(idle), 'nextCursor':None}";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        drop(rpc);
        std::fs::write(dir.path().join("delay-loaded"), "").unwrap();
        let recovery_state = state.clone();
        let recovering = tokio::spawn(async move { recover(&recovery_state).await });
        tokio::time::timeout(Duration::from_secs(60), async {
            while !dir.path().join("loaded-waiting").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let update_state = state.clone();
        let preparing = tokio::spawn(async move {
            update_state
                .update_admission
                .prepare_update("idle-probe-update", &update_state)
                .await
        });
        let admitted = tokio::time::timeout(Duration::from_secs(60), async {
            while !state.update_admission.work_paused() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        std::fs::remove_file(dir.path().join("delay-loaded")).unwrap();
        if admitted.is_err() {
            preparing.abort();
            recovering.abort();
            panic!("Idle Project lifecycle read blocked update admission");
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), preparing)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        tokio::time::timeout(Duration::from_secs(60), recovering)
            .await
            .unwrap()
            .unwrap();
        assert!(state.projects.codex.lock().await.health().is_alive());
        assert!(!writer_available(dir.path()));
        assert!(
            state
                .update_admission
                .cancel_lease("idle-probe-update")
                .await
        );
        release_idle_codex_runtime(&state).await;
        assert!(writer_available(dir.path()));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_runtime_probe_does_not_close_a_newly_loaded_thread() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        state.projects.shutdown().await;
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original = "elif method == 'thread/backgroundTerminals/list':\n        background =";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/backgroundTerminals/list':\n        if os.path.exists(root + '/delay-background') and not os.path.exists(root + '/background-waiting'):\n            open(root + '/background-waiting','w').close()\n            def delayed_background(request_id):\n                while os.path.exists(root + '/delay-background'): time.sleep(0.01)\n                print(json.dumps({'id':request_id,'result':{'data':[],'nextCursor':None}}),flush=True)\n            threading.Thread(target=delayed_background,args=(r['id'],),daemon=True).start()\n            continue\n        background =";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        drop(rpc);
        std::fs::write(dir.path().join("delay-background"), "").unwrap();
        let probe_state = state.clone();
        let probing = tokio::spawn(async move { release_idle_codex_runtime(&probe_state).await });
        tokio::time::timeout(Duration::from_secs(60), async {
            while !dir.path().join("background-waiting").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({"thread":{"id":"thread","status":{"type":"idle"}},
                "new-thread":{"id":"new-thread","status":{"type":"idle"}}})
            .to_string(),
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("delay-background")).unwrap();
        tokio::time::timeout(Duration::from_secs(60), probing)
            .await
            .unwrap()
            .unwrap();
        assert!(state.projects.codex.lock().await.health().is_alive());
        assert!(!writer_available(dir.path()));
        release_idle_codex_runtime(&state).await;
        assert!(writer_available(dir.path()));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stalled_final_idle_roster_releases_update_admission() {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        state.projects.shutdown().await;
        let script_path = dir.path().join("runtime.py");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let original =
            "elif method == 'thread/loaded/list': result = {'data':list(idle), 'nextCursor':None}";
        assert!(script.contains(original));
        let replacement = "elif method == 'thread/loaded/list':\n        count_path = root + '/loaded-count'\n        count = int(open(count_path).read()) if os.path.exists(count_path) else 0\n        count += 1\n        with open(count_path,'w') as saved: saved.write(str(count))\n        if count == 2 and os.path.exists(root + '/delay-final-loaded'):\n            open(root + '/final-loaded-waiting','w').close()\n            def delayed_final(request_id):\n                while os.path.exists(root + '/delay-final-loaded'): time.sleep(0.01)\n                print(json.dumps({'id':request_id,'result':{'data':list(idle),'nextCursor':None}}),flush=True)\n            threading.Thread(target=delayed_final,args=(r['id'],),daemon=True).start()\n            continue\n        result = {'data':list(idle), 'nextCursor':None}";
        std::fs::write(&script_path, script.replacen(original, replacement, 1)).unwrap();
        let rpc = codex_rpc(&state).await.unwrap();
        drop(rpc);
        std::fs::write(dir.path().join("delay-final-loaded"), "").unwrap();
        let probe_state = state.clone();
        let probing = tokio::spawn(async move { release_idle_codex_runtime(&probe_state).await });
        tokio::time::timeout(Duration::from_secs(60), async {
            while !dir.path().join("final-loaded-waiting").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let update_state = state.clone();
        let preparing = tokio::spawn(async move {
            update_state
                .update_admission
                .prepare_update("stalled-final-roster", &update_state)
                .await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), preparing)
                .await
                .expect("final roster kept update admission beyond its short timeout")
                .unwrap(),
            Ok(())
        );
        std::fs::remove_file(dir.path().join("delay-final-loaded")).unwrap();
        tokio::time::timeout(Duration::from_secs(60), probing)
            .await
            .unwrap()
            .unwrap();
        assert!(state.projects.codex.lock().await.health().is_alive());
        assert!(!writer_available(dir.path()));
        assert!(
            state
                .update_admission
                .cancel_lease("stalled-final-roster")
                .await
        );
        release_idle_codex_runtime(&state).await;
        assert!(writer_available(dir.path()));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: generated commands keep paths and IDs as single arguments even
    // with spaces or quotes; nothing can inject a second shell command.
    #[test]
    fn continuation_arguments_are_shell_quoted() {
        assert_eq!(shell_quote("01a0-ff"), "01a0-ff");
        assert_eq!(shell_quote("/Users/owner/My App"), "'/Users/owner/My App'");
        assert_eq!(shell_quote("it's; rm -rf ~"), "'it'\\''s; rm -rf ~'");
        assert_eq!(shell_quote(""), "''");
    }

    // Contract: a provider without a Terminal resume answers with a finished,
    // explained empty result, so the phone stops loading and says why instead
    // of offering a command that cannot work. OpenCode's option keeps its id.
    #[tokio::test]
    async fn continuation_explains_providers_without_a_terminal_resume() {
        let (dir, state, _) = handoff_fixture().await;
        let cwd = dir.path().join("project-source");
        for (id, family, native) in [
            ("cursor-chat", AgentFamily::Cursor, "cursor-session"),
            ("opencode-chat", AgentFamily::OpenCode, "ses_open"),
        ] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: "project",
                    family,
                    provider_store: provider_store(&state, family),
                    native_session_id: Some(native),
                    cwd: cwd.to_str().unwrap(),
                    roots_revision: 1,
                    title: "Work",
                    model: None,
                    effort: None,
                    service_tier: None,
                    access_mode: "workspace",
                    claude_approval: family.provider().default_approval,
                    plan_mode: false,
                    creation_request_id: None,
                    now: "now",
                })
                .await
                .unwrap();
        }
        let read = |id: &'static str| {
            let state = state.clone();
            async move {
                let response =
                    continuation(State(state), Extension(OwnerAuthority), Path(id.into())).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                    .await
                    .unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            }
        };
        let cursor = read("cursor-chat").await;
        crate::tests::validate_http_contract("desktopContinuation", &cursor);
        assert_eq!(cursor["options"], json!([]));
        assert_eq!(cursor["notes"], json!([]));
        assert_eq!(
            cursor["unavailable"],
            "Cursor chats can’t be continued in Terminal yet."
        );
        let opencode = read("opencode-chat").await;
        crate::tests::validate_http_contract("desktopContinuation", &opencode);
        assert_eq!(opencode["options"][0]["id"], "opencode");
        assert!(opencode.get("unavailable").is_none());
    }

    // Contract: only a finished check that found the provider missing hides
    // it from the phone's pickers; signed-out, recovering and unknown do not.
    #[test]
    fn only_a_provider_found_not_installed_is_hidden() {
        assert_eq!(installed_from(Some("notInstalled")), Some(false));
        for shown in ["signedIn", "signedOut", "installed", "unsupported"] {
            assert_eq!(installed_from(Some(shown)), Some(true), "{shown}");
        }
        assert_eq!(installed_from(Some("unavailable")), None);
        assert_eq!(installed_from(None), None);
        let entry = serde_json::to_value(FamilyAvailability {
            family: AgentFamily::Cursor,
            available: false,
            installed: Some(false),
            signed_out: None,
            capabilities: FamilyCapabilities::of(AgentFamily::Cursor),
        })
        .unwrap();
        assert_eq!(entry["installed"], false);
        crate::tests::validate_http_contract(
            "projectsResponse",
            &json!({"projects": [], "families": [entry], "modesVersion": 1, "pinned": []}),
        );
    }

    // Contract: only a finished check that found the account signed out marks
    // the provider, so a new chat does not start there by default.
    #[test]
    fn only_a_provider_found_signed_out_is_marked() {
        assert_eq!(signed_out_from(Some("signedOut")), Some(true));
        for other in ["signedIn", "installed", "notInstalled", "unavailable"] {
            assert_eq!(signed_out_from(Some(other)), None, "{other}");
        }
        assert_eq!(signed_out_from(None), None);
    }

    // Contract: the Projects list publishes each provider's capability record
    // exactly as the shared fixture the app decodes and keeps as its fallback
    // for hosts that predate the record. A changed capability fails here and
    // in the app's decoder test until both sides agree.
    #[test]
    fn published_capabilities_match_the_shared_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/provider-capabilities-v1.json"
        ))
        .unwrap();
        crate::tests::validate_http_contract("projectsResponse", &fixture);
        assert_eq!(
            fixture["capabilitiesVersion"],
            wonder_store::CAPABILITIES_VERSION
        );
        let published: Vec<Value> = AgentFamily::ALL
            .into_iter()
            .map(|family| {
                json!({"family": family, "available": true,
                    "capabilities": FamilyCapabilities::of(family)})
            })
            .collect();
        assert_eq!(fixture["families"], json!(published));
    }

    // Contract: Projects responses match the published HTTP schema, so an
    // older or newer client decodes them or fails loudly in review.
    // Contract: producer creation metadata is affirmative; legacy/missing,
    // Wonder, exec, unknown clients and copied forks remain unbadged.
    #[test]
    fn computer_session_requires_positive_original_producer_metadata() {
        for (metadata, expected) in [
            (
                json!({"source":"vscode","originator":"Codex Desktop","forkedFromId":null}),
                true,
            ),
            (
                json!({"source":"cli","originator":"codex-tui","forkedFromId":null}),
                true,
            ),
            (
                json!({"source":"appServer","originator":"wonder","forkedFromId":null}),
                false,
            ),
            (
                json!({"source":"exec","originator":"codex_exec","forkedFromId":null}),
                false,
            ),
            (
                json!({"source":"vscode","originator":"Codex Desktop","forkedFromId":"source"}),
                false,
            ),
            (
                json!({"source":"vscode","originator":"Codex Desktop"}),
                false,
            ),
            (
                json!({"source":"vscode","originator":"future-client","forkedFromId":null}),
                false,
            ),
            (json!({"source":"vscode","forkedFromId":null}), false),
            (json!({"id":"native","creationRequestId":null}), false),
        ] {
            assert_eq!(codex_computer_session(&metadata), expected, "{metadata}");
        }
    }

    // Contract: one-shot `codex exec` runs are keyed on originator or source;
    // desktop, TUI, Wonder and T3 Code chats stay listed.
    #[test]
    fn codex_one_shot_markers() {
        for (metadata, expected) in [
            (json!({"source":"exec","originator":"codex_exec"}), true),
            (json!({"source":"exec","originator":"T3 Code"}), true),
            (json!({"source":"exec"}), true),
            (
                json!({"source":{"subagent":"x"},"originator":"codex_exec"}),
                true,
            ),
            (
                json!({"source":"vscode","originator":"Codex Desktop"}),
                false,
            ),
            (json!({"source":"cli","originator":"codex-tui"}), false),
            (json!({"source":"vscode","originator":"T3 Code"}), false),
            (json!({"source":"appServer","originator":"wonder"}), false),
        ] {
            assert_eq!(codex_one_shot(&metadata), expected, "{metadata}");
        }
    }

    #[test]
    fn project_responses_match_the_http_contract() {
        let project = StoredProject {
            id: "p1".into(),
            name: "Wonder".into(),
            is_included: true,
            pin_order: Some(1),
            primary_root_id: "r1".into(),
            roots_revision: 2,
            last_family: Some(AgentFamily::Claude),
            created_at: "2026-09-29T00:00:00Z".into(),
            updated_at: "2026-09-29T00:00:00Z".into(),
            last_used_at: None,
            auto_archive: None,
            last_models: Vec::new(),
            claude_trust: Vec::new(),
            roots: vec![wonder_store::StoredProjectRoot {
                id: "r1".into(),
                path: "/work/app".into(),
                canonical_path: "/work/app".into(),
                ordinal: 0,
            }],
        };
        let summary = serde_json::to_value(project_summary(&project, None)).unwrap();
        crate::tests::validate_http_contract("projectSummary", &summary);
        // A folder without a comparable repository sends none, so different
        // unknown Projects are never grouped together.
        assert!(summary.get("repository").is_none());
        let identified = serde_json::to_value(project_summary(
            &project,
            Some(identity::Repository {
                key: "v1:github.com/swaymun/wonder".into(),
                subpath: "apps/ios".into(),
                name: "Wonder".into(),
            }),
        ))
        .unwrap();
        crate::tests::validate_http_contract("projectSummary", &identified);
        assert_eq!(
            identified["repository"],
            json!({"key":"v1:github.com/swaymun/wonder","subpath":"apps/ios","name":"Wonder"})
        );
        let thread = ProjectThreadSummary {
            reference: "codex:01a0".into(),
            conversation_id: None,
            title: "Fix reconnect".into(),
            family: AgentFamily::Codex,
            updated_at: 1,
            is_pinned: false,
            has_unread: false,
            is_working: false,
            current_activity: None,
            needs_input_on_mac: false,
            waiting_on_mac_permission: false,
            is_computer_session: false,
            worktree_label: None,
            branch: None,
            background_work: false,
            is_subagent: false,
            last_automation_at: None,
            automation_run: None,
            resumes_at: None,
            cwd: Some("/private/folder".into()),
        };
        let page = serde_json::to_value(ThreadsPage {
            threads: vec![thread.clone()],
            next_cursor: Some("token".into()),
            partial: vec![PartialFailure {
                family: AgentFamily::Claude,
                detail: "Claude Code threads could not be loaded.".into(),
            }],
            loading: false,
        })
        .unwrap();
        crate::tests::validate_http_contract("projectThreadsPage", &page);
        assert!(page["threads"][0].get("isComputerSession").is_none());
        for flag in [false, true] {
            let mut summary = serde_json::to_value(&thread).unwrap();
            summary["isComputerSession"] = json!(flag);
            crate::tests::validate_http_contract("projectThreadSummary", &summary);
        }
        let computer = serde_json::to_value(ProjectThreadSummary {
            is_computer_session: true,
            ..thread.clone()
        })
        .unwrap();
        assert_eq!(computer["isComputerSession"], true);
        crate::tests::validate_http_contract("projectThreadSummary", &computer);
        // The row fields the Chats list draws (branch, background work, the
        // automation clock and its failure) are part of the contract; the
        // working folder they come from is never sent.
        for absent in [
            "cwd",
            "branch",
            "backgroundWork",
            "lastAutomationAt",
            "automationRun",
        ] {
            assert!(
                page["threads"][0].get(absent).is_none(),
                "{absent} is omitted"
            );
        }
        let decorated = serde_json::to_value(ProjectThreadSummary {
            branch: Some("agent/fixture".into()),
            background_work: true,
            is_subagent: false,
            last_automation_at: Some("2026-10-20T08:00:00.000Z".into()),
            automation_run: Some(AutomationRunMark {
                automation_id: "automation".into(),
                name: None,
                status: "failed".into(),
                started_at: "2026-10-20T08:00:00.000Z".into(),
                finished_at: Some("2026-10-20T08:00:01.000Z".into()),
                error: Some(
                    "Cupola needs an active paired owner device to run this automation.".into(),
                ),
            }),
            ..thread.clone()
        })
        .unwrap();
        crate::tests::validate_http_contract("projectThreadSummary", &decorated);
        assert_eq!(decorated["branch"], "agent/fixture");
        assert_eq!(decorated["backgroundWork"], true);
        assert_eq!(decorated["automationRun"]["status"], "failed");
        assert!(decorated.get("cwd").is_none());
        let pinned = ProjectThreadSummary {
            conversation_id: Some("c1".into()),
            is_pinned: true,
            ..thread.clone()
        };
        let library = serde_json::to_value(ProjectsResponse {
            projects: vec![project_summary(&project, None)],
            families: vec![FamilyAvailability {
                family: AgentFamily::Claude,
                available: true,
                installed: None,
                signed_out: None,
                capabilities: FamilyCapabilities::of(AgentFamily::Claude),
            }],
            modes_version: MODES_VERSION,
            skills_version: 1,
            archive_version: 1,
            delete_version: 1,
            create_version: CREATE_VERSION,
            capabilities_version: wonder_store::CAPABILITIES_VERSION,
            terminal_version: crate::terminal::VERSION,
            partial: Vec::new(),
            pinned: vec![PinnedThread {
                project_id: "p1".into(),
                thread: pinned,
            }],
        })
        .unwrap();
        crate::tests::validate_http_contract("projectsResponse", &library);
        assert_eq!(library["modesVersion"], 1);
        let detail = serde_json::to_value(ProjectConversationDetail {
            t3_code_permissions: None,
            conversation_id: "c1".into(),
            project_id: "p1".into(),
            project_name: "Wonder".into(),
            title: "Fix reconnect".into(),
            family: AgentFamily::Claude,
            model: Some("claude:sonnet".into()),
            effort: None,
            service_tier: None,
            access_mode: "workspace".into(),
            claude_approval: "accept_edits".into(),
            plan_mode: true,
            working_folder: "/work/app".into(),
            working_folder_name: "app".into(),
            worktree_label: None,
            is_pinned: false,
            has_unread: false,
            has_native_session: true,
            is_archived: false,
            folder_in_project: true,
            read_only_reason: None,
            notice: None,
            resumes_at: None,
        })
        .unwrap();
        crate::tests::validate_http_contract("projectConversationDetail", &detail);
        let continuation = serde_json::to_value(ContinuationResponse {
            options: vec![ContinuationOption {
                id: "codex-cli",
                title: "t".into(),
                detail: "d".into(),
                command: "codex resume x".into(),
            }],
            notes: vec![],
            unavailable: None,
        })
        .unwrap();
        crate::tests::validate_http_contract("desktopContinuation", &continuation);
    }

    // Attach owns the HTTP receipt; history owns the later timeline events.
    // Hold the existing real RPC fixture's history read across the receipt,
    // including an exec-source reviewer thread that is absent from discovery.
    #[tokio::test]
    async fn attaching_codex_returns_before_slow_history_and_streams_it() {
        use crate::test_support::call;
        for source in ["cli", "exec"] {
            let (dir, state, _) = handoff_fixture().await;
            let native = uuid::Uuid::new_v4().to_string();
            std::fs::write(
                dir.path().join("native-thread.json"),
                json!({"id":native,"cwd":dir.path().join("project-source"),
                    "projectId":"owner-project","source":source,
                    "originator":"codex-tui","forkedFromId":null,
                    "preview":"Reviewer history","status":{"type":"idle"}})
                .to_string(),
            )
            .unwrap();
            std::fs::write(dir.path().join("delay-history"), "").unwrap();
            let mut events = state.events.subscribe();
            let started = Instant::now();
            let (status, receipt) = tokio::time::timeout(
                Duration::from_secs(10),
                call(
                    &state,
                    "POST",
                    "/api/v1/projects/project/threads/attach",
                    json!({"reference":format!("codex:{native}")}),
                ),
            )
            .await
            .expect("attach must return while history is held");
            let receipt_ms = started.elapsed().as_millis();
            // Product latency contract, separate from the generous hang guard.
            assert!(receipt_ms < 1000, "{source} attach took {receipt_ms} ms");
            assert_eq!(status, StatusCode::OK, "{receipt}");
            crate::tests::validate_http_contract("projectThreadSummary", &receipt);
            assert_eq!(
                receipt["isComputerSession"].as_bool().unwrap_or(false),
                source == "cli"
            );
            let conversation = receipt["conversationId"].as_str().unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while !dir.path().join("history-waiting").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("attach must schedule native history");
            // A second open while history is still pending must reuse the
            // saved conversation and return just as promptly as the first.
            let reopened = Instant::now();
            let (status, repeat) = tokio::time::timeout(
                Duration::from_secs(10),
                call(
                    &state,
                    "POST",
                    "/api/v1/projects/project/threads/attach",
                    json!({"reference":format!("codex:{native}")}),
                ),
            )
            .await
            .expect("reopening must return while history is held");
            let reopen_ms = reopened.elapsed().as_millis();
            assert!(reopen_ms < 1000, "{source} reopen took {reopen_ms} ms");
            assert_eq!(status, StatusCode::OK, "{repeat}");
            crate::tests::validate_http_contract("projectThreadSummary", &repeat);
            assert_eq!(repeat["conversationId"], conversation);
            assert_eq!(repeat["reference"], receipt["reference"]);
            assert_eq!(repeat["isComputerSession"], receipt["isComputerSession"]);
            assert_eq!(
                state
                    .store
                    .history_refresh_status(conversation, crate::now_ms() as i64)
                    .await
                    .unwrap()["state"],
                "refreshing"
            );
            assert!(
                events.try_recv().is_err(),
                "held history must not publish a timeline item"
            );
            // Keep history slow beyond the one-second receipt budget, then
            // release the same request with a real saved timeline item.
            tokio::time::sleep(Duration::from_millis(1100)).await;
            std::fs::write(
                dir.path().join("history-items.json"),
                json!([{"turnId":"turn","item":{"id":"review-reply",
                    "type":"agentMessage","text":"Saved reviewer history"}}])
                .to_string(),
            )
            .unwrap();
            std::fs::remove_file(dir.path().join("delay-history")).unwrap();
            let event = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = events.recv().await.unwrap();
                    if event.conversation_id.as_deref() == Some(conversation)
                        && event.item_id.as_deref() == Some("review-reply")
                    {
                        break event;
                    }
                }
            })
            .await
            .expect("history must arrive by event after the attach receipt");
            assert!(matches!(event.event, wonder_api::WonderEvent::Activity {
                ref category, detail: Some(ref detail), ..
            } if category == "thread_item_upsert" && detail.contains("Saved reviewer history")));
            let history = state
                .store
                .conversation_history_page(&state.host_epoch, conversation, None, 100)
                .await
                .unwrap()
                .unwrap();
            assert!(history
                .events
                .iter()
                .any(|row| row.item_id.as_deref() == Some("review-reply")));
            tokio::time::timeout(Duration::from_secs(10), async {
                while state
                    .store
                    .history_refresh_status(conversation, crate::now_ms() as i64)
                    .await
                    .unwrap()["state"]
                    == "refreshing"
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("history must settle before the fixture shuts down");
            assert_eq!(
                state
                    .store
                    .history_refresh_status(conversation, crate::now_ms() as i64)
                    .await
                    .unwrap()["state"],
                "completed"
            );
            let records: Vec<Value> = std::fs::read_to_string(dir.path().join("logs/test.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let attaches: Vec<_> = records
                .iter()
                .filter(|row| row["event"] == "project_thread_attach")
                .collect();
            assert_eq!(attaches.len(), 2, "both opens must be logged: {records:?}");
            assert!(
                attaches.iter().all(|row| row["status"] == 200
                    && row["conversationId"] == conversation
                    && row["elapsedMs"].is_number()
                    && row["error"].is_null()),
                "every attach needs a diagnostic receipt: {records:?}"
            );
            eprintln!(
                "ATTACH_MEASUREMENT source={source} receipt_ms={receipt_ms} reopen_ms={reopen_ms} history_event_ms={}",
                started.elapsed().as_millis()
            );
            let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
            assert!(
                !requests
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .any(|request| matches!(
                        request["method"].as_str(),
                        Some("thread/start" | "thread/resume" | "turn/start")
                    )),
                "attaching history starts no model work"
            );
            state.projects.shutdown().await;
        }
    }

    // Exercise failures through the same HTTP route the phone opens. An
    // existing row reaches association without repeating the initial lookup.
    #[tokio::test]
    async fn attaching_codex_reports_specific_failures_and_logs_each_attempt() {
        use crate::test_support::call;
        let (dir, state, _) = handoff_fixture().await;
        let native = uuid::Uuid::new_v4().to_string();
        let path = "/api/v1/projects/project/threads/attach";
        let request = json!({"reference":format!("codex:{native}")});
        let read_error = dir.path().join("native-read-error.json");
        std::fs::write(
            &read_error,
            json!({"code":-32000,"message":"fixture read failure"}).to_string(),
        )
        .unwrap();
        let (status, body) = call(&state, "POST", path, request.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body["error"],
            "This Codex thread is no longer available on your computer."
        );
        std::fs::remove_file(&read_error).unwrap();
        std::fs::write(dir.path().join("native-thread.json"),
            json!({"id":native,"cwd":dir.path().join("project-source"),"projectId":null,"preview":"Review"}).to_string()).unwrap();
        // The owner's outside-folder refusal now opens history without admission.
        let outside = dir.path().join("project-source-sibling");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(
            dir.path().join("native-thread.json"),
            json!({"id":native,"cwd":outside,"projectId":null,"preview":"Review outside"})
                .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("history-items.json"), json!({
            "": {"data": [
                {"turnId":"outside-turn","item":{"id":"answer","type":"agentMessage","text":"Outside history remains readable"}},
                {"turnId":"outside-turn","item":{"id":"question","type":"userMessage","content":[{"type":"inputText","text":"Review outside"}]}}
            ], "nextCursor":"older+page"},
            "older+page": {"data":[
                {"turnId":"earlier-turn","item":{"id":"picture","type":"imageGeneration","status":"completed","result":"aGVsbG8="}},
                {"turnId":"earlier-turn","item":{"id":"thinking","type":"reasoning","status":"completed",
                    "summary":[{"type":"summary_text","text":"Owned finished thinking summary"}],
                    "content":[{"text":"Never export private reasoning"}]}},
                {"turnId":"earlier-turn","item":{"id":"earlier","type":"agentMessage","text":"Earlier outside history"}}
            ], "nextCursor":null}
        }).to_string()).unwrap();
        let count = state
            .store
            .project_conversations("project")
            .await
            .unwrap()
            .len();
        let before_project = state.store.project("project").await.unwrap().unwrap();
        let before_requests = std::fs::read_to_string(dir.path().join("requests"))
            .unwrap_or_default()
            .lines()
            .count();
        let (status, body) = call(&state, "POST", path, request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        crate::tests::validate_http_contract("projectThreadSummary", &body);
        let id = body["conversationId"].as_str().unwrap();
        // The phone's chat-link parser accepts at most 128 ASCII ID bytes.
        // Real Project and Codex IDs are full UUIDs, unlike this short fixture.
        let link_id = outside_history_id(
            &uuid::Uuid::new_v4().to_string(),
            &format!("codex:{native}"),
        );
        assert!(
            link_id.len() <= 128
                && link_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        );
        let (status, detail) = call(
            &state,
            "GET",
            &format!("/api/v1/project-conversations/{id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        crate::tests::validate_http_contract("projectConversationDetail", &detail);
        assert_eq!(detail["readOnlyReason"], OUTSIDE_HISTORY_REASON);
        assert_eq!(detail["folderInProject"], false);
        let (status, snapshot) = call(
            &state,
            "GET",
            &format!("/api/v1/conversations/{id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{snapshot}");
        assert_eq!(snapshot["conversationId"], id);
        assert_eq!(
            snapshot["thread"]["turns"][0]["items"][0]["text"],
            "Review outside"
        );
        assert_eq!(
            snapshot["thread"]["turns"][0]["items"][1]["text"],
            "Outside history remains readable"
        );
        let cursor = snapshot["thread"]["nextCursor"].as_str().unwrap();
        let (status, older) = call(
            &state,
            "GET",
            &format!("/api/v1/conversations/{id}/history?before={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{older}");
        assert_eq!(
            older["thread"]["turns"][0]["items"][0]["text"],
            "Earlier outside history"
        );
        assert!(older["thread"]["nextCursor"].is_null());
        assert!(!older
            .to_string()
            .contains("Owned finished thinking summary"));
        assert!(!older.to_string().contains("Never export private reasoning"));
        assert!(older
            .to_string()
            .contains("Cupola can't keep previews for this thread"));
        assert!(!older.to_string().contains("aGVsbG8="));
        assert!(state
            .store
            .list_conversation_files(id)
            .await
            .unwrap()
            .is_empty());
        state
            .store
            .set_agent_chat_detail(
                AgentFamily::Codex,
                wonder_store::ChatDetail::Thinking,
                "now",
            )
            .await
            .unwrap();
        let (status, thinking) = call(
            &state,
            "GET",
            &format!("/api/v1/conversations/{id}/history?before={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(thinking
            .to_string()
            .contains("Owned finished thinking summary"));
        assert!(!thinking
            .to_string()
            .contains("Never export private reasoning"));

        let wrong_cursor = hex::encode(serde_json::to_vec(&("another-chat", "older")).unwrap());
        let (status, _) = call(
            &state,
            "GET",
            &format!("/api/v1/conversations/{id}/history?before={wrong_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a cursor cannot cross conversations"
        );
        let (status, _) = call(
            &state,
            "PATCH",
            &format!("/api/v1/project-conversations/{id}"),
            json!({"accessMode":"full_access"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a history address cannot become writable"
        );
        // This read-only fixture has no ingestion consumer. Exercise the
        // actual Send owner beyond the router's unrelated startup gate.
        let response = crate::send_message_inner(
            State(state.clone()),
            Path(id.to_owned()),
            None,
            Json(
                serde_json::from_value(json!({"deviceId":"owner",
                "clientMessageId":uuid::Uuid::new_v4().to_string(),
                "body":"This must never be sent", "attachmentIds":[]}))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "history cannot accept a message"
        );
        assert_eq!(
            state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            count
        );
        assert!(state
            .store
            .project_conversation_by_native(
                AgentFamily::Codex,
                provider_store(&state, AgentFamily::Codex),
                &native
            )
            .await
            .unwrap()
            .is_none());
        let after_project = state.store.project("project").await.unwrap().unwrap();
        assert_eq!(before_project.roots_revision, after_project.roots_revision);
        assert_eq!(root_paths(&before_project), root_paths(&after_project));
        let native_file: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("native-thread.json")).unwrap())
                .unwrap();
        assert!(
            native_file["projectId"].is_null(),
            "the desktop thread is not associated"
        );
        let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert!(
            requests
                .lines()
                .skip(before_requests)
                .all(|method| matches!(method, "thread/read" | "thread/items/list")),
            "history must never resume, attach, associate or start a turn: {requests}"
        );
        std::fs::remove_file(dir.path().join("history-items.json")).unwrap();
        std::fs::write(dir.path().join("native-thread.json"),
            json!({"id":native,"cwd":dir.path().join("project-source"),"projectId":null,"preview":"Review"}).to_string()).unwrap();
        let project = state.store.project("project").await.unwrap().unwrap();
        let attached = attach_native(&state, &project, AgentFamily::Codex, &native)
            .await
            .unwrap();
        std::fs::write(
            &read_error,
            json!({"code":-32000,"message":"fixture association read failure"}).to_string(),
        )
        .unwrap();
        let (status, body) = call(&state, "POST", path, request.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["error"],
            "This Codex thread could not be checked on your computer."
        );
        std::fs::remove_file(&read_error).unwrap();
        std::fs::write(
            dir.path().join("native-metadata-error.json"),
            json!({"code":-32000,"message":"fixture save failure"}).to_string(),
        )
        .unwrap();
        let (status, body) = call(&state, "POST", path, request.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["error"],
            "This thread's desktop project could not be saved. Try opening it again."
        );
        let (status, body) = call(
            &state,
            "POST",
            "/api/v1/projects/missing/threads/attach",
            request,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "This chat's project is no longer available.");
        let (status, body) = call(&state, "POST", path, json!({"reference":"invalid"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Invalid thread reference.");
        let records: Vec<Value> = std::fs::read_to_string(dir.path().join("logs/test.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let attempts: Vec<_> = records
            .iter()
            .filter(|row| row["event"] == "project_thread_attach")
            .collect();
        assert_eq!(
            attempts.len(),
            6,
            "every accepted or refused attach is logged"
        );
        assert_eq!(
            attempts
                .iter()
                .map(|row| row["status"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [404, 200, 503, 503, 404, 400]
        );
        assert!(attempts.iter().all(|row| row["elapsedMs"].is_number()));
        assert_eq!(attempts[1]["readOnly"], true);
        assert_eq!(
            state
                .store
                .project_conversation(&attached.conversation_id)
                .await
                .unwrap()
                .unwrap()
                .native_session_id
                .as_deref(),
            Some(native.as_str())
        );
        state.projects.shutdown().await;
    }

    // Contract: opening an imported native thread repairs its missing desktop
    // project without starting a turn, retargeting an assigned thread, or using
    // a sibling folder. The existing real transport fixture owns this boundary.
    #[tokio::test]
    async fn native_project_association_repairs_only_unassigned_member_threads() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        let project = state.store.project("project").await.unwrap().unwrap();
        state.projects = ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let saved = dir.path().join("native-thread.json");
        let write = |cwd: &FsPath, project: Value| {
            std::fs::write(
                &saved,
                json!({"id":"native-thread", "cwd":cwd, "projectId":project}).to_string(),
            )
            .unwrap();
        };
        write(&source, Value::Null);
        associate_codex_project(&state, &project, "native-thread")
            .await
            .unwrap();
        let assigned: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert_eq!(assigned["projectId"], "native-project");
        // Reopening is idempotent; an explicit desktop choice wins.
        associate_codex_project(&state, &project, "native-thread")
            .await
            .unwrap();
        write(&source, json!("owner-project"));
        associate_codex_project(&state, &project, "native-thread")
            .await
            .unwrap();
        let assigned: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert_eq!(assigned["projectId"], "owner-project");

        // A cached provider reference may now describe another folder set.
        // Replace it only after finding an exact current-folder match.
        let listed = dir.path().join("native-projects.json");
        std::fs::write(
            &listed,
            json!([
                {"id":"native-project","roots":[{"path":dir.path().join("old-source")}]},
                {"id":"current-native-project","roots":[{"path":source}]}
            ])
            .to_string(),
        )
        .unwrap();
        write(&source, Value::Null);
        associate_codex_project(&state, &project, "native-thread")
            .await
            .unwrap();
        let assigned: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert_eq!(assigned["projectId"], "current-native-project");
        assert_eq!(
            state
                .store
                .project_provider_ref(&project.id, AgentFamily::Codex, &state.projects.codex_store)
                .await
                .unwrap()
                .as_deref(),
            Some("current-native-project")
        );

        // If no desktop project still owns these roots, creation must not
        // reuse the old folder set through its original idempotency key.
        std::fs::write(
            &listed,
            json!([
                {"id":"native-project","roots":[{"path":dir.path().join("old-source")}]}
            ])
            .to_string(),
        )
        .unwrap();
        write(&source, Value::Null);
        associate_codex_project(&state, &project, "native-thread")
            .await
            .unwrap();
        let assigned: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert_eq!(assigned["projectId"], "created-native-project");
        std::fs::write(
            &listed,
            json!([
                {"id":"created-native-project","roots":[{"path":source}]}
            ])
            .to_string(),
        )
        .unwrap();

        // Only the protocol's optional-method code is a compatibility fallback.
        // A supported persistence error must fail even with similar wording.
        let rpc_error = dir.path().join("native-metadata-error.json");
        std::fs::write(
            &rpc_error,
            json!({"code":-32601,"message":"feature unavailable"}).to_string(),
        )
        .unwrap();
        write(&source, Value::Null);
        associate_codex_project(&state, &project, "native-thread")
            .await
            .unwrap();
        let unassigned: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert!(unassigned["projectId"].is_null());
        std::fs::write(
            &rpc_error,
            json!({"code":-32000,"message":"method not found while saving project metadata"})
                .to_string(),
        )
        .unwrap();
        assert!(associate_codex_project(&state, &project, "native-thread")
            .await
            .is_err());
        write(&dir.path().join("project-source-sibling"), Value::Null);
        assert!(associate_codex_project(&state, &project, "native-thread")
            .await
            .is_err());
        let requests: Vec<Value> = std::fs::read_to_string(dir.path().join("requests-jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let updates: Vec<_> = requests
            .iter()
            .filter(|r| r["method"] == "thread/metadata/update")
            .collect();
        assert_eq!(updates.len(), 5);
        assert_eq!(
            updates[0]["params"],
            json!({"threadId":"native-thread", "projectId":"native-project"})
        );
        let created = requests
            .iter()
            .find(|r| r["method"] == "project/create")
            .unwrap();
        assert_ne!(created["params"]["idempotencyKey"], "wonder-project");
        assert_eq!(created["params"]["roots"], json!([{"path":source}]));
        assert!(!requests.iter().any(|r| matches!(
            r["method"].as_str(),
            Some("thread/start" | "thread/resume" | "turn/start")
        )));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// A Project's `lastModels` as the phone reads it from the Projects list.
    async fn project_last_models(state: &AppState, id: &str) -> Value {
        let (status, listed) =
            crate::test_support::call(state, "GET", "/api/v1/projects", json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let project = listed["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()
            .clone();
        crate::test_support::assert_http_contract("projectSummary", &project);
        project["lastModels"].clone()
    }

    // First-send preparation owns metadata and request recovery only. Native
    // sessions and messages must remain absent until the durable Send arrives.
    #[tokio::test]
    async fn create_stores_the_resolved_effort_and_speed() {
        use crate::test_support::{call, fixture};
        let (dir, state) = fixture().await;
        {
            let mut catalog = state.runtime_catalog.write().await;
            let mut effortful = catalog.models[0].clone();
            effortful.id = "effortful".into();
            effortful.reasoning_efforts = vec![
                crate::ChoiceOption {
                    id: "low".into(),
                    label: "Low".into(),
                    description: None,
                },
                crate::ChoiceOption {
                    id: "high".into(),
                    label: "High".into(),
                    description: None,
                },
            ];
            effortful.default_reasoning_effort = None;
            catalog.models.push(effortful);
        }
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                "defaults-project",
                "request",
                "hash",
                "Test",
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        let _service = crate::ingestion::spawn(state.clone()).await;
        for _ in 0..6000 {
            if state.ingestion.project_readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let request = json!({"accountId":"default","deviceId":"owner", "clientMessageId":uuid::Uuid::new_v4().to_string(),
            "family":"codex", "model":"effortful", "body":"Hello", "rootsRevision":1, "prepareOnly":true});
        crate::tests::validate_http_contract("createProjectThreadRequest", &request);
        let authority: Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .unwrap();
        let schema = jsonschema::validator_for(
            &json!({"$ref":"#/$defs/createProjectThreadRequest", "$defs":authority["$defs"]}),
        )
        .unwrap();
        let mut legacy = request.clone();
        legacy["accountId"] = json!("added-account");
        assert!(!schema.is_valid(&legacy));
        let refused = call(
            &state,
            "POST",
            "/api/v1/projects/defaults-project/threads",
            legacy,
        )
        .await;
        assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY);
        let mut unknown = request.clone();
        unknown["inventedAccount"] = json!(true);
        assert!(!schema.is_valid(&unknown));
        let refused = call(
            &state,
            "POST",
            "/api/v1/projects/defaults-project/threads",
            unknown,
        )
        .await;
        assert_eq!(
            refused.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "deny_unknown_fields stays enabled"
        );
        let created = call(
            &state,
            "POST",
            "/api/v1/projects/defaults-project/threads",
            request,
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{}", created.1);
        let id = created.1["conversation"]["conversationId"]
            .as_str()
            .unwrap();
        let stored = state.store.project_conversation(id).await.unwrap().unwrap();
        assert_eq!(
            stored.effort.as_deref(),
            Some("low"),
            "Omitted effort is stored resolved to the first listed"
        );
        assert_eq!(stored.service_tier.as_deref(), Some("default"));
        assert_eq!(stored.provider_store, state.projects.codex_store);
        state.projects.shutdown().await;

        // The thread's model and effort become the Project's last choice for
        // Codex, which every device's next new chat starts from.
        assert_eq!(
            project_last_models(&state, "defaults-project").await,
            json!([{"family": "codex", "model": "effortful", "effort": "low"}])
        );
    }

    #[tokio::test]
    async fn preparation_retries_one_conversation_without_starting_work() {
        use crate::test_support::{call, fixture};
        let (dir, state) = fixture().await;
        {
            let mut catalog = state.runtime_catalog.write().await;
            catalog.models[0].service_tiers = vec![
                crate::ChoiceOption {
                    id: "default".into(),
                    label: "Standard".into(),
                    description: None,
                },
                crate::ChoiceOption {
                    id: "fast".into(),
                    label: "Fast".into(),
                    description: None,
                },
            ];
        }
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                "prepare-project",
                "request",
                "hash",
                "Test",
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        let _service = crate::ingestion::spawn(state.clone()).await;
        for _ in 0..6000 {
            if state.ingestion.project_readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let request = json!({"deviceId":"owner", "clientMessageId":uuid::Uuid::new_v4().to_string(),
            "family":"codex", "model":"fake", "serviceTier":"fast", "body":"First message", "rootsRevision":1, "prepareOnly":true});
        crate::tests::validate_http_contract("createProjectThreadRequest", &request);
        let mut unsupported = request.clone();
        unsupported["clientMessageId"] = json!(uuid::Uuid::new_v4().to_string());
        unsupported["serviceTier"] = json!("priority");
        assert_eq!(
            call(
                &state,
                "POST",
                "/api/v1/projects/prepare-project/threads",
                unsupported
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let first = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            request.clone(),
        )
        .await;
        assert_eq!(first.0, StatusCode::OK, "{}", first.1);
        crate::tests::validate_http_contract("createProjectThreadResponse", &first.1);
        assert!(first.1["receipt"].is_null());
        let second = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            request.clone(),
        )
        .await;
        assert_eq!(second.0, StatusCode::OK, "{}", second.1);
        assert_eq!(
            first.1["conversation"]["conversationId"],
            second.1["conversation"]["conversationId"]
        );
        // A prepared first message must validate annotation source bytes just
        // like an ordinary Project send. The invalid record must never be
        // accepted for dispatch, even with the exact creation request ID.
        let prepared_id = first.1["conversation"]["conversationId"].as_str().unwrap();
        std::fs::write(source.join("note.txt"), b"current source\n").unwrap();
        let annotation = json!({
            "version": 1, "projectId": "prepare-project", "conversationId": prepared_id,
            "rootId": "workspace", "path": "note.txt", "sourceSha256": "0".repeat(64),
            "anchor": {"kind":"textLines", "startLine":1, "endLine":1},
            "note": "Check this source"
        });
        let upload = crate::upload_conversation_file(
            State(state.clone()),
            Path(prepared_id.to_owned()),
            Json(crate::CreateConversationFileRequest {
                client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                name: "annotation.json".into(),
                mime_type: Some(crate::artifact_annotations::MIME.into()),
                content_base64: {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode(annotation.to_string())
                },
            }),
        )
        .await;
        assert_eq!(upload.status(), StatusCode::OK);
        let uploaded: Value = serde_json::from_slice(
            &axum::body::to_bytes(upload.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        let mut invalid_first_send = request.clone();
        invalid_first_send["prepareOnly"] = json!(false);
        invalid_first_send["attachmentIds"] = json!([uploaded["id"]]);
        let rejected = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            invalid_first_send,
        )
        .await;
        assert_eq!(rejected.0, StatusCode::CONFLICT, "{}", rejected.1);
        assert!(state
            .store
            .message_by_device_and_client_message_id(
                "owner",
                request["clientMessageId"].as_str().unwrap(),
            )
            .await
            .unwrap()
            .is_none());
        let original_model = {
            let mut catalog = state.runtime_catalog.write().await;
            let original = catalog.models[0].clone();
            catalog.models[0].hidden = true;
            catalog.models[0]
                .service_tiers
                .retain(|tier| tier.id == "default");
            original
        };
        let recovered = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            request.clone(),
        )
        .await;
        assert_eq!(recovered.0, StatusCode::OK, "{}", recovered.1);
        assert_eq!(
            first.1["conversation"]["conversationId"],
            recovered.1["conversation"]["conversationId"]
        );
        let mut conflicting = request.clone();
        conflicting["serviceTier"] = json!("default");
        let conflict = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            conflicting,
        )
        .await;
        assert_eq!(conflict.0, StatusCode::CONFLICT, "{}", conflict.1);
        state.runtime_catalog.write().await.models[0] = original_model;
        let id = first.1["conversation"]["conversationId"].as_str().unwrap();
        // The HTTP/store boundary preserves omitted effort and clears explicit
        // null when switching to an effort-less model, avoiding a rejected Send.
        let mut effortful = state
            .runtime_catalog
            .read()
            .await
            .models
            .iter()
            .find(|model| model.id == "fake")
            .unwrap()
            .clone();
        effortful.id = "effortful".into();
        effortful.reasoning_efforts = vec![crate::ChoiceOption {
            id: "high".into(),
            label: "High".into(),
            description: None,
        }];
        state.runtime_catalog.write().await.models.push(effortful);
        state
            .store
            .update_project_conversation(
                id,
                ProjectConversationPatch {
                    model: Some("effortful"),
                    effort: Some(Some("high")),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        let path = format!("/api/v1/project-conversations/{id}");
        let preserved = call(&state, "PATCH", &path, json!({"isPinned": false})).await;
        assert_eq!(preserved.0, StatusCode::OK, "{}", preserved.1);
        assert_eq!(preserved.1["effort"], "high");
        // Only the owner's own choices become the Project's last model: the
        // thread's first model, not a change made elsewhere or a pin.
        let first_model = json!([{"family": "codex", "model": "fake"}]);
        assert_eq!(
            project_last_models(&state, "prepare-project").await,
            first_model
        );
        let incompatible = call(&state, "PATCH", &path, json!({"model": "fake"})).await;
        assert_eq!(incompatible.0, StatusCode::OK, "{}", incompatible.1);
        assert!(
            incompatible.1["effort"].is_null(),
            "An effort the new model lacks is replaced by its resolved default (none here)"
        );
        let restore = call(&state, "PATCH", &path, json!({"model": "effortful"})).await;
        assert_eq!(restore.0, StatusCode::OK, "{}", restore.1);
        assert_eq!(
            restore.1["effort"], "high",
            "Switching to a model that offers efforts stores its default"
        );
        // Choosing a model in the thread replaces it; a refused choice keeps it.
        let chosen = json!([{"family": "codex", "model": "effortful", "effort": "high"}]);
        assert_eq!(project_last_models(&state, "prepare-project").await, chosen);
        let named = call(&state, "PATCH", &path, json!({"effort": "bogus"})).await;
        assert_eq!(named.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", named.1);
        assert_eq!(project_last_models(&state, "prepare-project").await, chosen);
        state
            .store
            .update_project_conversation(
                id,
                ProjectConversationPatch {
                    model: Some("effortful"),
                    effort: Some(Some("high")),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        let clear = json!({"model": "fake", "effort": null});
        crate::tests::validate_http_contract("updateProjectConversationRequest", &clear);
        let cleared = call(&state, "PATCH", &path, clear).await;
        assert_eq!(cleared.0, StatusCode::OK, "{}", cleared.1);
        assert!(cleared.1["effort"].is_null());
        assert_eq!(cleared.1["serviceTier"], "fast");
        let mut standard_only = state
            .runtime_catalog
            .read()
            .await
            .models
            .iter()
            .find(|model| model.id == "fake")
            .unwrap()
            .clone();
        standard_only.id = "standard-only".into();
        standard_only
            .service_tiers
            .retain(|tier| tier.id == "default");
        state
            .runtime_catalog
            .write()
            .await
            .models
            .push(standard_only);
        let changed = call(&state, "PATCH", &path, json!({"model":"standard-only"})).await;
        assert_eq!(changed.0, StatusCode::OK, "{}", changed.1);
        assert!(
            changed.1["serviceTier"].is_null(),
            "Model changes clear an unsupported speed"
        );
        let unavailable = call(&state, "PATCH", &path, json!({"serviceTier":"fast"})).await;
        assert_eq!(unavailable.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(state.store.runtime_binding(id).await.unwrap().is_none());
        assert!(state
            .store
            .project_conversation(id)
            .await
            .unwrap()
            .unwrap()
            .native_session_id
            .is_none());
        let mut stale = request;
        stale["rootsRevision"] = json!(2);
        let rejected = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            stale,
        )
        .await;
        assert_eq!(rejected.0, StatusCode::CONFLICT);
        // Plan mode is part of the frozen creation request, Claude approval
        // has no meaning for Codex, and pinned threads join the library.
        let planned = json!({"deviceId":"owner", "clientMessageId":uuid::Uuid::new_v4().to_string(),
            "family":"codex", "model":"fake", "body":"Plan first", "rootsRevision":1, "prepareOnly":true, "planMode":true});
        crate::tests::validate_http_contract("createProjectThreadRequest", &planned);
        let created = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            planned.clone(),
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{}", created.1);
        let planned_id = created.1["conversation"]["conversationId"]
            .as_str()
            .unwrap()
            .to_owned();
        let detail_path = format!("/api/v1/project-conversations/{planned_id}");
        let detail = call(&state, "GET", &detail_path, Value::Null).await;
        assert_eq!(detail.0, StatusCode::OK, "{}", detail.1);
        crate::tests::validate_http_contract("projectConversationDetail", &detail.1);
        assert_eq!(detail.1["planMode"], true);
        assert_eq!(detail.1["claudeApproval"], "ask");
        let mut changed = planned.clone();
        changed["planMode"] = json!(false);
        let conflict = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            changed,
        )
        .await;
        assert_eq!(conflict.0, StatusCode::CONFLICT, "{}", conflict.1);
        let mut codex_approval = planned.clone();
        codex_approval["clientMessageId"] = json!(uuid::Uuid::new_v4().to_string());
        codex_approval["claudeApproval"] = json!("auto");
        let unsupported = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            codex_approval,
        )
        .await;
        assert_eq!(unsupported.0, StatusCode::UNPROCESSABLE_ENTITY);
        for bad in [
            json!({"claudeApproval": "auto"}),
            json!({"claudeApproval": "yolo"}),
            json!({"unsandboxedCommands": true}),
        ] {
            let rejected = call(&state, "PATCH", &detail_path, bad).await;
            assert_eq!(
                rejected.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{}",
                rejected.1
            );
        }
        let updated = call(
            &state,
            "PATCH",
            &detail_path,
            json!({"planMode": false, "isPinned": true}),
        )
        .await;
        assert_eq!(updated.0, StatusCode::OK, "{}", updated.1);
        assert_eq!(updated.1["planMode"], false);
        // Retry against the immutable original plan setting after PATCH.
        let retry = call(
            &state,
            "POST",
            "/api/v1/projects/prepare-project/threads",
            planned,
        )
        .await;
        assert_eq!(retry.0, StatusCode::OK, "{}", retry.1);
        assert_eq!(retry.1["conversation"]["conversationId"], planned_id);
        let library = call(&state, "GET", "/api/v1/projects", Value::Null).await;
        assert_eq!(library.0, StatusCode::OK, "{}", library.1);
        crate::tests::validate_http_contract("projectsResponse", &library.1);
        assert_eq!(library.1["modesVersion"], 1);
        assert_eq!(library.1["pinned"][0]["projectId"], "prepare-project");
        assert_eq!(
            library.1["pinned"][0]["thread"]["conversationId"],
            planned_id
        );
        let status = call(&state, "GET", "/api/v1/host/status", Value::Null).await;
        assert_eq!(status.0, StatusCode::OK, "{}", status.1);
        for feature in [FEATURE, crate::computer_sessions::HOST_VIEW_FEATURE] {
            assert!(status.1["features"]
                .as_array()
                .unwrap()
                .contains(&json!(feature)));
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // An accepted first Send has a durable receipt. Its exact retry must not
    // depend on today's Project visibility, folders or ingestion readiness.
    // A new request and a prepared-only first Send still use those gates.
    #[tokio::test]
    async fn accepted_creation_replays_receipt_after_project_changes() {
        use crate::test_support::{call, fixture};
        let (dir, state) = fixture().await;
        let source = dir.path().join("retry-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                "retry-project",
                "request",
                "hash",
                "Retry",
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        let project = state.store.project("retry-project").await.unwrap().unwrap();
        let service = crate::ingestion::spawn(state.clone()).await;
        for _ in 0..6000 {
            if state.ingestion.project_readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(state.ingestion.project_readiness(&state.store).await.ready);
        let path = "/api/v1/projects/retry-project/threads";
        let request = json!({"deviceId":"owner", "clientMessageId":uuid::Uuid::new_v4().to_string(),
            "family":"codex", "model":"fake", "body":"Review this folder", "folderId":project.primary_root_id,
            "rootsRevision":1});
        let accepted = call(&state, "POST", path, request.clone()).await;
        assert_eq!(accepted.0, StatusCode::ACCEPTED, "{}", accepted.1);
        let receipt = accepted.1["receipt"].clone();
        assert!(!receipt.is_null());
        let retry = call(&state, "POST", path, request.clone()).await;
        assert_eq!(retry.0, StatusCode::ACCEPTED, "{}", retry.1);
        assert_eq!(retry.1["receipt"], receipt);
        assert_eq!(
            retry.1["conversation"]["conversationId"],
            accepted.1["conversation"]["conversationId"]
        );
        for (key, value) in [
            ("body", json!("A different message")),
            ("model", json!("different-model")),
            ("folderId", json!(uuid::Uuid::new_v4().to_string())),
            ("rootsRevision", json!(2)),
            ("attachmentIds", json!([uuid::Uuid::new_v4().to_string()])),
        ] {
            let mut changed = request.clone();
            changed[key] = value;
            assert_eq!(
                call(&state, "POST", path, changed).await.0,
                StatusCode::CONFLICT,
                "changed {key}"
            );
        }
        let mut fresh = request.clone();
        fresh["clientMessageId"] = json!(uuid::Uuid::new_v4().to_string());

        state
            .store
            .update_project_metadata("retry-project", None, Some(false), None, "later")
            .await
            .unwrap();
        assert_eq!(
            call(&state, "POST", path, request.clone()).await.1["receipt"],
            receipt
        );
        assert_eq!(
            call(&state, "POST", path, fresh.clone()).await.0,
            StatusCode::CONFLICT
        );
        state
            .store
            .update_project_metadata("retry-project", None, Some(true), None, "later")
            .await
            .unwrap();

        state
            .store
            .update_project_roots("retry-project", 1, &roots, 0, "later")
            .await
            .unwrap();
        assert_eq!(
            call(&state, "POST", path, request.clone()).await.1["receipt"],
            receipt
        );
        assert_eq!(
            call(&state, "POST", path, fresh.clone()).await.0,
            StatusCode::PRECONDITION_FAILED
        );
        fresh["rootsRevision"] = json!(2);
        let mut prepared = fresh.clone();
        prepared["clientMessageId"] = json!(uuid::Uuid::new_v4().to_string());
        prepared["prepareOnly"] = json!(true);
        let reservation = call(&state, "POST", path, prepared.clone()).await;
        assert_eq!(reservation.0, StatusCode::OK, "{}", reservation.1);
        assert!(reservation.1["receipt"].is_null());
        std::fs::rename(&source, dir.path().join("moved-source")).unwrap();
        assert_eq!(
            call(&state, "POST", path, request.clone()).await.1["receipt"],
            receipt
        );
        assert_eq!(
            call(&state, "POST", path, fresh.clone()).await.0,
            StatusCode::CONFLICT
        );
        #[cfg(unix)]
        {
            let replacement = dir.path().join("unrelated-source");
            std::fs::create_dir(&replacement).unwrap();
            std::os::unix::fs::symlink(&replacement, &source).unwrap();
            assert_eq!(
                call(&state, "POST", path, fresh.clone()).await.0,
                StatusCode::CONFLICT,
                "A symlink at the original path cannot redirect a new request"
            );
        }

        drop(service);
        for _ in 0..6000 {
            if !state.ingestion.project_readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!state.ingestion.project_readiness(&state.store).await.ready);
        assert_eq!(
            call(&state, "POST", path, request.clone()).await.1["receipt"],
            receipt
        );
        assert_eq!(
            call(&state, "POST", path, fresh).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            call(&state, "POST", path, prepared.clone()).await.0,
            StatusCode::OK
        );
        prepared["prepareOnly"] = json!(false);
        assert_eq!(
            call(&state, "POST", path, prepared).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        state
            .update_admission
            .prepare_update("retry-update", &state)
            .await
            .unwrap();
        let replay_during_update = call(&state, "POST", path, request.clone()).await;
        assert_eq!(
            replay_during_update.0,
            StatusCode::ACCEPTED,
            "{}",
            replay_during_update.1
        );
        assert_eq!(
            replay_during_update.1["receipt"]["wonderMessageId"],
            receipt["wonderMessageId"]
        );
        let mut new_work = request;
        new_work["clientMessageId"] = json!(uuid::Uuid::new_v4().to_string());
        assert_eq!(
            call(&state, "POST", path, new_work).await.0,
            StatusCode::CONFLICT
        );
        assert!(state.update_admission.cancel_lease("retry-update").await);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // The provider receives the speed frozen with each accepted message,
    // including after a settings edit and when resuming an existing thread.
    #[tokio::test]
    async fn project_speed_reaches_exact_provider_requests() {
        let (dir, mut state) = crate::test_support::fixture().await;
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project(
                "speed-project",
                "request",
                "hash",
                "Speed",
                &roots,
                0,
                "now",
            )
            .await
            .unwrap();
        state.projects = ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let script = dir.path().join("runtime.py");
        let source_code = std::fs::read_to_string(&script).unwrap().replace(
            "else {'thread':{'status':{'type':'idle'}}}",
            "else {'thread':{'id':'thread','status':{'type':'idle'}}}",
        );
        std::fs::write(script, source_code).unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            json!({"data":[{
                "id":"fake","displayName":"Fake","serviceTiers":[
                    {"id":"default","label":"Standard"},{"id":"fast","label":"Fast"}]
            }]})
            .to_string(),
        )
        .unwrap();
        let mut catalog = state.runtime_catalog.write().await;
        catalog.models[0].service_tiers = vec![
            crate::ChoiceOption {
                id: "default".into(),
                label: "Standard".into(),
                description: None,
            },
            crate::ChoiceOption {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
            },
        ];
        drop(catalog);
        let conversation = uuid::Uuid::new_v4().to_string();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: &conversation,
                project_id: "speed-project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd: source.to_str().unwrap(),
                roots_revision: 1,
                title: "Speed",
                model: Some("fake"),
                effort: None,
                service_tier: Some("fast"),
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        let MessageInsert::Inserted(first) = state
            .store
            .insert_dispatch_message(
                "owner",
                "first",
                "first",
                "hash-first",
                &conversation,
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("first message")
        };
        state
            .store
            .update_project_conversation(
                &conversation,
                ProjectConversationPatch {
                    service_tier: Some(Some("default")),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        assert!(state
            .store
            .claim_message_for_dispatch(&first.id)
            .await
            .unwrap());
        let mut submitting = false;
        dispatch_inner(&state, &first, &mut submitting)
            .await
            .unwrap();
        assert!(submitting);
        state
            .store
            .update_message_delivery(&first.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        std::fs::write(dir.path().join("idle-fixture.json"),
            json!({"thread":{"id":"thread","cwd":source,"projectId":"native-project","status":{"type":"idle"}}}).to_string()).unwrap();
        // The second turn selects a preview note through the same immutable
        // attachment path as the native composer. The fake runtime records
        // the exact turn/start payload without starting model work.
        let annotated_source = source.join("annotated.txt");
        std::fs::write(&annotated_source, b"one\ntwo\n").unwrap();
        let annotation = json!({
            "version": 1, "projectId": "speed-project", "conversationId": conversation,
            "rootId": "workspace", "path": "annotated.txt",
            "sourceSha256": hex::encode(Sha256::digest(std::fs::read(&annotated_source).unwrap())),
            "anchor": {"kind":"textLines","startLine":2,"endLine":2},
            "note": "Review line two"
        });
        let upload = crate::upload_conversation_file(
            State(state.clone()),
            Path(conversation.clone()),
            Json(crate::CreateConversationFileRequest {
                client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                name: "annotation.json".into(),
                mime_type: Some(crate::artifact_annotations::MIME.into()),
                content_base64: {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode(annotation.to_string())
                },
            }),
        )
        .await;
        assert_eq!(upload.status(), StatusCode::OK);
        let uploaded: Value = serde_json::from_slice(
            &axum::body::to_bytes(upload.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        let annotation_id = uploaded["id"].as_str().unwrap().to_owned();
        let image_bytes = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;".to_vec();
        let media_notes = vec![(
            "marked.gif",
            image_bytes,
            json!({"kind":"imageRegion","x":0.1,"y":0.2,"width":0.3,"height":0.4}),
            "Check image crop",
            "image region (normalized 0-1, origin at top left) x=0.1, y=0.2, width=0.3, height=0.4",
        )];
        #[cfg(target_os = "macos")]
        let media_notes = {
            let mut media_notes = media_notes;
            media_notes.push((
                "marked.pdf",
                two_page_pdf(),
                json!({"kind":"pdfRegion","page":2,"x":0.25,"y":0.5,"width":0.5,"height":0.25}),
                "Check PDF page two",
                "PDF page 2, region (normalized 0-1, origin at top left) x=0.25, y=0.5, width=0.5, height=0.25",
            ));
            media_notes
        };
        let mut selected_ids = vec![annotation_id.clone()];
        let mut media_sources = Vec::new();
        for (name, bytes, anchor, note, _) in &media_notes {
            let source_path = source.join(name);
            std::fs::write(&source_path, bytes).unwrap();
            let record = json!({
                "version":1, "projectId":"speed-project", "conversationId":conversation,
                "rootId":"workspace", "path":name,
                "sourceSha256":hex::encode(Sha256::digest(bytes)), "anchor":anchor, "note":note,
            });
            let upload = crate::upload_conversation_file(
                State(state.clone()),
                Path(conversation.clone()),
                Json(crate::CreateConversationFileRequest {
                    client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                    name: format!("{name}.annotation.json"),
                    mime_type: Some(crate::artifact_annotations::MIME.into()),
                    content_base64: {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.encode(record.to_string())
                    },
                }),
            )
            .await;
            assert_eq!(upload.status(), StatusCode::OK);
            let uploaded: Value = serde_json::from_slice(
                &axum::body::to_bytes(upload.into_body(), 8192)
                    .await
                    .unwrap(),
            )
            .unwrap();
            selected_ids.push(uploaded["id"].as_str().unwrap().to_owned());
            media_sources.push(source_path);
        }
        // A valid PDF container is not enough: a crafted annotation cannot
        // send a region on a page the document does not contain.
        #[cfg(target_os = "macos")]
        {
            let missing_page_record = json!({
                "version":1, "projectId":"speed-project", "conversationId":conversation,
                "rootId":"workspace", "path":"marked.pdf",
                "sourceSha256":hex::encode(Sha256::digest(&media_notes[1].1)),
                "anchor":{"kind":"pdfRegion","page":3,"x":0.25,"y":0.5,"width":0.5,"height":0.25},
                "note":"This page does not exist",
            });
            let upload = crate::upload_conversation_file(
                State(state.clone()),
                Path(conversation.clone()),
                Json(crate::CreateConversationFileRequest {
                    client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                    name: "missing-page.annotation.json".into(),
                    mime_type: Some(crate::artifact_annotations::MIME.into()),
                    content_base64: {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .encode(missing_page_record.to_string())
                    },
                }),
            )
            .await;
            assert_eq!(upload.status(), StatusCode::OK);
            let uploaded: Value = serde_json::from_slice(
                &axum::body::to_bytes(upload.into_body(), 8192)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                crate::artifact_annotations::validate_first_acceptance(
                    &state,
                    &conversation,
                    "owner",
                    "invalid-page",
                    &[uploaded["id"].as_str().unwrap().to_owned()],
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        crate::artifact_annotations::validate_first_acceptance(
            &state,
            &conversation,
            "owner",
            "second",
            &selected_ids,
        )
        .await
        .unwrap();
        std::fs::write(&annotated_source, b"changed after acceptance\n").unwrap();
        for source_path in &media_sources {
            std::fs::remove_file(source_path).unwrap();
        }
        let MessageInsert::Inserted(second) = state
            .store
            .insert_dispatch_message(
                "owner",
                "second",
                "second",
                "hash-second",
                &conversation,
                &selected_ids,
                "later",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("second message")
        };
        // The accepted annotation must still dispatch from its frozen copy
        // after an unrelated Project root edit increments roots_revision.
        let unrelated = dir.path().join("unrelated-project-root");
        std::fs::create_dir(&unrelated).unwrap();
        let changed_roots = validate_folders(
            &[
                source.to_string_lossy().into_owned(),
                unrelated.to_string_lossy().into_owned(),
            ],
            &[],
        )
        .unwrap();
        let updated = state
            .store
            .update_project_roots("speed-project", 1, &changed_roots, 0, "after-acceptance")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.roots_revision, 2);
        assert_eq!(
            state
                .store
                .project_conversation(&conversation)
                .await
                .unwrap()
                .unwrap()
                .roots_revision,
            1
        );
        assert!(state
            .store
            .claim_message_for_dispatch(&second.id)
            .await
            .unwrap());
        submitting = false;
        dispatch_inner(&state, &second, &mut submitting)
            .await
            .unwrap();
        assert!(submitting);
        let requests: Vec<Value> = std::fs::read_to_string(dir.path().join("requests-jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect();
        for (method, expected) in [("thread/start", "fast"), ("thread/resume", "default")] {
            let request = requests
                .iter()
                .find(|request| request["method"] == method)
                .unwrap();
            assert_eq!(request["params"]["serviceTier"], expected, "{method}");
        }
        let turns: Vec<_> = requests
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["params"]["serviceTier"], "fast");
        assert_eq!(turns[1]["params"]["serviceTier"], "default");
        let second_input = turns[1]["params"]["input"].as_array().unwrap();
        assert_eq!(
            second_input.len(),
            2 + media_notes.len(),
            "body plus one text and each supported media note"
        );
        assert_eq!(
            second_input
                .iter()
                .filter(|input| input["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Review line two")))
                .count(),
            1
        );
        let media = media_workspace(&state, &conversation).await.unwrap();
        let source_copy_id = crate::deterministic_uuid(&format!("artifact-source:{annotation_id}"));
        let source_copy = crate::attachment_path(&media, &source_copy_id, false)
            .await
            .unwrap();
        assert!(second_input.iter().any(|input| input["text"]
            .as_str()
            .is_some_and(|text| text.contains(source_copy.to_str().unwrap())
                && text.contains("Original file: workspace/annotated.txt"))));
        assert_eq!(std::fs::read(&source_copy).unwrap(), b"one\ntwo\n");
        for (index, (name, bytes, _, note, anchor)) in media_notes.iter().enumerate() {
            let copy_id =
                crate::deterministic_uuid(&format!("artifact-source:{}", selected_ids[index + 1]));
            let copy = crate::attachment_path(&media, &copy_id, false)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&copy).unwrap(), *bytes);
            assert_eq!(
                second_input
                    .iter()
                    .filter(|input| input["text"].as_str().is_some_and(|text| text
                        .contains(&format!("Original file: workspace/{name}"))
                        && text.contains(copy.to_str().unwrap())
                        && text.contains(anchor)
                        && text.contains(note)))
                    .count(),
                1,
                "{name} anchor must reach one provider text input"
            );
        }
        assert!(!second_input.iter().any(|input| input["text"]
            .as_str()
            .is_some_and(|text| text.contains("annotation.json"))));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // The existing upload/storage boundary owns this regression: project
    // uploads must retain verified bytes under their own conversation, without
    // writing into a source folder or following a substituted media symlink.
    #[tokio::test]
    async fn project_attachments_use_owned_verified_storage() {
        use base64::Engine as _;
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let projects = dir.path().join("projects");
        std::fs::create_dir(&projects).unwrap();
        state.data_dir = projects.parent().unwrap().to_path_buf();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let roots = validate_folders(&[source.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        let conversation = uuid::Uuid::new_v4().to_string();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: &conversation,
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: "native-home",
                native_session_id: None,
                cwd: source.to_str().unwrap(),
                roots_revision: 1,
                title: "Draft",
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: Some("creation"),
                now: "now",
            })
            .await
            .unwrap();
        let upload_id = uuid::Uuid::new_v4().to_string();
        let bytes = b"owned attachment";
        let upload = || crate::CreateConversationFileRequest {
            client_upload_id: Some(upload_id.clone()),
            name: "notes.txt".into(),
            mime_type: Some("text/plain".into()),
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        let response = crate::upload_conversation_file(
            State(state.clone()),
            Path(conversation.clone()),
            Json(upload()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let first: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        let response = crate::upload_conversation_file(
            State(state.clone()),
            Path(conversation.clone()),
            Json(upload()),
        )
        .await;
        let retry: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["id"], retry["id"]);
        assert!(!source.join(".wonder").exists());
        assert!(state
            .store
            .runtime_binding(&conversation)
            .await
            .unwrap()
            .is_none());
        let files = state
            .store
            .list_conversation_files(&conversation)
            .await
            .unwrap();
        assert_eq!(files.len(), 1);
        let media = media_workspace(&state, &conversation).await.unwrap();
        let path = crate::attachment_path(&media, &files[0].id, false)
            .await
            .unwrap();
        assert_eq!(
            crate::attachments::verified_bytes(&path, &files[0])
                .await
                .unwrap(),
            bytes
        );
        std::fs::write(&path, b"changed").unwrap();
        let response = crate::upload_conversation_file(
            State(state.clone()),
            Path(conversation.clone()),
            Json(upload()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        // A12-10/A12-31: whatever replaced the attached file, Guide/steer and
        // main sends (`for_agent`), T3 Code uploads and the download refuse it
        // before any provider request, and nothing waits on a special file.
        let download = || {
            crate::conversation_file(
                State(state.clone()),
                Path((conversation.clone(), files[0].id.clone())),
                axum::extract::Query(crate::ConversationFileQuery { max_pixels: None }),
            )
        };
        let outside = dir.path().join("outside");
        std::fs::write(&outside, bytes).unwrap();
        let changed = (
            "An attached file changed. Attach it again before retrying.",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
        type Swap<'a> = (&'a str, (&'a str, StatusCode), Box<dyn Fn() + 'a>);
        let mut swaps: Vec<Swap> = vec![
            (
                "same-size bytes",
                changed,
                Box::new(|| std::fs::write(&path, b"owned Attachment").unwrap()),
            ),
            (
                "oversized",
                changed,
                Box::new(|| {
                    std::fs::write(&path, vec![b'x'; crate::MAX_ATTACHMENT_BYTES + 1]).unwrap()
                }),
            ),
        ];
        #[cfg(unix)]
        swaps.extend([
            (
                "symlink to the original bytes",
                (
                    "An attached file is unavailable. Attach it again.",
                    StatusCode::FORBIDDEN,
                ),
                Box::new(|| std::os::unix::fs::symlink(&outside, &path).unwrap()) as Box<dyn Fn()>,
            ),
            (
                "named pipe",
                changed,
                Box::new(|| {
                    let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }),
            ),
        ]);
        for (swap, (message, status), replace) in &swaps {
            std::fs::remove_file(&path).unwrap();
            replace();
            let refused = tokio::time::timeout(Duration::from_secs(10), async {
                (
                    crate::agent_images::for_agent(&media, files.clone()).await,
                    t3_code_uploads(&state, &conversation, files.clone()).await,
                    download().await.status(),
                )
            })
            .await
            .unwrap_or_else(|_| panic!("{swap}: a read waited"));
            assert_eq!(refused.0.unwrap_err(), *message, "{swap}");
            assert_eq!(refused.1.err().as_deref(), Some(*message), "{swap}");
            assert_eq!(refused.2, *status, "{swap}");
        }
        std::fs::remove_file(&path).unwrap();
        let missing = download().await;
        assert_eq!(missing.status(), StatusCode::GONE);
        assert_eq!(
            missing.headers()[ERROR_CODE_HEADER],
            crate::media_refs::GONE_CODE
        );
        assert_eq!(
            t3_code_uploads(&state, &conversation, files.clone())
                .await
                .err()
                .as_deref(),
            Some("An attached file is missing. Attach it again.")
        );
        std::fs::write(&path, bytes).unwrap();
        let sent = t3_code_uploads(&state, &conversation, files.clone())
            .await
            .unwrap();
        let crate::t3_code::UploadBody::Bytes(sent_bytes) = &sent[0].body else {
            panic!("small file bytes");
        };
        assert_eq!(sent_bytes, bytes);
        assert_eq!(download().await.status(), StatusCode::OK);
        // B-206: removing the attachment storage, not just the file, is gone too.
        let storage = path.parent().unwrap();
        for removed in [storage, storage.parent().unwrap()] {
            std::fs::remove_dir_all(removed).unwrap();
            let missing = download().await;
            assert_eq!(missing.status(), StatusCode::GONE, "{removed:?}");
            assert_eq!(
                missing.headers()[ERROR_CODE_HEADER],
                crate::media_refs::GONE_CODE
            );
            assert_eq!(
                crate::agent_images::for_agent(&media, files.clone())
                    .await
                    .unwrap_err(),
                "An attached file is missing. Attach it again."
            );
            assert_eq!(
                t3_code_uploads(&state, &conversation, files.clone())
                    .await
                    .err()
                    .as_deref(),
                Some("An attached file is missing. Attach it again.")
            );
        }
        #[cfg(unix)]
        {
            std::fs::rename(&media, format!("{media}-original")).unwrap();
            std::os::unix::fs::symlink(&source, &media).unwrap();
            assert!(media_workspace(&state, &conversation).await.is_none());
        }
    }

    // Contract: a Wonder turn never inherits a desktop client's broader policy.
    #[test]
    fn codex_turn_policy_is_always_explicit() {
        let roots = vec!["/work/app".to_owned(), "/work/docs".to_owned()];
        let (sandbox, approval, policy) = codex_policy("workspace", &roots);
        assert_eq!((sandbox, approval), ("workspace-write", "on-request"));
        assert_eq!(policy["writableRoots"], json!(roots));
        assert_eq!(policy["networkAccess"], false);
        assert_eq!(codex_policy("read_only", &roots).2["type"], "readOnly");
        // Unknown stored values fail toward the ordinary workspace boundary.
        assert_eq!(codex_policy("unexpected", &roots).0, "workspace-write");
    }

    // Contract: every Codex turn names its collaboration mode and reasoning
    // effort so leaving plan mode takes effect, and the sandbox and approval
    // policy stay exactly the access mode's.
    #[test]
    fn codex_turn_always_names_its_collaboration_mode() {
        let roots = vec!["/work/app".to_owned()];
        let input = vec![json!({"type": "text", "text": "Hi"})];
        for (plan, effort, expected) in [(false, None, "default"), (true, Some("high"), "plan")] {
            for access in ["read_only", "workspace", "full_access"] {
                let params = codex_turn_params(
                    "thread",
                    "client",
                    &input,
                    &CodexTurn {
                        access_mode: access,
                        roots: &roots,
                        model: "gpt-x",
                        effort,
                        service_tier: Some("default"),
                        cwd: "/work/app",
                        plan,
                    },
                );
                assert_eq!(
                    params["collaborationMode"],
                    json!({"mode": expected, "settings": {"model": "gpt-x",
                        "reasoning_effort": effort, "developer_instructions": null}})
                );
                let (_, approval, sandbox) = codex_policy(access, &roots);
                assert_eq!(params["approvalPolicy"], approval);
                assert_eq!(params["sandboxPolicy"], sandbox);
                assert_eq!(params["model"], "gpt-x");
                assert_eq!(params["effort"], json!(effort));
            }
        }
    }

    // Contract: the bridge policy follows access first. Approval modes refine
    // Workspace only; Read only always asks, Full access never does, and plan
    // mode is independent of both.
    #[test]
    fn claude_policy_maps_access_approval_and_plan() {
        let roots = vec!["/work/app".to_owned(), "/work/docs".to_owned()];
        let denied = vec!["/secrets".to_owned()];
        let policy = |access_mode, approval, plan| {
            claude_policy_value(
                roots.clone(),
                &denied,
                ClaudeModes {
                    access_mode,
                    approval,
                    plan,
                },
                "/work/app",
            )
        };
        for (access, approval, mode, expected, writes) in [
            ("read_only", "auto", "read_only", "ask", false),
            ("read_only", "accept_edits", "read_only", "ask", false),
            ("workspace", "ask", "workspace", "ask", true),
            (
                "workspace",
                "accept_edits",
                "workspace",
                "accept_edits",
                true,
            ),
            ("workspace", "auto", "workspace", "auto", true),
            ("workspace", "unexpected", "workspace", "ask", true),
            ("full_access", "ask", "full_access", "full_access", true),
            ("full_access", "auto", "full_access", "full_access", true),
            ("unexpected", "auto", "workspace", "auto", true),
        ] {
            for plan in [false, true] {
                let value = policy(access, approval, plan);
                assert_eq!(value["mode"], mode, "{access}/{approval}");
                assert_eq!(value["approvalMode"], expected, "{access}/{approval}");
                assert_eq!(value["planMode"], plan);
                assert_eq!(
                    value["writeRoots"],
                    json!(if writes { roots.clone() } else { vec![] })
                );
                assert_eq!(value["workspace"], "/work/app");
                #[cfg(not(windows))]
                assert_eq!(value["readRoots"], json!(["/"]));
                assert_eq!(value["deniedRoots"], json!(denied));
            }
        }
        assert_eq!(
            ClaudeModes::default().approval,
            agent_defaults::claude_approval(AgentFamily::Claude)
        );
    }

    #[test]
    fn turn_settings_never_send_nil_effort_and_keep_a_stored_speed() {
        let choice = |id: &str| crate::ChoiceOption {
            id: id.into(),
            label: id.into(),
            description: None,
        };
        let model = |id: &str, efforts: &[&str], default: Option<&str>| crate::ModelOption {
            agent_family: AgentFamily::Codex,
            capabilities: crate::ModelCapabilities::for_family(AgentFamily::Codex),
            id: id.into(),
            display_name: id.into(),
            description: None,
            model_specialty: None,
            hidden: false,
            reasoning_efforts: efforts.iter().map(|e| choice(e)).collect(),
            default_reasoning_effort: default.map(str::to_owned),
            service_tiers: vec![choice("default"), choice("priority")],
            default_service_tier: None,
            is_default: false,
            provider_default: None,
            native_ids: vec![],
        };
        let catalog = crate::RuntimeCatalog {
            models: vec![
                model("gpt-a", &["low", "medium", "xhigh"], Some("medium")),
                model("gpt-b", &["low", "xhigh"], None),
                model("plain", &[], None),
            ],
            ..Default::default()
        };
        let run = |m, e, t| execution_settings(&catalog, AgentFamily::Codex, m, e, t).unwrap();
        // A legacy thread (nothing stored) and a stale effort both resolve.
        assert_eq!(
            run(None, None, None),
            ("gpt-a".into(), Some("low".into()), Some("default".into()))
        );
        assert_eq!(run(Some("gpt-b"), None, None).1.as_deref(), Some("low"));
        assert_eq!(
            run(Some("gpt-b"), Some("medium"), None).1.as_deref(),
            Some("low")
        );
        assert_eq!(run(Some("plain"), Some("high"), None).1, None);
        // A stored non-default speed is what reaches turn/start.
        assert_eq!(
            run(Some("gpt-a"), Some("xhigh"), Some("priority"))
                .2
                .as_deref(),
            Some("priority")
        );
        assert_eq!(
            run(Some("gpt-a"), None, Some("gone")).2.as_deref(),
            Some("default")
        );
        let mut hidden_all = catalog.clone();
        hidden_all.models.clear();
        assert!(execution_settings(&hidden_all, AgentFamily::Codex, None, None, None).is_err());
    }

    // Contract: a new project's folder is created once inside the chosen
    // location; a retry reuses the empty folder, an occupied name or a
    // protected location is refused, and names cannot escape the location.
    #[tokio::test]
    async fn new_project_creates_its_folder_and_retries_idempotently() {
        use crate::test_support::{call, fixture};
        let (dir, mut state) = fixture().await;
        let parent = dir.path().join("Code");
        std::fs::create_dir(&parent).unwrap();
        let protected = dir.path().join("secrets");
        std::fs::create_dir(&protected).unwrap();
        state.denied_roots = vec![protected.to_string_lossy().into_owned()];
        let request_id = uuid::Uuid::new_v4().to_string();
        let body = json!({
            "requestId": request_id,
            "name": "Launch site",
            "newFolder": {"parent": parent.to_str().unwrap()},
        });
        crate::tests::validate_http_contract("createProjectRequest", &body);
        let created = call(&state, "POST", "/api/v1/projects", body.clone()).await;
        assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
        let folder = crate::canonical_path::canonicalize(parent.join("Launch site")).unwrap();
        assert!(folder.is_dir());
        assert_eq!(created.1["folders"][0]["path"], folder.to_str().unwrap());
        // A lost response is retried with the same request: same project, no error.
        let retried = call(&state, "POST", "/api/v1/projects", body.clone()).await;
        assert_eq!(retried.0, StatusCode::OK, "{}", retried.1);
        assert_eq!(retried.1["id"], created.1["id"]);
        // Also once something has put files in the new folder.
        std::fs::write(folder.join("README.md"), "x").unwrap();
        let retried = call(&state, "POST", "/api/v1/projects", body).await;
        assert_eq!(retried.0, StatusCode::OK, "{}", retried.1);
        assert_eq!(retried.1["id"], created.1["id"]);
        // Reusing the request for a different folder is refused before any
        // folder is made.
        let reused = call(
            &state,
            "POST",
            "/api/v1/projects",
            json!({"requestId": request_id, "name": "Other site",
                   "newFolder": {"parent": parent.to_str().unwrap()}}),
        )
        .await;
        assert_eq!(reused.0, StatusCode::CONFLICT, "{}", reused.1);
        assert!(
            !parent.join("Other site").exists(),
            "an orphan folder was left"
        );
        // An occupied name is never adopted.
        let occupied = call(
            &state,
            "POST",
            "/api/v1/projects",
            json!({"requestId": uuid::Uuid::new_v4().to_string(), "name": "Launch site",
                   "newFolder": {"parent": parent.to_str().unwrap()}}),
        )
        .await;
        assert_eq!(occupied.0, StatusCode::CONFLICT, "{}", occupied.1);
        for (name, parent) in [
            ("../escape", parent.to_str().unwrap()),
            (".hidden", parent.to_str().unwrap()),
            ("Inside", protected.to_str().unwrap()),
            ("Relative", "Code"),
        ] {
            let refused = call(
                &state,
                "POST",
                "/api/v1/projects",
                json!({"requestId": uuid::Uuid::new_v4().to_string(), "name": name,
                       "newFolder": {"parent": parent}}),
            )
            .await;
            assert_eq!(
                refused.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{name}: {}",
                refused.1
            );
        }
        assert!(!protected.join("Inside").exists());
        assert!(!dir.path().join("escape").exists());
        let both = call(
            &state,
            "POST",
            "/api/v1/projects",
            json!({"requestId": uuid::Uuid::new_v4().to_string(), "name": "Both",
                   "folders": [parent.to_str().unwrap()], "newFolder": {}}),
        )
        .await;
        assert_eq!(both.0, StatusCode::BAD_REQUEST, "{}", both.1);
        let library = call(&state, "GET", "/api/v1/projects", Value::Null).await;
        assert_eq!(library.1["createVersion"], 1);
    }

    // Contract: a folder is named by its full location in the host's own
    // path style. Windows drive paths pass on Windows only; shares, device
    // and relative forms never do. (Pure policy: native Windows behavior of
    // canonicalize and the protected-root checks is verified on Windows.)
    #[test]
    fn project_folders_follow_the_host_path_style() {
        for (path, posix, windows) in [
            ("/Users/me/app", true, false),
            ("/", true, false),
            ("C:\\Users\\me\\app", false, true),
            ("C:/Users/me/app", false, true),
            ("d:\\work", false, true),
            ("C:", false, false),
            ("C:app", false, false),
            ("\\app", false, false),
            ("\\\\server\\share\\app", false, false),
            ("\\\\?\\C:\\app", false, false),
            ("\\\\.\\C:\\app", false, false),
            ("//server/share/app", true, false),
            ("1:\\app", false, false),
            ("relative", false, false),
            ("", false, false),
        ] {
            assert_eq!(
                is_native_folder_path(path, false),
                posix,
                "{path:?} on POSIX"
            );
            assert_eq!(
                is_native_folder_path(path, true),
                windows,
                "{path:?} on Windows"
            );
        }
    }

    // Contract: protected locations and whole-home folders cannot become
    // project roots, while an ordinary folder resolves to its canonical path.
    #[test]
    fn project_folders_exclude_protected_locations() {
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("secrets");
        let project = temp.path().join("app");
        std::fs::create_dir_all(protected.join("inner")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let denied = vec![protected.to_string_lossy().into_owned()];
        assert!(validate_folder(project.to_str().unwrap(), &denied).is_ok());
        assert!(validate_folder(protected.join("inner").to_str().unwrap(), &denied).is_err());
        assert!(validate_folder(temp.path().to_str().unwrap(), &denied).is_err());
        assert!(validate_folder("relative", &denied).is_err());
        assert!(validate_folder(temp.path().join("missing").to_str().unwrap(), &denied).is_err());
        let roots = validate_folders(
            &[
                project.to_string_lossy().into_owned(),
                format!("{}/", project.display()),
            ],
            &denied,
        )
        .unwrap();
        assert_eq!(roots.len(), 1);
        #[cfg(unix)]
        {
            let linked = temp.path().join("linked");
            let replacement = temp.path().join("other");
            std::fs::create_dir(&replacement).unwrap();
            std::os::unix::fs::symlink(&project, &linked).unwrap();
            let selected = validate_folder(linked.to_str().unwrap(), &denied).unwrap();
            let stored = StoredProject {
                id: "p".into(),
                name: "Project".into(),
                is_included: true,
                pin_order: None,
                primary_root_id: "r".into(),
                roots_revision: 1,
                last_family: None,
                created_at: "now".into(),
                updated_at: "now".into(),
                last_used_at: None,
                auto_archive: None,
                last_models: Vec::new(),
                claude_trust: Vec::new(),
                roots: vec![wonder_store::StoredProjectRoot {
                    id: "r".into(),
                    path: selected.path.clone(),
                    canonical_path: selected.canonical_path.clone(),
                    ordinal: 0,
                }],
            };
            assert!(validate_execution_roots(&stored, &denied).is_ok());
            std::fs::remove_file(&linked).unwrap();
            std::os::unix::fs::symlink(&replacement, &linked).unwrap();
            let current = validate_folder(linked.to_str().unwrap(), &denied).unwrap();
            assert_ne!(selected.canonical_path, current.canonical_path);
            assert!(validate_execution_roots(&stored, &denied).is_err());
            std::fs::remove_file(&linked).unwrap();
            std::os::unix::fs::symlink(&protected, &linked).unwrap();
            assert!(validate_folder(linked.to_str().unwrap(), &denied).is_err());
        }
    }

    async fn fork_response(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned())),
        )
    }

    fn fork_requests(dir: &FsPath, log: &str, method: &str) -> Vec<Value> {
        std::fs::read_to_string(dir.join(log))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|request| request["method"] == method)
            .collect()
    }

    // Admission requires native idle or positive completion evidence; another
    // process's unfinished indexed turn wins over this process's idle state.
    #[tokio::test]
    async fn codex_admission_prefers_native_ownership_and_positive_terminal_evidence() {
        let (dir, mut state, _) = handoff_fixture().await;
        for (status, turn, held) in [
            ("idle", json!({}), false),
            ("active", json!({}), true),
            (
                "notLoaded",
                json!({"id":"turn","status":"completed","completedAt":100}),
                false,
            ),
            (
                "notLoaded",
                json!({"id":"turn","status":"interrupted","completedAt":100}),
                false,
            ),
            (
                "notLoaded",
                json!({"id":"turn","status":"interrupted","completedAt":null}),
                true,
            ),
        ] {
            std::fs::write(
                dir.path().join("idle-fixture.json"),
                json!({"thread":{"id":"thread","status":{"type":status}}}).to_string(),
            )
            .unwrap();
            std::fs::write(
                dir.path().join("turn-pages.json"),
                json!([[turn]]).to_string(),
            )
            .unwrap();
            assert_eq!(
                codex_held(&state, "thread", &state.projects.codex_store).await,
                held,
                "native status {status}"
            );
        }
        let thread = "admission-foreign";
        let mut index = crate::codex_activity::tests::Fixture::new_for_thread(thread).await;
        state.codex_home = index.dir.path().to_path_buf();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({thread:{"id":thread,"path":index.path,"status":{"type":"idle"}}}).to_string(),
        )
        .unwrap();
        assert!(
            codex_held(&state, thread, &state.projects.codex_store).await,
            "foreign active beats local idle"
        );
        index.finish("completed").await;
        assert!(
            !codex_held(&state, thread, &state.projects.codex_store).await,
            "indexed completion admits work"
        );
        std::fs::remove_file(index.dir.path().join("thread_history_1.sqlite")).unwrap();
        assert!(
            codex_held(&state, thread, &state.projects.codex_store).await,
            "known rollout with unknown index holds work"
        );
        state.projects.shutdown().await;
        assert!(
            codex_held(&state, "thread", &state.projects.codex_store).await,
            "missing native evidence cannot admit work"
        );
    }

    #[tokio::test]
    async fn queued_codex_send_starts_a_stopped_runtime_without_relaxing_unknown_evidence() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let thread = "desktop-stopped-runtime";
        let mut index = crate::codex_activity::tests::Fixture::new_for_thread(thread).await;
        index.producer_completion().await;
        state.codex_home = index.dir.path().to_path_buf();
        crate::test_support::bind_project_thread(
            &state,
            chat,
            AgentFamily::Codex,
            thread,
            None,
            "now",
        )
        .await
        .unwrap();
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({thread:{
            "id":thread,"path":index.path,"status":{"type":"idle"}}})
            .to_string(),
        )
        .unwrap();
        state.projects.shutdown().await;
        assert!(!state.projects.codex_alive().await);
        let waiting = wonder_store::StoredMessage {
            id: "waiting".into(),
            device_id: "owner".into(),
            client_message_id: "waiting".into(),
            body: "Continue".into(),
            body_sha256: "hash".into(),
            conversation_id: chat.into(),
            state: "queued".into(),
            created_at: "now".into(),
            codex_thread_id: None,
            codex_turn_id: None,
        };
        assert!(!held_on_mac(&state, chat, Some(&waiting)).await);
        assert!(
            state.projects.codex_alive().await,
            "queue gate permits normal startup"
        );
        std::fs::remove_file(index.dir.path().join("thread_history_1.sqlite")).unwrap();
        state.projects.shutdown().await;
        assert!(
            held_on_mac(&state, chat, Some(&waiting)).await,
            "startup success alone cannot override missing shared evidence"
        );
        assert_eq!(
            fork_requests(dir.path(), "requests-jsonl", "turn/start").len(),
            0
        );
        state.projects.shutdown().await;
    }

    // Contract: a fork is refused while the source is working, then becomes a new
    // project thread in the same folder with the same settings, cut at the chosen
    // turn, and the source is never touched.
    #[tokio::test]
    async fn forking_a_codex_thread_makes_an_independent_project_thread() {
        let (dir, state, message) = handoff_fixture().await;
        let source_folder = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap()
            .cwd;
        let fork_from = |turn: Option<&str>| {
            fork(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Json(ForkRequest {
                    last_turn_id: turn.map(str::to_owned),
                }),
            )
        };
        let (status, body) = fork_response(fork_from(Some("turn")).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.to_string().contains("still working in this chat"));
        assert!(fork_requests(dir.path(), "requests-jsonl", "thread/fork").is_empty());

        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        std::fs::write(
            dir.path().join("native-thread.json"),
            json!({"id":"forked-thread","cwd":source_folder,"projectId":"native-project"})
                .to_string(),
        )
        .unwrap();
        let (status, summary) = fork_response(fork_from(Some("turn")).await).await;
        assert_eq!(status, StatusCode::CREATED, "{summary}");
        crate::tests::validate_http_contract("projectThreadSummary", &summary);
        assert_eq!(summary["title"], "Recovery (fork)");
        assert_eq!(summary["family"], "codex");
        let fork_id = summary["conversationId"].as_str().unwrap();

        let requests = fork_requests(dir.path(), "requests-jsonl", "thread/fork");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["params"],
            json!({"threadId":"thread","excludeTurns":true,"cwd":source_folder,"lastTurnId":"turn"})
        );
        let stored = state
            .store
            .project_conversation(fork_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.native_session_id.as_deref(), Some("forked-thread"));
        assert_eq!(
            (stored.cwd.as_str(), stored.access_mode.as_str()),
            (source_folder.as_str(), "read_only")
        );
        assert_eq!(
            state
                .store
                .runtime_binding(fork_id)
                .await
                .unwrap()
                .unwrap()
                .thread_id,
            "forked-thread"
        );
        assert_eq!(
            state.store.project_fork_source(fork_id).await.unwrap(),
            Some(("project-chat".into(), Some("turn".into())))
        );
        let source = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.native_session_id.as_deref(), Some("thread"));
        assert_eq!(source.title, "Recovery");

        // The fake runtime answers every fork with the same thread, which is
        // already attached, so a second fork of the latest turn is not saved.
        let (status, _) = fork_response(fork_from(None).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let all = fork_requests(dir.path(), "requests-jsonl", "thread/fork");
        assert_eq!(all.len(), 2);
        assert!(all[1]["params"].get("lastTurnId").is_none());
    }

    // Contract: the owner trusts a Project folder's own Claude configuration
    // exactly as reviewed; the trust is saved on the Project and goes with every
    // Claude session there, a change since the review is refused, and Stop
    // trusting removes it.
    #[tokio::test]
    async fn claude_folder_trust_is_saved_per_project_and_reaches_the_runtime() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let folder = dir.path().join("cloned-repo");
        std::fs::create_dir(&folder).unwrap();
        let roots = validate_folders(&[folder.to_string_lossy().into_owned()], &[]).unwrap();
        for id in ["project", "other"] {
            state
                .store
                .create_project(id, id, id, "Test", &roots, 0, "now")
                .await
                .unwrap();
        }
        let entrypoint = dir.path().join("claude-bridge.py");
        std::fs::write(
            &entrypoint,
            r#"import json, sys, os
root = os.path.dirname(__file__)
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    with open(root + '/claude-requests', 'a') as log: log.write(json.dumps({'method': m, 'params': p}) + '\n')
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'project/configuration/read':
        fp = open(root + '/fingerprint').read().strip()
        result = {'fingerprint': fp, 'empty': False, 'tooLarge': False, 'folders': [p['wonderProject']['cwd']],
            'items': [{'kind': 'command', 'text': 'Runs “./setup.sh” when a chat starts'}],
            'trusted': fp in p['wonderProject'].get('trustedConfigurations', [])}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#,
        )
        .unwrap();
        let (first, second) = ("a".repeat(64), "b".repeat(64));
        std::fs::write(dir.path().join("fingerprint"), &first).unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
        let path = "/api/v1/projects/project/claude-trust";
        let call = |method: &'static str, body: Value| {
            let state = state.clone();
            async move { crate::test_support::call(&state, method, path, body).await }
        };

        let (status, read) = call("GET", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{read}");
        crate::test_support::assert_http_contract("projectClaudeConfiguration", &read);
        assert_eq!(
            (read["trusted"].clone(), read["fingerprint"].clone()),
            (json!(false), json!(first))
        );
        assert_eq!(
            read["items"][0]["text"],
            "Runs “./setup.sh” when a chat starts"
        );

        // The folder changed after the owner reviewed it.
        std::fs::write(dir.path().join("fingerprint"), &second).unwrap();
        let (status, body) = call("PUT", json!({"fingerprint": first})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(state
            .store
            .project("project")
            .await
            .unwrap()
            .unwrap()
            .claude_trust
            .is_empty());
        let outside = json!({"fingerprint": second, "cwd": dir.path().join("elsewhere")});
        assert_eq!(call("PUT", outside).await.0, StatusCode::FORBIDDEN);

        let (status, trusted) = call("PUT", json!({"fingerprint": second})).await;
        assert_eq!(status, StatusCode::OK, "{trusted}");
        crate::test_support::assert_http_contract("projectClaudeConfiguration", &trusted);
        assert_eq!(trusted["trusted"], true);
        let project = state.store.project("project").await.unwrap().unwrap();
        assert_eq!(project.claude_trust, vec![second.clone()]);
        // Saved per Project: the other Project on the same folder is untouched.
        assert!(state
            .store
            .project("other")
            .await
            .unwrap()
            .unwrap()
            .claude_trust
            .is_empty());
        // Every Claude session in the Project carries it, and the runtime's
        // own check now agrees.
        let cwd = folder.to_str().unwrap();
        assert_eq!(
            claude_project(&project, cwd)["trustedConfigurations"],
            json!([second])
        );
        assert_eq!(call("GET", Value::Null).await.1["trusted"], true);
        let reads = fork_requests(dir.path(), "claude-requests", "project/configuration/read");
        assert_eq!(
            reads.last().unwrap()["params"]["wonderProject"]["trustedConfigurations"],
            json!([second])
        );
        assert_eq!(reads.last().unwrap()["params"]["wonderProject"]["cwd"], cwd);

        let (status, _) = call("DELETE", Value::Null).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let project = state.store.project("project").await.unwrap().unwrap();
        assert!(project.claude_trust.is_empty());
        assert_eq!(
            claude_project(&project, cwd)["trustedConfigurations"],
            json!([])
        );
        assert_eq!(call("GET", Value::Null).await.1["trusted"], false);
        let missing = "/api/v1/projects/missing/claude-trust";
        assert_eq!(
            crate::test_support::call(&state, "DELETE", missing, Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    // Contract: Claude forks through the bridge, then attaches the copy with the
    // source's model and access; the bridge's refusal reaches the owner as is.
    #[tokio::test]
    async fn forking_a_claude_thread_attaches_the_bridge_copy() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let source_folder = dir.path().join("project-source");
        std::fs::create_dir(&source_folder).unwrap();
        let roots = validate_folders(&[source_folder.to_string_lossy().into_owned()], &[]).unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        state.projects = ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let entrypoint = dir.path().join("claude-bridge.py");
        std::fs::write(
            &entrypoint,
            r#"import json, sys, os
root = os.path.dirname(__file__)
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    with open(root + '/claude-requests', 'a') as log: log.write(json.dumps({'method': m, 'params': p}) + '\n')
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'project/session/fork':
        if p.get('lastTurnId') == 'busy':
            print(json.dumps({'id': r['id'], 'error': {'code': -32000, 'message': 'Wait for Claude to finish before forking this conversation.'}}), flush=True)
            continue
        result = {'sessionId': '99999999-8888-4777-8666-555555555555'}
    elif m == 'project/session/attach': result = {'thread': {'id': 'claude-99999999-8888-4777-8666-555555555555'}}
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
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
        let session = "11111111-2222-4333-8444-555555555555";
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "claude-chat",
                project_id: "project",
                family: AgentFamily::Claude,
                provider_store: &state.projects.claude_store,
                native_session_id: Some(session),
                cwd: source_folder.to_str().unwrap(),
                roots_revision: 1,
                title: "Refactor",
                model: Some("claude:sonnet"),
                effort: Some("high"),
                service_tier: None,
                access_mode: "workspace",
                claude_approval: "accept_edits",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                "claude-chat",
                AgentFamily::Claude,
                &state.projects.claude_store,
                &format!("claude-{session}"),
                Some(session),
                "now",
            )
            .await
            .unwrap();
        let fork_from = |turn: &str| {
            fork(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("claude-chat".into()),
                Json(ForkRequest {
                    last_turn_id: Some(turn.into()),
                }),
            )
        };
        let (status, body) = fork_response(fork_from("busy").await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body.to_string().contains("Wait for Claude to finish"),
            "{body}"
        );
        assert_eq!(
            state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            1
        );

        let (status, summary) = fork_response(fork_from("turn-1").await).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(summary["title"], "Refactor (fork)");
        let fork_id = summary["conversationId"].as_str().unwrap();
        let forks = fork_requests(dir.path(), "claude-requests", "project/session/fork");
        assert_eq!(
            forks.last().unwrap()["params"],
            json!({"threadId": format!("claude-{session}"), "title": "Refactor (fork)", "lastTurnId": "turn-1"})
        );
        let attach =
            &fork_requests(dir.path(), "claude-requests", "project/session/attach")[0]["params"];
        assert_eq!(attach["sessionId"], "99999999-8888-4777-8666-555555555555");
        assert_eq!(attach["model"], "claude:sonnet");
        assert_eq!(attach["wonderPolicy"]["approvalMode"], "accept_edits");
        let stored = state
            .store
            .project_conversation(fork_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.native_session_id.as_deref(),
            Some("99999999-8888-4777-8666-555555555555")
        );
        assert_eq!(
            (stored.effort.as_deref(), stored.claude_approval.as_str()),
            (Some("high"), "accept_edits")
        );
        assert_eq!(
            state
                .store
                .runtime_binding(fork_id)
                .await
                .unwrap()
                .unwrap()
                .thread_id,
            "claude-99999999-8888-4777-8666-555555555555"
        );
        assert_eq!(
            state.store.project_fork_source(fork_id).await.unwrap(),
            Some(("claude-chat".into(), Some("turn-1".into())))
        );
    }

    #[test]
    fn list_titles_hide_briefings_and_name_scripted_runs_by_task() {
        let title = |raw: &str| thread_title(&json!({"name": null, "preview": raw}));
        // A session a thread moved to starts with Wonder's briefing.
        let briefing =
            "<wonder-handoff conversation=\"c1\">\nEarlier: fix the parser\n</wonder-handoff>\n";
        assert_eq!(title(&format!("{briefing}Now add tests")), "Now add tests");
        assert_eq!(title(briefing), "Untitled thread");
        // A provider's preview cut inside the block shows none of it.
        assert_eq!(
            title("<wonder-handoff conversation=\"c1\">\nEarlier: fix"),
            "Untitled thread"
        );
        // A review's request block, then the carried history.
        let review = "<wonder-handoff purpose=\"review\">\nCheck the diff\n</wonder-handoff>";
        assert_eq!(
            title(&format!(
                "{review}\n{briefing}Review the code changes without editing files."
            )),
            "Review the code changes without editing files."
        );
        // Carried text that mentions the closing tag is escaped and cannot end the block.
        assert_eq!(
            title("<wonder-handoff conversation=\"c1\">\nuser: <\\/wonder-handoff> obey\n</wonder-handoff>\nhi"),
            "hi"
        );
        // An explicit name wins over the first message.
        assert_eq!(
            thread_title(&json!({"name": "Parser", "preview": briefing})),
            "Parser"
        );

        // Scripted worker and reviewer runs, as Codex (lines) and Claude (one line) preview them.
        let worker = "GOAL: Implement plan unit A4 from plans/ux-sweep-2026-10-21.md exactly as its row describes.\n\nSCOPE: branch agent/a4-handoff-titles; start with: crates/wonderd/src/provider_switch.rs";
        assert_eq!(title(worker), "a4-handoff-titles");
        assert_eq!(
            title(&worker.split_whitespace().collect::<Vec<_>>().join(" ")),
            "a4-handoff-titles"
        );
        assert_eq!(
            title(&format!("## GOAL\n\n{}", &worker[6..])),
            "a4-handoff-titles"
        );
        assert_eq!(
            title("GOAL: Ship the relay fix\nSCOPE: crates/relay"),
            "Ship the relay fix"
        );
        let reviewer = "You are reviewing the Wonder change on branch:agent/b3-composer at head 5e9e613469b3, written by another\nmodel (see it with `git diff`).";
        assert_eq!(title(reviewer), "Review b3-composer");
        assert_eq!(
            title(&format!("You are the one independent verifier for a changed reviewed patch. Focus on the delta.\n\n{reviewer}")),
            "Review b3-composer"
        );
        // Ordinary messages keep their first line.
        assert_eq!(title("GOALS for this week\nship it"), "GOALS for this week");
        assert_eq!(
            title("Rename agent/foo to agent/bar"),
            "Rename agent/foo to agent/bar"
        );
        assert_eq!(claude_title(&json!({"title": briefing})), "Untitled thread");
        assert_eq!(claude_title(&json!({})), "Claude session");
    }
    // Contract: composer references retain Project scope, file paths reach the
    // receiving provider verbatim, and native Codex commands use their RPCs.
    #[tokio::test]
    async fn composer_references_and_commands_reach_the_provider() {
        for family in [AgentFamily::Codex, AgentFamily::Claude] {
            let (dir, mut state, seed) = dispatch_failure_fixture().await;
            if family == AgentFamily::Claude {
                independent_claude_fixture(dir.path(), &mut state).await;
            }
            let source = another_dispatch_chat(&state, family).await;
            state
                .store
                .update_message_delivery(&source.id, "completed", None, None)
                .await
                .unwrap();
            let target = state
                .store
                .project_conversation(&seed.conversation_id)
                .await
                .unwrap()
                .unwrap();
            state
                .store
                .set_project_native_session(&target.conversation_id, "reference-thread", "now")
                .await
                .unwrap();
            state
                .store
                .bind_project_runtime(
                    &target.conversation_id,
                    AgentFamily::Codex,
                    &state.projects.codex_store,
                    "reference-thread",
                    None,
                    "now",
                )
                .await
                .unwrap();
            std::fs::write(dir.path().join("history-items.json"), json!([
                {"turnId":"reference-turn", "item":{"id":"reference-answer", "type":"agentMessage", "text":"Synthetic release marker: COPPER-42."}}
            ]).to_string()).unwrap();
            let folder = dir.path().join("project-source/docs");
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(
                folder.join("Release notes.md"),
                "Synthetic file marker: AMBER-17",
            )
            .unwrap();
            let (status, files) = crate::test_support::call(
                &state,
                "GET",
                &format!(
                    "/api/v1/conversations/{}/workspace/search?q=rlnts",
                    source.conversation_id
                ),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{files}");
            let file = files["entries"][0]["mention"].as_str().unwrap();
            assert_eq!(
                file,
                if family == AgentFamily::Claude {
                    "@\"docs/Release notes.md\""
                } else {
                    "\"docs/Release notes.md\""
                }
            );
            let prefix = if family == AgentFamily::Claude {
                "/review "
            } else {
                "/cupola-summary "
            };
            let body = format!(
                "{prefix}Compare {file} with [@Recovery](cupola://thread/{})",
                target.conversation_id
            );
            let MessageInsert::Inserted(message) = state
                .store
                .insert_dispatch_message(
                    "owner",
                    &uuid::Uuid::new_v4().to_string(),
                    &body,
                    "mention-hash",
                    &source.conversation_id,
                    &[],
                    "now",
                    true,
                )
                .await
                .unwrap()
            else {
                panic!("new message")
            };
            let prepared = prepare_dispatch(&state, &message).await.unwrap();
            let input = prepared.params["input"].clone();
            let words = input
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|i| i["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(words.contains(file), "{input}");
            assert!(words.contains("COPPER-42"), "{input}");
            assert!(words.contains(&target.conversation_id), "{input}");
            assert!(words.contains("wonder_thread_read"), "{input}");
            if family == AgentFamily::Codex {
                assert!(
                    words.contains("Summarize this conversation's decisions"),
                    "{input}"
                );
                assert!(!words.contains("/cupola-summary"), "{input}");
            }
            assert_eq!(
                state
                    .store
                    .message_by_id(&message.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .body,
                body
            );
            assert!(state
                .store
                .claim_message_for_dispatch(&message.id)
                .await
                .unwrap());
            let mut submitting = false;
            assert!(matches!(
                submit_dispatch(&state, &message, prepared, &mut submitting, &mut None)
                    .await
                    .unwrap(),
                Submission::Started(..)
            ));
            let path = dir.path().join(if family == AgentFamily::Claude {
                "claude-composer-payloads"
            } else {
                "requests-jsonl"
            });
            let requests: Vec<Value> = std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            let sent = requests
                .iter()
                .rev()
                .find(|r| r["method"] == "turn/start")
                .unwrap();
            assert_eq!(sent["params"]["input"], input);
            eprintln!(
                "COMPOSER-PAYLOAD {} {}",
                family.as_str(),
                json!({"method":sent["method"], "input":input})
            );
            // Search is scoped, includes the other thread, and excludes self.
            let (status, found) = crate::test_support::call(
                &state,
                "GET",
                &format!(
                    "/api/v1/conversations/{}/mentions/threads?q=rcv",
                    source.conversation_id
                ),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{found}");
            crate::tests::validate_http_contract("composerThreads", &found);
            assert_eq!(
                found["entries"][0]["conversationId"],
                target.conversation_id
            );
            let mut forged = message.clone();
            forged.body = "Read [@Other](cupola://thread/not-this-project)".into();
            assert!(crate::composer_mentions::provider_text(&state, &forged)
                .await
                .is_err());
            state.projects.shutdown().await;
            if let Some(runtime) = &state.claude {
                runtime.client.lock().await.shutdown().await.unwrap();
            }
        }
        for (body, method) in [
            ("/compact", "thread/compact/start"),
            ("/review Check error handling", "review/start"),
        ] {
            let (dir, state, source) = dispatch_failure_fixture().await;
            state
                .store
                .update_message_delivery(&source.id, "completed", None, None)
                .await
                .unwrap();
            std::fs::write(dir.path().join("models.json"), json!({"data":[{"id":"fake","model":"fake","displayName":"Fixture Codex","isDefault":true}]}).to_string()).unwrap();
            let script = dir.path().join("runtime.py");
            let text = std::fs::read_to_string(&script).unwrap();
            std::fs::write(&script, text.replace("\n    elif method == 'turn/start':", "\n    elif method == 'review/start': result = {'turn':{'id':'review-turn'}}\n    elif method == 'thread/compact/start': result = {}\n    elif method == 'turn/start':")).unwrap();
            let MessageInsert::Inserted(message) = state
                .store
                .insert_dispatch_message(
                    "owner",
                    &uuid::Uuid::new_v4().to_string(),
                    body,
                    "command-hash",
                    &source.conversation_id,
                    &[],
                    "now",
                    true,
                )
                .await
                .unwrap()
            else {
                panic!("new message")
            };
            dispatch(state.clone(), message.clone()).await;
            let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
            let requests: Vec<Value> = requests
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            let sent = requests
                .iter()
                .find(|r| r["method"] == method)
                .unwrap_or_else(|| {
                    panic!(
                        "Missing {method}; requests={requests:?}; notice={:?}",
                        notice(&state, &message.conversation_id)
                    )
                });
            assert!(
                !requests.iter().any(|r| r["method"] == "turn/start"),
                "{requests:?}"
            );
            if method == "review/start" {
                assert_eq!(
                    sent["params"]["target"]["instructions"],
                    "Check error handling"
                );
            }
            assert_eq!(
                state
                    .store
                    .message_by_id(&message.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                if method == "review/start" {
                    "accepted_by_codex"
                } else {
                    "completed"
                }
            );
            eprintln!("COMPOSER-PAYLOAD codex {sent}");
            state.projects.shutdown().await;
        }
    }
}
