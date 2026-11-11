//! Whether Claude still runs agent tasks, commands or monitors in
//! an attached chat after its reply ended, for list rows.
//!
//! Claude Code's session record can say "idle" while a background command
//! runs, so only Cupola's Claude runtime can tell, from the transcript, task
//! files, process liveness and command output (`thread/backgroundTasks/list`).
//! That read can take a while on a long transcript, so lists never wait on it
//! twice: each answer is kept per runtime generation and chat, at most one
//! read per chat runs at a time, and a list shows the last verified answer
//! while a newer one is read off the list path. A chat never answered waits
//! for its own read until the page's deadline; a read still running then
//! keeps going for the next list instead of being asked again, and its
//! answer, if it changes the row, tells phones to read their Chats again.
//! One slow chat never discards another chat's answer, and a failed or slow
//! read never clears a verified one: the last answer is shown until it ages
//! out, after which the row is unmarked until a read answers.

use super::*;
use tokio::sync::watch;

/// A verified answer is shown without asking again for this long.
#[cfg(not(test))]
const FRESH: Duration = Duration::from_secs(10);
#[cfg(test)]
const FRESH: Duration = Duration::ZERO;
/// A verified answer is shown while a newer one is read for at most this
/// long; older, the row waits for its read like one never answered.
const SHOWN: Duration = Duration::from_secs(120);
/// One read may take this long before it stops holding the chat's slot. The
/// runtime may still finish it; a later read is then asked again.
const READ_LIMIT: Duration = Duration::from_secs(30);
/// Answers kept across chats and runtime generations.
const MAX_ENTRIES: usize = 512;

/// (runtime generation, Claude thread ID).
type Key = (String, String);

/// What a read concluded: Some(tasks still running) when the
/// runtime answered, None when it failed or ran out of time.
type Outcome = Option<bool>;

#[derive(Default)]
pub(crate) struct TaskSummaries {
    entries: std::sync::Mutex<HashMap<Key, Entry>>,
}

#[derive(Default)]
struct Entry {
    /// The last verified answer and when it was read.
    verified: Option<(bool, Instant)>,
    /// The read running for this chat; it publishes its outcome once.
    reading: Option<watch::Receiver<Option<Outcome>>>,
}

impl Entry {
    fn shown(&self, now: Instant) -> Option<bool> {
        self.verified
            .filter(|(_, at)| now.duration_since(*at) < SHOWN)
            .map(|(running, _)| running)
    }
}

/// One attached chat a list asks about.
pub(super) struct Asked {
    pub(super) thread: String,
    pub(super) native: String,
    pub(super) conversation: Option<String>,
}

/// Native IDs of the asked chats with tasks still running, as far
/// as verified answers know by `deadline`.
pub(super) async fn running(
    state: &AppState,
    rpc: &RpcClient,
    asked: Vec<Asked>,
    deadline: tokio::time::Instant,
) -> HashSet<String> {
    let summaries = &state.projects.task_summaries;
    let generation = rpc.health().id().to_owned();
    let now = Instant::now();
    let mut running = HashSet::new();
    let mut waits = Vec::new();
    {
        let mut entries = summaries.entries.lock().unwrap_or_else(|e| e.into_inner());
        for asked in asked {
            let key = (generation.clone(), asked.thread.clone());
            // Keep the bound even when every existing slot is still reading.
            // An idle entry can be replaced; an in-flight read keeps its slot.
            if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
                let oldest = entries
                    .iter()
                    .filter(|(_, entry)| entry.reading.is_none())
                    .min_by_key(|(_, entry)| entry.verified.map(|(_, at)| at))
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    entries.remove(&oldest);
                } else {
                    continue;
                }
            }
            let entry = entries.entry(key.clone()).or_default();
            let shown = entry.shown(now);
            let fresh = entry
                .verified
                .is_some_and(|(_, at)| now.duration_since(at) < FRESH);
            if shown == Some(true) {
                running.insert(asked.native.clone());
            }
            if fresh {
                continue;
            }
            let reading = match &entry.reading {
                Some(reading) => reading.clone(),
                None => {
                    let (tx, rx) = watch::channel(None);
                    entry.reading = Some(rx.clone());
                    spawn_read(state, rpc.clone(), key, asked.conversation.clone(), tx);
                    rx
                }
            };
            // A shown answer is enough for this page; the read updates it.
            if shown.is_none() {
                waits.push((asked.native, reading));
            }
        }
        prune(&mut entries, now);
    }
    let answered =
        futures_util::future::join_all(waits.into_iter().map(|(native, mut reading)| async move {
            let outcome = tokio::time::timeout_at(deadline, reading.wait_for(Option::is_some))
                .await
                .ok()?
                .ok()?
                .flatten()?;
            outcome.then_some(native)
        }))
        .await;
    running.extend(answered.into_iter().flatten());
    running
}

