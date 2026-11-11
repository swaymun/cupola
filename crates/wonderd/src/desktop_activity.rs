//! Threads that a desktop or terminal app on this Mac is running right now.
//! Claude Code records each live interactive session in
//! `~/.claude/sessions/<pid>.json`; Codex maintains an indexed turn lifecycle.
//! Only these records are read.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const MAX_SESSION_RECORDS: usize = 256;
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_KNOWN_ROLLOUTS: usize = 2048;

/// Rollout paths learned from Codex thread listings and reads.
static CODEX_ROLLOUTS: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn remember_codex_rollout(thread: &str, path: &str) {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return;
    }
    if let Ok(mut known) = CODEX_ROLLOUTS.lock() {
        if known.len() >= MAX_KNOWN_ROLLOUTS && !known.contains_key(thread) {
            known.clear();
        }
        known.insert(thread.to_owned(), path);
    }
}

/// Tests that remember a rollout under a shared thread name forget it so a
/// deleted fixture never answers for the next test.
#[cfg(test)]
pub(crate) fn forget_codex_rollout(thread: &str) {
    if let Ok(mut known) = CODEX_ROLLOUTS.lock() {
        known.remove(thread);
    }
}

pub(crate) fn codex_rollout(thread: &str) -> Option<PathBuf> {
    CODEX_ROLLOUTS.lock().ok()?.get(thread).cloned()
}

/// Indexed native lifecycle, or unknown if the producer's evidence cannot be read.
pub(crate) async fn codex_thread_activity(
    home: &Path,
    thread: &str,
) -> Option<crate::codex_activity::Activity> {
    crate::codex_activity::read(home, thread, &codex_rollout(thread)?).await
}

/// Whether the thread's latest turn runs, for list rows: Codex's own rollout
/// and index decide, not when Wonder last used the thread.
pub(crate) async fn codex_thread_working(home: &Path, thread: &str) -> bool {
    match codex_rollout(thread) {
        Some(path) => crate::codex_activity::working(home, thread, &path).await,
        None => false,
    }
}

/// Native active state proves work in this process. Idle describes only this
/// process: a known shared rollout requires indexed evidence before admission.
pub(crate) async fn codex_thread_busy(
    home: &Path,
    thread: &str,
    runtime: Option<&wonder_app_server::RpcClient>,
) -> Option<bool> {
    if let Some(runtime) = runtime {
        let native = tokio::time::timeout(std::time::Duration::from_millis(750), async {
            let response = runtime
                .request(
                    "thread/read",
                    serde_json::json!({"threadId":thread,"includeTurns":false}),
                )
                .await
                .ok()?;
            if let Some(read) = response.result.filter(|_| runtime.health().is_alive()) {
                let value = read.get("thread").unwrap_or(&read);
                if value.get("id").and_then(serde_json::Value::as_str) == Some(thread) {
                    if let Some(path) = value.get("path").and_then(serde_json::Value::as_str) {
                        remember_codex_rollout(thread, path);
                    }
                    let loaded = codex_loaded_busy(value);
                    if loaded == Some(true) {
                        return loaded;
                    }
                    // A session loaded idle here can still run in another
                    // App Server. Shared producer evidence must arbitrate it.
                    if codex_rollout(thread).is_some() {
                        return None;
                    }
                    if loaded == Some(false) {
                        return loaded;
                    }
                    // A recorded completion is already native evidence. Older
                    // imported sessions need no private index just to continue.
                    let response = runtime
                        .request(
                            "thread/turns/list",
                            serde_json::json!({
                                "threadId":thread,"limit":1,"itemsView":"notLoaded"
                            }),
                        )
                        .await
                        .ok()?;
                    let result = response.result.filter(|_| runtime.health().is_alive())?;
                    let latest = result.get("data")?.as_array()?.first()?;
                    if matches!(
                        latest.get("status").and_then(serde_json::Value::as_str),
                        Some("completed" | "failed" | "interrupted")
                    ) && latest
                        .get("completedAt")
                        .and_then(serde_json::Value::as_i64)
                        .is_some_and(|at| at >= 0)
                    {
                        return Some(false);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        if native.is_some() {
            return native;
        }
    }
    crate::codex_activity::busy(home, thread, &codex_rollout(thread)?).await
}

pub(crate) fn codex_loaded_busy(thread: &serde_json::Value) -> Option<bool> {
    match thread
        .pointer("/status/type")
        .and_then(serde_json::Value::as_str)
    {
        Some("active") => Some(true),
        Some("idle") => Some(false),
        _ => None,
    }
}

pub(crate) fn claude_sessions_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(path).join("sessions"));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude/sessions"))
}

/// Sessions whose reply Claude Code on the Mac is writing right now. Its
/// record says "busy" also while only background agents run after the reply
/// ended; the transcript's last entry tells the two apart, so background work
/// alone does not show the chat as working.
/// `waiting` holds sessions showing a permission prompt or question on the Mac.
pub(crate) async fn claude_busy_sessions() -> ClaudeDesktopSessions {
    let Some(dir) = claude_sessions_dir() else {
        return ClaudeDesktopSessions::default();
    };
    let projects = claude_projects_dir();
    tokio::task::spawn_blocking(move || {
        #[allow(unused_mut)]
        let mut sessions = busy_sessions_with_transcripts(&dir, projects.as_deref());
        // Tests add private record folders for their own sessions.
        #[cfg(all(test, unix))]
        for (dir, projects) in tests::BUSY_DIRS.lock().unwrap().clone() {
            let extra = busy_sessions_with_transcripts(&dir, Some(&projects));
            sessions.busy.extend(extra.busy);
            sessions.waiting.extend(extra.waiting);
            sessions.background.extend(extra.background);
        }
        sessions
    })
    .await
    .unwrap_or_default()
}

#[derive(Debug, Default)]
pub(crate) struct ClaudeDesktopSessions {
    pub(crate) busy: HashSet<String>,
    pub(crate) waiting: HashSet<String>,
    /// Sessions whose process on the Mac still runs background agents,
    /// commands or monitors after its reply ended (`Holder::background`).
    pub(crate) background: HashSet<String>,
}

fn busy_sessions_with_transcripts(dir: &Path, projects: Option<&Path>) -> ClaudeDesktopSessions {
    let mut sessions = ClaudeDesktopSessions::default();
    for record in live_records_in(dir) {
        if record.status == "waiting" {
            sessions.waiting.insert(record.session.clone());
        }
        let writing = record.writing(projects);
        if record.background_after(writing) {
            sessions.background.insert(record.session.clone());
        }
        if writing {
            sessions.busy.insert(record.session);
        }
    }
    sessions
}

/// The live records folder and transcripts folder a session is read from.
/// Tests give their own sessions private folders, so they never read or
/// signal the owner's real Claude Code processes.
fn claude_dirs(session: &str) -> Option<(PathBuf, Option<PathBuf>)> {
    #[cfg(all(test, unix))]
    if let Some((sessions, projects)) = tests::SESSION_DIRS.lock().unwrap().get(session) {
        return Some((sessions.clone(), Some(projects.clone())));
    }
    let _ = session;
    Some((claude_sessions_dir()?, claude_projects_dir()))
}

fn claude_projects_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(path).join("projects"));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude/projects"))
}

/// The transcript Claude Code keeps for this session in this working folder.
pub(crate) fn claude_transcript(cwd: &str, session: &str) -> Option<PathBuf> {
    if !valid_session_id(session) || !cwd.starts_with('/') {
        return None;
    }
    Some(
        claude_dirs(session)?
            .1?
            .join(project_folder_name(cwd))
            .join(format!("{session}.jsonl")),
    )
}

/// `claude_transcript` in the folder this session's records are read from
/// (a test's own folder), for matching it to T3 Code's thread.
pub(crate) fn claude_session_transcript(session: &str, cwd: &str) -> Option<PathBuf> {
    let (_, projects) = claude_dirs(session)?;
    if !valid_session_id(session) || !cwd.starts_with('/') {
        return None;
    }
    Some(
        projects?
            .join(project_folder_name(cwd))
            .join(format!("{session}.jsonl")),
    )
}

/// `claude_live_sessions` for a thread that may block.
pub(crate) fn claude_live_sessions_now() -> HashMap<String, bool> {
    claude_sessions_dir()
        .map(|dir| live_sessions_in(&dir))
        .unwrap_or_default()
}

/// Claude Code names a project's transcript folder after its working folder.
fn project_folder_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

/// The shared reader records the main-turn state as each complete line arrives.
/// An unreadable, incomplete or oversized newest record remains unknown.
pub(crate) fn main_turn_running(path: &Path) -> Option<bool> {
    transcript::with(path, |tail| tail.running()).flatten()
}

// Retain the coarse-clock guard for files that were just written. It checks
// bytes at the cursor, without parsing an unchanged transcript again.
#[cfg(not(test))]
const TRANSCRIPT_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
pub(crate) const TRANSCRIPT_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// One cursor and one ingestion pass per native transcript. Consumers ask for
/// projections, never their own tail scan. Only complete records advance the
/// cursor; replacing/truncating/rewriting a file discards all its projections.
pub(crate) mod transcript {
    use super::*;
    use std::io::{BufRead, Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::time::SystemTime;

    const MAX_FILES: usize = 256;
    const MAX_FILE_BYTES: u64 = 2 << 30;
    const MAX_LINE_BYTES: usize = 64 << 20;
    // A cold read can yield between complete records. Incomplete projections
    // never prove idle; subsequent polls continue at the cursor.
    const READ_BYTES: u64 = 16 << 20;
    const GUARD_BYTES: u64 = 1024;
    const MAX_IMAGES: usize = 16_384;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Revision {
        file: (u64, u64),
        length: u64,
        modified: SystemTime,
        changed: (i64, i64),
    }

    impl Revision {
        fn of(metadata: &std::fs::Metadata) -> Option<Self> {
            let stamp = crate::platform::file_stamp(metadata);
            metadata.is_file().then_some(Self {
                file: (stamp.device, stamp.inode),
                length: metadata.len(),
                modified: metadata.modified().ok()?,
                changed: stamp.changed,
            })
        }

        fn settled(&self) -> bool {
            let changed = u64::try_from(self.changed.0).ok().map(|seconds| {
                SystemTime::UNIX_EPOCH + std::time::Duration::new(seconds, self.changed.1 as u32)
            });
            SystemTime::now()
                .duration_since(changed.map_or(self.modified, |c| c.max(self.modified)))
                .is_ok_and(|age| age >= TRANSCRIPT_SETTLE)
        }
    }

    #[derive(Default)]
    struct Readers {
        tick: u64,
        files: HashMap<PathBuf, (u64, Arc<Mutex<Tail>>)>,
    }

    static READERS: LazyLock<Mutex<Readers>> = LazyLock::new(Default::default);

    /// Reduced answers and two bounded byte guards. The guards detect prefix
    /// and boundary rewrites during growth; file revisions fence other rewrites.
    pub(crate) struct Tail {
        revision: Option<Revision>,
        offset: u64,
        guard: Vec<u8>,
        prefix: Vec<u8>,
        caught_up: bool,
        unfinished: bool,
        skipping: bool,
        main: Option<bool>,
        usage: crate::context_usage::TranscriptUsage,
        images: HashMap<[u8; 32], (u64, u64)>,
    }

    impl Default for Tail {
        fn default() -> Self {
            Self {
                revision: None,
                offset: 0,
                guard: Vec::new(),
                prefix: Vec::new(),
                caught_up: false,
                unfinished: false,
                skipping: false,
                main: Some(false),
                usage: Default::default(),
                images: HashMap::new(),
            }
        }
    }

    /// The registry lock covers lookup only. Different transcripts can ingest
    /// concurrently; simultaneous readers of one path join the same cursor.
    pub(crate) fn with<T>(path: &Path, answer: impl FnOnce(&Tail) -> T) -> Option<T> {
        let reader = {
            let mut readers = READERS.lock().unwrap_or_else(|e| e.into_inner());
            readers.tick = readers.tick.wrapping_add(1);
            let tick = readers.tick;
            if !readers.files.contains_key(path) && readers.files.len() >= MAX_FILES {
                let oldest = readers
                    .files
                    .iter()
                    .filter(|(_, (_, reader))| Arc::strong_count(reader) == 1)
                    .min_by_key(|(_, (used, _))| used)
                    .map(|(path, _)| path.clone())?;
                readers.files.remove(&oldest);
            }
            let (used, reader) = readers.files.entry(path.to_owned()).or_default();
            *used = tick;
            reader.clone()
        };
        let mut tail = reader.lock().unwrap_or_else(|e| e.into_inner());
        if tail.refresh(path).is_none() {
            *tail = Tail::default();
            return None;
        }
        Some(answer(&tail))
    }

    /// A media hash check independently detected a rewrite. Discard every
    /// projection, even if the filesystem's timestamps did not expose it.
    pub(crate) fn invalidate(path: &Path) {
        let reader = READERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .get(path)
            .map(|(_, reader)| reader.clone());
        if let Some(reader) = reader {
            *reader.lock().unwrap_or_else(|e| e.into_inner()) = Tail::default();
        }
    }

    impl Tail {
        pub(crate) fn running(&self) -> Option<bool> {
            self.caught_up.then_some(self.main).flatten()
        }

        pub(crate) fn context(
            &self,
            source: &crate::context_usage::Source,
        ) -> crate::context_usage::FileReading {
            if self.caught_up {
                self.usage.reading(source)
            } else {
                crate::context_usage::FileReading::Incomplete
            }
        }

        pub(crate) fn image(&self, digest: &[u8; 32]) -> Option<(u64, u64)> {
            self.images.get(digest).copied()
        }

        fn refresh(&mut self, path: &Path) -> Option<()> {
            let revision = Revision::of(&std::fs::metadata(path).ok()?)?;
            if revision.length > MAX_FILE_BYTES {
                return None;
            }
            if self.revision.as_ref() == Some(&revision) && revision.settled() {
                // A partial line must wait for an append, but a budget-limited
                // read has complete records left to ingest at this revision.
                if self.caught_up || self.offset == revision.length || self.partial() {
                    return Some(());
                }
            }
            #[cfg(all(test, unix))]
            {
                *tests::TRANSCRIPT_READS
                    .lock()
                    .unwrap()
                    .entry(path.to_owned())
                    .or_default() += 1;
            }
            let (mut file, _) = crate::media_refs::open_regular(path)?;
            if Revision::of(&file.metadata().ok()?)? != revision {
                return None;
            }
            let same_file = self.revision.as_ref().is_some_and(|previous| {
                previous.file == revision.file
                    && previous.length <= revision.length
                    && (previous.length < revision.length || previous == &revision)
            });
            let guards_match = same_file
                && read_at(&mut file, 0, self.prefix.len())? == self.prefix
                && read_at(
                    &mut file,
                    self.revision
                        .as_ref()
                        .map_or(0, |r| r.length)
                        .saturating_sub(self.guard.len() as u64),
                    self.guard.len(),
                )? == self.guard;
            if !guards_match {
                *self = Self::default();
            }
            if guards_match && self.revision.as_ref() == Some(&revision) && self.unfinished {
                if Revision::of(&file.metadata().ok()?)? != revision {
                    return None;
                }
                return Some(());
            }
            let start = self.offset;
            file.seek(SeekFrom::Start(start)).ok()?;
            let mut reader =
                std::io::BufReader::with_capacity(64 << 10, file.take(revision.length - start));
            let mut line = Vec::new();
            #[cfg(all(test, unix))]
            let mut ingested = 0;
            self.caught_up = false;
            self.unfinished = false;
            while self.offset < revision.length && self.offset - start < READ_BYTES {
                if self.skipping {
                    let buffer = reader.fill_buf().ok()?;
                    if buffer.is_empty() {
                        return None;
                    }
                    let end = memchr::memchr(b'\n', buffer);
                    let len = end.map_or(buffer.len(), |at| at + 1);
                    reader.consume(len);
                    self.offset += len as u64;
                    self.skipping = end.is_none();
                    continue;
                }
                line.clear();
                let read = (&mut reader)
                    .take(MAX_LINE_BYTES as u64)
                    .read_until(b'\n', &mut line)
                    .ok()?;
                if read == 0 {
                    return None;
                }
                if line.last() != Some(&b'\n') {
                    if read < MAX_LINE_BYTES {
                        self.unfinished = true;
                        break;
                    }
                    // Stream past oversized records over bounded polls, rather
                    // than letting one unterminated record monopolize a reader.
                    self.main = None;
                    self.usage.incomplete();
                    self.offset += read as u64;
                    self.skipping = true;
                    continue;
                }
                self.accept(&line);
                #[cfg(all(test, unix))]
                {
                    ingested += read;
                }
                self.offset += read as u64;
            }
            let mut file = reader.into_inner().into_inner();
            self.guard = read_at(
                &mut file,
                revision.length.saturating_sub(GUARD_BYTES),
                revision.length.min(GUARD_BYTES) as usize,
            )?;
            self.prefix = read_at(&mut file, 0, self.offset.min(GUARD_BYTES) as usize)?;
            let after = Revision::of(&file.metadata().ok()?)?;
            if Revision::of(&std::fs::metadata(path).ok()?)? != after
                || after.file != revision.file
                || after.length < revision.length
                || (after.length == revision.length && after != revision)
            {
                return None;
            }
            // An append during ingestion leaves these complete records useful,
            // but cannot prove the latest activity/usage. Continue next poll,
            // instead of restarting a long, actively growing transcript.
            self.caught_up = self.offset == after.length && !self.skipping;
            self.revision = Some(revision);
            #[cfg(all(test, unix))]
            {
                *tests::INGESTED_BYTES
                    .lock()
                    .unwrap()
                    .entry(path.to_owned())
                    .or_default() += ingested;
            }
            Some(())
        }

        fn partial(&self) -> bool {
            self.unfinished
        }

        fn accept(&mut self, line: &[u8]) {
            if self.images.len() < MAX_IMAGES {
                crate::media_refs::payloads(line, self.offset, &mut self.images, MAX_IMAGES);
            }
            // Preserve the existing bounded-observation contract. Large image
            // records are indexed without allocating a second decoded JSON tree.
            if line.len() as u64 > TRANSCRIPT_TAIL_BYTES {
                self.main = None;
                self.usage.incomplete();
                return;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                return;
            }
            // Validate metadata-only records without allocating their unused
            // text/payload trees. The reducers only consume these native kinds.
            #[derive(serde::Deserialize)]
            struct Kind<'a> {
                #[serde(borrow, rename = "type")]
                kind: Option<std::borrow::Cow<'a, str>>,
            }
            if let Ok(Kind { kind }) = serde_json::from_slice::<Kind<'_>>(line) {
                if !matches!(
                    kind.as_deref(),
                    Some("assistant" | "user" | "system" | "compacted" | "event_msg")
                ) {
                    return;
                }
            }
            let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
                self.main = None;
                self.usage.incomplete();
                return;
            };
            self.usage.accept(&entry);
            if main_message(&entry) {
                match entry.get("type").and_then(serde_json::Value::as_str) {
                    Some("assistant") => self.main = Some(assistant_continues(&entry)),
                    Some("user") => {
                        self.main = Some(!interrupted(&entry) && !local_command_output(&entry))
                    }
                    _ => {}
                }
            }
        }
    }

    fn read_at(file: &mut std::fs::File, offset: u64, length: usize) -> Option<Vec<u8>> {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }
}

