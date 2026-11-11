//! Owner terminals: a login shell on a host PTY in a Project conversation's
//! verified working folder, streamed to a paired phone over the authenticated
//! API connection.
//!
//! Output is kept in a bounded ring buffer addressed by byte offsets
//! ("sequences"). A client reattaches with the offset it already has, so a
//! reconnect replays only the bytes it missed and never duplicates any. When
//! the ring no longer holds that offset, the client is told to reset and the
//! retained tail is replayed from its first whole line.
//!
//! The design follows T3 Code's terminal service (bounded server-side
//! scrollback replayed on reconnect, explicit close, process-group kill), with
//! Wonder's own transport and authentication.

use super::*;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path as FsPath;
use std::sync::Mutex as StdMutex;
use std::time::Instant;

#[cfg(unix)]
#[path = "terminal/pty_unix.rs"]
mod pty;
#[cfg(windows)]
#[path = "terminal/pty_windows.rs"]
mod pty;
use pty::{login_shell, sanitized_environment, ProcessOwner};

/// `terminalVersion` in `GET /api/v1/projects`; clients hide the terminal
/// entry on hosts that do not publish it.
pub(crate) const VERSION: u8 = 1;

/// Bounds for one host. Defaults are production values; tests shrink them.
#[derive(Clone, Debug)]
pub struct TerminalLimits {
    pub per_device: usize,
    pub total: usize,
    pub scrollback_bytes: usize,
    /// A session nobody is attached to closes after this long without input.
    pub detached_idle: Duration,
    pub max_input_frame: usize,
}

impl Default for TerminalLimits {
    fn default() -> Self {
        Self {
            per_device: 4,
            total: 12,
            scrollback_bytes: 2 * 1024 * 1024,
            detached_idle: Duration::from_secs(30 * 60),
            max_input_frame: 64 * 1024,
        }
    }
}

const MIN_COLS: u16 = 2;
const MAX_COLS: u16 = 500;
const MIN_ROWS: u16 = 1;
const MAX_ROWS: u16 = 300;
const OUTPUT_FRAME: usize = 64 * 1024;
/// How far past a truncation point replay looks for a line start.
const LINE_ALIGN_WINDOW: usize = 4096;

pub struct TerminalHost {
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
    /// Includes grace-period cleanup after a session has left the visible map.
    cleanup_owners: StdMutex<Vec<std::sync::Weak<ProcessOwner>>>,
    limits: TerminalLimits,
    /// Overrides the login shell for tests.
    shell_override: Option<PathBuf>,
}

impl Default for TerminalHost {
    fn default() -> Self {
        Self::new(TerminalLimits::default())
    }
}

