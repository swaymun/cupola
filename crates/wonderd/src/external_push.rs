//! Notifications for followed project threads whose turn runs outside Wonder:
//! Claude Code (desktop app or terminal) and the Codex app on this Mac. Wonder
//! receives no runtime events for that work, so this watcher reads the same
//! local records that show those threads as working. Each change it sees is
//! also published as a `native_status` event, so a phone's sidebar follows it
//! without waiting for its periodic refresh. A Codex turn running in
//! ChatGPT's desktop app is also followed there for its approvals
//! (`chatgpt_approvals`).
use super::*;
use crate::desktop_activity::ExternalActivity;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const POLL: Duration = Duration::from_secs(4);
/// How often unknown Codex rollout paths are looked up again.
const CODEX_INDEX_REFRESH: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Notify {
    Completed,
    /// Waiting on the owner on the Mac (a permission prompt or question).
    Attention,
    /// The turn ended without finishing: it failed, or its process exited.
    Failed,
    /// The process exited and its transcript cannot tell how the reply ended.
    Ended,
}

/// A Claude session's transition between two polls. The first sighting only
/// records the state, so a daemon restart does not replay old prompts.
/// `exited` is whether the process that held the session is known to have
/// exited; an unreadable record alone is never an exit. `finished` is what its
/// transcript says about the reply then (`None`: it cannot tell).
pub(crate) fn claude_transition(
    previous: Option<ExternalActivity>,
    current: Option<ExternalActivity>,
    exited: bool,
    finished: Option<bool>,
) -> Option<Notify> {
    match (previous?, current) {
        (ExternalActivity::Waiting, Some(ExternalActivity::Waiting)) => None,
        (_, Some(ExternalActivity::Waiting)) => Some(Notify::Attention),
        (ExternalActivity::Working | ExternalActivity::Waiting, Some(ExternalActivity::Idle)) => {
            Some(Notify::Completed)
        }
        (ExternalActivity::Working | ExternalActivity::Waiting, None) if exited => {
            Some(match finished {
                Some(true) => Notify::Completed,
                Some(false) => Notify::Failed,
                None => Notify::Ended,
            })
        }
        _ => None,
    }
}

/// A Codex thread's transition: the turn that was open has ended. Completed
/// notifies with its reply, failed asks for attention, and a turn the owner
/// interrupted on the Mac notifies nothing, as for Wonder's own Stop.
pub(crate) fn codex_transition(
    previous_open: Option<&str>,
    activity: &crate::codex_activity::Activity,
) -> Option<(String, Notify, String)> {
    let open = previous_open?;
    if activity.turn != open {
        return None;
    }
    let notify = match activity.status {
        "completed" => Notify::Completed,
        "failed" => Notify::Failed,
        _ => return None,
    };
    Some((
        activity.turn.clone(),
        notify,
        activity.reply.clone().unwrap_or_default(),
    ))
}

/// Idle polls in a row before a reply counts as final: a wakeup queued as
/// the reply ended starts the next turn within one poll.
const SETTLE_POLLS: u32 = 2;
/// Background work the transcript does not account for (started before the
/// watch, or in a form it does not know) may still wake the session: it
/// must stay quiet this long before its newest reply counts as final.
const QUIET_POLLS: u32 = (120 / POLL.as_secs()) as u32;

/// A prompt the owner sent in Claude on the Mac, and the reply it ended
/// with once nothing more follows it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Answered {
    pub(crate) prompt: String,
    pub(crate) reply: Option<String>,
}

/// One followed Claude session's prompts, for completion notifications.
/// Only a prompt the owner sent notifies, once, after the last turn that
/// follows it: turns Claude Code starts by itself (a background agent,
/// command or monitor reporting back) never notify on their own, and while
/// a task the conversation started runs, or its report waits to be taken
/// in, the prompt waits however long that takes. A new prompt starts clean:
/// tasks and reports earlier prompts left outstanding never hold it.
///
/// The watch begins clean at the transcript's end and follows prompts sent
/// after that. A prompt already under way when it began (the daemon started,
/// or the thread was attached, mid-reply) gets no completion notification:
/// the history before it cannot say which tasks still run or which reports
/// are still to come. Its questions, approvals and failures still notify
/// through `claude_transition`.
#[derive(Debug, Default)]
pub(crate) struct PromptWatch {
    path: Option<PathBuf>,
    log: crate::desktop_activity::PromptLog,
    /// The newest prompt already notified, or cut short by an exit.
    done: Option<String>,
    /// The prompt waiting for its final reply.
    pending: Option<Pending>,
}

/// How a prompt's final reply is settling. A new prompt starts afresh.
#[derive(Debug)]
struct Pending {
    prompt: String,
    /// Polls in a row with its reply ended, the session idle and nothing
    /// more to come: no background task the conversation started still
    /// running, and no report queued that a turn has yet to take in.
    quiet: u32,
}

impl PromptWatch {
    /// Another process now holds the session. Reports the old one queued
    /// went with it and will not wake the session.
    pub(crate) fn restarted(&mut self) {
        self.log.queued.clear();
    }

