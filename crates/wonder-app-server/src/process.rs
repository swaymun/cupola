use std::{
    borrow::Cow,
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

use serde_json::Value;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex, Semaphore};
use tokio::time::{timeout, Duration};

use crate::{
    initialize_request, is_allowed_method, require_experimental_api, JsonRpcError, RpcResponse,
};

const APP_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds taking the stdin lock and writing one frame. A runtime that stops
/// reading would otherwise hold every writer, and shutdown, indefinitely.
const STDIN_WRITE_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};
/// How long a provider bridge (ACP, Claude SDK) gets to exit after stdin
/// closes or SIGTERM before its process group is killed. This crate's tests
/// use a shorter grace that still outlasts the fixture's 2.2 s exit and its
/// two Python children stopping on a loaded Mac (3 s did not), and fits the
/// lifecycle test's 14 s shutdown bound with the 2 s reap.
const BRIDGE_EXIT_GRACE: Duration = if cfg!(test) {
    Duration::from_secs(6)
} else {
    Duration::from_secs(10)
};
/// In-flight requests the host sends one provider bridge generation; more
/// wait for a slot within the request timeout. This stays well below the
/// Claude runtime's own 128-request bound, so a reply racing the runtime's
/// slot release is never refused as overload.
const BRIDGE_MAX_IN_FLIGHT: usize = 64;
/// Bytes of redacted stderr kept per runtime generation, newest last.
#[cfg(debug_assertions)]
const STDERR_TAIL_BYTES: usize = 8 * 1024;
/// Only this much of each stderr line is read; the rest is discarded up to
/// the newline, so one long line never becomes several independently
/// redacted entries.
const STDERR_LINE_BYTES: usize = 1024;
/// How long a reader that hit EOF waits for stderr to report why.
const EXIT_CAUSE_GRACE: Duration = Duration::from_millis(250);
const CONTROL_PLANE_ENV_VARS: [&str; 8] = [
    "TS_AUTHKEY",
    "WONDER_CODEX_BIN",
    "WONDER_COMPUTER_USE_BIN",
    "WONDER_COMPUTER_USE_ENABLED",
    "WONDER_LOOPBACK_CAPABILITY",
    "WONDER_LOGO_PATH",
    "WONDER_PUBLIC_ORIGIN",
    "WONDER_PUBLIC_ORIGIN_FILE",
];

fn bridge_environment(
    command: &mut Command,
    family: &str,
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) {
    command
        .env_clear()
        .envs(crate::provider_environment::runtime_environment(
            family, variables,
        ));
}

#[cfg(unix)]
fn signal_owned_child(child: &Child, signal: i32, whole_group: bool) -> std::io::Result<()> {
    let Some(id) = child.id() else { return Ok(()) };
    let pid = i32::try_from(id).map_err(std::io::Error::other)?;
    // The retained, unreaped child owns this PID (and its fresh process group).
    // After wait reaps it, Child::id returns None and it cannot be signalled.
    let result = unsafe { libc::kill(if whole_group { -pid } else { pid }, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(target_os = "macos")]
fn only_group_leader_remains(child: &Child) -> bool {
    let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) else {
        return false;
    };
    // A full buffer is deliberately not enough evidence. The unreaped leader
    // cannot create children or release its PID while we inspect its group.
    let mut members = [0 as libc::pid_t; 2];
    let count = unsafe {
        libc::proc_listpgrppids(
            pid,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as i32,
        )
    };
    count == 1 && members[0] == pid
}

// Keep the group leader waitable until its remaining children are stopped.
// Reaping it first would release the PID and lose safe ownership of the group.
#[cfg(unix)]
async fn wait_unreaped(child: &Child) -> std::io::Result<()> {
    let Some(id) = child.id() else {
        return Ok(());
    };
    let mut exited = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    loop {
        // WNOHANG keeps this call off the blocking path; WNOWAIT preserves the
        // leader's PID even after a crash. No other owner waits on this Child.
        let stopped = {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    id,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
            unsafe { info.si_pid() != 0 }
        };
        if stopped {
            return Ok(());
        }
        if exited.recv().await.is_none() {
            return Err(std::io::Error::other("Child exit notification stopped"));
        }
    }
}

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RpcResponse, String>>>>>;

#[derive(Debug)]
pub enum RuntimeError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Protocol(String),
    Incompatible(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "App Server I/O error: {error}"),
            Self::Json(error) => write!(formatter, "App Server JSONL error: {error}"),
            Self::Protocol(message) => write!(formatter, "App Server protocol error: {message}"),
            Self::Incompatible(message) => {
                write!(formatter, "Codex runtime incompatible: {message}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

impl RuntimeError {
    /// The installed runtime is not one Wonder supports (unknown protocol
    /// contract, failed handshake or helper), as opposed to a transient failure.
    pub fn is_incompatible(&self) -> bool {
        matches!(self, Self::Incompatible(_))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_transport_failure(&self) -> bool {
        match self {
            Self::Io(_) => true,
            Self::Protocol(message) => {
                message.contains("App Server EOF")
                    || message.contains("App Server read error")
                    || message.contains("App Server response reader")
                    || message.contains("response channel closed")
                    || message.contains("App Server write timed out")
            }
            Self::Json(_) | Self::Incompatible(_) => false,
        }
    }
}

impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug)]
pub struct LaunchConfig {
    pub codex_bin: PathBuf,
    pub wonder_version: String,
    /// The Codex home (`CODEX_HOME`) this runtime uses. `None` keeps the
    /// owner's own home as the Mac's environment selects it; an additional
    /// account is a separate home Wonder owns.
    pub codex_home: Option<PathBuf>,
}

/// Wonder-owned provider bridge, using the same durable JSONL transport. This
/// has its own handshake; it never bypasses verification for a Codex process.
#[derive(Clone, Debug)]
pub struct BridgeLaunchConfig {
    pub node_bin: PathBuf,
    pub entrypoint: PathBuf,
    pub state_dir: PathBuf,
    pub npm_cli: Option<PathBuf>,
    pub wonder_version: String,
    /// The `wonderBridge.family` the bridge must announce (`claude`, `opencode`, `cursor`).
    pub family: &'static str,
    /// Claude only: the Claude Code configuration folder (`CLAUDE_CONFIG_DIR`)
    /// selected by the host. `None` preserves the owner's `CLAUDE_CONFIG_DIR`
    /// environment setting (or the CLI default).
    pub claude_config_dir: Option<PathBuf>,
}

impl LaunchConfig {
    /// Project threads use the owner's normal provider home and configuration.
    pub fn args(&self) -> Vec<String> {
        vec!["app-server".into(), "--stdio".into()]
    }
}

/// Called before a notification is broadcast. Returning an error keeps the
/// original envelope in the reader and retries with backpressure.
pub type NotificationSink =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

#[derive(Clone, Default)]
pub struct RuntimeHealth {
    id: Arc<String>,
    alive: Arc<AtomicBool>,
    storage_blocked: Arc<AtomicBool>,
    exit: Arc<Mutex<ExitDiagnostics>>,
}

/// Why one runtime generation stopped. `cause` is a fixed classification
/// safe for any build; `tail` is redacted stderr that only Diagnostics
/// (unoptimized) host builds keep. Optimized Release builds compile it out
/// and are never given raw runtime output.
#[derive(Default)]
struct ExitDiagnostics {
    cause: Option<&'static str>,
    #[cfg(debug_assertions)]
    tail: std::collections::VecDeque<String>,
    #[cfg(debug_assertions)]
    tail_bytes: usize,
    finished: bool,
}

impl ExitDiagnostics {
    /// Records the start of one stderr line; callers pass at most
    /// `STDERR_LINE_BYTES` and drop the rest of an overlong line.
    fn record(&mut self, line: &[u8], bridge: bool) {
        let line = String::from_utf8_lossy(&line[..line.len().min(STDERR_LINE_BYTES)]);
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        if let Some(cause) = classify_stderr(line, bridge) {
            // A specific cause replaces the generic one; the runtime's own
            // classified line replaces any other.
            if matches!(self.cause, None | Some(UNEXPECTED_ERROR))
                || (line.starts_with(RUNTIME_STOPPED_PREFIX) && cause != UNEXPECTED_ERROR)
            {
                self.cause = Some(cause);
            }
        }
        #[cfg(debug_assertions)]
        self.keep_tail(line);
    }

    #[cfg(debug_assertions)]
    fn keep_tail(&mut self, line: &str) {
        let line = redact_stderr_line(line);
        // Count the joining newline so the joined tail honors the bound.
        self.tail_bytes += line.len() + 1;
        self.tail.push_back(line);
        while self.tail_bytes > STDERR_TAIL_BYTES {
            let Some(oldest) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes -= oldest.len() + 1;
        }
    }
}

/// The Claude runtime writes `Claude runtime stopped: <cause>` before it exits.
const RUNTIME_STOPPED_PREFIX: &str = "Claude runtime stopped: ";
const RUNTIME_STOP_CAUSES: [&str; 3] = ["storage-or-transport", "frame-too-large", "invalid-frame"];
const UNEXPECTED_ERROR: &str = "unexpected-error";

fn classify_stderr(line: &str, bridge: bool) -> Option<&'static str> {
    if let Some(cause) = line.strip_prefix(RUNTIME_STOPPED_PREFIX) {
        let known = RUNTIME_STOP_CAUSES
            .into_iter()
            .find(|known| *known == cause.trim());
        return known.or(Some(UNEXPECTED_ERROR));
    }
    if line.contains("JavaScript heap out of memory") || line.contains("Allocation failed") {
        return Some("out-of-memory");
    }
    // Codex logs routinely to stderr; only a bridge's output means trouble.
    bridge.then_some(UNEXPECTED_ERROR)
}

/// Drops whole lines that may carry credentials, labeled or bare, and
/// hides the home folder.
#[cfg(debug_assertions)]
fn redact_stderr_line(line: &str) -> String {
    let lowered = line.to_ascii_lowercase();
    let labeled = [
        "token",
        "secret",
        "authorization",
        "bearer",
        "cookie",
        "password",
        "passwd",
        "credential",
        "api_key",
        "apikey",
        "api-key",
        "private",
        "x-api",
    ]
    .iter()
    .any(|needle| lowered.contains(needle));
    if labeled
        || line
            .split(|c: char| !is_secret_char(c))
            .any(looks_like_secret)
    {
        return "[redacted]".into();
    }
    match std::env::var("HOME") {
        Ok(home) if home.len() > 1 => line.replace(&home, "~"),
        _ => line.to_owned(),
    }
}

#[cfg(debug_assertions)]
fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '=' | '.')
}

/// Known key prefixes (`sk-`, `ghp_`, `xoxb-`, `AKIA`, JWTs) or a long run
/// mixing letters and digits, which is how unlabeled keys usually look.
#[cfg(debug_assertions)]
fn looks_like_secret(word: &str) -> bool {
    const PREFIXES: [&str; 12] = [
        "sk-",
        "sk_",
        "pk_",
        "rk_",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "xox",
        "AKIA",
        "eyJ",
    ];
    if word.len() >= 12 && PREFIXES.iter().any(|prefix| word.starts_with(prefix)) {
        return true;
    }
    word.len() >= 32
        && word.bytes().any(|b| b.is_ascii_digit())
        && word.bytes().any(|b| b.is_ascii_alphabetic())
}

