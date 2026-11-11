//! Wake-ups for Codex and Claude Project chats written by another app. Watch
//! only files for chats a phone is looking at; Codex supplies its rollout path
//! through thread/read. The native Codex filesystem watcher can defer writes
//! until the writer closes, so it cannot wake a still-streaming chat on macOS.
//! Claude process records also wake the chat when a reply starts or its process
//! opens/closes. Events only invalidate history: native APIs still supply the
//! messages and status, and the phone keeps polling as a fallback.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::AppState;

/// How long a phone's look at a chat keeps it watched. The open chat's
/// activity check renews it every 3-5 s.
const INTEREST: Duration = Duration::from_secs(60);
const MAX_WATCHED: usize = 32;
/// Writes that arrive together (a reply's items) become one wake-up.
const SETTLE: Duration = Duration::from_millis(100);
/// Writes that never pause for SETTLE (a streaming reply) are flushed at most
/// this long after the first one.
#[cfg(target_os = "macos")]
const MAX_COLLECT: Duration = Duration::from_millis(300);
/// At most one wake-up per chat in this interval while another app streams.
const MIN_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone)]
struct Watched {
    claude_session: Option<String>,
    path: PathBuf,
    until: Instant,
}

#[derive(Default)]
struct Shared {
    /// By Wonder conversation.
    watched: HashMap<String, Watched>,
    started: bool,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(Default::default);

/// A phone is looking at this Claude Project chat: watch its transcript for a
/// while. Starts the watcher on first use.
pub(crate) fn interest_claude(state: &AppState, conversation: &str, session: &str, cwd: &str) {
    let Some(path) = crate::desktop_activity::claude_transcript(cwd, session) else {
        return;
    };
    register(state, conversation, path, Some(session.to_owned()));
}

/// The foreground Codex activity read already supplies the native rollout path.
pub(crate) fn interest_codex(
    state: &AppState,
    conversation: &str,
    thread: &str,
    read: &serde_json::Value,
) {
    if let Some(path) = codex_path(thread, read) {
        register(state, conversation, path, None);
    }
}

fn codex_path(thread: &str, read: &serde_json::Value) -> Option<PathBuf> {
    if read.pointer("/thread/id")?.as_str()? != thread {
        return None;
    }
    let path = PathBuf::from(read.pointer("/thread/path")?.as_str()?);
    path.is_absolute().then_some(path)
}

fn register(state: &AppState, conversation: &str, path: PathBuf, claude_session: Option<String>) {
    if cfg!(test) {
        return;
    }
    let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    if shared.watched.len() >= MAX_WATCHED && !shared.watched.contains_key(conversation) {
        let oldest = shared
            .watched
            .iter()
            .min_by_key(|(_, watched)| watched.until)
            .map(|(id, _)| id.clone());
        if let Some(oldest) = oldest {
            shared.watched.remove(&oldest);
        }
    }
    shared.watched.insert(
        conversation.to_owned(),
        Watched {
            claude_session,
            path,
            until: Instant::now() + INTEREST,
        },
    );
    if !shared.started {
        shared.started = true;
        start(state.clone());
    }
}

fn start(state: AppState) {
    let (wakes, mut received) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    let spawned = std::thread::Builder::new()
        .name("native-transcript-watch".into())
        .spawn(move || watch(wakes));
    if spawned.is_err() {
        SHARED.lock().unwrap_or_else(|e| e.into_inner()).started = false;
        return;
    }
    tokio::spawn(async move {
        let mut last: HashMap<String, Instant> = HashMap::new();
        while let Some(batch) = received.recv().await {
            let mut pending: HashSet<String> = batch.into_iter().collect();
            // A chat woken just now waits out the interval, collecting more.
            let soonest = pending
                .iter()
                .filter_map(|id| last.get(id))
                .map(|at| MIN_INTERVAL.saturating_sub(at.elapsed()))
                .max()
                .unwrap_or_default();
            if !soonest.is_zero() {
                tokio::time::sleep(soonest).await;
            }
            while let Ok(more) = received.try_recv() {
                pending.extend(more);
            }
            for conversation in pending {
                // Wonder's own reply writes the same file; its events already
                // reach the phone. A message sent into T3 Code is not one:
                // T3 Code's reply may be written before that send is answered.
                if crate::t3_code::stream::streams(&conversation) {
                    continue;
                }
                if state
                    .store
                    .conversation_has_own_active_turn(&conversation)
                    .await
                    .unwrap_or(true)
                {
                    continue;
                }
                last.insert(conversation.clone(), Instant::now());
                wake(&state, &conversation).await;
            }
            last.retain(|_, at| at.elapsed() < Duration::from_secs(60));
        }
    });
}

/// Tells the phones the chat's native history changed, so the open chat reads
/// it again. The transcript watcher sends it when another app writes the
/// session; a send into T3 Code sends it when T3 Code reports the run ended
/// (`thread_changes::t3_sent`), without waiting for a file watch to exist.
pub(crate) async fn wake(state: &AppState, conversation: &str) {
    let _ = crate::publish_event_with_context(
        state,
        wonder_api::WonderEvent::Activity {
            category: "native_history".into(),
            state: "changed".into(),
            detail: None,
        },
        crate::EventContext {
            conversation_id: Some(conversation.to_owned()),
            ..crate::EventContext::default()
        },
    )
    .await;
}

/// The chats still watched, dropping expired ones.
fn wanted() -> HashMap<String, Watched> {
    let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    shared.watched.retain(|_, watched| watched.until > now);
    shared.watched.clone()
}

#[cfg(target_os = "macos")]
fn watch(wakes: tokio::sync::mpsc::UnboundedSender<Vec<String>>) {
    use std::os::unix::ffi::OsStrExt;
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        SHARED.lock().unwrap_or_else(|e| e.into_inner()).started = false;
        return;
    }
    let open = |path: &std::path::Path| -> Option<i32> {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let change = libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE
                | libc::NOTE_EXTEND
                | libc::NOTE_DELETE
                | libc::NOTE_RENAME
                | libc::NOTE_ATTRIB,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let added =
            unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if added < 0 {
            unsafe { libc::close(fd) };
            return None;
        }
        Some(fd)
    };
    // Process records: a reply starting or a process opening or closing the
    // chat in another app. Records are added and removed as files.
    let records_dir = crate::desktop_activity::claude_sessions_dir();
    let mut records = records_dir.as_deref().and_then(open);
    let mut live = crate::desktop_activity::claude_live_sessions_now();
    // Transcript path -> descriptor.
    let mut files: HashMap<PathBuf, i32> = HashMap::new();
    let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
    loop {
        let watched = wanted();
        let paths: HashSet<&PathBuf> = watched.values().map(|w| &w.path).collect();
        files.retain(|path, fd| {
            let keep = paths.contains(path);
            if !keep {
                unsafe { libc::close(*fd) };
            }
            keep
        });
        for path in paths {
            if !files.contains_key(path) {
                // A new chat's transcript appears with its first reply.
                if let Some(fd) = open(path) {
                    files.insert(path.clone(), fd);
                }
            }
        }
        if records.is_none() {
            records = records_dir.as_deref().and_then(open);
        }
        let wait = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let mut count = unsafe {
            libc::kevent(
                queue,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                &wait,
            )
        };
        if count <= 0 {
            continue;
        }
        let mut fired: HashSet<i32> = HashSet::new();
        let mut gone: HashSet<i32> = HashSet::new();
        let first = Instant::now();
        loop {
            for event in &events[..count as usize] {
                let fd = event.ident as i32;
                fired.insert(fd);
                if event.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                    gone.insert(fd);
                }
            }
            let remaining = MAX_COLLECT.saturating_sub(first.elapsed()).min(SETTLE);
            if remaining.is_zero() {
                break;
            }
            let settle = libc::timespec {
                tv_sec: 0,
                tv_nsec: remaining.as_nanos() as i64,
            };
            count = unsafe {
                libc::kevent(
                    queue,
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    &settle,
                )
            };
            if count <= 0 {
                break;
            }
        }
        let mut wake: Vec<String> = watched
            .iter()
            .filter(|(_, w)| files.get(&w.path).is_some_and(|fd| fired.contains(fd)))
            .map(|(id, _)| id.clone())
            .collect();
        if records.is_some_and(|fd| fired.contains(&fd)) {
            let now = crate::desktop_activity::claude_live_sessions_now();
            for (id, w) in &watched {
                if w.claude_session
                    .as_ref()
                    .is_some_and(|session| live.get(session) != now.get(session))
                    && !wake.contains(id)
                {
                    wake.push(id.clone());
                }
            }
            live = now;
        }
        // A replaced or removed file is opened again on the next pass.
        files.retain(|_, fd| {
            let keep = !gone.contains(fd);
            if !keep {
                unsafe { libc::close(*fd) };
            }
            keep
        });
        if let Some(fd) = records.filter(|fd| gone.contains(fd)) {
            unsafe { libc::close(fd) };
            records = None;
        }
        if !wake.is_empty() && wakes.send(wake).is_err() {
            return;
        }
    }
}