/// Sessions a live Claude Code process (desktop or terminal) has open, and
/// whether each has work running: a reply, a dialog, or background agents,
/// commands or monitors.
pub(crate) async fn claude_live_sessions() -> HashMap<String, bool> {
    let Some(dir) = claude_sessions_dir() else {
        return HashMap::new();
    };
    tokio::task::spawn_blocking(move || live_sessions_in(&dir))
        .await
        .unwrap_or_default()
}

/// What a Claude Code session on this Mac (not Wonder's own runs) is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeSessionActivity {
    pub(crate) state: ExternalActivity,
    /// Background agents, commands or monitors still run after its reply,
    /// and may wake it again.
    pub(crate) background: bool,
    /// When the record's status last changed, in ms since the epoch.
    pub(crate) status_at: Option<i64>,
    pub(crate) cwd: Option<String>,
    /// The Claude Code process holding the session, to tell an exit from a
    /// record that could not be read.
    pub(crate) process: ProcessIdentity,
}

/// A process ID with its start time, which fences PID reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pid: i32,
    start: Option<(u64, u64)>,
}

impl ProcessIdentity {
    /// Known to have exited: no process has this ID, or another process
    /// started under it. Unknown start times are never an exit.
    pub(crate) fn exited(&self) -> bool {
        if !process_alive(self.pid) {
            return true;
        }
        match (self.start, process_start(self.pid)) {
            (Some(then), Some(now)) => then != now,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExternalActivity {
    Idle,
    /// Writing the chat's reply.
    Working,
    /// Showing a dialog: a permission prompt or a question.
    Waiting,
}

/// The activity of each of `followed` that a live Claude Code process has
/// open. A session open in several processes reports its most urgent state.
/// Only those sessions' transcripts are read.
pub(crate) fn claude_session_activity(
    followed: &HashSet<String>,
) -> HashMap<String, ClaudeSessionActivity> {
    let Some(dir) = claude_sessions_dir() else {
        return HashMap::new();
    };
    session_activity_in(&dir, claude_projects_dir().as_deref(), followed)
}

pub(crate) fn session_activity_in(
    dir: &Path,
    projects: Option<&Path>,
    followed: &HashSet<String>,
) -> HashMap<String, ClaudeSessionActivity> {
    let mut sessions: HashMap<String, ClaudeSessionActivity> = HashMap::new();
    for record in live_records_in(dir) {
        if !record.known_status() || !followed.contains(&record.session) {
            continue;
        }
        let writing = record.status != "waiting" && record.writing(projects);
        let state = if record.status == "waiting" {
            ExternalActivity::Waiting
        } else if writing {
            ExternalActivity::Working
        } else {
            ExternalActivity::Idle
        };
        let mut activity = ClaudeSessionActivity {
            state,
            background: record.background_after(writing),
            status_at: record.status_at,
            cwd: record.cwd.clone(),
            process: ProcessIdentity {
                pid: record.pid,
                start: record.process_start,
            },
        };
        match sessions.get_mut(&record.session) {
            Some(existing) if existing.state >= state => existing.background |= activity.background,
            Some(existing) => {
                activity.background |= existing.background;
                *existing = activity;
            }
            None => {
                sessions.insert(record.session, activity);
            }
        }
    }
    sessions
}

/// A message in the session's main conversation: not a subagent's, nor
/// context Claude Code adds for itself (meta entries, compaction summaries).
fn main_message(entry: &serde_json::Value) -> bool {
    let flag = |name: &str| entry.get(name).and_then(serde_json::Value::as_bool) == Some(true);
    !(flag("isSidechain")
        || flag("isMeta")
        || flag("is_meta")
        || flag("isCompactSummary")
        || entry
            .get("parent_tool_use_id")
            .is_some_and(|value| !value.is_null()))
}

/// A reply that stopped to call a tool (or has no stop yet) goes on.
fn assistant_continues(entry: &serde_json::Value) -> bool {
    let reason = entry
        .pointer("/message/stop_reason")
        .and_then(serde_json::Value::as_str);
    matches!(reason, None | Some("tool_use" | "pause_turn"))
}

fn text_blocks(entry: &serde_json::Value) -> impl Iterator<Item = &str> {
    let content = entry.pointer("/message/content");
    let whole = content.and_then(serde_json::Value::as_str);
    let blocks = content
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str));
    whole.into_iter().chain(blocks)
}

fn interrupted(entry: &serde_json::Value) -> bool {
    text_blocks(entry).next().is_some_and(|text| {
        text.trim_start()
            .starts_with("[Request interrupted by user")
    })
}

/// Who wrote a user message in a Claude Code session's main conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserEntry {
    /// The owner's prompt, typed in Claude, a terminal or an app built on
    /// Claude Code such as T3 Code.
    Prompt,
    /// Claude Code woke the session itself: a background agent, command or
    /// monitor reported back, a scheduled task ran, or another session wrote.
    Wake,
    /// A tool's result: the turn goes on.
    ToolResult,
    /// The owner stopped the reply.
    Interrupt,
    /// Output of a command that ran without a reply from Claude (a local
    /// slash command, or `!` shell input).
    Local,
}

/// What Claude Code writes in the owner's place, in transcripts that predate
/// its `origin` field.
const GENERATED_TAGS: [&str; 3] = ["<task-notification", "<scheduled-task", "<ci-monitor-event"];
const LOCAL_TAGS: [&str; 6] = [
    "<local-command-stdout",
    "<local-command-stderr",
    "<local-command-caveat",
    "<bash-input",
    "<bash-stdout",
    "<bash-stderr",
];

/// The result a command that ran without a reply from Claude printed: it
/// ends the command's turn. `/compact`, which T3 Code sends on its own
/// before the prompt cache expires, ends on one with no assistant entry after
/// it, so the turn would otherwise stay running for good.
fn local_command_output(entry: &serde_json::Value) -> bool {
    const OUTPUT_TAGS: [&str; 4] = [
        "<local-command-stdout",
        "<local-command-stderr",
        "<bash-stdout",
        "<bash-stderr",
    ];
    let text: String = text_blocks(entry).collect();
    let text = text.trim_start();
    OUTPUT_TAGS.iter().any(|tag| text.starts_with(tag))
}

pub(crate) fn user_entry(entry: &serde_json::Value) -> UserEntry {
    if interrupted(entry) {
        return UserEntry::Interrupt;
    }
    if entry
        .pointer("/message/content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"))
    {
        return UserEntry::ToolResult;
    }
    let text: String = text_blocks(entry).collect();
    let text = text.trim_start();
    if LOCAL_TAGS.iter().any(|tag| text.starts_with(tag)) {
        return UserEntry::Local;
    }
    let field = |pointer: &str| entry.pointer(pointer).and_then(serde_json::Value::as_str);
    // Claude Code records who started a turn (2.1.281 and later).
    if let Some(kind) = field("/origin/kind") {
        return if kind == "human" {
            UserEntry::Prompt
        } else {
            UserEntry::Wake
        };
    }
    if !matches!(field("/turnOrigin"), None | Some("human" | "sdk"))
        || field("/promptSource") == Some("system")
        || GENERATED_TAGS.iter().any(|tag| text.starts_with(tag))
        || only_reminders(text)
    {
        return UserEntry::Wake;
    }
    UserEntry::Prompt
}

/// Text made only of `<system-reminder>` blocks, with no words from the owner.
fn only_reminders(mut text: &str) -> bool {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut any = false;
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return any;
        }
        let Some(end) = text.strip_prefix(OPEN).and_then(|rest| rest.find(CLOSE)) else {
            return false;
        };
        text = &text[OPEN.len() + end + CLOSE.len()..];
        any = true;
    }
}

/// Unread transcript beyond this is skipped: the read begins again at its
/// end, as a first read does.
const MAX_UNREAD_BYTES: u64 = 32 * 1024 * 1024;

/// The owner's newest prompt in a Claude Code session and the reply the
/// conversation ended on, read forward through its transcript poll by poll.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptLog {
    read: bool,
    /// Just past the last complete line read, or inside the record the
    /// first read began in (`partial`).
    offset: u64,
    /// The rest of that record is still to be skipped.
    partial: bool,
    /// The newest prompt the owner sent (its entry's uuid). Cleared when the
    /// owner stopped its reply, or it was a command that needs no reply.
    pub(crate) prompt: Option<String>,
    /// Claude has written to the conversation since that prompt.
    pub(crate) replied: bool,
    /// The newest turn was started by that prompt, not by a wakeup.
    pub(crate) owner_turn: bool,
    /// The main turn is unfinished (as `main_turn_running` tells it).
    pub(crate) running: bool,
    /// The text the conversation's newest reply ended on. `None` when its
    /// last step was a tool call with no text after it: earlier text in the
    /// reply was commentary, not its answer.
    pub(crate) reply: Option<String>,
    /// The API message `reply` came from (Claude Code writes a line per block).
    reply_message: Option<String>,
    /// Background agents, commands and monitors the conversation started
    /// since that prompt that have not reported back yet, by task ID. A
    /// persistent monitor never ends and is not kept.
    pub(crate) tasks: HashSet<String>,
    /// Task reports Claude Code has queued since that prompt that no turn
    /// has taken in yet, oldest first. Each wakes the session (or joins the
    /// running turn).
    pub(crate) queued: Vec<String>,
}

/// At most this many unfinished background tasks, and as many queued
/// reports, are kept per session.
const MAX_TASKS: usize = 256;

/// The background task a tool result started, which reports back when it
/// ends: a background command, an agent run in the background, or a monitor
/// that expires.
fn task_started(entry: &serde_json::Value) -> Option<&str> {
    let result = entry.get("toolUseResult")?;
    let field = |name: &str| result.get(name).and_then(serde_json::Value::as_str);
    field("backgroundTaskId")
        .or_else(|| field("agentId").filter(|_| field("status") == Some("async_launched")))
        .or_else(|| {
            field("taskId").filter(|_| {
                result
                    .get("persistent")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
            })
        })
}

/// The tasks each task notification in `text` names, and whether it reports
/// them as ended: it carries a status, which a monitor's event does not.
fn task_reports(text: &str) -> impl Iterator<Item = (&str, bool)> {
    text.split("<task-notification>")
        .skip(1)
        .map(|body| body.split("</task-notification>").next().unwrap_or(body))
        .flat_map(|body| {
            let ended = body.contains("<status>");
            body.split("<task-id>").skip(1).filter_map(move |rest| {
                rest.split_once("</task-id>")
                    .map(|(id, _)| (id.trim(), ended))
            })
        })
}

impl PromptLog {
    /// Reads the lines added since the previous read. The first read (or one
    /// after the file shrank, or grew by more than `MAX_UNREAD_BYTES`) reads
    /// nothing and knows nothing: it begins at the transcript's end, and the
    /// log follows only what is written after it. History it did not read in
    /// full cannot tell which tasks still run or which reports are still to
    /// come. A line still being written waits for the next read. False when
    /// the file cannot be read.
    pub(crate) fn read(&mut self, path: &Path) -> bool {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        let Ok(length) = file.metadata().map(|meta| meta.len()) else {
            return false;
        };
        if !self.read || length < self.offset || length - self.offset > MAX_UNREAD_BYTES {
            // The newest record may still be being written.
            let mut last = *b"\n";
            if length > 0
                && (file.seek(SeekFrom::Start(length - 1)).is_err()
                    || file.read_exact(&mut last).is_err())
            {
                return false;
            }
            *self = Self {
                read: true,
                offset: length,
                partial: last[0] != b'\n',
                ..Self::default()
            };
            return true;
        }
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err()
            || file
                .take(length - self.offset)
                .read_to_end(&mut bytes)
                .is_err()
        {
            return false;
        }
        let mut position = self.offset;
        let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
        if self.partial {
            if let Some(rest) = lines.next() {
                position += rest.len() as u64;
                self.partial = !rest.ends_with(b"\n");
            }
        }
        for line in lines {
            if !line.ends_with(b"\n") {
                break;
            }
            self.accept(line, position);
            position += line.len() as u64;
        }
        self.offset = position;
        true
    }

    fn accept(&mut self, line: &[u8], position: u64) {
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
            return;
        };
        if !main_message(&entry) {
            return;
        }
        // A task's report is queued as soon as it is sent. An idle session
        // takes it in as a new turn; a running turn takes it in as an
        // attachment, and its queue entry is then removed.
        let field = |pointer: &str| entry.pointer(pointer).and_then(serde_json::Value::as_str);
        match (field("/type"), field("/operation")) {
            (Some("queue-operation"), Some("enqueue")) => {
                if let Some(text) =
                    field("/content").filter(|text| task_reports(text).next().is_some())
                {
                    self.end_tasks(text);
                    if self.queued.len() < MAX_TASKS {
                        self.queued.push(text.to_owned());
                    }
                }
            }
            (Some("queue-operation"), Some("remove")) => {
                if let Some(text) = field("/content") {
                    self.take_in(text);
                }
            }
            (Some("attachment"), _) => {
                if let Some(text) = field("/attachment/prompt") {
                    self.take_in(text);
                }
            }
            _ => {}
        }
        match entry.get("type").and_then(serde_json::Value::as_str) {
            Some("user") => match user_entry(&entry) {
                UserEntry::Prompt => {
                    let id = entry.get("uuid").and_then(serde_json::Value::as_str);
                    self.prompt = Some(id.map_or_else(|| format!("@{position}"), str::to_owned));
                    self.replied = false;
                    self.owner_turn = true;
                    self.running = true;
                    self.reply = None;
                    self.reply_message = None;
                    // The owner has moved on: work earlier prompts left
                    // outstanding, or reports that may never be taken in,
                    // never hold this prompt.
                    self.tasks.clear();
                    self.queued.clear();
                }
                UserEntry::Wake => {
                    self.owner_turn = false;
                    self.running = true;
                    for text in text_blocks(&entry) {
                        self.take_in(text);
                    }
                }
                UserEntry::ToolResult => {
                    self.running = true;
                    if let Some(task) = task_started(&entry) {
                        if self.tasks.len() < MAX_TASKS {
                            self.tasks.insert(task.to_owned());
                        }
                    }
                    // A task the conversation stopped itself.
                    if let Some(task) = entry
                        .pointer("/toolUseResult/task_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        self.tasks.remove(task);
                    }
                }
                UserEntry::Interrupt => {
                    self.running = false;
                    self.prompt = None;
                }
                UserEntry::Local => {
                    // A command that Claude does not answer (`/compact`,
                    // `/model`) is over once it prints its result.
                    if local_command_output(&entry) {
                        self.running = false;
                    }
                    if !self.replied {
                        self.prompt = None;
                    }
                }
            },
            Some("assistant") => {
                self.running = assistant_continues(&entry);
                self.replied = self.prompt.is_some();
                self.take_reply(&entry);
            }
            _ => {}
        }
    }

    fn end_tasks(&mut self, text: &str) {
        for (task, _) in task_reports(text).filter(|(_, ended)| *ended) {
            self.tasks.remove(task);
        }
    }

    /// A turn took in a report (the same text Claude Code queued). Older
    /// reports from its tasks still queued will not come: Claude Code
    /// dropped them for it (seen with a command's progress report).
    fn take_in(&mut self, text: &str) {
        self.end_tasks(text);
        let Some(at) = self.queued.iter().position(|queued| queued == text) else {
            return;
        };
        let tasks: HashSet<&str> = task_reports(text).map(|(task, _)| task).collect();
        let mut index = 0;
        self.queued.retain(|queued| {
            index += 1;
            index > at + 1 || !task_reports(queued).any(|(task, _)| tasks.contains(task))
        });
    }

    fn take_reply(&mut self, entry: &serde_json::Value) {
        let message = entry
            .pointer("/message/id")
            .and_then(serde_json::Value::as_str);
        let blocks = entry
            .pointer("/message/content")
            .and_then(serde_json::Value::as_array);
        for block in blocks.into_iter().flatten() {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    let text = block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        continue;
                    }
                    match &mut self.reply {
                        Some(reply)
                            if message.is_some() && self.reply_message.as_deref() == message =>
                        {
                            reply.push('\n');
                            reply.push_str(text);
                        }
                        _ => self.reply = Some(text.to_owned()),
                    }
                    self.reply_message = message.map(str::to_owned);
                }
                Some("tool_use" | "server_tool_use") => {
                    self.reply = None;
                    self.reply_message = None;
                }
                _ => {}
            }
        }
    }
}

/// The app on this Mac that has a Claude chat open, as the phone names it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Holder {
    /// What the owner knows it as: "Claude", "T3 Code", "Claude Code in Terminal".
    pub(crate) app: String,
    pub(crate) kind: HolderKind,
    /// It is writing a reply (or shows a dialog) in the chat.
    pub(crate) replying: bool,
    /// Background agents, commands or monitors still run in its process.
    pub(crate) background: bool,
    /// A message sent normally waits for it (see `holds`).
    pub(crate) waits: bool,
    /// Send now can end it: every process for the chat is verified as Claude
    /// Code by pid, start time and executable.
    pub(crate) can_take_over: bool,
}

