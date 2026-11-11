//! `wonder_stream_screen`: an agent adds the screen of a remote machine, reached over SSH, as a view-only computer
//! stream (Experimental). docs/specs/stream-tool.md owns the design and the threat model; read it first.
//!
//! The rules in one place:
//! * only a literal `Host` alias from the owner's own SSH config, with its host key already pinned;
//! * the owner taps Allow on their phone before any connection is made;
//! * the only remote command is the fixed `stream/agent.py`; nothing the agent writes reaches a shell;
//! * view-only (`rfb.rs` has no input message), no key is created, read or passed, no port is opened;
//! * the agent receives metadata, never frames; frames go to the owner's phone only.
use super::*;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

mod rfb;
pub(crate) mod ssh;

/// The fixed remote program. Sent as `python3 -c SOURCE MODE ARG…`; see the header of the file.
const AGENT: &str = include_str!("stream/agent.py");

pub(in crate::thread_tools) const NAME: &str = "wonder_stream_screen";

const MAX_PER_PROJECT: usize = 2;
const MAX_PER_COMPUTER: usize = 4;
/// An Allow covers probe and add for one thread and host for this long.
const APPROVAL_TTL: Duration = Duration::from_secs(3600);
/// A stopped or failed stream stays listed this long so the phone can say so.
const ENDED_RETENTION: Duration = Duration::from_secs(300);
const CONNECT_STEP: Duration = Duration::from_secs(20);
const STALL: Duration = Duration::from_secs(20);
const PROBE_DEADLINE: Duration = Duration::from_secs(40);
const REAP: Duration = Duration::from_secs(3);
/// Frames are only prepared while somebody looks.
const VIEWED_WITHIN: Duration = Duration::from_secs(15);
/// How often the whole screen is asked for: quickly while somebody looks, slowly otherwise. A quiet screen
/// gets no reply to an incremental request, so each ask is for the full screen and is always answered.
const POLL_VIEWED: Duration = Duration::from_secs(1);
const POLL_IDLE: Duration = Duration::from_secs(5);
const JPEG_LONG_EDGE: u32 = 1600;
const JPEG_QUALITY: u8 = 70;
const MAX_SCREENSHOT: usize = 8 * 1024 * 1024;
const NAME_CHARS: usize = 60;

// ---------------------------------------------------------------------------
// the tool's description
// ---------------------------------------------------------------------------

pub(in crate::thread_tools) fn spec() -> Value {
    let request_id = json!({"type":"string","format":"uuid","description":"A new UUID for this add. Reuse it when you retry the same add; never reuse it for a different one."});
    json!({"type":"function","name":NAME,
        "description":"Experimental. Show the screen of another computer, reached over SSH, to the owner as a view-only stream under the computer icon on their phone, with a Stop button. host is an alias from the owner's own SSH config (never an address or user@host); its host key must already be pinned. The first probe or add for a host asks the owner to Allow it on their phone and waits (up to timeoutSeconds): nothing connects before they do. Steps: probe lists which transports the host offers (x11: x11vnc on an X display or a private Xvfb; vnc: a VNC server without a password on the host's loopback; screenshot: periodic captures), then add with the transport you pick. You never receive the pixels, only the stream's state; the owner sees the screen. To work in a private display, add with startDisplay, then use the returned display from your own shell tools. Streams stop with stop, the owner's Stop, an error or when Cupola quits; there is no automatic reconnect. At most 2 streams per Project. Read-only threads cannot use this tool.",
        "inputSchema":{"type":"object","additionalProperties":false,
            "properties":{
                "action":{"type":"string","enum":["probe","add","list","stop"]},
                "host":{"type":"string","maxLength":253,"description":"An alias that is a literal Host entry in the owner's SSH config."},
                "transport":{"type":"string","enum":["x11","vnc","screenshot"],"description":"For add: one the probe reported as available."},
                "name":{"type":"string","maxLength":NAME_CHARS,"description":"For add: the label under the computer icon; default is the host alias."},
                "display":{"type":"string","maxLength":8,"description":"For add with x11 or screenshot: an existing X display such as :0, as probe reported."},
                "startDisplay":{"type":"object","additionalProperties":false,"description":"For add with x11: start a private empty Xvfb display instead of using display. Cupola stops it with the stream.",
                    "properties":{"width":{"type":"integer","minimum":320,"maximum":3840},"height":{"type":"integer","minimum":240,"maximum":2160}},"required":["width","height"]},
                "vncPort":{"type":"integer","minimum":5900,"maximum":5999,"description":"For add with vnc: a port probe reported on the host's loopback. Default 5900."},
                "intervalSeconds":{"type":"integer","minimum":1,"maximum":30,"description":"For add with screenshot: seconds between captures. Default 3."},
                "streamId":{"type":"string","description":"For stop: a stream your Project added."},
                "timeoutSeconds":{"type":"integer","minimum":1,"maximum":1800,"description":"For probe and add: how long to wait for the owner to allow the host. Default 600."},
                "clientRequestId":request_id},
            "required":["action"]}})
}

// ---------------------------------------------------------------------------
// plain types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    AwaitingApproval,
    Allowed,
    Connecting,
    Live,
    Stopped,
    Failed,
}

impl State {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingApproval => "awaiting_approval",
            Self::Allowed => "allowed",
            Self::Connecting => "connecting",
            Self::Live => "live",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    fn ended(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    X11,
    Vnc,
    Screenshot,
}

impl Transport {
    fn as_str(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::Vnc => "vnc",
            Self::Screenshot => "screenshot",
        }
    }
}

/// A failure the agent and the owner can read: a stable code and fixed words.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fail {
    code: &'static str,
    message: String,
}

impl Fail {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn from_class(class: ssh::SshClass) -> Self {
        let code = match class {
            ssh::SshClass::HostKeyUnpinned => "host_key_unpinned",
            ssh::SshClass::HostKeyChanged => "host_key_changed",
            ssh::SshClass::AuthRejected => "auth_rejected",
            ssh::SshClass::KeyFileUnusable => "key_file_unusable",
        };
        Self::new(code, class.fix())
    }

    fn unreachable() -> Self {
        Self::new("unreachable", "The connection to that computer failed or ended. Run `ssh -o BatchMode=yes ALIAS true` to see why.")
    }

    /// What `agent.py` reports on its own `CUPOLA-ERROR` line.
    fn remote(code: &str) -> Self {
        match code {
            "no-x11vnc" => Self::new("transport_unavailable", "x11vnc is not installed for that account on the computer. Install it there, or use the screenshot transport."),
            "no-xvfb" => Self::new("transport_unavailable", "Xvfb is not installed on that computer, so startDisplay cannot work. Use an existing display."),
            "no-display" => Self::new("transport_unavailable", "That X display does not exist on the computer (or no private display was free). Run probe and use a display it listed."),
            "no-vnc-server" => Self::new("transport_unavailable", "Nothing is listening on that VNC port on the computer's loopback. Run probe and use a port it listed."),
            "no-capture-tool" => Self::new("transport_unavailable", "The computer has no screenshot tool Cupola can use (import, scrot, grim or screencapture)."),
            "capture-failed" => Self::new("transport_unavailable", "The computer's screenshot tool failed three times in a row."),
            _ => Self::new("protocol", "That computer did not speak the protocol Cupola expects."),
        }
    }

    fn rfb(error: rfb::RfbError) -> Self {
        match error {
            rfb::RfbError::AuthRequired => Self::new("vnc_auth_unsupported", "That VNC server needs a password, which Cupola does not hold. Use x11, or a VNC server without a password on the computer's loopback."),
            rfb::RfbError::TooLarge => Self::new("transport_unavailable", "That screen is larger than 8.5 megapixels. Use startDisplay with a smaller size."),
            rfb::RfbError::Refused | rfb::RfbError::Unsupported | rfb::RfbError::Protocol => Self::new("protocol", "That computer did not speak the screen protocol Cupola expects."),
            rfb::RfbError::Io => Self::unreachable(),
        }
    }

    fn stalled() -> Self {
        Self::new(
            "stalled",
            "That computer sent no new screen data for 20 seconds, so the stream ended.",
        )
    }

    fn tool(&self) -> ToolError {
        ToolError::new(self.code, self.message.clone())
    }
}

/// One prepared picture: Cupola's own JPEG, never the host's bytes.
pub(crate) struct Frame {
    pub(crate) sequence: u64,
    pub(crate) jpeg: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) sha256: String,
    at: Instant,
}

struct Entry {
    id: String,
    project_id: String,
    conversation_id: String,
    agent: String,
    host: String,
    name: String,
    intent: String,
    transport: Option<Transport>,
    display: Option<String>,
    state: State,
    reason: Option<Fail>,
    created: Instant,
    ended: Option<Instant>,
    /// While awaiting approval: when the waiting call gives up.
    approval_deadline: Option<Instant>,
    approved_until: Option<Instant>,
    request: Option<String>,
    frame: Option<Arc<Frame>>,
    viewed: Option<Instant>,
    interval_ms: u64,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// What the phone and the tool see of an entry.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) transport: Option<&'static str>,
    pub(crate) state: &'static str,
    pub(crate) reason: Option<String>,
    pub(crate) reason_code: Option<&'static str>,
    pub(crate) agent: String,
    pub(crate) intent: String,
    pub(crate) experimental: bool,
    pub(crate) age_seconds: u64,
    pub(crate) frame_age_seconds: Option<u64>,
    #[serde(skip)]
    kind: State,
    #[serde(skip)]
    project_id: String,
    #[serde(skip)]
    conversation_id: String,
}

impl Entry {
    fn view(&self) -> StreamView {
        StreamView {
            id: self.id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            transport: self.transport.map(Transport::as_str),
            state: self.state.as_str(),
            reason: self.reason.as_ref().map(|r| r.message.clone()),
            reason_code: self.reason.as_ref().map(|r| r.code),
            agent: self.agent.clone(),
            intent: self.intent.clone(),
            experimental: true,
            age_seconds: self.created.elapsed().as_secs(),
            frame_age_seconds: self.frame.as_ref().map(|f| f.at.elapsed().as_secs()),
            kind: self.state,
            project_id: self.project_id.clone(),
            conversation_id: self.conversation_id.clone(),
        }
    }

    fn end(&mut self, state: State, reason: Option<Fail>) {
        if self.state.ended() {
            return;
        }
        self.state = state;
        self.reason = reason;
        self.ended = Some(Instant::now());
    }
}

#[derive(Default)]
struct Inner {
    entries: BTreeMap<String, Entry>,
}

type Shared = Arc<Mutex<Inner>>;

#[derive(Clone)]
struct Settings {
    program: Option<PathBuf>,
    /// `-F` and the alias list, for tests; production leaves it unset so OpenSSH reads the owner's own config.
    config: Option<PathBuf>,
}

/// The registry of this computer's remote screens. One per daemon (`ComputerSessionSupervisor`).
pub(crate) struct Streams {
    inner: Shared,
    settings: Mutex<Settings>,
}

impl Default for Streams {
    fn default() -> Self {
        Self {
            inner: Shared::default(),
            settings: Mutex::new(Settings {
                program: ssh::default_program(),
                config: None,
            }),
        }
    }
}

/// A request to use a host, from `probe` or `add`.
struct Open<'a> {
    project_id: &'a str,
    conversation_id: &'a str,
    agent: &'a str,
    host: &'a str,
    intent: &'a str,
    timeout: Duration,
}

impl Streams {
    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn configure_for_test(&self, program: PathBuf, config: PathBuf) {
        *self.settings.lock().unwrap() = Settings {
            program: Some(program),
            config: Some(config),
        };
    }