impl RuntimeHealth {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    pub fn storage_blocked(&self) -> bool {
        self.storage_blocked.load(Ordering::SeqCst)
    }
    /// A fixed, safe classification of why this generation's runtime
    /// stopped (`out-of-memory`, `storage-or-transport`, ...), when known.
    pub fn exit_cause(&self) -> Option<&'static str> {
        self.exit.lock().unwrap_or_else(|e| e.into_inner()).cause
    }
    /// Redacted recent stderr of this generation. Diagnostics builds only:
    /// optimized Release builds keep none and report `exit_cause` instead.
    #[cfg(debug_assertions)]
    pub fn diagnostic_stderr(&self) -> String {
        let exit = self.exit.lock().unwrap_or_else(|e| e.into_inner());
        exit.tail
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct ReaderGuard {
    health: RuntimeHealth,
    pending: PendingRequests,
    failure: Cow<'static, str>,
}
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        // Close admission before draining. A racing request either already has
        // a sender here, or sees dead health in write_json and removes itself.
        self.health.alive.store(false, Ordering::SeqCst);
        for (_, sender) in self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            // A transport failure is an unknown outcome, never an RPC reply
            // claiming that the runtime rejected the submitted request.
            let _ = sender.send(Err(self.failure.to_string()));
        }
    }
}

pub struct AppServerClient {
    child: Option<Child>,
    /// Windows: owns the runtime's whole process tree (see `crate::job`).
    #[cfg(windows)]
    job: Option<crate::job::Job>,
    bridge: Option<&'static str>,
    health: RuntimeHealth,
    reader_task: tokio::task::JoinHandle<()>,
    notification_sink: Option<NotificationSink>,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    next_id: Arc<AtomicU64>,
    wonder_version: String,
    pending: PendingRequests,
    slots: Option<Arc<Semaphore>>,
    notification_tx: tokio::sync::broadcast::Sender<Value>,
}

/// Multiplexed requests bound to one runtime generation. Cloning does not own
/// the child process; restart closes old handles instead of retargeting them.
#[derive(Clone)]
pub struct RpcClient {
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    pending: PendingRequests,
    next_id: Arc<AtomicU64>,
    health: RuntimeHealth,
    slots: Option<Arc<Semaphore>>,
}

struct PendingRequest {
    id: u64,
    pending: PendingRequests,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

impl RpcClient {
    pub fn health(&self) -> RuntimeHealth {
        self.health.clone()
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<RpcResponse, RuntimeError> {
        if !self.health.is_alive() {
            return Err(RuntimeError::Protocol("App Server transport closed".into()));
        }
        if !is_allowed_method(method) && !crate::is_project_method(method) {
            return Err(RuntimeError::Protocol(format!(
                "App Server method is not allowlisted: {method}"
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.request_value(serde_json::json!({ "method": method, "id": id, "params": params }))
            .await
    }

    async fn request_value(&self, request: Value) -> Result<RpcResponse, RuntimeError> {
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| RuntimeError::Protocol("request id is required".into()))?;
        let started = tokio::time::Instant::now();
        let _slot = match &self.slots {
            Some(slots) => Some(
                timeout(APP_SERVER_REQUEST_TIMEOUT, slots.clone().acquire_owned())
                    .await
                    .map_err(|_| RuntimeError::Protocol("App Server response timed out".into()))?
                    .map_err(|_| RuntimeError::Protocol("App Server transport closed".into()))?,
            ),
            None => None,
        };
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);
        let _pending = PendingRequest {
            id,
            pending: self.pending.clone(),
        };
        if let Err(error) = self.write_json(&request).await {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(error);
        }
        let remaining = APP_SERVER_REQUEST_TIMEOUT.saturating_sub(started.elapsed());
        match timeout(remaining, receiver).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(message))) => Err(RuntimeError::Protocol(message)),
            Ok(Err(_)) => Err(RuntimeError::Protocol(
                "App Server response channel closed".into(),
            )),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(RuntimeError::Protocol(
                    "App Server response timed out".into(),
                ))
            }
        }
    }

    pub async fn respond_value(
        &self,
        id: Value,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    ) -> Result<(), RuntimeError> {
        self.write_json(&response_value(id, result, error)?).await
    }
    async fn write_json(&self, value: &Value) -> Result<(), RuntimeError> {
        if !self.health.is_alive() {
            return Err(RuntimeError::Protocol("App Server transport closed".into()));
        }
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let write = async {
            let mut guard = self.stdin.lock().await;
            let stdin = guard
                .as_mut()
                .ok_or_else(|| RuntimeError::Protocol("Runtime unavailable".into()))?;
            stdin.write_all(&line).await?;
            stdin.flush().await?;
            Ok(())
        };
        match timeout(STDIN_WRITE_TIMEOUT, write).await {
            Ok(result) => result,
            Err(_) => {
                // A frame may be partly written: this generation can no longer
                // be trusted, and the caller must not assume it was delivered.
                self.health.alive.store(false, Ordering::SeqCst);
                Err(RuntimeError::Protocol("App Server write timed out".into()))
            }
        }
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        self.fail_pending();
        self.reader_task.abort();
        self.health.alive.store(false, Ordering::SeqCst);
        if let Some(child) = self.child.as_mut() {
            #[cfg(unix)]
            if matches!(self.bridge, Some("opencode" | "cursor" | "claude")) {
                let _ = signal_owned_child(child, libc::SIGKILL, true);
            }
            let _ = child.start_kill();
        }
    }
}

impl AppServerClient {
    /// A closed transport lets authenticated local reads survive missing or
    /// incompatible installation. Recovery uses the same strict spawn checks.
    pub fn unavailable(wonder_version: String, sink: NotificationSink) -> Self {
        let (notification_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            child: None,
            #[cfg(windows)]
            job: None,
            bridge: None,
            health: RuntimeHealth::default(),
            reader_task: tokio::spawn(async {}),
            notification_sink: Some(sink),
            stdin: Arc::new(AsyncMutex::new(None)),
            next_id: Arc::new(AtomicU64::new(1)),
            wonder_version,
            pending: Arc::new(Mutex::new(HashMap::new())),
            slots: None,
            notification_tx,
        }
    }
    pub async fn spawn(config: LaunchConfig) -> Result<Self, RuntimeError> {
        let (notification_tx, _) = tokio::sync::broadcast::channel(256);
        Self::spawn_with_notifications(config, notification_tx, None).await
    }

    pub async fn spawn_with_notification_sink(
        config: LaunchConfig,
        sink: NotificationSink,
    ) -> Result<Self, RuntimeError> {
        let (tx, _) = tokio::sync::broadcast::channel(256);
        Self::spawn_with_notifications(config, tx, Some(sink)).await
    }

    pub fn health(&self) -> RuntimeHealth {
        self.health.clone()
    }

    /// A captured RPC generation may still serve history or an approval.
    /// Lifecycle callers hold the client lock before checking this, so no new
    /// handle can be captured between the check and an idle shutdown.
    pub fn has_rpc_handles(&self) -> bool {
        Arc::strong_count(&self.next_id) > 1
    }

