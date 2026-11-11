//! Approvals on the phone for Codex threads ChatGPT's desktop app runs
//! (owner, Oct 22: "How do we not have approvals?"). Cupola's own Codex
//! server never sees those requests: ChatGPT runs its own. But ChatGPT lets
//! another client follow a thread it runs (`codex_desktop_ipc::Session::
//! follow`): its state arrives as a snapshot and then numbered patches, and
//! `requests` in it lists what the turn waits on, as app-server requests
//! `{id, method, params}`. An answer sent back through the follower methods
//! is taken as a click in ChatGPT would be.
//!
//! The invariant: while a followed thread's turn runs in ChatGPT, the
//! phone's open requests for it (keyed `chatgpt:{thread}:{turn}:{id}`) are
//! those ChatGPT lists, as of the newest state applied, except one the
//! phone is answering right now (`resolving`), which its answer closes. A
//! request ChatGPT stops listing (answered there, or dropped) closes on the
//! phone. A gap in the numbering is never guessed across: the follower asks
//! for a new snapshot. When the watch ends, nothing it leaves open looks
//! answerable: its turn ended, or only ChatGPT can answer it now. One closed
//! because Cupola lost ChatGPT is offered again, under a new nonce, once a
//! later state shows ChatGPT still asks it; after a restart, what the phone
//! still offers is read back once (`recheck`).
//!
//! The external poll (`external_push`) calls `watch` for each followed
//! Codex thread with a running turn, and only those, so ChatGPT broadcasts
//! changes to Cupola only while a turn runs there.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::codex_desktop_ipc::{self, Decided, FOLLOWING_REQUESTED, STATE_CHANGED};
use crate::AppState;

const PREFIX: &str = "chatgpt:";
/// The requests ChatGPT takes a follower's answer to.
const METHODS: [&str; 4] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
];
/// The decision a request closed by ChatGPT (answered on the computer)
/// records.
pub(crate) const RESOLVED_THERE: &str = "chatgpt_resolved";
/// A request left when the watch lost ChatGPT: only the computer can
/// answer it now.
pub(crate) const ON_COMPUTER: &str = "chatgpt_on_computer";
const TURN_ENDED: &str = "turn_ended";
pub(crate) const GONE: &str =
    "ChatGPT no longer asks this. It was answered on your computer, or its reply ended.";
pub(crate) const ANSWER_ON_COMPUTER: &str =
    "Answer this in ChatGPT on your computer. Cupola lost its connection to ChatGPT.";
const UNREACHABLE: &str =
    "Cupola couldn't reach ChatGPT on your computer. Try again, or answer it in ChatGPT.";
const UNCONFIRMED: &str = "ChatGPT on your computer didn't confirm this answer. Try again shortly.";

/// The external poll asks every 4 s; a watch it stopped asking for (the
/// turn ended) ends after this.
#[cfg(not(test))]
const WANTED_FOR: Duration = Duration::from_secs(15);
#[cfg(test)]
const WANTED_FOR: Duration = Duration::from_millis(400);
#[cfg(not(test))]
const CHECK: Duration = Duration::from_secs(5);
#[cfg(test)]
const CHECK: Duration = Duration::from_millis(50);
/// Connections in a row that fail before a watch gives up, each this much
/// longer after the last.
const RECONNECTS: u32 = 3;
#[cfg(not(test))]
const BACKOFF: Duration = Duration::from_secs(1);
#[cfg(test)]
const BACKOFF: Duration = Duration::from_millis(20);
/// A thread whose watch could not start, or gave up, is not tried again
/// for this long.
#[cfg(not(test))]
const REST: Duration = Duration::from_secs(30);
#[cfg(test)]
const REST: Duration = Duration::from_millis(300);