impl Holder {
    /// The holder as the owner's Claude app sync setting has it: off, a chat
    /// open in the Claude app waits like one open in a terminal (`holds`).
    /// `claude_holder` reads it with sync on.
    pub(crate) fn with_app_sync(mut self, app_sync: bool) -> Self {
        self.waits |= !app_sync && self.kind == HolderKind::Desktop;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum HolderKind {
    /// The Claude desktop app.
    Desktop,
    /// Another app built on Claude Code, such as T3 Code.
    App,
    /// Claude Code in a terminal.
    Terminal,
}

/// Who has this chat open on the Mac, if any app does (Wonder's own runs
/// excluded). Display only: a partial scan names what it could read.
pub(crate) async fn claude_holder(session: &str) -> Option<Holder> {
    let (dir, projects) = claude_dirs(session)?;
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || holder_in(&dir, projects.as_deref(), &session))
        .await
        .ok()
        .flatten()
}

fn holder_in(dir: &Path, projects: Option<&Path>, session: &str) -> Option<Holder> {
    #[cfg(all(test, unix))]
    {
        *tests::HOLDER_SCANS
            .lock()
            .unwrap()
            .entry(session.to_owned())
            .or_default() += 1;
    }
    let mut records: Vec<_> = live_records_in(dir)
        .into_iter()
        .filter(|record| record.session == session)
        .collect();
    // The process a message waits for names the chat: a terminal first.
    records.sort_by_key(|record| std::cmp::Reverse(record.kind));
    let first = records.first()?;
    Some(Holder {
        app: holder_app(first.kind, first.pid),
        kind: match first.kind {
            Kind::Desktop => HolderKind::Desktop,
            Kind::SdkApp => HolderKind::App,
            Kind::Terminal => HolderKind::Terminal,
        },
        replying: records.iter().any(|record| record.writing(projects)),
        background: records
            .iter()
            .any(|record| record.background_work(projects)),
        waits: holds(&records, projects, true).is_some(),
        can_take_over: records.iter().all(LiveRecord::verified),
    })
}

/// The name the owner knows a Claude Code process by, from the app that
/// started it: the outermost app bundle among its parent processes.
fn holder_app(kind: Kind, pid: i32) -> String {
    let host = host_app(pid);
    match kind {
        Kind::Desktop => host.unwrap_or_else(|| "Claude".to_owned()),
        Kind::SdkApp => host.unwrap_or_else(|| "Claude Code".to_owned()),
        Kind::Terminal => match host {
            Some(terminal) => format!("Claude Code in {terminal}"),
            None => "Claude Code in a terminal".to_owned(),
        },
    }
}

fn host_app(pid: i32) -> Option<String> {
    #[cfg(all(test, unix))]
    if let Some(name) = tests::HOST_APPS.lock().unwrap().get(&pid) {
        return Some(name.clone());
    }
    let mut current = pid;
    for _ in 0..16 {
        current = parent_pid(current).filter(|parent| *parent > 1)?;
        if let Some(name) = executable_path(current)
            .as_deref()
            .and_then(app_bundle_name)
        {
            return Some(name);
        }
    }
    None
}

/// "T3 Code" for "/Applications/T3 Code (Beta).app/Contents/MacOS/T3 Code
/// (Beta)": the outermost bundle, so an Electron helper names its app, without
/// a release channel suffix.
pub(crate) fn app_bundle_name(path: &str) -> Option<String> {
    let end = path.find(".app/")?;
    let name = path[..end].rsplit('/').next()?.trim();
    let name = match name.rfind(" (") {
        Some(start) if name.ends_with(')') && start > 0 => &name[..start],
        _ => name,
    };
    (!name.is_empty()).then(|| name.to_owned())
}

/// Session IDs whose live Claude Code process reports itself busy.
#[cfg(all(test, unix))]
pub(crate) fn busy_sessions_in(dir: &Path) -> HashSet<String> {
    busy_sessions_with_transcripts(dir, None).busy
}

fn live_sessions_in(dir: &Path) -> HashMap<String, bool> {
    let mut live = HashMap::new();
    for record in live_records_in(dir) {
        let working = record.background();
        *live.entry(record.session).or_insert(false) |= working;
        if live.len() >= MAX_SESSION_RECORDS {
            break;
        }
    }
    live
}

/// One live Claude Code process and the session it has open.
struct LiveRecord {
    pid: i32,
    session: String,
    /// "busy" (a reply or background agents), "shell" (only background
    /// commands or monitors), "waiting" (a dialog) or "idle".
    status: String,
    /// When `status` last changed, in ms since the epoch.
    status_at: Option<i64>,
    cwd: Option<String>,
    kind: Kind,
    process_start: Option<(u64, u64)>,
    record_start: Option<String>,
}

/// Who runs a live Claude Code process. Wonder's own Agent SDK processes are
/// never listed (see `descends_from_this_daemon`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// The Claude desktop app.
    Desktop,
    /// Another Agent SDK app, such as t3code.
    SdkApp,
    /// Claude Code in a terminal, or anything not recognised.
    Terminal,
}

impl LiveRecord {
    fn known_status(&self) -> bool {
        matches!(self.status.as_str(), "idle" | "busy" | "shell" | "waiting")
    }
    fn same_process(&self) -> bool {
        self.process_start
            .is_some_and(|start| process_start(self.pid) == Some(start))
            && self
                .record_start
                .as_deref()
                .is_some_and(|start| process_start_label(self.pid).as_deref() == Some(start))
    }
    fn busy(&self) -> bool {
        matches!(self.status.as_str(), "busy" | "waiting")
    }
    /// Whether this process is writing the chat's reply (or waits on a dialog
    /// in it), rather than only keeping background work after its reply.
    fn writing(&self, projects: Option<&Path>) -> bool {
        if !self.busy() {
            return false;
        }
        let transcript = projects
            .zip(self.cwd.as_deref())
            .map(|(projects, cwd)| transcript_in(projects, cwd, &self.session));
        // Without a readable transcript, trust the record.
        transcript
            .and_then(|path| main_turn_running(&path))
            .unwrap_or(true)
    }
    /// A process a plain send never stops, only Send now: a terminal, another
    /// Agent SDK app such as T3 Code (owner, Oct 18), or one whose executable
    /// is not the Claude Code its record names (a reused process ID).
    fn protected(&self) -> bool {
        self.kind != Kind::Desktop || !self.verified()
    }
    /// The exact process that wrote this record, still running Claude Code:
    /// the only kind Send now ever signals.
    fn verified(&self) -> bool {
        self.same_process()
            && match self.kind {
                Kind::Desktop => desktop_claude_process(self.pid),
                Kind::SdkApp | Kind::Terminal => claude_executable(self.pid),
            }
    }
    /// Background agents, commands or monitors after its reply ended.
    fn background_work(&self, projects: Option<&Path>) -> bool {
        self.background_after(self.writing(projects))
    }
    /// `background_work`, given whether its reply is being written.
    fn background_after(&self, writing: bool) -> bool {
        self.status == "shell" || (self.status == "busy" && !writing)
    }
    /// A reply or dialog that started after `sent_at_ms`: the Mac sent last.
    fn busy_since(&self, sent_at_ms: i64) -> bool {
        !self.known_status() || (self.busy() && self.status_at.is_none_or(|at| at > sent_at_ms))
    }
    /// Work that reports back into this process's copy of the conversation.
    fn background(&self) -> bool {
        self.status != "idle"
    }
}

/// Why Wonder cannot continue a Claude chat right now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopRefusal {
    /// Claude on the Mac started its reply after Wonder's message was sent,
    /// or the process changed between the check and the signal.
    Busy,
    /// It is open in Claude Code in a terminal or another Agent SDK app such
    /// as T3 Code (or an unrecognised process), which only Send now stops.
    Protected,
    /// Native session evidence is incomplete; admission must wait.
    Unknown,
    /// The process for it did not exit.
    StillRunning,
    /// Its process still runs background agents, commands or monitors, which
    /// only Send now ends.
    Background,
    /// Send now found a process for the chat it cannot prove is Claude Code
    /// (a reused process ID): nothing is signalled.
    Unverified,
    /// The process that has the thread open also runs other threads (a
    /// `codex app-server`, whichever app runs it): ending it would end them
    /// too, so it is never signalled.
    Shared,
}

/// Whether Claude on the Mac holds this chat, so a Wonder message waits: the
/// chat is open in Claude Code in a terminal or another Agent SDK app (T3
/// Code), or background agents, commands or monitors still run in the app
/// that has it open. Only reads. Otherwise the desktop app never holds it:
/// the last sender wins, so Wonder's message stops its reply (see
/// `end_desktop_session`).
/// With `sent_at_ms` (a message waiting to be dispatched), a reply Claude on
/// the Mac started after it also holds it: the Mac sent last. Send now takes
/// any of these over (see `take_over_claude_session`).
/// With Claude app sync off (`app_sync`), the desktop app holds it too.
pub(crate) async fn claude_holds_session(
    session: &str,
    sent_at_ms: Option<i64>,
    app_sync: bool,
) -> Option<StopRefusal> {
    let Some((dir, projects)) = claude_dirs(session) else {
        return Some(StopRefusal::Unknown);
    };
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || {
        holds_in(&dir, projects.as_deref(), &session, sent_at_ms, app_sync)
    })
    .await
    .unwrap_or(Some(StopRefusal::StillRunning))
}

fn holds_in(
    dir: &Path,
    projects: Option<&Path>,
    session: &str,
    sent_at_ms: Option<i64>,
    app_sync: bool,
) -> Option<StopRefusal> {
    let records = match session_records(dir, session) {
        Ok(records) => records,
        Err(refusal) => return Some(refusal),
    };
    holds(&records, projects, app_sync).or_else(|| {
        sent_at_ms
            .is_some_and(|sent| records.iter().any(|record| record.busy_since(sent)))
            .then_some(StopRefusal::Busy)
    })
}

fn session_records(dir: &Path, session: &str) -> Result<Vec<LiveRecord>, StopRefusal> {
    let scan = read_live_records(dir);
    if !scan.complete || scan.uncertain.iter().any(|named| named == session) {
        return Err(StopRefusal::Unknown);
    }
    Ok(scan
        .records
        .into_iter()
        .filter(|record| record.session == session)
        .collect())
}

/// `app_sync`: the owner's Claude app sync setting. Off, the Claude app
/// holds a chat it has open like a terminal does (see `Holder::waits`).
fn holds(records: &[LiveRecord], projects: Option<&Path>, app_sync: bool) -> Option<StopRefusal> {
    if records
        .iter()
        .any(|record| record.protected() || (!app_sync && record.kind == Kind::Desktop))
    {
        return Some(StopRefusal::Protected);
    }
    if records.iter().any(|record| !record.known_status()) {
        return Some(StopRefusal::Busy);
    }
    records
        .iter()
        .any(|record| record.background_work(projects))
        .then_some(StopRefusal::Background)
}

/// Last sender wins: ends the process the Claude desktop app keeps for this
/// chat, so a turn from Wonder becomes part of
/// the conversation; the app starts the chat again from its transcript when it
/// is next used. A reply it is writing is interrupted like Stop (SIGINT keeps
/// the text written so far in the transcript; SIGTERM lost it, measured).
/// Background agents, commands or monitors, a reply the Mac started after
/// `sent_at_ms` (it sent last) and a chat open in a terminal or another Agent
/// SDK app are never ended here: only Send now does that
/// (`take_over_claude_session`).
/// `Ok` once no process has the chat open.
pub(crate) async fn end_desktop_session(session: &str, sent_at_ms: i64) -> Result<(), StopRefusal> {
    let Some((dir, projects)) = claude_dirs(session) else {
        return Err(StopRefusal::Unknown);
    };
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || {
        end_desktop_session_in(&dir, projects.as_deref(), &session, Some(sent_at_ms))
    })
    .await
    .unwrap_or(Err(StopRefusal::StillRunning))
}

/// After Wonder's turn, closes a desktop process opened for the chat during
/// it, which holds the conversation from before, but only while it is idle:
/// nothing the app runs there is ended for this.
pub(crate) async fn close_idle_desktop_session(session: &str) {
    let Some((dir, projects)) = claude_dirs(session) else {
        return;
    };
    let session = session.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        end_desktop_session_in(&dir, projects.as_deref(), &session, None)
    })
    .await;
}

/// Send now: the owner chose to take the chat over from the app on the Mac
/// that has it open, a terminal too, even while it replies or runs background
/// agents. Ends exactly the processes whose live record names this session
/// and whose identity is verified (pid, OS start time, the record's start
/// label, Claude Code's executable), never another session's: SIGINT first
/// (Claude Code keeps the reply written so far), SIGTERM for one that did not
/// exit. Shells and agents it started end with it. `Ok` once none runs, no
/// live record has the chat open and its transcript stopped changing.
pub(crate) async fn take_over_claude_session(session: &str) -> Result<(), StopRefusal> {
    let Some((dir, projects)) = claude_dirs(session) else {
        return Err(StopRefusal::Unknown);
    };
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || take_over_in(&dir, projects.as_deref(), &session))
        .await
        .unwrap_or(Err(StopRefusal::StillRunning))
}

fn take_over_in(dir: &Path, projects: Option<&Path>, session: &str) -> Result<(), StopRefusal> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    let processes = |records: &[&LiveRecord]| {
        let mut ids: Vec<_> = records
            .iter()
            .map(|record| {
                (
                    record.pid,
                    record.process_start,
                    record.record_start.clone(),
                )
            })
            .collect();
        ids.sort_unstable();
        ids
    };
    let records = session_records(dir, session)?;
    if !records.iter().all(LiveRecord::verified) {
        return Err(StopRefusal::Unverified);
    }
    let transcripts: Vec<PathBuf> = records
        .iter()
        .filter_map(|record| Some(transcript_in(projects?, record.cwd.as_deref()?, session)))
        .collect();
    // Shells and commands it started, to end any it leaves behind.
    let children: Vec<ProcessIdentity> = records
        .iter()
        .flat_map(|record| descendants(record.pid))
        .collect();
    #[cfg(all(test, unix))]
    tests::after_check(session);
    let exited = |record: &LiveRecord| {
        ProcessIdentity {
            pid: record.pid,
            start: record.process_start,
        }
        .exited()
    };
    for (signal, polls) in [
        (crate::platform::Signal::Interrupt, 30),
        (crate::platform::Signal::Terminate, 50),
    ] {
        for record in &records {
            if exited(record) {
                continue;
            }
            // Immediately before each signal: the same processes still have
            // the chat open (the app did not open it again in a new one), and
            // this one is still the Claude Code that wrote its record.
            let latest = session_records(dir, session)?;
            let survivors: Vec<_> = records.iter().filter(|record| !exited(record)).collect();
            if processes(&latest.iter().collect::<Vec<_>>()) != processes(&survivors) {
                return Err(StopRefusal::Busy);
            }
            if !record.verified() {
                return Err(StopRefusal::Unverified);
            }
            crate::platform::signal(record.pid, signal);
        }
        for _ in 0..polls {
            if std::time::Instant::now() >= deadline {
                return Err(StopRefusal::StillRunning);
            }
            if records.iter().all(exited) {
                for child in &children {
                    if !child.exited() {
                        crate::platform::signal(child.pid, crate::platform::Signal::Terminate);
                    }
                }
                if !session_records(dir, session)?.is_empty() {
                    return Err(StopRefusal::Busy);
                }
                return wait_for_flush(&transcripts, deadline);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    Err(StopRefusal::StillRunning)
}

/// Codex keeps one writer per thread: the process that has its rollout open
/// (a `codex app-server` that Codex's desktop app, T3 Code, VS Code or
/// another app runs; a codex CLI; or Wonder's own runtime). Another
/// process's message queues into it, so it runs there. This names that
/// process when it is not Wonder's. Send now only ever ends a codex CLI that
/// has just this thread open; a server process runs other threads too.
pub(crate) async fn codex_holder(thread: &str) -> Option<Holder> {
    let rollout = codex_rollout(thread)?;
    tokio::task::spawn_blocking(move || codex_holder_of(&rollout).map(|found| found.holder))
        .await
        .ok()
        .flatten()
}

struct CodexWriter {
    process: ProcessIdentity,
    holder: Holder,
    /// Why Send now must not end it, if it must not.
    refusal: Option<StopRefusal>,
}

/// How a codex process runs threads, decided from its own arguments rather
/// than the app that started it: `codex app-server` (what Codex's desktop
/// app, T3 Code and VS Code run) and `codex mcp-server` serve any number of
/// threads from one process, whichever app hosts them; anything else is the
/// codex CLI, which runs one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexProcess {
    Server,
    Cli,
}

fn codex_process(pid: i32) -> Option<CodexProcess> {
    let arguments = codex_arguments(pid)?;
    let server = arguments
        .iter()
        .skip(1)
        .any(|argument| matches!(argument.as_str(), "app-server" | "mcp-server"));
    Some(if server {
        CodexProcess::Server
    } else {
        CodexProcess::Cli
    })
}

fn codex_arguments(pid: i32) -> Option<Vec<String>> {
    #[cfg(all(test, unix))]
    return tests::CODEX_ARGS.lock().unwrap().get(&pid).cloned();
    #[cfg(not(all(test, unix)))]
    process_arguments(pid)
}

fn codex_holder_of(rollout: &Path) -> Option<CodexWriter> {
    let rollout = crate::canonical_path::canonicalize(rollout).ok()?;
    for pid in codex_processes() {
        if descends_from_this_daemon(pid) {
            continue;
        }
        let start = process_start(pid);
        let Some(files) = open_files(pid) else {
            continue;
        };
        if !files.contains(&rollout) {
            continue;
        }
        let others = files
            .iter()
            .filter(|file| **file != rollout && codex_rollout_file(file))
            .count();
        let host = host_app(pid);
        let process = codex_process(pid);
        // Codex's desktop app is named as the Dock shows it (now ChatGPT).
        let desktop = matches!(host.as_deref(), Some("ChatGPT" | "Codex"));
        let (app, kind) = match (process, host) {
            (Some(CodexProcess::Cli), Some(terminal)) => {
                (format!("Codex CLI in {terminal}"), HolderKind::Terminal)
            }
            (Some(CodexProcess::Cli), None) => {
                ("Codex CLI in a terminal".to_owned(), HolderKind::Terminal)
            }
            (_, Some(app)) if desktop => (app, HolderKind::Desktop),
            (_, Some(app)) => (app, HolderKind::App),
            (_, None) => ("Codex".to_owned(), HolderKind::App),
        };
        let refusal = match process {
            Some(CodexProcess::Server) => Some(StopRefusal::Shared),
            Some(CodexProcess::Cli) if others > 0 => Some(StopRefusal::Shared),
            Some(CodexProcess::Cli) if start.is_some() => None,
            _ => Some(StopRefusal::Unverified),
        };
        return Some(CodexWriter {
            process: ProcessIdentity { pid, start },
            holder: Holder {
                app,
                kind,
                replying: false,
                background: false,
                waits: true,
                can_take_over: refusal.is_none(),
            },
            refusal,
        });
    }
    None
}

fn codex_rollout_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

/// Send now on a Codex thread a codex CLI has open: ends that process (SIGINT
/// first, so Codex records the running turn as interrupted; SIGTERM if it did
/// not exit). Never a `codex app-server` (whatever app runs it), a CLI with
/// another thread open, or a process it cannot classify: nothing is signalled.
/// `Ok` once no other process has the rollout open and it stopped changing;
/// Wonder's runtime then resumes the thread from it.
pub(crate) async fn take_over_codex_thread(thread: &str) -> Result<(), StopRefusal> {
    let Some(rollout) = codex_rollout(thread) else {
        return Err(StopRefusal::Unknown);
    };
    tokio::task::spawn_blocking(move || take_over_codex_in(&rollout))
        .await
        .unwrap_or(Err(StopRefusal::StillRunning))
}

fn take_over_codex_in(rollout: &Path) -> Result<(), StopRefusal> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    let Some(writer) = codex_holder_of(rollout) else {
        return Ok(());
    };
    if let Some(refusal) = writer.refusal {
        return Err(refusal);
    }
    let process = writer.process;
    for (signal, polls) in [
        (crate::platform::Signal::Interrupt, 30),
        (crate::platform::Signal::Terminate, 50),
    ] {
        if !process.exited() {
            // Immediately before each signal: still the same process, still
            // holding only this thread.
            match codex_holder_of(rollout) {
                Some(current) if current.process == process && current.refusal.is_none() => {}
                _ => return Err(StopRefusal::Busy),
            }
            crate::platform::signal(process.pid, signal);
        }
        for _ in 0..polls {
            if std::time::Instant::now() >= deadline {
                return Err(StopRefusal::StillRunning);
            }
            if process.exited() {
                if codex_holder_of(rollout).is_some() {
                    return Err(StopRefusal::Busy);
                }
                return wait_for_flush(&[rollout.to_owned()], deadline);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    Err(StopRefusal::StillRunning)
}

/// Codex processes on this Mac, by executable name.
#[cfg(target_os = "macos")]
fn codex_processes() -> Vec<i32> {
    #[cfg(test)]
    return tests::CODEX_PIDS.lock().unwrap().iter().copied().collect();
    #[cfg(not(test))]
    {
        let mut pids = vec![0i32; 16384];
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(pids.as_slice()) as i32,
            )
        };
        // Returns a count of PIDs, not bytes (measured on macOS 26).
        let count = usize::try_from(count).unwrap_or(0).min(pids.len());
        pids[..count]
            .iter()
            .copied()
            .filter(|pid| *pid > 1 && process_name(*pid).as_deref() == Some("codex"))
            .collect()
    }
}

#[cfg(not(target_os = "macos"))]
fn codex_processes() -> Vec<i32> {
    Vec::new()
}

/// The files a process has open, bounded.
#[cfg(target_os = "macos")]
fn open_files(pid: i32) -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    const PROC_PIDFDVNODEPATHINFO: i32 = 2;
    // <sys/proc_info.h>; libc does not define these two.
    #[repr(C)]
    struct ProcFileInfo {
        openflags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guardflags: u32,
    }
    #[repr(C)]
    struct VnodeFdInfoWithPath {
        file: ProcFileInfo,
        path: libc::vnode_info_path,
    }
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        1024
    ];
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.len() * entry) as i32,
        )
    };
    let count = (usize::try_from(bytes).ok()? / entry).min(fds.len());
    let mut paths = Vec::new();
    for fd in &fds[..count] {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<VnodeFdInfoWithPath>();
        let read = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                (&mut info as *mut VnodeFdInfoWithPath).cast(),
                size as i32,
            )
        };
        if usize::try_from(read).ok() != Some(size) {
            continue;
        }
        let raw = unsafe {
            std::slice::from_raw_parts(
                info.path.vip_path.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&info.path.vip_path),
            )
        };
        let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        if end > 0 {
            paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(&raw[..end])));
        }
    }
    Some(paths)
}

