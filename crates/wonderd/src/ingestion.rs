//! Durable notification projection and runtime recovery. Broadcast is only a
//! optional observer: SQLite owns every envelope before it can leave the reader.
use crate::AppState;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use wonder_app_server::{NotificationSink, RuntimeHealth};
use wonder_store::{AgentFamily, Store};

#[derive(Clone, Default)]
pub struct Ingestion {
    inner: Arc<Mutex<Status>>,
    claim: Arc<tokio::sync::Mutex<()>>,
    projection_locks: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
}

#[derive(Default)]
struct Status {
    providers: HashMap<AgentFamily, ProviderStatus>,
    heartbeat: Option<Instant>,
    consumer: Option<tokio::task::AbortHandle>,
    error: Option<String>,
    runtimes: HashMap<String, RuntimeRegistration>,
    /// The inbox row each lane's projection keeps failing on.
    failing: HashMap<String, LaneFailure>,
    active: HashMap<String, Option<String>>,
    /// Threads whose notification was recently set aside. Their later
    /// notifications are set aside on the first failure instead of each
    /// holding the inbox for the full limit again.
    set_aside_threads: VecDeque<(String, Instant)>,
    /// Which provider each recently registered runtime belonged to, dead ones
    /// included, so a failing notification can be attributed after a restart.
    runtime_families: VecDeque<(String, AgentFamily)>,
}

/// How long a notification may fail projection before it is quarantined (set
/// aside) so later chat updates, above all the turn's `turn/completed`, are
/// not held behind it. Native history recovery still reconciles the turn it
/// belonged to.
const FAILING_NOTIFICATION_LIMIT: Duration = Duration::from_secs(10);
/// A `turn/completed` is what ends a message's "working" state, so it is never
/// quarantined by the short limit. Only this last resort keeps one permanently
/// unprojectable terminal from holding its lane for good.
const FAILING_TERMINAL_LIMIT: Duration = Duration::from_secs(5 * 60);

/// The one notification a lane keeps failing on, and what that means for it.
/// `LaneFailure::fail` is the only transition: a lane is either retrying its
/// head row or has been told to quarantine it.
#[derive(Debug, Clone, Copy)]
struct LaneFailure {
    /// The inbox row; a failure recorded for another row never counts for it.
    id: i64,
    since: Instant,
    attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// Ends the turn and settles its message.
    Terminal,
    Other,
}

impl RowKind {
    fn of(method: &str) -> Self {
        if method == "turn/completed" {
            Self::Terminal
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureVerdict {
    /// Keep the row; it is retried.
    Retry,
    /// Acknowledge the row without projecting it.
    Quarantine,
}

impl LaneFailure {
    /// Records one failed attempt at `id` and says what to do with the row.
    /// `thread_quarantined`: an earlier row of the same thread was quarantined,
    /// so a later non-terminal failure is quarantined at once.
    fn fail(
        failures: &mut HashMap<String, LaneFailure>,
        lane: &str,
        id: i64,
        kind: RowKind,
        thread_quarantined: bool,
    ) -> (bool, FailureVerdict) {
        let (first, current) = match failures.get_mut(lane) {
            Some(current) if current.id == id => {
                current.attempts += 1;
                (false, *current)
            }
            _ => {
                let current = LaneFailure {
                    id,
                    since: Instant::now(),
                    attempts: 1,
                };
                failures.insert(lane.to_owned(), current);
                (true, current)
            }
        };
        let verdict = match kind {
            RowKind::Terminal if current.since.elapsed() >= FAILING_TERMINAL_LIMIT => {
                FailureVerdict::Quarantine
            }
            RowKind::Terminal => FailureVerdict::Retry,
            RowKind::Other
                if current.since.elapsed() >= FAILING_NOTIFICATION_LIMIT
                    || (first && thread_quarantined) =>
            {
                FailureVerdict::Quarantine
            }
            RowKind::Other => FailureVerdict::Retry,
        };
        (first, verdict)
    }
}

/// Delay before the notification consumer is restarted: doubles with each
/// consecutive short-lived run up to the cap, and starts over once a run has
/// stayed up for `STABLE`.
#[derive(Debug)]
struct RestartBackoff {
    next: Duration,
}

impl RestartBackoff {
    const FIRST: Duration = Duration::from_millis(250);
    const CAP: Duration = Duration::from_secs(30);
    const STABLE: Duration = Duration::from_secs(60);

    fn new() -> Self {
        Self { next: Self::FIRST }
    }

    fn after_run(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= Self::STABLE {
            self.next = Self::FIRST;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(Self::CAP);
        delay
    }
}
/// One projection attempt that has not finished by then counts as a failure,
/// so a hang is retried and eventually set aside like any other failure
/// instead of holding the inbox and the shared dispatch lock indefinitely.
const PROJECTION_ATTEMPT_LIMIT: Duration = Duration::from_secs(60);
const SET_ASIDE_THREADS: usize = 64;
const RUNTIME_FAMILIES: usize = 64;

#[derive(Default)]
struct ProviderStatus {
    health: Option<RuntimeHealth>,
    recovering: bool,
    // Keep a failed start visible while automatic retries continue.
    recovery_failed: bool,
    needs_validation: bool,
    reconcile_requested: bool,
    /// The bridge activated a different SDK; its model catalog is stale.
    discovery_requested: bool,
    /// The installed runtime was rejected as unsupported at its last start.
    incompatible: bool,
    /// The installed runtime changed since the running one started; it is
    /// replaced once the provider is idle.
    update_pending: bool,
    /// The runtime answered that OpenCode or Cursor is not usable yet. Its
    /// runtime stays up; new work is refused with this reason.
    setup: Option<crate::claude::SetupIssue>,
    /// Wakes the recovery loop from its retry sleep. A wake sent before the
    /// loop waits is kept, so a request is never left to the next timeout.
    wake: Arc<tokio::sync::Notify>,
    /// The latest reconcile request, and the latest one the recovery loop has
    /// finished, so Reconnect waits for its own request, not an earlier one.
    requested: u64,
    reconciled: u64,
}

impl ProviderStatus {
    /// Wake the recovery loop, fencing new work until it has finished when
    /// `fence`; returns the request.
    fn request(&mut self, fence: bool) -> u64 {
        self.reconcile_requested = true;
        self.recovering |= fence;
        self.requested += 1;
        self.wake.notify_one();
        self.requested
    }

    /// The loop finished recovering through request `served`. A request that
    /// arrived meanwhile keeps the provider fenced until it is served too.
    fn recovered(&mut self, served: u64) {
        self.reconciled = self.reconciled.max(served);
        self.recovery_failed = false;
        if !self.reconcile_requested {
            self.recovering = false;
        }
    }
}

impl Status {
    fn remember_family(&mut self, runtime_id: &str, family: AgentFamily) {
        if self.runtime_families.iter().any(|(id, _)| id == runtime_id) {
            return;
        }
        if self.runtime_families.len() >= RUNTIME_FAMILIES {
            self.runtime_families.pop_front();
        }
        self.runtime_families
            .push_back((runtime_id.to_owned(), family));
    }
}

/// One message's receipt was found: reconcile the provider that owns its
/// conversation, without making every other provider refuse work meanwhile.
/// A conversation whose provider cannot be read reconciles them all.
pub(crate) async fn request_conversation_reconcile(state: &AppState, conversation: &str) {
    match crate::claude::conversation_family(state, conversation).await {
        Ok(family) => {
            let mut status = state
                .ingestion
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // A provider without a recovery loop has nothing to reconcile.
            if let Some(provider) = status.providers.get_mut(&family) {
                provider.request(true);
            }
        }
        Err(_) => state.ingestion.request_reconciliation(),
    }
}

/// What wonder providers shows about a provider's runtime in wonderd.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderRuntimeState {
    pub alive: bool,
    pub recovering: bool,
    pub starting: bool,
    pub incompatible: bool,
    pub update_pending: bool,
    pub setup: Option<crate::claude::SetupIssue>,
}

/// Shown for a provider whose installed runtime Wonder refuses to start.
pub(crate) const CODEX_UNSUPPORTED_DETAIL: &str =
    "Codex was updated to a version Cupola doesn't support yet. Update Cupola on your computer.";

struct RuntimeRegistration {
    family: AgentFamily,
    health: RuntimeHealth,
    client: Weak<tokio::sync::Mutex<wonder_app_server::AppServerClient>>,
}

/// A snapshot of an existing runtime registration, not authority for an
/// arbitrary thread. Child discovery verifies the parent binding separately.
#[derive(Clone)]
pub(crate) struct RuntimeRoute {
    pub family: AgentFamily,
    pub runtime_id: String,
    client: Arc<tokio::sync::Mutex<wonder_app_server::AppServerClient>>,
}

impl RuntimeRoute {
    pub(crate) async fn rpc(&self) -> Result<wonder_app_server::RpcClient, String> {
        let rpc = self.client.lock().await.rpc();
        if rpc.health().id() != self.runtime_id || !rpc.health().is_alive() {
            return Err(
                "The provider runtime changed. Reopen the conversation and try again.".into(),
            );
        }
        Ok(rpc)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Readiness {
    pub ready: bool,
    pub detail: &'static str,
    pub reason: &'static str,
}

/// What `/readyz` and host status report: whether the host can run work
/// (any provider usable), and each provider's own answer, so one rejected
/// provider is reported as that provider's problem instead of the host's.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessReport {
    pub ready: bool,
    pub detail: &'static str,
    pub reason: &'static str,
    pub providers: Vec<ProviderReadiness>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderReadiness {
    pub id: &'static str,
    pub ready: bool,
    pub detail: &'static str,
    pub reason: &'static str,
}

impl Ingestion {
    pub(crate) async fn projection_lane(&self, thread: &str) -> tokio::sync::OwnedMutexGuard<()> {
        crate::projects::keyed_lock(&self.projection_locks, thread).await
    }

    /// Reconcile every provider (an update handoff). One message's receipt
    /// reconciles only its own provider (`request_conversation_reconcile`).
    pub(crate) fn request_reconciliation(&self) {
        let mut status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for provider in status.providers.values_mut() {
            provider.request(true);
        }
    }

    pub(crate) fn register_provider(
        &self,
        family: AgentFamily,
        client: &Arc<tokio::sync::Mutex<wonder_app_server::AppServerClient>>,
        health: RuntimeHealth,
    ) {
        let mut status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        status.providers.entry(family).or_default().health = Some(health.clone());
        status
            .runtimes
            .retain(|_, runtime| runtime.health.is_alive());
        status.remember_family(health.id(), family);
        status.runtimes.insert(
            health.id().to_owned(),
            RuntimeRegistration {
                family,
                health,
                client: Arc::downgrade(client),
            },
        );
    }

    /// Register the Project provider runtime and its generation-bound transport.
    pub(crate) fn register_project_runtime(
        &self,
        client: &Arc<tokio::sync::Mutex<wonder_app_server::AppServerClient>>,
        health: RuntimeHealth,
    ) {
        self.register_provider(AgentFamily::Codex, client, health);
    }

    /// Sending requires durable ingestion; provider startup is handled separately.
    pub(crate) async fn project_readiness(&self, store: &Store) -> Readiness {
        let issue = {
            let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if status.runtimes.values().any(|r| r.health.storage_blocked()) {
                Some(("Chat updates could not be saved. Cupola is retrying; check available disk space if this continues.", "storage_unavailable"))
            } else if status.error.is_some()
                || status
                    .consumer
                    .as_ref()
                    .is_none_or(tokio::task::AbortHandle::is_finished)
                || status
                    .heartbeat
                    .is_none_or(|time| time.elapsed() > Duration::from_secs(5))
            {
                Some((
                    "Cupola is recovering chat updates. Please wait before sending again.",
                    "recovering",
                ))
            } else {
                None
            }
        };
        if let Some((detail, reason)) = issue {
            return Readiness {
                ready: false,
                detail,
                reason,
            };
        }
        let failing = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failing
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        match store.notification_backlog_outside_lanes(&failing).await {
            Ok(0..=256) => Readiness {
                ready: true,
                detail: "Ready",
                reason: "ready",
            },
            Ok(_) => Readiness {
                ready: false,
                detail: "Cupola is catching up on chat updates. Please wait.",
                reason: "backlog",
            },
            Err(_) => Readiness {
                ready: false,
                detail:
                    "Chat storage is unavailable. Cupola is retrying; check available disk space.",
                reason: "storage_unavailable",
            },
        }
    }

    pub(crate) fn runtime_alive(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .get(id)
            .is_some_and(|r| r.health.is_alive())
    }

    pub(crate) fn runtime_routes(&self) -> Vec<RuntimeRoute> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .iter()
            .filter(|(_, registration)| registration.health.is_alive())
            .filter_map(|(id, registration)| {
                Some(RuntimeRoute {
                    family: registration.family,
                    runtime_id: id.clone(),
                    client: registration.client.upgrade()?,
                })
            })
            .collect()
    }

    pub(crate) fn runtime_route(&self, id: &str) -> Option<RuntimeRoute> {
        self.runtime_routes()
            .into_iter()
            .find(|route| route.runtime_id == id)
    }

    pub fn approval_client(
        &self,
        params: &Value,
    ) -> Option<Arc<tokio::sync::Mutex<wonder_app_server::AppServerClient>>> {
        let id = params.get("_wonderRuntimeId")?.as_str()?;
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .get(id)
            .filter(|r| r.health.is_alive())?
            .client
            .upgrade()
    }

    /// Whether work can start. Gating stays "any provider usable" so a broken
    /// Codex does not stop Claude chats.
    pub async fn readiness(&self, store: &Store) -> Readiness {
        self.provider_readiness(store, None).await
    }

    /// What `/readyz` and host status report: the gating answer ("any
    /// provider usable"), with each known provider's own readiness. A
    /// provider Cupola refuses to run is reported on that provider; the host
    /// says so only when no other provider can run work.
    pub async fn readiness_report(&self, store: &Store) -> ReadinessReport {
        let readiness = self.readiness(store).await;
        let mut families: Vec<AgentFamily> = {
            let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            status.providers.keys().copied().collect()
        };
        families.sort_by_key(|family| family.as_str());
        let mut providers = Vec::with_capacity(families.len());
        for family in families {
            let own = self.provider_readiness(store, Some(family)).await;
            providers.push(ProviderReadiness {
                id: family.as_str(),
                ready: own.ready,
                detail: own.detail,
                reason: own.reason,
            });
        }
        let (detail, reason) = if !readiness.ready
            && matches!(readiness.reason, "runtime_unavailable" | "recovering")
            && !providers.is_empty()
            && providers.iter().all(|p| p.reason == "runtime_incompatible")
        {
            (CODEX_UNSUPPORTED_DETAIL, "runtime_incompatible")
        } else {
            (readiness.detail, readiness.reason)
        };
        ReadinessReport {
            ready: readiness.ready,
            detail,
            reason,
            providers,
        }
    }

    /// Record the outcome of starting a provider runtime: rejected as
    /// unsupported, or not (started, or failed for another reason).
    pub(crate) fn set_runtime_incompatible(&self, family: AgentFamily, incompatible: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(family)
            .or_default()
            .incompatible = incompatible;
    }

    pub(crate) fn set_update_pending(&self, family: AgentFamily, pending: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(family)
            .or_default()
            .update_pending = pending;
    }

    /// Ask the provider's recovery loop to restart its runtime now. The
    /// provider counts as recovering until the loop has finished this
    /// request; wait for it with `reconnected`.
    pub(crate) fn request_reconcile(&self, family: AgentFamily) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(family)
            .or_default()
            .request(true)
    }

    /// Ask the recovery loop to reconcile a runtime its caller already
    /// started and verified (Codex's Reconnect), without fencing it.
    pub(crate) fn wake_reconcile(&self, family: AgentFamily) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(family)
            .or_default()
            .request(false);
    }

    /// After `request_reconcile` returned `request`: None until the recovery
    /// loop has finished it and the provider's own runtime (not an added
    /// account's) is running; then what still keeps the provider from work.
    pub(crate) fn reconnected(
        &self,
        family: AgentFamily,
        request: u64,
    ) -> Option<Option<crate::claude::SetupIssue>> {
        let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let provider = status.providers.get(&family)?;
        (provider.reconciled >= request
            && !provider.recovering
            && provider
                .health
                .as_ref()
                .is_some_and(RuntimeHealth::is_alive))
        .then_some(provider.setup)
    }

    /// The provider a runtime belonged to, while it is remembered.
    fn runtime_family(&self, runtime_id: &str) -> Option<AgentFamily> {
        let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        status
            .runtime_families
            .iter()
            .rev()
            .find(|(id, _)| id == runtime_id)
            .map(|(_, family)| *family)
    }

    /// A provider whose runtime answered with an unusable catalog keeps that
    /// runtime, which is not used until a later check validates it.
    pub(crate) fn set_needs_validation(&self, family: AgentFamily, needed: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(family)
            .or_default()
            .needs_validation = needed;
    }

    pub(crate) fn needs_validation(&self, family: AgentFamily) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .get(&family)
            .is_some_and(|p| p.needs_validation)
    }

    /// Ask the recovery loop to reread the catalog of the provider that owns
    /// this live runtime. A notification from a replaced runtime is ignored.
    pub(crate) fn request_discovery(&self, runtime_id: &str) {
        let mut status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(family) = status
            .runtimes
            .get(runtime_id)
            .filter(|r| r.health.is_alive())
            .map(|r| r.family)
        else {
            return;
        };
        status
            .providers
            .entry(family)
            .or_default()
            .discovery_requested = true;
    }

    pub(crate) fn provider_runtime_state(&self, family: AgentFamily) -> ProviderRuntimeState {
        let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let provider = status.providers.get(&family);
        ProviderRuntimeState {
            alive: provider
                .and_then(|p| p.health.as_ref())
                .is_some_and(RuntimeHealth::is_alive)
                || status
                    .runtimes
                    .values()
                    .any(|r| r.family == family && r.health.is_alive()),
            recovering: provider.is_some_and(|p| p.recovering),
            starting: provider.is_some_and(|p| p.recovering && !p.recovery_failed),
            incompatible: provider.is_some_and(|p| p.incompatible),
            update_pending: provider.is_some_and(|p| p.update_pending),
            setup: provider.and_then(|p| p.setup),
        }
    }

    /// Record what the last check of `family` found; None means usable.
    pub(crate) fn set_setup_issue(
        &self,
        family: AgentFamily,
        issue: Option<crate::claude::SetupIssue>,
    ) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(family)
            .or_default()
            .setup = issue;
    }

