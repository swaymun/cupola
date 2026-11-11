//! Sends a phone message into a Codex thread that ChatGPT's desktop app is
//! running, through the app's own local IPC router (`~/.codex/ipc/ipc.sock`),
//! so the app is neither stopped nor queued behind. Measured against ChatGPT
//! 26.1013 (plans/desktop-owner-relay.md): frames are a little-endian u32
//! length and JSON; a client registers with `initialize`, asks which client
//! owns the thread (`thread-owner-discovery`) and asks that owner to start a
//! turn (`thread-follower-start-turn`) or add to the running one
//! (`thread-follower-steer-turn`). The owner runs it with the thread's own
//! settings, and approvals for it appear in ChatGPT. Stop asks the owner to
//! interrupt that exact turn (`thread-follower-interrupt-turn`).
//!
//! The protocol is private and each method is versioned. The router answers
//! `no-client-found` when the owner does not speak a request's version, so a
//! refusal from the owner discovery just named is taken as a version change:
//! the relay stays off for that build of the app for a while and sends go the
//! usual way (wait for the reply there, or Codex's own queue).
//!
//! A client can also follow a thread ChatGPT runs (`Session::follow`): the
//! owner sends it the thread's state (`thread-stream-state-changed`, a
//! snapshot, then immer patches, each numbered), including the requests the
//! turn waits on, and takes an answer to one of them from any client
//! (`Session::decide`) as if it were clicked there. Measured against ChatGPT
//! 26.1016.51333 (crates/wonderd/tests/fixtures/chatgpt-follower-approvals.jsonl);
//! `chatgpt_approvals` puts those requests on the phone.
//!
//! Only a router that is ChatGPT itself is used: same user, an owner-only
//! socket, and a peer process signed by OpenAI as ChatGPT. Other systems have
//! no ChatGPT desktop app, so it never applies there.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(not(unix))]
type UnixStream = tokio::io::DuplexStream;

const DISCOVERY: (&str, u64) = ("thread-owner-discovery", 1);
const START_TURN: (&str, u64) = ("thread-follower-start-turn", 2);
const STEER_TURN: (&str, u64) = ("thread-follower-steer-turn", 1);
/// Version 4 carries `expectedTurnId`: the owner interrupts only that turn.
const INTERRUPT_TURN: (&str, u64) = ("thread-follower-interrupt-turn", 4);
/// Broadcasts: following a thread, and the owner's state of it. A broadcast
/// in another version is dropped by its receiver, so a change shows as
/// silence, not an error.
pub(crate) const FOLLOWING: (&str, u64) = ("thread-stream-following-changed", 1);
pub(crate) const STATE_CHANGED: (&str, u64) = ("thread-stream-state-changed", 11);
/// The owner asks who follows (after it took the thread over).
pub(crate) const FOLLOWING_REQUESTED: (&str, u64) = ("thread-stream-following-status-requested", 1);
/// Answers to a request the owner's turn waits on, by the request's method.
const COMMAND_DECISION: (&str, u64) = ("thread-follower-command-approval-decision", 1);
const FILE_DECISION: (&str, u64) = ("thread-follower-file-approval-decision", 1);
const PERMISSIONS_RESPONSE: (&str, u64) =
    ("thread-follower-permissions-request-approval-response", 1);
const USER_INPUT_RESPONSE: (&str, u64) = ("thread-follower-submit-user-input", 1);

/// The router waits up to 10 s for each client to say whether it owns a
/// thread; an owner answers in milliseconds (measured 2 ms).
const DISCOVERY_WAIT: Duration = Duration::from_secs(3);
const CONNECT_WAIT: Duration = Duration::from_secs(2);
/// A start returned in 0.23 s (measured); the owner may first load the thread.
const SEND_WAIT: Duration = Duration::from_secs(20);
/// A followed thread's snapshot carries its whole state (measured 9-14 kB
/// for a short thread); ChatGPT's own limit is 256 MB.
const MAX_FRAME: usize = 64 * 1024 * 1024;
/// After an attempt that sent nothing, sends to the thread go the usual way
/// for this long before the relay is tried again.
const REFUSAL_COOLDOWN: Duration = Duration::from_secs(60);
/// A build that refused our method versions is not tried again for this
/// long, so one mistaken refusal does not end the relay until a restart.
const RETIRED_FOR: Duration = Duration::from_secs(60 * 60);

/// ChatGPT's main process, which hosts the router: OpenAI's Developer ID and
/// the app's own identifier (measured with `codesign --verify -R` on the
/// router's pid).
#[cfg(target_os = "macos")]
const CHATGPT_REQUIREMENT: &str = "identifier \"com.openai.codex\" and anchor apple generic and certificate leaf[subject.OU] = \"2DC432GLL2\"";

/// Why nothing was delivered: the message may safely go another way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotSent {
    /// No router, or it did not answer.
    Unavailable,
    /// The router is not ChatGPT's (another app runs it, or its signature
    /// or socket could not be verified).
    Untrusted,
    /// ChatGPT does not have the thread open.
    NoOwner,
    /// ChatGPT no longer speaks these method versions.
    Version,
    /// The turn being added to ended first.
    TurnEnded,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// ChatGPT started this turn (or added the message to it).
    Delivered {
        turn: String,
        steered: bool,
    },
    NotSent(NotSent),
    /// The request reached ChatGPT but its answer was lost: it may have run.
    Unknown(String),
}

/// What became of an answer sent to the owner (`Session::decide`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decided {
    /// The owner took it.
    Sent,
    /// Certainly not delivered: it may be sent again.
    NotSent(NotSent),
    /// Written, but its acknowledgement was lost or was an error.
    Unknown(String),
}

/// The app build behind a router, so a refused version retires only it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AppBuild {
    executable: String,
    modified: Option<std::time::SystemTime>,
}

impl AppBuild {
    fn of(executable: &str) -> Self {
        Self {
            executable: executable.to_owned(),
            modified: std::fs::metadata(executable)
                .and_then(|m| m.modified())
                .ok(),
        }
    }
}

/// What ChatGPT did with Stop for a relayed turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The owner interrupted that turn; it ends there a moment later.
    Stopping,
    /// That turn is not running there any more.
    NotRunning,
    NotSent(NotSent),
    /// The request reached ChatGPT but its answer was lost or was an error.
    Unknown(String),
}

/// A retired build's modification time, and when it refused.
type Retirement = (Option<std::time::SystemTime>, Instant);
/// Builds that refused our method versions, by executable; an update (a new
/// modification time) is a new build.
static RETIRED: LazyLock<Mutex<HashMap<String, Retirement>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whether `build` refused our versions recently.
fn retired(build: &AppBuild) -> bool {
    let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
    retired.retain(|_, (_, at)| at.elapsed() < RETIRED_FOR);
    retired
        .get(&build.executable)
        .is_some_and(|(modified, _)| *modified == build.modified)
}