impl TerminalHost {
    pub fn new(limits: TerminalLimits) -> Self {
        Self {
            sessions: StdMutex::new(HashMap::new()),
            cleanup_owners: StdMutex::new(Vec::new()),
            limits,
            shell_override: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_shell(limits: TerminalLimits, shell: PathBuf) -> Self {
        Self {
            sessions: StdMutex::new(HashMap::new()),
            cleanup_owners: StdMutex::new(Vec::new()),
            limits,
            shell_override: Some(shell),
        }
    }

    fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn summaries(&self, conversation_id: &str) -> Vec<TerminalSummary> {
        let sessions = self.sessions.lock().unwrap();
        let mut list: Vec<_> = sessions
            .values()
            .filter(|session| session.conversation_id == conversation_id)
            .map(|session| session.summary())
            .collect();
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        list
    }

    fn open(
        &self,
        device_id: &str,
        conversation_id: &str,
        cwd: &FsPath,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Session>, (StatusCode, &'static str)> {
        let shell = match &self.shell_override {
            Some(shell) => shell.clone(),
            None => login_shell().ok_or((
                StatusCode::SERVICE_UNAVAILABLE,
                "Your computer's login shell could not be found.",
            ))?,
        };
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= self.limits.total {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "Too many terminals are open on your computer. Close one first.",
            ));
        }
        if sessions
            .values()
            .filter(|session| session.device_id == device_id)
            .count()
            >= self.limits.per_device
        {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "This device has the most terminals it can open. Close one first.",
            ));
        }
        let session = Session::spawn(
            device_id,
            conversation_id,
            cwd,
            &shell,
            cols,
            rows,
            self.limits.scrollback_bytes,
        )
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The terminal could not start on your computer.",
            )
        })?;
        let mut owners = self.cleanup_owners.lock().unwrap();
        owners.retain(|owner| owner.upgrade().is_some_and(|owner| !owner.finished()));
        owners.push(Arc::downgrade(&session.processes));
        sessions.insert(session.id.clone(), Arc::clone(&session));
        Ok(session)
    }

    /// Removes the session and ends its process group.
    pub fn close(&self, id: &str) -> bool {
        let session = self.sessions.lock().unwrap().remove(id);
        match session {
            Some(session) => {
                session.terminate();
                true
            }
            None => false,
        }
    }

    /// Closes sessions nobody has been attached to for `detached_idle`.
    pub fn reap_idle(&self, now: Instant) -> usize {
        let expired: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|session| session.idle_expired(now, self.limits.detached_idle))
            .map(|session| session.id.clone())
            .collect();
        expired.iter().filter(|id| self.close(id)).count()
    }

    /// Ends every terminal a revoked device opened.
    pub fn close_device(&self, device_id: &str) -> usize {
        let ids: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|session| session.device_id == device_id)
            .map(|session| session.id.clone())
            .collect();
        ids.iter().filter(|id| self.close(id)).count()
    }

    /// Closes every terminal; at daemon exit, waits briefly and then kills
    /// what ignored SIGHUP, since the delayed kill would not outlive the daemon.
    pub fn close_all(&self) {
        let (sessions, owners) = {
            // Keep the same lock order as open, so a newly registered owner
            // cannot be separated from its session by shutdown's snapshot.
            let mut sessions = self.sessions.lock().unwrap();
            let active: Vec<_> = sessions.drain().map(|(_, session)| session).collect();
            let owners: Vec<_> = self
                .cleanup_owners
                .lock()
                .unwrap()
                .drain(..)
                .filter_map(|owner| owner.upgrade())
                .collect();
            (active, owners)
        };
        for session in &sessions {
            session.terminate();
        }
        if !owners.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            for owner in owners {
                owner.force_kill();
            }
        }
    }
}

/// Closes idle sessions and the sessions of revoked devices until the host is
/// dropped.
pub fn spawn_reaper(
    host: &Arc<TerminalHost>,
    mut revocations: tokio::sync::broadcast::Receiver<String>,
) -> tokio::task::JoinHandle<()> {
    let host = Arc::downgrade(host);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let Some(host) = host.upgrade() else { return };
                    host.reap_idle(Instant::now());
                }
                revoked = revocations.recv() => match revoked {
                    Ok(device_id) => {
                        let Some(host) = host.upgrade() else { return };
                        host.close_device(&device_id);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
            }
        }
    })
}

#[derive(Debug)]
struct Output {
    ring: VecDeque<u8>,
    /// Sequence (stream byte offset) of `ring[0]`.
    start: u64,
    exit_code: Option<i32>,
    exited: bool,
    closed: bool,
    cols: u16,
    rows: u16,
    /// Attached views, least recently active first. The last one owns the
    /// grid; every other view renders that grid instead of resizing the shell.
    viewers: Vec<Viewer>,
    next_viewer: u64,
    last_activity: Instant,
}

#[derive(Debug)]
struct Viewer {
    id: u64,
    /// The grid this view last asked for.
    grid: Option<(u16, u16)>,
}

impl Output {
    fn end(&self) -> u64 {
        self.start + self.ring.len() as u64
    }

    fn append(&mut self, bytes: &[u8], capacity: usize) {
        self.ring.extend(bytes);
        if self.ring.len() <= capacity {
            return;
        }
        let mut drop = self.ring.len() - capacity;
        // Start the retained tail at a line boundary when one is close, so a
        // replay does not begin in the middle of a line or escape sequence.
        if let Some(newline) = self
            .ring
            .range(drop..self.ring.len().min(drop + LINE_ALIGN_WINDOW))
            .position(|byte| *byte == b'\n')
        {
            drop += newline + 1;
        }
        self.ring.drain(..drop);
        self.start += drop as u64;
    }

    fn slice(&self, from: u64, max: usize) -> Vec<u8> {
        let offset = from.saturating_sub(self.start) as usize;
        let end = self.ring.len().min(offset.saturating_add(max));
        self.ring.range(offset..end).copied().collect()
    }
}