    pub(crate) fn setup_issue(&self, family: AgentFamily) -> Option<crate::claude::SetupIssue> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .get(&family)
            .and_then(|p| p.setup)
    }

    /// The setup issue of a provider whose runtime is running, if any.
    pub(crate) fn parked(&self, family: AgentFamily) -> Option<crate::claude::SetupIssue> {
        let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        status
            .providers
            .get(&family)
            .filter(|p| p.health.as_ref().is_some_and(RuntimeHealth::is_alive))
            .and_then(|p| p.setup)
    }

    /// Explicit Providers refresh or a setup change checks this bridge
    /// provider now, including a ready provider retaining a stale catalog.
    /// Routine reads leave freshness and retry timing to the catalog owner.
    pub(crate) fn wake_setup_check(&self, family: AgentFamily) {
        if !bridge_family(family) {
            return;
        }
        let mut status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(provider) = status.providers.get_mut(&family) {
            provider.discovery_requested = true;
            provider.wake.notify_one();
        }
    }

    fn provider_incompatible(&self, family: Option<AgentFamily>) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .iter()
            .any(|(id, p)| p.incompatible && family.is_none_or(|family| family == *id))
    }

    async fn provider_readiness(&self, store: &Store, family: Option<AgentFamily>) -> Readiness {
        if family.is_some() && self.provider_incompatible(family) {
            return Readiness {
                ready: false,
                detail: CODEX_UNSUPPORTED_DETAIL,
                reason: "runtime_incompatible",
            };
        }
        let issue = {
            let status = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let runtime_healthy = status.providers.iter().any(|(id, p)| {
                family.is_none_or(|family| family == *id)
                    && p.health.as_ref().is_some_and(RuntimeHealth::is_alive)
            });
            let available = status.providers.iter().any(|(id, p)| {
                family.is_none_or(|family| family == *id)
                    && !p.recovering
                    && p.setup.is_none()
                    && p.health.as_ref().is_some_and(RuntimeHealth::is_alive)
            });
            // A running runtime whose provider is not set up: say what to do,
            // not that it is reconnecting.
            let setup = family.and_then(|family| {
                status
                    .providers
                    .get(&family)
                    .filter(|p| {
                        !p.recovering && p.health.as_ref().is_some_and(RuntimeHealth::is_alive)
                    })
                    .and_then(|p| p.setup)
                    .map(|issue| issue.detail(family))
            });
            if status.runtimes.values().any(|r| r.health.storage_blocked()) {
                Some(("Chat updates could not be saved. Cupola is retrying; check available disk space if this continues.", "storage_unavailable"))
            } else if status.error.is_some() {
                Some((
                    "Cupola is recovering chat updates. Please wait before sending again.",
                    "recovering",
                ))
            } else if let Some(detail) = setup {
                Some((detail, "not_set_up"))
            } else if !available {
                Some(("This agent is reconnecting. Check its sign-in and runtime with cupola providers on your computer if this continues.", if runtime_healthy { "recovering" } else { "runtime_unavailable" }))
            } else if status
                .consumer
                .as_ref()
                .is_none_or(tokio::task::AbortHandle::is_finished)
                || status
                    .heartbeat
                    .is_none_or(|time| time.elapsed() > Duration::from_secs(5))
            {
                Some((
                    "Cupola is recovering chat updates. Please wait before sending again.",
                    "recovering",
                ))
            } else {
                None
            }
        };
        if let Some((detail, reason)) = issue {
            return Readiness {
                ready: false,
                detail,
                reason,
            };
        }
        let failing = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failing
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        match store.notification_backlog_outside_lanes(&failing).await {
            Ok(0..=256) => Readiness {
                ready: true,
                detail: "Ready",
                reason: "ready",
            },
            Ok(_) => Readiness {
                ready: false,
                detail: "Cupola is catching up on chat updates. Please wait.",
                reason: "backlog",
            },
            Err(_) => Readiness {
                ready: false,
                detail:
                    "Chat storage is unavailable. Cupola is retrying; check available disk space.",
                reason: "storage_unavailable",
            },
        }
    }

    fn error(&self, error: Option<String>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).error = error;
    }
}