/// Whether the ChatGPT that would answer for `thread` refused our versions
/// recently, without connecting: a retired build is still installed as is.
fn installed_build_retired(thread: &str) -> bool {
    #[cfg(test)]
    if TEST_ROUTERS.lock().unwrap().contains_key(thread) {
        return retired(&AppBuild {
            executable: format!("test-router:{thread}"),
            modified: None,
        });
    }
    let _ = thread;
    let executables = RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    executables
        .iter()
        .any(|executable| retired(&AppBuild::of(executable)))
}
/// Threads whose last attempt sent nothing, by when.
static REFUSED: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn refused_recently(thread: &str) -> bool {
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    refused.retain(|_, at| at.elapsed() < REFUSAL_COOLDOWN);
    refused.contains_key(thread)
}

/// Sends to `thread` go the usual way for a while.
pub(crate) fn refuse(thread: &str) {
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    if refused.len() >= 256 {
        refused.clear();
    }
    refused.insert(thread.to_owned(), Instant::now());
}

#[cfg(test)]
pub(crate) static TEST_ROUTERS: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn socket_path(home: &Path, thread: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_ROUTERS.lock().unwrap().get(thread) {
        return path.clone();
    }
    let _ = thread;
    home.join("ipc").join("ipc.sock")
}

/// Whether a send to this thread may go through ChatGPT: its router is
/// there, ChatGPT's own Codex server has the thread open, no attempt for it
/// failed in the last minute and the installed ChatGPT did not refuse our
/// versions. Cheap: reads files and the process table.
pub(crate) async fn may_deliver(home: &Path, thread: &str) -> bool {
    if refused_recently(thread)
        || installed_build_retired(thread)
        || std::fs::symlink_metadata(socket_path(home, thread)).is_err()
    {
        return false;
    }
    #[cfg(test)]
    if TEST_ROUTERS.lock().unwrap().contains_key(thread) {
        return true;
    }
    crate::desktop_activity::codex_holder(thread)
        .await
        .is_some_and(|holder| holder.kind == crate::desktop_activity::HolderKind::Desktop)
}