#[cfg(not(target_os = "macos"))]
fn open_files(_pid: i32) -> Option<Vec<PathBuf>> {
    None
}

fn transcript_in(projects: &Path, cwd: &str, session: &str) -> PathBuf {
    projects
        .join(project_folder_name(cwd))
        .join(format!("{session}.jsonl"))
}

/// The chat's history is saved once its transcript stops changing: the
/// process that wrote it has exited, and nothing else appends to it.
fn wait_for_flush(
    transcripts: &[PathBuf],
    deadline: std::time::Instant,
) -> Result<(), StopRefusal> {
    wait_until_quiet(|| transcript_marks(transcripts), deadline)
}

/// Each transcript's length and modification time, or None when it is gone.
fn transcript_marks(transcripts: &[PathBuf]) -> Vec<Option<(u64, Option<std::time::SystemTime>)>> {
    transcripts
        .iter()
        .map(|path| {
            std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.len(), meta.modified().ok()))
        })
        .collect()
}

/// Reads every 150 ms until two reads agree, or answers Busy at `deadline`.
fn wait_until_quiet<T: PartialEq>(
    mut read: impl FnMut() -> T,
    deadline: std::time::Instant,
) -> Result<(), StopRefusal> {
    let mut last = read();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let now = read();
        if now == last {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(StopRefusal::Busy);
        }
        last = now;
    }
}

/// With `sent_at_ms`, Wonder's message takes the chat over; without it, only
/// an idle process is closed and nothing running in it is ended.
fn end_desktop_session_in(
    dir: &Path,
    projects: Option<&Path>,
    session: &str,
    sent_at_ms: Option<i64>,
) -> Result<(), StopRefusal> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let check = |records: &[LiveRecord]| {
        if std::time::Instant::now() >= deadline {
            return Err(StopRefusal::Unknown);
        }
        // Only runs with Claude app sync on (see `end_desktop_session`).
        if let Some(refusal) = holds(records, projects, true) {
            return Err(refusal);
        }
        let later = |record: &LiveRecord| match sent_at_ms {
            Some(sent) => record.busy_since(sent),
            None => record.background(),
        };
        if records.iter().any(later) {
            return Err(StopRefusal::Busy);
        }
        Ok(())
    };
    let records = session_records(dir, session)?;
    check(&records)?;
    #[cfg(all(test, unix))]
    tests::after_check(session);
    // Immediately before signalling, the exact processes checked must still
    // be all there is, unchanged, and still pass: the owner may have started a
    // reply or opened the chat in a terminal meanwhile. Any change waits.
    let current = session_records(dir, session)?;
    if identities(current.iter()) != identities(records.iter()) {
        return Err(StopRefusal::Busy);
    }
    check(&current)?;
    // SIGINT first: Claude Code ends like Stop and saves the reply written so
    // far (measured: 2 of 2 kept it, exit in ~100 ms); SIGTERM, which lost it,
    // only for a process that did not exit. Either ends its shells and agents.
    for (signal, polls) in [
        (crate::platform::Signal::Interrupt, 30),
        (crate::platform::Signal::Terminate, 50),
    ] {
        // Re-read before escalation and before each signal, including the
        // native status clock and OS process birth time. A PID alone is not an identity.
        for record in &current {
            if !process_alive(record.pid) {
                continue;
            }
            let latest = session_records(dir, session)?;
            let survivors: Vec<_> = current
                .iter()
                .filter(|record| process_alive(record.pid))
                .collect();
            if identities(latest.iter()) != identities(survivors.into_iter()) {
                return Err(StopRefusal::Busy);
            }
            check(&latest)?;
            if record.protected() {
                return Err(StopRefusal::Busy);
            }
            crate::platform::signal(record.pid, signal);
        }
        for _ in 0..polls {
            if std::time::Instant::now() >= deadline {
                return Err(StopRefusal::StillRunning);
            }
            if current.iter().all(|record| !process_alive(record.pid)) {
                return if session_records(dir, session)?.is_empty() {
                    Ok(())
                } else {
                    Err(StopRefusal::Busy)
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    Err(StopRefusal::StillRunning)
}

/// What identifies a checked process record: a change to any of it is a
/// different process state than the one checked.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RecordIdentity<'a> {
    pid: i32,
    status: &'a str,
    status_at: Option<i64>,
    cwd: Option<&'a str>,
    kind: Kind,
    process_start: Option<(u64, u64)>,
    record_start: Option<&'a str>,
}

fn identities<'a>(records: impl Iterator<Item = &'a LiveRecord>) -> Vec<RecordIdentity<'a>> {
    let mut identities: Vec<_> = records
        .map(|record| RecordIdentity {
            pid: record.pid,
            status: &record.status,
            status_at: record.status_at,
            cwd: record.cwd.as_deref(),
            kind: record.kind,
            process_start: record.process_start,
            record_start: record.record_start.as_deref(),
        })
        .collect();
    identities.sort_unstable();
    identities
}

const DESKTOP_APP: &str = "/Applications/Claude.app";

/// Whether the Claude desktop app is installed on this Mac.
pub(crate) fn claude_app_installed() -> bool {
    cfg!(target_os = "macos") && Path::new(DESKTOP_APP).is_dir()
}
const DESKTOP_BUNDLE_ID: &str = "com.anthropic.claudefordesktop";

/// With Claude app sync on, a new Claude chat started in Wonder also belongs
/// in the Claude desktop app. After its first response, the Mac opens Claude's own resume link in the
/// background, which adds the chat to the app's sidebar, then closes the idle
/// process the app starts for it so Wonder keeps sending directly. The link
/// waits while the app is in front, so the chat you are reading never changes.
pub(crate) fn schedule_desktop_import(
    state: crate::AppState,
    conversation: String,
    session: String,
) {
    if cfg!(test) || !claude_app_installed() || !valid_session_id(&session) {
        return;
    }
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(2 * 60 * 60) {
            // Only with Claude app sync on, read before each attempt so
            // turning it off stops one already waiting.
            if !crate::agent_settings::claude_app_sync(&state).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            // The runtime can still run a reply the store already ended (a
            // Guide answered after its reply); opening the app then would
            // give the chat a second writer.
            let working = state
                .store
                .conversation_has_active_turn(&conversation)
                .await
                .unwrap_or(true)
                || crate::projects::claude_reply_running(&state, &conversation).await;
            if working || desktop_app_frontmost().await {
                continue;
            }
            if claude_live_sessions().await.contains_key(&session) {
                return; // Already open on the Mac.
            }
            let link = format!("claude://resume?session={session}");
            let opened = tokio::process::Command::new("/usr/bin/open")
                .args(["-g", &link])
                .status()
                .await
                .is_ok_and(|status| status.success());
            if !opened {
                return;
            }
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if claude_live_sessions().await.get(&session) == Some(&false) {
                    close_idle_desktop_session(&session).await;
                    break;
                }
            }
            let _ = state.logger.record(
                "info",
                "claude_desktop_import",
                serde_json::json!({"conversationId": conversation}),
            );
            return;
        }
    });
}

fn valid_session_id(value: &str) -> bool {
    value.len() == 36 && value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

async fn desktop_app_frontmost() -> bool {
    let Ok(front) = tokio::process::Command::new("/usr/bin/lsappinfo")
        .arg("front")
        .output()
        .await
    else {
        return true;
    };
    let asn = String::from_utf8_lossy(&front.stdout).trim().to_owned();
    if asn.is_empty() {
        return false;
    }
    let Ok(info) = tokio::process::Command::new("/usr/bin/lsappinfo")
        .args(["info", "-only", "bundleid", &asn])
        .output()
        .await
    else {
        return true;
    };
    String::from_utf8_lossy(&info.stdout).contains(DESKTOP_BUNDLE_ID)
}

/// The Claude Code binary the Claude desktop app runs for its chats, checked
/// by executable path so a reused process ID is never signalled.
#[cfg(target_os = "macos")]
fn desktop_claude_process(pid: i32) -> bool {
    #[cfg(test)]
    if tests::DESKTOP_PIDS.lock().unwrap().contains(&pid) {
        return true;
    }
    match executable_path(pid) {
        Some(path) => {
            path.contains("/Library/Application Support/Claude/claude-code/")
                && path.ends_with("/MacOS/claude")
        }
        // An update replaced the file it started from (see `claude_executable`).
        None => process_name(pid).as_deref() == Some("claude"),
    }
}

#[cfg(not(target_os = "macos"))]
fn desktop_claude_process(_pid: i32) -> bool {
    false
}

/// The Claude Code binary an Agent SDK app or a terminal runs, checked by
/// executable name so a reused process ID is never signalled: `claude`, or
/// `claude.exe` as current installs name it (measured: T3 Code's and the npm
/// CLI's). When an update replaced the file a running process started from,
/// its path can no longer be read (T3 Code's long-lived process, measured);
/// the kernel still keeps the name it started under.
#[cfg(target_os = "macos")]
fn claude_executable(pid: i32) -> bool {
    #[cfg(test)]
    if tests::DESKTOP_PIDS.lock().unwrap().contains(&pid) {
        return true;
    }
    let name = match executable_path(pid) {
        Some(path) => path.rsplit('/').next().map(str::to_owned),
        None => process_name(pid),
    };
    matches!(name.as_deref(), Some("claude" | "claude.exe"))
}

#[cfg(not(target_os = "macos"))]
fn claude_executable(_pid: i32) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub(crate) fn executable_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn executable_path(_pid: i32) -> Option<String> {
    None
}

/// A process's arguments (argv), bounded; `None` when they cannot be read.
#[cfg(target_os = "macos")]
fn process_arguments(pid: i32) -> Option<Vec<String>> {
    // ARG_MAX on macOS: arguments and environment never exceed it.
    let mut size = 1024 * 1024usize;
    let mut buffer = vec![0u8; size];
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 || size < 4 {
        return None;
    }
    // argc, the executable path, padding, then argc NUL-terminated arguments.
    let argc = i32::from_ne_bytes(buffer[..4].try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|count| *count > 0)?;
    let mut rest = &buffer[4..size.min(buffer.len())];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    rest = &rest[path_end..];
    rest = &rest[rest.iter().position(|byte| *byte != 0)?..];
    let arguments: Vec<String> = rest
        .split(|byte| *byte == 0)
        .take(argc)
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect();
    (arguments.len() == argc).then_some(arguments)
}

#[cfg(not(target_os = "macos"))]
#[cfg_attr(test, allow(dead_code))] // Tests read Codex arguments from a table.
fn process_arguments(_pid: i32) -> Option<Vec<String>> {
    None
}

#[cfg(target_os = "macos")]
fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0u8; 64];
    let length = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

/// The processes started under `pid` (its shells, commands and their
/// children), each with its start time, bounded.
#[cfg(target_os = "macos")]
fn descendants(pid: i32) -> Vec<ProcessIdentity> {
    let mut found = Vec::new();
    let mut pending = vec![pid];
    while let Some(parent) = pending.pop() {
        let mut children = [0i32; 64];
        let bytes = unsafe {
            libc::proc_listchildpids(
                parent,
                children.as_mut_ptr().cast(),
                std::mem::size_of_val(&children) as i32,
            )
        };
        // Returns a count of PIDs, not bytes (measured on macOS 26).
        let count = usize::try_from(bytes).unwrap_or(0).min(children.len());
        for &child in &children[..count] {
            if child <= 1 || found.len() >= 64 {
                continue;
            }
            if let Some(start) = process_start(child) {
                found.push(ProcessIdentity {
                    pid: child,
                    start: Some(start),
                });
                pending.push(child);
            }
        }
    }
    found
}

#[cfg(not(target_os = "macos"))]
fn descendants(_pid: i32) -> Vec<ProcessIdentity> {
    Vec::new()
}