    /// One poll of the session: its activity (`None`: no live record), any
    /// background work, and whether its process is known to have exited.
    pub(crate) fn poll(
        &mut self,
        path: &std::path::Path,
        state: Option<ExternalActivity>,
        background: bool,
        exited: bool,
    ) -> Option<Answered> {
        if self.path.as_deref() != Some(path) {
            *self = Self::default();
            if !self.log.read(path) {
                return None;
            }
            self.path = Some(path.to_owned());
            return None;
        }
        if !self.log.read(path) {
            return None;
        }
        if exited {
            // Reports its process queued went with it.
            self.log.queued.clear();
        }
        if state == Some(ExternalActivity::Idle) && !background && !self.log.running {
            // Nothing runs in the background: tasks that never reported back
            // were stopped some other way. Their queued reports still wake
            // the session.
            self.log.tasks.clear();
        }
        let Some(prompt) = self
            .log
            .prompt
            .clone()
            .filter(|_| self.log.prompt != self.done)
        else {
            self.pending = None;
            return None;
        };
        let pending = match &mut self.pending {
            Some(pending) if pending.prompt == prompt => pending,
            slot => slot.insert(Pending {
                prompt: prompt.clone(),
                quiet: 0,
            }),
        };
        let ended = self.log.replied && !self.log.running;
        if exited && !ended {
            // Its reply was cut short: the exit notifies as a failure.
            self.done = Some(prompt);
            self.pending = None;
            return None;
        }
        let idle = state == Some(ExternalActivity::Idle) && ended;
        // A queued report will wake the session; or the record shows
        // background work, and the transcript names tasks the conversation
        // started that have not reported back.
        let more = !self.log.queued.is_empty() || (background && !self.log.tasks.is_empty());
        pending.quiet = if idle && !more { pending.quiet + 1 } else { 0 };
        let settle = if background {
            QUIET_POLLS
        } else {
            SETTLE_POLLS
        };
        if (exited && ended) || pending.quiet >= settle {
            self.done = Some(prompt.clone());
            self.pending = None;
            return Some(Answered {
                prompt,
                reply: self.log.reply.clone(),
            });
        }
        None
    }
}

/// What the watcher last saw of a Claude session.
#[derive(Clone, Debug)]
struct ClaudeSeen {
    /// `None` while its record is missing.
    state: Option<ExternalActivity>,
    process: crate::desktop_activity::ProcessIdentity,
    cwd: Option<String>,
}

/// A Claude session's process this poll, against the one last seen.
#[derive(Debug)]
struct Sighting {
    /// Known to have exited (its record is gone too).
    exited: bool,
    /// Another process holds the session now.
    restarted: bool,
    /// What to remember for the next poll.
    seen: Option<ClaudeSeen>,
}

impl Sighting {
    /// A missing record keeps the process it last named until that process
    /// is known to have exited, or the record names another: either ends the
    /// reports it queued, which a later prompt must not wait for.
    fn of(
        previous: Option<&ClaudeSeen>,
        activity: Option<&crate::desktop_activity::ClaudeSessionActivity>,
    ) -> Self {
        let Some(activity) = activity else {
            let exited = previous.is_some_and(|seen| seen.process.exited());
            return Self {
                exited,
                restarted: false,
                seen: previous.filter(|_| !exited).map(|seen| ClaudeSeen {
                    state: None,
                    ..seen.clone()
                }),
            };
        };
        Self {
            exited: false,
            restarted: previous.is_some_and(|seen| seen.process != activity.process),
            seen: Some(ClaudeSeen {
                state: Some(activity.state),
                process: activity.process,
                cwd: activity.cwd.clone(),
            }),
        }
    }
}

#[derive(Default)]
struct Watcher {
    claude: HashMap<String, ClaudeSeen>,
    prompts: HashMap<String, PromptWatch>,
    codex: HashMap<String, Option<String>>,
    codex_paths: HashMap<String, PathBuf>,
    codex_indexed: Option<std::time::Instant>,
    /// After the first poll: earlier states are unknown, so the first poll
    /// publishes nothing.
    primed: bool,
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Before any watch starts: what a restart left open is read back.
        crate::ingestion::recheck_desktop_approvals(&state).await;
        let mut watcher = Watcher::default();
        let mut interval = tokio::time::interval(POLL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Ok(followed) = state.store.followed_native_threads().await else {
                continue;
            };
            poll(&state, &mut watcher, followed).await;
        }
    })
}