/// A verified connection to ChatGPT's router that found the thread's owner.
pub(crate) struct Session {
    stream: UnixStream,
    buffer: Vec<u8>,
    client_id: String,
    owner: String,
    thread: String,
    build: AppBuild,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("thread", &self.thread)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

enum CallError {
    /// Nothing was written.
    NotWritten,
    /// Written, then the connection closed or no answer came in time.
    Lost(&'static str),
}

/// Connects to the router, checks it is ChatGPT's, and finds which client
/// owns `thread`. Nothing reaches the thread.
pub(crate) async fn open(home: &Path, thread: &str) -> Result<Session, NotSent> {
    #[cfg(not(unix))]
    {
        let _ = (home, thread);
        Err(NotSent::Unavailable)
    }
    #[cfg(unix)]
    open_unix(home, thread).await
}

#[cfg(unix)]
async fn open_unix(home: &Path, thread: &str) -> Result<Session, NotSent> {
    let path = socket_path(home, thread);
    trusted_socket(&path)?;
    let stream = tokio::time::timeout(CONNECT_WAIT, UnixStream::connect(&path))
        .await
        .map_err(|_| NotSent::Unavailable)?
        .map_err(|_| NotSent::Unavailable)?;
    let build = verify_peer(&stream, thread)?;
    if retired(&build) {
        return Err(NotSent::Version);
    }
    let mut session = Session {
        stream,
        buffer: Vec::new(),
        client_id: "initializing".into(),
        owner: String::new(),
        thread: thread.to_owned(),
        build,
    };
    let init = session
        .call(
            "initialize",
            0,
            json!({"clientType": "cupola-relay"}),
            None,
            CONNECT_WAIT,
        )
        .await
        .map_err(|_| NotSent::Unavailable)?;
    session.client_id = init
        .pointer("/result/clientId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(NotSent::Unavailable)?
        .to_owned();
    session.owner = session.discover().await.ok_or(NotSent::NoOwner)?;
    Ok(session)
}

impl Session {
    /// Delivers `input` once, keyed by `client_message_id`: added to the
    /// running turn `running` (Codex records it as a steer), or as a new turn
    /// when none runs or it ended first.
    pub(crate) async fn deliver(
        &mut self,
        client_message_id: &str,
        input: &Value,
        running: Option<&str>,
    ) -> Outcome {
        if let Some(turn) = running {
            match self.steer(client_message_id, input, turn).await {
                Outcome::NotSent(NotSent::TurnEnded) => {}
                outcome => return outcome,
            }
        }
        self.start(client_message_id, input).await
    }

    async fn start(&mut self, client_message_id: &str, input: &Value) -> Outcome {
        let params = json!({
            "conversationId": self.thread,
            "turnStart": {
                "request": {
                    "threadId": self.thread,
                    "input": input,
                    "clientUserMessageId": client_message_id,
                },
                "context": {},
            },
        });
        let response = self.targeted(START_TURN, params).await;
        self.outcome(
            response,
            |result| {
                result
                    .pointer("/result/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            },
            false,
        )
        .await
    }

    async fn steer(&mut self, client_message_id: &str, input: &Value, turn: &str) -> Outcome {
        // `restoreMessage` is the owner's record of the draft (it reads its
        // `context`); attachments are already in `input`.
        let params = json!({
            "conversationId": self.thread,
            "clientUserMessageId": client_message_id,
            "input": input,
            "attachments": [],
            "restoreMessage": {"context": {}},
        });
        let response = self.targeted(STEER_TURN, params).await;
        let running = turn.to_owned();
        self.outcome(
            response,
            move |result| {
                let result = result.get("result").unwrap_or(result);
                Some(
                    result
                        .get("turnId")
                        .or_else(|| result.pointer("/turn/id"))
                        .and_then(Value::as_str)
                        .unwrap_or(&running)
                        .to_owned(),
                )
            },
            true,
        )
        .await
    }

    async fn targeted(
        &mut self,
        (method, version): (&'static str, u64),
        params: Value,
    ) -> Result<Value, CallError> {
        let owner = self.owner.clone();
        self.call(method, version, params, Some(&owner), SEND_WAIT)
            .await
    }

    async fn outcome(
        &mut self,
        response: Result<Value, CallError>,
        turn: impl FnOnce(&Value) -> Option<String>,
        steered: bool,
    ) -> Outcome {
        let response = match response {
            Ok(response) => response,
            Err(CallError::NotWritten) => return Outcome::NotSent(NotSent::Unavailable),
            Err(CallError::Lost(why)) => return Outcome::Unknown(why.into()),
        };
        if response.get("resultType").and_then(Value::as_str) == Some("success") {
            let result = response.get("result").unwrap_or(&Value::Null);
            return match turn(result) {
                Some(turn) if !turn.is_empty() => Outcome::Delivered { turn, steered },
                _ => Outcome::Unknown("ChatGPT returned no turn".into()),
            };
        }
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match self.refusal(error).await {
            Some(refusal) => Outcome::NotSent(refusal),
            None if steer_inactive(error) => Outcome::NotSent(NotSent::TurnEnded),
            // The owner ran the request and it failed: it may have started.
            None => Outcome::Unknown(error.chars().take(200).collect()),
        }
    }

    /// Asks the owner to interrupt `turn`, and only that turn: one that
    /// already ended is left alone, as is any turn after it.
    pub(crate) async fn interrupt(&mut self, turn: &str) -> Stop {
        let params = json!({
            "conversationId": self.thread,
            "mode": "user-stop",
            "expectedTurnId": turn,
        });
        let response = match self.targeted(INTERRUPT_TURN, params).await {
            Ok(response) => response,
            Err(CallError::NotWritten) => return Stop::NotSent(NotSent::Unavailable),
            Err(CallError::Lost(why)) => return Stop::Unknown(why.into()),
        };
        if response.get("resultType").and_then(Value::as_str) == Some("success") {
            let result = response.get("result").unwrap_or(&Value::Null);
            let result = result.get("result").unwrap_or(result);
            return match result.get("interruptedTurnId") {
                Some(Value::String(stopped)) if stopped == turn => Stop::Stopping,
                // Measured: `null` when that turn is not the one running.
                Some(Value::Null) => Stop::NotRunning,
                None => Stop::Unknown("ChatGPT didn't say which turn it stopped".into()),
                Some(other) => Stop::Unknown(format!("ChatGPT stopped another turn: {other}")),
            };
        }
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match self.refusal(error).await {
            Some(refusal) => Stop::NotSent(refusal),
            None => Stop::Unknown(error.chars().take(200).collect()),
        }
    }

    /// The router's word that a request was not forwarded: the owner left,
    /// or refused this method version (it names the same owner again).
    async fn refusal(&mut self, error: &str) -> Option<NotSent> {
        match error {
            "no-client-found" | "client-not-found" => {
                if self.discover().await.as_deref() == Some(self.owner.as_str()) {
                    self.retire();
                    Some(NotSent::Version)
                } else {
                    Some(NotSent::NoOwner)
                }
            }
            "request-version-mismatch" | "no-handler-for-request" => {
                self.retire();
                Some(NotSent::Version)
            }
            _ => None,
        }
    }

    fn retire(&self) {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        if retired.len() >= 16 {
            retired.clear();
        }
        retired.insert(
            self.build.executable.clone(),
            (self.build.modified, Instant::now()),
        );
    }

    async fn discover(&mut self) -> Option<String> {
        let params = json!({"hostId": "local", "conversationId": self.thread});
        let response = self
            .call(DISCOVERY.0, DISCOVERY.1, params, None, DISCOVERY_WAIT)
            .await
            .ok()?;
        (response.get("resultType").and_then(Value::as_str) == Some("success"))
            .then(|| {
                response
                    .get("handledByClientId")?
                    .as_str()
                    .map(str::to_owned)
            })
            .flatten()
            .filter(|owner| !owner.is_empty())
    }

    /// One request and its response. Requests the router routes to us are
    /// declined meanwhile: this client owns no thread.
    async fn call(
        &mut self,
        method: &str,
        version: u64,
        params: Value,
        target: Option<&str>,
        wait: Duration,
    ) -> Result<Value, CallError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut request = json!({
            "type": "request", "requestId": request_id, "sourceClientId": self.client_id,
            "version": version, "method": method, "params": params,
        });
        if let Some(target) = target {
            request["targetClientId"] = json!(target);
            request["timeoutMs"] = json!(wait.as_millis() as u64);
        }
        self.write(&request)
            .await
            .map_err(|_| CallError::NotWritten)?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let message = tokio::time::timeout_at(deadline, self.read())
                .await
                .map_err(|_| CallError::Lost("ChatGPT did not answer"))?
                .ok_or(CallError::Lost("ChatGPT closed the connection"))?;
            if message.get("type").and_then(Value::as_str) == Some("response")
                && message.get("requestId").and_then(Value::as_str) == Some(request_id.as_str())
            {
                return Ok(message);
            }
            self.decline_routed(&message).await;
        }
    }

    /// Requests the router routes to this client are declined: it owns no
    /// thread. True when `message` was one.
    async fn decline_routed(&mut self, message: &Value) -> bool {
        let reply = match message.get("type").and_then(Value::as_str) {
            Some("client-discovery-request") => json!({"type": "client-discovery-response",
                "requestId": message.get("requestId"), "response": {"canHandle": false}}),
            Some("request") => json!({"type": "response", "requestId": message.get("requestId"),
                "resultType": "error", "error": "no-handler-for-request"}),
            _ => return false,
        };
        let _ = self.write(&reply).await;
        true
    }

    /// Starts (or stops) following the thread: its owner answers with a
    /// snapshot of its state, then sends every change (`next_broadcast`).
    /// Asked again while following, the owner sends a fresh snapshot.
    pub(crate) async fn follow(&mut self, following: bool) -> std::io::Result<()> {
        let message = json!({
            "type": "broadcast", "method": FOLLOWING.0, "version": FOLLOWING.1,
            "sourceClientId": self.client_id, "targetClientIds": [self.owner],
            "params": {"conversationId": self.thread, "hostId": "local", "following": following},
        });
        self.write(&message).await
    }

    /// The next broadcast this client receives, in the version we speak;
    /// requests routed here meanwhile are declined. `None` once the
    /// connection ends.
    pub(crate) async fn next_broadcast(&mut self) -> Option<Value> {
        loop {
            let message = self.read().await?;
            if self.decline_routed(&message).await
                || message.get("type").and_then(Value::as_str) != Some("broadcast")
            {
                continue;
            }
            let method = message.get("method").and_then(Value::as_str);
            let version = message.get("version").and_then(Value::as_u64).unwrap_or(0);
            let ours = [FOLLOWING, STATE_CHANGED, FOLLOWING_REQUESTED]
                .iter()
                .any(|(name, ours)| method == Some(name) && version == *ours);
            if ours {
                return Some(message);
            }
        }
    }

    /// Answers request `request` (its app-server ID, as the owner listed it)
    /// with the app-server `response` the phone signed, through the owner,
    /// as a click in ChatGPT would. The owner acknowledges even a request it
    /// no longer has (answered there first); its state says which.
    pub(crate) async fn decide(
        &mut self,
        method: &str,
        request: &Value,
        response: &Value,
    ) -> Decided {
        let (call, answer) = match method {
            "item/commandExecution/requestApproval" => (COMMAND_DECISION, "decision"),
            "item/fileChange/requestApproval" => (FILE_DECISION, "decision"),
            "item/permissions/requestApproval" => (PERMISSIONS_RESPONSE, "response"),
            "item/tool/requestUserInput" => (USER_INPUT_RESPONSE, "response"),
            _ => return Decided::NotSent(NotSent::Version),
        };
        // The owner wraps a decision as `{decision}` itself.
        let value = if answer == "decision" {
            match response.get("decision") {
                Some(decision) => decision.clone(),
                None => return Decided::NotSent(NotSent::Version),
            }
        } else {
            response.clone()
        };
        let params = json!({"conversationId": self.thread, "requestId": request, answer: value});
        let response = match self.targeted(call, params).await {
            Ok(response) => response,
            Err(CallError::NotWritten) => return Decided::NotSent(NotSent::Unavailable),
            Err(CallError::Lost(why)) => return Decided::Unknown(why.into()),
        };
        if response.get("resultType").and_then(Value::as_str) == Some("success") {
            return Decided::Sent;
        }
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match self.refusal(error).await {
            Some(refusal) => Decided::NotSent(refusal),
            None => Decided::Unknown(error.chars().take(200).collect()),
        }
    }

    async fn write(&mut self, message: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(message)?;
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        self.stream.write_all(&frame).await
    }

    /// The next frame, or `None` once the connection ends or sends garbage.
    async fn read(&mut self) -> Option<Value> {
        loop {
            if self.buffer.len() >= 4 {
                let length = u32::from_le_bytes(self.buffer[..4].try_into().ok()?) as usize;
                if length > MAX_FRAME {
                    return None;
                }
                if self.buffer.len() >= 4 + length {
                    let body: Vec<u8> = self.buffer.drain(..4 + length).skip(4).collect();
                    return serde_json::from_slice(&body).ok();
                }
            }
            let mut chunk = [0u8; 16 * 1024];
            let read = self.stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// ChatGPT's message when the turn a steer targeted ended first.
fn steer_inactive(error: &str) -> bool {
    error.contains("SteerTurnInactiveError")
        || error.contains("because its active turn already ended")
        || error.contains("no active turn to steer")
}

/// An owner-only socket of this user: anything else is not ChatGPT's.
#[cfg(unix)]
fn trusted_socket(path: &Path) -> Result<(), NotSent> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(path).map_err(|_| NotSent::Unavailable)?;
    let owner = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket() || metadata.uid() != owner || metadata.mode() & 0o077 != 0
    {
        return Err(NotSent::Untrusted);
    }
    Ok(())
}

/// The process on the other end is ChatGPT, signed by OpenAI, run by this
/// user. Returns its build.
#[cfg(target_os = "macos")]
fn verify_peer(stream: &UnixStream, thread: &str) -> Result<AppBuild, NotSent> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let (mut uid, mut gid) = (0, 0);
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 || uid != unsafe { libc::geteuid() }
    {
        return Err(NotSent::Untrusted);
    }
    let pid = peer_pid(fd).ok_or(NotSent::Untrusted)?;
    #[cfg(test)]
    if TEST_ROUTERS.lock().unwrap().contains_key(thread) {
        // The fake router runs in this test process; the real check refuses it.
        return (pid == std::process::id() as i32)
            .then(|| AppBuild {
                // One build per test thread, so retiring it touches no other test.
                executable: format!("test-router:{thread}"),
                modified: None,
            })
            .ok_or(NotSent::Untrusted);
    }
    let _ = thread;
    let start = crate::desktop_activity::process_start(pid);
    signed_as_chatgpt(pid)?;
    let executable = crate::desktop_activity::executable_path(pid).ok_or(NotSent::Untrusted)?;
    if crate::desktop_activity::app_bundle_name(&executable).as_deref() != Some("ChatGPT") {
        return Err(NotSent::Untrusted);
    }
    // Still the process that was checked.
    if start.is_none()
        || peer_pid(fd) != Some(pid)
        || crate::desktop_activity::process_start(pid) != start
    {
        return Err(NotSent::Untrusted);
    }
    Ok(AppBuild::of(&executable))
}

#[cfg(target_os = "macos")]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    (status == 0 && pid > 0).then_some(pid)
}

#[cfg(not(target_os = "macos"))]
fn verify_peer(_stream: &UnixStream, _thread: &str) -> Result<AppBuild, NotSent> {
    Err(NotSent::Untrusted)
}

#[cfg(target_os = "macos")]
fn signed_as_chatgpt(pid: i32) -> Result<(), NotSent> {
    use security_framework::os::macos::code_signing::{
        Flags, GuestAttributes, SecCode, SecRequirement,
    };
    let requirement: SecRequirement = CHATGPT_REQUIREMENT
        .parse()
        .map_err(|_| NotSent::Untrusted)?;
    let mut attributes = GuestAttributes::new();
    attributes.set_pid(pid);
    SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE)
        .and_then(|code| code.check_validity(Flags::NONE, &requirement))
        .map_err(|_| NotSent::Untrusted)
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::net::UnixListener;

    /// What the fake router does with a follower request.
    #[derive(Clone)]
    pub(crate) enum Reply {
        /// Answers as the owner: a turn id.
        Turn(&'static str),
        /// Answers an interrupt as the owner: the turn it stopped, if any.
        Interrupted(Option<&'static str>),
        /// Answers as the owner without naming a turn.
        Ok,
        /// A router or owner error string.
        Error(&'static str),
        /// Reads the request and closes the connection.
        Drop,
    }

    /// A stand-in for ChatGPT's router and the thread's owner in one socket.
    pub(crate) struct FakeRouter {
        pub(crate) dir: tempfile::TempDir,
        pub(crate) path: PathBuf,
        /// Every request received, as sent.
        pub(crate) requests: Arc<Mutex<Vec<Value>>>,
        /// The owner's state of the thread, as followers see it.
        pub(crate) owned: Arc<Mutex<Owned>>,
    }

    /// The owner's thread state: what its turn waits on, numbered as
    /// ChatGPT numbers its changes, and who follows it.
    #[derive(Default)]
    pub(crate) struct Owned {
        pub(crate) revision: u64,
        pub(crate) requests: Vec<Value>,
        followers: Vec<tokio::sync::mpsc::UnboundedSender<Value>>,
        /// Every follower answer received, by its params.
        pub(crate) answers: Vec<Value>,
        /// Follow requests received (`following: true`).
        pub(crate) follows: usize,
        /// Answers are acknowledged but the request stays (a lost click).
        pub(crate) ignore_answers: bool,
        /// Each connection closes once its owner is found: no follow lands.
        pub(crate) refuse_follows: bool,
    }

    impl Owned {
        fn change(&mut self, change: Value) {
            let message = json!({"type": "broadcast", "method": STATE_CHANGED.0,
                "version": STATE_CHANGED.1, "sourceClientId": "owner",
                "params": {"conversationId": "", "hostId": "local", "change": change}});
            self.followers
                .retain(|follower| follower.send(message.clone()).is_ok());
        }

        fn patch(&mut self, patches: Value) {
            self.revision += 1;
            let change = json!({"type": "patches", "baseRevision": self.revision - 1,
                "revision": self.revision, "patches": patches, "acceptedTextChanges": null});
            self.change(change);
        }

        fn snapshot(&mut self) {
            let change = json!({"type": "snapshot", "revision": self.revision,
                "conversationState": {"id": "", "requests": self.requests, "turns": []}});
            self.change(change);
        }

        /// The turn asks: a request is added, as ChatGPT adds one.
        pub(crate) fn ask(&mut self, request: Value) {
            self.requests.push(request.clone());
            let at = self.requests.len() - 1;
            self.patch(
                json!([{"op": "add", "path": ["requests", at], "value": request},
                {"op": "replace", "path": ["hasUnreadTurn"], "value": true}]),
            );
        }

        /// The request is answered or dropped there: ChatGPT replaces the list.
        pub(crate) fn remove(&mut self, id: &Value) {
            self.requests.retain(|request| &request["id"] != id);
            let requests = self.requests.clone();
            self.patch(json!([{"op": "replace", "path": ["requests"], "value": requests}]));
        }

        /// A change this follower never receives (its next patch has a gap).
        pub(crate) fn missed(&mut self, request: Value) {
            self.requests.push(request);
            self.revision += 1;
        }

        /// A change that does not touch the requests.
        pub(crate) fn unrelated(&mut self) {
            self.patch(json!([{"op": "replace", "path": ["threadRuntimeStatus"], "value": {"type": "active"}}]));
        }

        /// The owner sends every follower a new snapshot (as at a turn's end).
        pub(crate) fn resend(&mut self) {
            self.revision += 1;
            self.snapshot();
        }

        /// The connection to every follower closes.
        pub(crate) fn disconnect(&mut self) {
            for follower in self.followers.drain(..) {
                let _ = follower.send(json!({"close": true}));
            }
        }

        pub(crate) fn following(&self) -> bool {
            self.followers.iter().any(|follower| !follower.is_closed())
        }
    }

    impl FakeRouter {
        /// `owner`: whether discovery names an owner. `replies`: per method.
        pub(crate) fn start(owner: bool, replies: HashMap<&'static str, Reply>) -> Self {
            Self::start_with_mode(owner, replies, 0o600)
        }

        fn start_with_mode(owner: bool, replies: HashMap<&'static str, Reply>, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ipc.sock");
            let listener = UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let owned = Arc::new(Mutex::new(Owned::default()));
            let (seen, thread) = (requests.clone(), owned.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let seen = seen.clone();
                    let replies = replies.clone();
                    tokio::spawn(serve(stream, owner, replies, seen, thread.clone()));
                }
            });
            Self {
                dir,
                path,
                requests,
                owned,
            }
        }

        pub(crate) fn owner(&self) -> std::sync::MutexGuard<'_, Owned> {
            self.owned.lock().unwrap()
        }

        pub(crate) fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter_map(|r| r["method"].as_str().map(str::to_owned))
                .collect()
        }

        /// Routes `thread`'s relay to this router.
        pub(crate) fn serve_thread(&self, thread: &str) {
            TEST_ROUTERS
                .lock()
                .unwrap()
                .insert(thread.to_owned(), self.path.clone());
        }
    }

    async fn serve(
        stream: UnixStream,
        owner: bool,
        replies: HashMap<&'static str, Reply>,
        seen: Arc<Mutex<Vec<Value>>>,
        owned: Arc<Mutex<Owned>>,
    ) {
        let (to_follower, mut changes) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let mut session = Session {
            stream,
            buffer: Vec::new(),
            client_id: "router".into(),
            owner: String::new(),
            thread: String::new(),
            build: AppBuild {
                executable: String::new(),
                modified: None,
            },
        };
        loop {
            let request = tokio::select! {
                request = session.read() => match request {
                    Some(request) => request,
                    None => return,
                },
                change = changes.recv() => {
                    let Some(mut change) = change else { return };
                    if change["close"] == true {
                        return;
                    }
                    change["params"]["conversationId"] = json!(session.thread);
                    if session.write(&change).await.is_err() {
                        return;
                    }
                    continue;
                }
            };
            seen.lock().unwrap().push(request.clone());
            let method = request["method"].as_str().unwrap_or_default().to_owned();
            let id = request["requestId"].clone();
            if request["type"] == "broadcast" {
                if method == FOLLOWING.0 && request["version"] == FOLLOWING.1 && owner {
                    session.thread = request["params"]["conversationId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let mut owned = owned.lock().unwrap();
                    if request["params"]["following"] == true {
                        owned.follows += 1;
                        owned
                            .followers
                            .retain(|follower| !follower.same_channel(&to_follower));
                        owned.followers.push(to_follower.clone());
                        // The first follower gets a new revision, as in ChatGPT.
                        if owned.followers.len() == 1 && owned.follows == 1 {
                            owned.revision += 1;
                        }
                        let change = json!({"type": "snapshot", "revision": owned.revision,
                            "conversationState": {"id": "", "requests": owned.requests, "turns": []}});
                        let _ = to_follower.send(
                            json!({"type": "broadcast", "method": STATE_CHANGED.0,
                            "version": STATE_CHANGED.1, "sourceClientId": "owner",
                            "params": {"conversationId": "", "hostId": "local", "change": change}}),
                        );
                    } else {
                        owned
                            .followers
                            .retain(|follower| !follower.same_channel(&to_follower));
                    }
                }
                continue;
            }
            if request["type"] != "request" {
                continue;
            }
            if [
                COMMAND_DECISION,
                FILE_DECISION,
                PERMISSIONS_RESPONSE,
                USER_INPUT_RESPONSE,
            ]
            .iter()
            .any(|(name, version)| method == *name && request["version"] == *version)
                && !replies.contains_key(method.as_str())
            {
                {
                    let mut owned = owned.lock().unwrap();
                    owned.answers.push(request["params"].clone());
                    let asked = request["params"]["requestId"].clone();
                    if !owned.ignore_answers && owned.requests.iter().any(|r| r["id"] == asked) {
                        owned.remove(&asked);
                    }
                }
                let reply = json!({"type":"response","requestId":id,"resultType":"success",
                    "method":method,"handledByClientId":"owner","result":{"ok":true}});
                if session.write(&reply).await.is_err() {
                    return;
                }
                continue;
            }
            let reply = match method.as_str() {
                "initialize" => json!({"type":"response","requestId":id,"resultType":"success",
                    "method":"initialize","handledByClientId":"follower","result":{"clientId":"follower"}}),
                "thread-owner-discovery" if owner => {
                    // An unrelated routed request first: the client declines it.
                    let _ = session
                        .write(
                            &json!({"type":"client-discovery-request","requestId":"other",
                            "request":{"method":"thread-follower-start-turn"}}),
                        )
                        .await;
                    json!({"type":"response","requestId":id,"resultType":"success",
                        "method":method,"handledByClientId":"owner","result":{"supportsUntrustedAppInput":true}})
                }
                "thread-owner-discovery" => {
                    json!({"type":"response","requestId":id,"resultType":"error","error":"no-client-found"})
                }
                _ => match replies.get(method.as_str()) {
                    Some(Reply::Turn(turn)) => {
                        json!({"type":"response","requestId":id,"resultType":"success","method":method,
                        "handledByClientId":"owner","result":{"result":{"turn":{"id":turn,"status":"inProgress"}}}})
                    }
                    Some(Reply::Interrupted(turn)) => {
                        json!({"type":"response","requestId":id,"resultType":"success","method":method,
                        "handledByClientId":"owner","result":{"method":method,"result":{"interruptedTurnId":turn,"ok":true}}})
                    }
                    Some(Reply::Ok) => {
                        json!({"type":"response","requestId":id,"resultType":"success","method":method,
                        "handledByClientId":"owner","result":{"method":method,"result":{"ok":true}}})
                    }
                    Some(Reply::Error(error)) => {
                        json!({"type":"response","requestId":id,"resultType":"error","error":error})
                    }
                    Some(Reply::Drop) => return,
                    None => {
                        json!({"type":"response","requestId":id,"resultType":"error","error":"no-client-found"})
                    }
                },
            };
            if session.write(&reply).await.is_err() {
                return;
            }
            // Found, then gone before the follower can follow.
            if method == "thread-owner-discovery" && owner && owned.lock().unwrap().refuse_follows {
                return;
            }
        }
    }

    fn input() -> Value {
        json!([{"type":"text","text":"hello","text_elements":[]}])
    }

    fn thread() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[tokio::test]
    async fn discovery_without_an_owner_sends_nothing_to_the_thread() {
        let router = FakeRouter::start(false, HashMap::new());
        let thread = thread();
        router.serve_thread(&thread);
        let home = router.dir.path();
        assert!(may_deliver(home, &thread).await);
        assert_eq!(open(home, &thread).await.unwrap_err(), NotSent::NoOwner);
        assert_eq!(router.methods(), ["initialize", "thread-owner-discovery"]);
    }

    #[tokio::test]
    async fn an_idle_thread_gets_a_new_turn_keyed_by_the_message() {
        let router =
            FakeRouter::start(true, HashMap::from([(START_TURN.0, Reply::Turn("turn-1"))]));
        let thread = thread();
        router.serve_thread(&thread);
        let mut session = open(router.dir.path(), &thread).await.unwrap();
        assert_eq!(
            session.deliver("client-1", &input(), None).await,
            Outcome::Delivered {
                turn: "turn-1".into(),
                steered: false
            }
        );
        let requests = router.requests.lock().unwrap().clone();
        let start = requests.last().unwrap();
        assert_eq!(start["method"], START_TURN.0);
        assert_eq!(start["version"], START_TURN.1);
        assert_eq!(start["targetClientId"], "owner");
        assert_eq!(start["sourceClientId"], "follower");
        assert_eq!(start["params"]["conversationId"], thread.as_str());
        let request = &start["params"]["turnStart"]["request"];
        assert_eq!(request["threadId"], thread.as_str());
        assert_eq!(request["clientUserMessageId"], "client-1");
        assert_eq!(request["input"], input());
        // The routed request it received meanwhile was declined.
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["type"] == "client-discovery-response")
                .map(|r| r["response"]["canHandle"].clone())
                .collect::<Vec<_>>(),
            [json!(false)]
        );
    }