/// Wonder's own Claude processes are children of its Claude runtime, which is
/// this daemon's child; other Agent SDK apps (t3code) write the same records.
fn descends_from_this_daemon(pid: i32) -> bool {
    let me = std::process::id() as i32;
    let mut current = pid;
    for _ in 0..4 {
        match parent_pid(current) {
            Some(parent) if parent == me => return true,
            Some(parent) if parent > 1 => current = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then_some(info.pbi_ppid as i32)
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: i32) -> Option<i32> {
    use std::io::Read;
    let mut record = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(4097)
        .read_to_string(&mut record)
        .ok()?;
    if record.len() > 4096 {
        return None;
    }
    // comm can contain spaces and parentheses. The fields follow its last ')'.
    let (_, fields) = record.rsplit_once(')')?;
    fields.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_pid(_pid: i32) -> Option<i32> {
    None
}

fn live_records_in(dir: &Path) -> Vec<LiveRecord> {
    read_live_records(dir).records
}

/// One read of Claude Code's live session records.
struct LiveScan {
    records: Vec<LiveRecord>,
    /// False when a limit on the whole scan was hit (too many entries or
    /// records, the deadline, an unreadable directory): any session may be
    /// held by a record that was not read.
    complete: bool,
    /// Sessions named by a record that failed validation (damaged, oversized,
    /// another PID, a recycled PID). Only these are unknown: one bad file must
    /// not hold every other chat. A record naming no session holds none.
    uncertain: Vec<String>,
}

/// The session a damaged or oversized record names, from its readable prefix.
fn named_session(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("\"sessionId\"")?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let (session, _) = rest.strip_prefix('"')?.split_once('"')?;
    (!session.is_empty() && session.len() <= 128 && !session.contains('\\'))
        .then(|| session.to_owned())
}

// Display may omit unknown evidence. Admission and cleanup must retain it as
// an error: a live record omitted from a partial scan could hold this session.
fn read_live_records(dir: &Path) -> LiveScan {
    // Fixtures can control scanner time without replacing OS identity or file
    // evidence. Outside that directory-scoped test guard this is the real clock.
    #[cfg(all(test, unix))]
    let clock = tests::scan_clock(dir);
    let now = || {
        #[cfg(all(test, unix))]
        if let Some(clock) = &clock {
            return clock.now();
        }
        std::time::Instant::now()
    };

    use std::io::Read;
    let mut scan = LiveScan {
        records: Vec::new(),
        complete: true,
        uncertain: Vec::new(),
    };
    let doubt = |scan: &mut LiveScan, session: Option<String>| {
        if let Some(session) = session.filter(|session| !scan.uncertain.contains(session)) {
            scan.uncertain.push(session);
        }
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return scan,
        Err(_) => {
            scan.complete = false;
            return scan;
        }
    };
    let deadline = now() + std::time::Duration::from_millis(250);
    for (count, entry) in entries.enumerate() {
        if count >= MAX_SESSION_RECORDS * 4 || now() >= deadline {
            scan.complete = false;
            return scan;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.is_empty() || stem.len() > 10 || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = stem.parse::<i32>() else {
            continue;
        };
        if !process_alive(pid) {
            continue;
        }
        let start = process_start(pid);
        let read = || -> Option<Vec<u8>> {
            let file = crate::platform::open_regular_nofollow(&entry.path()).ok()?;
            if !file.metadata().ok()?.is_file() {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(MAX_RECORD_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            Some(bytes)
        };
        // An unreadable file names no session, so it cannot hold one.
        let Some(bytes) = read() else { continue };
        let prefix = &bytes[..bytes.len().min(MAX_RECORD_BYTES as usize)];
        let parsed = (bytes.len() as u64 <= MAX_RECORD_BYTES)
            .then(|| serde_json::from_slice::<serde_json::Value>(prefix).ok())
            .flatten();
        let Some(record) = parsed else {
            doubt(&mut scan, named_session(&String::from_utf8_lossy(prefix)));
            continue;
        };
        let entrypoint = record.get("entrypoint").and_then(serde_json::Value::as_str);
        let Some(session) = record
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .filter(|session| !session.is_empty() && session.len() <= 128)
        else {
            continue;
        };
        if record.get("pid").and_then(serde_json::Value::as_i64) != Some(i64::from(pid)) {
            doubt(&mut scan, Some(session.to_owned()));
            continue;
        }
        if entrypoint == Some("sdk-ts") && descends_from_this_daemon(pid) {
            continue;
        }
        if !process_alive(pid) {
            continue;
        }
        if start != process_start(pid)
            || (cfg!(target_os = "macos")
                && (start.is_none()
                    || record.get("procStart").and_then(serde_json::Value::as_str)
                        != process_start_label(pid).as_deref()))
        {
            doubt(&mut scan, Some(session.to_owned()));
            continue;
        }
        if scan.records.len() == MAX_SESSION_RECORDS {
            scan.complete = false;
            return scan;
        }
        scan.records.push(LiveRecord {
            pid,
            session: session.to_owned(),
            status: record
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .chars()
                .take(32)
                .collect(),
            status_at: record
                .get("statusUpdatedAt")
                .and_then(serde_json::Value::as_i64)
                .filter(|at| *at >= 0),
            cwd: record
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .filter(|cwd| cwd.starts_with('/') && cwd.len() <= 4096)
                .map(str::to_owned),
            kind: kind_of(entrypoint),
            process_start: start,
            record_start: record
                .get("procStart")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    }
    scan
}

/// What a live record's `entrypoint` says started the process.
fn kind_of(entrypoint: Option<&str>) -> Kind {
    match entrypoint {
        Some("claude-desktop") => Kind::Desktop,
        // "t3-code" is what T3 Code's `t3-claude` wrapper reports (measured
        // 2026-11-08 in ~/.claude/sessions); as a terminal, no T3 route held.
        Some("sdk-ts" | "t3-code") => Kind::SdkApp,
        _ => Kind::Terminal,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn process_start(pid: i32) -> Option<(u64, u64)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn process_start(_pid: i32) -> Option<(u64, u64)> {
    None
}

// Claude Code records the native process birth label in UTC, independently of
// the Mac's current timezone. The microseconds above also fence PID reuse.
fn process_start_label(pid: i32) -> Option<String> {
    use chrono::TimeZone;
    let (seconds, _) = process_start(pid)?;
    Some(
        chrono::Utc
            .timestamp_opt(i64::try_from(seconds).ok()?, 0)
            .single()?
            .format("%a %b %e %H:%M:%S %Y")
            .to_string(),
    )
}

/// The birth label a Claude Code record carries for `pid` (test rigs write records).
#[cfg(all(test, unix))]
pub(crate) fn record_start_label(pid: i32) -> Option<String> {
    process_start_label(pid)
}

fn process_alive(pid: i32) -> bool {
    crate::platform::process_alive(pid)
}

/// The identity a live record names for `pid` (test rigs follow real processes).
#[cfg(all(test, unix))]
pub(crate) fn process_identity(pid: i32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start: process_start(pid),
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    #[derive(Clone)]
    pub(super) struct ScanClock {
        instant: std::rc::Rc<std::cell::Cell<std::time::Instant>>,
        step: std::rc::Rc<std::cell::Cell<std::time::Duration>>,
    }
    impl ScanClock {
        pub(super) fn now(&self) -> std::time::Instant {
            let instant = self.instant.get();
            self.instant.set(instant + self.step.get());
            instant
        }
    }
    thread_local! {
        static SCAN_CLOCK: std::cell::RefCell<Option<(PathBuf, ScanClock)>> = const { std::cell::RefCell::new(None) };
    }
    pub(super) fn scan_clock(dir: &Path) -> Option<ScanClock> {
        SCAN_CLOCK.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|(root, _)| dir.starts_with(root))
                .map(|(_, clock)| clock.clone())
        })
    }

    /// Controls only a synchronous fixture's live-record scan budget. Real
    /// process starts, liveness, signals and takeover deadlines remain real.
    /// The previous clock is restored on normal return and assertion failure.
    struct ScopedScanClock {
        clock: ScanClock,
        previous: Option<(PathBuf, ScanClock)>,
    }
    impl ScopedScanClock {
        fn new(dir: &Path) -> Self {
            let clock = ScanClock {
                instant: std::rc::Rc::new(std::cell::Cell::new(std::time::Instant::now())),
                step: std::rc::Rc::new(std::cell::Cell::new(std::time::Duration::ZERO)),
            };
            let previous =
                SCAN_CLOCK.with(|slot| slot.replace(Some((dir.to_owned(), clock.clone()))));
            Self { clock, previous }
        }
        fn step_each_read(&self, step: std::time::Duration) {
            self.clock.step.set(step);
        }
    }
    impl Drop for ScopedScanClock {
        fn drop(&mut self) {
            SCAN_CLOCK.with(|slot| {
                slot.replace(self.previous.take());
            });
        }
    }

    /// Private live-record and transcript folders per test session (see `claude_dirs`).
    pub(crate) static SESSION_DIRS: std::sync::LazyLock<
        std::sync::Mutex<HashMap<String, (PathBuf, PathBuf)>>,
    > = std::sync::LazyLock::new(Default::default);

    /// How often each transcript was read for `main_turn_running`.
    pub(crate) static TRANSCRIPT_READS: std::sync::LazyLock<
        std::sync::Mutex<HashMap<PathBuf, usize>>,
    > = std::sync::LazyLock::new(Default::default);

    pub(crate) static INGESTED_BYTES: std::sync::LazyLock<
        std::sync::Mutex<HashMap<PathBuf, usize>>,
    > = std::sync::LazyLock::new(Default::default);

    /// How often the live records were scanned for each session's holder.
    pub(crate) static HOLDER_SCANS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, usize>>> =
        std::sync::LazyLock::new(Default::default);

    pub(crate) fn holder_scans(session: &str) -> usize {
        HOLDER_SCANS
            .lock()
            .unwrap()
            .get(session)
            .copied()
            .unwrap_or_default()
    }

    /// Extra live-record and transcript folders `claude_busy_sessions` also
    /// reads; tests write records only for their own sessions there.
    pub(crate) static BUSY_DIRS: std::sync::Mutex<Vec<(PathBuf, PathBuf)>> =
        std::sync::Mutex::new(Vec::new());

    #[tokio::test]
    async fn codex_local_idle_does_not_override_shared_writer_or_unknown_store() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let thread = "desktop-native-overlap";
        let mut index = crate::codex_activity::tests::Fixture::new_for_thread(thread).await;
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            serde_json::json!({thread:{
            "id":thread,"path":index.path,"status":{"type":"idle"}}})
            .to_string(),
        )
        .unwrap();
        let rpc = state.projects.codex.lock().await.rpc();
        assert_eq!(
            codex_thread_busy(index.dir.path(), thread, Some(&rpc)).await,
            Some(true)
        );
        let other_store = tempfile::tempdir().unwrap();
        assert_eq!(
            codex_thread_busy(other_store.path(), thread, Some(&rpc)).await,
            None
        );
        index.producer_completion().await;
        assert_eq!(
            codex_thread_busy(index.dir.path(), thread, Some(&rpc)).await,
            Some(false)
        );
        state.projects.shutdown().await;
        assert_eq!(
            codex_thread_busy(index.dir.path(), thread, Some(&rpc)).await,
            Some(false),
            "a dead generation cannot invalidate matching producer proof"
        );
        std::fs::remove_file(index.dir.path().join("thread_history_1.sqlite")).unwrap();
        assert_eq!(
            codex_thread_busy(index.dir.path(), thread, None).await,
            None,
            "process exit and missing evidence do not become idle"
        );
    }

    // Claude app sync off: a chat open in the Claude app waits like one open
    // in a terminal. On: only the holders that always wait do.
    #[test]
    fn claude_app_sync_off_holds_a_chat_open_in_the_claude_app() {
        let holder = |kind, waits| Holder {
            app: "App".into(),
            kind,
            replying: false,
            background: false,
            waits,
            can_take_over: true,
        };
        for (kind, waits, app_sync, expected) in [
            (HolderKind::Desktop, false, true, false),
            (HolderKind::Desktop, false, false, true),
            (HolderKind::Desktop, true, true, true),
            (HolderKind::Terminal, true, false, true),
            (HolderKind::App, true, true, true),
            (HolderKind::App, false, false, false),
        ] {
            assert_eq!(
                holder(kind, waits).with_app_sync(app_sync).waits,
                expected,
                "{kind:?} waits={waits} sync={app_sync}"
            );
        }
    }

    // One damaged, oversized, misnamed or recycled-PID record leaves only the
    // chat it names unknown; a record naming no chat holds none, and every
    // other chat's admission still reads (it used to fail the whole scan).
    #[test]
    fn one_bad_record_holds_only_the_chat_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let scan_clock = ScopedScanClock::new(dir.path());
        let me = std::process::id() as i32;
        let bad = unsafe { libc::getppid() };
        std::fs::write(
            dir.path().join(format!("{me}.json")),
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "other", "status": "busy"}).to_string(),
        )
        .unwrap();
        let mut cases = vec![
            ("truncated", format!("{{\"pid\":{bad},\"sessionId\":\"named\",\"stat"), true),
            (
                "oversized",
                format!(
                    "{{\"pid\":{bad},\"sessionId\":\"named\",\"padding\":\"{}\"}}",
                    "x".repeat(MAX_RECORD_BYTES as usize)
                ),
                true,
            ),
            (
                "another PID",
                serde_json::json!({"pid": bad + 1, "procStart": process_start_label(bad), "sessionId": "named", "status": "idle"}).to_string(),
                true,
            ),
            ("names no chat", "{".to_owned(), false),
            ("oversized, names no chat", "x".repeat(MAX_RECORD_BYTES as usize + 1), false),
        ];
        if cfg!(target_os = "macos") {
            cases.push((
                "recycled PID",
                serde_json::json!({"pid": bad, "procStart": "another process", "sessionId": "named", "status": "idle"}).to_string(),
                true,
            ));
        }
        for (label, text, holds_named) in cases {
            std::fs::write(dir.path().join(format!("{bad}.json")), text).unwrap();
            assert_eq!(
                holds_in(dir.path(), None, "named", Some(0), true),
                holds_named.then_some(StopRefusal::Unknown),
                "{label}: the chat it names"
            );
            assert_eq!(
                holds_in(dir.path(), None, "other", Some(0), true),
                Some(StopRefusal::Protected),
                "{label}: another open chat"
            );
            assert_eq!(
                holds_in(dir.path(), None, "nowhere", Some(0), true),
                None,
                "{label}: a chat open nowhere"
            );
        }
        // An exhausted scan budget is incomplete evidence for every chat,
        // including one whose valid record was otherwise protected above.
        scan_clock.step_each_read(std::time::Duration::from_millis(250));
        for session in ["named", "other", "nowhere"] {
            assert_eq!(
                holds_in(dir.path(), None, session, Some(0), true),
                Some(StopRefusal::Unknown),
                "expired scan: {session}"
            );
        }
        scan_clock.step_each_read(std::time::Duration::ZERO);
        assert_eq!(
            holds_in(dir.path(), None, "other", Some(0), true),
            Some(StopRefusal::Protected)
        );
    }

    #[test]
    fn only_live_busy_records_named_by_their_pid_count() {
        let dir = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(dir.path());
        let me = std::process::id() as i32;
        let write = |name: &str, value: serde_json::Value| {
            std::fs::write(dir.path().join(name), value.to_string()).unwrap()
        };
        write(
            &format!("{me}.json"),
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "busy", "status": "busy"}),
        );
        write(
            "999999999.json",
            serde_json::json!({"pid": 999999999, "sessionId": "gone", "status": "busy"}),
        );
        write(
            "1.json",
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "mismatch", "status": "busy"}),
        );
        write(
            &format!("{me}.key"),
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "key", "status": "busy"}),
        );
        let idle = dir.path().join("idle");
        std::fs::create_dir(&idle).unwrap();
        std::fs::write(
            idle.join(format!("{me}.json")),
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "idle", "status": "idle"}).to_string(),
        )
        .unwrap();
        assert_eq!(
            busy_sessions_in(dir.path()),
            HashSet::from(["busy".to_owned()])
        );
        assert!(busy_sessions_in(&idle).is_empty());
        // 1.json names another PID: only the chat it names is unknown.
        assert_eq!(
            holds_in(dir.path(), None, "mismatch", Some(0), true),
            Some(StopRefusal::Unknown)
        );
        assert_eq!(
            holds_in(dir.path(), None, "busy", Some(0), true),
            Some(StopRefusal::Protected)
        );
        std::fs::remove_file(dir.path().join("1.json")).unwrap();
        // A process with no known app (a terminal) is never stopped.
        assert_eq!(
            end_desktop_session_in(dir.path(), None, "busy", Some(0)),
            Err(StopRefusal::Protected)
        );
        std::fs::write(
            idle.join(format!("{me}.json")),
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "idle", "status": "idle", "entrypoint": "claude-desktop"}).to_string(),
        )
        .unwrap();
        // The test process is not the desktop app's Claude Code, so it is never stopped.
        assert_eq!(
            end_desktop_session_in(&idle, None, "idle", Some(0)),
            Err(StopRefusal::Protected)
        );
        assert_eq!(
            holds_in(&idle, None, "idle", None, true),
            Some(StopRefusal::Protected)
        );
        assert_eq!(
            end_desktop_session_in(&idle, None, "absent", Some(0)),
            Ok(())
        );
        // Only a chat Wonder cannot take over is reported as held on the Mac.
        assert!(holder_in(&idle, None, "idle").is_some_and(|holder| holder.waits));
        assert_eq!(holder_in(&idle, None, "absent"), None);
        // An idle session is still open in that app.
        assert_eq!(
            live_sessions_in(&idle),
            HashMap::from([("idle".to_owned(), false)])
        );
    }

    pub(crate) static DESKTOP_PIDS: std::sync::LazyLock<std::sync::Mutex<HashSet<i32>>> =
        std::sync::LazyLock::new(Default::default);
    /// Stand-ins for codex processes (`codex_processes`).
    pub(crate) static CODEX_PIDS: std::sync::LazyLock<std::sync::Mutex<HashSet<i32>>> =
        std::sync::LazyLock::new(Default::default);
    /// A stand-in codex process's arguments; none means they cannot be read.
    pub(crate) static CODEX_ARGS: std::sync::LazyLock<std::sync::Mutex<HashMap<i32, Vec<String>>>> =
        std::sync::LazyLock::new(Default::default);
    /// The app a stand-in process was started by, as its ancestry would name it.
    pub(crate) static HOST_APPS: std::sync::LazyLock<std::sync::Mutex<HashMap<i32, String>>> =
        std::sync::LazyLock::new(Default::default);

    /// A stand-in for the desktop app's Claude Code: a child process that is
    /// reaped when it exits, so its pid stops existing.
    fn desktop_process() -> i32 {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        DESKTOP_PIDS.lock().unwrap().insert(pid);
        std::thread::spawn(move || child.wait());
        pid
    }

    // Last sender wins. A reply the desktop app is writing, and background
    // work it keeps after its reply, are stopped when a Wonder message sent
    // after them is dispatched; a reply started after the message holds it,
    // and the idle-only close after Wonder's own turn never ends running work.
    // Background work alone does not show the chat as working.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_wonder_message_stops_desktop_work_started_before_it() {
        let sessions = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(sessions.path());
        let projects = tempfile::tempdir().unwrap();
        let cwd = "/work/My App";
        let folder = projects.path().join(project_folder_name(cwd));
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(project_folder_name(cwd), "-work-My-App");
        let record = |pid: i32, status: &str, at: i64| {
            for entry in std::fs::read_dir(sessions.path()).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
            std::fs::write(
                sessions.path().join(format!("{pid}.json")),
                serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": "s1", "status": status, "statusUpdatedAt": at, "cwd": cwd, "entrypoint": "claude-desktop"})
                    .to_string(),
            )
            .unwrap()
        };
        let transcript = |lines: &[serde_json::Value]| {
            let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            std::fs::write(folder.join("s1.jsonl"), text.join("\n") + "\n").unwrap();
        };
        let prompt = serde_json::json!({"type": "user", "message": {"content": "Go"}});
        let tool = serde_json::json!({"type": "assistant", "message": {"stop_reason": "tool_use", "content": []}});
        let reply = serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": []}});
        let child = serde_json::json!({"type": "assistant", "isSidechain": true, "message": {"stop_reason": null}});
        let busy = || busy_sessions_with_transcripts(sessions.path(), Some(projects.path())).busy;
        let end = |sent: Option<i64>| {
            end_desktop_session_in(sessions.path(), Some(projects.path()), "s1", sent)
        };
        let (before, sent, after) = (1_000, 2_000, 3_000);

        // Missing/unknown native state never authorizes either admission or
        // idle cleanup. The old reader defaulted these records to idle.
        let uncertain = desktop_process();
        for (label, value) in [
            (
                "missing status",
                serde_json::json!({"pid": uncertain, "procStart": process_start_label(uncertain), "sessionId": "s1", "entrypoint": "claude-desktop"}),
            ),
            (
                "unknown status",
                serde_json::json!({"pid": uncertain, "procStart": process_start_label(uncertain), "sessionId": "s1", "status": "future-state", "entrypoint": "claude-desktop"}),
            ),
            (
                "missing busy clock",
                serde_json::json!({"pid": uncertain, "procStart": process_start_label(uncertain), "sessionId": "s1", "status": "busy", "entrypoint": "claude-desktop"}),
            ),
        ] {
            std::fs::write(
                sessions.path().join(format!("{uncertain}.json")),
                value.to_string(),
            )
            .unwrap();
            assert_eq!(
                holds_in(
                    sessions.path(),
                    Some(projects.path()),
                    "s1",
                    Some(sent),
                    true
                ),
                Some(StopRefusal::Busy),
                "{label}"
            );
            assert_eq!(end(Some(sent)), Err(StopRefusal::Busy), "{label}");
            assert_eq!(end(None), Err(StopRefusal::Busy), "{label}");
            assert!(process_alive(uncertain), "{label}");
        }
        let named = format!("{{\"pid\":{uncertain},\"sessionId\":\"s1\",\"stat");
        // Claude Code writes the session ID near the start, as here.
        let oversized = format!(
            "{{\"pid\":{uncertain},\"sessionId\":\"s1\",\"padding\":\"{}\"}}",
            "x".repeat(MAX_RECORD_BYTES as usize)
        );
        for text in [named.as_str(), &oversized] {
            std::fs::write(sessions.path().join(format!("{uncertain}.json")), text).unwrap();
            assert_eq!(
                holds_in(
                    sessions.path(),
                    Some(projects.path()),
                    "s1",
                    Some(sent),
                    true
                ),
                Some(StopRefusal::Unknown),
                "an unreadable live record is unknown for the chat it names"
            );
            assert_eq!(end(None), Err(StopRefusal::Unknown));
            assert!(process_alive(uncertain));
        }
        // A record naming no chat cannot hold this one (it would hold every chat).
        for text in ["{\"pid\":", &"x".repeat(MAX_RECORD_BYTES as usize + 1)] {
            std::fs::write(sessions.path().join(format!("{uncertain}.json")), text).unwrap();
            assert_eq!(
                holds_in(
                    sessions.path(),
                    Some(projects.path()),
                    "s1",
                    Some(sent),
                    true
                ),
                None
            );
            assert!(process_alive(uncertain));
        }
        let path = sessions.path().join(format!("{uncertain}.json"));
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("absent", &path).unwrap();
        assert_eq!(end(None), Ok(()), "a link names no chat");
        std::fs::remove_file(&path).unwrap();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(end(None), Ok(()), "a FIFO is never read and names no chat");
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
        std::fs::remove_file(&path).unwrap();
        record(uncertain, "idle", before);
        assert_eq!(end(None), Ok(()), "native idle still permits cleanup");

        // A reply the Mac started after the message: the Mac sent last.
        let pid = desktop_process();
        record(pid, "busy", after);
        transcript(&[prompt.clone(), tool.clone()]);
        assert_eq!(
            busy(),
            HashSet::from(["s1".to_owned()]),
            "the reply is running"
        );
        assert_eq!(
            holds_in(
                sessions.path(),
                Some(projects.path()),
                "s1",
                Some(sent),
                true
            ),
            Some(StopRefusal::Busy)
        );
        assert_eq!(
            holds_in(sessions.path(), Some(projects.path()), "s1", None, true),
            None,
            "a new send is not held"
        );
        assert_eq!(end(Some(sent)), Err(StopRefusal::Busy));
        assert!(process_alive(pid));
        // The same reply, started before the message: Wonder's message stops it.
        record(pid, "busy", before);
        assert_eq!(
            holds_in(
                sessions.path(),
                Some(projects.path()),
                "s1",
                Some(sent),
                true
            ),
            None
        );
        assert_eq!(
            end(Some(sent)),
            Ok(()),
            "the last sender takes the chat over"
        );
        assert!(!process_alive(pid));

        let pid = desktop_process();
        record(pid, "busy", before);
        transcript(&[prompt.clone(), tool, reply.clone(), child]);
        assert!(busy().is_empty(), "the reply ended; only agents run");
        assert_eq!(
            end(None),
            Err(StopRefusal::Background),
            "the idle-only close keeps the agents"
        );
        assert!(process_alive(pid));
        // Background agents end only when the owner chooses Send now.
        assert_eq!(
            holds_in(
                sessions.path(),
                Some(projects.path()),
                "s1",
                Some(sent),
                true
            ),
            Some(StopRefusal::Background)
        );
        assert_eq!(end(Some(sent)), Err(StopRefusal::Background));
        assert!(process_alive(pid));
        assert_eq!(
            take_over_in(sessions.path(), Some(projects.path()), "s1"),
            Ok(())
        );
        assert!(!process_alive(pid));

        for status in ["shell", "idle"] {
            let pid = desktop_process();
            record(pid, status, before);
            assert!(busy().is_empty());
            assert_eq!(
                live_sessions_in(sessions.path()),
                HashMap::from([("s1".to_owned(), status == "shell")])
            );
            if status == "shell" {
                assert_eq!(end(Some(sent)), Err(StopRefusal::Background));
                assert!(process_alive(pid));
                assert_eq!(
                    take_over_in(sessions.path(), Some(projects.path()), "s1"),
                    Ok(())
                );
            } else {
                assert_eq!(end(Some(sent)), Ok(()));
            }
            assert!(!process_alive(pid));
        }

        let me = std::process::id() as i32;
        let record = |status: &str| {
            std::fs::write(
                sessions.path().join(format!("{me}.json")),
                serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "s1", "status": status, "cwd": cwd})
                    .to_string(),
            )
            .unwrap()
        };
        // An unreadable transcript keeps trusting the record.
        std::fs::remove_file(folder.join("s1.jsonl")).unwrap();
        record("busy");
        assert_eq!(busy(), HashSet::from(["s1".to_owned()]));
        let interrupted = serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
        transcript(&[prompt, reply, interrupted]);
        assert!(busy().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_owns_its_sdk_children_and_protects_other_apps() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(descends_from_this_daemon(child.id() as i32));
        let dir = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(dir.path());
        let write = |pid: i32| {
            std::fs::write(dir.path().join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": "linux-session", "entrypoint": "sdk-ts", "status": "busy"}).to_string()).unwrap()
        };
        write(child.id() as i32);
        assert!(live_records_in(dir.path()).is_empty());
        // This process is not its own descendant. It stands in for an SDK
        // process outside Wonder, which Linux must never signal.
        write(std::process::id() as i32);
        assert_eq!(
            end_desktop_session_in(dir.path(), None, "linux-session", Some(i64::MAX)),
            Err(StopRefusal::Protected)
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// A process outside this test's process tree, as another app's would
    /// be, that writes "INT" or "TERM" to `signals` before exiting.
    pub(crate) fn foreign_process(signals: &Path, resume_after_int: Option<&Path>) -> i32 {
        let resume = resume_after_int.map(|dir| format!(
            "    if name == 'INT':\n        p={dir:?}+'/'+str(os.getpid())+'.json'; r=json.load(open(p)); r['statusUpdatedAt']=3000; open(p,'w').write(json.dumps(r)); return\n",
            dir=dir.to_str().unwrap())).unwrap_or_default();
        let ready = signals.with_extension("ready");
        let script = format!(
            "import signal,sys,time,os,json\n\
             def on(name):\n    open({out:?},'w').write(name)\n{resume}    sys.exit(0)\n\
             signal.signal(signal.SIGINT, lambda *a: on('INT'))\n\
             signal.signal(signal.SIGTERM, lambda *a: on('TERM'))\n\
             open({ready:?},'w').close()\n\
             time.sleep(30)\n",
            out = signals.to_str().unwrap(),
            ready = ready.to_str().unwrap()
        );
        // The shell exits at once, so the process is re-parented away from us.
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "/usr/bin/python3 -c \"$0\" >/dev/null 2>&1 & echo $!",
                &script,
            ])
            .output()
            .unwrap();
        let pid: i32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        for _ in 0..1200 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(ready.exists(), "the stand-in process started");
        pid
    }

    // Another Agent SDK app (t3code) writes the same records as Wonder's own
    // Claude runs. Only processes Wonder started are its own; another app's
    // process holds the chat like a terminal: a plain send waits and never
    // stops it, only Send now does (owner, Oct 18). The desktop app's process
    // is a writer that Wonder's message stops, with SIGINT so Claude Code
    // keeps the reply written so far.
    #[cfg(target_os = "macos")]
    #[test]
    fn another_sdk_apps_process_waits_the_desktop_apps_is_stopped_with_sigint() {
        let sessions = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(sessions.path());
        let write_as = |pid: i32, session: &str, entrypoint: &str| {
            std::fs::write(
                sessions.path().join(format!("{pid}.json")),
                serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": session, "status": "busy", "statusUpdatedAt": 1, "entrypoint": entrypoint})
                    .to_string(),
            )
            .unwrap()
        };
        let write = |pid: i32, session: &str| write_as(pid, session, "sdk-ts");
        let own = plain_process();
        write(own, "wonder");
        assert!(descends_from_this_daemon(own));
        assert!(
            live_sessions_in(sessions.path()).is_empty(),
            "Cupola's own run is not listed"
        );

        let signals = sessions.path().join("signals");
        let other = foreign_process(&signals, None);
        assert!(!descends_from_this_daemon(other));
        write(other, "shared");
        assert_eq!(
            live_sessions_in(sessions.path()),
            HashMap::from([("shared".to_owned(), true)])
        );
        // Unverified executable: protected like a terminal.
        assert_eq!(
            holds_in(sessions.path(), None, "shared", None, true),
            Some(StopRefusal::Protected)
        );
        // Verified, it still holds the chat: a plain send waits for it.
        DESKTOP_PIDS.lock().unwrap().insert(other);
        assert_eq!(
            holds_in(sessions.path(), None, "shared", None, true),
            Some(StopRefusal::Protected)
        );
        assert_eq!(
            end_desktop_session_in(sessions.path(), None, "shared", Some(2)),
            Err(StopRefusal::Protected)
        );
        assert!(process_alive(other));
        assert!(!signals.exists(), "no signal reached the other app");

        // The same chat in the desktop app: Wonder's message stops it.
        write_as(other, "shared", "claude-desktop");
        assert_eq!(holds_in(sessions.path(), None, "shared", None, true), None);
        assert_eq!(
            end_desktop_session_in(sessions.path(), None, "shared", Some(2)),
            Ok(())
        );
        assert!(!process_alive(other));
        assert_eq!(std::fs::read_to_string(&signals).unwrap(), "INT");

        // The exact process survives Stop and starts a newer reply. Escalation
        // must reread the native status clock before sending SIGTERM.
        let resumed_signals = sessions.path().join("resumed-signals");
        let resumed = foreign_process(&resumed_signals, Some(sessions.path()));
        DESKTOP_PIDS.lock().unwrap().insert(resumed);
        write_as(resumed, "shared", "claude-desktop");
        assert_eq!(
            end_desktop_session_in(sessions.path(), None, "shared", Some(2000)),
            Err(StopRefusal::Busy)
        );
        assert!(process_alive(resumed));
        assert_eq!(
            std::fs::read_to_string(&resumed_signals).unwrap(),
            "INT",
            "no SIGTERM reached the newer reply"
        );
        unsafe {
            libc::kill(resumed, libc::SIGTERM);
            libc::kill(own, libc::SIGTERM);
        };
    }

    // The transcript is read from its end. When the newest record is cut off
    // (being appended, or larger than the bounded read, such as a prompt with a
    // large inline image), the turn's state is unknown: the busy record wins
    // and the chat shows as working, even when an older reply had finished.
    #[test]
    fn an_incomplete_newest_record_shows_a_busy_chat_working() {
        let sessions = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let cwd = "/work/Big";
        let folder = projects.path().join(project_folder_name(cwd));
        std::fs::create_dir_all(&folder).unwrap();
        let pid = desktop_process();
        std::fs::write(
            sessions.path().join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": "s2", "status": "busy", "cwd": cwd, "entrypoint": "claude-desktop"})
                .to_string(),
        )
        .unwrap();
        let reply = serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": []}});
        let image = "A".repeat(TRANSCRIPT_TAIL_BYTES as usize + 4096);
        let large = serde_json::json!({"type": "user", "message": {"content": [{"type": "image", "source": {"data": image}}]}});
        let transcript = folder.join("s2.jsonl");
        let busy = || busy_sessions_with_transcripts(sessions.path(), Some(projects.path())).busy;
        for (label, text) in [
            ("larger than the read", format!("{reply}\n{large}\n")),
            (
                "being appended",
                format!("{reply}\n{{\"type\":\"user\",\"message\":{{\"content\":\"Go"),
            ),
        ] {
            std::fs::write(&transcript, text).unwrap();
            assert_eq!(busy(), HashSet::from(["s2".to_owned()]), "{label}");
            assert_eq!(main_turn_running(&transcript), None, "{label}");
        }
        // A whole transcript with no reply yet is known: nothing runs.
        std::fs::write(&transcript, "{\"type\":\"summary\"}\n").unwrap();
        assert_eq!(main_turn_running(&transcript), Some(false));
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }

    // Contract (owner, Nov 6): a Claude session whose newest entries are a
    // `/compact` and the "Compacted" it printed (T3 Code compacts an idle
    // thread before its prompt cache expires, with no reply after) is idle,
    // not working. Entries are the shape on the owner's Mac.
    #[test]
    fn a_compacted_session_with_no_reply_after_it_is_not_running() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compacted.jsonl");
        let line = |value: serde_json::Value| format!("{value}\n");
        let append = |text: &str| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
            std::thread::sleep(TRANSCRIPT_SETTLE + std::time::Duration::from_millis(50));
        };
        append(&line(
            serde_json::json!({"type":"user","message":{"content":"Go"}}),
        ));
        append(&line(
            serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}),
        ));
        append(&line(
            serde_json::json!({"type":"system","subtype":"compact_boundary","content":"Conversation compacted"}),
        ));
        append(&line(
            serde_json::json!({"type":"user","isCompactSummary":true,
            "message":{"content":"This session is being continued from a previous conversation"}}),
        ));
        append(&line(serde_json::json!({"type":"user","isMeta":true,
            "message":{"content":"<local-command-caveat>Caveat</local-command-caveat>"}})));
        append(&line(serde_json::json!({"type":"user",
            "message":{"content":"<command-name>/compact</command-name>\n<command-message>compact</command-message>"}})));
        assert_eq!(
            main_turn_running(&path),
            Some(true),
            "compacting is work until it reports"
        );
        append(&line(serde_json::json!({"type":"user",
            "message":{"content":"<local-command-stdout>Compacted </local-command-stdout>"}})));
        assert_eq!(
            main_turn_running(&path),
            Some(false),
            "the printed result ends the command's turn"
        );
        let mut log = PromptLog::default();
        assert!(log.read(&path));
        append(&line(
            serde_json::json!({"type":"user","message":{"content":"Next"}}),
        ));
        assert!(log.read(&path));
        assert!(log.running);
        append(&line(
            serde_json::json!({"type":"user","message":{"content":"<command-name>/compact</command-name>"}}),
        ));
        append(&line(serde_json::json!({"type":"user",
            "message":{"content":"<local-command-stdout>Compacted </local-command-stdout>"}})));
        assert!(log.read(&path));
        assert!(!log.running, "the prompt log ends it as well");
    }

    // Who started a turn, across the transcript shapes on the owner's Mac
    // (Claude Code 2.1.28x-2.1.29x, desktop, terminal and Agent SDK apps).
    #[test]
    fn transcript_entries_tell_the_owners_prompts_from_wakeups() {
        use UserEntry::*;
        let user = |extra: serde_json::Value, content: serde_json::Value| {
            let mut entry = serde_json::json!({"type": "user", "message": {"role": "user", "content": content}});
            entry
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            entry
        };
        let none = serde_json::json!({});
        let human = serde_json::json!({"promptSource": "sdk", "turnOrigin": "human", "origin": {"kind": "human"}});
        let sdk = serde_json::json!({"promptSource": "sdk", "turnOrigin": "sdk"});
        let text = |text: &str| serde_json::json!([{"type": "text", "text": text}]);
        for (label, entry, expected) in [
            (
                "typed in Claude",
                user(human.clone(), "Fix it".into()),
                Prompt,
            ),
            (
                "typed with reminders first",
                user(
                    human.clone(),
                    text("<system-reminder>\nLinked sessions</system-reminder>\nFix it"),
                ),
                Prompt,
            ),
            (
                "sent by an Agent SDK app",
                user(sdk.clone(), "Fix it".into()),
                Prompt,
            ),
            (
                "before origin was recorded",
                user(none.clone(), "Fix it".into()),
                Prompt,
            ),
            (
                "a slash command",
                user(human.clone(), "<command-name>/review</command-name>".into()),
                Prompt,
            ),
            (
                "an agent reported back",
                user(
                    serde_json::json!({"promptSource": "system", "turnOrigin": "task_notification", "origin": {"kind": "task-notification"}}),
                    "<task-notification>\n<task-id>a</task-id>".into(),
                ),
                Wake,
            ),
            (
                "a scheduled task",
                user(
                    serde_json::json!({"promptSource": "system", "turnOrigin": "scheduled"}),
                    "Daily check".into(),
                ),
                Wake,
            ),
            (
                "another session wrote",
                user(
                    serde_json::json!({"origin": {"kind": "peer"}}),
                    "Hello".into(),
                ),
                Wake,
            ),
            (
                "an older agent report",
                user(
                    none.clone(),
                    "<task-notification>\n<task-id>a</task-id>".into(),
                ),
                Wake,
            ),
            (
                "a CI monitor event",
                user(sdk.clone(), "<ci-monitor-event>\"Checks\" failed".into()),
                Wake,
            ),
            (
                "only reminders",
                user(
                    sdk.clone(),
                    text("<system-reminder>\nThe session you started finished.</system-reminder>"),
                ),
                Wake,
            ),
            (
                "a tool result",
                user(
                    none.clone(),
                    serde_json::json!([{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]),
                ),
                ToolResult,
            ),
            (
                "stopped",
                user(
                    none.clone(),
                    text("[Request interrupted by user for tool use]"),
                ),
                Interrupt,
            ),
            (
                "a local command's output",
                user(
                    none.clone(),
                    "<local-command-stdout>Set model</local-command-stdout>".into(),
                ),
                Local,
            ),
            (
                "shell input",
                user(human.clone(), "<bash-input>ls</bash-input>".into()),
                Local,
            ),
        ] {
            assert_eq!(user_entry(&entry), expected, "{label}");
        }
        // A prompt is read once its line is complete.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut log = PromptLog::default();
        assert!(log.read(&path));
        let line = user(serde_json::json!({"uuid": "p1"}), "Fix it".into()).to_string();
        let (head, tail) = line.split_at(20);
        std::fs::write(&path, head).unwrap();
        assert!(log.read(&path));
        assert_eq!(log.prompt, None);
        std::fs::write(&path, format!("{head}{tail}\n")).unwrap();
        assert!(log.read(&path));
        assert_eq!((log.prompt.as_deref(), log.running), (Some("p1"), true));
        // A local command that Claude did not answer is no prompt.
        std::fs::write(
            &path,
            format!(
                "{line}\n{}\n{}\n",
                user(
                    serde_json::json!({"uuid": "c1"}),
                    "<command-name>/model</command-name>".into()
                ),
                user(
                    none,
                    "<local-command-stdout>Set model</local-command-stdout>".into()
                )
            ),
        )
        .unwrap();
        assert!(log.read(&path));
        assert_eq!(log.prompt, None);
    }

    pub(crate) type Interleaving = Box<dyn FnOnce() + Send>;
    pub(crate) static AFTER_CHECK: std::sync::LazyLock<
        std::sync::Mutex<HashMap<String, Interleaving>>,
    > = std::sync::LazyLock::new(Default::default);

    /// Runs what a test scheduled for this session between the handoff's check
    /// and its signal: the owner acting on the Mac in that gap.
    pub(super) fn after_check(session: &str) {
        let step = AFTER_CHECK.lock().unwrap().remove(session);
        if let Some(step) = step {
            step();
        }
    }

    #[cfg(target_os = "macos")]
    fn plain_process() -> i32 {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        std::thread::spawn(move || child.wait());
        pid
    }

    // The owner starts a Desktop reply, or opens the chat in a terminal, after
    // the handoff checked the chat: nothing is signalled.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_handoff_never_signals_a_process_that_changed_after_its_check() {
        let sessions = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(sessions.path());
        let projects = tempfile::tempdir().unwrap();
        let cwd = "/work/Race";
        let folder = projects.path().join(project_folder_name(cwd));
        std::fs::create_dir_all(&folder).unwrap();
        let write = |pid: i32, status: &str, entrypoint: &str| {
            std::fs::write(
                sessions.path().join(format!("{pid}.json")),
                serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": "s3", "status": status, "cwd": cwd, "entrypoint": entrypoint})
                    .to_string(),
            )
            .unwrap()
        };
        let reply = serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": []}});
        let prompt = serde_json::json!({"type": "user", "message": {"content": "Next"}});
        let transcript = folder.join("s3.jsonl");

        // A reply starts in the Desktop process.
        let desktop = desktop_process();
        write(desktop, "idle", "claude-desktop");
        std::fs::write(&transcript, format!("{reply}\n")).unwrap();
        let (sessions_path, transcript_path) = (sessions.path().to_owned(), transcript.clone());
        AFTER_CHECK.lock().unwrap().insert(
            "s3".into(),
            Box::new(move || {
                std::fs::write(
                    sessions_path.join(format!("{desktop}.json")),
                    serde_json::json!({"pid": desktop, "procStart": process_start_label(desktop), "sessionId": "s3", "status": "busy", "cwd": "/work/Race", "entrypoint": "claude-desktop"})
                        .to_string(),
                )
                .unwrap();
                std::fs::write(&transcript_path, format!("{reply}\n{prompt}\n")).unwrap();
            }),
        );
        let result = end_desktop_session_in(sessions.path(), None, "s3", Some(0));
        assert!(process_alive(desktop), "the new reply's process was ended");
        assert_eq!(result, Err(StopRefusal::Busy));

        // The status stays busy, but its native clock changes: the Mac sent
        // again after the Wonder message. Identity includes this clock.
        for (field, value) in [
            ("statusUpdatedAt", serde_json::json!(3000)),
            ("procStart", serde_json::json!("a different process")),
        ] {
            let record_path = sessions.path().join(format!("{desktop}.json"));
            let old = serde_json::json!({"pid": desktop, "procStart": process_start_label(desktop), "sessionId":"s3", "status":"busy", "statusUpdatedAt":1000, "entrypoint":"claude-desktop"});
            std::fs::write(&record_path, old.to_string()).unwrap();
            AFTER_CHECK.lock().unwrap().insert(
                "s3".into(),
                Box::new(move || {
                    let mut next = old;
                    next[field] = value;
                    std::fs::write(&record_path, next.to_string()).unwrap();
                }),
            );
            assert_eq!(
                end_desktop_session_in(sessions.path(), None, "s3", Some(2000)),
                Err(if field == "procStart" {
                    StopRefusal::Unknown
                } else {
                    StopRefusal::Busy
                }),
                "{field}"
            );
            assert!(process_alive(desktop), "{field}");
        }

        // The chat opens in a terminal.
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn"}})
            ),
        )
        .unwrap();
        write(desktop, "idle", "claude-desktop");
        let terminal = plain_process();
        let sessions_path = sessions.path().to_owned();
        AFTER_CHECK.lock().unwrap().insert(
            "s3".into(),
            Box::new(move || {
                std::fs::write(
                    sessions_path.join(format!("{terminal}.json")),
                    serde_json::json!({"pid": terminal, "procStart": process_start_label(terminal), "sessionId": "s3", "status": "idle", "cwd": "/work/Race", "entrypoint": "cli"})
                        .to_string(),
                )
                .unwrap();
            }),
        );
        let result = end_desktop_session_in(sessions.path(), None, "s3", Some(0));
        assert!(process_alive(terminal), "the terminal's process was ended");
        assert!(
            process_alive(desktop),
            "the Desktop process was ended while a terminal held the chat"
        );
        assert_ne!(result, Ok(()));
        unsafe {
            libc::kill(desktop, libc::SIGTERM);
            libc::kill(terminal, libc::SIGTERM);
        }
    }

    #[test]
    fn app_names_come_from_the_outermost_bundle() {
        for (path, name) in [
            (
                "/Applications/T3 Code (Beta).app/Contents/MacOS/T3 Code (Beta)",
                Some("T3 Code"),
            ),
            ("/Applications/Claude.app/Contents/Helpers/disclaimer", Some("Claude")),
            (
                "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)",
                Some("Visual Studio Code"),
            ),
            (
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
                Some("Terminal"),
            ),
            ("/usr/bin/login", None),
            ("/opt/homebrew/bin/tmux", None),
        ] {
            assert_eq!(app_bundle_name(path).as_deref(), name, "{path}");
        }
    }

    #[cfg(target_os = "macos")]
    fn write_record(dir: &Path, pid: i32, session: &str, entrypoint: &str, status: &str) {
        std::fs::write(
            dir.join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": session, "status": status, "statusUpdatedAt": 1, "cwd": "/work/Held", "entrypoint": entrypoint})
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_t3_claude_wrapped_process_is_an_sdk_app() {
        assert_eq!(kind_of(Some("t3-code")), Kind::SdkApp);
        assert_eq!(kind_of(Some("sdk-ts")), Kind::SdkApp);
        assert_eq!(kind_of(Some("claude-desktop")), Kind::Desktop);
        assert_eq!(kind_of(Some("cli")), Kind::Terminal);
        assert_eq!(kind_of(None), Kind::Terminal);
    }

    // Owner (Oct 18): a T3 Code thread said "Sends after you exit Claude
    // Code". Each live record now names the app that holds the chat, from its
    // entrypoint and the app among its parent processes. A message waits for
    // a terminal, another Agent SDK app or background work, not for the
    // desktop app; Send now can end any verified holder.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_holder_is_named_by_its_kind_and_the_app_that_started_it() {
        let sessions = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(sessions.path());
        let dir = sessions.path();
        let holder = |session: &str| holder_in(dir, None, session).unwrap();
        let verified = |name: &str| {
            let pid = foreign_process(&dir.join(name), None);
            DESKTOP_PIDS.lock().unwrap().insert(pid);
            pid
        };

        let desktop = verified("desktop");
        HOST_APPS.lock().unwrap().insert(desktop, "Claude".into());
        write_record(dir, desktop, "desktop", "claude-desktop", "idle");
        let t3 = verified("t3");
        HOST_APPS.lock().unwrap().insert(t3, "T3 Code".into());
        write_record(dir, t3, "t3", "sdk-ts", "busy");
        let wrapped = verified("wrapped");
        HOST_APPS.lock().unwrap().insert(wrapped, "T3 Code".into());
        write_record(dir, wrapped, "wrapped", "t3-code", "idle");
        let terminal = verified("terminal");
        HOST_APPS
            .lock()
            .unwrap()
            .insert(terminal, "Terminal".into());
        write_record(dir, terminal, "terminal", "cli", "idle");
        // Started under no app (re-parented, as in tmux): still named.
        let bare = verified("bare");
        write_record(dir, bare, "bare", "cli", "idle");
        // Not Claude Code's executable: named, but never signalled.
        let unverified = foreign_process(&dir.join("unverified"), None);
        HOST_APPS
            .lock()
            .unwrap()
            .insert(unverified, "T3 Code".into());
        write_record(dir, unverified, "unverified", "sdk-ts", "idle");

        assert_eq!(
            holder("desktop"),
            Holder {
                app: "Claude".into(),
                kind: HolderKind::Desktop,
                replying: false,
                background: false,
                waits: false,
                can_take_over: true,
            }
        );
        assert_eq!(
            holder("t3"),
            Holder {
                app: "T3 Code".into(),
                kind: HolderKind::App,
                replying: true,
                background: false,
                waits: true,
                can_take_over: true,
            },
            "only Send now stops another app's reply"
        );
        let wrapped = holder("wrapped");
        assert_eq!(
            (wrapped.app.as_str(), wrapped.kind),
            ("T3 Code", HolderKind::App),
            "a t3-claude wrapped process (entrypoint t3-code) is T3 Code's"
        );
        let held = holder("terminal");
        assert_eq!(
            (held.app.as_str(), held.kind, held.waits, held.can_take_over),
            ("Claude Code in Terminal", HolderKind::Terminal, true, true)
        );
        assert_eq!(holder("bare").app, "Claude Code in a terminal");
        let held = holder("unverified");
        assert_eq!(
            (held.app.as_str(), held.waits, held.can_take_over),
            ("T3 Code", true, false)
        );
        write_record(dir, t3, "t3", "sdk-ts", "shell");
        let held = holder("t3");
        assert!(held.background && held.waits && !held.replying);
        assert_eq!(holder_in(dir, None, "absent"), None);
        for pid in [desktop, t3, terminal, bare, unverified] {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    // Send now ends exactly the processes holding this chat whose identity is
    // verified, never another chat's, never a process it cannot prove is
    // Claude Code, and nothing when the app opened the chat again meanwhile.
    #[cfg(target_os = "macos")]
    #[test]
    fn send_now_ends_only_the_verified_process_holding_this_chat() {
        let sessions = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(sessions.path());
        let projects = tempfile::tempdir().unwrap();
        let dir = sessions.path();
        let folder = projects.path().join(project_folder_name("/work/Held"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("s1.jsonl"), "{\"type\":\"summary\"}\n").unwrap();
        let take = |session: &str| take_over_in(dir, Some(projects.path()), session);
        let verified = |name: &str| {
            let pid = foreign_process(&dir.join(name), None);
            DESKTOP_PIDS.lock().unwrap().insert(pid);
            pid
        };

        let stranger = foreign_process(&dir.join("stranger"), None);
        write_record(dir, stranger, "s3", "cli", "idle");
        assert_eq!(take("s3"), Err(StopRefusal::Unverified));
        assert!(process_alive(stranger) && !dir.join("stranger").exists());
        // A record another process start wrote (a reused pid) is unknown evidence.
        std::fs::write(
            dir.join(format!("{stranger}.json")),
            serde_json::json!({"pid": stranger, "procStart": "a different process", "sessionId": "s3", "status": "idle", "entrypoint": "cli"}).to_string(),
        )
        .unwrap();
        DESKTOP_PIDS.lock().unwrap().insert(stranger);
        assert_eq!(take("s3"), Err(StopRefusal::Unknown));
        assert!(process_alive(stranger) && !dir.join("stranger").exists());
        std::fs::remove_file(dir.join(format!("{stranger}.json"))).unwrap();

        // A terminal mid-reply, and another chat's process beside it.
        let held = verified("held");
        write_record(dir, held, "s1", "cli", "busy");
        let other = verified("other");
        write_record(dir, other, "s2", "claude-desktop", "busy");
        assert_eq!(take("s1"), Ok(()));
        assert!(!process_alive(held));
        assert_eq!(std::fs::read_to_string(dir.join("held")).unwrap(), "INT");
        assert!(process_alive(other), "another chat's process kept running");
        assert!(!dir.join("other").exists());
        assert_eq!(take("s1"), Ok(()), "nothing holds it any more");

        // The app opens the chat again in a new process after the check.
        let again = verified("again");
        write_record(dir, again, "s4", "sdk-ts", "idle");
        let reopened = verified("reopened");
        let path = dir.to_owned();
        AFTER_CHECK.lock().unwrap().insert(
            "s4".into(),
            Box::new(move || write_record(&path, reopened, "s4", "sdk-ts", "idle")),
        );
        assert_eq!(take("s4"), Err(StopRefusal::Busy));
        assert!(process_alive(again) && process_alive(reopened));
        for pid in [stranger, other, again, reopened] {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    // A holder that ignores both signals is reported, not killed outright:
    // its message stays queued.
    #[cfg(target_os = "macos")]
    #[test]
    fn send_now_reports_a_holder_that_does_not_exit() {
        let sessions = tempfile::tempdir().unwrap();
        let _scan_clock = ScopedScanClock::new(sessions.path());
        let process = StubbornProcess::start(60);
        let stubborn = process.pid;
        DESKTOP_PIDS.lock().unwrap().insert(stubborn);
        write_record(sessions.path(), stubborn, "s5", "cli", "idle");
        let started = std::time::Instant::now();
        assert_eq!(
            take_over_in(sessions.path(), None, "s5"),
            Err(StopRefusal::StillRunning)
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(13));
        assert!(process_alive(stubborn));
    }

    /// A shell outside this test's tree that ignores SIGINT and SIGTERM, in
    /// process group `group` of its own. Dropping it, including when an
    /// assertion panics, kills the group (the shell and its `sleep`); it exits
    /// by itself after `lifetime_secs`, so a test binary killed outright
    /// leaves nothing behind for long. Without both, failed runs left these
    /// shells under init for good.
    pub(crate) struct StubbornProcess {
        pub(crate) pid: i32,
        pub(crate) group: i32,
    }

    impl StubbornProcess {
        pub(crate) fn start(lifetime_secs: u32) -> Self {
            use std::os::unix::process::CommandExt;
            let script = format!(
                "/bin/sh -c 'trap \"\" INT TERM; i=0; \
                 while [ $i -lt {lifetime_secs} ]; do sleep 1; i=$((i+1)); done' \
                 >/dev/null 2>&1 & echo $!"
            );
            // The outer shell leads a new group that its background job keeps
            // after it exits; the group kill never reaches this test binary.
            let child = std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .stdout(std::process::Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap();
            let group = child.id() as i32;
            let output = child.wait_with_output().unwrap();
            let pid = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .unwrap();
            Self { pid, group }
        }
    }

    impl Drop for StubbornProcess {
        fn drop(&mut self) {
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
    }

    /// Live (non-zombie) processes in process group `group`.
    fn group_members(group: i32) -> Vec<i32> {
        let output = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,pgid=,stat="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid: i32 = fields.next()?.parse().ok()?;
                let pgid: i32 = fields.next()?.parse().ok()?;
                let stat = fields.next()?;
                (pgid == group && !stat.starts_with('Z')).then_some(pid)
            })
            .collect()
    }

    fn wait_for_empty_group(group: i32, seconds: u64) -> Vec<i32> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        loop {
            let members = group_members(group);
            if members.is_empty() || std::time::Instant::now() > deadline {
                return members;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // A test that fails after starting a stubborn holder still removes it and
    // its `sleep`, and one whose cleanup never runs exits by itself.
    #[test]
    fn stubborn_stand_in_leaves_no_process_behind() {
        let group = std::panic::catch_unwind(|| {
            let process = StubbornProcess::start(60);
            let (pid, group) = (process.pid, process.group);
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let own = unsafe { libc::getpgid(0) };
            assert_ne!(group, own, "never this test binary's group");
            let members = group_members(group);
            assert!(members.contains(&pid), "in its group: {members:?}");
            assert!(members.len() >= 2, "the shell and its sleep: {members:?}");
            // SIGINT and SIGTERM leave it running, as Send now finds it.
            unsafe {
                libc::kill(pid, libc::SIGINT);
                libc::kill(pid, libc::SIGTERM);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(process_alive(pid));
            std::panic::panic_any(group);
        })
        .unwrap_err()
        .downcast::<i32>()
        .map(|group| *group)
        .expect("the stand-in started");
        assert_eq!(wait_for_empty_group(group, 5), Vec::<i32>::new());

        // Cleanup skipped, as when the test binary is killed outright.
        let forgotten = StubbornProcess::start(2);
        let group = forgotten.group;
        std::mem::forget(forgotten);
        assert_eq!(wait_for_empty_group(group, 10), Vec::<i32>::new());
    }

    /// A stand-in codex process outside this test's tree that keeps `files`
    /// open and writes "INT" or "TERM" to `signals` before exiting.
    #[cfg(target_os = "macos")]
    pub(crate) fn codex_stand_in(files: &[&Path], signals: &Path) -> i32 {
        let ready = signals.with_extension("ready");
        let opens: Vec<String> = files
            .iter()
            .map(|file| format!("open({:?},'a')", file.to_str().unwrap()))
            .collect();
        let script = format!(
            "import signal,sys,time\nkept=[{}]\n\
             def on(name):\n    open({out:?},'w').write(name); sys.exit(0)\n\
             signal.signal(signal.SIGINT, lambda *a: on('INT'))\n\
             signal.signal(signal.SIGTERM, lambda *a: on('TERM'))\n\
             open({ready:?},'w').close()\ntime.sleep(30)\n",
            opens.join(","),
            out = signals.to_str().unwrap(),
            ready = ready.to_str().unwrap()
        );
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "/usr/bin/python3 -c \"$0\" >/dev/null 2>&1 & echo $!",
                &script,
            ])
            .output()
            .unwrap();
        let pid: i32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        for _ in 0..1200 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(ready.exists(), "the stand-in codex process started");
        CODEX_PIDS.lock().unwrap().insert(pid);
        pid
    }

    // Codex keeps one writer per thread: the process with its rollout open.
    // Send now stops a codex CLI that has only this thread open. A `codex
    // app-server` serves other threads too whichever app runs it (Codex's
    // desktop app, T3 Code, VS Code or one it cannot name), even while it has
    // only this thread open; so does a CLI with two threads open; a process
    // whose arguments cannot be read is never classified. None is signalled.
    #[cfg(target_os = "macos")]
    #[test]
    fn codex_send_now_stops_only_a_cli_that_has_the_thread_alone() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = |name: &str| {
            let path = dir
                .path()
                .join(format!("rollout-2026-10-18T00-00-00-{name}.jsonl"));
            std::fs::write(&path, "{}\n").unwrap();
            path
        };
        let other_thread = rollout("other");
        let server = ["codex", "app-server", "--analytics-default-enabled"];
        /// Name, process arguments, parent app, shares other threads, shown app,
        /// holder kind, refusal.
        type Case<'a> = (
            &'a str,
            Option<&'a [&'a str]>,
            Option<&'a str>,
            bool,
            &'a str,
            HolderKind,
            StopRefusal,
        );
        let cases: [Case; 6] = [
            (
                "chatgpt",
                Some(&server),
                Some("ChatGPT"),
                true,
                "ChatGPT",
                HolderKind::Desktop,
                StopRefusal::Shared,
            ),
            (
                "t3",
                Some(&server),
                Some("T3 Code"),
                false,
                "T3 Code",
                HolderKind::App,
                StopRefusal::Shared,
            ),
            (
                "vscode",
                Some(&server),
                Some("Visual Studio Code"),
                false,
                "Visual Studio Code",
                HolderKind::App,
                StopRefusal::Shared,
            ),
            (
                "no-app",
                Some(&server),
                None,
                false,
                "Codex",
                HolderKind::App,
                StopRefusal::Shared,
            ),
            (
                "two-threads",
                Some(&["codex", "resume"]),
                Some("Terminal"),
                true,
                "Codex CLI in Terminal",
                HolderKind::Terminal,
                StopRefusal::Shared,
            ),
            (
                "unreadable",
                None,
                None,
                false,
                "Codex",
                HolderKind::App,
                StopRefusal::Unverified,
            ),
        ];
        let mut refused = Vec::new();
        for (name, arguments, host, other, app, kind, refusal) in cases {
            let thread = rollout(name);
            let files: Vec<&Path> = if other {
                vec![&thread, &other_thread]
            } else {
                vec![&thread]
            };
            let pid = codex_stand_in(&files, &dir.path().join(name));
            if let Some(arguments) = arguments {
                let arguments = arguments.iter().map(|arg| (*arg).to_owned()).collect();
                CODEX_ARGS.lock().unwrap().insert(pid, arguments);
            }
            if let Some(host) = host {
                HOST_APPS.lock().unwrap().insert(pid, host.into());
            }
            let held = codex_holder_of(&thread).unwrap();
            assert_eq!(
                (
                    held.holder.app.as_str(),
                    held.holder.kind,
                    held.holder.can_take_over
                ),
                (app, kind, false),
                "{name}"
            );
            assert_eq!(take_over_codex_in(&thread), Err(refusal), "{name}");
            refused.push((name, pid));
        }

        let cli_thread = rollout("cli");
        let cli = codex_stand_in(&[&cli_thread], &dir.path().join("cli"));
        let arguments = ["codex", "resume", "--last"].map(str::to_owned).to_vec();
        CODEX_ARGS.lock().unwrap().insert(cli, arguments);
        HOST_APPS.lock().unwrap().insert(cli, "Terminal".into());
        let held = codex_holder_of(&cli_thread).unwrap();
        assert_eq!(
            (
                held.holder.app.as_str(),
                held.holder.kind,
                held.holder.can_take_over
            ),
            ("Codex CLI in Terminal", HolderKind::Terminal, true)
        );
        assert!(codex_holder_of(&rollout("free")).is_none());
        assert_eq!(take_over_codex_in(&cli_thread), Ok(()));
        assert!(!process_alive(cli));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cli")).unwrap(),
            "INT"
        );
        assert_eq!(
            take_over_codex_in(&cli_thread),
            Ok(()),
            "nothing holds it now"
        );

        std::thread::sleep(std::time::Duration::from_millis(200));
        for (name, pid) in refused {
            assert!(process_alive(pid), "{name} kept running");
            assert!(!dir.path().join(name).exists(), "{name} was not signalled");
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    // Send now classifies a codex process by its own arguments.
    #[cfg(target_os = "macos")]
    #[test]
    fn process_arguments_reads_another_processs_argv() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let arguments = process_arguments(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            arguments,
            Some(["/bin/sleep", "30"].map(str::to_owned).to_vec())
        );
        assert_eq!(process_arguments(-1), None);
    }

    // History counts as saved once the transcript stops changing.
    #[test]
    fn send_now_waits_for_the_transcript_to_stop_changing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        // An append is a change.
        let before = transcript_marks(std::slice::from_ref(&path));
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{{}}").unwrap();
        }
        assert_ne!(transcript_marks(std::slice::from_ref(&path)), before);
        // A transcript that changes between every read is Busy at the
        // deadline. Each read changes, rather than a writer thread that a
        // busy Mac could leave unscheduled for a whole poll interval.
        let mut reads = 0;
        let soon = std::time::Instant::now() + std::time::Duration::from_millis(200);
        assert_eq!(
            wait_until_quiet(
                || {
                    reads += 1;
                    reads
                },
                soon
            ),
            Err(StopRefusal::Busy)
        );
        // A transcript that stopped changing is saved.
        let later = std::time::Instant::now() + std::time::Duration::from_secs(60);
        assert_eq!(wait_for_flush(&[path], later), Ok(()));
    }

    #[tokio::test]
    async fn transcript_consumers_share_ingestion_and_follow_rewrites() {
        use crate::context_usage::{resolve, Source};
        use base64::Engine;
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.jsonl");
        let bytes: Vec<u8> = (0..80).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let reply = serde_json::json!({"type":"assistant", "timestamp":"1970-01-01T00:00:01Z",
            "message":{"stop_reason":"end_turn"},
            "context_usage":{"total_tokens":100, "raw_max_tokens":1000, "model":"fixture"}});
        let picture = serde_json::json!({"type":"user", "isMeta":true,
            "message":{"content":[{"type":"image", "source":{"data":encoded}}]}});
        let initial = format!("{reply}\n{picture}\n");
        std::fs::write(&path, &initial).unwrap();
        let source = Some(Source::ClaudeTranscript(path.clone()));
        let ingested = || {
            INGESTED_BYTES
                .lock()
                .unwrap()
                .get(&path)
                .copied()
                .unwrap_or_default()
        };
        assert_eq!(main_turn_running(&path), Some(false));
        assert_eq!(
            resolve("shared-tail", source.clone())
                .await
                .unwrap()
                .used_tokens,
            100
        );
        let location = crate::media_refs::locate_in_transcript(&path, &bytes).unwrap();
        assert_eq!(
            ingested(),
            initial.len(),
            "all consumers share the same ingestion pass"
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let prompt = "{\"type\":\"user\",\"message\":{\"content\":\"Next\"}}";
        file.write_all(&prompt.as_bytes()[..20]).unwrap();
        assert_eq!(
            main_turn_running(&path),
            None,
            "an incomplete newest line is not idle"
        );
        assert_eq!(
            resolve("shared-tail", source.clone())
                .await
                .unwrap()
                .used_tokens,
            100
        );
        assert_eq!(
            ingested(),
            initial.len(),
            "partial records are not ingested"
        );
        file.write_all(&prompt.as_bytes()[20..]).unwrap();
        file.write_all(b"\n").unwrap();
        assert_eq!(main_turn_running(&path), Some(true));
        assert_eq!(
            crate::media_refs::locate_in_transcript(&path, &bytes),
            Some(location)
        );
        assert_eq!(
            ingested(),
            initial.len() + prompt.len() + 1,
            "only appended records are ingested"
        );
        let invalid_usage = serde_json::json!({"type":"assistant", "timestamp":"1970-01-01T00:00:02Z",
            "message":{"stop_reason":"end_turn", "model":"fixture", "usage":{"input_tokens":u64::MAX, "output_tokens":1}}});
        writeln!(file, "{invalid_usage}").unwrap();
        assert_eq!(
            main_turn_running(&path),
            Some(false),
            "invalid usage must not panic the activity reader"
        );
        assert_eq!(
            resolve("shared-tail", source.clone())
                .await
                .unwrap()
                .used_tokens,
            100
        );
        // Replacement with a longer transcript invalidates every projection,
        // including a previously found image and a completed main turn.
        let replacement = path.with_extension("replacement");
        let compact = serde_json::json!({"type":"system", "subtype":"compact_boundary", "timestamp":"1970-01-01T00:00:02Z"});
        std::fs::write(
            &replacement,
            format!("{compact}\n{prompt}\n{}\n", " ".repeat(initial.len())),
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(main_turn_running(&path), Some(true));
        assert_eq!(resolve("shared-tail", source).await, None);
        assert!(crate::media_refs::locate_in_transcript(&path, &bytes).is_none());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(main_turn_running(&path), None);
    }

    #[test]
    fn an_oversized_record_yields_and_the_next_poll_reaches_its_reply() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"{\"huge\":\"").unwrap();
        // A sparse fixture avoids retaining a huge string in the test itself.
        file.seek(SeekFrom::Start((64 << 20) + 1)).unwrap();
        file.write_all(
            b"\"}\n{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();
        assert_eq!(
            main_turn_running(&path),
            None,
            "the first bounded ingestion yields unknown"
        );
        assert_eq!(
            main_turn_running(&path),
            Some(false),
            "the cursor resumes past the oversized record"
        );
        assert_eq!(main_turn_running(&path), Some(false));
    }

    /// CPU (user + system), not elapsed time: run alone with --nocapture.
    #[test]
    fn twenty_chat_busy_poll_cpu() {
        use std::io::Write;
        fn cpu() -> f64 {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            assert_eq!(
                unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
                0
            );
            let usage = unsafe { usage.assume_init() };
            usage.ru_utime.tv_sec as f64
                + usage.ru_utime.tv_usec as f64 / 1e6
                + usage.ru_stime.tv_sec as f64
                + usage.ru_stime.tv_usec as f64 / 1e6
        }
        let dir = tempfile::tempdir().unwrap();
        let reply = "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n";
        let prompt = "{\"type\":\"user\",\"message\":{\"content\":\"Go\"}}\n";
        let history = format!(
            "{}{}",
            "{\"type\":\"summary\",\"text\":\"history\"}\n".repeat(16_384),
            reply
        );
        let paths: Vec<_> = (0..20)
            .map(|n| dir.path().join(format!("{n}.jsonl")))
            .collect();
        for path in &paths {
            std::fs::write(path, &history).unwrap();
        }
        std::thread::sleep(TRANSCRIPT_SETTLE + std::time::Duration::from_millis(50));
        let start = cpu();
        for path in &paths {
            assert_eq!(main_turn_running(path), Some(false));
        }
        let cold = cpu() - start;
        let start = cpu();
        for _ in 0..50 {
            for path in &paths {
                assert_eq!(main_turn_running(path), Some(false));
            }
        }
        let unchanged = cpu() - start;
        let start = cpu();
        for poll in 0..50 {
            for path in &paths {
                let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                file.write_all(if poll % 2 == 0 { prompt } else { reply }.as_bytes())
                    .unwrap();
                assert_eq!(main_turn_running(path), Some(poll % 2 == 0));
            }
        }
        let appending = cpu() - start;
        eprintln!("TRANSCRIPT_POLL_CPU chats=20 polls=50 initial_bytes={} cold_ms={:.3} unchanged_ms={:.3} appending_ms={:.3}",
            history.len(), cold * 1000., unchanged * 1000., appending * 1000.);
    }

    // B-401: a busy chat's transcript that has not changed is not read
    // again on every list refresh and poll; any change is. A cut-off newest
    // record stays unknown until it is whole, never an older reply's answer.
    #[test]
    fn an_unchanged_transcript_is_read_once_and_a_change_is_read_again() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let reads = || {
            TRANSCRIPT_READS
                .lock()
                .unwrap()
                .get(&path)
                .copied()
                .unwrap_or_default()
        };
        let append = |text: &str, settled: bool| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
            if settled {
                std::thread::sleep(TRANSCRIPT_SETTLE + std::time::Duration::from_millis(50));
            }
        };
        let prompt = serde_json::json!({"type":"user","message":{"content":"Go"}});
        let reply = serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn","content":[]}});
        append(&format!("{prompt}\n"), true);
        assert_eq!(main_turn_running(&path), Some(true));
        assert_eq!(main_turn_running(&path), Some(true));
        assert_eq!(reads(), 1, "an unchanged transcript is read once");
        append(
            &format!("{reply}\n{{\"type\":\"user\",\"message\":{{\"content\":\"Next"),
            true,
        );
        assert_eq!(
            main_turn_running(&path),
            None,
            "cut off is unknown, not idle"
        );
        assert_eq!(main_turn_running(&path), None);
        assert_eq!(reads(), 2);
        append("\"}}\n", true);
        assert_eq!(main_turn_running(&path), Some(true));
        assert_eq!(reads(), 3);
        // Just written: its times may not change with the next write, so it
        // is read each time until it settles.
        append(&format!("{reply}\n"), false);
        assert_eq!(main_turn_running(&path), Some(false));
        assert_eq!(main_turn_running(&path), Some(false));
        assert_eq!(reads(), 5);
    }

    // B-401 lifecycle: a kept answer never outlives its transcript or its
    // process. A removed transcript is unknown, a replaced one (same length
    // and time, another file) is read again, and a process that exits leaves
    // its chat even while its transcript is unchanged.
    #[test]
    fn a_kept_answer_ends_with_its_transcript_and_its_process() {
        let sessions = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let cwd = "/work/Lifecycle";
        let transcript = transcript_in(projects.path(), cwd, "s5");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        // Every write carries the same modification time, as a copy that
        // keeps times would: only the file and its change time differ.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let write = |path: &Path, entry: serde_json::Value| {
            std::fs::write(path, format!("{entry}\n")).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(past)
                .unwrap();
        };
        let settle =
            || std::thread::sleep(TRANSCRIPT_SETTLE + std::time::Duration::from_millis(50));
        let reads = || {
            TRANSCRIPT_READS
                .lock()
                .unwrap()
                .get(&transcript)
                .copied()
                .unwrap_or_default()
        };
        // Same length either way.
        let running = serde_json::json!({"type":"user","message":{"content":"Keep going please"}});
        let stopped = serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn"}});
        assert_eq!(running.to_string().len(), stopped.to_string().len());
        write(&transcript, running);
        settle();
        let pid = desktop_process();
        std::fs::write(
            sessions.path().join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "procStart": process_start_label(pid), "sessionId": "s5", "status": "busy", "cwd": cwd, "entrypoint": "claude-desktop"})
                .to_string(),
        )
        .unwrap();
        let followed = HashSet::from(["s5".to_owned()]);
        let state = || {
            session_activity_in(sessions.path(), Some(projects.path()), &followed)
                .get("s5")
                .map(|activity| activity.state)
        };
        assert_eq!(state(), Some(ExternalActivity::Working));
        assert_eq!(state(), Some(ExternalActivity::Working));
        assert_eq!(reads(), 1);
        // Replaced by another file of the same length and time.
        let other = transcript.with_extension("new");
        write(&other, stopped);
        std::fs::rename(&other, &transcript).unwrap();
        settle();
        assert_eq!(state(), Some(ExternalActivity::Idle));
        assert_eq!(state(), Some(ExternalActivity::Idle));
        assert_eq!(reads(), 2);
        // Removed: the busy record is trusted again, not the kept answer.
        std::fs::remove_file(&transcript).unwrap();
        assert_eq!(main_turn_running(&transcript), None);
        assert_eq!(state(), Some(ExternalActivity::Working));
        // Written again under a reused file number, with the same length and
        // modification time as the kept answer's: still read again.
        write(
            &transcript,
            serde_json::json!({"type":"user","message":{"content":"Keep going please"}}),
        );
        assert_eq!(state(), Some(ExternalActivity::Working));
        settle();
        assert_eq!(state(), Some(ExternalActivity::Working));
        // The process exits with its transcript unchanged.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        // A hang guard only (about 10 s).
        for _ in 0..1000 {
            if state().is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("an exited process still shows its chat");
    }

    // B-394: the background poll reads only followed sessions' transcripts.
    #[test]
    fn session_activity_reads_only_followed_sessions_transcripts() {
        let me = std::process::id() as i32;
        let records = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let cwd = "/work/Unfollowed";
        let transcript = transcript_in(projects.path(), cwd, "s2");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type":"user","message":{"content":"Go"}})
            ),
        )
        .unwrap();
        std::fs::write(
            records.path().join(format!("{me}.json")),
            serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "s2", "status": "busy", "cwd": cwd}).to_string(),
        )
        .unwrap();
        let reads = || {
            TRANSCRIPT_READS
                .lock()
                .unwrap()
                .get(&transcript)
                .copied()
                .unwrap_or_default()
        };
        let other = HashSet::from(["s1".to_owned()]);
        assert!(session_activity_in(records.path(), Some(projects.path()), &other).is_empty());
        assert_eq!(reads(), 0, "a session nobody follows is not read");
        let followed = HashSet::from(["s2".to_owned()]);
        let activity = session_activity_in(records.path(), Some(projects.path()), &followed);
        assert_eq!(activity["s2"].state, ExternalActivity::Working);
        assert_eq!(reads(), 1);
    }

    #[test]
    fn session_activity_reads_prompts_replies_and_the_final_reply_text() {
        let me = std::process::id() as i32;
        let projects = tempfile::tempdir().unwrap();
        let cwd = "/work/Notify";
        let folder = projects.path().join(project_folder_name(cwd));
        std::fs::create_dir_all(&folder).unwrap();
        let transcript = folder.join("s1.jsonl");
        let line = |value: serde_json::Value| format!("{value}\n");
        let prompt =
            line(serde_json::json!({"type":"user","message":{"role":"user","content":"Hi"}}));
        let reply = line(
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"All done."}],"stop_reason":"end_turn"}}),
        );
        let side = line(
            serde_json::json!({"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"Subagent"}]}}),
        );
        let activity = |status: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join(format!("{me}.json")),
                serde_json::json!({"pid": me, "procStart": process_start_label(me), "sessionId": "s1", "status": status, "statusUpdatedAt": 7, "cwd": cwd}).to_string(),
            )
            .unwrap();
            session_activity_in(
                dir.path(),
                Some(projects.path()),
                &HashSet::from(["s1".to_owned()]),
            )
            .get("s1")
            .map(|activity| activity.state)
        };
        std::fs::write(&transcript, &prompt).unwrap();
        assert_eq!(activity("busy"), Some(ExternalActivity::Working));
        assert_eq!(activity("waiting"), Some(ExternalActivity::Waiting));
        std::fs::write(&transcript, format!("{prompt}{reply}{side}")).unwrap();
        // Busy only with background work after the reply finished.
        assert_eq!(activity("busy"), Some(ExternalActivity::Idle));
        assert_eq!(activity("idle"), Some(ExternalActivity::Idle));
        for flag in ["is_meta", "isMeta", "isCompactSummary"] {
            let summary = line(
                serde_json::json!({"type":"user", flag:true, "message":{"content":"Native harness context"}}),
            );
            std::fs::write(&transcript, format!("{prompt}{reply}{summary}")).unwrap();
            assert_eq!(activity("busy"), Some(ExternalActivity::Idle), "{flag}");
        }
        assert_eq!(
            activity("future-state"),
            None,
            "unknown is not a completed reply"
        );
        // The reply is the main conversation's, not a subagent's.
        std::fs::write(&transcript, "").unwrap();
        let mut log = PromptLog::default();
        assert!(log.read(&transcript));
        std::fs::write(&transcript, format!("{prompt}{reply}{side}")).unwrap();
        assert!(log.read(&transcript));
        assert_eq!(log.reply.as_deref(), Some("All done."));
    }
}