/// Wakes the projector as soon as a runtime notification is stored, instead of
/// leaving each streamed delta to its next idle poll (up to 100 ms later). A
/// wakeup without new work only costs one inbox read.
static PROJECTOR_WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn notification_sink(store: Store) -> NotificationSink {
    let redactor = Arc::new(tokio::sync::Mutex::new(
        crate::secrets::NotificationRedactor::default(),
    ));
    Arc::new(move |notification| {
        let store = store.clone();
        let redactor = redactor.clone();
        Box::pin(async move {
            // Commit stream state only after durable acceptance. A disk-full
            // retry must not join leaked prefixes with an unredacted suffix.
            let mut committed = redactor.lock().await;
            let mut candidate = committed.clone();
            for notification in candidate.ingest(notification) {
                if crate::context_usage::record_notification(&notification) {
                    continue;
                }
                store
                    .enqueue_notification(&notification)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            *committed = candidate;
            PROJECTOR_WAKE.notify_one();
            Ok(())
        })
    })
}

struct AbortChild(tokio::task::AbortHandle);
impl Drop for AbortChild {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How often a working provider's catalog is read again. The read is cheap:
/// the runtime answers from its own catalog unless that is stale.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(300);
/// A provider that is not installed, signed out or has no models is checked
/// again this often; Reconnect and opening Providers check it at once.
const SETUP_RECHECK: Duration = Duration::from_secs(600);
/// A failed check is retried from this, doubling up to the discovery interval.
const CHECK_RETRY_MIN: Duration = Duration::from_secs(30);

fn recheck_after(issue: crate::claude::SetupIssue, check_retry: &mut Duration) -> Duration {
    if issue != crate::claude::SetupIssue::CheckFailed {
        return SETUP_RECHECK;
    }
    let after = *check_retry;
    *check_retry = (after * 2).min(DISCOVERY_INTERVAL);
    after
}

fn bridge_family(family: AgentFamily) -> bool {
    matches!(
        family,
        AgentFamily::Claude | AgentFamily::OpenCode | AgentFamily::Cursor
    )
}

/// Logs a provider that is not set up once per reason, not at every re-check.
fn log_setup(
    state: &AppState,
    family: AgentFamily,
    issue: crate::claude::SetupIssue,
    diagnostic: Option<String>,
    logged: &mut Option<String>,
) {
    let detail = issue.detail(family);
    if logged.as_deref() == Some(detail) {
        return;
    }
    let event = if issue == crate::claude::SetupIssue::CheckFailed {
        "provider_check_failed"
    } else {
        "provider_not_set_up"
    };
    let _ = state.logger.record(
        "info",
        event,
        json!({"agentFamily":family,"detail":detail,"diagnostic":diagnostic}),
    );
    *logged = Some(detail.to_owned());
}

pub struct NotificationService {
    ingestion: Ingestion,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for NotificationService {
    fn drop(&mut self) {
        for provider in self
            .ingestion
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .values_mut()
        {
            provider.recovering = true;
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub async fn spawn(state: AppState) -> NotificationService {
    let mut families = vec![AgentFamily::Codex];
    if state.claude.is_some() {
        families.push(AgentFamily::Claude);
    }
    if state.opencode.is_some() {
        families.push(AgentFamily::OpenCode);
    }
    if state.cursor.is_some() {
        families.push(AgentFamily::Cursor);
    }
    for family in &families {
        let client = crate::claude::client(&state, *family).expect("configured provider");
        let health = client.lock().await.health();
        state.ingestion.register_provider(*family, &client, health);
        state
            .ingestion
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .providers
            .entry(*family)
            .or_default()
            .recovering = true;
    }
    let projection_state = state.clone();
    let projection = tokio::spawn(async move {
        let mut backoff = RestartBackoff::new();
        loop {
            let started = Instant::now();
            // A separate child makes panic/cancellation observable. The durable
            // row remains until the entire projection reports success.
            let child_state = projection_state.clone();
            let mut child = tokio::spawn(async move { project_loop(child_state).await });
            {
                projection_state.ingestion.inner.lock().unwrap().consumer =
                    Some(child.abort_handle());
            }
            let _abort = AbortChild(child.abort_handle());
            let outcome = (&mut child).await;
            projection_state
                .ingestion
                .error(Some(format!("notification consumer stopped: {outcome:?}")));
            let delay = backoff.after_run(started.elapsed());
            let _ = projection_state.logger.record(
                "error",
                "notification_consumer_restarting",
                json!({"outcome": format!("{outcome:?}"), "delayMs": delay.as_millis() as u64}),
            );
            tokio::time::sleep(delay).await;
        }
    });
    let ingestion = state.ingestion.clone();
    let maintenance_state = state.clone();
    let mut recovery_tasks = Vec::new();
    for family in families {
        let state = state.clone();
        recovery_tasks.push(tokio::spawn(async move {
            let mut needs_recovery = true;
            let mut recover_all = true;
            let mut delay = Duration::from_secs(1);
            let wake = state
                .ingestion
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .providers
                .entry(family)
                .or_default()
                .wake
                .clone();
            // The latest reconcile request this loop has taken on.
            let mut serving = 0;
            // A provider that is not set up (OpenCode not installed, Cursor
            // signed out) fails the same way at every retry: log it once, and
            // again only when the reason changes or after it recovers.
            let mut logged_failure: Option<String> = None;
            let mut next_discovery = std::time::Instant::now() + DISCOVERY_INTERVAL;
            // A failed check of a provider that is otherwise working is tried
            // again sooner, with its own backoff.
            let mut check_retry = CHECK_RETRY_MIN;
            loop {
                // The sidecar can activate an SDK patch or observe a new login
                // without restarting Wonder. Check its catalog now when it
                // reports an activation or the owner asks (Reconnect, opening
                // Providers). The periodic read lets the runtime answer from
                // its own catalog, which it checks again only when stale or
                // when the provider's CLI or settings changed.
                let requested = bridge_family(family)
                    && !needs_recovery
                    && std::mem::take(
                        &mut state
                            .ingestion
                            .inner
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .providers
                            .entry(family)
                            .or_default()
                            .discovery_requested,
                    );
                if bridge_family(family)
                    && !needs_recovery
                    && (requested || std::time::Instant::now() >= next_discovery)
                {
                    next_discovery = std::time::Instant::now() + DISCOVERY_INTERVAL;
                    if let Ok(client) = crate::claude::client(&state, family) {
                        // A model check must not own the lifecycle lock:
                        // sends need it briefly to capture this same transport.
                        let rpc = client.lock().await.rpc();
                        let outcome =
                            crate::claude::discover_with(&state, &rpc, family, requested).await;
                        let parked = state.ingestion.setup_issue(family);
                        match outcome {
                            Ok(()) => {
                                check_retry = CHECK_RETRY_MIN;
                                if parked.is_none() {
                                    logged_failure = None;
                                } else {
                                    // Set up now: finish recovery (approvals,
                                    // history) as after a restart.
                                    state.ingestion.set_setup_issue(family, None);
                                    needs_recovery = true;
                                }
                            }
                            Err(crate::claude::DiscoveryFailure::NotSetUp {
                                issue,
                                diagnostic,
                            }) => {
                                // A working provider whose check failed keeps
                                // working; a confirmed answer replaces it.
                                if issue != crate::claude::SetupIssue::CheckFailed
                                    || parked.is_some()
                                {
                                    state.ingestion.set_setup_issue(family, Some(issue));
                                }
                                next_discovery = std::time::Instant::now()
                                    + recheck_after(issue, &mut check_retry);
                                log_setup(&state, family, issue, diagnostic, &mut logged_failure);
                            }
                            Err(
                                crate::claude::DiscoveryFailure::Runtime(_)
                                | crate::claude::DiscoveryFailure::Unreachable(_),
                            ) => {
                                needs_recovery = true;
                                state.ingestion.set_needs_validation(family, true);
                            }
                        }
                    }
                }
                {
                    let mut status = state
                        .ingestion
                        .inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let provider = status.providers.entry(family).or_default();
                    if std::mem::take(&mut provider.reconcile_requested) {
                        // Someone asked (Reconnect, a found receipt): retry
                        // now and from the shortest delay, not the backoff.
                        serving = provider.requested;
                        delay = Duration::from_secs(1);
                        needs_recovery = true;
                        recover_all = true;
                    } else if provider.health.as_ref().is_none_or(|h| !h.is_alive()) {
                        needs_recovery = true;
                        recover_all = true;
                    }
                    if needs_recovery {
                        status.providers.entry(family).or_default().recovering = true;
                    }
                }
                if needs_recovery {
                    match recover(&state, family, recover_all).await {
                        Ok(()) => {
                            let announce = logged_failure.take().is_some() || recover_all;
                            needs_recovery = false;
                            recover_all = false;
                            delay = Duration::from_secs(1);
                            check_retry = CHECK_RETRY_MIN;
                            let mut status = state
                                .ingestion
                                .inner
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            status
                                .runtimes
                                .retain(|_, r| r.family != family || r.health.is_alive());
                            status
                                .providers
                                .entry(family)
                                .or_default()
                                .recovered(serving);
                            drop(status);
                            if announce {
                                let _ = state.logger.record(
                                    "info",
                                    "notification_runtime_recovered",
                                    json!({"agentFamily":family}),
                                );
                            }
                        }
                        Err(error)
                            if state
                                .ingestion
                                .parked(family)
                                .is_some_and(|issue| issue.detail(family) == error) =>
                        {
                            // The runtime is up and answered that the provider
                            // is not set up: wait for its slow re-check or a
                            // wake instead of restarting it every 30 seconds.
                            let issue = state.ingestion.parked(family).expect("checked");
                            needs_recovery = false;
                            recover_all = false;
                            delay = Duration::from_secs(1);
                            next_discovery =
                                std::time::Instant::now() + recheck_after(issue, &mut check_retry);
                            let mut status = state
                                .ingestion
                                .inner
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            status
                                .providers
                                .entry(family)
                                .or_default()
                                .recovered(serving);
                            drop(status);
                            log_setup(&state, family, issue, None, &mut logged_failure);
                        }
                        Err(error) => {
                            state
                                .ingestion
                                .inner
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .providers
                                .entry(family)
                                .or_default()
                                .recovery_failed = true;
                            if logged_failure.as_ref() != Some(&error) {
                                let _ = state.logger.record(
                                    "error",
                                    "notification_runtime_recovery_failed",
                                    json!({"agentFamily":family,"error":error}),
                                );
                                logged_failure = Some(error);
                            }
                            delay = (delay * 2).min(Duration::from_secs(30));
                        }
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = wake.notified() => {}
                }
            }
        }));
    }
    let maintenance = tokio::spawn(async move {
        // Each second the replay cap writers leave slack in is enforced (a
        // read only, unless over it); expired replay and receipts, whose
        // bound is a week, go every few minutes (and at startup).
        let mut aged_at: Option<Instant> = None;
        // Store::connect ran the first cleanup at startup.
        let mut next_cleanup = Instant::now() + wonder_store::PERIODIC_CLEANUP_INTERVAL;
        loop {
            let pass = if aged_at.is_none_or(|at| at.elapsed() >= REPLAY_AGE_INTERVAL) {
                aged_at = Some(Instant::now());
                maintenance_state.store.prune_replay().await
            } else {
                maintenance_state.store.enforce_replay_cap().await
            };
            if let Err(error) = pass {
                let _ = maintenance_state.logger.record(
                    "error",
                    "replay_retention_failed",
                    json!({"error":error.to_string()}),
                );
            }
            if Instant::now() >= next_cleanup {
                next_cleanup = Instant::now() + wonder_store::PERIODIC_CLEANUP_INTERVAL;
                if let Err(error) = maintenance_state
                    .store
                    .run_periodic_cleanup(crate::now_ms() as i64)
                    .await
                {
                    let _ = maintenance_state.logger.record(
                        "error",
                        "store_cleanup_failed",
                        json!({"error":error.to_string()}),
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    recovery_tasks.extend([projection, maintenance]);
    NotificationService {
        ingestion,
        tasks: recovery_tasks,
    }
}

/// Expired replay and notification receipts are pruned this often.
const REPLAY_AGE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long an idle projector worker waits for a wakeup before it looks for
/// work no wakeup announced (a lane another worker released, a retry): 100 ms
/// after recent work or a failure, doubling while idle up to the maximum.
const PROJECTOR_POLL: Duration = Duration::from_millis(100);
const PROJECTOR_IDLE_POLL: Duration = Duration::from_secs(1);

fn next_projector_poll(current: Duration) -> Duration {
    (current * 2).min(PROJECTOR_IDLE_POLL)
}

/// How often the projector proves it is alive for readiness, which allows
/// five seconds without it.
const PROJECTOR_HEARTBEAT: Duration = Duration::from_secs(1);

async fn project_loop(state: AppState) {
    // Four owned workers: a lane's slow preparation never occupies all of them.
    // Dropping this JoinSet aborts workers and releases their RAII claims.
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let state = state.clone();
        workers.spawn(async move {
            let mut poll = PROJECTOR_POLL;
            loop {
                match project_next(&state).await {
                    Ok(true) => {
                        poll = PROJECTOR_POLL;
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        poll = PROJECTOR_POLL;
                        let _ = state.logger.record(
                            "warn",
                            "notification_projection_retry",
                            json!({"error":error}),
                        );
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(poll) => poll = next_projector_poll(poll),
                    _ = PROJECTOR_WAKE.notified() => poll = PROJECTOR_POLL,
                }
            }
        });
    }
    loop {
        tokio::select! {
            _ = tokio::time::sleep(PROJECTOR_HEARTBEAT) => {
                let mut status = state.ingestion.inner.lock().unwrap_or_else(|e| e.into_inner());
                status.heartbeat = Some(Instant::now());
                status.error = None;
            }
            result = workers.join_next() => panic!("projection worker stopped: {result:?}"),
        }
    }
}

struct LaneClaim {
    ingestion: Ingestion,
    lane: String,
}
impl Drop for LaneClaim {
    fn drop(&mut self) {
        self.ingestion
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .remove(&self.lane);
    }
}

// Only already-durable adjacent deltas are coalesced: no fill timer, no extra
// streaming latency, and requests/terminals/other items are hard boundaries.
const DELTA_BATCH_ROWS: i64 = 32;
const DELTA_BATCH_BYTES: usize = 64 * 1024;

#[cfg(test)]
pub(crate) static PREPARING: std::sync::LazyLock<
    Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
pub(crate) struct DeltaBatch {
    pub(crate) deltas: Vec<(String, String)>,
    ids: Vec<i64>,
    // Early returns (unknown ownership, stale generation, receipt replay) must
    // acknowledge only the head; the rest have not been projected at all.
    pub(crate) consumed: bool,
}

async fn delta_batch(state: &AppState, id: i64, head: &Value) -> Result<DeltaBatch, String> {
    fn identity(n: &Value) -> Option<(Option<String>, String, String, String)> {
        if n.get("method")?.as_str()? != "item/agentMessage/delta" || n.get("id").is_some() {
            return None;
        }
        let p = n.get("params")?;
        Some((
            n.get("_wonderRuntimeId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            crate::notification_thread_id(p)?,
            crate::notification_turn_id(p)?,
            crate::string_field(p, "itemId")
                .or_else(|| p.get("item").and_then(|i| crate::string_field(i, "id")))?,
        ))
    }
    fn delta(n: &Value) -> Option<(String, String)> {
        let p = n.get("params")?;
        Some((
            crate::extract_text(p.get("delta")?)?,
            crate::app_server_notification_key("item/agentMessage/delta", n, p)?,
        ))
    }
    let mut batch = DeltaBatch::default();
    let (Some(key), Some(first)) = (identity(head), delta(head)) else {
        return Ok(batch);
    };
    let mut bytes = first.0.len();
    batch.deltas.push(first);
    batch.ids.push(id);
    if bytes > DELTA_BATCH_BYTES {
        return Ok(batch);
    }
    for (next_id, mut n) in state
        .store
        .notification_prefix_after(id, DELTA_BATCH_ROWS - 1, (DELTA_BATCH_BYTES * 2) as i64)
        .await
        .map_err(|e| e.to_string())?
    {
        n["_wonderInboxId"] = json!(next_id);
        if identity(&n).as_ref() != Some(&key)
            || crate::request_id(&n, &n["params"]) != crate::request_id(head, &head["params"])
        {
            break;
        }
        let Some(next) = delta(&n) else {
            break;
        };
        bytes += next.0.len();
        if bytes > DELTA_BATCH_BYTES {
            break;
        }
        batch.deltas.push(next);
        batch.ids.push(next_id);
    }
    Ok(batch)
}

async fn project_next(state: &AppState) -> Result<bool, String> {
    project_next_with_limit(state, PROJECTION_ATTEMPT_LIMIT).await
}

async fn project_next_with_limit(
    state: &AppState,
    attempt_limit: Duration,
) -> Result<bool, String> {
    project_next_outside(state, &[], attempt_limit)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug)]
enum ProjectionError {
    /// The head of `lane` failed and stays in the inbox for a retry.
    Failing {
        id: i64,
        lane: String,
        /// The provider that sent it, when known.
        family: Option<AgentFamily>,
    },
    Storage(String),
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failing { id, .. } => write!(f, "notification {id} could not be projected"),
            Self::Storage(error) => f.write_str(error),
        }
    }
}

impl From<String> for ProjectionError {
    fn from(error: String) -> Self {
        Self::Storage(error)
    }
}

/// Projects the next inbox row outside the active lanes and `skip`.
async fn project_next_outside(
    state: &AppState,
    skip: &[String],
    attempt_limit: Duration,
) -> Result<bool, ProjectionError> {
    let selection = state.ingestion.claim.lock().await;
    let excluded = {
        let status = state
            .ingestion
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if status.active.len() >= 4 {
            return Ok(false);
        }
        status
            .active
            .iter()
            .flat_map(|(lane, thread)| {
                std::iter::once(lane.clone()).chain(thread.as_ref().map(|t| format!("thread:{t}")))
            })
            .chain(skip.iter().cloned())
            .collect::<Vec<_>>()
    };
    let Some((id, mut notification, lane)) = state
        .store
        .next_notification_outside_lanes(&excluded)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(false);
    };
    state
        .ingestion
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active
        .insert(
            lane.clone(),
            crate::notification_thread_id(&notification["params"]),
        );
    let _claim = LaneClaim {
        ingestion: state.ingestion.clone(),
        lane: lane.clone(),
    };
    drop(selection);
    let thread = crate::notification_thread_id(&notification["params"]);
    let _lane = match thread.as_deref() {
        Some(thread) => Some(state.ingestion.projection_lane(thread).await),
        None => None,
    };
    // Unsequenced deltas still need a stable identity when replayed.
    notification["_wonderInboxId"] = json!(id);
    let params = notification.get("params").cloned().unwrap_or_default();
    let method = notification
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if state
        .store
        .notification_was_started(id)
        .await
        .map_err(|e| e.to_string())?
    {
        if let Some(key) = crate::app_server_notification_key(method, &notification, &params) {
            state
                .store
                .release_app_server_notification(&key)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    state
        .store
        .start_notification(id)
        .await
        .map_err(|e| e.to_string())?;
    if notification.get("id").is_some()
        && notification
            .get("_wonderRuntimeId")
            .and_then(Value::as_str)
            .is_some_and(|id| !state.ingestion.runtime_alive(id))
    {
        // History can recover messages, but an RPC request from a dead
        // transport is no longer actionable.
        state
            .store
            .acknowledge_notification(id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(true);
    }
    let method = method.to_owned();
    // Coalescing is only an optimization. A failed read of later rows must not
    // skip the failure/set-aside bookkeeping below: project the head alone.
    let mut batch = match delta_batch(state, id, &notification).await {
        Ok(batch) => batch,
        Err(error) => {
            let _ = state.logger.record(
                "warn",
                "notification_delta_batch_failed",
                json!({"inboxId": id, "error": error}),
            );
            DeltaBatch::default()
        }
    };
    let thread = crate::notification_thread_id(&params);
    let runtime = notification
        .get("_wonderRuntimeId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let projected = tokio::time::timeout(
        attempt_limit,
        crate::apply_app_server_notification_with_deltas(
            state,
            notification,
            false,
            false,
            Some(&mut batch),
        ),
    )
    .await;
    if projected != Ok(true) {
        let timed_out = projected.is_err();
        let (first, verdict) = {
            let mut status = state
                .ingestion
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            status
                .set_aside_threads
                .retain(|(_, at)| at.elapsed() < FAILING_TERMINAL_LIMIT * 2);
            let thread_set_aside = thread
                .as_deref()
                .is_some_and(|t| status.set_aside_threads.iter().any(|(s, _)| s == t));
            LaneFailure::fail(
                &mut status.failing,
                &lane,
                id,
                RowKind::of(&method),
                thread_set_aside,
            )
        };
        if first {
            let _ = state.logger.record(
                "warn",
                "notification_projection_failing",
                json!({"inboxId": id, "method": method, "threadId": thread, "timedOut": timed_out}),
            );
        }
        if verdict == FailureVerdict::Retry {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let family = match runtime.and_then(|r| state.ingestion.runtime_family(&r)) {
                Some(family) => Some(family),
                // A conversation's lane is its id.
                None if !lane.starts_with("thread:") && lane != "unthreaded" => {
                    crate::claude::conversation_family(state, &lane).await.ok()
                }
                None => None,
            };
            return Err(ProjectionError::Failing { id, lane, family });
        }
        let _ = state.logger.record(
            "error",
            "notification_set_aside",
            json!({"inboxId": id, "method": method, "threadId": thread}),
        );
        if let Some(thread) = thread {
            let mut status = state
                .ingestion
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            status.set_aside_threads.retain(|(t, _)| *t != thread);
            if status.set_aside_threads.len() >= SET_ASIDE_THREADS {
                status.set_aside_threads.pop_front();
            }
            status.set_aside_threads.push_back((thread, Instant::now()));
        }
    }
    state
        .ingestion
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .failing
        .remove(&lane);
    // An empty batch (nothing coalesced, or the batch read failed) still
    // acknowledges its head.
    let ids = if projected == Ok(true) && batch.consumed && !batch.ids.is_empty() {
        batch.ids
    } else {
        vec![id]
    };
    state
        .store
        .acknowledge_notifications(&ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

async fn recover(state: &AppState, family: AgentFamily, _recover_all: bool) -> Result<(), String> {
    settle_inbox(state, family).await?;
    // A bridge runtime whose catalog failed validation is validated again
    // before it is used (`projects::bridge_rpc`).
    crate::projects::rpc_for(state, family).await?;
    retire_lost_approvals(state).await
}

/// Committed completion envelopes settle before `family`'s runtime history is
/// inspected. A failing lane another provider sent is left to the projection
/// workers, which own its retries and set-aside; it does not hold this
/// provider's recovery. A failure of this provider's own, or one that cannot
/// be attributed, still does, and storage errors are never skipped.
async fn settle_inbox(state: &AppState, family: AgentFamily) -> Result<(), String> {
    let mut others = Vec::new();
    loop {
        match project_next_outside(state, &others, PROJECTION_ATTEMPT_LIMIT).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(ProjectionError::Failing {
                lane,
                family: Some(owner),
                ..
            }) if owner != family => others.push(lane),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// No request from a previous transport can be answered on the replacement.
/// A question T3 Code asks, or a request ChatGPT's desktop app asks, has no
/// provider runtime and is not one of these (`t3_code::questions`,
/// `chatgpt_approvals`).
pub(crate) async fn retire_lost_approvals(state: &AppState) -> Result<(), String> {
    for approval in state
        .store
        .list_pending_approvals()
        .await
        .map_err(|e| e.to_string())?
    {
        let params: Value =
            serde_json::from_str(&approval.params_json).map_err(|e| e.to_string())?;
        if crate::t3_code::questions::asked_in_t3_code(&approval.server_request_id, &params)
            || crate::chatgpt_approvals::asked_in_chatgpt(&approval.server_request_id, &params)
        {
            continue;
        }
        if params
            .get("_wonderRuntimeId")
            .and_then(Value::as_str)
            .is_none_or(|id| !state.ingestion.runtime_alive(id))
        {
            state
                .store
                .resolve_approval_by_server_request_id(
                    &approval.server_request_id,
                    "app_server_lost",
                    &crate::now_ms().to_string(),
                )
                .await
                .map_err(|e| e.to_string())?;
            crate::publish_event(
                state,
                wonder_api::WonderEvent::ApprovalResolved {
                    request_id: approval.server_request_id,
                    decision: "app_server_lost".into(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// When the host starts, nothing watches ChatGPT or T3 Code yet, so a
/// question one of them asked before the restart could outlive its turn on
/// the phone. Each one still offered is read back from its asker once, a thread
/// at a time (`chatgpt_approvals::recheck`, `t3_code::questions::recheck`);
/// one whose conversation can't be found is left to the computer.
pub(crate) async fn recheck_desktop_approvals(state: &AppState) {
    let Ok(pending) = state.store.list_pending_approvals().await else {
        return;
    };
    // By asker (T3 Code or not) and thread.
    let mut threads: std::collections::BTreeMap<(bool, String), Vec<_>> = Default::default();
    for approval in pending {
        let Ok(params) = serde_json::from_str::<Value>(&approval.params_json) else {
            continue;
        };
        let key = &approval.server_request_id;
        let t3 = crate::t3_code::questions::asked_in_t3_code(key, &params);
        // A T3 Code question being answered from the phone is that answer's
        // to settle. No answer is under way after a restart, so a ChatGPT
        // request left `resolving` is read back and closed like a pending one.
        if t3 && approval.state != "pending" {
            continue;
        }
        if t3 || crate::chatgpt_approvals::asked_in_chatgpt(key, &params) {
            threads
                .entry((t3, approval.thread_id.clone()))
                .or_default()
                .push(approval);
        }
    }
    for ((t3, thread), asked) in threads {
        let conversation = if t3 {
            state.store.t3_code_conversation(&thread).await
        } else {
            state
                .store
                .chatgpt_request_conversation(&thread, &asked[0].turn_id)
                .await
        };
        let waiting: Vec<(String, String)> = asked
            .into_iter()
            .map(|approval| (approval.server_request_id, approval.action_nonce))
            .collect();
        match conversation.ok().flatten() {
            Some(conversation) if t3 => {
                crate::t3_code::questions::recheck(state, &conversation, &thread, &waiting).await
            }
            Some(conversation) => {
                crate::chatgpt_approvals::recheck(state, &conversation, &thread, &waiting).await
            }
            None => {
                let decision = if t3 {
                    crate::t3_code::questions::ON_COMPUTER
                } else {
                    crate::chatgpt_approvals::ON_COMPUTER
                };
                for (key, nonce) in waiting {
                    if state
                        .store
                        .retire_pending_approval_offered(
                            &key,
                            &nonce,
                            decision,
                            &crate::now_ms().to_string(),
                        )
                        .await
                        .unwrap_or(false)
                    {
                        let _ = crate::publish_event(
                            state,
                            wonder_api::WonderEvent::ApprovalResolved {
                                request_id: key,
                                decision: decision.into(),
                            },
                        )
                        .await;
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    use wonder_app_server::{AppServerClient, LaunchConfig};

    // B-425: an idle projector waits longer between looks, yet its heartbeat
    // keeps readiness ready across a quiet stretch longer than readiness's
    // window, and it still finds an inbox row no wakeup announced (its idle
    // poll, not a wakeup, projects it).
    #[tokio::test]
    async fn an_idle_projector_stays_ready_and_still_finds_unannounced_work() {
        let (_dir, state) = fixture().await;
        let _service = spawn(state.clone()).await;
        let ready = || async { state.ingestion.project_readiness(&state.store).await.ready };
        for _ in 0..6000 {
            if ready().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready().await);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(ready().await, "an idle host stays ready");
        // Stored without the sink, so nothing wakes the projector.
        state
            .store
            .enqueue_notification(&json!({"method":"turn/diff/updated",
                "params":{"threadId":"quiet","turnId":"quiet-turn","diff":"x"}}))
            .await
            .unwrap();
        // A hang guard only.
        tokio::time::timeout(Duration::from_secs(10), async {
            while state
                .store
                .notification_backlog_outside_lanes(&[])
                .await
                .unwrap()
                > 0
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("an unannounced row is still projected");
    }

    // Actual terminal projection waiting on the signed-answer owner. Measure
    // whether Stop's lifecycle lock and an unrelated inbox row can progress.
    #[tokio::test]
    #[ignore = "ingestion lock isolation performance measurement"]
    async fn ingestion_lock_isolation_measurement() {
        for round in 0..3 {
            let (_dir, state) = fixture().await;
            let runtime = state.projects.codex.lock().await.health().id().to_owned();
            state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
                "method":"turn/completed", "params":{"threadId":"held", "turn":{"id":"turn","status":"completed"}}})).await.unwrap();
            state.store.enqueue_notification(&json!({"method":"turn/diff/updated","params":{"threadId":"other","turnId":"other-turn","diff":"x"}})).await.unwrap();
            let held = state.approval_lock.lock().await;
            let s = state.clone();
            let first = tokio::spawn(async move { project_next(&s).await });
            let (id, _) = state.store.next_notification().await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                while !state.store.notification_was_started(id).await.unwrap() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let started = Instant::now();
            let s = state.clone();
            let stop_lock = tokio::spawn(async move {
                let _g = s.dispatch_lock.lock().await;
                started.elapsed().as_secs_f64() * 1000.0
            });
            let s = state.clone();
            let other = tokio::spawn(async move {
                project_next(&s).await.unwrap();
                started.elapsed().as_secs_f64() * 1000.0
            });
            // Fixture-controlled blocking interval, not a test hang timeout.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let control_free = stop_lock.is_finished();
            let other_free = other.is_finished();
            drop(held);
            first.await.unwrap().unwrap();
            eprintln!(
                "INGESTION_LOCK_MEASUREMENT={}",
                json!({"round":round,"heldMs":250,
                "controlFreeWhileHeld":control_free,"otherConversationFreeWhileHeld":other_free,
                "controlWaitMs":stop_lock.await.unwrap(),"otherConversationWaitMs":other.await.unwrap(),"profile":"debug"})
            );
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    // Same durable inbox/projection path as the daemon, without provider work.
    // Seed the real retention cap so per-delta pruning participates in timing.
    #[tokio::test]
    #[ignore = "durable delta ingestion performance measurement"]
    async fn delta_ingestion_at_retention_cap_measurement() {
        for round in 0..3 {
            let (dir, state) = fixture().await;
            active_message(&state, "turn").await;
            let db = sqlx::SqlitePool::connect(&format!(
                "sqlite://{}",
                dir.path().join("state.db").display()
            ))
            .await
            .unwrap();
            sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000) INSERT INTO sync_journal(host_epoch,occurred_at,resource,change_kind) SELECT 'epoch','now','messages','update' FROM n")
                .execute(&db).await.unwrap();
            state.store.prune_replay().await.unwrap();
            let count = 1000;
            for i in 0..count {
                state.store.enqueue_notification(&json!({"method":"item/agentMessage/delta", "params":{
                    "threadId":"thread","turnId":"turn","itemId":"item","sequence":i,"delta":{"text":"x".repeat(64)}}})).await.unwrap();
            }
            let start = Instant::now();
            let mut passes = 0;
            while project_next(&state).await.unwrap() {
                passes += 1;
            }
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            let messages = state
                .store
                .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap();
            assert_eq!(messages[0].text, "x".repeat(count * 64));
            let kind = serde_json::to_value(wonder_api::WonderEvent::AssistantDelta {
                text: String::new(),
            })
            .unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned();
            let events: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM events WHERE json_extract(payload_json,'$.event.type')=?",
            )
            .bind(kind)
            .fetch_one(&db)
            .await
            .unwrap();
            eprintln!(
                "DELTA_INGESTION_MEASUREMENT={}",
                json!({"round":round,"deltas":count,"bytes":count*64,"elapsedMs":elapsed,"projectionPasses":passes,"durableDeltaEvents":events,"profile":"debug","initialReplayRows":100000})
            );
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    fn delta_fixture(sequence: usize, text: &str) -> Value {
        json!({"method":"item/agentMessage/delta", "params":{
            "threadId":"thread", "turnId":"turn", "itemId":"item", "sequence":sequence, "delta":text}})
    }

    // Coalescing is an optimization: when the read of later rows fails, the
    // head still projects alone instead of bypassing failure bookkeeping.
    #[tokio::test]
    async fn a_failed_batch_read_projects_the_head_delta_alone() {
        let (dir, state) = fixture().await;
        active_message(&state, "turn").await;
        state
            .store
            .enqueue_notification(&delta_fixture(0, "head"))
            .await
            .unwrap();
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        // A row the batch read cannot decode stands in for a failed read.
        sqlx::query("INSERT INTO notification_inbox (notification_json) VALUES ('{unreadable')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(project_next(&state).await.unwrap());
        let messages = state
            .store
            .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap();
        assert_eq!(messages[0].text, "head");
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        sqlx::query("DELETE FROM notification_inbox")
            .execute(&pool)
            .await
            .unwrap();
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // The durable inbox owns the boundary: crash replay, duplicate fragments,
    // approvals/terminal events, byte/row limits and unmapped turns cannot lose
    // or reorder text. This exercises the same projector as the daemon.
    #[tokio::test]
    async fn delta_batches_preserve_replay_and_boundaries() {
        let (dir, mut state) = fixture().await;
        active_message(&state, "turn").await;
        for i in 0..34 {
            state
                .store
                .enqueue_notification(&delta_fixture(i, "x"))
                .await
                .unwrap();
        }
        let (id, mut head) = state.store.next_notification().await.unwrap().unwrap();
        head["_wonderInboxId"] = json!(id);
        let mut batch = delta_batch(&state, id, &head).await.unwrap();
        state.store.start_notification(id).await.unwrap();
        assert!(
            crate::apply_app_server_notification_with_deltas(
                &state,
                head,
                false,
                false,
                Some(&mut batch)
            )
            .await
        );
        // Intentionally leave the rows unacknowledged and reopen storage.
        state.store = Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        assert!(project_next(&state).await.unwrap());
        assert_eq!(state.store.notification_backlog().await.unwrap(), 2);
        assert!(project_next(&state).await.unwrap());
        for (i, text) in [(33, "x"), (34, "Y"), (34, "Y")] {
            state
                .store
                .enqueue_notification(&delta_fixture(i, text))
                .await
                .unwrap();
        }
        assert!(project_next(&state).await.unwrap());
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let messages = state
            .store
            .assistant_messages_for_conversation(conversation)
            .await
            .unwrap();
        assert_eq!(messages[0].text, format!("{}Y", "x".repeat(34)));
        let delta_text: String = state
            .store
            .events_for_conversation(conversation)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| {
                if let wonder_api::WonderEvent::AssistantDelta { text } = e.event {
                    Some(text)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(delta_text, messages[0].text);

        for barrier in [
            json!({"id":17,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","turnId":"turn","itemId":"tool"}}),
            json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"completed"}}}),
            json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"item","type":"agentMessage","text":"done"}}}),
            json!({"method":"item/agentMessage/delta","_wonderRuntimeId":"other","params":{"threadId":"thread","turnId":"turn","itemId":"item","delta":"other"}}),
        ] {
            state
                .store
                .enqueue_notification(&delta_fixture(35, "Z"))
                .await
                .unwrap();
            state.store.enqueue_notification(&barrier).await.unwrap();
            state
                .store
                .enqueue_notification(&delta_fixture(36, "W"))
                .await
                .unwrap();
            assert!(project_next(&state).await.unwrap());
            assert_eq!(state.store.notification_backlog().await.unwrap(), 2);
            let (id, saved) = state.store.next_notification().await.unwrap().unwrap();
            assert_eq!(saved, barrier);
            state.store.acknowledge_notification(id).await.unwrap();
            assert!(project_next(&state).await.unwrap());
        }
        // Byte budgets split independently of the row budget.
        for i in 40..43 {
            state
                .store
                .enqueue_notification(&delta_fixture(i, &"b".repeat(33 * 1024)))
                .await
                .unwrap();
        }
        for left in [2, 1, 0] {
            assert!(project_next(&state).await.unwrap());
            assert_eq!(state.store.notification_backlog().await.unwrap(), left);
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delta_batch_does_not_acknowledge_unmapped_tail() {
        let (_dir, state) = fixture().await;
        for _ in 0..2 {
            let mut n = delta_fixture(0, "same");
            n["params"].as_object_mut().unwrap().remove("sequence");
            state.store.enqueue_notification(&n).await.unwrap();
        }
        assert!(project_next(&state).await.unwrap());
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        assert!(project_next(&state).await.unwrap());
        assert_eq!(
            state
                .store
                .pending_app_server_notifications_for_turn("turn")
                .await
                .unwrap()
                .len(),
            2
        );
        active_message(&state, "turn").await;
        crate::drain_pending_app_server_notifications(&state, "thread", "turn").await;
        let messages = state
            .store
            .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap();
        assert_eq!(messages[0].text, "samesame");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Terminal identity survives an acknowledged inbox and a database reopen,
    // even if no message maps the turn yet. A delayed/replayed request must not
    // reopen a request or publish ApprovalOpened to a connected phone.
    #[tokio::test]
    async fn terminal_unmapped_turn_rejects_late_and_replayed_requests() {
        let (dir, mut state) = fixture().await;
        let health = state.projects.codex.lock().await.health();
        let runtime = health.id().to_owned();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, health);
        let terminal = json!({"_wonderRuntimeId":runtime, "method":"turn/completed",
            "params":{"threadId":"unmapped", "turn":{"id":"turn","status":"interrupted"}}});
        state.store.enqueue_notification(&terminal).await.unwrap();
        assert!(project_next(&state).await.unwrap());
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        state.store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let mut events = state.events.subscribe();
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/permissions/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "item/tool/call",
        ] {
            let request = json!({"_wonderRuntimeId":runtime, "id":method, "method":method,
                "params":{"threadId":"unmapped", "turnId":"turn", "itemId":"item"}});
            // Replay both the held completion and the request, as after a crash.
            for notification in [&terminal, &request, &request] {
                state
                    .store
                    .enqueue_notification(notification)
                    .await
                    .unwrap();
                assert!(project_next(&state).await.unwrap());
            }
            assert!(state
                .store
                .approval(&format!("{runtime}:{method}"))
                .await
                .unwrap()
                .is_none());
        }
        assert!(state
            .store
            .list_pending_approvals()
            .await
            .unwrap()
            .is_empty());
        assert!(events.try_recv().is_err());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: native Projects children cannot poison the shared inbox by
    // requiring a Project parent. Live requests remain pending, historical
    // envelopes convey no authority, and the following parent completion wins.
    #[tokio::test]
    async fn project_children_do_not_block_parent_completion_or_lose_approvals() {
        let (_dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let runtime = crate::projects::codex_rpc(&state)
            .await
            .unwrap()
            .health()
            .id()
            .to_owned();
        for origin in [runtime.as_str(), "retired-runtime"] {
            state.store.enqueue_notification(&json!({"_wonderRuntimeId":origin,
                "method":"thread/status/changed", "params":{"threadId":"child-thread", "status":{"type":"active"}}})).await.unwrap();
        }
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime, "id":42,
            "method":"item/commandExecution/requestApproval", "params":{"threadId":"child-thread", "turnId":"child-turn", "itemId":"command", "availableDecisions":["accept","decline"]}})).await.unwrap();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"turn/completed", "params":{"threadId":"thread", "turn":{"id":"turn", "status":"completed"}}})).await.unwrap();
        while project_next(&state).await.unwrap() {}
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
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
        assert!(state
            .store
            .subagent_ownership_for_thread("child-thread")
            .await
            .unwrap()
            .is_none());
        assert_eq!(state.store.list_pending_approvals().await.unwrap().len(), 1);
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Unmapped Project child notifications must not block later committed
    // receipts and approvals while the provider's task roster catches up.
    #[tokio::test]
    async fn project_agent_tasks_on_a_shared_runtime_do_not_block_the_inbox() {
        let (_dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let shared = state.projects.codex.lock().await.health();
        state.ingestion.register_provider(
            AgentFamily::Codex,
            &state.projects.codex,
            shared.clone(),
        );
        let runtime = shared.id().to_owned();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"turn/started", "params":{"threadId":"child-thread", "turn":{"id":"child-turn","items":[],"status":"inProgress"}}})).await.unwrap();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"item/started", "params":{"threadId":"thread", "turnId":"turn", "item":{"type":"subAgentActivity","id":"task","agentThreadId":"child-thread","kind":"started","status":"running"}}})).await.unwrap();
        // A task's own agent task reports the task's thread, not the Project
        // chat, as its parent (stalled the inbox on 1.0.132-1.0.133).
        for method in [
            "turn/started",
            "item/started",
            "item/completed",
            "turn/completed",
        ] {
            state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
                "method":method, "params":{"threadId":"grandchild-thread", "turnId":"grandchild-turn",
                    "turn":{"id":"grandchild-turn","status":"inProgress"},
                    "item":{"type":"commandExecution","id":"command","status":"completed",
                        "aggregatedOutput":"<persisted-output>\nOutput too large (31.1KB).\n</persisted-output>"}}})).await.unwrap();
        }
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"turn/completed", "params":{"threadId":"thread", "turn":{"id":"turn", "status":"completed"}}})).await.unwrap();
        while project_next(&state).await.unwrap() {}
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
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
        for thread in ["child-thread", "grandchild-thread"] {
            assert!(state
                .store
                .subagent_ownership_for_thread(thread)
                .await
                .unwrap()
                .is_none());
        }
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a held lane keeps FIFO, while a second lane and the actual
    // Stop handler finish before the fixture releases the preparation.
    #[tokio::test]
    async fn slow_projection_isolates_events_stop_and_cancellation() {
        let (_dir, state) = fixture().await;
        active_message(&state, "turn").await;
        let runtime = state.projects.codex.lock().await.health().id().to_owned();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"turn/completed", "params":{"threadId":"blocked", "turn":{"id":"blocked-turn","status":"completed"}}})).await.unwrap();
        let (id, _) = state.store.next_notification().await.unwrap().unwrap();
        state.store.enqueue_notification(&json!({"method":"turn/diff/updated", "params":{"threadId":"blocked","turnId":"later","diff":"ordered"}})).await.unwrap();
        state
            .store
            .enqueue_notification(&delta_fixture(0, "unblocked"))
            .await
            .unwrap();
        let held = state.approval_lock.lock().await;
        let copy = state.clone();
        let blocked = tokio::spawn(async move { project_next(&copy).await });
        tokio::time::timeout(Duration::from_secs(60), async {
            while !state.store.notification_was_started(id).await.unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            assert!(project_next(&state).await.unwrap());
            let response = crate::interrupt_turn(
                axum::extract::State(state.clone()),
                axum::extract::Path(("d19b0f7e-97bb-45d7-8206-324a62386dd9".into(), "turn".into())),
            )
            .await;
            assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        })
        .await
        .expect("unrelated events and Stop complete while approval is held");
        assert!(!blocked.is_finished());
        assert!(
            crate::turn_diff::saved_turns(&crate::turn_diff::saved_directory(
                &state.data_dir,
                "blocked"
            ))
            .is_empty()
        );
        let messages = state
            .store
            .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap();
        assert_eq!(messages[0].text, "unblocked");
        blocked.abort();
        assert!(blocked.await.unwrap_err().is_cancelled());
        assert!(state.ingestion.inner.lock().unwrap().active.is_empty());
        drop(held);
        while project_next(&state).await.unwrap() {}
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        assert_eq!(
            crate::turn_diff::saved_turns(&crate::turn_diff::saved_directory(
                &state.data_dir,
                "blocked"
            ))
            .len(),
            1
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn preparation_revalidates_changed_workspace_ownership() {
        let (dir, state) = fixture().await;
        let message = active_message(&state, "turn").await;
        let runtime = state.projects.codex.lock().await.health().id().to_owned();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"turn/completed", "params":{"threadId":"thread","turn":{"id":"turn","status":"completed"}}})).await.unwrap();
        let held = state.approval_lock.lock().await;
        let (preparing, prepared) = tokio::sync::oneshot::channel();
        PREPARING.lock().unwrap().insert(runtime.clone(), preparing);
        let copy = state.clone();
        let projecting = tokio::spawn(async move { project_next(&copy).await });
        tokio::time::timeout(Duration::from_secs(60), async {
            prepared.await.unwrap();
            // Getting lifecycle serialization proves preparation released it.
            let _dispatch = state.dispatch_lock.lock().await;
            let pool = sqlx::SqlitePool::connect(&format!(
                "sqlite://{}",
                dir.path().join("state.db").display()
            ))
            .await
            .unwrap();
            sqlx::query("UPDATE project_conversations SET roots_revision=roots_revision+1")
                .execute(&pool)
                .await
                .unwrap();
        })
        .await
        .unwrap();
        drop(held);
        assert!(projecting.await.unwrap().is_err());
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_codex"
        );
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        assert!(project_next(&state).await.unwrap());
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
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failing_lane_retries_in_order_without_hiding_another_lane() {
        let (dir, state) = fixture().await;
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("ALTER TABLE goal_threads RENAME TO unavailable_goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        state
            .store
            .enqueue_notification(
                &json!({"method":"thread/goal/cleared","params":{"threadId":"failing"}}),
            )
            .await
            .unwrap();
        state.store.enqueue_notification(&json!({"method":"turn/diff/updated","params":{"threadId":"failing","turnId":"later","diff":"held"}})).await.unwrap();
        state
            .store
            .enqueue_notification(&json!({"method":"runtime/activated","params":{}}))
            .await
            .unwrap();
        let (id, _) = state.store.next_notification().await.unwrap().unwrap();
        let copy = state.clone();
        let failing = tokio::spawn(async move { project_next(&copy).await });
        tokio::time::timeout(Duration::from_secs(60), async {
            while state.ingestion.inner.lock().unwrap().failing.is_empty() {
                tokio::task::yield_now().await;
            }
            assert!(project_next(&state).await.unwrap());
        })
        .await
        .unwrap();
        assert!(failing.await.unwrap().is_err());
        assert_eq!(
            state.store.next_notification().await.unwrap().unwrap().0,
            id
        );
        assert_eq!(state.store.notification_backlog().await.unwrap(), 2);
        sqlx::query("ALTER TABLE unavailable_goal_threads RENAME TO goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        while project_next(&state).await.unwrap() {}
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_notification_that_keeps_failing_is_set_aside_after_the_limit() {
        let (_dir, state) = fixture().await;
        // A goal update without a thread can never be projected.
        state
            .store
            .enqueue_notification(&json!({"method":"thread/goal/updated","params":{}}))
            .await
            .unwrap();
        state
            .store
            .enqueue_notification(&json!({"method":"turn/diff/updated","params":{}}))
            .await
            .unwrap();
        assert!(project_next(&state).await.is_err());
        assert!(
            project_next(&state).await.is_err(),
            "Still within the limit"
        );
        assert_eq!(state.store.notification_backlog().await.unwrap(), 2);
        {
            let mut status = state.ingestion.inner.lock().unwrap();
            for failure in status.failing.values_mut() {
                failure.since = Instant::now() - FAILING_NOTIFICATION_LIMIT;
            }
        }
        while project_next(&state).await.unwrap() {}
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        assert!(state.ingestion.inner.lock().unwrap().failing.is_empty());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // SM-06: a poison row ahead of `turn/completed` on a thread's lane held the
    // message on "working" for the full five-minute limit. The poison row is
    // quarantined at the short limit and the completion behind it projects.
    #[tokio::test]
    async fn a_poison_notification_ahead_of_turn_completed_is_quarantined_not_waited_out() {
        let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("ALTER TABLE goal_threads RENAME TO unavailable_goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        let runtime = state.projects.codex.lock().await.health().id().to_owned();
        state
            .store
            .enqueue_notification(
                &json!({"method":"thread/goal/cleared","params":{"threadId":"thread"}}),
            )
            .await
            .unwrap();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":runtime,
            "method":"turn/completed", "params":{"threadId":"thread","turn":{"id":"turn","status":"completed"}}})).await.unwrap();
        let message_state = || async {
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state
        };
        for _ in 0..3 {
            assert!(project_next(&state).await.is_err());
            assert_eq!(message_state().await, "streaming", "held within the limit");
        }
        {
            let mut status = state.ingestion.inner.lock().unwrap();
            for failure in status.failing.values_mut() {
                failure.since = Instant::now() - FAILING_NOTIFICATION_LIMIT;
            }
        }
        assert!(
            project_next(&state).await.unwrap(),
            "poison row quarantined"
        );
        // The completion needs the table the poison row could not do without.
        sqlx::query("ALTER TABLE unavailable_goal_threads RENAME TO goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        assert!(project_next(&state).await.unwrap(), "completion projected");
        assert_eq!(message_state().await, "completed");
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn a_failing_terminal_is_not_quarantined_by_the_short_limit() {
        let mut failures = HashMap::new();
        let age = |failures: &mut HashMap<String, LaneFailure>, by: Duration| {
            failures.get_mut("lane").unwrap().since = Instant::now() - by;
        };
        let (first, verdict) = LaneFailure::fail(&mut failures, "lane", 1, RowKind::Terminal, true);
        assert!(first);
        assert_eq!(
            verdict,
            FailureVerdict::Retry,
            "a quarantined thread is no excuse"
        );
        age(&mut failures, FAILING_NOTIFICATION_LIMIT * 3);
        assert_eq!(
            LaneFailure::fail(&mut failures, "lane", 1, RowKind::Terminal, true).1,
            FailureVerdict::Retry
        );
        age(&mut failures, FAILING_TERMINAL_LIMIT);
        assert_eq!(
            LaneFailure::fail(&mut failures, "lane", 1, RowKind::Terminal, false).1,
            FailureVerdict::Quarantine,
            "last resort"
        );
        // Another row on the lane starts its own clock.
        let (first, verdict) = LaneFailure::fail(&mut failures, "lane", 2, RowKind::Other, false);
        assert!(first);
        assert_eq!(verdict, FailureVerdict::Retry);
        assert_eq!(failures["lane"].attempts, 1);
        age(&mut failures, FAILING_NOTIFICATION_LIMIT);
        assert_eq!(
            LaneFailure::fail(&mut failures, "lane", 2, RowKind::Other, false).1,
            FailureVerdict::Quarantine
        );
    }

    #[test]
    fn the_consumer_restart_backs_off_and_resets_after_a_stable_run() {
        let mut backoff = RestartBackoff::new();
        let quick = Duration::from_millis(10);
        let delays: Vec<_> = (0..9).map(|_| backoff.after_run(quick)).collect();
        assert_eq!(delays[0], Duration::from_millis(250));
        assert_eq!(delays[1], Duration::from_millis(500));
        assert_eq!(delays[2], Duration::from_secs(1));
        assert_eq!(*delays.last().unwrap(), RestartBackoff::CAP);
        assert_eq!(
            backoff.after_run(RestartBackoff::STABLE),
            Duration::from_millis(250)
        );
    }

    // Regression (1.0.132): every notification of one unprojectable agent
    // thread waited out the full limit in turn, so a 4-event task held all
    // chats for 20 minutes. Once one is set aside, the thread's later
    // failures are set aside at once; other threads still get the full limit.
    #[tokio::test]
    async fn later_failures_on_a_set_aside_thread_do_not_wait_the_limit_again() {
        let (dir, state) = fixture().await;
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("ALTER TABLE goal_threads RENAME TO unavailable_goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        for thread in ["stuck", "stuck", "stuck", "other"] {
            state
                .store
                .enqueue_notification(
                    &json!({"method":"thread/goal/cleared","params":{"threadId":thread}}),
                )
                .await
                .unwrap();
        }
        assert!(project_next(&state).await.is_err());
        {
            let mut status = state.ingestion.inner.lock().unwrap();
            for failure in status.failing.values_mut() {
                failure.since = Instant::now() - FAILING_NOTIFICATION_LIMIT;
            }
        }
        assert!(project_next(&state).await.unwrap());
        assert!(project_next(&state).await.unwrap());
        assert!(project_next(&state).await.unwrap());
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        assert!(project_next(&state).await.is_err());
        assert!(
            project_next(&state).await.is_err(),
            "Another thread keeps the full limit"
        );
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // A projection that never finishes is a failure: the row stays, is retried
    // and is set aside at the limit, instead of holding the inbox forever.
    #[tokio::test]
    async fn a_projection_that_hangs_counts_as_a_failure() {
        let (dir, state) = fixture().await;
        let shared = state.projects.codex.lock().await.health();
        state
            .store
            .enqueue_notification(&json!({"_wonderRuntimeId":shared.id(),
                "method":"turn/completed", "params":{"threadId":"unmapped", "turn":{"id":"turn","status":"completed"}}}))
            .await
            .unwrap();
        let (id, _) = state.store.next_notification().await.unwrap().unwrap();
        // Terminal projection must acquire this lock before retiring approvals.
        // Keep real time: pausing Tokio while SQLite is pending can manufacture
        // an unrelated storage timeout instead of testing the projection bound.
        let held = state.approval_lock.lock().await;
        assert!(tokio::time::timeout(
            Duration::from_secs(60),
            project_next_with_limit(&state, Duration::from_millis(10)),
        )
        .await
        .expect("the blocked projection must reach its attempt deadline")
        .is_err());
        assert!(state.store.notification_was_started(id).await.unwrap());
        assert_eq!(
            state
                .ingestion
                .inner
                .lock()
                .unwrap()
                .failing
                .values()
                .next()
                .map(|f| f.id),
            Some(id)
        );
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        let logs = fs::read_to_string(dir.path().join("logs/test.jsonl")).unwrap();
        assert!(logs.lines().any(|line| {
            let record: Value = serde_json::from_str(line).unwrap();
            record["event"] == "notification_projection_failing"
                && record["inboxId"] == id
                && record["timedOut"] == true
        }));
        drop(held);
        assert!(project_next(&state).await.unwrap());
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        assert!(state.ingestion.inner.lock().unwrap().failing.is_empty());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    pub(crate) async fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let workspace = dir.path().join("d19b0f7e-97bb-45d7-8206-324a62386dd9");
        fs::create_dir(&workspace).unwrap();
        crate::test_support::project_thread(
            &store,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            "Project",
            workspace.to_str().unwrap(),
            &crate::projects::store_key("codex", dir.path()),
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        store
            .upsert_owner_device("owner", "Owner", "{}", "now")
            .await
            .unwrap();
        let source = r#"import json, sys, os, threading, time, fcntl
root = os.path.dirname(__file__)
if os.path.exists(root + '/lock-fixture'):
    writer = open(root + '/writer.lock', 'w')
    fcntl.flock(writer, fcntl.LOCK_EX | fcntl.LOCK_NB)
def child_thread():
    status = 'idle'
    if os.path.exists(root + '/child-active'): status = 'active'
    if os.path.exists(root + '/child-interrupted'): status = 'idle'
    return {'id':'child-thread','parentThreadId':'thread','source':{'subAgent':{'thread_spawn':{'parent_thread_id':'thread','depth':1,'agent_nickname':'Scout','agent_role':'research'}}},'status':{'type':status},'canAcceptDirectInput':True,'cwd':'/child/work','model':'child-model'}
for line in sys.stdin:
    r = json.loads(line)
    method = r.get('method')
    if method is None:
        with open(root + '/responses', 'a') as log: log.write(json.dumps(r) + '\n')
        continue
    if method == 'initialized': continue
    with open(root + '/requests', 'a') as log: log.write(method + '\n')
    with open(root + '/requests-jsonl', 'a') as log: log.write(json.dumps(r) + '\n')
    with open(root + '/routing-requests-jsonl', 'a') as log:
        log.write(json.dumps({'scope':os.getenv('WONDER_FIXTURE_SCOPE','d19b0f7e-97bb-45d7-8206-324a62386dd9'),'method':method,'params':r.get('params',{})}) + '\n')
    result = {}
    if os.path.exists(root + '/update-fixture.json') and method in ('thread/resume','thread/read','thread/list','thread/loaded/list','thread/turns/list','thread/items/list','turn/interrupt','turn/start'):
        checkpoint = json.load(open(root + '/update-fixture.json'))
        params = r.get('params',{})
        thread = params.get('threadId','thread')
        turns = checkpoint.get(thread,[])
        active = any(t['status'] == 'inProgress' for t in turns)
        if method in ('thread/resume','thread/read'):
            result = {'thread':{'id':thread,'status':{'type':'active' if active else 'idle'}}}
            if os.path.exists(root + '/update-cwd'): result['thread']['cwd'] = open(root + '/update-cwd').read()
            if thread == 'child-thread':
                result['thread'] = child_thread()
                result['thread']['status'] = {'type':'active' if active else 'idle'}
        elif method == 'thread/list':
            children = [child_thread()] if 'child-thread' in checkpoint and params.get('parentThreadId') == 'thread' and not params.get('archived',False) else []
            if children: children[0]['status'] = {'type':'active' if any(t['status']=='inProgress' for t in checkpoint['child-thread']) else 'idle'}
            result = {'data':children,'nextCursor':None}
            if os.path.exists(root + '/retained-journals') and 'parentThreadId' not in params: result = {'data':[{'id':'retained-%d' % i} for i in range(100)],'nextCursor':'more'}
        elif method == 'thread/loaded/list': result = {'data':list(checkpoint),'nextCursor':None}
        elif method == 'thread/turns/list': result = {'data':list(reversed(turns)),'nextCursor':None}
        elif method == 'thread/items/list': result = {'data':[{'turnId':t['id'],'item':{'type':'userMessage','clientId':t.get('clientId','')}} for t in reversed(turns)],'nextCursor':None}
        elif method == 'turn/interrupt':
            for t in turns:
                if t['id'] == params['turnId']: t['status'] = 'completed' if os.path.exists(root + '/complete-during-update') else 'interrupted'
            result = {}
        elif method == 'turn/start':
            if os.path.exists(root + '/reject-update-continuation'):
                print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'Cannot accept continuation'}}),flush=True)
                continue
            new = {'id':'resumed-'+str(len(turns)), 'status':'inProgress','clientId':params['clientUserMessageId']}
            turns.append(new)
            result = {'turn':new}
        with open(root + '/update-fixture.json','w') as saved: json.dump(checkpoint,saved)
        if method == 'turn/start' and os.path.exists(root + '/lose-update-response'): sys.exit(0)
        if method == 'turn/interrupt' and os.path.exists(root + '/lose-stop-response'): sys.exit(0)
        print(json.dumps({'id':r['id'],'result':result}),flush=True)
        continue
    idle = json.load(open(root + '/idle-fixture.json')) if os.path.exists(root + '/idle-fixture.json') else {}
    if os.path.exists(root + '/project-subagents-fixture.json') and method in ('thread/read','thread/list','thread/items/list','thread/turns/list'):
        fixture = json.load(open(root + '/project-subagents-fixture.json'))
        params = r.get('params',{})
        if method == 'thread/read':
            thread = fixture['threads'].get(params.get('threadId'))
            if thread is None:
                print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'Thread not found'}}),flush=True)
                continue
            result = {'thread':thread}
        elif method == 'thread/list':
            threads = [thread for thread in fixture['threads'].values() if thread.get('parentThreadId') == params.get('parentThreadId') and thread.get('isArchived',False) == params.get('archived',False)]
            page_size = fixture.get('listPageSize')
            if page_size:
                start = 0 if params.get('cursor') == 'repeat' else int(params.get('cursor') or 0)
                end = start + page_size
                result = {'data':threads[start:end],'nextCursor':str(end) if end < len(threads) else None}
                if fixture.get('repeatListCursor') and result['nextCursor']:
                    result['nextCursor'] = 'repeat'
            else:
                result = {'data':[],'nextCursor':None} if params.get('cursor') else {'data':threads,'nextCursor':fixture.get('listNextCursor') if not params.get('archived',False) else None}
        elif method == 'thread/turns/list':
            result = {'data':fixture.get('turns',{}).get(params.get('threadId'),[]),'nextCursor':None}
        else:
            result = {'data':fixture.get('items',{}).get(params.get('threadId'),[]),'nextCursor':None}
        print(json.dumps({'id':r['id'],'result':result}),flush=True)
        continue
    if os.path.exists(root + '/refresh-fixture.json') and method in ('thread/read','thread/turns/list','thread/items/list'):
        fixture = json.load(open(root + '/refresh-fixture.json'))
        params = r.get('params',{})
        if method == 'thread/read':
            result = {'thread':{'id':params['threadId'],'path':root + '/refresh-source.jsonl','status':{'type':'idle'}}}
        elif method == 'thread/turns/list':
            result = {'data':[{'id':'turn','status':fixture.get('status','completed')}], 'nextCursor':None}
            if fixture.get('turnPages'):
                page = int(params.get('cursor') or '0')
                result['nextCursor'] = str(page + 1) if page + 1 < fixture['turnPages'] else None
            if fixture.get('turnsWithoutData'):
                del result['data']
            if params.get('includeHistoryRevision') and fixture.get('projection'):
                state = root + '/projection-state'
                result['historyRevision'] = fixture['projection'] + (open(state).read() if os.path.exists(state) else '')
        else:
            start = int(params.get('cursor') or '0')
            if start == fixture.get('failAt'):
                print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'Fixture page failed'}}),flush=True)
                continue
            count = fixture['count']
            end = min(count, start + params.get('limit',100))
            indices = range(start,end) if params.get('sortDirection') == 'asc' else range(count-1-start,count-1-end,-1)
            result = {'data':[{'turnId':'turn','item':{'id':'item-'+str(i),'type':'agentMessage','text':('changed' if fixture.get('changed') == i else 'x') * fixture.get('bytes',128),'createdAt':str(1700000000000+i)}} for i in indices], 'nextCursor':str(end) if end < count else None}
            for entry in result['data']:
                if entry['item']['id'] == 'item-' + str(fixture.get('media')):
                    svg = __import__('base64').b64encode(b'<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>').decode()
                    entry['item'] = {'id':entry['item']['id'],'type':'mcpToolCall','server':'chart','tool':'draw','status':'completed','arguments':{},
                        'result':{'content':[{'type':'resource','resource':{'uri':'file:///chart.svg','mimeType':'image/svg+xml','blob':svg}}]}}
            if fixture.get('mutateProjectionDuringRead'):
                with open(root + '/projection-state','a') as state: state.write('x')
            if fixture.get('mutateDuringRead'):
                with open(root + '/refresh-source.jsonl','a') as source: source.write('changed\n')
        print(json.dumps({'id':r['id'],'result':result}),flush=True)
        continue
    if method == 'initialize': result = {'capabilities': {'experimentalApi': True}}
    elif method == 'account/rateLimits/read' and os.path.exists(root + '/unsupported-usage'):
        error = json.load(open(root + '/unsupported-usage'))
        print(json.dumps({'id':r['id'],'error':error}),flush=True)
        continue
    elif method == 'account/rateLimits/read' and os.path.exists(root + '/usage-fixture.json'):
        result = json.load(open(root + '/usage-fixture.json'))
    elif method == 'app/installed' and os.path.exists(root + '/apps-error.json'):
        print(json.dumps({'id':r['id'],'error':json.load(open(root + '/apps-error.json'))}),flush=True)
        continue
    elif method == 'app/installed' and os.path.exists(root + '/apps-fixture.json'):
        result = json.load(open(root + '/apps-fixture.json'))
    elif method == 'app/read' and os.path.exists(root + '/apps-read.json'):
        result = json.load(open(root + '/apps-read.json'))
    elif method == 'app/read' and os.path.exists(root + '/apps-fixture.json'):
        result = {'apps':[{'id':'app-fixture','name':'Fixture app'}]}
    elif method == 'app/list' and os.path.exists(root + '/apps-list.json'):
        pages = json.load(open(root + '/apps-list.json'))
        page = int(r['params'].get('cursor') or 0)
        result = {'data':pages[page], 'nextCursor':str(page + 1) if page + 1 < len(pages) else None}
    elif method == 'thread/loaded/list': result = {'data':list(idle), 'nextCursor':None}
    elif method == 'thread/backgroundTerminals/list' and idle.get(r['params']['threadId'],{}).get('backgroundError'):
        print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'Fixture page failed'}}),flush=True)
        continue
    elif method == 'thread/backgroundTerminals/list':
        background = idle.get(r['params']['threadId'],{}).get('background',[])
        size = idle.get(r['params']['threadId'],{}).get('backgroundPageSize') or len(background) or 1
        start = int(r['params'].get('cursor') or 0)
        result = {'data':background[start:start+size], 'nextCursor':str(start+size) if start+size < len(background) else None}
    elif method == 'thread/backgroundTerminals/terminate':
        thread = idle.get(r['params']['threadId'],{})
        kept = [t for t in thread.get('background',[]) if t.get('processId') != r['params']['processId']]
        result = {'terminated': len(kept) != len(thread.get('background',[]))}
        if result['terminated']:
            thread['background'] = kept
            with open(root + '/idle-fixture.json','w') as saved: json.dump(idle,saved)
    elif method == 'thread/read' and r.get('params',{}).get('threadId') in idle:
        result = {'thread':idle[r['params']['threadId']]}
    elif method == 'model/list' and os.path.exists(root + '/models.json'): result = json.load(open(root + '/models.json'))
    elif method == 'thread/goal/get':
        result = {'goal':idle[r['params']['threadId']].get('goal') if r.get('params',{}).get('threadId') in idle else (json.load(open(root + '/goal.json')) if os.path.exists(root + '/goal.json') else None)}
    elif method == 'thread/goal/set':
        goal = json.load(open(root + '/goal.json')) if os.path.exists(root + '/goal.json') else {'threadId':'thread','createdAt':1700000000,'updatedAt':1700000000,'objective':'','status':'active','tokensUsed':0,'timeUsedSeconds':0,'tokenBudget':None}
        goal.update({k:v for k,v in r['params'].items() if k in ('objective','status','tokenBudget')})
        with open(root + '/goal.json','w') as saved: json.dump(goal,saved)
        result = {'goal':goal}
    elif method == 'thread/goal/clear':
        if os.path.exists(root + '/goal.json'): os.remove(root + '/goal.json')
        result = {}
    elif method == 'project/list' and os.path.exists(root + '/native-thread.json'):
        projects = json.load(open(root + '/native-projects.json')) if os.path.exists(root + '/native-projects.json') else [{'id':'native-project','roots':[{'path':root + '/project-source'}]}]
        result = {'data':projects}
    elif method == 'project/read' and os.path.exists(root + '/native-thread.json'):
        projects = json.load(open(root + '/native-projects.json')) if os.path.exists(root + '/native-projects.json') else [{'id':'native-project','roots':[{'path':root + '/project-source'}]}]
        project = next((p for p in projects if p['id'] == r['params']['projectId']), None)
        result = {'project':project}
    elif method == 'project/create' and os.path.exists(root + '/native-thread.json'):
        result = {'project':{'id':'created-native-project','roots':r['params']['roots']}}
    elif method == 'thread/metadata/update' and os.path.exists(root + '/native-thread.json'):
        if os.path.exists(root + '/native-metadata-error.json'):
            print(json.dumps({'id':r['id'],'error':json.load(open(root + '/native-metadata-error.json'))}),flush=True)
            continue
        thread = json.load(open(root + '/native-thread.json'))
        thread['projectId'] = r['params']['projectId']
        with open(root + '/native-thread.json','w') as saved: json.dump(thread,saved)
        result = {'thread':thread}
    elif method == 'thread/read' and os.path.exists(root + '/native-read-error.json'):
        print(json.dumps({'id':r['id'],'error':json.load(open(root + '/native-read-error.json'))}),flush=True)
        continue
    elif method == 'thread/read' and os.path.exists(root + '/native-thread.json'):
        result = {'thread':json.load(open(root + '/native-thread.json'))}
    elif method in ('thread/search','thread/searchOccurrences'):
        if not os.path.exists(root + '/thread-search.json'):
            print(json.dumps({'id':r['id'],'error':{'code':-32601,'message':'Method not found'}}),flush=True)
            continue
        search = json.load(open(root + '/thread-search.json'))
        params = r['params']
        if method == 'thread/search':
            start = int(params.get('cursor') or 0)
            end = start + search.get('pageSize', params.get('limit') or 50)
            result = {'data':search['results'][start:end],'nextCursor':str(end) if end < len(search['results']) else None}
        else: result = {'data':search.get('occurrences',{}).get(params['threadId'],[]),'nextCursor':None}
    elif method == 'thread/start': result = {'thread':{'id':'thread'}}
    elif method == 'thread/fork': result = {'thread':{'id':'forked-thread','cwd':r['params'].get('cwd')}}
    elif method == 'thread/revert':
        if os.path.exists(root + '/revert-error'):
            print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'revert failed'}}),flush=True)
            continue
        if os.path.exists(root + '/rewind-journal-fixture'):
            open(root + '/rewind-entered', 'w').close()
            while os.path.exists(root + '/rewind-wait-before'): time.sleep(0.01)
            turns = [turn for page in json.load(open(root + '/turn-pages.json')) for turn in page]
            point = next(i for i, turn in enumerate(turns) if turn['id'] == r['params']['beforeTurnId'])
            with open(root + '/turn-pages-next.json', 'w') as saved: json.dump([turns[point+1:]], saved)
            os.replace(root + '/turn-pages-next.json', root + '/turn-pages.json')
            if os.path.exists(root + '/rewind-fail-store'):
                import sqlite3
                db = sqlite3.connect(root + '/state.db')
                db.execute("CREATE TRIGGER rewind_write_fault BEFORE UPDATE OF hidden_by_reset ON history_entries BEGIN SELECT RAISE(ABORT,'injected rewind store failure'); END")
                db.commit()
                db.close()
            open(root + '/rewind-applied', 'w').close()
            while os.path.exists(root + '/rewind-wait-after'): time.sleep(0.01)
        result = {'thread':{'id':r['params']['threadId'],'turns':[]},'turnsBackwardsCursor':None}
    elif method == 'turn/start':
        with open(root + '/accepted-client', 'w') as saved: saved.write(r['params']['clientUserMessageId'])
        open(root + '/completed', 'w').close()
        if os.path.exists(root + '/crash-after-execution'): sys.exit(0)
        result = {'turn':{'id':'turn'}}
    elif method == 'thread/resume' and os.path.exists(root + '/resume-active-writer'):
        print(json.dumps({'id':r['id'],'error':{'code':-32600,'message':'thread thread already has an active writer'}}),flush=True)
        continue
    elif method == 'thread/resume': result = {'thread':child_thread()} if r.get('params',{}).get('threadId') == 'child-thread' else {'thread':{'status':{'type':'idle'}}}
    elif method == 'thread/list':
        if os.path.exists(root + '/archive-fixture'):
            while os.path.exists(root + '/delay-archive-list'): time.sleep(0.01)
            archived = os.path.exists(root + '/archived-thread')
            result = {'data':[idle['thread']] if r.get('params',{}).get('archived',False) == archived else [], 'nextCursor':None}
        else:
            params = r.get('params',{})
            # child-unarchivable: a runtime without archival lists the child for both states.
            # child-second-page: the parent's first page is empty and names a cursor.
            listed = os.path.exists(root + '/child-fixture') and (not params.get('archived',False) or os.path.exists(root + '/child-unarchivable'))
            later = listed and os.path.exists(root + '/child-second-page') and not params.get('cursor')
            result = {'data':[child_thread(), {'id':'ordinary-task','parentThreadId':None,'source':'appServer','status':{'type':'idle'},'canAcceptDirectInput':True}] if listed and not later else [], 'nextCursor':'second' if later else None}
    elif method in ('thread/archive','thread/unarchive') and os.path.exists(root + '/archive-fixture'):
        if method == 'thread/archive' and os.path.exists(root + '/archive-active-writer'):
            print(json.dumps({'id':r['id'],'error':{'code':-32600,'message':'thread thread already has an active writer'}}),flush=True)
            continue
        if method == 'thread/archive' and os.path.exists(root + '/delay-archive'):
            open(root + '/archive-waiting','w').close()
            def delayed_archive(request_id):
                while os.path.exists(root + '/delay-archive'): time.sleep(0.01)
                open(root + '/archived-thread','w').close()
                print(json.dumps({'id':request_id,'result':{}}),flush=True)
            threading.Thread(target=delayed_archive,args=(r['id'],),daemon=True).start()
            continue
        if os.path.exists(root + '/archive-fail'):
            print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'Archive failed'}}),flush=True)
            continue
        if method == 'thread/archive': open(root + '/archived-thread','w').close()
        elif os.path.exists(root + '/archived-thread'): os.remove(root + '/archived-thread')
        if method == 'thread/archive' and os.path.exists(root + '/archive-lost-response'):
            print(json.dumps({'id':r['id'],'error':{'code':-32000,'message':'Archive response lost'}}),flush=True)
            continue
        result = {} if method == 'thread/archive' else {'thread':idle['thread']}
    elif method == 'turn/steer': result = {'turnId':'turn'}
    elif method == 'thread/items/list' and os.path.exists(root + '/history-items.json'):
        history = json.load(open(root + '/history-items.json'))
        result = history.get(r.get('params',{}).get('cursor') or '', {}) if isinstance(history, dict) else {'data':history,'nextCursor':None}
    elif method == 'thread/items/list':
        if os.path.exists(root + '/long-history'):
            page = int(r.get('params',{}).get('cursor') or '0')
            result = {'data':[{'turnId':'turn' if page == 99 else 'other','item':{'id':'item-'+str(page)+'-'+str(i),'type':'agentMessage','text':'history'}} for i in range(100)],'nextCursor':str(page+1) if page < 99 else None}
            print(json.dumps({'id':r['id'],'result':result}),flush=True)
            continue
        if os.path.exists(root + '/delay-history'):
            open(root + '/history-waiting','w').close()
            def delayed(request_id):
                while os.path.exists(root + '/delay-history'): time.sleep(0.01)
                items = json.load(open(root + '/history-items.json')) if os.path.exists(root + '/history-items.json') else []
                print(json.dumps({'id':request_id,'result':{'data':items, 'nextCursor':None}}),flush=True)
            threading.Thread(target=delayed,args=(r['id'],),daemon=True).start()
            continue
        result = {'data': [{'turnId':'turn','item':{'id':'item','type':'agentMessage','content':[{'text':'recovered'}]}}] if os.path.exists(root + '/completed') else [], 'nextCursor':None}
        if os.path.exists(root + '/accepted-client'):
            with open(root + '/accepted-client') as saved: client = saved.read()
            result['data'].append({'turnId':'turn','item':{'type':'userMessage','clientId':client}})
    elif method == 'thread/turns/list':
        if os.path.exists(root + '/turn-pages.json'):
            if os.path.exists(root + '/turns-list-wait'):
                open(root + '/turns-list-entered', 'w').close()
                while os.path.exists(root + '/turns-list-wait'): time.sleep(0.01)
            pages = json.load(open(root + '/turn-pages.json'))
            page = int(r.get('params',{}).get('cursor') or '0')
            result = {'data':pages[page], 'nextCursor':str(page+1) if page+1 < len(pages) else None}
        elif r.get('params',{}).get('threadId') == 'child-thread':
            child_status = 'interrupted' if os.path.exists(root + '/child-interrupted') else ('inProgress' if os.path.exists(root + '/child-active') else 'completed')
            result = {'data':[{'id':'turn','status':child_status}], 'nextCursor':None}
        else: result = {} if os.path.exists(root + '/bad-history') else {'data':[{'id':'turn','status':'completed'}] if os.path.exists(root + '/completed') else [], 'nextCursor':None}
    elif method == 'thread/read' and r.get('params',{}).get('threadId') == 'child-thread': result = {'thread':child_thread()}
    elif method == 'thread/read' and r.get('params',{}).get('threadId') == 'grandchild-thread':
        result = {'thread':{'id':'grandchild-thread','parentThreadId':'child-thread','source':{'subAgent':{'thread_spawn':{'parent_thread_id':'child-thread','depth':2}}},'status':{'type':'active'},'canAcceptDirectInput':False}}
    elif method == 'turn/interrupt':
        if os.path.exists(root + '/interrupt-response.json'):
            reply = json.load(open(root + '/interrupt-response.json'))
            print(json.dumps({'id':r['id'], **reply}), flush=True)
            continue
        open(root + '/child-interrupted','w').close()
        result = {}
    elif method == 'thread/unarchive': result = {'thread':child_thread()}
    elif method == 'thread/read':
        mode = r.get('params', {}).get('mode')
        if mode == 'die':
            open(root + '/completed', 'w').close()
            print(json.dumps({'id':r['id'],'result':{}}), flush=True)
            sys.exit(0)
        count = r.get('params', {}).get('count', 600)
        delta = 'x' * r.get('params', {}).get('deltaSize', 1)
        print(json.dumps({'id':r.get('params', {}).get('approvalId', r['id']),'method':'item/commandExecution/requestApproval','params':{'threadId':'thread','turnId':'turn','itemId':'command','availableDecisions':['accept','decline']}}), flush=True)
        for i in range(count):
            print(json.dumps({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','itemId':'item','sequence':i,'delta':{'text':delta}}}), flush=True)
        for _ in range(r.get('params', {}).get('activations', 0)):
            print(json.dumps({'method':'runtime/activated','params':{}}), flush=True)
    print(json.dumps({'id':r['id'],'result':result}), flush=True)
"#;
        fs::write(dir.path().join("runtime.py"), source).unwrap();
        let schemas = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research/codex-app-server/0.155.0-alpha.9");
        let script = format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.155.0-alpha.9'; exit 0; fi\nif [ \"$2\" = generate-json-schema ]; then last=''; for arg in \"$@\"; do last=\"$arg\"; done; flavor=stable; if [ \"$3\" = --experimental ]; then flavor=experimental; fi; cp '{}/'\"$flavor\"'/codex_app_server_protocol.v2.schemas.json' \"$last/codex_app_server_protocol.v2.schemas.json\"; exit 0; fi\nexec python3 \"$(dirname \"$0\")/runtime.py\"\n", schemas.display());
        // This fake server does not invoke code mode, but models its launch
        // check. Every fixture writes the same content, verified once.
        let helper = dir.path().join("codex-code-mode-host");
        fs::write(
            &helper,
            include_str!("../../../tests/fixtures/code-mode-host.py"),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let bin = dir.path().join("codex");
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        wonder_app_server::fixture_runtime::register(&bin);
        let config = LaunchConfig {
            codex_bin: bin,
            wonder_version: "test".into(),
            codex_home: None,
        };
        let runtime = AppServerClient::spawn_with_notification_sink(
            config.clone(),
            notification_sink(store.clone()),
        )
        .await
        .unwrap();
        store.start_event_epoch("epoch").await.expect("sync epoch");
        let state = AppState {
            projects: crate::projects::ProjectRuntime::configured(
                config.codex_bin.clone(),
                "test".into(),
                dir.path(),
                dir.path(),
                crate::ingestion::notification_sink(store.clone()),
            ),
            claude: None,
            opencode: None,
            cursor: None,
            ingestion: Ingestion::default(),
            store,
            logger: Arc::new(
                crate::logging::JsonlLogger::new(dir.path().join("logs"), "test.jsonl").unwrap(),
            ),
            loopback_capability: "test".into(),
            host_epoch: "epoch".into(),
            started_at: "now".into(),
            events: tokio::sync::broadcast::channel(256).0,
            journal_wake: Default::default(),
            revocations: tokio::sync::broadcast::channel(64).0,
            pairing: Arc::new(tokio::sync::Mutex::new(Default::default())),
            public_origin: "https://wonder.test".into(),
            public_origin_file: None,
            home_routes: Default::default(),
            host_installation_id: "test".into(),
            data_dir: dir.path().to_path_buf(),
            codex_home: dir.path().to_path_buf(),
            denied_roots: vec![],
            provider_reconnect: Default::default(),
            installed_providers: Default::default(),
            dispatch_lock: Arc::new(tokio::sync::Mutex::new(())),
            dispatch_wake: Arc::default(),
            thread_waits: Arc::default(),
            update_admission: Arc::new(Default::default()),
            approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            runtime_catalog: Arc::new(tokio::sync::RwLock::new(Default::default())),
            computer_use_enabled: false,
            computer_use_bin: None,
            computer_supervisor: Arc::new(
                crate::computer_sessions::ComputerSessionSupervisor::default(),
            ),
            simulators: Arc::default(),
            voice: Arc::new(crate::voice::Voice::in_memory()),
            t3_code: Arc::new(crate::t3_code::T3Code::disabled()),
            claude_app: Arc::default(),
            terminals: Arc::default(),
            live_activity: Arc::default(),
            sealed_handshakes: Default::default(),
            relay: crate::relay::Manager::new(
                dir.path().to_path_buf(),
                crate::relay::Environment::disabled(),
                None,
            ),
        };
        *state.projects.codex.lock().await = runtime;
        (dir, state)
    }

    // Fixtures skip the installation probes for this shared fake; it must
    // still be a runtime the production verification accepts.
    #[tokio::test]
    async fn the_fixture_runtime_passes_production_verification() {
        let (dir, _state) = fixture().await;
        let verified = wonder_app_server::verify_runtime(&dir.path().join("codex"))
            .await
            .unwrap();
        assert_eq!(
            verified,
            crate::canonical_path::canonicalize(dir.path().join("codex")).unwrap()
        );
    }

    // Contract: one unavailable provider cannot block the other, and receipt,
    // streamed output and recovery stay with the chosen provider's transport.
    #[tokio::test]
    async fn claude_dispatch_survives_missing_codex_and_keeps_its_session_binding() {
        let (dir, mut state) = fixture().await;
        let entrypoint = dir.path().join("claude.py");
        fs::write(dir.path().join("startup-hold"), "").unwrap();
        fs::write(&entrypoint, r#"import json, sys, time
from pathlib import Path
root=Path(__file__).parent
for line in sys.stdin:
    r=json.loads(line)
    if 'id' not in r: continue
    p=r.get('params',{}); m=r.get('method'); result={}
    if m=='initialize':
        (root/'startup-entered').touch()
        while (root/'startup-hold').exists(): time.sleep(.01)
        if (root/'startup-fail').exists(): sys.exit(1)
        result={'wonderBridge':{'protocolVersion':1,'family':'claude'},'capabilities':{'experimentalApi':True}}
    elif m=='account/read': result={'connected':True}
    elif m=='model/list': result={'data':[{'id':'claude:haiku','displayName':(root/'haiku-label').read_text() if (root/'haiku-label').exists() else 'Haiku 4.5'}]}
    elif m=='thread/start':
        assert p['wonderPolicy']['mode']=='workspace'
        assert p['model']=='claude:haiku'
        result={'thread':{'id':'claude-owned','sessionId':'sdk-owned'}}
    elif m=='turn/start':
        assert p['threadId']=='claude-owned'
        # Only the host's Cua Driver configuration survives provider routing;
        # this host has computer use off, so the driver must be disabled.
        assert set(p['config']) == {'mcp_servers.cua_driver'}
        assert p['config']['mcp_servers.cua_driver']['enabled'] is False
        result={'turn':{'id':'claude-turn'}}
    elif m=='thread/resume': result={'thread':{'id':'claude-owned','status':{'type':'idle'}}}
    elif m=='thread/turns/list': result={'data':[{'id':'claude-turn','status':'completed'}]}
    elif m=='thread/items/list': result={'data':[{'turnId':'claude-turn','item':{'id':'reply','type':'agentMessage','text':'Claude routed correctly'}}]}
    print(json.dumps({'id':r['id'],'result':result}),flush=True)
    if m=='turn/start':
        item={'id':'reply','type':'agentMessage','text':'Claude routed correctly'}
        print(json.dumps({'method':'item/completed','params':{'threadId':'claude-owned','turnId':'claude-turn','item':item}}),flush=True)
        print(json.dumps({'method':'turn/completed','params':{'threadId':'claude-owned','turn':{'id':'claude-turn','status':'completed','items':[item]}}}),flush=True)
"#).unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime {
            client: Arc::new(tokio::sync::Mutex::new(AppServerClient::unavailable(
                "test".into(),
                notification_sink(state.store.clone()),
            ))),
            config: wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint,
                state_dir: dir.path().join("claude-state"),
                npm_cli: None,
                claude_config_dir: None,
                wonder_version: "test".into(),
                family: "claude",
            },
        }));
        state.projects.codex.lock().await.shutdown().await.unwrap();
        state
            .projects
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .codex_bin = dir.path().join("missing-codex");
        let workspace = crate::canonical_path::canonicalize(
            dir.path().join("d19b0f7e-97bb-45d7-8206-324a62386dd9"),
        )
        .unwrap()
        .to_string_lossy()
        .into_owned();
        crate::test_support::project_thread(
            &state.store,
            "835be06d-38a8-490e-8e25-3b832e4c6314",
            "Claude",
            &workspace,
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Claude),
            Some("claude:haiku"),
            None,
            "now",
        )
        .await
        .unwrap();
        let _service = spawn(state.clone()).await;
        async fn entered(path: &std::path::Path) {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !path.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("bridge reached initialize");
        }
        entered(&dir.path().join("startup-entered")).await;
        let runtime = || state.ingestion.provider_runtime_state(AgentFamily::Claude);
        assert!(
            runtime().starting,
            "A held initial handshake is pending, not failed"
        );
        assert!(!runtime().alive);
        fs::write(dir.path().join("startup-fail"), "").unwrap();
        fs::remove_file(dir.path().join("startup-hold")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while runtime().starting {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("failed initialization becomes actionable");
        assert!(!runtime().alive);
        // A retry must not hide the failure until it actually succeeds.
        fs::write(dir.path().join("startup-hold"), "").unwrap();
        fs::remove_file(dir.path().join("startup-fail")).unwrap();
        fs::remove_file(dir.path().join("startup-entered")).unwrap();
        entered(&dir.path().join("startup-entered")).await;
        assert!(!runtime().starting);
        fs::remove_file(dir.path().join("startup-hold")).unwrap();
        ready(&state).await;
        assert!(runtime().alive);
        assert!(!runtime().starting);
        assert!(
            state
                .ingestion
                .provider_readiness(&state.store, Some(AgentFamily::Claude))
                .await
                .ready
        );
        assert!(
            !state
                .ingestion
                .provider_readiness(&state.store, Some(AgentFamily::Codex))
                .await
                .ready
        );
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                "claude-client",
                "hello",
                "hash",
                "835be06d-38a8-490e-8e25-3b832e4c6314",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        crate::projects::dispatch(state.clone(), message.clone()).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if state
                    .store
                    .message_by_id(&message.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state
                    == "completed"
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|e| {
            panic!(
                "Claude receipt did not settle: {e}; {:?}",
                fs::read_to_string(dir.path().join("logs/test.jsonl"))
            )
        });
        let binding = state
            .store
            .runtime_binding("835be06d-38a8-490e-8e25-3b832e4c6314")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.family, AgentFamily::Claude);
        assert_eq!(binding.session_id.as_deref(), Some("sdk-owned"));
        assert_eq!(
            state
                .store
                .assistant_messages_for_conversation("835be06d-38a8-490e-8e25-3b832e4c6314")
                .await
                .unwrap()[0]
                .text,
            "Claude routed correctly"
        );
        assert_eq!(
            state.runtime_catalog.read().await.models[0]
                .service_tiers
                .len(),
            0
        );
        // An SDK activation in the bridge refreshes the phone's labels now,
        // not on the next five-minute poll.
        fs::write(dir.path().join("haiku-label"), "Haiku 5.5").unwrap();
        let runtime_id = state
            .claude
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        notification_sink(state.store.clone())(json!({"method":"runtime/activated",
            "params":{"agentFamily":"claude","version":"0.3.300"},"_wonderRuntimeId":runtime_id}))
        .await
        .unwrap();
        let label = || async {
            state
                .runtime_catalog
                .read()
                .await
                .models
                .iter()
                .find(|m| m.agent_family == AgentFamily::Claude)
                .map(|m| m.display_name.clone())
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while label().await.as_deref() != Some("Haiku 5.5") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("activation rediscovers the Claude catalog");
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
    }

    pub(crate) async fn ready(state: &AppState) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if state.ingestion.readiness(&state.store).await.ready
                    && state.store.next_notification().await.unwrap().is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("execution should recover");
    }

    async fn active_message(state: &AppState, turn: &str) -> wonder_store::StoredMessage {
        let inserted = state
            .store
            .insert_message(
                "owner",
                turn,
                "hello",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "now",
            )
            .await
            .unwrap();
        let wonder_store::MessageInsert::Inserted(message) = inserted else {
            panic!("new message")
        };

        crate::test_support::bind_project_thread(
            state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            wonder_store::AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        state
            .store
            .update_message_delivery(&message.id, "accepted_by_codex", Some("thread"), Some(turn))
            .await
            .unwrap();
        message
    }

    /// Codex bundled with ChatGPT was updated past what Wonder supports. The
    /// host must say so everywhere an owner or the phone looks, while the other
    /// provider keeps working and gating stays "any provider usable".
    #[tokio::test]
    async fn unsupported_codex_is_reported_not_ready_in_user_language() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut state) = fixture().await;
        // Present and executable, but without the code-mode helper Wonder
        // requires: the same Incompatible class as an unknown protocol contract.
        fs::create_dir(dir.path().join("updated")).unwrap();
        let codex = dir.path().join("updated/codex");
        fs::write(&codex, "#!/bin/sh\necho 'codex-cli 9.9.9'\n").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        state.projects = crate::projects::ProjectRuntime::configured(
            codex,
            "test".into(),
            dir.path(),
            dir.path(),
            notification_sink(state.store.clone()),
        );
        let _service = spawn(state.clone()).await;

        let error = crate::projects::codex_rpc(&state).await.err().unwrap();
        assert_eq!(error, CODEX_UNSUPPORTED_DETAIL);
        let codex = state
            .ingestion
            .provider_readiness(&state.store, Some(AgentFamily::Codex))
            .await;
        assert!(!codex.ready);
        assert_eq!(codex.reason, "runtime_incompatible");
        assert_eq!(codex.detail, CODEX_UNSUPPORTED_DETAIL);
        // This fixture has no other provider; the report identifies the incompatible runtime.
        assert!(!state.ingestion.readiness(&state.store).await.ready);
        let report = state.ingestion.readiness_report(&state.store).await;
        assert!(!report.ready);
        assert_eq!(report.reason, "runtime_incompatible");
        use tower::ServiceExt;
        let response = crate::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/readyz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap();
        crate::tests::validate_http_contract("executionReadiness", &body);
        assert_eq!(body["reason"], "runtime_incompatible");
        assert_eq!(
            body["providers"],
            json!([{"id":"codex","ready":false,"reason":"runtime_incompatible","detail":CODEX_UNSUPPORTED_DETAIL}])
        );

        // Clearing incompatibility alone cannot establish a live transport.
        state
            .ingestion
            .set_runtime_incompatible(AgentFamily::Codex, false);
        assert!(!state.ingestion.readiness_report(&state.store).await.ready);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// One provider Cupola refuses to run is that provider's problem: while
    /// another can run work, `/readyz` stays 200 (the phone's route probe and
    /// `cupola update` read it) and names the refused provider.
    #[tokio::test]
    async fn one_refused_provider_keeps_readyz_ready_and_is_reported_on_its_own() {
        let (_dir, state) = fixture().await;
        let _service = spawn(state.clone()).await;
        ready(&state).await;
        state
            .ingestion
            .set_runtime_incompatible(AgentFamily::Claude, true);
        use tower::ServiceExt;
        let response = crate::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/readyz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap();
        crate::tests::validate_http_contract("executionReadiness", &body);
        assert_eq!(body["ready"], true);
        assert_eq!(body["reason"], "ready");
        let providers = body["providers"].as_array().unwrap();
        let by_id = |id: &str| providers.iter().find(|p| p["id"] == id).unwrap();
        assert_eq!(by_id("codex")["ready"], true);
        assert_eq!(by_id("claude")["ready"], false);
        assert_eq!(by_id("claude")["reason"], "runtime_incompatible");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn readiness_reason_separates_missing_runtime_from_recovery() {
        let (_dir, state) = fixture().await;
        let readiness = state.ingestion.readiness(&state.store).await;
        assert!(!readiness.ready);
        assert_eq!(readiness.reason, "runtime_unavailable");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn overflow_while_projection_is_blocked_preserves_messages_and_approval() {
        let (_dir, state) = fixture().await;
        let _service = spawn(state.clone()).await;
        ready(&state).await;
        active_message(&state, "turn").await;
        let guard = state.dispatch_lock.lock().await;
        let mut observer = state.projects.codex.lock().await.subscribe_notifications();
        state
            .projects
            .codex
            .lock()
            .await
            // Evict both reply chunks and the approval from the 256-slot
            // observer with metadata, rather than making this correctness
            // check a benchmark of 600 chat projections against ready's timer.
            .request(
                "thread/read",
                json!({"mode":"burst","count":2,"deltaSize":300,"activations":256}),
            )
            .await
            .unwrap();
        assert!(matches!(
            observer.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(3))
        ));
        while let Ok(notification) = observer.try_recv() {
            assert_eq!(notification["method"], "runtime/activated");
        }
        assert!(!state.ingestion.readiness(&state.store).await.ready);
        use tower::ServiceExt;
        for (path, expected) in [("/healthz", 200), ("/readyz", 503)] {
            let response = crate::router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected);
        }
        let host = crate::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/host/status")
                    .header("x-wonder-loopback-capability", "test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let host: Value =
            serde_json::from_slice(&axum::body::to_bytes(host.into_body(), 65536).await.unwrap())
                .unwrap();
        crate::tests::validate_http_contract("hostStatus", &host);
        crate::tests::validate_http_contract("executionReadiness", &host["execution"]);
        assert_eq!(host["execution"]["ready"], false);
        assert!(host["execution"]["reason"].as_str().is_some());
        assert_eq!(host["state"], "degraded");
        let rejected = crate::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/messages")
                    .header("x-wonder-loopback-capability", "test")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status().as_u16(), 503);
        drop(guard);
        ready(&state).await;
        let messages = state
            .store
            .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "x".repeat(600));
        let approvals = state.store.list_pending_approvals().await.unwrap();
        assert_eq!(approvals.len(), 1);
        assert!(
            approvals[0].server_request_id.ends_with(":2"),
            "server request IDs may equal an outstanding client request ID"
        );
        let params = serde_json::from_str(&approvals[0].params_json).unwrap();
        assert!(state
            .ingestion
            .approval_client(&params)
            .is_some_and(|client| Arc::ptr_eq(&client, &state.projects.codex)));
        assert!(state.store.next_notification().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stopped_consumer_restarts_and_replays_an_interrupted_receipt() {
        let (_dir, state) = fixture().await;
        let _service = spawn(state.clone()).await;
        ready(&state).await;
        active_message(&state, "turn").await;
        let guard = state.dispatch_lock.lock().await;
        let notification = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"item","delta":{"text":"once"}}});
        state
            .store
            .enqueue_notification(&notification)
            .await
            .unwrap();
        let (id, mut notification) = state.store.next_notification().await.unwrap().unwrap();
        notification["_wonderInboxId"] = json!(id);
        let key = crate::app_server_notification_key(
            "item/agentMessage/delta",
            &notification,
            &notification["params"],
        )
        .unwrap();
        state.store.start_notification(id).await.unwrap();
        // The interrupted consumer committed the text, its receipt and its
        // delta event, then stopped before acknowledging the inbox row.
        let mut event = crate::event_envelope(
            &state,
            wonder_api::WonderEvent::AssistantDelta {
                text: String::new(),
            },
            crate::EventContext {
                conversation_id: Some("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: Some("item".into()),
                ..Default::default()
            },
        );
        let (_, appended) = state
            .store
            .upsert_assistant_deltas_with_event(
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "thread",
                "turn",
                "item",
                &[("once", key.as_str())],
                "now",
                Some(&mut event),
            )
            .await
            .unwrap();
        assert!(appended);
        state
            .ingestion
            .inner
            .lock()
            .unwrap()
            .consumer
            .as_ref()
            .unwrap()
            .abort();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let readiness = state.ingestion.readiness(&state.store).await;
        assert!(!readiness.ready);
        assert_eq!(readiness.reason, "recovering");
        drop(guard);
        ready(&state).await;
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        assert_eq!(
            state
                .store
                .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()[0]
                .text,
            "once"
        );
        let deltas: Vec<String> = state
            .store
            .events_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.event {
                wonder_api::WonderEvent::AssistantDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["once"]);
    }

    #[tokio::test]
    async fn goal_continuation_stays_in_parent_after_goal_is_cleared() {
        let (_dir, state) = fixture().await;
        crate::test_support::bind_project_thread(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            wonder_store::AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let created = crate::goals::set(
            axum::extract::State(state.clone()),
            axum::extract::Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".to_owned()),
            axum::Json(
                json!({"objective":"Finish the review","tokenBudget":1000,"timeBudgetSeconds":600}),
            ),
        )
        .await;
        assert_eq!(created.status(), axum::http::StatusCode::OK);
        assert_eq!(
            state
                .store
                .goal_time_limit("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()
                .unwrap()
                .budget_seconds,
            600
        );
        let start = json!({"method":"turn/started","params":{"threadId":"thread","turn":{"id":"continuation","status":"inProgress"}}});
        assert!(crate::process_app_server_notification(&state, start, false).await);
        assert_eq!(
            state
                .store
                .goal_conversation_for_turn("thread", "continuation")
                .await
                .unwrap()
                .as_deref(),
            Some("d19b0f7e-97bb-45d7-8206-324a62386dd9")
        );
        let removed = crate::goals::clear(
            axum::extract::State(state.clone()),
            axum::extract::Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".to_owned()),
        )
        .await;
        assert_eq!(removed.status(), axum::http::StatusCode::NO_CONTENT);
        let delta = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"continuation","itemId":"reply","sequence":1,"delta":{"text":"Finished"}}});
        assert!(crate::process_app_server_notification(&state, delta, false).await);
        let messages = state
            .store
            .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Finished");
    }

    #[tokio::test]
    async fn goal_time_limit_pauses_after_active_time() {
        let (dir, state) = fixture().await;
        crate::test_support::bind_project_thread(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            wonder_store::AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let created = crate::goals::set(
            axum::extract::State(state.clone()),
            axum::extract::Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".to_owned()),
            axum::Json(json!({"objective":"Finish a long review","timeBudgetSeconds":60})),
        )
        .await;
        assert_eq!(created.status(), axum::http::StatusCode::OK);
        let goal_path = dir.path().join("goal.json");
        let mut goal: Value = serde_json::from_slice(&fs::read(&goal_path).unwrap()).unwrap();
        goal["timeUsedSeconds"] = json!(59);
        fs::write(&goal_path, serde_json::to_vec(&goal).unwrap()).unwrap();
        crate::goals::enforce_time_limits_now(&state).await;
        let goal: Value = serde_json::from_slice(&fs::read(&goal_path).unwrap()).unwrap();
        assert_eq!(goal["status"], "active");
        let mut goal = goal;
        goal["timeUsedSeconds"] = json!(60);
        fs::write(&goal_path, serde_json::to_vec(&goal).unwrap()).unwrap();
        crate::goals::enforce_time_limits_now(&state).await;
        let goal: Value = serde_json::from_slice(&fs::read(&goal_path).unwrap()).unwrap();
        assert_eq!(goal["status"], "paused");
        assert_eq!(
            state
                .store
                .goal_time_limit("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()
                .unwrap()
                .budget_seconds,
            60
        );
    }

    #[tokio::test]
    async fn runtime_eof_restarts_reconciles_history_and_expires_old_approvals_without_resubmission(
    ) {
        let (dir, state) = fixture().await;
        let _service = spawn(state.clone()).await;
        ready(&state).await;
        let message = active_message(&state, "turn").await;
        let uncertain = active_message(&state, "unknown-turn").await;
        state
            .projects
            .codex
            .lock()
            .await
            .request("thread/read", json!({"mode":"burst","count":1}))
            .await
            .unwrap();
        ready(&state).await;
        let old = state.projects.codex.lock().await.health();
        assert_eq!(state.store.list_pending_approvals().await.unwrap().len(), 1);
        state
            .projects
            .codex
            .lock()
            .await
            .request("thread/read", json!({"mode":"die"}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            while old.is_alive() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!state.ingestion.readiness(&state.store).await.ready);
        ready(&state).await;
        assert_ne!(old.id(), state.projects.codex.lock().await.health().id());
        crate::projects::recover(&state).await;
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
        assert_eq!(
            state
                .store
                .message_by_id(&uncertain.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        assert!(!state
            .store
            .requeue_message_for_retry(&uncertain.id)
            .await
            .unwrap());
        assert_eq!(
            state
                .store
                .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()[0]
                .text,
            "recovered"
        );
        assert!(state
            .store
            .list_pending_approvals()
            .await
            .unwrap()
            .is_empty());
        state
            .projects
            .codex
            .lock()
            .await
            .request(
                "thread/read",
                json!({"mode":"burst","count":0,"approvalId":2}),
            )
            .await
            .unwrap();
        ready(&state).await;
        let new_approvals = state.store.list_pending_approvals().await.unwrap();
        assert_eq!(
            new_approvals.len(),
            1,
            "a reused numeric request ID belongs to a new runtime"
        );
        assert!(!new_approvals[0].server_request_id.starts_with(old.id()));
        assert!(!fs::read_to_string(dir.path().join("requests"))
            .unwrap()
            .lines()
            .any(|line| line == "turn/start"));
    }

    #[tokio::test]
    async fn fresh_service_replays_persisted_inbox_and_does_not_resurrect_dead_requests() {
        let (dir, mut state) = fixture().await;
        let message = active_message(&state, "turn").await;
        let old_id = state.projects.codex.lock().await.health().id().to_owned();
        state.store.enqueue_notification(&json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"item","delta":{"text":"saved before restart"}}})).await.unwrap();
        state.store.enqueue_notification(&json!({"_wonderRuntimeId":old_id,"id":42,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","turnId":"turn"}})).await.unwrap();
        state.projects.codex.lock().await.shutdown().await.unwrap();
        state.store = Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let _service = spawn(state.clone()).await;
        ready(&state).await;
        crate::projects::recover(&state).await;
        assert_eq!(
            state
                .store
                .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()[0]
                .text,
            "saved before restart"
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
        assert!(state
            .store
            .list_pending_approvals()
            .await
            .unwrap()
            .is_empty());
        assert!(state.store.next_notification().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn storage_failure_degrades_readiness_and_reader_retries_original_envelope() {
        let (dir, state) = fixture().await;
        let _service = spawn(state.clone()).await;
        ready(&state).await;
        active_message(&state, "turn").await;
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("ALTER TABLE notification_inbox RENAME TO unavailable_inbox")
            .execute(&pool)
            .await
            .unwrap();
        let client = state.projects.codex.clone();
        let request = tokio::spawn(async move {
            client
                .lock()
                .await
                .request("thread/read", json!({"mode":"burst","count":1}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(350)).await;
        let readiness = state.ingestion.readiness(&state.store).await;
        assert!(!readiness.ready);
        assert_eq!(readiness.reason, "storage_unavailable");
        assert!(state
            .ingestion
            .inner
            .lock()
            .unwrap()
            .providers
            .get(&AgentFamily::Codex)
            .unwrap()
            .health
            .as_ref()
            .unwrap()
            .storage_blocked());
        sqlx::query("ALTER TABLE unavailable_inbox RENAME TO notification_inbox")
            .execute(&pool)
            .await
            .unwrap();
        request.await.unwrap().unwrap();
        ready(&state).await;
        assert_eq!(
            state
                .store
                .assistant_messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()[0]
                .text,
            "x"
        );
        assert_eq!(state.store.list_pending_approvals().await.unwrap().len(), 1);
    }

    /// An OpenCode bridge that logs each launch and check. Files beside it
    /// choose its answer: `opencode-ready` signs it in, `opencode-flaky`
    /// fails the check, `opencode-foreign-models` lists an unusable catalog.
    fn opencode_bridge(dir: &tempfile::TempDir, state: &mut AppState) {
        let bridge = dir.path().join("opencode-bridge.py");
        fs::write(
            &bridge,
            r#"import json, os, sys, threading, time
root = os.path.dirname(__file__)
with open(root + '/opencode-launches', 'a') as log: log.write('launch\n')
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request or 'method' not in request: continue
    method = request['method']
    if method == 'initialize':
        result = {'wonderBridge': {'protocolVersion': 1, 'family': 'opencode'}, 'capabilities': {'experimentalApi': True}}
    elif method == 'account/read':
        forced = request.get('params', {}).get('refresh') is True
        with open(root + '/opencode-checks', 'a') as log: log.write('refresh\n' if forced else 'routine\n')
        if os.path.exists(root + '/opencode-hold-check'):
            def held_check(request):
                open(root + '/opencode-check-held', 'w').close()
                while os.path.exists(root + '/opencode-hold-check'): time.sleep(0.01)
                print(json.dumps({'id': request['id'], 'result': {'connected': True, 'availability': 'ready'}}), flush=True)
            threading.Thread(target=held_check, args=(request,), daemon=True).start()
            continue
        ready = os.path.exists(root + '/opencode-ready')
        flaky = os.path.exists(root + '/opencode-flaky')
        result = {'connected': ready and not flaky, 'availability': 'unavailable' if flaky else 'ready' if ready else 'notInstalled'}
    elif method == 'model/list':
        model = 'cursor:other' if os.path.exists(root + '/opencode-foreign-models') else 'opencode:local/chosen'
        result = {'data': [{'id': model, 'model': model, 'displayName': 'Chosen', 'agentFamily': 'opencode', 'hidden': False, 'isDefault': True, 'supportedReasoningEfforts': [], 'defaultReasoningEffort': None}], 'nextCursor': None}
    else:
        result = {'data': [], 'nextCursor': None}
    print(json.dumps({'id': request['id'], 'result': result}), flush=True)
"#,
        )
        .unwrap();
        state.opencode = Some(Arc::new(crate::claude::Runtime {
            client: Arc::new(tokio::sync::Mutex::new(
                wonder_app_server::AppServerClient::unavailable(
                    "test".into(),
                    notification_sink(state.store.clone()),
                ),
            )),
            config: wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint: bridge,
                state_dir: dir.path().join("opencode-runtime"),
                npm_cli: None,
                wonder_version: "test".into(),
                family: "opencode",
                claude_config_dir: None,
            },
        }));
    }

    fn opencode_launches(dir: &tempfile::TempDir) -> usize {
        fs::read_to_string(dir.path().join("opencode-launches"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    // Contract: an OpenCode that is not set up keeps one running runtime and
    // says what to fix; it is not restarted every 30 seconds, and a re-check
    // once it is set up makes it ready without a restart.
    #[tokio::test]
    async fn provider_not_set_up_keeps_its_runtime_and_wakes_ready() {
        let (dir, mut state) = fixture().await;
        opencode_bridge(&dir, &mut state);
        let launches = || opencode_launches(&dir);
        let family = AgentFamily::OpenCode;
        let missing = crate::claude::SetupIssue::NotInstalled;
        let _service = spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while state.ingestion.parked(family) != Some(missing) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the provider is reported as not installed");
        let readiness = state
            .ingestion
            .provider_readiness(&state.store, Some(family))
            .await;
        assert_eq!(
            (readiness.ready, readiness.reason, readiness.detail),
            (false, "not_set_up", missing.detail(family))
        );
        assert_eq!(
            crate::projects::rpc_for(&state, family)
                .await
                .err()
                .as_deref(),
            Some(missing.detail(family)),
            "a running runtime is not used before its provider is set up"
        );
        // Restarting to learn the same answer took 2 s, then 4 s, up to 30 s.
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(launches(), 1, "the runtime is kept, not restarted");
        assert!(state.ingestion.provider_runtime_state(family).alive);

        fs::write(dir.path().join("opencode-ready"), "").unwrap();
        state.ingestion.wake_setup_check(family);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !state
                .ingestion
                .provider_readiness(&state.store, Some(family))
                .await
                .ready
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a wake after setup makes the provider ready");
        assert_eq!(launches(), 1, "ready on the same runtime");
        assert!(state
            .runtime_catalog
            .read()
            .await
            .models
            .iter()
            .any(|m| m.id == "opencode:local/chosen"));
        assert!(crate::projects::rpc_for(&state, family).await.is_ok());

        // A check that fails on a working provider never reads as "not
        // installed" and does not restart it.
        let checks = || {
            fs::read_to_string(dir.path().join("opencode-checks"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let recheck = |before: usize| {
            // Providers refresh must check a ready provider too, including
            // after a transient discovery failure retained its ready state.
            state.ingestion.wake_setup_check(family);
            tokio::time::timeout(Duration::from_secs(10), async move {
                while checks() == before {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
        };
        fs::write(dir.path().join("opencode-flaky"), "").unwrap();
        recheck(checks())
            .await
            .expect("the provider is checked again");
        assert!(
            state
                .ingestion
                .provider_readiness(&state.store, Some(family))
                .await
                .ready
        );
        assert_eq!(state.ingestion.setup_issue(family), None);
        assert_eq!(launches(), 1);
        // A confirmed answer does replace it.
        fs::remove_file(dir.path().join("opencode-flaky")).unwrap();
        fs::remove_file(dir.path().join("opencode-ready")).unwrap();
        recheck(checks())
            .await
            .expect("the provider is checked again");
        assert_eq!(state.ingestion.parked(family), Some(missing));
        assert_eq!(launches(), 1);
        // Starting and every wake above checked now; only the periodic read
        // may let the runtime answer from its own catalog.
        let log = fs::read_to_string(dir.path().join("opencode-checks")).unwrap();
        assert!(log.lines().all(|line| line == "refresh"), "{log}");
        if let Some(runtime) = &state.opencode {
            runtime.client.lock().await.shutdown().await.unwrap();
        }
    }

    // Contract: discovery on a healthy bridge cannot keep a send waiting for
    // the lifecycle mutex before it even reaches the bridge's setup queue.
    #[tokio::test]
    async fn provider_discovery_leaves_the_send_transport_available() {
        let (dir, mut state) = fixture().await;
        opencode_bridge(&dir, &mut state);
        fs::write(dir.path().join("opencode-ready"), "").unwrap();
        let family = AgentFamily::OpenCode;
        let _service = spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !state
                .ingestion
                .provider_readiness(&state.store, Some(family))
                .await
                .ready
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the fixture starts ready");
        let runtime_id = state
            .opencode
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        let hold = dir.path().join("opencode-hold-check");
        fs::write(&hold, "").unwrap();
        state.ingestion.request_discovery(&runtime_id);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !dir.path().join("opencode-check-held").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovery is awaiting its account check");

        let sent = tokio::time::timeout(Duration::from_secs(10), async {
            let rpc = crate::projects::rpc_for(&state, family).await.unwrap();
            rpc.request("thread/list", json!({})).await.unwrap()
        })
        .await;
        // Always release discovery before asserting, including on regression.
        fs::remove_file(hold).unwrap();
        let reply = sent.expect("a send reaches the bridge while discovery is held");
        assert_eq!(reply.result.unwrap()["data"], json!([]));
        assert_eq!(opencode_launches(&dir), 1);
        state
            .opencode
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .shutdown()
            .await
            .unwrap();
    }

    // Contract: Reconnect wakes the provider's recovery loop at once, even
    // from a long retry delay, and reports only after the loop has finished
    // that request on the provider's own runtime. A runtime that answered
    // with an unusable catalog after starting is kept and validated again,
    // not shut down and restarted.
    #[tokio::test]
    async fn reconnect_wakes_recovery_and_waits_for_its_own_request() {
        let (dir, mut state) = fixture().await;
        opencode_bridge(&dir, &mut state);
        fs::write(dir.path().join("opencode-ready"), "").unwrap();
        fs::write(dir.path().join("opencode-foreign-models"), "").unwrap();
        let family = AgentFamily::OpenCode;
        let checks = || {
            fs::read_to_string(dir.path().join("opencode-checks"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let _service = spawn(state.clone()).await;
        let served = |request| {
            let state = state.clone();
            async move {
                while state.ingestion.reconnected(family, request).is_none() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        // Validation failed at 0, 2, 6 and 14 s; the next retry is 16 s away.
        tokio::time::timeout(Duration::from_secs(60), async {
            while checks() < 4 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("recovery validates the runtime again");
        assert_eq!(opencode_launches(&dir), 1, "the started runtime is kept");
        let refused = crate::projects::rpc_for(&state, family).await.err();
        assert!(
            refused
                .as_deref()
                .is_some_and(|e| e.contains("incompatible model list")),
            "work is refused with the real reason: {refused:?}"
        );

        fs::remove_file(dir.path().join("opencode-foreign-models")).unwrap();
        let request = state.ingestion.request_reconcile(family);
        tokio::time::timeout(Duration::from_secs(10), served(request))
            .await
            .expect("Reconnect wakes recovery instead of waiting out its retry delay");
        assert_eq!(state.ingestion.reconnected(family, request), Some(None));
        assert!(
            state
                .ingestion
                .provider_readiness(&state.store, Some(family))
                .await
                .ready
        );
        assert_eq!(opencode_launches(&dir), 1, "ready on the same runtime");

        // A healthy provider's live runtime does not answer a new request:
        // the provider is fenced until the loop has served it.
        let again = state.ingestion.request_reconcile(family);
        assert!(state.ingestion.provider_runtime_state(family).recovering);
        assert_eq!(state.ingestion.reconnected(family, again), None);
        tokio::time::timeout(Duration::from_secs(10), served(again))
            .await
            .expect("the second request is served");
        if let Some(runtime) = &state.opencode {
            runtime.client.lock().await.shutdown().await.unwrap();
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: finding one message's receipt reconciles the provider of its
    // conversation; other providers keep taking work. Without a known
    // provider, every provider is reconciled as before.
    #[tokio::test]
    async fn a_receipt_reconciles_only_its_own_provider() {
        let (_dir, state) = fixture().await;
        for family in [AgentFamily::Codex, AgentFamily::Claude] {
            state.ingestion.set_update_pending(family, false);
        }
        request_conversation_reconcile(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9").await;
        assert!(
            state
                .ingestion
                .provider_runtime_state(AgentFamily::Codex)
                .recovering
        );
        assert!(
            !state
                .ingestion
                .provider_runtime_state(AgentFamily::Claude)
                .recovering,
            "another provider is not fenced"
        );
        request_conversation_reconcile(&state, "unknown-conversation").await;
        assert!(
            state
                .ingestion
                .provider_runtime_state(AgentFamily::Claude)
                .recovering
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a notification another provider sent that keeps failing does
    // not hold this provider's recovery (and so its readiness); the projection
    // workers keep owning its retries. The provider that sent it, or a failure
    // nobody can be attributed, still settles before history is read.
    #[tokio::test]
    async fn another_providers_failing_notification_does_not_hold_recovery() {
        let (dir, state) = fixture().await;
        let codex = state.projects.codex.lock().await.health();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, codex.clone());
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("ALTER TABLE goal_threads RENAME TO unavailable_goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        state
            .store
            .enqueue_notification(&json!({"_wonderRuntimeId":codex.id(),
                "method":"thread/goal/cleared","params":{"threadId":"failing"}}))
            .await
            .unwrap();
        state
            .store
            .enqueue_notification(&json!({"method":"runtime/activated","params":{}}))
            .await
            .unwrap();
        let (failing, _) = state.store.next_notification().await.unwrap().unwrap();

        settle_inbox(&state, AgentFamily::Claude).await.unwrap();
        assert_eq!(state.store.notification_backlog().await.unwrap(), 1);
        assert_eq!(
            state.store.next_notification().await.unwrap().unwrap().0,
            failing,
            "the failing notification stays for its retries"
        );
        assert!(settle_inbox(&state, AgentFamily::Codex).await.is_err());

        state
            .store
            .enqueue_notification(
                &json!({"method":"thread/goal/cleared","params":{"threadId":"unowned"}}),
            )
            .await
            .unwrap();
        state.ingestion.inner.lock().unwrap().failing.clear();
        sqlx::query("DELETE FROM notification_inbox WHERE id=?")
            .bind(failing)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            settle_inbox(&state, AgentFamily::Claude).await.is_err(),
            "a failure nobody can be attributed still holds recovery"
        );

        sqlx::query("ALTER TABLE unavailable_goal_threads RENAME TO goal_threads")
            .execute(&pool)
            .await
            .unwrap();
        settle_inbox(&state, AgentFamily::Codex).await.unwrap();
        assert_eq!(state.store.notification_backlog().await.unwrap(), 0);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}