    #[tokio::test]
    async fn a_running_turn_is_steered_and_an_ended_one_gets_a_new_turn() {
        let router = FakeRouter::start(
            true,
            HashMap::from([(STEER_TURN.0, Reply::Turn("running"))]),
        );
        let thread = thread();
        router.serve_thread(&thread);
        let mut session = open(router.dir.path(), &thread).await.unwrap();
        assert_eq!(
            session.deliver("client-1", &input(), Some("running")).await,
            Outcome::Delivered {
                turn: "running".into(),
                steered: true
            }
        );
        let steer = router.requests.lock().unwrap().last().unwrap().clone();
        assert_eq!(steer["method"], STEER_TURN.0);
        assert_eq!(steer["version"], STEER_TURN.1);
        assert_eq!(steer["params"]["clientUserMessageId"], "client-1");
        assert_eq!(steer["params"]["input"], input());
        assert!(steer["params"]["restoreMessage"]["context"].is_object());

        let ended = FakeRouter::start(
            true,
            HashMap::from([
                (
                    STEER_TURN.0,
                    Reply::Error(
                        "Cannot steer conversation x because its active turn already ended",
                    ),
                ),
                (START_TURN.0, Reply::Turn("next")),
            ]),
        );
        let thread = self::thread();
        ended.serve_thread(&thread);
        let mut session = open(ended.dir.path(), &thread).await.unwrap();
        assert_eq!(
            session.deliver("client-2", &input(), Some("running")).await,
            Outcome::Delivered {
                turn: "next".into(),
                steered: false
            }
        );
        assert_eq!(
            ended.methods()[2..],
            [STEER_TURN.0, START_TURN.0],
            "one steer, then one start with the same message"
        );
    }