/// Experimental headless (Linux, Windows) host: no kqueue, so poll the watched
/// transcripts' size and modification time once a second. The phone keeps its
/// own polling fallback, so a missed change only delays the wake-up.
#[cfg(not(target_os = "macos"))]
fn watch(wakes: tokio::sync::mpsc::UnboundedSender<Vec<String>>) {
    let stamp = |path: &std::path::Path| {
        std::fs::metadata(path).ok().map(|m| {
            let stamp = crate::platform::file_stamp(&m);
            (stamp.inode, m.len(), stamp.modified.0, stamp.modified.1)
        })
    };
    let mut seen: HashMap<PathBuf, Option<(u64, u64, i64, i64)>> = HashMap::new();
    let mut live = crate::desktop_activity::claude_live_sessions_now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let watched = wanted();
        seen.retain(|path, _| watched.values().any(|w| &w.path == path));
        let now = crate::desktop_activity::claude_live_sessions_now();
        let mut wake = Vec::new();
        for (id, w) in &watched {
            let current = stamp(&w.path);
            let changed = match seen.insert(w.path.clone(), current) {
                Some(previous) => previous != current,
                None => false,
            };
            if changed
                || w.claude_session
                    .as_ref()
                    .is_some_and(|session| live.get(session) != now.get(session))
            {
                wake.push(id.clone());
            }
        }
        live = now;
        if !wake.is_empty() {
            // Writes that land together become one wake-up.
            std::thread::sleep(SETTLE);
            if wakes.send(wake).is_err() {
                return;
            }
        }
    }
}