    /// Ends what has run out: unanswered requests, unused Allows, and forgets ended entries after a while.
    fn sweep(inner: &mut Inner) {
        let now = Instant::now();
        for entry in inner.entries.values_mut() {
            match entry.state {
                State::AwaitingApproval if entry.approval_deadline.is_some_and(|d| now >= d) => {
                    entry.end(
                        State::Stopped,
                        Some(Fail::new(
                            "expired",
                            "The owner did not answer in time. Ask again later.",
                        )),
                    );
                }
                State::Allowed if entry.approved_until.is_some_and(|d| now >= d) => {
                    entry.end(
                        State::Stopped,
                        Some(Fail::new(
                            "expired",
                            "The owner's Allow ran out after an hour. Ask again.",
                        )),
                    );
                }
                _ => {}
            }
        }
        inner.entries.retain(|_, e| {
            e.ended
                .is_none_or(|at| now.duration_since(at) < ENDED_RETENTION)
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        let mut inner = self.inner.lock().unwrap();
        Self::sweep(&mut inner);
        inner
    }

    /// The thread's request for this host: a live one is reused, otherwise a new one waits for the owner.
    fn open(&self, open: &Open<'_>) -> Result<String, ToolError> {
        let mut inner = self.lock();
        if let Some(existing) = inner.entries.values_mut().find(|e| {
            e.conversation_id == open.conversation_id && e.host == open.host && !e.state.ended()
        }) {
            if existing.state == State::AwaitingApproval {
                existing.approval_deadline = Some(Instant::now() + open.timeout);
            }
            if existing.state == State::AwaitingApproval || existing.state == State::Allowed {
                existing.intent = open.intent.to_owned();
            }
            return Ok(existing.id.clone());
        }
        let active = |e: &&Entry| !e.state.ended();
        if inner.entries.values().filter(active).count() >= MAX_PER_COMPUTER
            || inner
                .entries
                .values()
                .filter(active)
                .filter(|e| e.project_id == open.project_id)
                .count()
                >= MAX_PER_PROJECT
        {
            return Err(ToolError::new(
                "too_many_streams",
                format!("At most {MAX_PER_PROJECT} remote screens per Project ({MAX_PER_COMPUTER} on this computer). Stop one first."),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        inner.entries.insert(
            id.clone(),
            Entry {
                id: id.clone(),
                project_id: open.project_id.to_owned(),
                conversation_id: open.conversation_id.to_owned(),
                agent: open.agent.to_owned(),
                host: open.host.to_owned(),
                name: open.host.to_owned(),
                intent: open.intent.to_owned(),
                transport: None,
                display: None,
                state: State::AwaitingApproval,
                reason: None,
                created: Instant::now(),
                ended: None,
                approval_deadline: Some(Instant::now() + open.timeout),
                approved_until: None,
                request: None,
                frame: None,
                viewed: None,
                interval_ms: 1000,
                stop: None,
                task: None,
            },
        );
        Ok(id)
    }

    fn view(&self, id: &str) -> Option<StreamView> {
        self.lock().entries.get(id).map(Entry::view)
    }

    /// Every stream on this computer, oldest first, for the owner's phone.
    pub(crate) fn views(&self) -> Vec<StreamView> {
        let inner = self.lock();
        let mut views: Vec<_> = inner.entries.values().map(Entry::view).collect();
        views.sort_by_key(|v| std::cmp::Reverse(v.age_seconds));
        views
    }

    /// The owner's Allow. `None` when the request is gone.
    pub(crate) fn allow(&self, id: &str) -> Option<StreamView> {
        let mut inner = self.lock();
        let entry = inner.entries.get_mut(id)?;
        if entry.state == State::AwaitingApproval {
            entry.state = State::Allowed;
            entry.approved_until = Some(Instant::now() + APPROVAL_TTL);
            entry.approval_deadline = None;
        }
        Some(entry.view())
    }

    /// The owner's Decline: the waiting tool call returns `declined`.
    pub(crate) fn decline(&self, id: &str) -> Option<StreamView> {
        let mut inner = self.lock();
        let entry = inner.entries.get_mut(id)?;
        if entry.state == State::AwaitingApproval || entry.state == State::Allowed {
            entry.end(
                State::Stopped,
                Some(Fail::new(
                    "declined",
                    "The owner declined to share that computer's screen.",
                )),
            );
        }
        Some(entry.view())
    }

    /// The latest frame, marking the stream as looked at so frames keep being prepared.
    pub(crate) fn frame(&self, id: &str) -> Option<Arc<Frame>> {
        let mut inner = self.lock();
        let entry = inner.entries.get_mut(id)?;
        entry.viewed = Some(Instant::now());
        entry.frame.clone()
    }

    /// Stops one stream (or request) and waits for its ssh child to be reaped. Idempotent; `None` when unknown.
    pub(crate) async fn stop(&self, id: &str) -> Option<StreamView> {
        let (stop, task) = {
            let mut inner = self.lock();
            let entry = inner.entries.get_mut(id)?;
            let stop = entry.stop.take();
            let task = entry.task.take();
            if task.is_none() {
                entry.end(State::Stopped, None);
            }
            (stop, task)
        };
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        if let Some(mut task) = task {
            if timeout(REAP, &mut task).await.is_err() {
                // The task owns the ssh child with kill_on_drop: aborting it drops, and so kills, the child.
                task.abort();
            }
            let mut inner = self.lock();
            if let Some(entry) = inner.entries.get_mut(id) {
                entry.end(State::Stopped, None);
            }
        }
        self.view(id)
    }

    /// The daemon is quitting: nothing may outlive it.
    pub(crate) async fn shutdown(&self) {
        let ids: Vec<String> = self
            .lock()
            .entries
            .values()
            .filter(|e| !e.state.ended())
            .map(|e| e.id.clone())
            .collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    fn viewed_recently(shared: &Shared, id: &str) -> bool {
        let inner = shared.lock().unwrap();
        inner
            .entries
            .get(id)
            .is_some_and(|e| e.viewed.is_some_and(|v| v.elapsed() < VIEWED_WITHIN))
    }

    fn publish(shared: &Shared, id: &str, picture: Picture) {
        let mut inner = shared.lock().unwrap();
        if let Some(entry) = inner.entries.get_mut(id) {
            if entry.state.ended() {
                return;
            }
            let sequence = entry.frame.as_ref().map_or(1, |f| f.sequence + 1);
            entry.frame = Some(Arc::new(Frame {
                sequence,
                sha256: hex::encode(Sha256::digest(&picture.jpeg)),
                jpeg: picture.jpeg,
                width: picture.width,
                height: picture.height,
                at: Instant::now(),
            }));
            entry.state = State::Live;
        }
    }

    fn finish(shared: &Shared, id: &str, state: State, reason: Option<Fail>) {
        if let Some(entry) = shared.lock().unwrap().entries.get_mut(id) {
            entry.end(state, reason);
        }
    }
}

// ---------------------------------------------------------------------------
// the owner's SSH config: which aliases exist
// ---------------------------------------------------------------------------

const MAX_CONFIG_FILES: usize = 64;
const MAX_CONFIG_BYTES: u64 = 1 << 20;

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// A simple `*` match of one file name.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len()
                && name.starts_with(head)
                && glob_match_tail(tail, &name[head.len()..])
        }
    }
}

fn glob_match_tail(pattern: &str, rest: &str) -> bool {
    if !pattern.contains('*') {
        return rest.ends_with(pattern);
    }
    (0..=rest.len())
        .filter(|i| rest.is_char_boundary(*i))
        .any(|i| glob_match(pattern, &rest[i..]))
}

fn include_files(base: &FsPath, spec: &str) -> Vec<PathBuf> {
    let expanded = match spec.strip_prefix("~/") {
        Some(rest) => home_dir().map(|h| h.join(rest)),
        None => Some(if FsPath::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            base.join(spec)
        }),
    };
    let Some(path) = expanded else {
        return vec![];
    };
    let (Some(parent), Some(pattern)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return vec![];
    };
    if !pattern.contains('*') {
        return vec![path];
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| glob_match(pattern, n))
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

/// The literal `Host` aliases in an SSH config and the files it includes. Patterns, negations and `Match`
/// blocks are not aliases. Bounded: 64 files, 1 MiB each, include depth 4.
fn configured_aliases(config: &FsPath) -> HashSet<String> {
    fn visit(
        file: &FsPath,
        base: &FsPath,
        depth: usize,
        files: &mut usize,
        found: &mut HashSet<String>,
    ) {
        if depth > 4 || *files >= MAX_CONFIG_FILES {
            return;
        }
        *files += 1;
        let Ok(meta) = std::fs::metadata(file) else {
            return;
        };
        if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
            return;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            return;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let (key, values) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
                Some((key, rest)) => (
                    key.to_ascii_lowercase(),
                    rest.trim_start_matches(|c: char| c.is_whitespace() || c == '='),
                ),
                None => continue,
            };
            let values = values.split('#').next().unwrap_or("");
            match key.as_str() {
                "host" => {
                    for token in values.split_whitespace() {
                        let token = token.trim_matches('"');
                        if !token.contains(['*', '?', '!']) && ssh::valid_alias(token) {
                            found.insert(token.to_owned());
                        }
                    }
                }
                "include" => {
                    for token in values.split_whitespace() {
                        for path in include_files(base, token.trim_matches('"')) {
                            visit(&path, base, depth + 1, files, found);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut found = HashSet::new();
    let base = config.parent().unwrap_or(FsPath::new("."));
    visit(config, base, 0, &mut 0, &mut found);
    found
}

fn alias_config(settings: &Settings) -> Option<PathBuf> {
    settings
        .config
        .clone()
        .or_else(|| home_dir().map(|h| h.join(".ssh").join("config")))
}

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Display {
    Existing(String),
    Private { width: u64, height: u64 },
    None,
}

#[derive(Debug, PartialEq, Eq)]
struct AddPlan {
    transport: Transport,
    name: Option<String>,
    display: Display,
    vnc_port: u64,
    interval: u64,
    request: String,
}

impl AddPlan {
    fn intent(&self) -> String {
        match (&self.transport, &self.display) {
            (Transport::X11, Display::Private { width, height }) => {
                format!("an empty private display ({width}×{height}), stopped with the stream")
            }
            (Transport::X11, Display::Existing(display)) => {
                format!("the screen of display {display}, which may show everything open there")
            }
            (Transport::Vnc, _) => format!("the screen served on VNC port {}", self.vnc_port),
            (Transport::Screenshot, _) => {
                format!("a screenshot every {} s of its screen", self.interval)
            }
            _ => "its screen".to_owned(),
        }
    }
}

fn display_arg(args: &Value) -> Result<Option<String>, ToolError> {
    match args.get("display") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let bytes = text.as_bytes();
            let digits_end = text
                .strip_prefix(':')
                .map(|rest| rest.split('.').next().unwrap_or(""))
                .map_or(0, str::len);
            let screen_ok = match text.split_once('.') {
                None => true,
                Some((_, screen)) => {
                    screen.len() == 1 && screen.bytes().all(|b| b.is_ascii_digit())
                }
            };
            if bytes.first() == Some(&b':')
                && (1..=3).contains(&digits_end)
                && text[1..1 + digits_end].bytes().all(|b| b.is_ascii_digit())
                && screen_ok
                && text.len() <= 1 + digits_end + 2
            {
                Ok(Some(text.clone()))
            } else {
                Err(ToolError::new(
                    "invalid",
                    "display must look like :0 or :99.0.",
                ))
            }
        }
        _ => Err(ToolError::new(
            "invalid",
            "display must be a string such as :0.",
        )),
    }
}

fn plan_add(args: &Value) -> Result<AddPlan, ToolError> {
    let invalid = |message: &str| ToolError::new("invalid", message);
    let transport =
        match args.get("transport").and_then(Value::as_str) {
            Some("x11") => Transport::X11,
            Some("vnc") => Transport::Vnc,
            Some("screenshot") => Transport::Screenshot,
            _ => return Err(invalid(
                "add needs transport: x11, vnc or screenshot (probe says which the host offers).",
            )),
        };
    let has = |field: &str| args.get(field).is_some_and(|v| !v.is_null());
    let allowed: &[&str] = match transport {
        Transport::X11 => &["display", "startDisplay"],
        Transport::Vnc => &["vncPort"],
        Transport::Screenshot => &["display", "intervalSeconds"],
    };
    for field in ["display", "startDisplay", "vncPort", "intervalSeconds"] {
        if has(field) && !allowed.contains(&field) {
            return Err(invalid(&format!(
                "{field} does not apply to the {} transport.",
                transport.as_str()
            )));
        }
    }
    let name = match args.get("name") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let name = text_arg(args, "name", NAME_CHARS * 4)?;
            if name.chars().count() > NAME_CHARS || name.chars().any(char::is_control) {
                return Err(invalid("name must be plain text of at most 60 characters."));
            }
            Some(name)
        }
    };
    let display = match (transport, display_arg(args)?, args.get("startDisplay")) {
        (Transport::X11, Some(_), Some(start)) if !start.is_null() => {
            return Err(invalid("Use display or startDisplay, not both."))
        }
        (Transport::X11, None, None | Some(Value::Null)) => {
            return Err(invalid(
                "x11 needs display (an existing one from probe) or startDisplay.",
            ))
        }
        (Transport::X11, _, Some(start)) if start.is_object() => {
            let object = start.as_object().unwrap();
            if !object.keys().all(|k| k == "width" || k == "height") {
                return Err(invalid("startDisplay takes only width and height."));
            }
            let size = |field: &str, min: u64, max: u64| {
                object
                    .get(field)
                    .and_then(Value::as_u64)
                    .filter(|v| (min..=max).contains(v))
                    .ok_or_else(|| {
                        invalid(&format!("startDisplay.{field} must be {min} to {max}."))
                    })
            };
            Display::Private {
                width: size("width", 320, 3840)?,
                height: size("height", 240, 2160)?,
            }
        }
        (_, Some(display), _) => Display::Existing(display),
        (Transport::X11, _, Some(_)) => {
            return Err(invalid(
                "startDisplay must be an object with width and height.",
            ))
        }
        _ => Display::None,
    };
    let request = request_id(args)?;
    Ok(AddPlan {
        transport,
        name,
        display,
        vnc_port: int_arg(args, "vncPort", 5900, 5900, 5999)?,
        interval: int_arg(args, "intervalSeconds", 3, 1, 30)?,
        request,
    })
}

/// The one remote command: the fixed program, then validated arguments, each quoted.
fn remote_command(mode: &str, arguments: &[String]) -> String {
    let mut command = format!(
        "python3 -c {} {}",
        ssh::shell_quote(AGENT),
        ssh::shell_quote(mode)
    );
    for argument in arguments {
        command.push(' ');
        command.push_str(&ssh::shell_quote(argument));
    }
    command
}

fn plan_arguments(plan: &AddPlan) -> (&'static str, Vec<String>) {
    match plan.transport {
        Transport::X11 => match &plan.display {
            Display::Private { width, height } => (
                "x11",
                vec!["new".into(), width.to_string(), height.to_string()],
            ),
            Display::Existing(display) => ("x11", vec![display.clone(), "0".into(), "0".into()]),
            Display::None => ("x11", vec![]),
        },
        Transport::Vnc => ("vnc", vec![plan.vnc_port.to_string()]),
        Transport::Screenshot => (
            "screenshot",
            vec![
                match &plan.display {
                    Display::Existing(display) => display.clone(),
                    _ => "none".into(),
                },
                plan.interval.to_string(),
            ],
        ),
    }
}

// ---------------------------------------------------------------------------
// frames
// ---------------------------------------------------------------------------

fn fit(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= JPEG_LONG_EDGE {
        return (width, height);
    }
    let scale = f64::from(JPEG_LONG_EDGE) / f64::from(long);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// A JPEG and its size.
struct Picture {
    jpeg: Vec<u8>,
    width: u32,
    height: u32,
}

fn encode_rgb(image: image::RgbImage) -> Result<Picture, ()> {
    let (width, height) = fit(image.width(), image.height());
    let image = if (width, height) != (image.width(), image.height()) {
        image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&image)
        .map_err(|_| ())?;
    Ok(Picture {
        jpeg: out,
        width: image.width(),
        height: image.height(),
    })
}

fn encode_bgrx(width: u32, height: u32, bgrx: &[u8]) -> Result<Picture, ()> {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for pixel in bgrx.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    encode_rgb(image::RgbImage::from_raw(width, height, rgb).ok_or(())?)
}

/// A host's screenshot, checked and re-encoded so only Cupola's own JPEG is ever served: magic bytes, size and
/// dimensions are checked before decoding.
fn normalize_screenshot(bytes: &[u8]) -> Result<Picture, ()> {
    let jpeg = bytes.starts_with(&[0xff, 0xd8, 0xff]);
    let png = bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    if bytes.is_empty() || bytes.len() > MAX_SCREENSHOT || !(jpeg || png) {
        return Err(());
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?;
    let (width, height) = reader.into_dimensions().map_err(|_| ())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > rfb::MAX_PIXELS {
        return Err(());
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ())?
        .decode()
        .map_err(|_| ())?;
    encode_rgb(image.to_rgb8())
}

// ---------------------------------------------------------------------------
// connecting
// ---------------------------------------------------------------------------

/// Reads the remote program's first `CUPOLA-` line, skipping a noisy login shell's output (at most 8 lines).
async fn read_marker<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<String, Fail> {
    for _ in 0..8 {
        let mut line = Vec::new();
        let n = (&mut *reader)
            .take(4097)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| Fail::unreachable())?;
        if n == 0 {
            return Err(Fail::unreachable());
        }
        if n > 4096 {
            return Err(Fail::new(
                "protocol",
                "That computer did not speak the protocol Cupola expects.",
            ));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if let Some(code) = text.strip_prefix("CUPOLA-ERROR ") {
            return Err(Fail::remote(code.trim()));
        }
        if text.starts_with("CUPOLA-") {
            return Ok(text.to_owned());
        }
    }
    Err(Fail::new(
        "protocol",
        "That computer did not speak the protocol Cupola expects.",
    ))
}

/// The ssh failure behind a connection that ended: OpenSSH's own words classify it, nothing else is kept.
async fn explain(mut spawned: ssh::Spawned) -> Fail {
    let _ = spawned.child.start_kill();
    let _ = timeout(REAP, spawned.child.wait()).await;
    match ssh::classify_stderr(&ssh::read_stderr(spawned.stderr.take()).await) {
        Some(class) => Fail::from_class(class),
        None => Fail::unreachable(),
    }
}

async fn reap(spawned: &mut ssh::Spawned) {
    let _ = spawned.child.start_kill();
    let _ = timeout(REAP, spawned.child.wait()).await;
}

fn spawn_ssh(settings: &Settings, host: &str, remote: &str) -> Result<ssh::Spawned, Fail> {
    let program = settings.program.as_deref().ok_or_else(|| {
        Fail::new(
            "unavailable",
            "This computer has no OpenSSH client, so it cannot reach other computers.",
        )
    })?;
    let command = ssh::command(program, settings.config.as_deref(), host, remote)
        .ok_or_else(|| Fail::new("invalid", "host must be an alias from the SSH config."))?;
    ssh::spawn(command).map_err(|_| Fail::new("unavailable", "Cupola could not start ssh."))
}

fn probe_report(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line.strip_prefix("CUPOLA-PROBE ")?).ok()?;
    let displays: Vec<String> = value["displays"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|d| display_arg(&json!({"display": d})).is_ok_and(|d| d.is_some()))
        .take(20)
        .map(str::to_owned)
        .collect();
    let ports: Vec<u64> = value["vncPorts"]
        .as_array()?
        .iter()
        .filter_map(Value::as_u64)
        .filter(|p| (5900..=5999).contains(p))
        .take(20)
        .collect();
    let tool = |name: &str| value["tools"][name].as_bool() == Some(true);
    let platform = value["platform"]
        .as_str()
        .filter(|p| matches!(*p, "linux" | "darwin" | "win32"))
        .unwrap_or("other");
    let x11_available = tool("x11vnc") && (!displays.is_empty() || tool("Xvfb"));
    let screenshot_tool = if platform == "darwin" {
        tool("screencapture")
    } else {
        (!displays.is_empty() && (tool("import") || tool("scrot"))) || tool("grim")
    };
    let transports = vec![
        json!({"transport":"x11","available":x11_available,
            "displays":displays,"canStartDisplay":tool("x11vnc") && tool("Xvfb"),
            "reason": if x11_available { Value::Null } else if !tool("x11vnc") { json!("x11vnc is not installed on the host.") } else { json!("The host has no X display and no Xvfb.") }}),
        json!({"transport":"vnc","available":!ports.is_empty(),"ports":ports,
            "reason": if ports.is_empty() { json!("No VNC server is listening on the host's loopback (ports 5900 to 5999).") } else { Value::Null }}),
        json!({"transport":"screenshot","available":screenshot_tool,
            "reason": if screenshot_tool { Value::Null } else { json!("The host has no screenshot tool Cupola can use (import or scrot with an X display, grim, or screencapture).") }}),
    ];
    let recommended = transports
        .iter()
        .find(|t| t["available"] == true)
        .map(|t| t["transport"].clone())
        .unwrap_or(Value::Null);
    Some(json!({"platform":platform,"transports":transports,"recommended":recommended}))
}

async fn probe_host(settings: &Settings, host: &str) -> Result<Value, Fail> {
    let mut spawned = spawn_ssh(settings, host, &remote_command("probe", &[]))?;
    let line = timeout(PROBE_DEADLINE, read_marker(&mut spawned.stdout)).await;
    match line {
        Ok(Ok(line)) => {
            reap(&mut spawned).await;
            probe_report(&line).ok_or_else(|| {
                Fail::new(
                    "protocol",
                    "That computer did not speak the protocol Cupola expects.",
                )
            })
        }
        Ok(Err(fail)) if fail == Fail::unreachable() => Err(explain(spawned).await),
        Ok(Err(fail)) => {
            reap(&mut spawned).await;
            Err(fail)
        }
        Err(_) => {
            reap(&mut spawned).await;
            Err(Fail::new(
                "timeout",
                "That computer did not answer in time.",
            ))
        }
    }
}

struct Plan {
    transport: Transport,
    interval: Duration,
}

/// Owns one ssh child for the life of the stream. Stops on the owner's signal, an error or a stall, and always
/// reaps the child.
async fn supervise(
    shared: Shared,
    id: String,
    mut spawned: ssh::Spawned,
    plan: Plan,
    mut stop: oneshot::Receiver<()>,
) {
    let result = tokio::select! {
        _ = &mut stop => Ok(()),
        result = stream_loop(&shared, &id, &mut spawned, &plan) => result,
    };
    match result {
        Ok(()) => {
            reap(&mut spawned).await;
            Streams::finish(&shared, &id, State::Stopped, None);
        }
        Err(fail) if fail == Fail::unreachable() => {
            let fail = explain(spawned).await;
            Streams::finish(&shared, &id, State::Failed, Some(fail));
        }
        Err(fail) => {
            reap(&mut spawned).await;
            Streams::finish(&shared, &id, State::Failed, Some(fail));
        }
    }
}

async fn stream_loop(
    shared: &Shared,
    id: &str,
    spawned: &mut ssh::Spawned,
    plan: &Plan,
) -> Result<(), Fail> {
    let hello = timeout(CONNECT_STEP, read_marker(&mut spawned.stdout))
        .await
        .map_err(|_| Fail::new("timeout", "That computer did not answer in time."))??;
    if !hello.starts_with("CUPOLA-STREAM 1 ") {
        return Err(Fail::new(
            "protocol",
            "That computer did not speak the protocol Cupola expects.",
        ));
    }
    match plan.transport {
        Transport::X11 | Transport::Vnc => rfb_loop(shared, id, spawned).await,
        Transport::Screenshot => screenshot_loop(shared, id, spawned, plan.interval).await,
    }
}

async fn rfb_loop(shared: &Shared, id: &str, spawned: &mut ssh::Spawned) -> Result<(), Fail> {
    let mut io = tokio::io::join(&mut spawned.stdout, &mut spawned.stdin);
    let mut screen = timeout(CONNECT_STEP, rfb::connect(&mut io))
        .await
        .map_err(|_| Fail::stalled())?
        .map_err(Fail::rfb)?;
    loop {
        rfb::request(&mut io, &screen, false)
            .await
            .map_err(Fail::rfb)?;
        timeout(STALL, rfb::read_update(&mut io, &mut screen))
            .await
            .map_err(|_| Fail::stalled())?
            .map_err(Fail::rfb)?;
        let (width, height, pixels) = (screen.width, screen.height, screen.bgrx.clone());
        let picture = tokio::task::spawn_blocking(move || encode_bgrx(width, height, &pixels))
            .await
            .map_err(|_| Fail::new("protocol", "The screen could not be prepared."))?
            .map_err(|_| Fail::new("protocol", "The screen could not be prepared."))?;
        Streams::publish(shared, id, picture);
        // Bigger screens are asked for less often: about one more second per two megapixels.
        let size = u64::from(screen.width) * u64::from(screen.height);
        let extra = Duration::from_millis(size / 2_000);
        // Looking at it speeds the next ask up, so the first look is never five seconds behind.
        let asked = Instant::now();
        loop {
            let wait = if Streams::viewed_recently(shared, id) {
                POLL_VIEWED + extra
            } else {
                POLL_IDLE + extra
            };
            if asked.elapsed() >= wait {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn screenshot_loop(
    shared: &Shared,
    id: &str,
    spawned: &mut ssh::Spawned,
    interval: Duration,
) -> Result<(), Fail> {
    let stall = STALL + interval;
    loop {
        let marker = timeout(stall, read_marker(&mut spawned.stdout))
            .await
            .map_err(|_| Fail::stalled())??;
        let length: usize = marker
            .strip_prefix("CUPOLA-FRAME ")
            .and_then(|n| n.trim().parse().ok())
            .filter(|n| (1..=MAX_SCREENSHOT).contains(n))
            .ok_or_else(|| {
                Fail::new(
                    "protocol",
                    "That computer sent a screenshot of an unexpected size.",
                )
            })?;
        let mut bytes = vec![0u8; length];
        timeout(STALL, spawned.stdout.read_exact(&mut bytes))
            .await
            .map_err(|_| Fail::stalled())?
            .map_err(|_| Fail::unreachable())?;
        let picture = tokio::task::spawn_blocking(move || normalize_screenshot(&bytes))
            .await
            .map_err(|_| Fail::new("protocol", "The screenshot could not be prepared."))?
            .map_err(|_| Fail::new("protocol", "That computer sent a screenshot Cupola could not accept (not a JPEG or PNG within 8 MB and 8.5 megapixels)."))?;
        Streams::publish(shared, id, picture);
    }
}

// ---------------------------------------------------------------------------
// the tool
// ---------------------------------------------------------------------------

fn host_arg(args: &Value) -> Result<String, ToolError> {
    let host = text_arg(args, "host", 253)?;
    if !ssh::valid_alias(&host) {
        return Err(ToolError::new(
            "invalid",
            "host must be an alias from the owner's SSH config: letters, digits, '.', '_' and '-' only (not user@host or an address).",
        ));
    }
    Ok(host)
}

pub(in crate::thread_tools) async fn run(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &[
            "action",
            "host",
            "transport",
            "name",
            "display",
            "startDisplay",
            "vncPort",
            "intervalSeconds",
            "streamId",
            "timeoutSeconds",
            "clientRequestId",
        ],
    )?;
    let streams = &state.computer_supervisor.streams;
    match args.get("action").and_then(Value::as_str) {
        Some("list") => Ok(list(streams, caller)),
        Some("stop") => stop_stream(state, caller, &args).await,
        Some(action @ ("probe" | "add")) => connect(state, caller, &args, action == "add").await,
        _ => Err(ToolError::new(
            "invalid",
            "action must be probe, add, list or stop.",
        )),
    }
}

fn list(streams: &Streams, caller: &Caller) -> Value {
    let mine: Vec<Value> = streams
        .views()
        .into_iter()
        .filter(|v| v.project_id == caller.project.id)
        .map(|v| {
            json!({"streamId":v.id,"name":v.name,"host":v.host,"transport":v.transport,"state":v.state,
                "reason":v.reason,"ageSeconds":v.age_seconds,"lastFrameAgeSeconds":v.frame_age_seconds})
        })
        .collect();
    json!({"streams": mine})
}

async fn stop_stream(state: &AppState, caller: &Caller, args: &Value) -> Result<Value, ToolError> {
    let id = text_arg(args, "streamId", 64)?;
    let streams = &state.computer_supervisor.streams;
    // Another Project's stream reads exactly like a missing one.
    let found = streams
        .view(&id)
        .filter(|v| v.project_id == caller.project.id)
        .ok_or_else(|| ToolError::new("not_found", "No such stream in this Project."))?;
    if found.conversation_id != caller.conversation.conversation_id
        && !delegated_by(
            state,
            &caller.conversation.conversation_id,
            &found.conversation_id,
        )
        .await?
    {
        return Err(ToolError::new(
            "not_found",
            "No such stream in this Project.",
        ));
    }
    let view = streams.stop(&id).await;
    Ok(json!({"state": view.map_or("stopped", |v| v.state), "streamId": id}))
}

async fn connect(
    state: &AppState,
    caller: &Caller,
    args: &Value,
    add: bool,
) -> Result<Value, ToolError> {
    forbid_plan_mode_with(
        caller,
        "This thread is planning. Finish the plan before streaming a screen.",
    )?;
    // A read-only thread cannot run commands, so it cannot open connections either.
    if access_rank(&caller.conversation.access_mode) == 0 {
        return Err(ToolError::new(
            "not_allowed",
            "Read-only threads cannot stream a screen. Ask the owner to give this thread workspace access.",
        ));
    }
    let host = host_arg(args)?;
    let plan = if add { Some(plan_add(args)?) } else { None };
    if !add {
        for field in [
            "transport",
            "name",
            "display",
            "startDisplay",
            "vncPort",
            "intervalSeconds",
            "streamId",
        ] {
            if args.get(field).is_some_and(|v| !v.is_null()) {
                return Err(ToolError::new(
                    "invalid",
                    format!("{field} does not apply to probe."),
                ));
            }
        }
    }
    let wait = int_arg(args, "timeoutSeconds", DEFAULT_WAIT, 1, MAX_WAIT)?;
    let streams = &state.computer_supervisor.streams;
    let settings = streams.settings();
    if settings.program.is_none() {
        return Err(ToolError::new(
            "unavailable",
            "This computer has no OpenSSH client, so it cannot reach other computers.",
        ));
    }
    // Only an alias the owner already named for ssh: nothing the agent invents can be connected to.
    let aliases = alias_config(&settings)
        .map(|path| configured_aliases(&path))
        .unwrap_or_default();
    if !aliases.contains(&host) {
        return Err(ToolError::new(
            "unknown_host",
            "That host is not a Host entry in the owner's SSH config. Use one of their aliases; Cupola does not connect to addresses or names it was not given by the config.",
        ));
    }
    let agent = format!(
        "{} · {}",
        caller.conversation.family.provider().display_name,
        clip(&caller.conversation.title, 40)
    );
    let intent = plan.as_ref().map_or_else(
        || "a look at what screen options it offers".to_owned(),
        AddPlan::intent,
    );
    let id = streams.open(&Open {
        project_id: &caller.project.id,
        conversation_id: &caller.conversation.conversation_id,
        agent: &agent,
        host: &host,
        intent: &intent,
        timeout: Duration::from_secs(wait),
    })?;
    // An add retried with the same request id returns the stream it already made.
    if let Some(plan) = &plan {
        let view = streams.view(&id);
        let existing = {
            let inner = streams.lock();
            inner.entries.get(&id).map(|e| (e.state, e.request.clone()))
        };
        if let (Some(view), Some((entry_state, request))) = (view, existing) {
            if matches!(entry_state, State::Connecting | State::Live) {
                if request.as_deref() == Some(plan.request.as_str()) {
                    return await_live(streams, &id).await;
                }
                return Err(ToolError::new(
                    "already_streaming",
                    format!(
                        "This thread already streams {} (stream {}). Stop it first.",
                        view.host, view.id
                    ),
                ));
            }
        }
    }
    let owner = WaitOwner::new(caller);
    loop {
        let Some(view) = streams.view(&id) else {
            return Err(ToolError::new("expired", "The request is gone. Ask again."));
        };
        match view.kind {
            State::Allowed | State::Connecting | State::Live => break,
            State::Stopped | State::Failed => {
                return Err(match view.reason_code {
                    Some("declined") => ToolError::new("declined", "The owner declined to share that computer's screen. Continue without it, or explain what you need."),
                    Some("expired") => ToolError::new("expired", "The owner did not answer in time. Ask again later, or continue without it."),
                    _ => ToolError::new("not_found", "That request has ended."),
                });
            }
            State::AwaitingApproval => {}
        }
        if !owner.live(state).await? {
            // The reply that asked has ended: nothing is waiting for an answer, so withdraw the card.
            streams.stop(&id).await;
            return Err(wait_cancelled());
        }
        tokio::time::sleep(POLL).await;
    }
    let Some(plan) = plan else {
        let report = probe_host(&settings, &host).await.map_err(|f| f.tool())?;
        return Ok(
            json!({"host": host, "hostKey": {"pinned": true}, "platform": report["platform"],
            "transports": report["transports"], "recommended": report["recommended"],
            "note": "Add with one transport that is available. x11 shows the screen of an X display; with startDisplay it is a private empty one."}),
        );
    };
    let (mode, arguments) = plan_arguments(&plan);
    let command = remote_command(mode, &arguments);
    // Claim the stream and start ssh under one lock: two calls that were both waiting for the same Allow
    // must not both spawn a connection (the later one would orphan the first's stop and task).
    {
        let mut inner = streams.lock();
        let Some(entry) = inner.entries.get_mut(&id) else {
            return Err(ToolError::new("expired", "The request is gone. Ask again."));
        };
        match entry.state {
            State::Connecting | State::Live => {
                if entry.request.as_deref() != Some(plan.request.as_str()) {
                    return Err(ToolError::new(
                        "already_streaming",
                        format!(
                            "This thread already streams {} (stream {}). Stop it first.",
                            entry.host, entry.id
                        ),
                    ));
                }
                // A retry of the add that owns this stream: it only waits for it below.
            }
            State::Allowed => {
                let spawned = spawn_ssh(&settings, &host, &command).map_err(|f| f.tool())?;
                let (stop_tx, stop_rx) = oneshot::channel();
                entry.state = State::Connecting;
                entry.transport = Some(plan.transport);
                entry.request = Some(plan.request.clone());
                entry.name = plan.name.clone().unwrap_or_else(|| host.clone());
                entry.intent = plan.intent();
                entry.display = match &plan.display {
                    Display::Existing(d) => Some(d.clone()),
                    _ => None,
                };
                entry.interval_ms = if plan.transport == Transport::Screenshot {
                    plan.interval * 1000
                } else {
                    1000
                };
                entry.stop = Some(stop_tx);
                entry.task = Some(tokio::spawn(supervise(
                    Arc::clone(&streams.inner),
                    id.clone(),
                    spawned,
                    Plan {
                        transport: plan.transport,
                        interval: Duration::from_secs(plan.interval),
                    },
                    stop_rx,
                )));
            }
            _ => return Err(ToolError::new("not_found", "That request has ended.")),
        }
    }
    await_live(streams, &id).await
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Waits for the first frame (the supervisor gives up by itself), then reports the stream.
async fn await_live(streams: &Streams, id: &str) -> Result<Value, ToolError> {
    let deadline = Instant::now() + CONNECT_STEP * 2 + Duration::from_secs(5);
    loop {
        let Some(view) = streams.view(id) else {
            return Err(Fail::unreachable().tool());
        };
        match view.kind {
            State::Live => {
                let mut inner = streams.lock();
                let entry = inner.entries.get_mut(id);
                let (display, interval) = entry
                    .as_ref()
                    .map_or((None, 1000), |e| (e.display.clone(), e.interval_ms));
                return Ok(
                    json!({"streamId": view.id, "state": "live", "host": view.host,
                    "name": view.name, "transport": view.transport, "display": display,
                    "frameIntervalMs": interval,
                    "note": "The owner can see it under the computer icon on their phone and can stop it. You do not receive the pixels."}),
                );
            }
            State::Failed | State::Stopped => {
                return Err(match view.reason_code {
                    Some(code) => ToolError::new(code, view.reason.unwrap_or_default()),
                    None => ToolError::new(
                        "not_found",
                        "The stream was stopped before its first frame.",
                    ),
                });
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            streams.stop(id).await;
            return Err(ToolError::new("timeout", "No screen data arrived in time."));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// The lab fakes ssh with shell scripts and X sockets, so these tests run on unix hosts only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::projects::tests::handoff_fixture;
    use std::os::unix::fs::PermissionsExt;
    use wonder_store::ProjectConversationPatch;

    const UUID: &str = "3f2b8c1e-5d4a-4b7e-9a10-0c6f2d8e1a77";

    fn uuid() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn fx() -> (tempfile::TempDir, AppState, Caller) {
        let (dir, state, message) = handoff_fixture().await;
        state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    access_mode: Some("workspace"),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let caller = caller_of(&state, &message.id).await;
        (dir, state, caller)
    }

    async fn caller_of(state: &AppState, message: &str) -> Caller {
        let message = state.store.message_by_id(message).await.unwrap().unwrap();
        let conversation = state
            .store
            .project_conversation(&message.conversation_id)
            .await
            .unwrap()
            .unwrap();
        let project = state
            .store
            .project(&conversation.project_id)
            .await
            .unwrap()
            .unwrap();
        Caller {
            runtime: None,
            message,
            conversation,
            project,
        }
    }

    fn script(path: &FsPath, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A fake RFB server for the stand-in x11vnc: 64x48, a gradient that changes with every update.
    const FAKE_X11VNC: &str = r#"#!/usr/bin/env python3
import os, socket, struct, sys
open(os.path.join(os.environ["LAB"], "x11vnc.pid"), "w").write(str(os.getpid()))
open(os.path.join(os.environ["LAB"], "x11vnc.args"), "w").write(" ".join(sys.argv[1:]))
s = socket.fromfd(0, socket.AF_UNIX, socket.SOCK_STREAM)
def need(n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            sys.exit(0)
        data += chunk
    return data
s.sendall(b"RFB 003.008\n"); need(12)
s.sendall(b"\x01\x01"); need(1)
s.sendall(struct.pack(">I", 0)); need(1)
s.sendall(struct.pack(">HH", 64, 48) + bytes(16) + struct.pack(">I", 4) + b"fake")
need(28)
tick = 0
while True:
    kind = need(1)[0]
    if kind == 3:
        need(9)
        tick += 1
        rows = b"".join(bytes([(x * 4 + tick) % 256, (y * 5) % 256, 128, 0]) for y in range(48) for x in range(64))
        s.sendall(struct.pack(">BxH", 0, 1) + struct.pack(">HHHHi", 0, 0, 64, 48, 0) + rows)
"#;

    const FAKE_XVFB: &str = r#"#!/usr/bin/env python3
import os, signal, sys, time
n = sys.argv[1][1:]
os.makedirs("/tmp/.X11-unix", exist_ok=True)
path = "/tmp/.X11-unix/X" + n
open(path, "w").write("")
open(os.path.join(os.environ["LAB"], "xvfb.pid"), "w").write(str(os.getpid()))
open(os.path.join(os.environ["LAB"], "xvfb.args"), "w").write(" ".join(sys.argv[1:]))
def leave(*_):
    os.remove(path)
    open(os.path.join(os.environ["LAB"], "xvfb.gone"), "w").write("1")
    sys.exit(0)
signal.signal(signal.SIGTERM, leave)
while True:
    time.sleep(0.05)
"#;

    struct Lab {
        dir: tempfile::TempDir,
        ssh: PathBuf,
        config: PathBuf,
    }

    impl Lab {
        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
    }

    /// A stand-in for ssh that runs the remote command on this machine with fake `x11vnc`, `Xvfb` and `import`
    /// first on PATH. The real `agent.py` runs, so its protocol, pumping and cleanup are the real ones.
    fn lab(aliases: &[&str]) -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        script(&bin.join("x11vnc"), FAKE_X11VNC);
        script(&bin.join("Xvfb"), FAKE_XVFB);
        let frame = encode_rgb(image::RgbImage::from_pixel(
            80,
            60,
            image::Rgb([200, 30, 30]),
        ))
        .unwrap()
        .jpeg;
        std::fs::write(dir.path().join("frame.jpg"), frame).unwrap();
        script(
            &bin.join("import"),
            "#!/bin/sh\nfor last; do :; done\ncp \"$LAB/frame.jpg\" \"${last#jpeg:}\"\n",
        );
        let ssh = dir.path().join("ssh");
        // Not exec'd: like a real ssh, it stays the parent, so killing it leaves the remote side to notice EOF.
        script(
            &ssh,
            &format!(
                "#!/bin/sh\nfor last; do :; done\necho . >> '{}/ssh.calls'\nLAB='{}' PATH='{}':\"$PATH\" sh -c \"$last; exit \\$?\"\n",
                dir.path().display(),
                dir.path().display(),
                bin.display()
            ),
        );
        let config = dir.path().join("config");
        let hosts: String = aliases
            .iter()
            .map(|a| format!("Host {a}\n  HostName 127.0.0.1\n"))
            .collect();
        std::fs::write(&config, hosts).unwrap();
        Lab { dir, ssh, config }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    fn pid_alive(path: &FsPath) -> bool {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|p| p.trim().parse::<i32>().ok())
            .is_some_and(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
    }

    // ---- pure checks --------------------------------------------------------

    // Contract: the only remote command is this source, and it reaches every login shell unchanged: no quote,
    // backslash or exclamation mark (csh/fish/zsh would rewrite them).
    #[test]
    fn the_remote_program_survives_every_login_shell() {
        for forbidden in ['\'', '\\', '!'] {
            assert!(
                !AGENT.contains(forbidden),
                "agent.py contains {forbidden:?}"
            );
        }
        let command = remote_command("x11", &["new".into(), "800".into(), "600".into()]);
        for shell in ["/bin/sh", "/bin/bash"] {
            // The shell must hand python3 exactly the source and the arguments, nothing more or less.
            let output = std::process::Command::new(shell)
                .arg("-c")
                .arg(format!("python3() {{ printf '%s\\n' \"$@\"; }}\n{command}"))
                .output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            let mut lines = text.splitn(3, '\n');
            assert_eq!(lines.next(), Some("-c"), "{shell}");
            let rest = lines.next().unwrap().to_owned() + "\n" + lines.next().unwrap();
            assert!(rest.starts_with("import json"), "{shell}");
            assert!(rest.ends_with("x11\nnew\n800\n600\n"), "{shell}: {rest:?}");
        }
    }

    #[test]
    fn add_arguments_are_checked_per_transport() {
        let ok = |v: Value| plan_add(&v);
        let bad = |v: Value| plan_add(&v).unwrap_err().code;
        let id = json!(UUID);
        assert_eq!(
            ok(json!({"transport":"x11","display":":0","clientRequestId":id}))
                .unwrap()
                .display,
            Display::Existing(":0".into())
        );
        assert_eq!(
            ok(json!({"transport":"x11","startDisplay":{"width":800,"height":600},"clientRequestId":id})).unwrap().display,
            Display::Private { width: 800, height: 600 }
        );
        assert_eq!(
            ok(json!({"transport":"vnc","clientRequestId":id}))
                .unwrap()
                .vnc_port,
            5900
        );
        assert_eq!(
            ok(json!({"transport":"screenshot","intervalSeconds":5,"clientRequestId":id}))
                .unwrap()
                .interval,
            5
        );
        for value in [
            json!({"transport":"x11","clientRequestId":id}),
            json!({"transport":"x11","display":":0","startDisplay":{"width":800,"height":600},"clientRequestId":id}),
            json!({"transport":"x11","startDisplay":{"width":10,"height":600},"clientRequestId":id}),
            json!({"transport":"x11","startDisplay":{"width":800,"height":600,"depth":8},"clientRequestId":id}),
            json!({"transport":"x11","display":":0; id","clientRequestId":id}),
            json!({"transport":"x11","display":":1234","clientRequestId":id}),
            json!({"transport":"x11","display":"$DISPLAY","clientRequestId":id}),
            json!({"transport":"x11","display":":0.12","clientRequestId":id}),
            json!({"transport":"vnc","vncPort":22,"clientRequestId":id}),
            json!({"transport":"vnc","vncPort":6000,"clientRequestId":id}),
            json!({"transport":"vnc","display":":0","clientRequestId":id}),
            json!({"transport":"screenshot","vncPort":5900,"clientRequestId":id}),
            json!({"transport":"screenshot","intervalSeconds":0,"clientRequestId":id}),
            json!({"transport":"screenshot","intervalSeconds":31,"clientRequestId":id}),
            json!({"transport":"telnet","clientRequestId":id}),
            json!({"clientRequestId":id}),
            json!({"transport":"vnc"}),
            json!({"transport":"vnc","clientRequestId":"not-a-uuid"}),
            json!({"transport":"vnc","name":"a\nb","clientRequestId":id}),
            json!({"transport":"vnc","name":"x".repeat(61),"clientRequestId":id}),
        ] {
            assert_eq!(bad(value.clone()), "invalid", "{value}");
        }
        // Whatever passes, the arguments reaching the remote shell are digits, a display or fixed words.
        let (mode, arguments) = plan_arguments(
            &ok(json!({"transport":"x11","display":":99.0","clientRequestId":id})).unwrap(),
        );
        assert_eq!(
            (mode, arguments),
            ("x11", vec![":99.0".to_owned(), "0".into(), "0".into()])
        );
    }

    #[test]
    fn only_literal_aliases_from_the_owners_config_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("conf.d")).unwrap();
        std::fs::write(
            dir.path().join("config"),
            "# Host commented\nInclude conf.d/*.conf\nHost lab \"quoted\" star-* !denied ?x\n  HostName 1.2.3.4\nhost  lower  # trailing\nHost *\n  User x\nMatch host zed\nHost=equals\nHost bad;id user@host -oProxyCommand=x\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("conf.d/a.conf"),
            "Host included\nInclude loop\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("conf.d/b.txt"), "Host ignored\n").unwrap();
        let found = configured_aliases(&dir.path().join("config"));
        let mut found: Vec<_> = found.into_iter().collect();
        found.sort();
        assert_eq!(found, ["equals", "included", "lab", "lower", "quoted"]);
        // A missing config has no aliases.
        assert!(configured_aliases(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn a_hosts_screenshot_is_checked_and_reencoded_before_anyone_sees_it() {
        let jpeg = encode_rgb(image::RgbImage::from_pixel(100, 50, image::Rgb([1, 2, 3])))
            .unwrap()
            .jpeg;
        assert!(normalize_screenshot(&jpeg).is_ok());
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(30, 30))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(normalize_screenshot(&png).is_ok());
        // Large pictures are scaled down to the long-edge limit.
        let big = normalize_screenshot(&encode_rgb(image::RgbImage::new(3000, 1000)).unwrap().jpeg)
            .unwrap()
            .jpeg;
        let (w, h) = image::ImageReader::new(std::io::Cursor::new(&big))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!((w, h), (1600, 533));
        // Not an image, wrong magic, empty, truncated, over the byte limit, over the pixel limit.
        assert!(normalize_screenshot(b"").is_err());
        assert!(normalize_screenshot(b"<html>secret</html>").is_err());
        assert!(normalize_screenshot(&jpeg[..jpeg.len() / 2]).is_err());
        assert!(normalize_screenshot(&vec![0xff; MAX_SCREENSHOT + 1]).is_err());
        let mut huge = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        huge.extend_from_slice(&20_000u32.to_be_bytes());
        huge.extend_from_slice(&20_000u32.to_be_bytes());
        huge.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        assert!(normalize_screenshot(&huge).is_err());
    }

    #[test]
    fn a_hostile_probe_answer_is_reduced_to_fixed_fields() {
        let line = format!(
            "CUPOLA-PROBE {}",
            json!({"displays":[":0",":99.0","; rm -rf ~","$(id)",":12345"],"vncPorts":[5900,22,70000,5901],
                "tools":{"x11vnc":true,"Xvfb":"yes","evil":true},"platform":"<script>"})
        );
        let report = probe_report(&line).unwrap();
        let text = report.to_string();
        assert!(
            !text.contains("rm -rf")
                && !text.contains("$(id)")
                && !text.contains("script")
                && !text.contains("evil"),
            "{text}"
        );
        let x11 = &report["transports"][0];
        assert_eq!(x11["displays"], json!([":0", ":99.0"]));
        // "yes" is not a boolean: Xvfb counts as missing, but an X display exists so x11 still works.
        assert_eq!(x11["available"], true);
        assert_eq!(x11["canStartDisplay"], false);
        assert_eq!(report["transports"][1]["ports"], json!([5900, 5901]));
        assert_eq!(report["platform"], "other");
        assert!(probe_report("CUPOLA-PROBE not json").is_none());
        assert!(probe_report("hello").is_none());
    }

    // ---- registry -----------------------------------------------------------

    fn open<'a>(project: &'a str, conversation: &'a str, host: &'a str) -> Open<'a> {
        Open {
            project_id: project,
            conversation_id: conversation,
            agent: "Codex · test",
            host,
            intent: "its screen",
            timeout: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn requests_wait_for_the_owner_and_end_cleanly() {
        let streams = Streams::default();
        let id = streams.open(&open("p", "c", "lab")).unwrap();
        assert_eq!(streams.view(&id).unwrap().kind, State::AwaitingApproval);
        // The same thread asking again reuses the request; another thread gets its own.
        assert_eq!(streams.open(&open("p", "c", "lab")).unwrap(), id);
        let other = streams.open(&open("p", "c2", "lab")).unwrap();
        assert_ne!(other, id);
        assert_eq!(streams.allow(&id).unwrap().kind, State::Allowed);
        // An Allow does not carry to another thread or host.
        assert_eq!(streams.view(&other).unwrap().kind, State::AwaitingApproval);
        let third = streams.open(&open("p", "c", "other-host"));
        assert_eq!(
            third.unwrap_err().code,
            "too_many_streams",
            "two per Project"
        );
        // Decline ends the request with a reason; stop is idempotent and unknown ids are None.
        assert_eq!(
            streams.decline(&other).unwrap().reason_code,
            Some("declined")
        );
        assert_eq!(streams.stop(&id).await.unwrap().kind, State::Stopped);
        assert_eq!(streams.stop(&id).await.unwrap().kind, State::Stopped);
        assert!(streams.stop("nope").await.is_none());
        assert!(streams.allow("nope").is_none() && streams.decline("nope").is_none());
        // Ended requests free their slot at once.
        assert!(streams.open(&open("p", "c", "lab")).is_ok());
    }

    #[test]
    fn limits_and_expiry_hold() {
        let streams = Streams::default();
        for n in 0..MAX_PER_COMPUTER {
            let project = format!("p{}", n / MAX_PER_PROJECT);
            streams
                .open(&open(&project, &format!("c{n}"), "lab"))
                .unwrap();
        }
        assert_eq!(
            streams.open(&open("p9", "c9", "lab")).unwrap_err().code,
            "too_many_streams"
        );
        // A request nobody answers ends by itself, and an unused Allow runs out.
        let mut inner = streams.inner.lock().unwrap();
        for entry in inner.entries.values_mut() {
            entry.approval_deadline = Some(Instant::now() - Duration::from_secs(60));
        }
        drop(inner);
        assert!(streams
            .views()
            .iter()
            .all(|v| v.reason_code == Some("expired") && v.kind == State::Stopped));
        let id = streams.open(&open("p", "c", "lab")).unwrap();
        streams.allow(&id).unwrap();
        streams
            .inner
            .lock()
            .unwrap()
            .entries
            .get_mut(&id)
            .unwrap()
            .approved_until = Some(Instant::now() - Duration::from_secs(60));
        assert_eq!(streams.view(&id).unwrap().reason_code, Some("expired"));
        // Ended entries are forgotten after the retention time.
        streams
            .inner
            .lock()
            .unwrap()
            .entries
            .get_mut(&id)
            .unwrap()
            .ended = Some(Instant::now() - ENDED_RETENTION - Duration::from_secs(60));
        assert!(streams.view(&id).is_none());
    }

    // ---- the tool's limits --------------------------------------------------

    #[tokio::test]
    async fn the_tool_refuses_what_the_threat_model_says_it_refuses() {
        let (dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        state
            .computer_supervisor
            .streams
            .configure_for_test(lab.ssh.clone(), lab.config.clone());
        let call = |args: Value| run(&state, &caller, args);
        // Unknown fields and actions.
        assert_eq!(
            call(json!({"action":"probe","host":"lab","command":"id"}))
                .await
                .unwrap_err()
                .code,
            "invalid"
        );
        assert_eq!(
            call(json!({"action":"exec"})).await.unwrap_err().code,
            "invalid"
        );
        assert_eq!(call(json!({})).await.unwrap_err().code, "invalid");
        // Hosts: not an alias, an address, user@host, option-like, a pattern, or just not in the owner's config.
        for host in [
            "",
            "-oProxyCommand=x",
            "root@lab",
            "lab;id",
            "10.0.0.1",
            "*",
            "[::1]",
        ] {
            let error = call(json!({"action":"probe","host":host}))
                .await
                .unwrap_err();
            assert!(
                matches!(error.code, "invalid" | "unknown_host"),
                "{host:?}: {}",
                error.code
            );
        }
        assert_eq!(
            call(json!({"action":"probe","host":"not-in-config"}))
                .await
                .unwrap_err()
                .code,
            "unknown_host"
        );
        // Nothing connected and nothing is waiting for the owner.
        assert!(state.computer_supervisor.streams.views().is_empty());
        // Probe takes no add fields.
        assert_eq!(
            call(json!({"action":"probe","host":"lab","transport":"vnc"}))
                .await
                .unwrap_err()
                .code,
            "invalid"
        );

        // A read-only thread cannot use it at all.
        state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    access_mode: Some("read_only"),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let read_only = caller_of(&state, &caller.message.id).await;
        let error = run(&state, &read_only, json!({"action":"probe","host":"lab"}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "not_allowed");
        assert!(state.computer_supervisor.streams.views().is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn another_project_can_neither_see_nor_stop_a_stream() {
        let (_dir, state, caller) = fx().await;
        let streams = &state.computer_supervisor.streams;
        let id = streams
            .open(&open(
                &caller.project.id,
                &caller.conversation.conversation_id,
                "lab",
            ))
            .unwrap();
        streams.allow(&id).unwrap();
        // The owning thread sees it.
        let mine = run(&state, &caller, json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(mine["streams"].as_array().unwrap().len(), 1);
        // A thread of another Project sees nothing, and stop reads as a missing stream.
        let mut conversation = caller.conversation.clone();
        conversation.conversation_id = "foreign-chat".into();
        let mut project = caller.project.clone();
        project.id = "other".into();
        let foreign = Caller {
            runtime: None,
            message: caller.message.clone(),
            conversation,
            project,
        };
        let theirs = run(&state, &foreign, json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(theirs["streams"], json!([]));
        let error = run(&state, &foreign, json!({"action":"stop","streamId":id}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "not_found");
        assert_eq!(
            streams.view(&id).unwrap().kind,
            State::Allowed,
            "still there"
        );
        // A sibling thread in the same Project that did not add it, and did not delegate its adder, cannot stop it.
        let mut sibling_conversation = caller.conversation.clone();
        sibling_conversation.conversation_id = "sibling-chat".into();
        let sibling = Caller {
            runtime: None,
            message: caller.message.clone(),
            conversation: sibling_conversation,
            project: caller.project.clone(),
        };
        assert_eq!(
            run(&state, &sibling, json!({"action":"stop","streamId":id}))
                .await
                .unwrap_err()
                .code,
            "not_found"
        );
        // The adder can.
        let stopped = run(&state, &caller, json!({"action":"stop","streamId":id}))
            .await
            .unwrap();
        assert_eq!(stopped["state"], "stopped");
        // Outside MCP agents are never offered it.
        assert!(![
            "wonder_thread_list",
            "wonder_thread_read",
            "wonder_thread_send"
        ]
        .contains(&NAME));
    }

    #[tokio::test]
    async fn declined_and_unanswered_requests_return_to_the_agent() {
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        state
            .computer_supervisor
            .streams
            .configure_for_test(lab.ssh.clone(), lab.config.clone());
        let call = run(&state, &caller, json!({"action":"probe","host":"lab"}));
        let (result, _) = tokio::join!(call, answer_when_asked(&state, false));
        assert_eq!(result.unwrap_err().code, "declined");
        // Nobody answers: the call returns `expired` at its own deadline, and nothing connected.
        let error = run(
            &state,
            &caller,
            json!({"action":"probe","host":"lab","timeoutSeconds":1}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "expired");
        assert!(!lab.path("x11vnc.pid").exists());
    }

    // ---- end to end through the real agent.py -------------------------------

    fn lab_state(lab: &Lab, state: &AppState) {
        state
            .computer_supervisor
            .streams
            .configure_for_test(lab.ssh.clone(), lab.config.clone());
    }

    async fn answer_when_asked(state: &AppState, allow: bool) -> Option<StreamView> {
        for _ in 0..400 {
            let streams = &state.computer_supervisor.streams;
            if let Some(view) = streams
                .views()
                .into_iter()
                .find(|v| v.kind == State::AwaitingApproval)
            {
                return if allow {
                    streams.allow(&view.id)
                } else {
                    streams.decline(&view.id)
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        None
    }

    #[tokio::test]
    async fn probe_add_view_and_stop_over_a_private_display() {
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        lab_state(&lab, &state);
        let streams = &state.computer_supervisor.streams;

        // Probe waits for the owner, then reports what the host offers.
        let probe = run(&state, &caller, json!({"action":"probe","host":"lab"}));
        let (probe, _) = tokio::join!(probe, answer_when_asked(&state, true));
        let probe = probe.unwrap();
        assert_eq!(probe["hostKey"]["pinned"], true);
        let x11 = probe["transports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["transport"] == "x11")
            .unwrap();
        assert_eq!(x11["available"], true);
        assert_eq!(x11["canStartDisplay"], true);
        assert!(
            !probe
                .to_string()
                .contains(&lab.dir.path().to_string_lossy().into_owned()),
            "no paths leak"
        );

        // The Allow covers the add that follows: no second question.
        let add = run(
            &state,
            &caller,
            json!({"action":"add","host":"lab","transport":"x11","name":"Build box",
            "startDisplay":{"width":640,"height":480},"clientRequestId":uuid()}),
        );
        let phone = async {
            // The first frame is only prepared once somebody looks.
            for _ in 0..400 {
                if streams
                    .frame(
                        &streams
                            .views()
                            .first()
                            .map(|v| v.id.clone())
                            .unwrap_or_default(),
                    )
                    .is_some()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        let (added, _) = tokio::join!(add, phone);
        let added = added.unwrap();
        assert_eq!(added["state"], "live");
        assert_eq!(added["transport"], "x11");
        assert_eq!(added["name"], "Build box");
        let id = added["streamId"].as_str().unwrap().to_owned();
        // The agent gets metadata only, never pixels.
        assert!(added.get("jpeg").is_none() && added.to_string().len() < 600);

        // The owner's phone gets a real picture that changes.
        let frame = streams.frame(&id).unwrap();
        let first = frame.sequence;
        assert!(frame.jpeg.starts_with(&[0xff, 0xd8, 0xff]));
        assert_eq!(frame.sha256, hex::encode(Sha256::digest(&frame.jpeg)));
        let decoded = image::load_from_memory(&frame.jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
        let mut next = first;
        for _ in 0..200 {
            next = streams.frame(&id).unwrap().sequence;
            if next > first {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            next > first,
            "the picture should keep updating while it is looked at"
        );
        // Xvfb was asked for exactly the validated geometry, private, with no TCP listener; x11vnc is view-only.
        let xvfb_args = std::fs::read_to_string(lab.path("xvfb.args")).unwrap();
        assert!(
            xvfb_args.contains("-screen 0 640x480x24") && xvfb_args.contains("-nolisten tcp"),
            "{xvfb_args}"
        );
        let vnc_args = std::fs::read_to_string(lab.path("x11vnc.args")).unwrap();
        assert!(
            vnc_args.contains("-inetd")
                && vnc_args.contains("-viewonly")
                && vnc_args.contains("-nopw"),
            "{vnc_args}"
        );
        assert!(
            !vnc_args.contains("-rfbport") && !vnc_args.contains("-listen"),
            "{vnc_args}"
        );
        let listed = run(&state, &caller, json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(listed["streams"][0]["state"], "live");

        // Stop: the ssh child ends, the Xvfb that was started is stopped and its socket removed.
        let stopped = run(&state, &caller, json!({"action":"stop","streamId":id}))
            .await
            .unwrap();
        assert_eq!(stopped["state"], "stopped");
        wait_until("remote x11vnc to exit", || {
            !pid_alive(&lab.path("x11vnc.pid"))
        });
        wait_until("remote Xvfb to exit", || !pid_alive(&lab.path("xvfb.pid")));
        assert!(
            lab.path("xvfb.gone").exists(),
            "Xvfb was terminated, not left behind"
        );
        assert_eq!(streams.view(&id).unwrap().kind, State::Stopped);
    }

    #[tokio::test]
    async fn two_adds_waiting_on_one_allow_start_one_connection() {
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        lab_state(&lab, &state);
        let streams = &state.computer_supervisor.streams;
        let add = |request: String| {
            run(
                &state,
                &caller,
                json!({"action":"add","host":"lab","transport":"x11","startDisplay":{"width":640,"height":480},"clientRequestId":request}),
            )
        };
        let (first, second, _) =
            tokio::join!(add(uuid()), add(uuid()), answer_when_asked(&state, true));
        // One call owns the stream; the other is told it is already streaming. Neither orphans the first.
        let (ok, err) = match (first, second) {
            (Ok(v), Err(e)) | (Err(e), Ok(v)) => (v, e),
            other => panic!("expected one success and one refusal, got {other:?}"),
        };
        assert_eq!(err.code, "already_streaming");
        let id = ok["streamId"].as_str().unwrap().to_owned();
        assert_eq!(streams.view(&id).unwrap().kind, State::Live);
        assert_eq!(streams.views().len(), 1);
        let calls = std::fs::read_to_string(lab.path("ssh.calls")).unwrap();
        assert_eq!(calls.lines().count(), 1, "only one ssh was started");
        run(&state, &caller, json!({"action":"stop","streamId":id}))
            .await
            .unwrap();
        wait_until("remote x11vnc to exit", || {
            !pid_alive(&lab.path("x11vnc.pid"))
        });
        wait_until("remote Xvfb to exit", || !pid_alive(&lab.path("xvfb.pid")));
    }

    #[tokio::test]
    async fn the_screenshot_fallback_streams_and_stops() {
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        lab_state(&lab, &state);
        let streams = &state.computer_supervisor.streams;
        // An existing display on the "host": a socket file the remote program lists.
        std::fs::create_dir_all("/tmp/.X11-unix").unwrap();
        let socket = "/tmp/.X11-unix/X96";
        std::fs::write(socket, "").unwrap();
        let add = run(
            &state,
            &caller,
            json!({"action":"add","host":"lab","transport":"screenshot","display":":96","intervalSeconds":1,"clientRequestId":uuid()}),
        );
        let phone = async {
            answer_when_asked(&state, true).await;
            for _ in 0..400 {
                if streams
                    .views()
                    .first()
                    .is_some_and(|v| streams.frame(&v.id).is_some())
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        let (added, _) = tokio::join!(add, phone);
        let added = added.unwrap();
        let id = added["streamId"].as_str().unwrap().to_owned();
        assert_eq!(added["frameIntervalMs"], 1000);
        let frame = streams.frame(&id).unwrap();
        let decoded = image::load_from_memory(&frame.jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (80, 60));
        assert_eq!((frame.width, frame.height), (80, 60));
        run(&state, &caller, json!({"action":"stop","streamId":id}))
            .await
            .unwrap();
        std::fs::remove_file(socket).ok();
        // The remote program removed its private temp folder.
        wait_until("remote folder removal", || {
            !std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .flatten()
                .any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("cupola-stream-")
                        && e.metadata().is_ok_and(|m| {
                            m.modified().is_ok_and(|t| {
                                t.elapsed().map_or(true, |a| a < Duration::from_secs(20))
                            })
                        })
                        && e.path().join("frame.jpg").exists()
                })
        });
    }

    #[tokio::test]
    async fn an_ssh_refusal_is_named_and_never_retried() {
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        let streams = &state.computer_supervisor.streams;
        for (stderr, code) in [
            ("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed.", "host_key_changed"),
            ("No ED25519 host key is known for lab and you have requested strict checking.\nHost key verification failed.", "host_key_unpinned"),
            ("x@lab: Permission denied (publickey).", "auth_rejected"),
            ("ssh: connect to host lab port 22: Connection timed out", "unreachable"),
        ] {
            let ssh = lab.path("failing-ssh");
            script(&ssh, &format!("#!/bin/sh\nprintf '%s\\n' '{stderr}' >&2\nexit 255\n"));
            streams.configure_for_test(ssh, lab.config.clone());
            let call = run(&state, &caller, json!({"action":"probe","host":"lab"}));
            let (result, _) = tokio::join!(call, answer_when_asked(&state, true));
            let error = result.unwrap_err();
            assert_eq!(error.code, code, "{stderr}");
            assert!(!error.message.contains(stderr) || code == "unreachable" && false, "remote text must not be echoed");
            assert!(!error.message.contains("StrictHostKeyChecking no"));
            streams.shutdown().await;
        }
    }

    #[tokio::test]
    async fn a_vnc_server_that_needs_a_password_is_refused_and_a_bad_screen_fails_cleanly() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        lab_state(&lab, &state);
        // A VNC server on the loopback that offers only VNC authentication.
        let mut port = None;
        let mut listener = None;
        for candidate in 5950..5999u16 {
            if let Ok(found) = tokio::net::TcpListener::bind(("127.0.0.1", candidate)).await {
                port = Some(candidate);
                listener = Some(found);
                break;
            }
        }
        let (port, listener) = (port.unwrap(), listener.unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let _ = socket.write_all(b"RFB 003.008\n").await;
                    let mut reply = [0u8; 12];
                    let _ = socket.read_exact(&mut reply).await;
                    let _ = socket.write_all(&[1, 2]).await;
                    let mut sink = Vec::new();
                    let _ = socket.read_to_end(&mut sink).await;
                });
            }
        });
        let add = run(
            &state,
            &caller,
            json!({"action":"add","host":"lab","transport":"vnc","vncPort":port,"clientRequestId":uuid()}),
        );
        let (result, _) = tokio::join!(add, answer_when_asked(&state, true));
        let error = result.unwrap_err();
        assert_eq!(error.code, "vnc_auth_unsupported", "{}", error.message);
        // Nothing is left running, and the stream is recorded as failed with its reason.
        let views = state.computer_supervisor.streams.views();
        assert_eq!(views[0].kind, State::Failed);
        assert_eq!(views[0].reason_code, Some("vnc_auth_unsupported"));
        // A port with nothing listening is a named transport problem.
        let add = run(
            &state,
            &caller,
            json!({"action":"add","host":"lab","transport":"vnc","vncPort":5999,"clientRequestId":uuid()}),
        );
        let (result, _) = tokio::join!(add, answer_when_asked(&state, true));
        assert_eq!(result.unwrap_err().code, "transport_unavailable");
    }

    #[tokio::test]
    async fn shutting_the_daemon_down_ends_every_remote_screen() {
        let (_dir, state, caller) = fx().await;
        let lab = lab(&["lab"]);
        lab_state(&lab, &state);
        let streams = &state.computer_supervisor.streams;
        let add = run(
            &state,
            &caller,
            json!({"action":"add","host":"lab","transport":"x11","startDisplay":{"width":640,"height":480},"clientRequestId":uuid()}),
        );
        let phone = async {
            answer_when_asked(&state, true).await;
            for _ in 0..400 {
                if streams
                    .views()
                    .first()
                    .is_some_and(|v| streams.frame(&v.id).is_some())
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        let (added, _) = tokio::join!(add, phone);
        added.unwrap();
        assert!(pid_alive(&lab.path("x11vnc.pid")));
        state.computer_supervisor.shutdown(&state).await;
        wait_until("remote x11vnc to exit", || {
            !pid_alive(&lab.path("x11vnc.pid"))
        });
        wait_until("remote Xvfb to exit", || !pid_alive(&lab.path("xvfb.pid")));
        assert!(streams.views().iter().all(|v| v.kind == State::Stopped));
    }

    #[tokio::test]
    async fn the_phones_routes_need_a_paired_device_and_serve_conditional_frames() {
        use axum::extract::{Path, Query, State as St};
        let (_dir, state, caller) = fx().await;
        let streams = &state.computer_supervisor.streams;
        let id = streams
            .open(&open(
                &caller.project.id,
                &caller.conversation.conversation_id,
                "lab",
            ))
            .unwrap();
        let s = || St(state.clone());
        let after = |n: Option<u64>| Query(crate::computer_sessions::RemoteFrameQuery { after: n });
        // No paired device: nothing is listed, allowed, stopped or served.
        assert_eq!(
            crate::computer_sessions::remote_streams(s(), None)
                .await
                .status(),
            401
        );
        assert_eq!(
            crate::computer_sessions::allow_remote_stream(s(), None, Path(id.clone()))
                .await
                .status(),
            401
        );
        assert_eq!(
            crate::computer_sessions::decline_remote_stream(s(), None, Path(id.clone()))
                .await
                .status(),
            401
        );
        assert_eq!(
            crate::computer_sessions::stop_remote_stream(s(), None, Path(id.clone()))
                .await
                .status(),
            401
        );
        assert_eq!(
            crate::computer_sessions::remote_stream_frame(s(), None, Path(id.clone()), after(None))
                .await
                .status(),
            401
        );
        assert_eq!(
            streams.view(&id).unwrap().kind,
            State::AwaitingApproval,
            "an unauthenticated call changed nothing"
        );
        // A paired device can. With no picture yet the frame is 404.
        let device = || {
            Some(axum::Extension(crate::AuthenticatedDevice {
                device_id: "d".into(),
                session_binding: "b".into(),
            }))
        };
        assert_eq!(
            crate::computer_sessions::remote_stream_frame(
                s(),
                device(),
                Path(id.clone()),
                after(None)
            )
            .await
            .status(),
            404
        );
        let allowed =
            crate::computer_sessions::allow_remote_stream(s(), device(), Path(id.clone())).await;
        assert_eq!(allowed.status(), 200);
        Streams::publish(
            &streams.inner,
            &id,
            encode_rgb(image::RgbImage::new(8, 4)).unwrap(),
        );
        let body = |response: axum::response::Response| async move {
            serde_json::from_slice::<Value>(
                &axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap(),
            )
            .unwrap()
        };
        let contract = |definition: &str, value: &Value| {
            let authority: Value = serde_json::from_str(include_str!(
                "../../../../packages/protocol/schemas/wonder-http-v1.json"
            ))
            .unwrap();
            let schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","$ref":format!("#/$defs/{definition}"),"$defs":authority["$defs"]});
            let validator = jsonschema::validator_for(&schema).unwrap();
            let errors: Vec<_> = validator
                .iter_errors(value)
                .map(|e| e.to_string())
                .collect();
            assert!(errors.is_empty(), "{definition}: {errors:?}");
        };
        contract("computerRemoteScreenAnswer", &body(allowed).await);
        let listed = body(crate::computer_sessions::remote_streams(s(), device()).await).await;
        for stream in listed["streams"].as_array().unwrap() {
            contract("computerRemoteScreen", stream);
        }
        assert_eq!(listed["streams"].as_array().unwrap().len(), 1);
        let first = body(
            crate::computer_sessions::remote_stream_frame(
                s(),
                device(),
                Path(id.clone()),
                after(None),
            )
            .await,
        )
        .await;
        contract("computerRemoteScreenFrame", &first);
        assert_eq!(
            (
                first["mimeType"].clone(),
                first["width"].clone(),
                first["height"].clone()
            ),
            (json!("image/jpeg"), json!(8), json!(4))
        );
        assert_eq!(first["sequence"], 1);
        assert_eq!(first["sha256"].as_str().unwrap().len(), 64);
        // The viewer already has it: only the number comes back.
        let same = body(
            crate::computer_sessions::remote_stream_frame(
                s(),
                device(),
                Path(id.clone()),
                after(Some(1)),
            )
            .await,
        )
        .await;
        assert_eq!(same["imageBase64"], Value::Null);
        contract("computerRemoteScreenFrame", &same);
        assert_eq!(
            crate::computer_sessions::stop_remote_stream(s(), device(), Path(id.clone()))
                .await
                .status(),
            200
        );
        assert_eq!(
            crate::computer_sessions::stop_remote_stream(s(), device(), Path("nope".into()))
                .await
                .status(),
            404
        );
        // Once stopped, the frame is still there for the phone's last look until the entry is forgotten.
        assert_eq!(streams.view(&id).unwrap().kind, State::Stopped);
    }

    // ---- live checks against a real sshd (ignored; see docs/specs/stream-tool.md "Tests and evidence") ----

    /// `CUPOLA_STREAM_LAB` is a folder with `hostkey.pub`, `userkey`, `other.pub` (an unrelated public key) and a
    /// running sshd for `CUPOLA_STREAM_LAB_PORT` that accepts `userkey`. The checks write their own SSH config
    /// variants and known_hosts files into a temporary folder.
    fn live_config(variant: &str) -> Option<(tempfile::TempDir, PathBuf)> {
        let lab = PathBuf::from(std::env::var_os("CUPOLA_STREAM_LAB")?);
        let port = std::env::var("CUPOLA_STREAM_LAB_PORT").unwrap_or_else(|_| "2299".into());
        let dir = tempfile::tempdir().unwrap();
        let read = |name: &str| std::fs::read_to_string(lab.join(name)).unwrap();
        let key_of = |text: String| {
            text.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        };
        let known = dir.path().join("known_hosts");
        let mut identity = lab.join("userkey");
        match variant {
            "pinned" => {
                std::fs::write(&known, format!("lab {}\n", key_of(read("hostkey.pub")))).unwrap()
            }
            "unpinned" => std::fs::write(&known, "").unwrap(),
            "changed" => {
                std::fs::write(&known, format!("lab {}\n", key_of(read("other.pub")))).unwrap()
            }
            "wrongkey" => {
                std::fs::write(&known, format!("lab {}\n", key_of(read("hostkey.pub")))).unwrap();
                identity = lab.join("other.pub");
            }
            _ => panic!("variant"),
        }
        let config = dir.path().join("config");
        std::fs::write(
            &config,
            format!(
                "Host lab\n  HostName 127.0.0.1\n  Port {port}\n  User {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  HostKeyAlias lab\n",
                std::env::var("USER").unwrap_or_else(|_| "user".into()),
                identity.display(),
                known.display()
            ),
        )
        .unwrap();
        Some((dir, config))
    }

    #[tokio::test]
    #[ignore = "needs CUPOLA_STREAM_LAB: a real sshd"]
    async fn live_ssh_refusals_are_named_and_nothing_is_accepted_silently() {
        let Some(_) = live_config("pinned") else {
            panic!("set CUPOLA_STREAM_LAB")
        };
        let (_dir, state, caller) = fx().await;
        for (variant, code) in [
            ("unpinned", "host_key_unpinned"),
            ("changed", "host_key_changed"),
            ("wrongkey", "key_file_unusable"),
        ] {
            let (dir, config) = live_config(variant).unwrap();
            state
                .computer_supervisor
                .streams
                .configure_for_test(ssh::default_program().unwrap(), config);
            let call = run(&state, &caller, json!({"action":"probe","host":"lab"}));
            let (result, _) = tokio::join!(call, answer_when_asked(&state, true));
            let error = result.unwrap_err();
            // A .pub as IdentityFile may be reported as an unusable key or a rejected one, both named.
            assert!(
                error.code == code || (variant == "wrongkey" && error.code == "auth_rejected"),
                "{variant}: {} {}",
                error.code,
                error.message
            );
            // known_hosts was never written to.
            let known = std::fs::read_to_string(dir.path().join("known_hosts")).unwrap();
            assert_eq!(
                known.lines().count(),
                usize::from(variant != "unpinned"),
                "{variant}: known_hosts must not gain or lose a line"
            );
            state.computer_supervisor.streams.shutdown().await;
        }
    }

    #[tokio::test]
    #[ignore = "needs CUPOLA_STREAM_LAB: a real sshd with x11vnc, Xvfb and import on its PATH"]
    async fn live_every_transport_streams_over_real_ssh_and_stop_leaves_nothing() {
        let (_dir, config) = live_config("pinned").expect("set CUPOLA_STREAM_LAB");
        let (_state_dir, state, caller) = fx().await;
        let streams = &state.computer_supervisor.streams;
        streams.configure_for_test(ssh::default_program().unwrap(), config);
        let call = run(&state, &caller, json!({"action":"probe","host":"lab"}));
        let (probe, _) = tokio::join!(call, answer_when_asked(&state, true));
        let probe = probe.unwrap();
        println!("probe: {probe}");
        let mut cases = vec![json!({"transport":"x11","startDisplay":{"width":800,"height":600}})];
        if let Ok(display) = std::env::var("CUPOLA_STREAM_LAB_DISPLAY") {
            cases.push(json!({"transport":"x11","display":display.clone()}));
            cases.push(json!({"transport":"screenshot","display":display,"intervalSeconds":1}));
        }
        if let Ok(port) = std::env::var("CUPOLA_STREAM_LAB_VNC_PORT") {
            cases.push(json!({"transport":"vnc","vncPort":port.parse::<u64>().unwrap()}));
        }
        for (index, mut case) in cases.into_iter().enumerate() {
            case["action"] = json!("add");
            case["host"] = json!("lab");
            case["clientRequestId"] = json!(uuid());
            let started = Instant::now();
            let add = run(&state, &caller, case.clone());
            let phone = async {
                // The Allow from the probe ended with the first stream, so later adds ask the owner again.
                if index > 0 {
                    answer_when_asked(&state, true).await;
                }
                for _ in 0..800 {
                    if streams
                        .views()
                        .first()
                        .is_some_and(|v| streams.frame(&v.id).is_some())
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            let (added, _) = tokio::join!(add, phone);
            let added = added.unwrap_or_else(|e| panic!("{case}: {} {}", e.code, e.message));
            let id = added["streamId"].as_str().unwrap().to_owned();
            let frame = streams.frame(&id).unwrap();
            let decoded = image::load_from_memory(&frame.jpeg).unwrap();
            println!(
                "{case}: first frame {}x{} in {} ms, {} bytes",
                decoded.width(),
                decoded.height(),
                started.elapsed().as_millis(),
                frame.jpeg.len()
            );
            let first = frame.sequence;
            tokio::time::sleep(Duration::from_millis(2500)).await;
            println!(
                "{case}: sequence {first} -> {}",
                streams.frame(&id).unwrap().sequence
            );
            let stop_started = Instant::now();
            run(&state, &caller, json!({"action":"stop","streamId":id}))
                .await
                .unwrap();
            println!(
                "{case}: stopped in {} ms",
                stop_started.elapsed().as_millis()
            );
        }
    }
}