    #[tokio::test]
    async fn an_owner_refusing_the_version_retires_that_build() {
        let router = FakeRouter::start(
            true,
            HashMap::from([(START_TURN.0, Reply::Error("no-client-found"))]),
        );
        let thread = thread();
        router.serve_thread(&thread);
        let mut session = open(router.dir.path(), &thread).await.unwrap();
        assert_eq!(
            session.deliver("client-1", &input(), None).await,
            Outcome::NotSent(NotSent::Version)
        );
        // Discovery named the same owner again, so its refusal was the version.
        assert_eq!(
            router.methods(),
            ["initialize", DISCOVERY.0, START_TURN.0, DISCOVERY.0]
        );
        assert_eq!(
            open(router.dir.path(), &thread).await.unwrap_err(),
            NotSent::Version
        );
        // The phone is not told the message goes through ChatGPT meanwhile.
        assert!(!may_deliver(router.dir.path(), &thread).await);
        // A mistaken refusal does not last until the daemon restarts.
        if let Some(long_ago) = Instant::now().checked_sub(RETIRED_FOR) {
            let build = format!("test-router:{thread}");
            RETIRED.lock().unwrap().get_mut(&build).unwrap().1 = long_ago;
            assert!(may_deliver(router.dir.path(), &thread).await);
        }
    }