/// Reads one chat off the list path and records the answer. When it changes
/// what the chat's row shows, phones read it again.
fn spawn_read(
    state: &AppState,
    rpc: RpcClient,
    key: Key,
    conversation: Option<String>,
    done: watch::Sender<Option<Outcome>>,
) {
    let state = state.clone();
    tokio::spawn(async move {
        let outcome = tokio::time::timeout(READ_LIMIT, history::task_count_for_list(&rpc, &key.1))
            .await
            .ok()
            .flatten()
            .map(|count| count > 0);
        let changed = {
            let summaries = &state.projects.task_summaries;
            let mut entries = summaries.entries.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let entry = entries.entry(key).or_default();
            let before = entry.shown(now).unwrap_or(false);
            entry.reading = None;
            if let Some(running) = outcome {
                entry.verified = Some((running, now));
            }
            outcome.is_some_and(|running| running != before)
        };
        let _ = done.send(Some(outcome));
        if let (true, Some(conversation)) = (changed, conversation) {
            crate::external_push::publish_status(&state, &conversation).await;
        }
    });
}

/// Drops answers nothing shows any more, then the oldest beyond the bound.
/// A chat whose read is running keeps its slot so the read is not repeated.
fn prune(entries: &mut HashMap<Key, Entry>, now: Instant) {
    entries.retain(|_, entry| entry.reading.is_some() || entry.shown(now).is_some());
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    let mut idle = entries
        .iter()
        .filter(|(_, entry)| entry.reading.is_none())
        .map(|(key, entry)| (entry.verified.map(|(_, at)| at), key.clone()))
        .collect::<Vec<_>>();
    idle.sort();
    for (_, key) in idle.into_iter().take(entries.len() - MAX_ENTRIES) {
        entries.remove(&key);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A Claude runtime that answers each request on its own thread, so one
    /// chat's slow transcript read does not hold another's answer. Per chat:
    /// `tasks-<thread>` holds its task list (missing: the read fails), and
    /// the read waits while `hold-<thread>` exists.
    const RUNTIME: &str = r#"import json, os, sys, threading, time
root = os.path.dirname(__file__)
out = threading.Lock()
def answer(r):
    p = r.get('params', {})
    thread = p.get('threadId', '')
    with out:
        with open(root + '/requests', 'a') as log: log.write(json.dumps(r) + '\n')
    while os.path.exists(root + '/hold-' + thread): time.sleep(0.02)
    tasks = root + '/tasks-' + thread
    if r.get('method') == 'initialize':
        reply = {'id': r['id'], 'result': {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}}}
    elif r.get('method') != 'thread/backgroundTasks/list' or os.path.exists(tasks):
        reply = {'id': r['id'], 'result': {'data': json.load(open(tasks)) if os.path.exists(tasks) else []}}
    else:
        reply = {'id': r['id'], 'error': {'code': -32000, 'message': 'unreadable'}}
    with out:
        print(json.dumps(reply), flush=True)
for line in sys.stdin:
    r = json.loads(line)
    if 'id' in r: threading.Thread(target=answer, args=(r,), daemon=True).start()
"#;

    // Contract (R7-01, P3-01): each chat's answer stands on its own. A chat
    // answered in time keeps its mark while another's read runs past the
    // page's deadline; that read is not asked again while it runs and, once
    // it answers, marks its row and tells phones. A failed read keeps the
    // verified answer instead of clearing it.
    #[tokio::test]
    async fn slow_reads_keep_verified_siblings_and_are_never_repeated() {
        let (dir, state) = crate::test_support::fixture().await;
        let root = tempfile::tempdir().unwrap();
        let entrypoint = root.path().join("runtime.py");
        std::fs::write(&entrypoint, RUNTIME).unwrap();
        let mut runtime = wonder_app_server::AppServerClient::spawn_bridge(
            wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint,
                state_dir: dir.path().join("claude-state"),
                npm_cli: None,
                wonder_version: "test".into(),
                family: "claude",
                claude_config_dir: None,
            },
            crate::ingestion::notification_sink(state.store.clone()),
        )
        .await
        .unwrap();
        let rpc = runtime.rpc();
        let write =
            |name: &str, value: &str| std::fs::write(root.path().join(name), value).unwrap();
        let running_tasks =
            r#"[{"id":"t","kind":"agent","status":"running","runningElsewhere":true}]"#;
        write("tasks-fast", running_tasks);
        write("tasks-slow", running_tasks);
        write("hold-slow", "");
        let asked = || {
            ["fast", "slow"]
                .map(|thread| Asked {
                    thread: thread.into(),
                    native: format!("native-{thread}"),
                    conversation: Some(format!("chat-{thread}")),
                })
                .into()
        };
        let page = || async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let mut marked = running(&state, &rpc, asked(), deadline)
                .await
                .into_iter()
                .collect::<Vec<_>>();
            marked.sort();
            marked
        };
        let reads = |thread: &str| {
            std::fs::read_to_string(root.path().join("requests"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains(&format!("\"threadId\": \"{thread}\"")))
                .count()
        };
        let mut events = state.events.subscribe();
        // Two pages while the slow chat's read is held past their deadline.
        let (first, second) = tokio::join!(page(), page());
        assert_eq!(
            first,
            ["native-fast"],
            "the slow chat does not hide the fast one"
        );
        assert_eq!(second, ["native-fast"]);
        assert_eq!(reads("slow"), 1, "a running read is joined, not repeated");
        let request = std::fs::read_to_string(root.path().join("requests")).unwrap();
        assert!(request.contains("\"runningOnly\": true"), "{request}");
        // The slow read answers off the list path and tells phones.
        std::fs::remove_file(root.path().join("hold-slow")).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.conversation_id.as_deref() == Some("chat-slow") {
                    break event;
                }
            }
        })
        .await
        .expect("the slow chat's row is announced");
        assert!(matches!(
            changed.event,
            wonder_api::WonderEvent::Activity { ref category, .. } if category == "native_status"
        ));
        assert_eq!(page().await, ["native-fast", "native-slow"]);
        // Reads that fail or are still running leave the verified answers
        // shown, without waiting for them.
        std::fs::remove_file(root.path().join("tasks-fast")).unwrap();
        write("hold-slow", "");
        assert_eq!(page().await, ["native-fast", "native-slow"]);
        std::fs::remove_file(root.path().join("hold-slow")).unwrap();
        // A verified "nothing running" unmarks the row.
        write("tasks-fast", "[]");
        let deadline = Instant::now() + Duration::from_secs(10);
        while page().await.contains(&"native-fast".to_owned()) {
            assert!(Instant::now() < deadline, "the finished chat is unmarked");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        runtime.shutdown().await.unwrap();
        // A page arriving while all bounded slots belong to pending reads
        // stays uncertain instead of admitting an unbounded parsing queue.
        let (_held, waiting) = watch::channel(None);
        {
            let mut entries = state.projects.task_summaries.entries.lock().unwrap();
            entries.clear();
            for index in 0..MAX_ENTRIES {
                entries.insert(
                    (rpc.health().id().to_owned(), format!("held-{index}")),
                    Entry {
                        verified: None,
                        reading: Some(waiting.clone()),
                    },
                );
            }
        }
        let unknown = running(
            &state,
            &rpc,
            vec![Asked {
                thread: "overflow".into(),
                native: "overflow".into(),
                conversation: None,
            }],
            tokio::time::Instant::now(),
        )
        .await;
        assert!(
            unknown.is_empty(),
            "capacity is uncertainty, never a fabricated badge"
        );
        assert_eq!(
            state.projects.task_summaries.entries.lock().unwrap().len(),
            MAX_ENTRIES,
            "pending reads retain their slots without exceeding the cache bound"
        );
    }
}