    fn fail_pending(&self) {
        for (_, sender) in self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            let _ = sender.send(Err("App Server transport closed".into()));
        }
    }

    async fn spawn_with_notifications(
        config: LaunchConfig,
        notification_tx: tokio::sync::broadcast::Sender<Value>,
        notification_sink: Option<NotificationSink>,
    ) -> Result<Self, RuntimeError> {
        let (revision, settings) = crate::provider_environment::launch_settings("codex");
        let codex_bin = settings.binary.clone().unwrap_or(config.codex_bin.clone());
        #[cfg(feature = "test-support")]
        let canonical_bin = fixture_runtime::verify(&codex_bin).await?;
        #[cfg(not(feature = "test-support"))]
        let canonical_bin = verify_runtime(&codex_bin).await?;

        let mut command = Command::new(canonical_bin);
        // The owner's arguments are Codex's global options, so they precede
        // the `app-server` subcommand.
        command
            .kill_on_drop(true)
            .args(&settings.args)
            .args(config.args())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for variable in CONTROL_PLANE_ENV_VARS {
            command.env_remove(variable);
        }
        for (name, value) in crate::provider_environment::settings_environment(&settings) {
            command.env(name, value);
        }
        // An additional account's home belongs to Wonder and wins over the
        // owner's home setting for the main runtime. The owner's home is the
        // one the host started with: thread identity and protected folders
        // follow it, so a launch never uses a home saved since.
        let home: Option<PathBuf> = config.codex_home.clone().or_else(|| {
            crate::provider_environment::active_codex_home().map(std::path::Path::to_path_buf)
        });
        if let Some(home) = &home {
            command.env("CODEX_HOME", home);
        }
        let runtime = Self::spawn_transport(
            command,
            config.wonder_version,
            notification_tx,
            notification_sink,
            None,
        )
        .await?;
        crate::provider_environment::record_launch("codex", revision, &settings, home.as_deref());
        Ok(runtime)
    }

    pub async fn spawn_bridge(
        config: BridgeLaunchConfig,
        sink: NotificationSink,
    ) -> Result<Self, RuntimeError> {
        let (tx, _) = tokio::sync::broadcast::channel(256);
        Self::spawn_bridge_with_notifications(config, tx, Some(sink)).await
    }

    async fn spawn_bridge_with_notifications(
        config: BridgeLaunchConfig,
        notification_tx: tokio::sync::broadcast::Sender<Value>,
        notification_sink: Option<NotificationSink>,
    ) -> Result<Self, RuntimeError> {
        let mut command = Command::new(tokio::fs::canonicalize(&config.node_bin).await?);
        bridge_environment(&mut command, config.family, std::env::vars_os());
        if let Some(dir) = config
            .claude_config_dir
            .as_ref()
            .filter(|_| config.family == "claude")
        {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        let (revision, settings) = crate::provider_environment::launch_settings(config.family);
        let names: Vec<_> = crate::provider_environment::settings_environment(&settings)
            .map(|(name, value)| {
                command.env(name, value);
                name
            })
            .collect();
        // The bridge passes through only variables named here (the same
        // selection the service launch uses), so the owner's names join the
        // ones a service launch already selected.
        if !names.is_empty() {
            let inherited = std::env::var(crate::provider_environment::SELECTION).ok();
            command.env(
                crate::provider_environment::SELECTION,
                crate::provider_environment::merge_selection(inherited.as_deref(), &names),
            );
        }
        // The runtimes read these themselves: services/claude-runtime/sdk-runtime.mjs
        // and services/acp-runtime/catalog.mjs.
        let upper = config.family.to_ascii_uppercase();
        if let Some(binary) = &settings.binary {
            command.env(format!("WONDER_{upper}_BIN"), binary);
        }
        if !settings.args.is_empty() {
            command.env(
                format!("WONDER_{upper}_ARGS"),
                serde_json::to_string(&settings.args).unwrap_or_default(),
            );
        }
        #[cfg(unix)]
        if matches!(config.family, "opencode" | "cursor" | "claude") {
            // Native ACP and Claude SDK children inherit this private group.
            // Cleanup covers descendants remaining in it; detached/setpgid/setsid
            // descendants outside the group are not tracked.
            command.process_group(0);
        }
        command
            .arg(tokio::fs::canonicalize(&config.entrypoint).await?)
            .arg("--state-dir")
            .arg(&config.state_dir)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if config.family == "cursor" {
            command.arg("--provider").arg("cursor");
        }
        if let Some(npm) = config.npm_cli {
            command.arg("--npm-cli").arg(npm);
        }
        // The Wonder-owned bridge also serves project session metadata.
        let runtime = Self::spawn_transport(
            command,
            config.wonder_version,
            notification_tx,
            notification_sink,
            Some(config.family),
        )
        .await?;
        crate::provider_environment::record_launch(config.family, revision, &settings, None);
        Ok(runtime)
    }

    async fn spawn_transport(
        mut command: Command,
        wonder_version: String,
        notification_tx: tokio::sync::broadcast::Sender<Value>,
        notification_sink: Option<NotificationSink>,
        bridge: Option<&'static str>,
    ) -> Result<Self, RuntimeError> {
        let mut child = command.spawn()?;
        #[cfg(windows)]
        let job = {
            let job = crate::job::Job::kill_on_close()?;
            if let Some(handle) = child.raw_handle() {
                job.assign(handle)?;
            }
            Some(job)
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RuntimeError::Protocol("stdout/stdin pipe unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RuntimeError::Protocol("stdout pipe unavailable".into()))?;
        let stdin = Arc::new(AsyncMutex::new(Some(stdin)));
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_notification_tx = notification_tx.clone();
        let health = RuntimeHealth {
            id: Arc::new(format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )),
            ..RuntimeHealth::default()
        };
        health.alive.store(true, Ordering::SeqCst);
        // Keep why the runtime stopped instead of discarding its stderr.
        let stderr_done = Arc::new(tokio::sync::Notify::new());
        if let Some(stderr) = child.stderr.take() {
            let exit = health.exit.clone();
            let done = stderr_done.clone();
            tokio::spawn(async move {
                let mut stderr = BufReader::new(stderr);
                let mut line = Vec::new();
                // Inside an overlong line whose start was already recorded.
                let mut discarding = false;
                loop {
                    line.clear();
                    // Bound one line's buffer even when no newline comes.
                    match (&mut stderr)
                        .take(STDERR_LINE_BYTES as u64)
                        .read_until(b'\n', &mut line)
                        .await
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let ended = line.ends_with(b"\n");
                            if !std::mem::replace(&mut discarding, !ended) {
                                exit.lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .record(&line, bridge.is_some());
                            }
                        }
                    }
                }
                exit.lock().unwrap_or_else(|e| e.into_inner()).finished = true;
                done.notify_waiters();
            });
        } else {
            health
                .exit
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finished = true;
        }
        let reader_health = health.clone();
        let sink = notification_sink.clone();
        // Construct outside the future so aborting before its first poll also
        // closes the generation. Drop owns every exit, including sink panics.
        let reader_guard = ReaderGuard {
            health: reader_health.clone(),
            pending: reader_pending.clone(),
            failure: "App Server response reader stopped before response".into(),
        };
        let reader_task = tokio::spawn(async move {
            let mut reader_guard = reader_guard;
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        let finished = stderr_done.notified();
                        let known = |health: &RuntimeHealth| {
                            health
                                .exit
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .finished
                        };
                        if !known(&reader_health) {
                            let _ = timeout(EXIT_CAUSE_GRACE, finished).await;
                        }
                        reader_guard.failure = match reader_health.exit_cause() {
                            Some(cause) => {
                                format!("App Server EOF before response (runtime stopped: {cause})")
                                    .into()
                            }
                            None => "App Server EOF before response".into(),
                        };
                        break;
                    }
                    Err(_) => {
                        reader_guard.failure = "App Server read error before response".into();
                        break;
                    }
                };
                let mut value: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(_) => {
                        reader_guard.failure =
                            "App Server response reader protocol failure: malformed JSON".into();
                        break;
                    }
                };
                if !value.is_object() {
                    reader_guard.failure =
                        "App Server response reader protocol failure: non-object JSON frame".into();
                    break;
                }
                if value.get("method").is_none() {
                    if let Some(id) = value.get("id").and_then(Value::as_u64) {
                        let sender = reader_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                        if let Some(sender) = sender {
                            let result = serde_json::from_value::<RpcResponse>(value)
                                .map_err(|error| error.to_string());
                            let _ = sender.send(result);
                        }
                        continue;
                    }
                }
                if let Some(sink) = &sink {
                    if value.is_object() {
                        value["_wonderRuntimeId"] = Value::String(reader_health.id().to_owned());
                    }
                    while sink(value.clone()).await.is_err() {
                        reader_health.storage_blocked.store(true, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    reader_health.storage_blocked.store(false, Ordering::SeqCst);
                }
                let _ = reader_notification_tx.send(value);
            }
        });
        let mut client = Self {
            child: Some(child),
            #[cfg(windows)]
            job,
            bridge,
            health,
            reader_task,
            notification_sink,
            stdin,
            next_id: Arc::new(AtomicU64::new(1)),
            wonder_version,
            pending,
            slots: bridge.map(|_| Arc::new(Semaphore::new(BRIDGE_MAX_IN_FLIGHT))),
            notification_tx,
        };
        client.initialize_protocol(bridge).await?;
        Ok(client)
    }

    pub async fn initialize(&mut self) -> Result<Value, RuntimeError> {
        self.initialize_protocol(None).await
    }

    /// `bridge` names the family a Wonder-owned bridge must announce.
    async fn initialize_protocol(
        &mut self,
        bridge: Option<&'static str>,
    ) -> Result<Value, RuntimeError> {
        if self.next_id.load(Ordering::SeqCst) != 1 {
            return Err(RuntimeError::Protocol(
                "initialize may only be sent once".into(),
            ));
        }
        let request = initialize_request(&self.wonder_version).request;
        let response = self.request_value(serde_json::to_value(request)?).await?;
        let result = response
            .result
            .ok_or_else(|| RuntimeError::Incompatible("initialize returned no result".into()))?;
        if let Some(family) = bridge {
            if result
                .pointer("/wonderBridge/protocolVersion")
                .and_then(Value::as_u64)
                != Some(1)
                || result
                    .pointer("/wonderBridge/family")
                    .and_then(Value::as_str)
                    != Some(family)
            {
                return Err(RuntimeError::Protocol(format!(
                    "The {family} bridge returned an incompatible handshake"
                )));
            }
        } else {
            require_experimental_api(&result)
                .map_err(|message| RuntimeError::Incompatible(message.into()))?;
        }
        self.write_json(&serde_json::json!({ "method": "initialized", "params": {} }))
            .await?;
        self.next_id.store(2, Ordering::SeqCst);
        Ok(result)
    }

    /// Capture one transport generation while holding only the lifecycle lock.
    pub fn rpc(&self) -> RpcClient {
        RpcClient {
            stdin: self.stdin.clone(),
            pending: self.pending.clone(),
            next_id: self.next_id.clone(),
            health: self.health.clone(),
            slots: self.slots.clone(),
        }
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<RpcResponse, RuntimeError> {
        self.rpc().request(method, params).await
    }

    async fn request_value(&self, request: Value) -> Result<RpcResponse, RuntimeError> {
        self.rpc().request_value(request).await
    }

    pub fn subscribe_notifications(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.notification_tx.subscribe()
    }

    pub async fn respond(
        &self,
        id: u64,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    ) -> Result<(), RuntimeError> {
        self.respond_value(serde_json::json!(id), result, error)
            .await
    }

    pub async fn respond_value(
        &self,
        id: Value,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    ) -> Result<(), RuntimeError> {
        self.write_json(&response_value(id, result, error)?).await
    }

    pub async fn restart(&mut self, config: LaunchConfig) -> Result<(), RuntimeError> {
        self.health.alive.store(false, Ordering::SeqCst);
        self.stop_child().await?;
        let replacement = Self::spawn_with_notifications(
            config,
            self.notification_tx.clone(),
            self.notification_sink.clone(),
        )
        .await?;
        *self = replacement;
        Ok(())
    }

    pub async fn restart_bridge(&mut self, config: BridgeLaunchConfig) -> Result<(), RuntimeError> {
        self.stop_child().await?;
        *self = Self::spawn_bridge_with_notifications(
            config,
            self.notification_tx.clone(),
            self.notification_sink.clone(),
        )
        .await?;
        Ok(())
    }

    async fn write_json(&self, value: &Value) -> Result<(), RuntimeError> {
        self.rpc().write_json(value).await
    }

    async fn stop_child(&mut self) -> Result<(), RuntimeError> {
        self.health.alive.store(false, Ordering::SeqCst);
        self.fail_pending();
        // A writer blocked on a runtime that stops reading holds stdin; ending
        // the child must not wait for it. Killing the child releases that writer.
        let stdin_closed = match self.stdin.try_lock() {
            Ok(mut guard) => {
                if let Some(mut stdin) = guard.take() {
                    let _ = timeout(Duration::from_millis(200), stdin.shutdown()).await;
                }
                true
            }
            Err(_) => false,
        };
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        #[cfg(unix)]
        if matches!(self.bridge, Some("opencode" | "cursor" | "claude")) {
            if !stdin_closed {
                signal_owned_child(child, libc::SIGTERM, false)?;
            }
            // ACP: 4 s child exit, 2 s journals/turns, 2 s output; Claude SDK:
            // 2 s stdin grace, then 5 s after SIGTERM before SIGKILL. Add margin.
            // A normal exit or crash can also leave a native child behind.
            let _leader_exited = match timeout(BRIDGE_EXIT_GRACE, wait_unreaped(child)).await {
                Ok(result) => {
                    result?;
                    true
                }
                Err(_) => false,
            };
            let signalled = signal_owned_child(child, libc::SIGKILL, true);
            #[cfg(target_os = "macos")]
            let signalled = signalled.or_else(|error| {
                // Darwin returns EPERM for an all-zombie group. Accept only a
                // complete native snapshot containing this exited leader alone.
                if _leader_exited
                    && error.raw_os_error() == Some(libc::EPERM)
                    && only_group_leader_remains(child)
                {
                    return Ok(());
                }
                Err(error)
            });
            signalled?;
            // Keep ownership if reaping stalls, so Drop can retry safely.
            timeout(Duration::from_secs(2), child.wait())
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Provider bridge did not exit after stopping its process group",
                    )
                })??;
            return Ok(());
        }
        #[cfg(windows)]
        if matches!(self.bridge, Some("opencode" | "cursor")) {
            // ACP bridge: same 10 s budget as Unix, then end the whole tree.
            let grace = if stdin_closed {
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            };
            let _ = timeout(grace, child.wait()).await;
            if let Some(job) = self.job.as_ref() {
                job.terminate()?;
            }
            child.kill().await.ok();
            child.wait().await?;
            return Ok(());
        }
        let grace = if stdin_closed {
            Duration::from_secs(2)
        } else {
            Duration::ZERO
        };
        match timeout(grace, child.wait()).await {
            Ok(status) => {
                let _ = status?;
            }
            Err(_) => {
                child.kill().await?;
                let _ = child.wait().await?;
            }
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<(), RuntimeError> {
        self.stop_child().await
    }

    /// Stops every runtime at once, so each gets its whole exit grace within
    /// the host's one stop deadline instead of waiting its turn. Returns the
    /// first failure once all have stopped.
    pub async fn shutdown_together(
        clients: impl IntoIterator<Item = Arc<AsyncMutex<Self>>>,
    ) -> Result<(), RuntimeError> {
        let mut stopping = tokio::task::JoinSet::new();
        for client in clients {
            stopping.spawn(async move { client.lock().await.shutdown().await });
        }
        let mut first = None;
        while let Some(result) = stopping.join_next().await {
            let result = result.unwrap_or_else(|error| Err(std::io::Error::other(error).into()));
            if let Err(error) = result {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

fn response_value(
    id: Value,
    result: Option<Value>,
    error: Option<JsonRpcError>,
) -> Result<Value, RuntimeError> {
    match (result, error) {
        (Some(result), None) => Ok(serde_json::json!({ "id": id, "result": result })),
        (None, Some(error)) => Ok(serde_json::json!({ "id": id, "error": error })),
        (None, None) => Ok(serde_json::json!({ "id": id, "result": null })),
        (Some(_), Some(_)) => Err(RuntimeError::Protocol(
            "JSON-RPC response must contain exactly one of result or error".into(),
        )),
    }
}

/// Daemon test fixtures launch the same fake runtime hundreds of times per
/// test process. A registered fixture's content skips the installation probes
/// and their wall-clock budgets; one daemon test verifies that content, and
/// changed content or an unregistered runtime is verified on every launch.
#[cfg(feature = "test-support")]
pub mod fixture_runtime {
    use super::{verify_runtime, RuntimeError};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static REGISTERED: Mutex<Option<HashSet<Vec<u8>>>> = Mutex::new(None);

    fn content(canonical_bin: &Path) -> Option<Vec<u8>> {
        let mut key = std::fs::read(canonical_bin).ok()?;
        key.push(0);
        key.extend(std::fs::read(canonical_bin.with_file_name("codex-code-mode-host")).ok()?);
        Some(key)
    }

    /// Mark a fake runtime whose verification depends only on its content.
    pub fn register(codex_bin: &Path) {
        let key = std::fs::canonicalize(codex_bin)
            .ok()
            .and_then(|bin| content(&bin))
            .expect("fixture runtime and helper are readable");
        REGISTERED
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(key);
    }

    pub(super) async fn verify(codex_bin: &Path) -> Result<PathBuf, RuntimeError> {
        let canonical_bin = tokio::fs::canonicalize(codex_bin).await?;
        let registered = content(&canonical_bin).is_some_and(|key| {
            REGISTERED
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|registered| registered.contains(&key))
        });
        if registered {
            return Ok(canonical_bin);
        }
        verify_runtime(codex_bin).await
    }
}

/// Offline compatibility check used by the installer before activation.
pub async fn verify_runtime(
    codex_bin: &std::path::Path,
) -> Result<std::path::PathBuf, RuntimeError> {
    timeout(Duration::from_secs(30), verify_runtime_inner(codex_bin))
        .await
        .map_err(|_| RuntimeError::Incompatible("Runtime verification timed out".into()))?
}

async fn verify_runtime_inner(
    codex_bin: &std::path::Path,
) -> Result<std::path::PathBuf, RuntimeError> {
    let canonical_bin = tokio::fs::canonicalize(codex_bin).await?;
    verify_code_mode_host(&canonical_bin).await?;
    let version = Command::new(&canonical_bin)
        .kill_on_drop(true)
        .arg("--version")
        .output()
        .await?;
    if !version.status.success() {
        return Err(RuntimeError::Incompatible("codex --version failed".into()));
    }
    let version = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    if !version.starts_with("codex-cli ") {
        return Err(RuntimeError::Incompatible(format!(
            "unexpected runtime version: {version}"
        )));
    }
    verify_generated_schemas(&canonical_bin, crate::expected_schema_hashes(&version)).await?;

    Ok(canonical_bin)
}

// A successful --help is not evidence that the embedded JavaScript runtime works.
// This local probe has no tools, credentials, network request or model invocation.
async fn verify_code_mode_host(codex_bin: &std::path::Path) -> Result<(), RuntimeError> {
    let helper = codex_bin.with_file_name("codex-code-mode-host");
    timeout(Duration::from_secs(10), probe_code_mode_host(&helper))
        .await
        .map_err(|_| RuntimeError::Incompatible("Code-mode helper timed out. Update or reinstall ChatGPT, then restart Cupola.".into()))?
        .map_err(|error| RuntimeError::Incompatible(format!("Code-mode helper could not execute: {error}. Update or reinstall ChatGPT, then restart Cupola.")))
}

async fn probe_code_mode_host(helper: &std::path::Path) -> Result<(), RuntimeError> {
    let mut child = Command::new(helper)
        .args(["--listen", "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| RuntimeError::Protocol("missing probe input".into()))?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::Protocol("missing probe output".into()))?;
    async fn send(input: &mut ChildStdin, value: Value) -> Result<(), RuntimeError> {
        let bytes = serde_json::to_vec(&value)?;
        input.write_u32_le(bytes.len() as u32).await?;
        input.write_all(&bytes).await?;
        input.flush().await?;
        Ok(())
    }
    async fn receive(output: &mut tokio::process::ChildStdout) -> Result<Value, RuntimeError> {
        let size = output.read_u32_le().await?;
        if size > 1024 * 1024 {
            return Err(RuntimeError::Protocol("oversized probe response".into()));
        }
        let mut bytes = vec![0; size as usize];
        output.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    send(&mut input, serde_json::json!({"type":"connection/hello", "supportedVersions":[1], "requiredCapabilities":[], "optionalCapabilities":[]})).await?;
    if receive(&mut output).await?["type"] != "connection/ready" {
        return Err(RuntimeError::Protocol(
            "unsupported code-mode handshake".into(),
        ));
    }
    send(&mut input, serde_json::json!({"type":"operation/request", "id":1, "request":{"method":"session/open", "sessionId":"wonder-readiness"}})).await?;
    let opened = receive(&mut output).await?;
    if opened["id"] != 1 || opened["result"]["value"]["type"] != "session/ready" {
        return Err(RuntimeError::Protocol(
            "code-mode session unavailable".into(),
        ));
    }
    send(&mut input, serde_json::json!({"type":"operation/request", "id":2, "request":{"method":"session/execute", "sessionId":"wonder-readiness", "request":{
        "tool_call_id":"readiness", "enabled_tools":[], "source":"text(6 * 7)", "yield_time_ms":1000, "max_output_tokens":100
    }}})).await?;
    let mut result = receive(&mut output).await?;
    // The host may acknowledge startup before publishing the initial result.
    if result["type"] == "operation/response"
        && result["id"] == 2
        && result["result"]["status"] == "ok"
        && result["result"]["value"]["type"] == "execution/started"
    {
        result = receive(&mut output).await?;
    }
    if !valid_code_mode_probe_result(&result) {
        return Err(RuntimeError::Protocol("code-mode execution failed".into()));
    }
    child.kill().await?;
    child.wait().await?;
    Ok(())
}

fn valid_code_mode_probe_result(result: &Value) -> bool {
    result["type"] == "execute/initialResponse"
        && result["id"] == 2
        && result["result"]["status"] == "ok"
        && result["result"]["value"]["Result"]["error_text"].is_null()
        && result["result"]["value"]["Result"]["content_items"]
            == serde_json::json!([{"type":"input_text", "text":"42"}])
}

async fn verify_generated_schemas(
    codex_bin: &std::path::Path,
    expected_hashes: Option<(&str, &str)>,
) -> Result<(), RuntimeError> {
    let temporary = tempfile::tempdir().map_err(RuntimeError::Io)?;
    // The native-queue check of this start may already have generated the
    // experimental schema from this same binary.
    let reused = match crate::binary_identity(codex_bin).await {
        Some(identity) => crate::take_generated_experimental(&identity),
        None => None,
    };
    let mut generate = vec![(false, temporary.path().join("stable"))];
    if reused.is_none() {
        generate.push((true, temporary.path().join("experimental")));
    }
    for (experimental, output_dir) in &generate {
        tokio::fs::create_dir_all(output_dir).await?;
        let mut command = Command::new(codex_bin);
        command
            .kill_on_drop(true)
            .args(["app-server", "generate-json-schema"]);
        if *experimental {
            command.arg("--experimental");
        }
        let output = command.arg("--out").arg(output_dir).output().await?;
        if !output.status.success() {
            return Err(RuntimeError::Incompatible(
                "schema generation failed".into(),
            ));
        }
    }
    let schema = "codex_app_server_protocol.v2.schemas.json";
    let stable = tokio::fs::read(temporary.path().join("stable").join(schema)).await?;
    let experimental = match reused {
        Some(bytes) => bytes,
        None => tokio::fs::read(temporary.path().join("experimental").join(schema)).await?,
    };
    let stable_hash = hex::encode(Sha256::digest(&stable));
    let experimental_hash = hex::encode(Sha256::digest(&experimental));
    if let Some(hashes) = expected_hashes {
        if stable_hash == hashes.0 && experimental_hash == hashes.1 {
            crate::record_degraded(codex_bin, &[]);
            return Ok(());
        }
    }
    let degraded = crate::schema_compat::verify_report(&stable, &experimental).map_err(|reason| {
        RuntimeError::Incompatible(format!(
            "Codex changed a required protocol contract ({reason}; stable {stable_hash}, experimental {experimental_hash}); update Cupola"
        ))
    })?;
    let methods: Vec<&'static str> = degraded.iter().map(|entry| entry.method).collect();
    crate::record_degraded(codex_bin, &methods);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in check of the Codex ChatGPT actually ships on this Mac, without
    /// the known-version hash shortcut: `cargo
    /// test -p wonder-app-server installed_chatgpt_codex -- --ignored`.
    /// `WONDER_CODEX_BIN` selects another binary.
    #[tokio::test]
    #[ignore = "needs the ChatGPT app's bundled Codex"]
    async fn installed_chatgpt_codex_passes_the_contract_check() {
        let binary = std::env::var("WONDER_CODEX_BIN").unwrap_or_else(|_| {
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex".into()
        });
        let binary = std::path::Path::new(&binary);
        verify_generated_schemas(binary, None)
            .await
            .expect("installed Codex must pass the contract check without its hash");
        verify_runtime(binary)
            .await
            .expect("installed Codex must pass full runtime verification");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newer_runtime_uses_local_schema_contract_and_rejects_breakage() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research/codex-app-server/0.155.0-alpha.16.3");
        let stable = fixture.join("stable/codex_app_server_protocol.v2.schemas.json");
        let experimental = dir.path().join("experimental.json");
        std::fs::copy(
            fixture.join("experimental/codex_app_server_protocol.v2.schemas.json"),
            &experimental,
        )
        .unwrap();
        let binary = dir.path().join("codex");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 9.9.9'; exit 0; fi\nlast=''; for arg in \"$@\"; do last=\"$arg\"; done\nif [ \"$3\" = --experimental ]; then cp '{}' \"$last/codex_app_server_protocol.v2.schemas.json\"; else cp '{}' \"$last/codex_app_server_protocol.v2.schemas.json\"; fi\n",
                experimental.display(),
                stable.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let helper = dir.path().join("codex-code-mode-host");
        std::fs::write(
            &helper,
            include_str!("../../../tests/fixtures/code-mode-host.py"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(verify_runtime(&binary).await.is_ok());
        let mut changed: Value =
            serde_json::from_slice(&std::fs::read(&experimental).unwrap()).unwrap();
        changed["definitions"]["TurnStartParams"]["properties"]["permissions"]["type"] =
            serde_json::json!("object");
        std::fs::write(&experimental, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(matches!(
            verify_runtime(&binary).await,
            Err(RuntimeError::Incompatible(_))
        ));
    }

    // A start checks the native queue and then verifies the runtime; both read
    // the experimental schema. It is generated once per start, and never
    // reused for a binary that changed in between.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_start_generates_the_experimental_schema_once() {
        use std::os::unix::fs::PermissionsExt;
        let _generated = crate::GENERATED_SCHEMA_TEST.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research/codex-app-server/0.162.0-alpha.17.2");
        let log = dir.path().join("generated");
        let binary = dir.path().join("codex");
        let write = |version: &str| {
            std::fs::write(
                &binary,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\nlast=''; for arg in \"$@\"; do last=\"$arg\"; done\nflavor=stable; if [ \"$3\" = --experimental ]; then flavor=experimental; fi\necho $flavor >> '{}'\ncp '{}/'$flavor'/codex_app_server_protocol.v2.schemas.json' \"$last/codex_app_server_protocol.v2.schemas.json\"\n",
                    log.display(),
                    fixture.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        write("codex-cli 0.163.0");
        let helper = dir.path().join("codex-code-mode-host");
        std::fs::write(
            &helper,
            include_str!("../../../tests/fixtures/code-mode-host.py"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let generated = |flavor: &str| {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == flavor)
                .count()
        };

        crate::supports_native_thread_queue(&binary).await;
        verify_runtime(&binary).await.unwrap();
        assert_eq!((generated("stable"), generated("experimental")), (1, 1));
        // Taken once: a later verification generates its own.
        verify_runtime(&binary).await.unwrap();
        assert_eq!((generated("stable"), generated("experimental")), (2, 2));
        // A binary replaced after the check is verified from its own output.
        crate::supports_native_thread_queue(&binary).await;
        write("codex-cli 0.163.0-alpha.11");
        verify_runtime(&binary).await.unwrap();
        assert_eq!((generated("stable"), generated("experimental")), (3, 4));
    }

    #[tokio::test]
    async fn missing_code_mode_helper_is_incompatible() {
        let dir = tempfile::tempdir().unwrap();
        let result = verify_code_mode_host(&dir.path().join("codex")).await;
        assert!(matches!(result, Err(RuntimeError::Incompatible(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn broken_code_mode_helper_is_incompatible() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("codex-code-mode-host");
        std::fs::write(&helper, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            verify_code_mode_host(&dir.path().join("codex")).await,
            Err(RuntimeError::Incompatible(_))
        ));
        std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
        // Even a binary that exits successfully must fail without the protocol.
        assert!(verify_code_mode_host(&dir.path().join("codex"))
            .await
            .is_err());
    }

    #[test]
    fn probe_requires_its_exact_successful_execution_result() {
        let mut result = serde_json::json!({"type":"execute/initialResponse", "id":2,
            "result":{"status":"ok","value":{"Result":{"content_items":[{"type":"input_text","text":"42"}],"error_text":null}}}});
        assert!(valid_code_mode_probe_result(&result));
        result["id"] = serde_json::json!(42);
        assert!(!valid_code_mode_probe_result(&result));
        result["id"] = serde_json::json!(2);
        result["result"]["value"]["Result"]["error_text"] = serde_json::json!("failed 42");
        assert!(!valid_code_mode_probe_result(&result));
        result["result"]["value"]["Result"]["error_text"] = Value::Null;
        result["result"]["value"]["Result"]["content_items"] = serde_json::json!([]);
        assert!(!valid_code_mode_probe_result(&result));
    }

    #[test]
    fn launch_is_direct_stdio_and_does_not_expose_a_listener() {
        let config = LaunchConfig {
            codex_bin: "/usr/local/bin/codex".into(),
            wonder_version: "0.1.0".into(),
            codex_home: None,
        };
        assert_eq!(config.args(), vec!["app-server", "--stdio"]);
    }

    // The bridge shares durable ingestion, but must not be accepted on a
    // Codex-like handshake alone. Exercise an actual child process and pipes.
    // Unix fixtures (absolute /paths, /bin/sh or python3 fakes, sips); Windows paths are not covered yet.
    #[cfg(unix)]
    #[tokio::test]
    async fn provider_bridge_checks_identity_and_persists_before_broadcast() {
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("bridge.py");
        std::fs::write(&entrypoint, r#"
import json,sys
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        result={'wonderBridge':{'protocolVersion':1,'family':'claude'}}
    elif value.get('method')=='account/read':
        for cursor in range(300):
            print(json.dumps({'method':'thread/started','params':{'thread':{'id':'claude-test'},'cursor':cursor}}),flush=True)
        result={'connected':True}
    else: continue
    print(json.dumps({'id':value['id'],'result':result}),flush=True)
"#).unwrap();
        let saved = Arc::new(Mutex::new(Vec::new()));
        let observed = saved.clone();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let attempted = attempts.clone();
        let paused = Arc::new(tokio::sync::Notify::new());
        let pause = paused.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let resume = release.clone();
        let sink: NotificationSink = Arc::new(move |frame| {
            let observed = observed.clone();
            let attempted = attempted.clone();
            let pause = pause.clone();
            let resume = resume.clone();
            Box::pin(async move {
                let attempt = {
                    let mut attempts = attempted.lock().unwrap();
                    attempts.push(frame.clone());
                    attempts.len()
                };
                if attempt == 1 {
                    return Err("synthetic storage failure".into());
                }
                if attempt == 2 {
                    pause.notify_one();
                    resume.notified().await;
                }
                observed.lock().unwrap().push(frame);
                Ok(())
            })
        });
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint: entrypoint.clone(),
            state_dir: root.path().join("state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let mut client = AppServerClient::spawn_bridge(config.clone(), sink.clone())
            .await
            .unwrap();
        let mut notifications = client.subscribe_notifications();
        let rpc = client.rpc();
        let request =
            tokio::spawn(async move { rpc.request("account/read", serde_json::json!({})).await });
        timeout(Duration::from_secs(2), paused.notified())
            .await
            .unwrap();
        assert!(client.health().is_alive());
        assert!(client.health().storage_blocked());
        assert!(!request.is_finished());
        assert_eq!(client.pending.lock().unwrap().len(), 1);
        assert!(saved.lock().unwrap().is_empty());
        assert!(notifications.try_recv().is_err());
        release.notify_one();
        let result = timeout(Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.result.unwrap()["connected"], true);
        assert!(!client.health().storage_blocked());
        assert_eq!(client.pending.lock().unwrap().len(), 0);
        let frames = saved.lock().unwrap().clone();
        assert_eq!(frames.len(), 300);
        {
            let attempts = attempts.lock().unwrap();
            assert_eq!(attempts.len(), 301);
            assert_eq!(attempts[0], attempts[1]);
        }
        for (cursor, frame) in frames.iter().enumerate() {
            assert_eq!(frame["params"]["cursor"], cursor);
            assert_eq!(frame["_wonderRuntimeId"], client.health().id());
        }
        assert!(matches!(
            notifications.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(44))
        ));
        for expected in &frames[44..] {
            let frame = timeout(Duration::from_secs(2), notifications.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&frame, expected);
        }
        eprintln!("reader storage attempts=301 persisted=300 ordered_cursors=0..299 broadcast_lag=44 pending=0 alive=true");
        client.shutdown().await.unwrap();
        let source = std::fs::read_to_string(&entrypoint)
            .unwrap()
            .replace("'family':'claude'", "'family':'other'");
        std::fs::write(&entrypoint, source).unwrap();
        assert!(AppServerClient::spawn_bridge(config, sink).await.is_err());
    }

    // The real pipe/reader owns all outstanding RPCs, even when the child
    // stays alive after emitting a bad frame. No model or installed runtime.
    #[cfg(unix)]
    async fn reader_fixture(mode: &str, sink: Option<NotificationSink>) -> AppServerClient {
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-u", "-c", r#"
import json,sys
mode=sys.argv[1]
requests=[]
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        print(json.dumps({'id':value['id'],'result':{}}),flush=True)
    elif value.get('method')=='account/read':
        requests.append(value)
        if len(requests)==3:
            print(json.dumps({'method':'fixture/ready','params':{'requests':len(requests)}}),flush=True)
    elif value.get('id')==0:
        if mode=='malformed': print('{"private-runtime-payload": invalid}',flush=True)
        elif mode=='non-object': print('["private-runtime-payload"]',flush=True)
        elif mode=='read-error':
            sys.stdout.buffer.write(b'\xff\n')
            sys.stdout.buffer.flush()
        elif mode=='eof': break
        elif mode=='responses':
            print(json.dumps({'id':999999,'result':{'index':'unknown'}}),flush=True)
            for request in reversed(requests):
                print(json.dumps({'id':request['id'],'result':request['params']}),flush=True)
            print(json.dumps({'method':'fixture/responsesDone','params':{}}),flush=True)
        else: print(json.dumps({'method':'fixture/terminal','params':{}}),flush=True)
"#, mode])
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let (tx, _) = tokio::sync::broadcast::channel(256);
        AppServerClient::spawn_transport(command, "test".into(), tx, sink, None)
            .await
            .unwrap()
    }

    #[cfg(unix)]
    async fn reader_failure_fails_all_requests(mode: &str, expected: &str) {
        let sink: NotificationSink = Arc::new(|frame| {
            Box::pin(async move {
                assert_ne!(frame["method"], "fixture/terminal", "synthetic sink panic");
                Ok(())
            })
        });
        let mut client = reader_fixture(mode, Some(sink)).await;
        let rpc = client.rpc();
        let mut notifications = client.subscribe_notifications();
        let mut requests = Vec::new();
        for index in 0..3 {
            let rpc = rpc.clone();
            requests.push(tokio::spawn(async move {
                rpc.request("account/read", serde_json::json!({"index":index}))
                    .await
            }));
        }
        let ready = timeout(Duration::from_secs(2), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready["params"]["requests"], 3);
        assert_eq!(client.pending.lock().unwrap().len(), 3);
        assert!(client.health().is_alive());
        let started = std::time::Instant::now();
        if mode == "cancel" {
            client.reader_task.abort();
        } else {
            client
                .respond(0, Some(serde_json::json!({})), None)
                .await
                .unwrap();
        }
        let reader_result = timeout(Duration::from_secs(2), &mut client.reader_task)
            .await
            .expect("reader ends promptly");
        match mode {
            "cancel" => assert!(reader_result.unwrap_err().is_cancelled()),
            "panic" => assert!(reader_result.unwrap_err().is_panic()),
            _ => reader_result.unwrap(),
        }
        let pending_at_reader_exit = client.pending.lock().unwrap().len();
        let mut errors = Vec::new();
        for request in requests {
            let error = timeout(Duration::from_secs(2), request)
                .await
                .expect("reader failure promptly ends every pending RPC")
                .unwrap()
                .unwrap_err();
            errors.push(error);
        }
        let elapsed = started.elapsed();
        let pending = client.pending.lock().unwrap().len();
        let alive = client.health().is_alive();
        let child_running = client.child.as_mut().unwrap().try_wait().unwrap().is_none();
        if mode != "eof" {
            assert!(
                child_running,
                "reader termination does not signal the child"
            );
        }
        eprintln!("reader mode={mode} outstanding_before=3 requests={} elapsed_us={} pending_at_reader_exit={pending_at_reader_exit} pending={pending} alive={alive} child_running={child_running} errors={errors:?}", errors.len(), elapsed.as_micros());
        assert!(
            elapsed < Duration::from_secs(2),
            "reader failure stranded callers for {elapsed:?}"
        );
        assert_eq!(pending_at_reader_exit, 0);
        assert_eq!(pending, 0);
        assert!(!alive);
        assert!(
            notifications.try_recv().is_err(),
            "terminal failures are never published"
        );
        for error in errors {
            assert!(matches!(error, RuntimeError::Protocol(_)), "{error}");
            assert!(error.is_transport_failure(), "{error}");
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!error.to_string().contains("private-runtime-payload"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reader_protocol_failure_malformed_json_fails_all_requests() {
        reader_failure_fails_all_requests("malformed", "malformed JSON").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reader_protocol_failure_non_object_fails_all_requests() {
        reader_failure_fails_all_requests("non-object", "non-object").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reader_eof_read_error_cancellation_and_panic_fail_all_requests() {
        for (mode, expected) in [
            ("eof", "App Server EOF"),
            ("read-error", "App Server read error"),
            ("cancel", "response reader stopped"),
            ("panic", "response reader stopped"),
        ] {
            reader_failure_fails_all_requests(mode, expected).await;
        }
    }

    // Dropping one caller removes only its entry; late/unknown replies do not
    // A Claude-family bridge fixture: answers the handshake, then runs `body`
    // for every later frame. No model or installed runtime.
    #[cfg(unix)]
    async fn bridge_fixture(body: &str) -> AppServerClient {
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-u", "-c", &format!(r#"
import json,sys
held=[]
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        print(json.dumps({{'id':value['id'],'result':{{'wonderBridge':{{'protocolVersion':1,'family':'claude'}}}}}}),flush=True)
        continue
{body}
"#)])
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let (tx, _) = tokio::sync::broadcast::channel(256);
        AppServerClient::spawn_transport(command, "test".into(), tx, None, Some("claude"))
            .await
            .unwrap()
    }

    // The host keeps why a bridge stopped: a safe classified cause in the
    // error and on health, and only a bounded, redacted stderr tail, which
    // optimized Release builds compile out.
    #[cfg(all(unix, debug_assertions))]
    #[tokio::test]
    async fn bridge_exit_keeps_a_classified_cause_and_redacted_bounded_stderr() {
        let client = bridge_fixture(
            r#"    if value.get('method')=='account/read':
        for i in range(2000): sys.stderr.write('filler line %d with some detail\n' % i)
        sys.stderr.write('Authorization: Bearer sk-private-value\n')
        sys.stderr.write('Authorization: Bearer ' + 'x' * 5000 + 'long-line-hidden-value\n')
        sys.stderr.write('request failed with sk-ant-api03-bare-value-1234\n')
        sys.stderr.write('retry with QWxhZGRpbjpvcGVuIHNlc2FtZQ9876543210xyz\n')
        sys.stderr.write('Claude runtime stopped: storage-or-transport\n')
        sys.stderr.flush()
        sys.exit(1)"#,
        )
        .await;
        let error = client
            .rpc()
            .request("account/read", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(error.is_transport_failure(), "{error}");
        assert!(
            error
                .to_string()
                .contains("runtime stopped: storage-or-transport"),
            "{error}"
        );
        let health = client.health();
        assert_eq!(health.exit_cause(), Some("storage-or-transport"));
        let tail = health.diagnostic_stderr();
        assert!(tail.len() <= STDERR_TAIL_BYTES, "{}", tail.len());
        assert!(tail.len() > STDERR_TAIL_BYTES / 2, "{}", tail.len());
        assert!(tail.ends_with("Claude runtime stopped: storage-or-transport"));
        assert!(tail.contains("[redacted]"));
        for secret in [
            "sk-private-value",
            "long-line-hidden-value",
            "bare-value",
            "9876543210xyz",
        ] {
            assert!(!tail.contains(secret), "{secret} leaked: {tail}");
        }
        assert!(tail.contains("filler line 1999 with some detail"));
        assert!(!error.to_string().contains("filler"));
    }

    #[test]
    fn stderr_classification_trusts_only_known_runtime_causes() {
        let mut exit = ExitDiagnostics::default();
        exit.record(b"Claude runtime stopped: something-invented\n", true);
        assert_eq!(exit.cause, Some("unexpected-error"));
        exit.record(
            b"FATAL ERROR: Reached heap limit Allocation failed - JavaScript heap out of memory\n",
            true,
        );
        assert_eq!(
            exit.cause,
            Some("out-of-memory"),
            "a specific cause replaces the generic one"
        );
        exit.record(b"    at later stack frame\n", true);
        assert_eq!(exit.cause, Some("out-of-memory"));
        let mut exit = ExitDiagnostics::default();
        exit.record(b"codex: ordinary log line\n", false);
        assert_eq!(exit.cause, None, "Codex stderr alone is not a failure");
        exit.record(b"FATAL ERROR: JavaScript heap out of memory\n", false);
        assert_eq!(exit.cause, Some("out-of-memory"));
        exit.record(b"Claude runtime stopped: frame-too-large\n", false);
        assert_eq!(exit.cause, Some("frame-too-large"));
    }

    // A burst beyond the bridge's in-flight bound waits for a slot instead
    // of reaching the runtime, and every caller still gets its own reply.
    #[cfg(unix)]
    #[tokio::test]
    async fn bridge_request_burst_waits_for_slots_and_all_complete() {
        let client = bridge_fixture(&format!(
            r#"    if value.get('method')=='account/read' and held is not None:
        held.append(value)
        if len(held)=={BRIDGE_MAX_IN_FLIGHT}:
            print(json.dumps({{'method':'fixture/full','params':{{}}}}),flush=True)
    elif value.get('method')=='account/read':
        print(json.dumps({{'id':value['id'],'result':value['params']}}),flush=True)
    elif value.get('id')==0:
        print(json.dumps({{'method':'fixture/count','params':{{'held':len(held)}}}}),flush=True)
        for request in held: print(json.dumps({{'id':request['id'],'result':request['params']}}),flush=True)
        held=None"#
        ))
        .await;
        let mut notifications = client.subscribe_notifications();
        let rpc = client.rpc();
        let total = BRIDGE_MAX_IN_FLIGHT + 8;
        let mut requests = Vec::new();
        for index in 0..total {
            let rpc = rpc.clone();
            requests.push(tokio::spawn(async move {
                rpc.request("account/read", serde_json::json!({ "index": index }))
                    .await
            }));
        }
        let full = timeout(Duration::from_secs(10), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(full["method"], "fixture/full");
        // An approval or other reply is not new work and never waits.
        client
            .respond(0, Some(serde_json::json!({})), None)
            .await
            .unwrap();
        let count = timeout(Duration::from_secs(10), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count["params"]["held"], BRIDGE_MAX_IN_FLIGHT);
        for (index, request) in requests.into_iter().enumerate() {
            let response = timeout(Duration::from_secs(10), request)
                .await
                .expect("queued request completes")
                .unwrap()
                .unwrap();
            assert_eq!(response.result.unwrap()["index"], index);
        }
    }

    // turn into notifications or steal another outstanding caller's result.
    #[cfg(unix)]
    #[tokio::test]
    async fn reader_matches_responses_after_individual_request_cancellation() {
        let mut client = reader_fixture("responses", None).await;
        let mut notifications = client.subscribe_notifications();
        let mut requests = Vec::new();
        for index in 0..3 {
            let rpc = client.rpc();
            requests.push(tokio::spawn(async move {
                rpc.request("account/read", serde_json::json!({"index":index}))
                    .await
            }));
        }
        timeout(Duration::from_secs(2), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(client.pending.lock().unwrap().len(), 3);
        let cancelled = requests.remove(0);
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        assert_eq!(client.pending.lock().unwrap().len(), 2);
        client
            .respond(0, Some(serde_json::json!({})), None)
            .await
            .unwrap();
        for (index, request) in (1..3).zip(requests) {
            let response = timeout(Duration::from_secs(2), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(response.result.unwrap()["index"], index);
        }
        let done = timeout(Duration::from_secs(2), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done["method"], "fixture/responsesDone");
        assert_eq!(client.pending.lock().unwrap().len(), 0);
        assert!(client.health().is_alive());
        assert!(notifications.try_recv().is_err());
        eprintln!("reader responses submitted=3 cancelled=1 pending_after_cancel=2 matched=2 late=1 unknown=1 pending=0 alive=true");
        client.shutdown().await.unwrap();
    }

    // Contract: Claude's bridge sees `CLAUDE_CONFIG_DIR` only for an added
    // account Wonder configured; the owner's own setup never inherits one.
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_bridge_gets_a_config_folder_only_for_an_added_account() {
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("bridge.py");
        std::fs::write(
            &entrypoint,
            r#"
import json,os,sys
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        result={'wonderBridge':{'protocolVersion':1,'family':'claude'}}
    elif value.get('method')=='account/read':
        result={'configDir':os.environ.get('CLAUDE_CONFIG_DIR')}
    else: continue
    print(json.dumps({'id':value['id'],'result':result}),flush=True)
"#,
        )
        .unwrap();
        let sink: NotificationSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let mut config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: root.path().join("state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let account = root.path().join("account");
        for (configured, expected) in [
            (None, serde_json::Value::Null),
            (Some(account.clone()), serde_json::json!(account)),
        ] {
            config.claude_config_dir = configured;
            let mut client = AppServerClient::spawn_bridge(config.clone(), sink.clone())
                .await
                .unwrap();
            let read = client
                .request("account/read", serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(read.result.unwrap()["configDir"], expected);
            client.shutdown().await.unwrap();
        }
    }

    // A runtime that stops reading stdin must not hold a request, or the
    // shutdown that would end it, behind a pipe that never drains.
    // Unix fixtures (absolute /paths, /bin/sh or python3 fakes, sips); Windows paths are not covered yet.
    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_runtime_stdin_fails_the_request_and_shutdown_still_ends_it() {
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("bridge.py");
        std::fs::write(&entrypoint, r#"
import json,sys,time
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        print(json.dumps({'id':value['id'],'result':{'wonderBridge':{'protocolVersion':1,'family':'claude'}}}),flush=True)
        time.sleep(3600)
"#).unwrap();
        let sink: NotificationSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: root.path().join("state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let mut client = AppServerClient::spawn_bridge(config, sink).await.unwrap();
        let rpc = client.rpc();
        // Larger than any pipe buffer: the write blocks once the child stops reading.
        let large = serde_json::json!({ "padding": "x".repeat(4 << 20) });
        let started = std::time::Instant::now();
        let blocked = tokio::spawn({
            let rpc = rpc.clone();
            async move { rpc.request("account/read", large).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        timeout(Duration::from_millis(900), client.shutdown())
            .await
            .expect("shutdown kills the runtime without waiting for its stdin")
            .unwrap();
        let result = timeout(Duration::from_secs(5), blocked)
            .await
            .expect("the blocked request ends")
            .unwrap();
        assert!(result.is_err());
        assert!(started.elapsed() < APP_SERVER_REQUEST_TIMEOUT);
        assert!(!rpc.health().is_alive());

        // Without a shutdown, the write itself is bounded and ends the generation.
        let entrypoint = root.path().join("bridge.py");
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: root.path().join("state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client =
            AppServerClient::spawn_bridge(config, Arc::new(|_| Box::pin(async { Ok(()) })))
                .await
                .unwrap();
        let rpc = client.rpc();
        let large = serde_json::json!({ "padding": "x".repeat(4 << 20) });
        let error = timeout(STDIN_WRITE_TIMEOUT * 3, rpc.request("account/read", large))
            .await
            .expect("the stalled write is bounded")
            .unwrap_err();
        assert!(error.is_transport_failure(), "{error}");
        assert!(!rpc.health().is_alive());
    }

    #[test]
    fn control_plane_environment_is_not_forwarded_to_app_server() {
        for variable in [
            "TS_AUTHKEY",
            "WONDER_LOOPBACK_CAPABILITY",
            "WONDER_PUBLIC_ORIGIN",
            "WONDER_PUBLIC_ORIGIN_FILE",
            "WONDER_COMPUTER_USE_BIN",
        ] {
            assert!(
                CONTROL_PLANE_ENV_VARS.contains(&variable),
                "missing scrub for {variable}"
            );
        }
    }

    // Contract: the shared ACP entrypoint receives Cursor's explicit selector,
    // and a bridge announcing another family cannot enter its runtime route.
    #[cfg(unix)]
    #[tokio::test]
    async fn cursor_launch_selects_and_verifies_its_provider() {
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("bridge.py");
        std::fs::write(&entrypoint, r#"
import json,sys
family=sys.argv[sys.argv.index('--provider')+1]
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize': result={'wonderBridge':{'protocolVersion':1,'family':family}}
    elif value.get('method')=='account/read': result={'family':family,'args':sys.argv[1:]}
    else: continue
    print(json.dumps({'id':value['id'],'result':result}),flush=True)
"#).unwrap();
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: root.path().join("cursor-runtime"),
            npm_cli: None,
            claude_config_dir: Some(root.path().join("must-not-inherit")),
            wonder_version: "test".into(),
            family: "cursor",
        };
        let mut client =
            AppServerClient::spawn_bridge(config.clone(), Arc::new(|_| Box::pin(async { Ok(()) })))
                .await
                .unwrap();
        let result = client
            .request("account/read", serde_json::json!({}))
            .await
            .unwrap()
            .result
            .unwrap();
        assert_eq!(result["family"], "cursor");
        assert_eq!(
            result["args"],
            serde_json::json!(["--state-dir", config.state_dir, "--provider", "cursor"])
        );
        client.shutdown().await.unwrap();
    }

    // Exercise Windows CreateProcess with spaces in the native executable path,
    // plus the case-preserving environment a real Node/SDK child receives.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bridge_spawn_preserves_lookup_and_home_without_host_secrets() {
        let root = tempfile::Builder::new()
            .prefix("Cupola spawn path ")
            .tempdir()
            .unwrap();
        let binary = root.path().join("runtime with spaces.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        let mut command = Command::new(std::fs::canonicalize(binary).unwrap());
        let mut variables: Vec<_> = std::env::vars_os().collect();
        variables.retain(|(name, _)| !name.to_string_lossy().eq_ignore_ascii_case("PATH"));
        variables.extend([
            (
                "Path".into(),
                r"C:\Program Files\nodejs;C:\Users\Test User\AppData\Roaming\npm".into(),
            ),
            ("USERPROFILE".into(), r"C:\Users\Test User".into()),
            (
                "WONDER_LOOPBACK_CAPABILITY".into(),
                "must-not-inherit".into(),
            ),
            ("NODE_OPTIONS".into(), "--require must-not-inherit".into()),
        ]);
        bridge_environment(&mut command, "claude", variables);
        let output = timeout(
            Duration::from_secs(20),
            command
                .env("CUPOLA_ENV_FIXTURE", "1")
                .args([
                    "--exact",
                    "process::tests::windows_bridge_environment_child",
                    "--nocapture",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("WINDOWS_SPAWN_OK"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_bridge_environment_child() {
        if std::env::var("CUPOLA_ENV_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        assert_eq!(
            std::env::var("PATH").unwrap(),
            r"C:\Program Files\nodejs;C:\Users\Test User\AppData\Roaming\npm"
        );
        assert_eq!(std::env::var("USERPROFILE").unwrap(), r"C:\Users\Test User");
        for name in [
            "SystemRoot",
            "ComSpec",
            "PATHEXT",
            "TEMP",
            "APPDATA",
            "LOCALAPPDATA",
        ] {
            assert!(std::env::var_os(name).is_some(), "missing {name}");
        }
        for name in ["WONDER_LOOPBACK_CAPABILITY", "NODE_OPTIONS"] {
            assert!(std::env::var_os(name).is_none(), "inherited {name}");
        }
        println!("WINDOWS_SPAWN_OK");
    }

    // Native settings survive the real child environment, while unrelated
    // credentials and Wonder's authority never reach the provider bridge.
    // Unix fixtures (absolute /paths, /bin/sh or python3 fakes, sips); Windows paths are not covered yet.
    #[cfg(unix)]
    #[tokio::test]
    async fn opencode_bridge_keeps_native_config_without_inheriting_secrets() {
        for family in ["opencode", "cursor", "claude"] {
            let mut command = Command::new("/usr/bin/python3");
            bridge_environment(
                &mut command,
                family,
                [
                    ("HOME".into(), "/fixture".into()),
                    ("WONDER_OPENCODE_BIN".into(), "/fixture/opencode".into()),
                    ("WONDER_CURSOR_BIN".into(), "/fixture/cursor".into()),
                    ("CLAUDE_CONFIG_DIR".into(), "/fixture/claude".into()),
                    ("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:1234".into()),
                    ("CUPOLA_PROVIDER_ENV".into(), "CUSTOM_API_KEY".into()),
                    ("CUSTOM_API_KEY".into(), "fixture-key".into()),
                    ("OPENCODE_PERMISSION".into(), "{\"bash\":\"deny\"}".into()),
                    ("OPENCODE_CONFIG".into(), "/fixture/opencode.json".into()),
                    ("XDG_CONFIG_HOME".into(), "/fixture/config".into()),
                    (
                        "WONDER_LOOPBACK_CAPABILITY".into(),
                        "private-fixture".into(),
                    ),
                    ("UNRELATED_API_KEY".into(), "private-fixture".into()),
                ],
            );
            let output = command
                .args(["-c", "import json,os;print(json.dumps(dict(os.environ)))"])
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            let env: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(env["HOME"], "/fixture");
            assert_eq!(env["CLAUDE_CONFIG_DIR"], "/fixture/claude");
            assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:1234");
            assert_eq!(env["CUSTOM_API_KEY"], "fixture-key");
            assert!(env["WONDER_LOOPBACK_CAPABILITY"].is_null());
            assert!(env["UNRELATED_API_KEY"].is_null());
            assert_eq!(
                env["WONDER_CURSOR_BIN"].as_str(),
                (family == "cursor").then_some("/fixture/cursor")
            );
            assert_eq!(
                env["WONDER_OPENCODE_BIN"].as_str(),
                (family == "opencode").then_some("/fixture/opencode")
            );
            if family == "opencode" {
                assert_eq!(env["OPENCODE_PERMISSION"], "{\"bash\":\"deny\"}");
                assert_eq!(env["OPENCODE_CONFIG"], "/fixture/opencode.json");
                assert_eq!(env["XDG_CONFIG_HOME"], "/fixture/config");
            } else {
                assert!(env["OPENCODE_PERMISSION"].is_null());
                assert!(env["OPENCODE_CONFIG"].is_null());
                assert!(env["XDG_CONFIG_HOME"].is_null());
            }
        }
    }

    // Contract: provider cleanup gets its grace, but forced shutdown, Drop and
    // bridge crashes stop the owned native child without stopping another session.
    // Stopping the host waits for every bridge to save, within one deadline:
    // three that each take 2 s finish together, not one after another.
    #[cfg(unix)]
    #[tokio::test]
    async fn bridges_stop_together_and_each_finishes_saving() {
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("bridge.py");
        std::fs::write(
            &entrypoint,
            r#"
import json,os,sys,time
state=sys.argv[sys.argv.index("--state-dir")+1]
family=os.path.basename(os.path.dirname(state))
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        print(json.dumps({'id':value['id'],'result':{'wonderBridge':{'protocolVersion':1,'family':family}}}),flush=True)
time.sleep(2)
with open(os.path.join(state,'saved'),'w') as receipt: receipt.write('saved')
"#,
        )
        .unwrap();
        let mut clients = Vec::new();
        let mut states = Vec::new();
        for name in ["first", "second", "third"] {
            let state_dir = root.path().join("opencode").join(name);
            std::fs::create_dir_all(&state_dir).unwrap();
            let config = BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint: entrypoint.clone(),
                state_dir: state_dir.clone(),
                npm_cli: None,
                claude_config_dir: None,
                wonder_version: "test".into(),
                family: "opencode",
            };
            let client =
                AppServerClient::spawn_bridge(config, Arc::new(|_| Box::pin(async { Ok(()) })))
                    .await
                    .unwrap();
            clients.push(Arc::new(AsyncMutex::new(client)));
            states.push(state_dir);
        }
        let started = std::time::Instant::now();
        timeout(
            Duration::from_secs(60),
            AppServerClient::shutdown_together(clients),
        )
        .await
        .unwrap()
        .unwrap();
        let took = started.elapsed();
        for state in states {
            assert_eq!(
                std::fs::read_to_string(state.join("saved")).unwrap(),
                "saved"
            );
        }
        // One after another takes at least 6 s.
        assert!(took < Duration::from_secs(5), "stopping took {took:?}");
    }

    // The child outlives the old two-second grace; its receipt proves cleanup ran.
    #[cfg(unix)]
    #[tokio::test]
    async fn provider_shutdown_and_drop_stop_the_owned_native_child() {
        use std::io::Read;
        use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};

        // Each fixture descendant holds the write end of `exited`, so EOF on the
        // read end means all of them have exited (a zombie has closed its fds).
        // SIGKILL ends a process asynchronously and a loaded host can keep it
        // running for seconds, so wait for that exit rather than a wall clock.
        fn exit_fence(state_dir: &std::path::Path) -> std::fs::File {
            let path = state_dir.join("exited");
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            // Open before the fixture starts: its writers block until a reader exists.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .unwrap()
        }
        // The hang guard only bounds a leak: the fixture keeps running for 60 s
        // while its marker exists, so a leaked descendant still fails here.
        // A fixture without descendants never opens a writer, so it has no fence.
        async fn stopped(fence: Option<&mut std::fs::File>) -> bool {
            let Some(fence) = fence else { return true };
            timeout(Duration::from_secs(30), async {
                let mut byte = [0; 1];
                loop {
                    // macOS kqueue does not wake Tokio's FIFO receiver for this
                    // empty FIFO's EOF. Read directly without blocking instead;
                    // both writers opened before native.json was published.
                    match fence.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("fixture exit fence: {error}"),
                    }
                }
            })
            .await
            .is_ok()
        }

        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("bridge.py");
        std::fs::write(
            &entrypoint,
            r#"
import json,os,subprocess,sys,time
state=sys.argv[sys.argv.index("--state-dir")+1]
family=os.path.basename(os.path.dirname(state))
# The private marker and deadline let even a failed test clean up without
# signalling a descendant by a PID whose original owner may have exited.
peer_code='''
import json,os,subprocess,sys,time
marker=sys.argv[1]
fence=open(os.path.join(os.path.dirname(marker),'exited'),'w')
grandchild=subprocess.Popen([sys.executable,'-c',"import os,sys,time; fence=open(os.path.join(os.path.dirname(sys.argv[1]),'exited'),'w'); open(sys.argv[1]+'.ready','w').close(); deadline=time.monotonic()+60\\nwhile os.path.exists(sys.argv[1]) and time.monotonic()<deadline: time.sleep(0.01)",marker])
ready_deadline=time.monotonic()+30
while not os.path.exists(marker+'.ready'):
    if time.monotonic()>ready_deadline: raise RuntimeError('grandchild did not start')
    time.sleep(0.01)
with open(os.path.join(os.path.dirname(marker),'native.json.tmp'),'w') as identity:
    json.dump({'peer':os.getpid(),'peerGroup':os.getpgrp(),'grandchild':grandchild.pid,'grandchildGroup':os.getpgid(grandchild.pid)},identity)
os.replace(os.path.join(os.path.dirname(marker),'native.json.tmp'),os.path.join(os.path.dirname(marker),'native.json'))
deadline=time.monotonic()+60
while os.path.exists(marker) and time.monotonic()<deadline:
    time.sleep(0.01)
grandchild.wait(timeout=3)
'''
peer=None if state.endswith('crash-empty') else subprocess.Popen([sys.executable,'-c',peer_code,os.path.join(state,'alive')],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
if peer:
    deadline=time.monotonic()+30
    while not os.path.exists(os.path.join(state,'native.json')):
        if time.monotonic()>deadline: raise RuntimeError('native child did not start')
        time.sleep(0.01)
    with open(os.path.join(state,'native.json')) as identity: native=json.load(identity)
else: native={'peer':None,'peerGroup':None,'grandchild':None,'grandchildGroup':None}
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='initialize':
        result={'wonderBridge':{'protocolVersion':1,'family':family}}
    elif value.get('method')=='account/read':
        result={'bridge':os.getpid(),'group':os.getpgrp(),**native}
    else: continue
    print(json.dumps({'id':value['id'],'result':result}),flush=True)
time.sleep(2.2)
if state.endswith('forced'): time.sleep(3600)
os.unlink(os.path.join(state,'alive'))
peer.wait()
with open(os.path.join(state,'cleaned'),'w') as receipt: receipt.write('graceful')
"#,
        )
        .unwrap();
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: root.path().join("state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "opencode",
        };
        let mut unrelated = Command::new("/usr/bin/python3")
            .args(["-c", "import time; time.sleep(3600)"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut failures = Vec::new();
        // One family: stop_child takes the same Unix path for opencode, cursor
        // and claude (the families differ only in launch arguments).
        for family in ["opencode"] {
            let mut config = config.clone();
            config.family = family;
            config.state_dir = root.path().join(family).join("unrelated");
            std::fs::create_dir_all(&config.state_dir).unwrap();
            let other_marker = config.state_dir.join("alive");
            std::fs::write(&other_marker, "test-owned").unwrap();
            let mut other_fence = exit_fence(&config.state_dir);
            let mut other_session = AppServerClient::spawn_bridge(
                config.clone(),
                Arc::new(|_| Box::pin(async { Ok(()) })),
            )
            .await
            .unwrap();
            let other_identity = other_session
                .request("account/read", serde_json::json!({}))
                .await
                .unwrap()
                .result
                .unwrap();
            for mode in ["graceful", "drop", "forced", "crash", "crash-empty"] {
                let mut config = config.clone();
                config.state_dir = root.path().join(family).join(mode);
                std::fs::create_dir_all(&config.state_dir).unwrap();
                let marker = config.state_dir.join("alive");
                let receipt = config.state_dir.join("cleaned");
                std::fs::write(&marker, "test-owned").unwrap();
                let mut fence = (mode != "crash-empty").then(|| exit_fence(&config.state_dir));
                let mut client =
                    AppServerClient::spawn_bridge(config, Arc::new(|_| Box::pin(async { Ok(()) })))
                        .await
                        .unwrap();
                let identity = client
                    .request("account/read", serde_json::json!({}))
                    .await
                    .unwrap()
                    .result
                    .unwrap();
                let pid = identity["peer"].as_i64().map(|id| id as i32);
                let grandchild = identity["grandchild"].as_i64().map(|id| id as i32);
                eprintln!(
                    "lifecycle family={family} mode={mode} owner={} owner_group={} identity={identity} other_session={other_identity} unrelated={:?}",
                    std::process::id(), unsafe { libc::getpgrp() }, unrelated.id(),
                );
                if identity["group"] != identity["bridge"]
                    || (pid.is_some() && identity["peerGroup"] != identity["group"])
                    || (grandchild.is_some() && identity["grandchildGroup"] != identity["group"])
                {
                    failures.push(format!(
                        "{family}/{mode}: expected one private process group"
                    ));
                }
                if mode.starts_with("crash") {
                    let child = client.child.as_ref().unwrap();
                    signal_owned_child(child, libc::SIGKILL, false).unwrap();
                    timeout(Duration::from_secs(1), wait_unreaped(child))
                        .await
                        .unwrap()
                        .unwrap();
                    #[cfg(target_os = "macos")]
                    if only_group_leader_remains(child) != (mode == "crash-empty") {
                        failures.push(format!("{family}/{mode}: unexpected group members"));
                    }
                }
                let started = std::time::Instant::now();
                if mode != "drop" {
                    timeout(Duration::from_secs(14), client.shutdown())
                        .await
                        .expect("provider shutdown is bounded")
                        .unwrap();
                }
                drop(client);
                let native_stopped = stopped(fence.as_mut()).await;
                let cleaned = receipt.exists();
                // Remove only this fixture's marker, even when production leaked
                // the child. TempDir also removes every marker during unwinding.
                if marker.exists() {
                    std::fs::remove_file(marker).unwrap();
                }
                assert!(
                    stopped(fence.as_mut()).await,
                    "test-owned marker cleanup failed"
                );
                eprintln!(
                    "lifecycle family={family} mode={mode} stopped={native_stopped} graceful_receipt={cleaned} cleanup_confirmed=true elapsed_ms={}",
                    started.elapsed().as_millis(),
                );
                if !native_stopped || (mode == "graceful" && !cleaned) {
                    failures.push(format!(
                        "{family}/{mode}: stopped={native_stopped} graceful_receipt={cleaned}"
                    ));
                }
                assert!(
                    unrelated.try_wait().unwrap().is_none(),
                    "A separate process must survive"
                );
                let still_running = other_session
                    .request("account/read", serde_json::json!({}))
                    .await
                    .unwrap()
                    .result
                    .unwrap();
                assert_eq!(
                    still_running, other_identity,
                    "another session must survive"
                );
                let other_pids = format!(
                    "{},{}",
                    other_identity["peer"], other_identity["grandchild"]
                );
                let status = Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &other_pids])
                    .output()
                    .await
                    .unwrap();
                let states = String::from_utf8_lossy(&status.stdout);
                assert!(status.status.success());
                assert_eq!(
                    states.lines().count(),
                    2,
                    "both unrelated descendants survive"
                );
                assert!(states.lines().all(|line| !line.trim().starts_with('Z')));
            }
            timeout(Duration::from_secs(14), other_session.shutdown())
                .await
                .unwrap()
                .unwrap();
            drop(other_session);
            if other_marker.exists() {
                std::fs::remove_file(other_marker).unwrap();
            }
            assert!(
                stopped(Some(&mut other_fence)).await,
                "unrelated session fixture cleanup failed"
            );
        }
        unrelated.kill().await.unwrap();
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[test]
    fn json_rpc_response_contains_only_result_or_error() {
        let success = response_value(
            serde_json::json!(7),
            Some(serde_json::json!({"ok": true})),
            None,
        )
        .expect("success");
        assert_eq!(
            success,
            serde_json::json!({"id": 7, "result": {"ok": true}})
        );
        let failure = response_value(
            serde_json::json!(7),
            None,
            Some(JsonRpcError {
                code: -1,
                message: "no".into(),
                data: None,
            }),
        )
        .expect("failure");
        assert_eq!(
            failure,
            serde_json::json!({"id": 7, "error": {"code": -1, "message": "no", "data": null}})
        );
        assert!(response_value(
            serde_json::json!(7),
            Some(serde_json::json!(null)),
            Some(JsonRpcError {
                code: -1,
                message: "no".into(),
                data: None
            })
        )
        .is_err());
    }
}