    #[tokio::test]
    async fn stop_interrupts_only_the_relayed_turn() {
        let stop = |reply: Reply| async move {
            let router = FakeRouter::start(true, HashMap::from([(INTERRUPT_TURN.0, reply)]));
            let thread = thread();
            router.serve_thread(&thread);
            let mut session = open(router.dir.path(), &thread).await.unwrap();
            let stopped = session.interrupt("turn-1").await;
            (stopped, router)
        };
        let (stopped, router) = stop(Reply::Interrupted(Some("turn-1"))).await;
        assert_eq!(stopped, Stop::Stopping);
        let request = router.requests.lock().unwrap().last().unwrap().clone();
        assert_eq!(request["method"], INTERRUPT_TURN.0);
        assert_eq!(request["version"], INTERRUPT_TURN.1);
        assert_eq!(request["targetClientId"], "owner");
        assert_eq!(request["params"]["expectedTurnId"], "turn-1");
        assert_eq!(request["params"]["mode"], "user-stop");
        // That turn already ended there: nothing was stopped.
        assert_eq!(stop(Reply::Interrupted(None)).await.0, Stop::NotRunning);
        // An answer naming no turn says nothing about it: the turn may still
        // run, so it is not taken as ended.
        assert!(matches!(stop(Reply::Ok).await.0, Stop::Unknown(_)));
        assert!(matches!(stop(Reply::Drop).await.0, Stop::Unknown(_)));
        let (refused, router) = stop(Reply::Error("no-handler-for-request")).await;
        assert_eq!(refused, Stop::NotSent(NotSent::Version));
        assert_eq!(router.methods()[2..], [INTERRUPT_TURN.0]);
    }