/// Tells phones that a followed thread's status on the Mac changed, so its
/// sidebar row reads it again. Wonder's own turns publish their own events.
pub(crate) async fn publish_status(state: &AppState, conversation: &str) {
    let _ = crate::publish_event_with_context(
        state,
        wonder_api::WonderEvent::Activity {
            category: "native_status".into(),
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

/// What one poll saw, for the Live Activity hub: each followed thread with
/// work and how any thread that stopped working ended.
#[derive(Default)]
struct Reading {
    desktop: HashMap<String, crate::live_activity::Desktop>,
    ended: Vec<(String, crate::live_activity::Phase)>,
}

async fn poll(
    state: &AppState,
    watcher: &mut Watcher,
    followed: Vec<(String, AgentFamily, String)>,
) {
    let mut reading = Reading::default();
    poll_followed(state, watcher, followed, &mut reading).await;
    state
        .live_activity
        .desktop_reading(reading.desktop, reading.ended);
}

async fn poll_followed(
    state: &AppState,
    watcher: &mut Watcher,
    followed: Vec<(String, AgentFamily, String)>,
    reading: &mut Reading,
) {
    use crate::live_activity::{Desktop, Phase};
    let primed = std::mem::replace(&mut watcher.primed, true);
    // Turns running in another app, for mac_dialogs.
    let mut running = std::collections::HashSet::new();
    let push = state.store.push_enabled().await.unwrap_or(false);
    let claude: Vec<_> = followed
        .iter()
        .filter(|(_, family, _)| *family == AgentFamily::Claude)
        .cloned()
        .collect();
    let codex: Vec<_> = followed
        .iter()
        .filter(|(_, family, _)| *family == AgentFamily::Codex)
        .cloned()
        .collect();
    if !claude.is_empty() {
        let followed_claude: std::collections::HashSet<String> = claude
            .iter()
            .map(|(_, _, session)| session.clone())
            .collect();
        // Only followed sessions' transcripts are read.
        let sessions = {
            let followed = followed_claude.clone();
            tokio::task::spawn_blocking(move || {
                crate::desktop_activity::claude_session_activity(&followed)
            })
            .await
            .unwrap_or_default()
        };
        let mut seen = HashMap::new();
        for (conversation, _, session) in claude {
            let activity = sessions.get(&session);
            let current = activity.map(|activity| activity.state);
            let previous = watcher.claude.get(&session).cloned();
            // An unreadable record is not an exit: only a process known to
            // be gone ends the reply it was writing.
            let Sighting {
                exited,
                restarted,
                seen: kept,
            } = Sighting::of(previous.as_ref(), activity);
            if let Some(kept) = kept {
                seen.insert(session.clone(), kept);
            }
            let before = previous.as_ref().and_then(|seen| seen.state);
            if current == Some(ExternalActivity::Working) {
                running.insert(conversation.clone());
            }
            if primed && before != current {
                publish_status(state, &conversation).await;
            }
            match current {
                Some(ExternalActivity::Working) => {
                    reading
                        .desktop
                        .insert(conversation.clone(), Desktop::Working);
                }
                Some(ExternalActivity::Waiting) => {
                    reading
                        .desktop
                        .insert(conversation.clone(), Desktop::Waiting);
                }
                _ => {}
            }
            let cwd = activity
                .and_then(|activity| activity.cwd.clone())
                .or_else(|| previous.and_then(|seen| seen.cwd));
            let transcript = cwd
                .as_deref()
                .and_then(|cwd| crate::desktop_activity::claude_transcript(cwd, &session));
            let finished = match (exited, transcript.clone()) {
                (true, Some(path)) => tokio::task::spawn_blocking(move || {
                    crate::desktop_activity::main_turn_running(&path).map(|running| !running)
                })
                .await
                .ok()
                .flatten(),
                _ => None,
            };
            let path = transcript.or_else(|| {
                watcher
                    .prompts
                    .get(&session)
                    .and_then(|watch| watch.path.clone())
            });
            if let Some(path) = path {
                let mut watch = watcher.prompts.remove(&session).unwrap_or_default();
                if restarted {
                    watch.restarted();
                }
                let background = activity.is_some_and(|activity| activity.background);
                let polled = tokio::task::spawn_blocking(move || {
                    let answered = watch.poll(&path, current, background, exited);
                    (watch, answered)
                })
                .await;
                if let Ok((watch, answered)) = polled {
                    watcher.prompts.insert(session.clone(), watch);
                    if let (true, Some(answered)) = (push, answered) {
                        notify_answered(state, &conversation, &session, answered).await;
                    }
                }
            }
            let Some(notify) = claude_transition(before, current, exited, finished) else {
                continue;
            };
            match notify {
                Notify::Completed => reading.ended.push((conversation.clone(), Phase::Done)),
                Notify::Failed => reading.ended.push((conversation.clone(), Phase::Failed)),
                Notify::Ended => reading.ended.push((conversation.clone(), Phase::Stopped)),
                Notify::Attention => {}
            }
            // A finished reply notifies through `PromptWatch`, once per prompt.
            if push && notify != Notify::Completed {
                let marker = activity
                    .and_then(|activity| activity.status_at)
                    .unwrap_or_else(|| now_ms() as i64);
                notify_claude(state, &conversation, &session, notify, marker).await;
            }
        }
        watcher.claude = seen;
        watcher
            .prompts
            .retain(|session, _| followed_claude.contains(session));
    } else {
        watcher.claude.clear();
        watcher.prompts.clear();
    }
    if codex.is_empty() {
        watcher.codex.clear();
        crate::mac_dialogs::set_external_running(running);
        return;
    }
    let stale = watcher
        .codex_indexed
        .is_none_or(|at| at.elapsed() >= CODEX_INDEX_REFRESH);
    let missing = codex.iter().any(|(_, _, thread)| {
        !watcher.codex_paths.contains_key(thread)
            && crate::desktop_activity::codex_rollout(thread).is_none()
    });
    if stale && missing {
        let wanted: Vec<String> = codex.iter().map(|(_, _, thread)| thread.clone()).collect();
        if let Ok(found) = tokio::task::spawn_blocking(move || index_codex_rollouts(&wanted)).await
        {
            // List rows read the same rollouts.
            for (thread, path) in &found {
                if let Some(path) = path.to_str() {
                    crate::desktop_activity::remember_codex_rollout(thread, path);
                }
            }
            watcher.codex_paths.extend(found);
        }
        watcher.codex_indexed = Some(std::time::Instant::now());
    }
    let mut seen = HashMap::new();
    for (conversation, _, thread) in codex {
        let Some(path) = crate::desktop_activity::codex_rollout(&thread)
            .or_else(|| watcher.codex_paths.get(&thread).cloned())
        else {
            continue;
        };
        let activity = crate::codex_activity::read(&state.codex_home, &thread, &path).await;
        let Some(activity) = activity else {
            // An incomplete observation cannot terminate a previously open
            // turn or lose its identity before a later confirmed completion.
            if let Some(previous) = watcher.codex.get(&thread) {
                if previous.is_some() {
                    reading
                        .desktop
                        .insert(conversation.clone(), Desktop::Working);
                }
                seen.insert(thread, previous.clone());
            }
            continue;
        };
        let open = activity.working().then(|| activity.turn.clone());
        if open.is_some() {
            // A turn ChatGPT runs asks for approvals there; follow it so the
            // phone can answer them too.
            crate::chatgpt_approvals::watch(state, &conversation, &thread).await;
            running.insert(conversation.clone());
            reading
                .desktop
                .insert(conversation.clone(), Desktop::Working);
        }
        if let Some(previous) = watcher.codex.get(&thread) {
            if primed && *previous != open {
                publish_status(state, &conversation).await;
            }
            if primed && previous.is_some() && open.is_none() {
                let phase = match activity.status {
                    "failed" => Phase::Failed,
                    "interrupted" => Phase::Stopped,
                    _ => Phase::Done,
                };
                reading.ended.push((conversation.clone(), phase));
            }
            if let Some((turn, notify, reply)) = codex_transition(previous.as_deref(), &activity) {
                if push {
                    notify_codex(state, &conversation, &thread, &turn, notify, &reply).await;
                }
            }
        } else if primed && open.is_some() {
            publish_status(state, &conversation).await;
        }
        seen.insert(thread, open);
    }
    watcher.codex = seen;
    crate::mac_dialogs::set_external_running(running);
}

async fn wonder_owns_turn(state: &AppState, conversation: &str) -> bool {
    // Wonder's own turns notify through their runtime events.
    state
        .store
        .conversation_has_active_turn(conversation)
        .await
        .unwrap_or(true)
}

async fn notify_claude(
    state: &AppState,
    conversation: &str,
    session: &str,
    notify: Notify,
    marker: i64,
) {
    if wonder_owns_turn(state, conversation).await {
        return;
    }
    let (kind, outcome) = notify.push();
    let _ = state
        .store
        .enqueue_external_push(
            &format!("claude:{session}:{marker}:{outcome}"),
            conversation,
            kind,
            None,
        )
        .await;
}

/// The owner's prompt on the Mac has its final reply. Without closing text
/// the notification says only that Claude finished.
async fn notify_answered(state: &AppState, conversation: &str, session: &str, answered: Answered) {
    if wonder_owns_turn(state, conversation).await {
        return;
    }
    // A prompt Cupola sent carries its turn's ID and notified through its
    // own events.
    if state
        .store
        .message_for_codex_turn(&answered.prompt)
        .await
        .map_or(true, |message| message.is_some())
    {
        return;
    }
    let (kind, outcome) = Notify::Completed.push();
    let _ = state
        .store
        .enqueue_external_push(
            &format!("claude:{session}:{}:{outcome}", answered.prompt),
            conversation,
            kind,
            answered.reply.as_deref(),
        )
        .await;
}

impl Notify {
    /// The push kind, and the outcome its source key ends with (which the
    /// notification's wording follows; see `Store::push_preview`).
    fn push(self) -> (&'static str, &'static str) {
        match self {
            Self::Completed => ("completed", "completed"),
            Self::Attention => ("attention", "attention"),
            Self::Failed => ("attention", "failed"),
            Self::Ended => ("attention", "ended"),
        }
    }
}

async fn notify_codex(
    state: &AppState,
    conversation: &str,
    thread: &str,
    turn: &str,
    notify: Notify,
    reply: &str,
) {
    if wonder_owns_turn(state, conversation).await {
        return;
    }
    // A turn Wonder dispatched already notified through its own events.
    if state
        .store
        .message_for_codex_thread_and_turn(thread, turn)
        .await
        .map_or(true, |message| message.is_some())
    {
        return;
    }
    let (kind, outcome) = notify.push();
    let _ = state
        .store
        .enqueue_external_push(
            &format!("codex:{thread}:{turn}:{outcome}"),
            conversation,
            kind,
            Some(reply).filter(|reply| notify == Notify::Completed && !reply.trim().is_empty()),
        )
        .await;
}

fn codex_sessions_dir() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME") {
        Some(home) => Some(PathBuf::from(home).join("sessions")),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex/sessions")),
    }
}

/// Find rollout logs for the wanted threads by file name
/// (`sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`).
fn index_codex_rollouts(wanted: &[String]) -> HashMap<String, PathBuf> {
    let mut found = HashMap::new();
    let Some(root) = codex_sessions_dir() else {
        return found;
    };
    index_rollouts_in(&root, wanted, &mut found);
    found
}

pub(crate) fn index_rollouts_in(
    root: &std::path::Path,
    wanted: &[String],
    found: &mut HashMap<String, PathBuf>,
) {
    let wanted: std::collections::HashSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 200_000 {
                return;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < 3 {
                stack.push((path, depth + 1));
            } else if kind.is_file() {
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some(stem) = name
                    .strip_prefix("rollout-")
                    .and_then(|name| name.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                if stem.len() > 36 && wanted.contains(&stem[stem.len() - 36..]) {
                    found.insert(stem[stem.len() - 36..].to_owned(), path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ExternalActivity::*;

    #[test]
    fn claude_sessions_notify_once_per_prompt_and_finished_reply() {
        let seen = |previous, current| claude_transition(previous, current, false, None);
        // First sighting never notifies (restart safety).
        assert_eq!(seen(None, Some(Waiting)), None);
        assert_eq!(seen(None, Some(Idle)), None);
        assert_eq!(seen(Some(Working), Some(Waiting)), Some(Notify::Attention));
        assert_eq!(seen(Some(Idle), Some(Waiting)), Some(Notify::Attention));
        assert_eq!(seen(Some(Waiting), Some(Waiting)), None);
        assert_eq!(seen(Some(Waiting), Some(Working)), None);
        assert_eq!(seen(Some(Working), Some(Working)), None);
        assert_eq!(seen(Some(Working), Some(Idle)), Some(Notify::Completed));
        assert_eq!(seen(Some(Waiting), Some(Idle)), Some(Notify::Completed));
        assert_eq!(seen(Some(Idle), Some(Idle)), None);
        // A record that could not be read is not an exit.
        assert_eq!(seen(Some(Working), None), None);
        // The process quit mid-reply: its transcript says how the reply ended.
        let exited = |previous, finished| claude_transition(Some(previous), None, true, finished);
        assert_eq!(exited(Working, Some(false)), Some(Notify::Failed));
        assert_eq!(exited(Waiting, Some(false)), Some(Notify::Failed));
        assert_eq!(exited(Working, Some(true)), Some(Notify::Completed));
        assert_eq!(exited(Working, None), Some(Notify::Ended));
        assert_eq!(
            exited(Idle, None),
            None,
            "an idle session closing is not news"
        );
    }

    /// A Claude Code transcript as 2.1.29x writes it, one line per entry.
    struct Transcript {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Transcript {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            std::fs::write(&path, "").unwrap();
            Self { _dir: dir, path }
        }
        fn add(&self, entries: &[serde_json::Value]) {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap();
            for entry in entries {
                writeln!(file, "{entry}").unwrap();
            }
        }
    }

    fn prompt(uuid: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","uuid":uuid,"promptSource":"sdk","turnOrigin":"human",
            "origin":{"kind":"human"},"message":{"role":"user","content":text}})
    }
    /// What a background task reports when it ends (`status`), or as it
    /// goes on (`None`: a monitor's event, a command's progress).
    fn report(task: &str, status: Option<&str>) -> String {
        let status =
            status.map_or_else(String::new, |status| format!("<status>{status}</status>\n"));
        format!("<task-notification>\n<task-id>{task}</task-id>\n{status}<summary>Report</summary>\n</task-notification>")
    }
    /// Claude Code queues a report as soon as the task sends it.
    fn queued(text: &str) -> serde_json::Value {
        serde_json::json!({"type":"queue-operation","operation":"enqueue","content":text})
    }
    /// A queued report wakes an idle session for a new turn.
    fn wake_with(uuid: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","uuid":uuid,"promptSource":"system","turnOrigin":"task_notification",
            "origin":{"kind":"task-notification"},"message":{"role":"user","content":text}})
    }
    /// A background agent reporting back wakes the session for a new turn.
    fn wake(uuid: &str, task: &str) -> serde_json::Value {
        wake_with(uuid, &report(task, Some("completed")))
    }
    /// A monitor's event: the monitor goes on.
    fn event(uuid: &str, task: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","uuid":uuid,"promptSource":"system","turnOrigin":"task_notification",
            "origin":{"kind":"task-notification"},
            "message":{"role":"user","content":format!("<task-notification>\n<task-id>{task}</task-id>\n<summary>Monitor event</summary>\n</task-notification>")}})
    }
    /// The tool result that started background work, as Claude Code 2.1.30x
    /// records an agent, a command and a monitor run in the background.
    fn started(kind: &str, task: &str) -> serde_json::Value {
        let result = match kind {
            "agent" => serde_json::json!({"isAsync":true,"status":"async_launched","agentId":task}),
            "command" => serde_json::json!({"stdout":"","backgroundTaskId":task}),
            _ => {
                serde_json::json!({"taskId":task,"timeoutMs":1_800_000,"persistent":kind == "persistent"})
            }
        };
        serde_json::json!({"type":"user","toolUseResult":result,"message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"Started."}]}})
    }
    fn say(message: &str, text: &str, stop: &str) -> serde_json::Value {
        serde_json::json!({"type":"assistant","message":{"id":message,"role":"assistant",
            "content":[{"type":"text","text":text}],"stop_reason":stop}})
    }
    fn call(message: &str) -> serde_json::Value {
        serde_json::json!({"type":"assistant","message":{"id":message,"role":"assistant",
            "content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}],"stop_reason":"tool_use"}})
    }
    fn result() -> serde_json::Value {
        serde_json::json!({"type":"user","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}})
    }

    /// Polls `times` times in one state and returns what notified.
    fn polls(
        watch: &mut PromptWatch,
        transcript: &Transcript,
        state: ExternalActivity,
        background: bool,
        times: u32,
    ) -> Vec<Answered> {
        (0..times)
            .filter_map(|_| watch.poll(&transcript.path, Some(state), background, false))
            .collect()
    }

    fn answer(prompt: &str, reply: &str) -> Vec<Answered> {
        vec![Answered {
            prompt: prompt.into(),
            reply: Some(reply.into()),
        }]
    }

    // Owner report (Oct 21): a lead session woken by each background agent
    // notified after every wakeup (191 a day). One prompt notifies once,
    // after the last turn that follows it, with that turn's reply.
    #[test]
    fn a_prompt_notifies_once_after_the_wakeups_that_follow_it() {
        let transcript = Transcript::new();
        transcript.add(&[
            prompt("p0", "Earlier"),
            say("m0", "Earlier answer", "end_turn"),
        ]);
        let mut watch = PromptWatch::default();
        assert!(
            polls(&mut watch, &transcript, Idle, false, 3).is_empty(),
            "an old prompt is not news"
        );

        let wakeups = 4;
        transcript.add(&[prompt("p1", "Run the agents")]);
        assert!(polls(&mut watch, &transcript, Working, false, 1).is_empty());
        for n in 1..=wakeups {
            transcript.add(&[call("m1"), started("agent", &format!("a{n}"))]);
        }
        assert!(polls(&mut watch, &transcript, Working, false, 1).is_empty());
        transcript.add(&[say("m2", "Started four agents.", "end_turn")]);
        // Its agents run on and will wake it.
        assert!(polls(&mut watch, &transcript, Idle, true, 5).is_empty());
        let mut answered = Vec::new();
        for n in 1..=wakeups {
            transcript.add(&[wake(&format!("w{n}"), &format!("a{n}"))]);
            assert!(polls(&mut watch, &transcript, Working, true, 1).is_empty());
            transcript.add(&[say(
                &format!("r{n}"),
                &format!("Agent {n} finished."),
                "end_turn",
            )]);
            let last = n == wakeups;
            answered.extend(polls(&mut watch, &transcript, Idle, !last, 3));
            assert_eq!(answered.is_empty(), !last, "wakeup {n}");
        }
        assert_eq!(answered, answer("p1", "Agent 4 finished."));
        // Later wakeups the owner did not start stay quiet.
        transcript.add(&[wake("w9", "b9"), say("r9", "Monitor tick.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, false, 5).is_empty());
    }

    // Review (Oct 21): an agent that ran 3 minutes, past the 2 minute quiet
    // window, notified with "Started the agent." and never with its result.
    // Second review: once the agent ended, its report sat queued while the
    // session was idle, and the prompt closed before the report woke it
    // ("Started agent" notified, "Final audit" did not).
    #[test]
    fn a_prompt_waits_for_the_background_work_it_started() {
        let transcript = Transcript::new();
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, false, 1).is_empty());
        transcript.add(&[
            prompt("p1", "Audit the code"),
            call("m1"),
            started("agent", "a1"),
            say("m2", "Started agent.", "end_turn"),
        ]);
        let three_minutes = 180 / POLL.as_secs() as u32;
        assert!(three_minutes > QUIET_POLLS);
        assert!(polls(&mut watch, &transcript, Idle, true, three_minutes).is_empty());
        // The agent ends: the session shows no background work, and its
        // report waits in Claude Code's queue until a turn takes it in.
        let done = report("a1", Some("completed"));
        transcript.add(&[queued(&done)]);
        assert!(polls(&mut watch, &transcript, Idle, false, QUIET_POLLS + 1).is_empty());
        transcript.add(&[
            serde_json::json!({"type":"queue-operation","operation":"dequeue"}),
            wake_with("w1", &done),
        ]);
        assert!(polls(&mut watch, &transcript, Working, false, 2).is_empty());
        transcript.add(&[say("r1", "Final audit: two bugs.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 5),
            answer("p1", "Final audit: two bugs.")
        );

        // A report that arrives while a turn runs is taken into that turn.
        transcript.add(&[
            prompt("p2", "Check both"),
            call("m3"),
            started("agent", "a2"),
            call("m4"),
        ]);
        assert!(polls(&mut watch, &transcript, Working, true, 2).is_empty());
        let done = report("a2", Some("completed"));
        transcript.add(&[
            queued(&done),
            result(),
            serde_json::json!({"type":"attachment","attachment":{"type":"queued_command","prompt":done}}),
            serde_json::json!({"type":"queue-operation","operation":"remove","reason":"absorbed_mid_turn","content":done}),
            say("m5", "Both checked.", "end_turn"),
        ]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, SETTLE_POLLS),
            answer("p2", "Both checked.")
        );

        // A command's progress report that its final report replaces is
        // never taken in, and holds nothing.
        transcript.add(&[
            prompt("p3", "Build it"),
            call("m6"),
            started("command", "b3"),
            say("m7", "Building.", "end_turn"),
        ]);
        let stopped = report("b3", Some("stopped"));
        transcript.add(&[queued(&report("b3", None)), queued(&stopped)]);
        assert!(polls(&mut watch, &transcript, Idle, false, 3).is_empty());
        transcript.add(&[wake_with("w3", &stopped)]);
        assert!(polls(&mut watch, &transcript, Working, false, 1).is_empty());
        transcript.add(&[say("r3", "The build stopped.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, SETTLE_POLLS),
            answer("p3", "The build stopped.")
        );

        // A report queued by a process that has since restarted never comes.
        transcript.add(&[
            prompt("p4", "Check again"),
            call("m8"),
            started("agent", "a4"),
            say("m9", "Started agent.", "end_turn"),
            queued(&report("a4", Some("completed"))),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, false, QUIET_POLLS + 1).is_empty());
        watch.restarted();
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, SETTLE_POLLS),
            answer("p4", "Started agent.")
        );
        // Nor one queued by a process that exited: the owner's next prompt,
        // once the session is opened again, is not held for it.
        transcript.add(&[
            prompt("p5", "Once more"),
            call("m10"),
            started("agent", "a5"),
            say("m11", "Started agent.", "end_turn"),
            queued(&report("a5", Some("completed"))),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, false, 3).is_empty());
        assert_eq!(
            watch.poll(&transcript.path, None, false, true),
            Some(Answered {
                prompt: "p5".into(),
                reply: Some("Started agent.".into()),
            })
        );
        transcript.add(&[prompt("p6", "Back"), say("m12", "Hello.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, SETTLE_POLLS),
            answer("p6", "Hello.")
        );
    }

    // Second review (Oct 21): after 10 minutes of background work the reply
    // so far notified. Nothing notifies as finished while that work runs.
    #[test]
    fn background_work_holds_the_prompt_however_long_it_runs() {
        let transcript = Transcript::new();
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, false, 1).is_empty());
        transcript.add(&[
            prompt("p1", "Start the server"),
            call("m1"),
            started("command", "b1"),
            say("m2", "The server is up.", "end_turn"),
        ]);
        let half_an_hour = 1800 / POLL.as_secs() as u32;
        assert!(polls(&mut watch, &transcript, Idle, true, half_an_hour).is_empty());
        // Its final reply, after the command ends, notifies once.
        transcript.add(&[wake("w1", "b1")]);
        assert!(polls(&mut watch, &transcript, Working, false, 1).is_empty());
        transcript.add(&[say("r1", "The server stopped.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 5),
            answer("p1", "The server stopped.")
        );
        // A monitor's event wakes the session but does not end the monitor.
        transcript.add(&[
            prompt("p2", "Watch the log"),
            call("m3"),
            started("monitor", "m1"),
            say("m4", "Watching.", "end_turn"),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, true, half_an_hour).is_empty());
        transcript.add(&[event("e1", "m1"), say("r2", "Saw a line.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, true, half_an_hour).is_empty());
        transcript.add(&[wake("w2", "m1"), say("r3", "The log is quiet.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 5),
            answer("p2", "The log is quiet.")
        );
        // A monitor that never ends, or work the transcript does not name,
        // only has to stay quiet before the reply notifies.
        transcript.add(&[
            prompt("p4", "Status?"),
            call("m7"),
            started("persistent", "m3"),
            say("m8", "All green.", "end_turn"),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, true, QUIET_POLLS - 1).is_empty());
        assert_eq!(
            polls(&mut watch, &transcript, Idle, true, 1),
            answer("p4", "All green.")
        );
    }

    // Review (Oct 21): the idle count carried over from earlier polls, so a
    // prompt answered between two polls notified on the first poll.
    #[test]
    fn a_new_prompt_settles_from_the_start() {
        let transcript = Transcript::new();
        transcript.add(&[prompt("p0", "Earlier"), say("m0", "Old.", "end_turn")]);
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, true, 40).is_empty());
        transcript.add(&[prompt("p1", "Quick one"), say("m1", "Quick.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, true, QUIET_POLLS - 1).is_empty());
        assert_eq!(
            polls(&mut watch, &transcript, Idle, true, 1),
            answer("p1", "Quick.")
        );
        assert!(polls(&mut watch, &transcript, Idle, false, 40).is_empty());
        transcript.add(&[prompt("p2", "Another"), say("m2", "Sure.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, false, SETTLE_POLLS - 1).is_empty());
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 1),
            answer("p2", "Sure.")
        );
    }

    // Final review (Oct 21): history the watch had not read in full left a
    // report that looked queued, and the owner's next prompt never notified.
    // The watch follows only what is written after it began.
    #[test]
    fn a_watch_begins_clean_at_the_transcript_end() {
        // Before the watch: an agent's report queued by a process that has
        // since gone, so no turn will take it in.
        let transcript = Transcript::new();
        transcript.add(&[
            prompt("p1", "Audit the code"),
            call("m1"),
            started("agent", "a1"),
            say("m2", "Started agent.", "end_turn"),
            queued(&report("a1", Some("completed"))),
        ]);
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, false, 3).is_empty());
        transcript.add(&[prompt("p2", "Thanks"), say("m3", "Welcome.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 5),
            answer("p2", "Welcome.")
        );

        // A prompt under way when the watch began, its newest record still
        // being written, does not notify; the next one does, once.
        let transcript = Transcript::new();
        transcript.add(&[prompt("p1", "Read the logs"), call("m1")]);
        let output = serde_json::json!({"type":"user","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"x".repeat(4096)}]}})
        .to_string();
        let (head, tail) = output.split_at(2048);
        let append = |text: &str| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript.path)
                .unwrap();
            write!(file, "{text}").unwrap();
        };
        append(head);
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Working, false, 2).is_empty());
        append(&format!("{tail}\n"));
        transcript.add(&[say("m2", "The logs are clean.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, false, 5).is_empty());
        transcript.add(&[prompt("p2", "Thanks"), say("m3", "Welcome.", "end_turn")]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 5),
            answer("p2", "Welcome.")
        );
        assert!(polls(&mut watch, &transcript, Idle, false, 5).is_empty());
    }

    // Fifth review (Oct 21): a report left queued blocked every later prompt.
    // Whatever earlier prompts left outstanding, the owner's next prompt
    // notifies once, after its own reply.
    #[test]
    fn a_new_prompt_is_never_held_by_what_earlier_prompts_left() {
        let transcript = Transcript::new();
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, false, 1).is_empty());
        // A report queued that no turn takes in.
        transcript.add(&[
            prompt("p1", "Audit the code"),
            call("m1"),
            started("agent", "a1"),
            say("m2", "Started agent.", "end_turn"),
            queued(&report("a1", Some("completed"))),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, false, QUIET_POLLS + 1).is_empty());
        transcript.add(&[prompt("p2", "Thanks"), say("m3", "Welcome.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, false, SETTLE_POLLS - 1).is_empty());
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 1),
            answer("p2", "Welcome.")
        );
        assert!(polls(&mut watch, &transcript, Idle, false, 5).is_empty());
        // A task that never reports back, while other work keeps the
        // session's background busy.
        transcript.add(&[
            prompt("p3", "Watch it"),
            call("m4"),
            started("persistent", "m1"),
            call("m5"),
            started("agent", "a3"),
            say("m6", "Watching.", "end_turn"),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, true, 3 * QUIET_POLLS).is_empty());
        transcript.add(&[prompt("p4", "Status?"), say("m7", "All green.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, true, QUIET_POLLS - 1).is_empty());
        assert_eq!(
            polls(&mut watch, &transcript, Idle, true, 1),
            answer("p4", "All green.")
        );
        assert!(polls(&mut watch, &transcript, Idle, true, 3 * QUIET_POLLS).is_empty());
    }

    // Fifth review (Oct 21): a session whose record went missing for a poll
    // was forgotten, so its process's later exit, or another process taking
    // the session, went unseen and the report it had queued held the prompt.
    #[cfg(unix)]
    #[test]
    fn a_missing_record_keeps_the_process_until_it_exits_or_is_replaced() {
        use crate::desktop_activity::{process_identity, ClaudeSessionActivity};
        let record = |pid: u32| ClaudeSessionActivity {
            state: Idle,
            background: false,
            status_at: None,
            cwd: Some("/work".into()),
            process: process_identity(pid as i32),
        };
        // One poll of the session, as the watcher takes it.
        let step = |seen: &mut Option<ClaudeSeen>,
                    watch: &mut PromptWatch,
                    path: &std::path::Path,
                    activity: Option<&ClaudeSessionActivity>| {
            let sighting = Sighting::of(seen.as_ref(), activity);
            *seen = sighting.seen;
            if sighting.restarted {
                watch.restarted();
            }
            let state = activity.map(|activity| activity.state);
            watch.poll(path, state, false, sighting.exited)
        };
        let queued_report = |transcript: &Transcript, n: u32| {
            transcript.add(&[
                prompt(&format!("p{n}"), "Audit the code"),
                call(&format!("m{n}")),
                started("agent", &format!("a{n}")),
                say(&format!("r{n}"), "Started agent.", "end_turn"),
                queued(&report(&format!("a{n}"), Some("completed"))),
            ]);
        };

        // The process exits while its record is missing.
        let mut first = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let transcript = Transcript::new();
        let (mut seen, mut watch) = (None, PromptWatch::default());
        let alive = record(first.id());
        assert!(step(&mut seen, &mut watch, &transcript.path, Some(&alive)).is_none());
        queued_report(&transcript, 1);
        for _ in 0..3 {
            assert!(step(&mut seen, &mut watch, &transcript.path, Some(&alive)).is_none());
        }
        assert!(step(&mut seen, &mut watch, &transcript.path, None).is_none());
        assert!(
            seen.as_ref().is_some_and(|seen| seen.state.is_none()),
            "a missing record keeps its process"
        );
        first.kill().unwrap();
        first.wait().unwrap();
        assert_eq!(
            step(&mut seen, &mut watch, &transcript.path, None),
            Some(Answered {
                prompt: "p1".into(),
                reply: Some("Started agent.".into()),
            }),
            "the exit is seen, and the report went with the process"
        );
        assert!(seen.is_none());

        // Another process takes the session while its record is missing.
        let mut second = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let transcript = Transcript::new();
        let (mut seen, mut watch) = (None, PromptWatch::default());
        let before = record(second.id());
        assert!(step(&mut seen, &mut watch, &transcript.path, Some(&before)).is_none());
        queued_report(&transcript, 2);
        for _ in 0..3 {
            assert!(step(&mut seen, &mut watch, &transcript.path, Some(&before)).is_none());
        }
        assert!(step(&mut seen, &mut watch, &transcript.path, None).is_none());
        let after = record(std::process::id());
        let answered: Vec<_> = (0..SETTLE_POLLS)
            .filter_map(|_| step(&mut seen, &mut watch, &transcript.path, Some(&after)))
            .collect();
        assert_eq!(answered, answer("p2", "Started agent."));
        second.kill().unwrap();
        second.wait().unwrap();
    }

    // A reply whose last step was a tool call, with no text after it, does
    // not quote the commentary written before that call.
    #[test]
    fn a_reply_ending_in_a_tool_call_has_no_body() {
        let transcript = Transcript::new();
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, false, 1).is_empty());
        transcript.add(&[
            prompt("p1", "Run the tests"),
            say("m1", "Running the tests now.", "tool_use"),
            call("m1"),
            result(),
            serde_json::json!({"type":"assistant","message":{"id":"m2","role":"assistant",
                "content":[{"type":"thinking","thinking":"Done."}],"stop_reason":"end_turn"}}),
        ]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 2),
            vec![Answered {
                prompt: "p1".into(),
                reply: None,
            }]
        );
        // Text after the call is the reply, including one written in parts.
        transcript.add(&[
            prompt("p2", "Again"),
            call("m3"),
            result(),
            say("m4", "Tests pass.", "end_turn"),
            say("m4", "All 12 of them.", "end_turn"),
        ]);
        assert_eq!(
            polls(&mut watch, &transcript, Idle, false, 2)[0]
                .reply
                .as_deref(),
            Some("Tests pass.\nAll 12 of them.")
        );
    }

    #[test]
    fn stopped_running_or_cut_short_prompts_do_not_notify_completion() {
        let transcript = Transcript::new();
        let mut watch = PromptWatch::default();
        assert!(polls(&mut watch, &transcript, Idle, false, 1).is_empty());
        transcript.add(&[prompt("p1", "Go"), call("m1")]);
        assert!(polls(&mut watch, &transcript, Working, false, 1).is_empty());
        transcript.add(&[result(), say("m2", "Went.", "end_turn")]);
        assert_eq!(polls(&mut watch, &transcript, Idle, false, 3).len(), 1);
        // The owner stopped it on the Mac.
        transcript.add(&[
            prompt("p2", "Go on"),
            call("m3"),
            serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}),
        ]);
        assert!(polls(&mut watch, &transcript, Idle, false, 3).is_empty());
        // Still writing, as the transcript tells it, though the record lags.
        transcript.add(&[prompt("p3", "Next"), call("m4")]);
        assert!(polls(&mut watch, &transcript, Idle, false, 3).is_empty());
        // Its process quit mid-reply: that exit notifies as a failure instead.
        assert!(watch.poll(&transcript.path, None, false, true).is_none());
        transcript.add(&[result(), say("m5", "Late.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, false, 3).is_empty());
        // A process that exits after its final reply notifies at once.
        transcript.add(&[prompt("p4", "Last"), say("m6", "Bye.", "end_turn")]);
        assert!(polls(&mut watch, &transcript, Idle, true, 1).is_empty());
        assert_eq!(
            watch.poll(&transcript.path, None, false, true),
            Some(Answered {
                prompt: "p4".into(),
                reply: Some("Bye.".into()),
            })
        );
    }

    #[test]
    fn codex_notifies_when_the_open_turn_completes_or_fails() {
        let tail = |turn: &str, status: &'static str, reply: Option<&str>| {
            crate::codex_activity::Activity {
                turn: turn.into(),
                status,
                reply: reply.map(Into::into),
            }
        };
        assert_eq!(
            codex_transition(Some("t1"), &tail("t1", "completed", Some("Done."))),
            Some(("t1".into(), Notify::Completed, "Done.".into()))
        );
        assert_eq!(
            codex_transition(Some("t1"), &tail("t1", "failed", None)),
            Some(("t1".into(), Notify::Failed, String::new()))
        );
        // Stopped by the owner on the Mac: no notification, as for Wonder's Stop.
        assert_eq!(
            codex_transition(Some("t1"), &tail("t1", "interrupted", None)),
            None
        );
        // Another turn's ending, a turn still running, or no open turn before.
        assert_eq!(
            codex_transition(Some("t1"), &tail("t0", "completed", Some("Old"))),
            None
        );
        assert_eq!(
            codex_transition(Some("t1"), &tail("t1", "inProgress", None)),
            None
        );
        assert_eq!(
            codex_transition(None, &tail("t1", "completed", Some("Done."))),
            None
        );
    }

    #[test]
    fn rollouts_are_found_by_thread_in_dated_folders() {
        let dir = tempfile::tempdir().unwrap();
        let thread = "01a12bb1-ee34-7b31-a385-e42c3b66acf6";
        let day = dir.path().join("2026/10/11");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join(format!("rollout-2026-10-11T12-00-43-{thread}.jsonl"));
        std::fs::write(&file, "").unwrap();
        std::fs::write(day.join("rollout-2026-10-11T12-00-43-other.jsonl"), "").unwrap();
        let mut found = HashMap::new();
        index_rollouts_in(dir.path(), &[thread.to_owned()], &mut found);
        assert_eq!(found.get(thread), Some(&file));
        assert_eq!(found.len(), 1);
    }
}