struct Session {
    id: String,
    device_id: String,
    conversation_id: String,
    title: String,
    shell_name: String,
    created_at: String,
    processes: Arc<ProcessOwner>,
    pty: pty::Pty,
    input: tokio::sync::mpsc::Sender<Vec<u8>>,
    output: StdMutex<Output>,
    capacity: usize,
    changed: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn spawn(
        device_id: &str,
        conversation_id: &str,
        cwd: &FsPath,
        shell: &FsPath,
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> std::io::Result<Arc<Self>> {
        let shell_name = shell
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sh".into());
        let pty::Spawned {
            pty,
            mut reader,
            mut writer,
            processes,
        } = pty::spawn(
            shell,
            &shell_name,
            cwd,
            sanitized_environment(shell),
            cols,
            rows,
        )?;

        let (input, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let (changed, _) = tokio::sync::watch::channel(0);
        let session = Arc::new(Self {
            id: format!("term_{}", uuid::Uuid::new_v4().simple()),
            device_id: device_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            title: cwd
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.to_string_lossy().into_owned()),
            shell_name,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            processes: Arc::clone(&processes),
            pty,
            input,
            output: StdMutex::new(Output {
                ring: VecDeque::new(),
                start: 0,
                exit_code: None,
                exited: false,
                closed: false,
                cols,
                rows,
                viewers: Vec::new(),
                next_viewer: 0,
                last_activity: Instant::now(),
            }),
            capacity,
            changed,
        });

        let owner = Arc::clone(&processes);
        std::thread::Builder::new()
            .name("wonder-terminal-processes".into())
            .spawn(move || owner.run())?;

        let writer_processes = Arc::clone(&processes);
        std::thread::Builder::new()
            .name("wonder-terminal-input".into())
            .spawn(move || {
                while let Some(bytes) = input_rx.blocking_recv() {
                    if writer_processes.finished() {
                        return;
                    }
                    if writer.write_all(&bytes).is_err() {
                        return;
                    }
                }
            })?;

        let weak = Arc::downgrade(&session);
        std::thread::Builder::new()
            .name("wonder-terminal-output".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => match weak.upgrade() {
                            Some(session) => session.append(&buffer[..read]),
                            None => break,
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                // Release the master before waiting: a shell can be stuck
                // exiting until its terminal output is read or discarded.
                drop(reader);
                // EOF can precede process exit when a job redirects its PTY.
                // The lifecycle owner alone signals and reaps the direct child.
                processes.close_after(Duration::from_secs(2));
                let code = processes.wait();
                if let Some(session) = weak.upgrade() {
                    session.mark_exited(code);
                }
            })?;
        Ok(session)
    }

    fn append(&self, bytes: &[u8]) {
        let end = {
            let mut output = self.output.lock().unwrap();
            output.append(bytes, self.capacity);
            output.end()
        };
        self.changed.send_replace(end);
    }

    fn mark_exited(&self, code: Option<i32>) {
        let end = {
            let mut output = self.output.lock().unwrap();
            output.exited = true;
            output.exit_code = code;
            output.end()
        };
        self.changed.send_modify(|value| *value = end);
        // Wake an idle writer even if the exited session is retained for replay.
        // If the queue is full, its next frame observes the finished owner.
        let _ = self.input.try_send(Vec::new());
    }

    fn summary(&self) -> TerminalSummary {
        let output = self.output.lock().unwrap();
        TerminalSummary {
            id: self.id.clone(),
            conversation_id: self.conversation_id.clone(),
            title: self.title.clone(),
            shell: self.shell_name.clone(),
            cols: output.cols,
            rows: output.rows,
            created_at: self.created_at.clone(),
            sequence: output.end(),
            exited: output.exited,
            exit_code: output.exit_code,
        }
    }

    /// Applies `grid` to the PTY under the output lock, so concurrent views
    /// cannot leave the recorded grid differing from the shell's. An unchanged
    /// grid sends the shell no resize signal. Returns whether the grid changed.
    fn apply_grid(&self, output: &mut Output, grid: Option<(u16, u16)>) -> bool {
        let Some((cols, rows)) = grid else {
            return false;
        };
        if (output.cols, output.rows) == (cols, rows) || !self.pty.resize(cols, rows) {
            return false;
        }
        output.cols = cols;
        output.rows = rows;
        true
    }

    fn grid_changed(&self, changed: bool) {
        if changed {
            self.changed.send_modify(|_| {});
        }
    }

    /// A view asks for its grid. Only the owning (most recently active) view
    /// resizes the shell; another view's request waits until it becomes owner.
    fn resize(&self, viewer: u64, cols: u16, rows: u16) -> bool {
        if !(MIN_COLS..=MAX_COLS).contains(&cols) || !(MIN_ROWS..=MAX_ROWS).contains(&rows) {
            return false;
        }
        let changed = {
            let mut output = self.output.lock().unwrap();
            let owner = output.viewers.last().map(|view| view.id);
            match output.viewers.iter_mut().find(|view| view.id == viewer) {
                Some(view) => view.grid = Some((cols, rows)),
                None => return false,
            }
            owner == Some(viewer) && self.apply_grid(&mut output, Some((cols, rows)))
        };
        self.grid_changed(changed);
        true
    }

    /// Typing in a view makes it the owner; the shell takes its grid.
    fn claim(&self, viewer: u64) {
        let changed = {
            let mut output = self.output.lock().unwrap();
            output.last_activity = Instant::now();
            match output.viewers.iter().position(|view| view.id == viewer) {
                Some(index) if index + 1 != output.viewers.len() => {
                    let view = output.viewers.remove(index);
                    let grid = view.grid;
                    output.viewers.push(view);
                    self.apply_grid(&mut output, grid)
                }
                _ => false,
            }
        };
        self.grid_changed(changed);
    }

    /// Asks full-screen programs to repaint after a reset replay.
    fn redraw(&self) {
        self.processes.redraw();
    }

    /// A newly attached view owns the grid, since opening it is the latest use.
    fn attach(&self, grid: Option<(u16, u16)>) -> u64 {
        let grid = grid.filter(|(cols, rows)| {
            (MIN_COLS..=MAX_COLS).contains(cols) && (MIN_ROWS..=MAX_ROWS).contains(rows)
        });
        let (id, changed) = {
            let mut output = self.output.lock().unwrap();
            let id = output.next_viewer;
            output.next_viewer += 1;
            output.viewers.push(Viewer { id, grid });
            output.last_activity = Instant::now();
            (id, self.apply_grid(&mut output, grid))
        };
        self.grid_changed(changed);
        id
    }

    /// When the owner leaves, the most recently active remaining view takes over.
    fn detach(&self, viewer: u64) {
        let changed = {
            let mut output = self.output.lock().unwrap();
            output.last_activity = Instant::now();
            let was_owner = output.viewers.last().is_some_and(|view| view.id == viewer);
            output.viewers.retain(|view| view.id != viewer);
            let grid = output.viewers.last().and_then(|view| view.grid);
            was_owner && self.apply_grid(&mut output, grid)
        };
        self.grid_changed(changed);
    }

    fn idle_expired(&self, now: Instant, limit: Duration) -> bool {
        let output = self.output.lock().unwrap();
        output.viewers.is_empty() && now.saturating_duration_since(output.last_activity) >= limit
    }

    fn terminate(&self) {
        self.output.lock().unwrap().closed = true;
        self.changed.send_modify(|_| {});
        self.processes.close_after(Duration::from_secs(2));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // `terminate` already gave a closed session its SIGHUP grace period.
        let live = self
            .output
            .lock()
            .map(|output| !output.exited && !output.closed)
            .unwrap_or(false);
        if live {
            self.processes.force_kill();
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalSummary {
    id: String,
    conversation_id: String,
    title: String,
    shell: String,
    cols: u16,
    rows: u16,
    created_at: String,
    /// Bytes of output produced so far.
    sequence: u64,
    exited: bool,
    exit_code: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OpenTerminalRequest {
    cols: u16,
    rows: u16,
}

fn requesting_device(device: Option<Extension<AuthenticatedDevice>>) -> String {
    device.map_or_else(|| "local".to_owned(), |Extension(device)| device.device_id)
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
) -> Response {
    if let Err((status, detail)) =
        filesystem::conversation_working_directory(&state, &conversation_id).await
    {
        return (status, detail).into_response();
    }
    Json(serde_json::json!({ "terminals": state.terminals.summaries(&conversation_id) }))
        .into_response()
}

pub(crate) async fn open(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    device: Option<Extension<AuthenticatedDevice>>,
    Path(conversation_id): Path<String>,
    Json(request): Json<OpenTerminalRequest>,
) -> Response {
    if !(MIN_COLS..=MAX_COLS).contains(&request.cols)
        || !(MIN_ROWS..=MAX_ROWS).contains(&request.rows)
    {
        return (
            StatusCode::BAD_REQUEST,
            "This terminal size is not supported.",
        )
            .into_response();
    }
    let cwd = match filesystem::conversation_working_directory(&state, &conversation_id).await {
        Ok(cwd) => cwd,
        Err((status, detail)) => return filesystem::folder_refusal(status, detail),
    };
    let device_id = requesting_device(device);
    let terminals = Arc::clone(&state.terminals);
    let opened = tokio::task::spawn_blocking(move || {
        terminals.open(
            &device_id,
            &conversation_id,
            &cwd,
            request.cols,
            request.rows,
        )
    })
    .await;
    match opened {
        Ok(Ok(session)) => (StatusCode::CREATED, Json(session.summary())).into_response(),
        Ok(Err((status, detail))) => (status, detail).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn close(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(terminal_id): Path<String>,
) -> Response {
    if state.terminals.close(&terminal_id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "This terminal has already closed.").into_response()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attach {
    device_id: Option<String>,
    csrf_token: Option<String>,
    challenge_id: Option<String>,
    signature: Option<String>,
    /// The stream offset the client already holds; absent for a fresh view.
    sequence: Option<u64>,
    cols: Option<u16>,
    rows: Option<u16>,
    /// The view renders the shell's grid when another view owns it, so it
    /// accepts `grid` frames. Older views never receive one.
    #[serde(default)]
    grid: bool,
}

/// Resize requests arriving this close together reach the shell as one.
const RESIZE_SETTLE: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientControl {
    Resize { cols: u16, rows: u16 },
}

pub(crate) async fn socket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    local: Option<Extension<LocalOwnerAuthority>>,
    Path(terminal_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(session) = state.terminals.get(&terminal_id) else {
        return (StatusCode::NOT_FOUND, "This terminal has already closed.").into_response();
    };
    let token = cookie_value(&headers, "__Host-wonder_session");
    let max_input = state.terminals.limits.max_input_frame;
    ws.max_message_size(max_input + 1024)
        .on_upgrade(move |socket| async move {
            stream(socket, state, session, token, local.is_some(), max_input).await
        })
}

/// Same proof as the events stream: the cookie session, its CSRF token and a
/// fresh signed challenge from the paired device.
async fn authenticate(
    state: &AppState,
    attach: &Attach,
    token: Option<&str>,
) -> Option<(String, Option<(String, tokio::time::Instant)>)> {
    let token = token?;
    let device_id = attach.device_id.as_deref()?;
    let mut pairing = state.pairing.lock().await;
    let now = now_ms();
    let verified = pairing
        .verify_session(token, Some(attach.csrf_token.as_deref()?), now)
        .ok()?;
    if verified != device_id {
        return None;
    }
    pairing
        .verify_fresh_challenge(
            attach.challenge_id.as_deref()?,
            device_id,
            attach.signature.as_deref()?,
            now,
        )
        .ok()?;
    let deadline = pairing
        .session_expires_at_ms(token)
        .and_then(|expires| {
            tokio::time::Instant::now().checked_add(std::time::Duration::from_millis(
                expires.saturating_sub(now),
            ))
        })
        .map(|at| (token.to_owned(), at));
    Some((verified, deadline))
}

/// The close code a terminal socket ends with when this device's sign-in is
/// refused or its access ended (revoked, or the session expired). The phone
/// stops reconnecting and explains it instead of retrying an outage.
pub(crate) const ACCESS_ENDED_CLOSE: u16 = 4401;

async fn close_access_ended(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: ACCESS_ENDED_CLOSE,
            reason: "access-ended".into(),
        })))
        .await;
}

async fn send_json(socket: &mut WebSocket, value: serde_json::Value) -> bool {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .is_ok()
}

async fn stream(
    mut socket: WebSocket,
    state: AppState,
    session: Arc<Session>,
    token: Option<String>,
    local: bool,
    max_input: usize,
) {
    let mut revocations = state.revocations.subscribe();
    let attach = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<Attach>(&text).ok(),
        _ => None,
    };
    let Some(attach) = attach else { return };
    let (device_id, mut deadline) = if local {
        ("local".to_owned(), None)
    } else {
        match authenticate(&state, &attach, token.as_deref()).await {
            Some(value) => value,
            None => {
                close_access_ended(&mut socket).await;
                return;
            }
        }
    };
    let mut changes = session.changed.subscribe();
    let viewer = session.attach(attach.cols.zip(attach.rows));
    struct Detach(Arc<Session>, u64);
    impl Drop for Detach {
        fn drop(&mut self) {
            self.0.detach(self.1);
        }
    }
    let _detach = Detach(Arc::clone(&session), viewer);
    let (start, end, exited, exit_code, cols, rows) = {
        let output = session.output.lock().unwrap();
        (
            output.start,
            output.end(),
            output.exited,
            output.exit_code,
            output.cols,
            output.rows,
        )
    };
    // Replay from the client's offset when the ring still holds it; otherwise
    // the client clears its screen and receives the retained tail.
    let (mut sent, reset) = match attach.sequence {
        Some(held) if held >= start && held <= end => (held, false),
        _ => (start, true),
    };
    if !send_json(
        &mut socket,
        serde_json::json!({
            "type": "ready", "sequence": sent, "reset": reset,
            "cols": cols, "rows": rows,
            "exited": exited, "exitCode": exit_code,
        }),
    )
    .await
    {
        return;
    }
    if reset && attach.sequence.is_some() && !exited {
        session.redraw();
    }
    let mut reported_exit = exited;
    let mut reported_grid = (cols, rows);
    let mut pending_resize: Option<(u16, u16, tokio::time::Instant)> = None;
    loop {
        // Drain everything produced since `sent`, in bounded frames.
        loop {
            let (start, chunk, closed, exited, exit_code, grid) = {
                let output = session.output.lock().unwrap();
                (
                    output.start,
                    output.slice(sent.max(output.start), OUTPUT_FRAME),
                    output.closed,
                    output.exited,
                    output.exit_code,
                    (output.cols, output.rows),
                )
            };
            if attach.grid && grid != reported_grid {
                // Sent before later output, which the shell wrote for this grid.
                reported_grid = grid;
                if !send_json(
                    &mut socket,
                    serde_json::json!({"type": "grid", "cols": grid.0, "rows": grid.1}),
                )
                .await
                {
                    return;
                }
            }
            if sent < start {
                // This client fell behind the ring; it must clear and resume.
                sent = start;
                if !send_json(
                    &mut socket,
                    serde_json::json!({"type": "reset", "sequence": sent}),
                )
                .await
                {
                    return;
                }
                continue;
            }
            if !chunk.is_empty() {
                let mut frame = Vec::with_capacity(8 + chunk.len());
                frame.extend_from_slice(&sent.to_be_bytes());
                frame.extend_from_slice(&chunk);
                if socket.send(Message::Binary(frame.into())).await.is_err() {
                    return;
                }
                sent += chunk.len() as u64;
                continue;
            }
            if exited && !reported_exit {
                reported_exit = true;
                if !send_json(
                    &mut socket,
                    serde_json::json!({"type": "exit", "exitCode": exit_code}),
                )
                .await
                {
                    return;
                }
            }
            if closed {
                let _ = send_json(&mut socket, serde_json::json!({"type": "closed"})).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            break;
        }
        let expiry = async {
            match &deadline {
                Some((_, at)) => tokio::time::sleep_until(*at).await,
                None => std::future::pending().await,
            }
        };
        let settle = async {
            match pending_resize {
                Some((_, _, at)) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() { return; }
            }
            _ = settle => {
                if let Some((cols, rows, _)) = pending_resize.take() {
                    session.resize(viewer, cols, rows);
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(bytes))) => {
                    if bytes.len() > max_input {
                        return;
                    }
                    // Input follows the grid this view asked for before it.
                    if let Some((cols, rows, _)) = pending_resize.take() {
                        session.resize(viewer, cols, rows);
                    }
                    session.claim(viewer);
                    if session.input.send(bytes.to_vec()).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientControl>(&text) {
                        Ok(ClientControl::Resize { cols, rows }) => {
                            // Keep the first deadline, so a steady stream of
                            // resizes still lands, with only the final grid.
                            let at = pending_resize.map_or_else(
                                || tokio::time::Instant::now() + RESIZE_SETTLE,
                                |(_, _, at)| at,
                            );
                            pending_resize = Some((cols, rows, at));
                        }
                        Err(_) => return,
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
            },
            _ = expiry => {
                let Some((token, _)) = deadline.take() else { continue };
                let pairing = state.pairing.lock().await;
                let now = now_ms();
                if pairing.verify_session(&token, None, now).is_err() {
                    drop(pairing);
                    close_access_ended(&mut socket).await;
                    return;
                }
                deadline = pairing
                    .session_expires_at_ms(&token)
                    .and_then(|expires| tokio::time::Instant::now().checked_add(
                        std::time::Duration::from_millis(expires.saturating_sub(now).max(1))))
                    .map(|at| (token, at));
            }
            revoked = revocations.recv() => match revoked {
                Ok(revoked) if revoked == device_id => {
                    close_access_ended(&mut socket).await;
                    return;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
        }
    }
}

#[cfg(all(test, unix))]
#[path = "terminal_tests.rs"]
mod tests;