/// One followed thread's state, as far as Cupola needs it: what its turn
/// waits on, and which owner revision that is.
#[derive(Debug, Default)]
pub(crate) struct Stream {
    owner: Option<String>,
    /// `None` until a snapshot arrives, and again after a gap.
    revision: Option<u64>,
    requests: Vec<Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    /// The requests changed (or arrived in a snapshot).
    Requests,
    Unchanged,
    /// A change was missed: ask the owner for a new snapshot.
    Gap,
    /// The owner asks who follows: say so again.
    Announce,
    /// Not about this thread, or not from its owner.
    Ignored,
}

impl Stream {
    pub(crate) fn requests(&self) -> &[Value] {
        &self.requests
    }

    /// Applies one broadcast about `thread`.
    pub(crate) fn apply(&mut self, thread: &str, message: &Value) -> Applied {
        let params = &message["params"];
        if params["conversationId"] != thread || params["hostId"] != "local" {
            return Applied::Ignored;
        }
        let method = message["method"].as_str();
        if method == Some(FOLLOWING_REQUESTED.0) {
            return Applied::Announce;
        }
        if method != Some(STATE_CHANGED.0) {
            return Applied::Ignored;
        }
        let source = message["sourceClientId"].as_str().map(str::to_owned);
        let change = &params["change"];
        let Some(revision) = change["revision"].as_u64() else {
            return Applied::Ignored;
        };
        match change["type"].as_str() {
            Some("snapshot") => {
                // Whoever sends a snapshot owns the thread now (as ChatGPT
                // itself takes it).
                self.owner = source;
                self.revision = Some(revision);
                let requests = change["conversationState"]["requests"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if requests == self.requests {
                    return Applied::Unchanged;
                }
                self.requests = requests;
                Applied::Requests
            }
            Some("patches") => {
                if source.is_none() || source != self.owner || self.revision.is_none() {
                    return Applied::Ignored;
                }
                if change["baseRevision"].as_u64() != self.revision {
                    self.revision = None;
                    return Applied::Gap;
                }
                let mut root = json!({"requests": self.requests});
                let mut touched = false;
                for patch in change["patches"].as_array().into_iter().flatten() {
                    if patch["path"].get(0) != Some(&json!("requests")) {
                        continue;
                    }
                    touched = true;
                    if apply_patch(&mut root, patch).is_none() {
                        self.revision = None;
                        return Applied::Gap;
                    }
                }
                self.revision = Some(revision);
                let Value::Array(requests) = root["requests"].take() else {
                    self.revision = None;
                    return Applied::Gap;
                };
                if !touched || requests == self.requests {
                    return Applied::Unchanged;
                }
                self.requests = requests;
                Applied::Requests
            }
            _ => Applied::Ignored,
        }
    }
}

/// One immer patch (`{op, path: [...], value}`) applied to `root`. `None`
/// when it does not fit: the state differs from the owner's.
fn apply_patch(root: &mut Value, patch: &Value) -> Option<()> {
    let op = patch["op"].as_str()?;
    let (last, parents) = patch["path"].as_array()?.split_last()?;
    let mut target = root;
    for key in parents {
        target = match target {
            Value::Array(items) => items.get_mut(index(key)?)?,
            Value::Object(fields) => fields.get_mut(&field(key)?)?,
            _ => return None,
        };
    }
    let value = || patch.get("value").cloned();
    match target {
        Value::Array(items) => {
            if last == "length" && op == "replace" {
                items.truncate(value()?.as_u64()? as usize);
                return Some(());
            }
            let at = if last == "-" {
                items.len()
            } else {
                index(last)?
            };
            match op {
                "add" if at <= items.len() => items.insert(at, value()?),
                "replace" => *items.get_mut(at)? = value()?,
                "remove" if at < items.len() => {
                    items.remove(at);
                }
                _ => return None,
            }
        }
        Value::Object(fields) => match op {
            "add" | "replace" => {
                fields.insert(field(last)?, value()?);
            }
            "remove" => {
                fields.remove(&field(last)?)?;
            }
            _ => return None,
        },
        _ => return None,
    }
    Some(())
}

fn index(key: &Value) -> Option<usize> {
    match key {
        Value::Number(n) => n.as_u64().map(|n| n as usize),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn field(key: &Value) -> Option<String> {
    match key {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The phone's request ID for ChatGPT's request `id` in `turn`: ChatGPT
/// numbers requests per process, so the turn keeps a reused number apart.
pub(crate) fn request_key(thread: &str, turn: &str, id: &Value) -> String {
    let id = id.as_str().map_or_else(|| id.to_string(), str::to_owned);
    format!("{PREFIX}{thread}:{turn}:{id}")
}

fn thread_prefix(thread: &str) -> String {
    format!("{PREFIX}{thread}:")
}

/// A request ChatGPT asked (`request_key`), not a provider runtime: none is
/// named in its parameters.
pub(crate) fn asked_in_chatgpt(server_request_id: &str, params: &Value) -> bool {
    server_request_id.starts_with(PREFIX)
        && params.get("_wonderRuntimeId").is_none()
        && params.get("_wonderChatGpt").is_some()
}

/// ChatGPT's request as the phone's approval: its key, method and params,
/// with what the answer needs kept aside (`_wonderChatGpt`, never sent to
/// the phone). Requests the follower methods can't answer stay in ChatGPT.
fn approval(thread: &str, request: &Value) -> Option<(String, &'static str, Value)> {
    let method = METHODS
        .into_iter()
        .find(|method| request["method"] == *method)?;
    let id = request
        .get("id")
        .filter(|id| id.is_string() || id.is_number())?;
    let mut params = request.get("params")?.as_object()?.clone();
    if params.get("threadId").and_then(Value::as_str) != Some(thread) {
        return None;
    }
    let turn = params
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|turn| !turn.is_empty())?
        .to_owned();
    // A follower's decision carries no instruction to the agent.
    params.remove("supportsDenyMessage");
    params.insert("_wonderChatGpt".into(), json!({"requestId": id}));
    Some((
        request_key(thread, &turn, id),
        method,
        Value::Object(params),
    ))
}

/// Makes the phone's open requests for `thread` equal ChatGPT's `requests`.
pub(crate) async fn reconcile(
    state: &AppState,
    conversation: &str,
    thread: &str,
    requests: &[Value],
) {
    let asked: Vec<_> = requests
        .iter()
        .filter_map(|request| approval(thread, request))
        .collect();
    for (key, method, params) in &asked {
        let opened = match state.store.approval(key).await {
            Ok(None) => state
                .store
                .insert_pending_approval(key, method, &params.to_string(), &now())
                .await
                .is_ok(),
            // Closed when Cupola lost ChatGPT, which still asks it: the
            // phone can answer it again.
            Ok(Some(stored)) if stored.decision.as_deref() == Some(ON_COMPUTER) => state
                .store
                .reopen_lost_approval(key, ON_COMPUTER, &params.to_string())
                .await
                .unwrap_or(false),
            _ => false,
        };
        if opened {
            publish(
                state,
                conversation,
                key,
                wonder_api::WonderEvent::ApprovalOpened {
                    request_id: key.clone(),
                },
            )
            .await;
        }
    }
    let _approval_guard = state.approval_lock.lock().await;
    let Ok(open) = state
        .store
        .open_approvals_with_prefix(&thread_prefix(thread))
        .await
    else {
        return;
    };
    // Answered or dropped in ChatGPT. An answer from the phone holds
    // `approval_lock` throughout, so a `resolving` row seen under it has no
    // answer under way (outcome unknown, or the host restarted): closed too.
    for (key, _) in &open {
        if !asked.iter().any(|(asked, _, _)| asked == key) {
            close(state, conversation, key, RESOLVED_THERE).await;
        }
    }
}

/// The watch ended: what it left open is closed once.
pub(crate) async fn stopped(state: &AppState, conversation: &str, thread: &str, turn_ended: bool) {
    let _approval_guard = state.approval_lock.lock().await;
    let Ok(open) = state
        .store
        .open_approvals_with_prefix(&thread_prefix(thread))
        .await
    else {
        return;
    };
    let decision = if turn_ended { TURN_ENDED } else { ON_COMPUTER };
    for (key, _) in &open {
        close(state, conversation, key, decision).await;
    }
}

async fn close(state: &AppState, conversation: &str, key: &str, decision: &str) {
    if state
        .store
        .retire_open_approval(key, decision, &now())
        .await
        .unwrap_or(false)
    {
        publish(
            state,
            conversation,
            key,
            wonder_api::WonderEvent::ApprovalResolved {
                request_id: key.to_owned(),
                decision: decision.to_owned(),
            },
        )
        .await;
    }
}

async fn publish(state: &AppState, conversation: &str, key: &str, event: wonder_api::WonderEvent) {
    let _ = crate::publish_event_with_context(
        state,
        event,
        crate::EventContext {
            request_id: Some(key.to_owned()),
            conversation_id: Some(conversation.to_owned()),
            approval_id: Some(key.to_owned()),
            ..crate::EventContext::default()
        },
    )
    .await;
}

fn now() -> String {
    crate::now_ms().to_string()
}

/// Threads followed now: when the poll last asked for each, and the keys
/// ChatGPT lists there (`None` before the first snapshot).
struct Watched {
    wanted_at: Instant,
    asked: Option<Vec<String>>,
}

static WATCHED: LazyLock<Mutex<HashMap<String, Watched>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RESTING: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn watched() -> std::sync::MutexGuard<'static, HashMap<String, Watched>> {
    WATCHED.lock().unwrap_or_else(|e| e.into_inner())
}

fn rest(thread: &str) {
    let mut resting = RESTING.lock().unwrap_or_else(|e| e.into_inner());
    resting.retain(|_, until| *until > Instant::now());
    resting.insert(thread.to_owned(), Instant::now() + REST);
}

fn resting(thread: &str) -> bool {
    RESTING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread)
        .is_some_and(|until| *until > Instant::now())
}

/// Whether ChatGPT still lists `key` in `thread`, as the watch last saw it:
/// `None` when no watch knows.
fn still_asked(thread: &str, key: &str) -> Option<bool> {
    watched()
        .get(thread)?
        .asked
        .as_ref()
        .map(|asked| asked.iter().any(|asked| asked == key))
}

pub(crate) fn watching(thread: &str) -> bool {
    watched().contains_key(thread)
}

/// Ends a watch however its task ends.
struct Watch(String);

impl Drop for Watch {
    fn drop(&mut self) {
        watched().remove(&self.0);
    }
}

/// Follows `thread` in ChatGPT while its turn runs there (one watch per
/// thread). Called on every external poll that sees the turn running.
pub(crate) async fn watch(state: &AppState, conversation: &str, thread: &str) {
    if let Some(watch) = watched().get_mut(thread) {
        watch.wanted_at = Instant::now();
        return;
    }
    if resting(thread) {
        return;
    }
    if !codex_desktop_ipc::may_deliver(&state.codex_home, thread).await {
        rest(thread);
        return;
    }
    {
        let mut watched = watched();
        if watched.contains_key(thread) {
            return;
        }
        watched.insert(
            thread.to_owned(),
            Watched {
                wanted_at: Instant::now(),
                asked: None,
            },
        );
    }
    let watch = Watch(thread.to_owned());
    let (state, conversation, thread) = (state.clone(), conversation.to_owned(), thread.to_owned());
    tokio::spawn(async move {
        let _watch = watch;
        let turn_ended = follow(&state, &conversation, &thread).await;
        if !turn_ended {
            rest(&thread);
        }
        stopped(&state, &conversation, &thread, turn_ended).await;
    });
}

fn wanted(thread: &str) -> bool {
    watched()
        .get(thread)
        .is_some_and(|watch| watch.wanted_at.elapsed() < WANTED_FOR)
}

/// Follows until the poll stops asking (true: the turn ended) or ChatGPT
/// can't be reached (false). A connection that could not open, was refused
/// the follow or dropped counts the same, each retry waiting longer; one
/// that delivered ChatGPT's state starts the count again.
async fn follow(state: &AppState, conversation: &str, thread: &str) -> bool {
    let mut failures = 0;
    loop {
        if connection(state, conversation, thread, &mut failures).await {
            return true;
        }
        failures += 1;
        if failures >= RECONNECTS {
            return false;
        }
        tokio::time::sleep(BACKOFF * failures).await;
        if !wanted(thread) {
            return true;
        }
    }
}

/// One connection's follow: true once the poll stops asking, false when it
/// could not start or was lost.
async fn connection(
    state: &AppState,
    conversation: &str,
    thread: &str,
    failures: &mut u32,
) -> bool {
    let Ok(mut session) = codex_desktop_ipc::open(&state.codex_home, thread).await else {
        return false;
    };
    if session.follow(true).await.is_err() {
        return false;
    }
    let mut stream = Stream::default();
    let mut check = tokio::time::interval(CHECK);
    check.tick().await;
    let ended = loop {
        tokio::select! {
            message = session.next_broadcast() => {
                let Some(message) = message else { break false };
                match stream.apply(thread, &message) {
                    Applied::Requests => {
                        *failures = 0;
                        let asked = stream
                            .requests()
                            .iter()
                            .filter_map(|request| approval(thread, request))
                            .map(|(key, _, _)| key)
                            .collect();
                        if let Some(watch) = watched().get_mut(thread) {
                            watch.asked = Some(asked);
                        }
                        reconcile(state, conversation, thread, stream.requests()).await;
                    }
                    Applied::Unchanged => {
                        *failures = 0;
                        if let Some(watch) = watched().get_mut(thread) {
                            watch.asked.get_or_insert_with(Vec::new);
                        }
                    }
                    Applied::Gap | Applied::Announce => {
                        if session.follow(true).await.is_err() {
                            break false;
                        }
                    }
                    Applied::Ignored => {}
                }
            }
            _ = check.tick() => if !wanted(thread) { break true },
        }
    };
    if ended {
        let _ = session.follow(false).await;
    }
    ended
}

/// How long a one-time read waits for ChatGPT's state.
#[cfg(not(test))]
const READ_WAIT: Duration = Duration::from_secs(5);
#[cfg(test)]
const READ_WAIT: Duration = Duration::from_millis(500);

/// What ChatGPT asks in `thread` now, read once by following it until its
/// state arrives: `None` when ChatGPT can't be reached or didn't say.
async fn read_requests(state: &AppState, thread: &str) -> Option<Vec<Value>> {
    if !codex_desktop_ipc::may_deliver(&state.codex_home, thread).await {
        return None;
    }
    let mut session = codex_desktop_ipc::open(&state.codex_home, thread)
        .await
        .ok()?;
    session.follow(true).await.ok()?;
    let mut stream = Stream::default();
    let read = tokio::time::timeout(READ_WAIT, async {
        loop {
            match stream.apply(thread, &session.next_broadcast().await?) {
                Applied::Requests | Applied::Unchanged => return Some(()),
                Applied::Announce => session.follow(true).await.ok()?,
                Applied::Gap | Applied::Ignored => {}
            }
        }
    })
    .await;
    let _ = session.follow(false).await;
    read.ok().flatten()?;
    Some(stream.requests().to_vec())
}

/// After a restart, nothing has followed `thread` yet: the requests the
/// phone still offers there (`waiting`, by the nonce each was read under)
/// are checked against ChatGPT once. One ChatGPT no longer asks closes;
/// one it still asks stays answerable (its turn's watch takes it over
/// while it runs); if ChatGPT can't be read, only the computer can answer.
/// Whether its turn still runs on the Mac doesn't decide what it asks.
pub(crate) async fn recheck(
    state: &AppState,
    conversation: &str,
    thread: &str,
    waiting: &[(String, String)],
) {
    // A watch owns the thread's requests while it runs.
    if watching(thread) {
        return;
    }
    if let Some(requests) = read_requests(state, thread).await {
        reconcile(state, conversation, thread, &requests).await;
        return;
    }
    let _approval_guard = state.approval_lock.lock().await;
    for (key, _) in waiting {
        if state
            .store
            .retire_open_approval(key, ON_COMPUTER, &now())
            .await
            .unwrap_or(false)
        {
            publish(
                state,
                conversation,
                key,
                wonder_api::WonderEvent::ApprovalResolved {
                    request_id: key.clone(),
                    decision: ON_COMPUTER.to_owned(),
                },
            )
            .await;
        }
    }
}

/// What became of the phone's answer to a request ChatGPT asked.
#[derive(Debug, PartialEq)]
pub(crate) enum Answered {
    Sent,
    /// ChatGPT no longer lists it: answered on the computer, or dropped.
    Gone,
    /// Not answered. `sent_nothing`: certainly never reached ChatGPT, so
    /// the phone may answer anew; otherwise the same answer is retried.
    Failed {
        message: &'static str,
        sent_nothing: bool,
    },
}

/// Sends the phone's verified answer (`response`, the app-server result) to
/// ChatGPT, which answers its request as a click there would.
pub(crate) async fn answer(
    state: &AppState,
    key: &str,
    method: &str,
    params: &Value,
    response: &Value,
) -> Answered {
    let (Some(thread), Some(request)) = (
        params["threadId"].as_str(),
        params["_wonderChatGpt"].get("requestId"),
    ) else {
        return Answered::Failed {
            message: UNREACHABLE,
            sent_nothing: true,
        };
    };
    if still_asked(thread, key) == Some(false) {
        return Answered::Gone;
    }
    let Ok(mut session) = codex_desktop_ipc::open(&state.codex_home, thread).await else {
        return Answered::Failed {
            message: UNREACHABLE,
            sent_nothing: true,
        };
    };
    match session.decide(method, request, response).await {
        Decided::Sent => Answered::Sent,
        Decided::NotSent(_) => Answered::Failed {
            message: UNREACHABLE,
            sent_nothing: true,
        },
        Decided::Unknown(_) => Answered::Failed {
            message: UNCONFIRMED,
            sent_nothing: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture of a live scratch turn (ChatGPT 26.1016.51333): one
    /// command approval in the first snapshot, answered by the follower;
    /// a second added by a patch, denied in ChatGPT.
    const CAPTURE: &str = include_str!("../tests/fixtures/chatgpt-follower-approvals.jsonl");

    fn frames() -> (String, Vec<Value>) {
        let frames: Vec<Value> = CAPTURE
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["dir"] == "in")
            .map(|row| row["frame"].clone())
            .collect();
        let thread = frames
            .iter()
            .find(|frame| frame["method"] == STATE_CHANGED.0)
            .unwrap()["params"]["conversationId"]
            .as_str()
            .unwrap()
            .to_owned();
        (thread, frames)
    }

    fn keys(thread: &str, stream: &Stream) -> Vec<String> {
        stream
            .requests()
            .iter()
            .filter_map(|request| approval(thread, request))
            .map(|(key, _, _)| key)
            .collect()
    }

    #[test]
    fn the_captured_stream_shows_each_request_while_chatgpt_asks_it() {
        let (thread, frames) = frames();
        let mut stream = Stream::default();
        let mut shown: Vec<Vec<String>> = vec![Vec::new()];
        for frame in &frames {
            if frame["type"] != "broadcast" {
                continue;
            }
            let applied = stream.apply(&thread, frame);
            assert_ne!(applied, Applied::Gap, "the capture has no gap");
            let now = keys(&thread, &stream);
            if shown.last() != Some(&now) {
                shown.push(now);
            }
        }
        let first = format!("chatgpt:{thread}:01a16448-7818-7410-a14a-80962aa03188:6");
        let second = format!("chatgpt:{thread}:01a16449-f1f9-78d3-bcaa-bef80ccd7938:7");
        assert_eq!(
            shown,
            [vec![], vec![first], vec![], vec![second], vec![]],
            "asked, answered by Cupola, asked again, denied in ChatGPT"
        );
        // The phone's view of the first: what the app-server asked, with
        // what the answer needs kept aside.
        let snapshot = frames
            .iter()
            .find(|frame| frame["params"]["change"]["type"] == "snapshot")
            .unwrap();
        let request = &snapshot["params"]["change"]["conversationState"]["requests"][0];
        let (_, method, params) = approval(&thread, request).unwrap();
        assert_eq!(method, "item/commandExecution/requestApproval");
        assert_eq!(params["_wonderChatGpt"], json!({"requestId": 6}));
        assert_eq!(
            params["availableDecisions"],
            json!(["accept", {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["touch", "approved.txt"]}}, "cancel"])
        );
        let summary = crate::redact_approval_params(method, &params);
        assert_eq!(
            summary["reason"],
            "Do you want to allow the command `touch approved.txt` in this folder?"
        );
        assert!(summary.get("_wonderChatGpt").is_none());
    }

    #[test]
    fn a_missed_change_waits_for_a_snapshot_instead_of_guessing() {
        let thread = "t";
        let state = |change: Value, source: &str| {
            json!({"type": "broadcast", "method": STATE_CHANGED.0, "version": STATE_CHANGED.1,
                "sourceClientId": source,
                "params": {"conversationId": thread, "hostId": "local", "change": change}})
        };
        let ask = |id: u64| {
            json!({"id": id, "method": "item/fileChange/requestApproval",
                "params": {"threadId": thread, "turnId": "turn", "itemId": "i"}})
        };
        let patch = |base: u64, patches: Value| json!({"type": "patches", "baseRevision": base, "revision": base + 1, "patches": patches});
        let mut stream = Stream::default();
        // Patches before any snapshot say nothing.
        assert_eq!(
            stream.apply(thread, &state(patch(0, json!([])), "owner")),
            Applied::Ignored
        );
        let snapshot = json!({"type": "snapshot", "revision": 3,
            "conversationState": {"requests": [ask(1)]}});
        assert_eq!(
            stream.apply(thread, &state(snapshot, "owner")),
            Applied::Requests
        );
        let add = json!([{"op": "add", "path": ["requests", 1], "value": ask(2)}]);
        assert_eq!(
            stream.apply(thread, &state(patch(3, add.clone()), "other")),
            Applied::Ignored,
            "only the owner changes it"
        );
        assert_eq!(
            stream.apply(thread, &state(patch(3, add), "owner")),
            Applied::Requests
        );
        assert_eq!(stream.requests().len(), 2);
        let unrelated = json!([{"op": "replace", "path": ["turnHistory", "x"], "value": 1}]);
        assert_eq!(
            stream.apply(thread, &state(patch(4, unrelated), "owner")),
            Applied::Unchanged
        );
        // Revision 6 never arrived.
        let remove = json!([{"op": "remove", "path": ["requests", 0]}]);
        assert_eq!(
            stream.apply(thread, &state(patch(6, remove.clone()), "owner")),
            Applied::Gap
        );
        assert_eq!(
            stream.apply(thread, &state(patch(7, remove.clone()), "owner")),
            Applied::Ignored,
            "nothing is applied until a snapshot"
        );
        assert_eq!(stream.requests().len(), 2, "the last known state stands");
        let snapshot = json!({"type": "snapshot", "revision": 8,
            "conversationState": {"requests": [ask(2)]}});
        assert_eq!(
            stream.apply(thread, &state(snapshot, "owner")),
            Applied::Requests
        );
        assert_eq!(
            stream.apply(thread, &state(patch(8, remove), "owner")),
            Applied::Requests
        );
        assert!(stream.requests().is_empty());
        // A patch that doesn't fit the state is a gap too.
        let bad = json!([{"op": "remove", "path": ["requests", 4]}]);
        assert_eq!(
            stream.apply(thread, &state(patch(9, bad), "owner")),
            Applied::Gap
        );
        // Another thread, or the owner asking who follows.
        let other = json!({"type": "broadcast", "method": STATE_CHANGED.0,
            "params": {"conversationId": "other", "hostId": "local", "change": {}}});
        assert_eq!(stream.apply(thread, &other), Applied::Ignored);
        let asks = json!({"type": "broadcast", "method": FOLLOWING_REQUESTED.0,
            "params": {"conversationId": thread, "hostId": "local"}});
        assert_eq!(stream.apply(thread, &asks), Applied::Announce);
    }

    #[test]
    fn immer_patches_apply_as_immer_writes_them() {
        let mut root = json!({"requests": [1, 2, 3]});
        for (patch, expected) in [
            (
                json!({"op": "add", "path": ["requests", 3], "value": 4}),
                json!([1, 2, 3, 4]),
            ),
            (
                json!({"op": "add", "path": ["requests", "-"], "value": 5}),
                json!([1, 2, 3, 4, 5]),
            ),
            (
                json!({"op": "replace", "path": ["requests", 0], "value": 0}),
                json!([0, 2, 3, 4, 5]),
            ),
            (
                json!({"op": "remove", "path": ["requests", 1]}),
                json!([0, 3, 4, 5]),
            ),
            (
                json!({"op": "replace", "path": ["requests", "length"], "value": 2}),
                json!([0, 3]),
            ),
            (
                json!({"op": "replace", "path": ["requests"], "value": []}),
                json!([]),
            ),
        ] {
            assert_eq!(apply_patch(&mut root, &patch), Some(()), "{patch}");
            assert_eq!(root["requests"], expected, "{patch}");
        }
        let mut nested = json!({"requests": [{"id": 1, "params": {"a": 1}}]});
        apply_patch(
            &mut nested,
            &json!({"op": "replace", "path": ["requests", 0, "params", "a"], "value": 2}),
        )
        .unwrap();
        assert_eq!(nested["requests"][0]["params"]["a"], 2);
        assert_eq!(
            apply_patch(&mut nested, &json!({"op": "move", "path": ["requests", 0]})),
            None
        );
    }

    #[test]
    fn only_requests_a_follower_can_answer_reach_the_phone() {
        let thread = "t";
        let request = |method: &str, thread: &str, turn: &str| {
            json!({"id": 1, "method": method,
                "params": {"threadId": thread, "turnId": turn, "supportsDenyMessage": true}})
        };
        for method in METHODS {
            let (key, _, params) = approval(thread, &request(method, thread, "turn")).unwrap();
            assert_eq!(key, "chatgpt:t:turn:1");
            assert!(asked_in_chatgpt(&key, &params));
            assert!(params.get("supportsDenyMessage").is_none());
        }
        for (method, in_thread, turn) in [
            ("item/plan/requestImplementation", thread, "turn"),
            ("mcpServer/elicitation/request", thread, "turn"),
            ("item/commandExecution/requestApproval", "another", "turn"),
            ("item/commandExecution/requestApproval", thread, ""),
        ] {
            assert_eq!(
                approval(thread, &request(method, in_thread, turn)),
                None,
                "{method}"
            );
        }
        let named = json!({"id": "a:b", "method": METHODS[0],
            "params": {"threadId": thread, "turnId": "turn"}});
        assert_eq!(approval(thread, &named).unwrap().0, "chatgpt:t:turn:a:b");
    }
}