    #[tokio::test]
    async fn an_answer_lost_after_sending_is_unknown_not_a_refusal() {
        let router = FakeRouter::start(true, HashMap::from([(START_TURN.0, Reply::Drop)]));
        let thread = thread();
        router.serve_thread(&thread);
        let mut session = open(router.dir.path(), &thread).await.unwrap();
        assert!(matches!(
            session.deliver("client-1", &input(), None).await,
            Outcome::Unknown(_)
        ));
        let owner_error = FakeRouter::start(
            true,
            HashMap::from([(START_TURN.0, Reply::Error("Failed to start turn"))]),
        );
        let thread = self::thread();
        owner_error.serve_thread(&thread);
        let mut session = open(owner_error.dir.path(), &thread).await.unwrap();
        assert_eq!(
            session.deliver("client-1", &input(), None).await,
            Outcome::Unknown("Failed to start turn".into())
        );
    }

    #[tokio::test]
    async fn a_router_that_is_not_chatgpt_is_refused_before_anything_is_sent() {
        // The real check: this test process is not ChatGPT.
        let router = FakeRouter::start(true, HashMap::new());
        let thread = thread();
        let stream = UnixStream::connect(&router.path).await.unwrap();
        assert_eq!(
            verify_peer(&stream, &thread).unwrap_err(),
            NotSent::Untrusted
        );
        // A socket other users can reach is not ChatGPT's.
        let open_socket = FakeRouter::start_with_mode(true, HashMap::new(), 0o666);
        open_socket.serve_thread(&thread);
        assert_eq!(
            open(open_socket.dir.path(), &thread).await.unwrap_err(),
            NotSent::Untrusted
        );
        assert!(router.requests.lock().unwrap().is_empty());
        assert!(open_socket.requests.lock().unwrap().is_empty());
    }

    // Each answer goes back as ChatGPT's owner reads it (26.1016.51333): it
    // wraps a decision as `{decision}` itself and passes a response as is.
    #[tokio::test]
    async fn an_answer_goes_back_in_the_owners_method_and_shape() {
        let router = FakeRouter::start(true, HashMap::new());
        let thread = thread();
        router.serve_thread(&thread);
        let mut session = open(router.dir.path(), &thread).await.unwrap();
        let permissions = json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"});
        let answers = json!({"answers": {"q": {"answers": ["Yes"]}}});
        for (method, response) in [
            (
                "item/commandExecution/requestApproval",
                json!({"decision": "accept"}),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision": "cancel"}),
            ),
            ("item/permissions/requestApproval", permissions.clone()),
            ("item/tool/requestUserInput", answers.clone()),
        ] {
            assert_eq!(
                session.decide(method, &json!(6), &response).await,
                Decided::Sent
            );
        }
        let sent: Vec<Value> = router
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| {
                r["method"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("thread-follower-"))
            })
            .map(|r| json!([r["method"], r["version"], r["targetClientId"], r["params"]]))
            .collect();
        let params = |answer: &str, value: Value| json!({"conversationId": thread, "requestId": 6, answer: value});
        assert_eq!(
            sent,
            [
                json!([
                    COMMAND_DECISION.0,
                    1,
                    "owner",
                    params("decision", json!("accept"))
                ]),
                json!([
                    FILE_DECISION.0,
                    1,
                    "owner",
                    params("decision", json!("cancel"))
                ]),
                json!([
                    PERMISSIONS_RESPONSE.0,
                    1,
                    "owner",
                    params("response", permissions)
                ]),
                json!([
                    USER_INPUT_RESPONSE.0,
                    1,
                    "owner",
                    params("response", answers)
                ]),
            ]
        );
        // Nothing is sent for a request the follower methods don't answer.
        assert_eq!(
            session
                .decide("mcpServer/elicitation/request", &json!(7), &json!({}))
                .await,
            Decided::NotSent(NotSent::Version)
        );
        let answer = |reply: Reply| async move {
            let router = FakeRouter::start(true, HashMap::from([(COMMAND_DECISION.0, reply)]));
            let thread = self::thread();
            router.serve_thread(&thread);
            let mut session = open(router.dir.path(), &thread).await.unwrap();
            let response = json!({"decision": "accept"});
            session
                .decide(
                    "item/commandExecution/requestApproval",
                    &json!(6),
                    &response,
                )
                .await
        };
        assert!(matches!(answer(Reply::Drop).await, Decided::Unknown(_)));
        assert_eq!(
            answer(Reply::Error("no-handler-for-request")).await,
            Decided::NotSent(NotSent::Version)
        );
    }

    #[test]
    fn a_failed_attempt_rests_the_thread() {
        let thread = thread();
        assert!(!refused_recently(&thread));
        refuse(&thread);
        assert!(refused_recently(&thread));
    }