// Unix fixtures (absolute /paths and POSIX shell lines); Windows paths are not covered yet.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn codex_watch_uses_only_the_requested_threads_native_absolute_path() {
        use serde_json::json;
        let path = "/native/session.jsonl";
        assert_eq!(
            codex_path("chat", &json!({"thread":{"id":"chat", "path":path}})),
            Some(PathBuf::from(path))
        );
        for read in [
            json!({"thread":{"id":"other", "path":path}}),
            json!({"thread":{"path":path}}),
            json!({"thread":{"id":"chat", "path":"relative/session.jsonl"}}),
            json!({"thread":{"id":"chat", "path":null}}),
            json!({"thread":{"id":"chat"}}),
        ] {
            assert_eq!(codex_path("chat", &read), None, "{read}");
        }
    }

    /// Generous under load (a landing runs this beside other suites): the
    /// wake-ups below are checked by what the writer was doing, not by how fast
    /// a loaded machine schedules the watcher.
    #[cfg(target_os = "macos")]
    const GUARD: Duration = Duration::from_secs(30);

    /// Appends until the watcher wakes `chat`, ignoring a late wake-up for any
    /// other chat. Each append follows the last by about 20 ms, under SETTLE.
    #[cfg(target_os = "macos")]
    fn wake_while_writing(
        received: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<String>>,
        chat: &str,
        mut append: impl FnMut(),
    ) -> Vec<String> {
        let started = Instant::now();
        loop {
            append();
            std::thread::sleep(Duration::from_millis(20));
            while let Ok(batch) = received.try_recv() {
                if batch.iter().any(|id| id == chat) {
                    return batch;
                }
            }
            assert!(started.elapsed() < GUARD, "{chat} not woken in {GUARD:?}");
        }
    }

    /// Drops every wake-up for what is already written: one append to the
    /// marker transcript, which the watcher reports in a batch after (or with)
    /// every earlier write's. Returns the batches dropped, marker batch last.
    #[cfg(target_os = "macos")]
    fn drain(
        received: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<String>>,
        marker: &mut std::fs::File,
    ) -> Vec<Vec<String>> {
        use std::io::Write;
        writeln!(marker, "{{}}").unwrap();
        let started = Instant::now();
        let mut dropped = Vec::new();
        loop {
            match received.try_recv() {
                Ok(batch) => {
                    let done = batch.iter().any(|id| id == "marker");
                    dropped.push(batch);
                    if done {
                        return dropped;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
            assert!(started.elapsed() < GUARD, "marker not woken in {GUARD:?}");
        }
    }

    // Another app appends to a chat's transcript while a phone has it open:
    // the chat is woken, once for writes that land together, and a streaming
    // reply wakes it while the writer is still going. On macOS the watcher has
    // no polling fallback, so every wake-up here is a kqueue event.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_write_to_a_watched_transcript_wakes_its_chat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        // The marker separates the steps below. It is watched before the
        // watcher starts, so the pass that arms "watched-chat" arms it too, and
        // it is written once per step: each of its batches answers one step.
        let marker_path = dir.path().join("marker.jsonl");
        std::fs::write(&marker_path, "{}\n").unwrap();
        for (id, path) in [("watched-chat", &path), ("marker", &marker_path)] {
            SHARED.lock().unwrap().watched.insert(
                id.into(),
                Watched {
                    claude_session: None,
                    path: path.clone(),
                    until: Instant::now() + Duration::from_secs(60),
                },
            );
        }
        let mut marker = std::fs::OpenOptions::new()
            .append(true)
            .open(&marker_path)
            .unwrap();
        let (wakes, mut received) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || watch(wakes));
        use std::io::Write;
        // Codex holds its rollout writer open across updates. Closing after
        // each append would miss the regression in the native fs/watch API.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let mut append = |line: &str| writeln!(file, "{line}").unwrap();
        // The watcher opens the transcript on its first pass, which a loaded
        // machine can delay past any fixed sleep: write until it is armed.
        assert_eq!(
            wake_while_writing(&mut received, "watched-chat", || append("{}")),
            vec!["watched-chat".to_owned()]
        );
        drain(&mut received, &mut marker);
        let written = Instant::now();
        append("{\"type\":\"assistant\"}");
        append("{\"type\":\"assistant\",\"more\":true}");
        let batch = loop {
            if let Ok(batch) = received.try_recv() {
                break batch;
            }
            assert!(written.elapsed() < GUARD, "no wake-up");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(batch, vec!["watched-chat".to_owned()]);
        let later = drain(&mut received, &mut marker);
        assert!(
            !later.iter().flatten().any(|id| id == "watched-chat"),
            "writes that land together wake once: {later:?}"
        );
        // Writes that never pause for SETTLE (a streaming reply) still wake
        // the chat while the writer keeps going, instead of waiting for it to
        // stop: a watcher that collects until writes pause never returns here.
        assert_eq!(
            wake_while_writing(&mut received, "watched-chat", || {
                append("{\"type\":\"assistant\",\"streaming\":true}")
            }),
            vec!["watched-chat".to_owned()]
        );
        drain(&mut received, &mut marker);
        // A Claude transcript uses the same write path and still wakes its
        // own chat; a different source must not receive the notification.
        let claude_path = dir.path().join("claude.jsonl");
        std::fs::write(&claude_path, "{}").unwrap();
        SHARED.lock().unwrap().watched.insert(
            "claude-chat".into(),
            Watched {
                claude_session: Some("session".into()),
                path: claude_path.clone(),
                until: Instant::now() + Duration::from_secs(60),
            },
        );
        let mut claude = std::fs::OpenOptions::new()
            .append(true)
            .open(claude_path)
            .unwrap();
        assert_eq!(
            wake_while_writing(&mut received, "claude-chat", || {
                writeln!(claude, "{{}}").unwrap()
            }),
            vec!["claude-chat".to_owned()]
        );
        SHARED.lock().unwrap().watched.clear();
    }
}