    /// Live: the real ChatGPT router on this Mac, a scratch thread only.
    /// `CUPOLA_RELAY_THREAD` must be open in ChatGPT (`open codex://threads/ID`)
    /// and its rollout's folder inside `CUPOLA_RELAY_FIXTURE_ROOT`; anything
    /// else is refused before connecting. Starts one turn, then adds a second
    /// message to it while it runs, and reads both back from the rollout.
    #[tokio::test]
    #[ignore = "live: sends two messages into a ChatGPT scratch thread"]
    async fn live_scratch_thread_start_then_steer() {
        let log = |step: &str, detail: Value| {
            println!(
                "{}",
                json!({"t": chrono::Utc::now().to_rfc3339(), "step": step, "detail": detail})
            );
        };
        let (thread, home, rollout) = scratch_thread();
        let before = std::fs::read_to_string(&rollout).unwrap().len();
        let started = Instant::now();
        let mut session = match open(&home, &thread).await {
            Ok(session) => session,
            Err(refusal) => {
                log("open", json!(format!("{refusal:?}")));
                panic!("open refused: {refusal:?}");
            }
        };
        log(
            "open",
            json!({"owner": session.owner, "ms": started.elapsed().as_millis()}),
        );
        let (one, two) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let text = |body: &str| json!([{"type":"text","text":body,"text_elements":[]}]);
        let sent = Instant::now();
        let outcome = session
            .deliver(&one, &text("Cupola relay check 1: run `sleep 15` in the shell, then reply with the single word FIRST."), None)
            .await;
        log(
            "start",
            json!({"outcome": format!("{outcome:?}"), "ms": sent.elapsed().as_millis()}),
        );
        let Outcome::Delivered {
            turn,
            steered: false,
        } = outcome
        else {
            panic!("{outcome:?}")
        };
        tokio::time::sleep(Duration::from_secs(4)).await;
        let sent = Instant::now();
        let outcome = session
            .deliver(
                &two,
                &text(
                    "Cupola relay check 2: after the sleep, end your reply with the word SECOND.",
                ),
                Some(&turn),
            )
            .await;
        log(
            "steer",
            json!({"outcome": format!("{outcome:?}"), "ms": sent.elapsed().as_millis()}),
        );
        // Both messages, by client id, then the turn's end.
        let deadline = Instant::now() + Duration::from_secs(150);
        let (mut seen_one, mut seen_two, mut done) = (false, false, false);
        while Instant::now() < deadline && !(seen_one && seen_two && done) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let tail = std::fs::read_to_string(&rollout).unwrap()[before..].to_owned();
            seen_one = tail.contains(&one) || tail.contains("Cupola relay check 1");
            seen_two = tail.contains(&two) || tail.contains("Cupola relay check 2");
            done = tail.contains("\"type\":\"task_complete\"")
                || tail.contains("\"type\":\"turn_complete\"");
        }
        let tail = std::fs::read_to_string(&rollout).unwrap()[before..].to_owned();
        log(
            "rollout",
            json!({
                "path": rollout, "grewBytes": tail.len(), "firstText": seen_one, "secondText": seen_two,
                "firstClientIdRecorded": tail.contains(&one), "secondClientIdRecorded": tail.contains(&two),
                "turnCompleted": done, "turnIdInRollout": tail.contains(&turn),
            }),
        );
        assert!(seen_one && seen_two && done);
    }

    /// Live: Stop for a relayed turn on the same scratch thread. Starts one
    /// turn that sleeps, interrupts exactly that turn, reads the interruption
    /// back from the rollout and ChatGPT's lifecycle record (what recovery
    /// reads), then asks again: that turn no longer runs.
    #[tokio::test]
    #[ignore = "live: sends one message into a ChatGPT scratch thread and stops it"]
    async fn live_scratch_thread_stop() {
        let log = |step: &str, detail: Value| {
            println!(
                "{}",
                json!({"t": chrono::Utc::now().to_rfc3339(), "step": step, "detail": detail})
            );
        };
        let (thread, home, rollout) = scratch_thread();
        let before = std::fs::read_to_string(&rollout).unwrap().len();
        let mut session = open(&home, &thread).await.expect("open");
        log("open", json!({"owner": session.owner}));
        let client = uuid::Uuid::new_v4().to_string();
        let text = json!([{"type":"text","text":"Cupola relay stop check: run `sleep 60` in the shell, then reply with the single word DONE.","text_elements":[]}]);
        let outcome = session.deliver(&client, &text, None).await;
        log("start", json!(format!("{outcome:?}")));
        let Outcome::Delivered { turn, .. } = outcome else {
            panic!("{outcome:?}")
        };
        // Until the sleep is running (or 20 s).
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline
            && !std::fs::read_to_string(&rollout).unwrap()[before..].contains("sleep 60")
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let sent = Instant::now();
        let stopped = session.interrupt(&turn).await;
        log(
            "interrupt",
            json!({"outcome": format!("{stopped:?}"), "ms": sent.elapsed().as_millis()}),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut aborted = false;
        while Instant::now() < deadline && !aborted {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let tail = std::fs::read_to_string(&rollout).unwrap()[before..].to_owned();
            aborted = tail.lines().any(|line| {
                line.contains(&turn)
                    && (line.contains("\"turn_aborted\"") || line.contains("\"interrupted\""))
            });
        }
        let tail = std::fs::read_to_string(&rollout).unwrap()[before..].to_owned();
        log(
            "rollout",
            json!({
                "aborted": aborted, "completed": tail.contains("\"task_complete\""),
                "clientIdRecorded": tail.contains(&client), "ms": sent.elapsed().as_millis(),
            }),
        );
        // ChatGPT's own lifecycle record, as recovery reads it.
        let mut record = None;
        for _ in 0..20 {
            record = crate::codex_activity::read(&home, &thread, &rollout).await;
            if record
                .as_ref()
                .is_some_and(|r| r.turn == turn && !r.working())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        log(
            "lifecycle",
            json!(record
                .as_ref()
                .map(|r| json!({"sameTurn": r.turn == turn, "status": r.status}))),
        );
        let mut again = open(&home, &thread).await.expect("reopen");
        let second = again.interrupt(&turn).await;
        log("interrupt-again", json!(format!("{second:?}")));
        assert_eq!(stopped, Stop::Stopping);
        assert!(aborted);
        assert_eq!(second, Stop::NotRunning);
    }

    /// Live, read only: ChatGPT's lifecycle record for the scratch thread's
    /// latest turn, as recovery reads it.
    #[tokio::test]
    #[ignore = "live: reads a ChatGPT scratch thread's lifecycle record"]
    async fn live_scratch_thread_lifecycle() {
        let (thread, home, rollout) = scratch_thread();
        let record = crate::codex_activity::read(&home, &thread, &rollout).await;
        println!(
            "{}",
            json!({"t": chrono::Utc::now().to_rfc3339(), "step": "lifecycle",
                "detail": record.map(|r| json!({"turn": r.turn, "status": r.status}))})
        );
    }

    /// The scratch thread named by `CUPOLA_RELAY_THREAD`, refused unless its
    /// rollout's folder is inside `CUPOLA_RELAY_FIXTURE_ROOT`.
    pub(crate) fn scratch_thread() -> (String, PathBuf, PathBuf) {
        let thread = std::env::var("CUPOLA_RELAY_THREAD").unwrap();
        let root = crate::canonical_path::canonicalize(
            std::env::var("CUPOLA_RELAY_FIXTURE_ROOT").unwrap(),
        )
        .unwrap();
        let home = PathBuf::from(std::env::var("HOME").unwrap()).join(".codex");
        let rollout = walk(&home.join("sessions"), &format!("{thread}.jsonl")).expect("rollout");
        let first = std::fs::read_to_string(&rollout).unwrap();
        let cwd: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
        let cwd =
            crate::canonical_path::canonicalize(cwd["payload"]["cwd"].as_str().unwrap()).unwrap();
        assert!(
            cwd.starts_with(&root),
            "refused: {cwd:?} is not a fixture thread"
        );
        (thread, home, rollout)
    }

    fn walk(dir: &Path, suffix: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = walk(&path, suffix) {
                    return Some(found);
                }
            } else if path.to_string_lossy().ends_with(suffix) {
                return Some(path);
            }
        }
        None
    }
}
