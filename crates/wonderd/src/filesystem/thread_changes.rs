//! What one thread changed ("This thread" in Code changes), as opposed to the
//! whole checkout (Uncommitted and Branch). A thread's changes are the union
//! of its turns' diffs, each turn limited to the paths its own tool calls
//! named:
//!
//! - Codex: each turn's whole-turn diff, saved by `turn_diff`; it wins over
//!   the same turn's checkpoint.
//! - Every runtime turn Cupola sends (Claude's shell edits included):
//!   checkpoints of the working tree at turn start and end, under hidden refs
//!   `refs/cupola/checkpoints/<thread>/<turn>/{start,end}`, the way T3 Code
//!   keeps `refs/t3/orchestration-v2/checkpoints/...`. A checkpoint
//!   stages the whole tree, untracked files included, into a private
//!   temporary index (`GIT_INDEX_FILE`) through the Git write view, so the
//!   owner's index, working tree, branch and HEAD never change. The start is
//!   taken before the send, so nothing the turn does can precede it; the end
//!   at `turn/completed`. A turn's diff is its start against its end; paths
//!   that turn's tool calls did not name are dropped, which keeps another
//!   thread's or the owner's edits in a shared checkout out (a path both
//!   named in that turn still shows both).
//! - A turn with neither (one started on the Mac, or a checkpoint that could
//!   not be read): the edits its own file-change items carry.
//!
//! T3 Code runs Cupola starts are checkpointed the same way, from the send to
//! the run's end, but which runtime turns a run covers is not known, so its
//! checkpoint is never read: its turns fall back to their items and the read
//! says it is partial.

use super::git_actions::{self, WriteView};
use super::repo_records::{self, Token};
use super::thread_repos;
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

const REF_ROOT: &str = "refs/cupola/checkpoints";
/// Where a runtime turn's starting snapshot waits for the turn's ID.
const BASELINE_ROOT: &str = "refs/cupola/baselines";
/// Checkpointed turns kept per thread; older ones and their refs are dropped.
const MAX_CHECKPOINTS: usize = 500;
/// A checkpoint that takes longer is skipped; its turn falls back to items.
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(60);
/// The owner's index is copied (so unchanged files are not hashed again) only
/// up to this size; a larger one is rebuilt from the working tree.
const MAX_INDEX_COPY: u64 = 512 * 1024 * 1024;
/// Paths named in a pathspec at most; a turn naming more reads every path.
const MAX_PATHSPECS: usize = 500;
/// How long a send waits for the snapshot its turn starts from. One not
/// ready by then is not used: the turn falls back to its edit items.
const BASELINE_WAIT: Duration = Duration::from_secs(10);
/// A baseline no turn took up within this is stale and is not used.
const BASELINE_FOR: Duration = Duration::from_secs(5 * 60);
/// One wait for a T3 Code run to end, under T3 Code's request timeout.
const T3_WAIT_STEP: Duration = Duration::from_secs(20);
/// A T3 Code run is watched this long at most; after that (or after a
/// restart) its checkpoint stays open until the next send closes it.
const T3_RUN_LIMIT: Duration = Duration::from_secs(12 * 60 * 60);

/// A thread's saved changes: its turn diffs and its checkpoint list.
pub(crate) fn thread_directory(data_dir: &Path, thread: &str) -> PathBuf {
    data_dir.join("thread-changes").join(key(thread))
}

/// Where `turn_diff` saves a thread's turn diffs.
pub(crate) fn turns_directory(thread_directory: &Path) -> PathBuf {
    thread_directory.join("turns")
}

fn key(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..32].to_owned()
}

/// An ID as one ref-name and file-name component: as is when plain, else its
/// hash, so no ID can form `..`, `.lock` or another path.
pub(crate) fn ref_component(value: &str) -> String {
    let plain = !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if plain {
        value.to_owned()
    } else {
        key(value)
    }
}

fn reference(thread: &str, turn: &str, boundary: &str) -> String {
    format!(
        "{REF_ROOT}/{}/{}/{boundary}",
        key(thread),
        ref_component(turn)
    )
}

fn baseline_reference(thread: &str) -> String {
    format!("{BASELINE_ROOT}/{}", key(thread))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Checkpoint {
    turn: String,
    repository: PathBuf,
    start: String,
    end: Option<String>,
    at_ms: u64,
    /// A T3 Code send's checkpoint: the runtime turns it covers are unknown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    t3: bool,
    /// What its turns named, saved once the turn ended and their items were
    /// read whole, so it still counts once they are older than the items a
    /// later read gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    named: Option<Named>,
}

/// A closed checkpoint's runtime turns and the absolute paths they named.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Named {
    turns: Vec<String>,
    paths: BTreeSet<PathBuf>,
}

/// A runtime turn's starting snapshot, taken before the turn was sent and
/// waiting for the turn's ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Baseline {
    repository: PathBuf,
    start: String,
    at_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline: Option<Baseline>,
}

fn load(directory: &Path) -> Saved {
    fs::read(directory.join("checkpoints.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(directory: &Path, saved: &Saved) {
    let Ok(bytes) = serde_json::to_vec(saved) else {
        return;
    };
    if fs::create_dir_all(directory).is_err() {
        return;
    }
    let path = directory.join("checkpoints.json");
    let temporary = path.with_extension("json.tmp");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(&temporary, &path);
    }
}

// -------------------------------------------------------------------------
// Checkpoints
// -------------------------------------------------------------------------

/// One change to a thread's checkpoints; a thread's steps are taken in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// A runtime turn is about to be sent: the working tree it starts from.
    Baseline,
    /// The runtime started this turn (its receipt or `turn/started`): it
    /// starts from the baseline taken before it was sent. Without one (a
    /// turn started on the Mac) it is not checkpointed.
    Began(String),
    /// The send started no turn: its baseline is dropped.
    Unsent,
    /// A message is about to go into T3 Code: its checkpoint starts now.
    Sending(String),
    /// That send started no run of its own (queued, steered or refused).
    Discard(String),
    /// The turn ended: the runtime's `turn/completed`, or T3 Code's run end.
    Ended(String),
}

/// The repository's write view staging into a private index seeded from the
/// owner's. The index file lives (and is removed) with the returned directory.
async fn checkpoint_view(top: &Path) -> Option<(WriteView, tempfile::TempDir)> {
    let view = WriteView::prepare(top, true).await.ok()?;
    let scratch = workspace_git::private_git_directory("wonder-checkpoint-").ok()?;
    let index = scratch.path().join("index");
    // The index can be hundreds of megabytes: copy it on a blocking thread,
    // not on the workers serving other requests.
    let (owner, copy) = (view.git_dir().join("index"), index.clone());
    tokio::task::spawn_blocking(move || match read_index(&owner) {
        Some(bytes) => fs::write(&copy, bytes).ok(),
        None => Some(()),
    })
    .await
    .ok()??;
    Some((view.for_checkpoint(&index), scratch))
}

/// The owner's index, opened without following a link and bounded; None when
/// absent or unusable (the checkpoint then stages from nothing).
fn read_index(path: &Path) -> Option<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_COPY {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_COPY + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_INDEX_COPY).then_some(bytes)
}

/// The working tree as a commit on HEAD (or on nothing), never on a branch.
async fn snapshot(view: &WriteView) -> Option<String> {
    // Fail closed: if the gitlinks can't be listed, no checkpoint is taken.
    let added = git_actions::add_parent_files(view, true).await.ok()?;
    added.success.then_some(())?;
    let tree = git_actions::checkpoint_git(view, &["write-tree"]).await?;
    let parent =
        git_actions::checkpoint_git(view, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await;
    let mut args = vec!["commit-tree", "--no-gpg-sign", "-m", "Cupola checkpoint"];
    if let Some(parent) = parent.as_deref() {
        args.extend(["-p", parent]);
    }
    args.push(&tree);
    git_actions::checkpoint_git(view, &args)
        .await
        .filter(|commit| !commit.is_empty())
}

async fn point(view: &WriteView, name: &str, commit: &str) -> bool {
    git_actions::checkpoint_git(
        view,
        &["update-ref", "-m", "Cupola checkpoint", name, commit],
    )
    .await
    .is_some()
}

async fn unpoint(view: &WriteView, name: &str) {
    let _ = git_actions::checkpoint_git(view, &["update-ref", "-d", name]).await;
}

/// T3 Code runs whose end is being waited for, as (thread, message).
static WATCHED: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Default::default);

fn watched(thread: &str, turn: &str) -> bool {
    WATCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(thread.to_owned(), turn.to_owned()))
}

/// Ends every open checkpoint `close` picks at `commit`: a turn whose end
/// never arrived ends where the next one starts. Returns the turns closed.
async fn close_open(
    view: &WriteView,
    saved: &mut Saved,
    thread: &str,
    top: &Path,
    commit: &str,
    close: impl Fn(&Checkpoint) -> bool,
) -> Vec<String> {
    let mut closed = Vec::new();
    for checkpoint in saved
        .checkpoints
        .iter_mut()
        .filter(|c| c.end.is_none() && c.repository == top && close(c))
    {
        if point(view, &reference(thread, &checkpoint.turn, "end"), commit).await {
            checkpoint.end = Some(commit.to_owned());
            closed.push(checkpoint.turn.clone());
        }
    }
    closed
}

/// Adds a started checkpoint, dropping the oldest ones (and their refs).
async fn push(view: &WriteView, saved: &mut Saved, thread: &str, checkpoint: Checkpoint) {
    saved.checkpoints.push(checkpoint);
    while saved.checkpoints.len() > MAX_CHECKPOINTS {
        let dropped = saved.checkpoints.remove(0);
        for boundary in ["start", "end"] {
            unpoint(view, &reference(thread, &dropped.turn, boundary)).await;
        }
    }
}

/// Takes one step for `thread` in the repository at `top`. `wanted` says
/// whether the send a starting snapshot is for still waits for it: one
/// that went ahead may already have edited the tree, so a late snapshot is
/// never used as a start. Returns the checkpoints (turns) the step closed.
async fn record(
    directory: &Path,
    thread: &str,
    step: &Step,
    top: &Path,
    wanted: &(dyn Fn() -> bool + Sync),
) -> Vec<String> {
    let mut saved = load(directory);
    let mut closed = Vec::new();
    let known = |saved: &Saved, turn: &str| {
        saved
            .checkpoints
            .iter()
            .any(|c| c.turn == turn && c.repository == top)
    };
    match step {
        Step::Baseline => {
            let Some((view, _scratch)) = checkpoint_view(top).await else {
                return Vec::new();
            };
            let Some(commit) = snapshot(&view).await else {
                return Vec::new();
            };
            if !wanted() || !point(&view, &baseline_reference(thread), &commit).await {
                return Vec::new();
            }
            saved.baseline = Some(Baseline {
                repository: top.to_path_buf(),
                start: commit,
                at_ms: now_ms(),
            });
            if view.finish().await.is_err() {
                return Vec::new();
            }
        }
        Step::Unsent => {
            if saved.baseline.take().is_none() {
                return Vec::new();
            }
            if let Ok(view) = WriteView::prepare(top, true).await {
                unpoint(&view, &baseline_reference(thread)).await;
                if view.finish().await.is_err() {
                    return Vec::new();
                }
            }
        }
        Step::Began(turn) => {
            // A replayed start keeps the turn's first one.
            if known(&saved, turn) {
                return Vec::new();
            }
            let Some(baseline) = saved.baseline.clone() else {
                return Vec::new();
            };
            let Ok(view) = WriteView::prepare(top, true).await else {
                return Vec::new();
            };
            saved.baseline = None;
            unpoint(&view, &baseline_reference(thread)).await;
            let fresh = now_ms().saturating_sub(baseline.at_ms) <= BASELINE_FOR.as_millis() as u64;
            if baseline.repository == top
                && fresh
                && point(&view, &reference(thread, turn, "start"), &baseline.start).await
            {
                closed =
                    close_open(&view, &mut saved, thread, top, &baseline.start, |c| !c.t3).await;
                let checkpoint = Checkpoint {
                    turn: turn.clone(),
                    repository: top.to_path_buf(),
                    start: baseline.start,
                    end: None,
                    at_ms: baseline.at_ms,
                    t3: false,
                    named: None,
                };
                push(&view, &mut saved, thread, checkpoint).await;
            }
            if view.finish().await.is_err() {
                return Vec::new();
            }
        }
        Step::Sending(message) => {
            if known(&saved, message) {
                return Vec::new();
            }
            let Some((view, _scratch)) = checkpoint_view(top).await else {
                return Vec::new();
            };
            let Some(commit) = snapshot(&view).await else {
                return Vec::new();
            };
            if !wanted() || !point(&view, &reference(thread, message, "start"), &commit).await {
                return Vec::new();
            }
            // A run nobody waits for any more (Cupola restarted) ends here.
            closed = close_open(&view, &mut saved, thread, top, &commit, |c| {
                c.t3 && !watched(thread, &c.turn)
            })
            .await;
            let checkpoint = Checkpoint {
                turn: message.clone(),
                repository: top.to_path_buf(),
                start: commit,
                end: None,
                at_ms: now_ms(),
                t3: true,
                named: None,
            };
            push(&view, &mut saved, thread, checkpoint).await;
            if view.finish().await.is_err() {
                return Vec::new();
            }
        }
        Step::Discard(turn) => {
            let Some(index) = saved
                .checkpoints
                .iter()
                .position(|c| &c.turn == turn && c.repository == top)
            else {
                return Vec::new();
            };
            saved.checkpoints.remove(index);
            if let Ok(view) = WriteView::prepare(top, true).await {
                for boundary in ["start", "end"] {
                    unpoint(&view, &reference(thread, turn, boundary)).await;
                }
                if view.finish().await.is_err() {
                    return Vec::new();
                }
            }
        }
        Step::Ended(turn) => {
            let Some(index) = saved
                .checkpoints
                .iter()
                .position(|c| &c.turn == turn && c.end.is_none() && c.repository == top)
            else {
                return Vec::new();
            };
            let Some((view, _scratch)) = checkpoint_view(top).await else {
                return Vec::new();
            };
            let Some(commit) = snapshot(&view).await else {
                return Vec::new();
            };
            if !point(&view, &reference(thread, turn, "end"), &commit).await {
                return Vec::new();
            }
            saved.checkpoints[index].end = Some(commit);
            closed.push(turn.clone());
            if view.finish().await.is_err() {
                return Vec::new();
            }
        }
    }
    save(directory, &saved);
    closed
}

/// The top folder of the repository holding `folder`, canonical.
async fn repository_top(folder: &Path) -> Option<PathBuf> {
    let bytes = read_only_git_bytes(folder.to_str()?, &["rev-parse", "--show-toplevel"])
        .await
        .ok()?;
    let top = String::from_utf8(bytes).ok()?;
    crate::canonical_path::canonicalize(top.trim()).ok()
}

/// Where a thread's checkpoints are taken: its Project folder's repository.
struct Place {
    conversation: String,
    directory: PathBuf,
    top: PathBuf,
}

/// Takes one step for the thread's Project folder; returns where, with the
/// turns the step closed.
async fn checkpoint_thread(
    state: &AppState,
    thread: &str,
    step: &Step,
    wanted: &(dyn Fn() -> bool + Sync),
) -> Option<(Place, Vec<String>)> {
    let conversation = state.store.conversation_for_thread(thread).await.ok()??;
    let stored = state
        .store
        .project_conversation(&conversation)
        .await
        .ok()??;
    let top = repository_top(Path::new(&stored.cwd)).await?;
    // Only the repository that holds the Project folder, with its own Git data.
    let denied = denied_paths(&state.denied_roots).ok()?;
    workspace_git::admit_write(&top, Path::new(&stored.cwd), &denied)
        .await
        .ok()?;
    let directory = thread_directory(&state.data_dir, thread);
    let closed = record(&directory, thread, step, &top, wanted).await;
    Some((
        Place {
            conversation,
            directory,
            top,
        },
        closed,
    ))
}

/// Saves what the checkpoints a step closed named, from the thread's items
/// read back only as far as their turns go.
async fn attribute_closed(state: &AppState, place: &Place, closed: &[String]) {
    // A runtime turn is whole once an older item shows. A T3 Code run's
    // turns are not known, so nothing is saved for it.
    let mut whole = load(&place.directory)
        .checkpoints
        .into_iter()
        .filter(|c| {
            closed.contains(&c.turn) && c.repository == place.top && c.named.is_none() && !c.t3
        })
        .map(|c| c.turn)
        .collect::<HashSet<_>>();
    if whole.is_empty() {
        return;
    }
    let mut previous = None::<String>;
    let mut enough = move |turn: Option<&str>| {
        let current = turn.map(str::to_owned);
        if previous != current {
            if let Some(newer) = previous.take() {
                whole.remove(&newer);
            }
        }
        previous = current;
        whole.is_empty()
    };
    let Ok(read) = thread_repos::thread_turn_items(state, &place.conversation, &mut enough).await
    else {
        return;
    };
    let cwd = state
        .store
        .conversation_execution_directory(&place.conversation)
        .await
        .ok()
        .flatten()
        .map_or_else(|| place.top.clone(), PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut saved = load(&place.directory);
    if attribute(
        &mut saved,
        &place.top,
        closed,
        &read.items,
        read.more,
        &cwd,
        home.as_deref(),
    ) {
        save(&place.directory, &saved);
    }
}

/// Saves on each of the `closed` checkpoints what its turns named, when the
/// items (newest first; `more`: older ones were not read) hold those turns
/// whole. Returns whether any was saved.
fn attribute(
    saved: &mut Saved,
    top: &Path,
    closed: &[String],
    items: &[(Option<String>, Value)],
    more: bool,
    cwd: &Path,
    home: Option<&Path>,
) -> bool {
    let entries = timeline(items);
    let cut = cut_short(&entries, more);
    let mut changed = false;
    for checkpoint in &mut saved.checkpoints {
        if checkpoint.named.is_some()
            || checkpoint.end.is_none()
            || checkpoint.repository != top
            || !closed.contains(&checkpoint.turn)
        {
            continue;
        }
        let Some(turns) = covered(&entries, checkpoint) else {
            continue;
        };
        if turns.iter().any(|&turn| Some(turn) == cut) {
            continue;
        }
        let named = Named {
            turns: turns
                .iter()
                .filter_map(|&turn| entries[turn].turn.map(str::to_owned))
                .collect(),
            paths: named_paths(
                turns
                    .iter()
                    .flat_map(|&turn| entries[turn].items.iter().copied()),
                cwd,
                home,
            ),
        };
        checkpoint.named = Some(named);
        changed = true;
    }
    changed
}

type Job = (Step, tokio::sync::oneshot::Sender<()>);

/// Steps waiting per thread, taken in order by one task; a thread is present
/// while its task runs.
static LANES: LazyLock<Mutex<HashMap<String, VecDeque<Job>>>> = LazyLock::new(Default::default);

/// Removes the thread's lane if its task stops early (a panic, or the
/// runtime shutting down), so the next step starts a new task.
struct Lane(Option<String>);

impl Drop for Lane {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            LANES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&thread);
        }
    }
}

/// Queues a step; a thread's steps are taken in the order they arrive. The
/// receiver resolves once it is done; dropping it before then tells a
/// starting snapshot that its send went ahead without it.
pub(crate) fn queue_step(
    state: &AppState,
    thread: &str,
    step: Step,
) -> tokio::sync::oneshot::Receiver<()> {
    let (done, finished) = tokio::sync::oneshot::channel();
    let job = (step, done);
    {
        let mut lanes = LANES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = lanes.get_mut(thread) {
            queue.push_back(job);
            return finished;
        }
        lanes.insert(thread.to_owned(), VecDeque::from([job]));
    }
    let state = state.clone();
    let thread = thread.to_owned();
    tokio::spawn(async move {
        let mut lane = Lane(Some(thread.clone()));
        loop {
            let next = {
                let mut lanes = LANES.lock().unwrap_or_else(|e| e.into_inner());
                match lanes.get_mut(&thread).and_then(VecDeque::pop_front) {
                    Some(job) => job,
                    None => {
                        lanes.remove(&thread);
                        lane.0 = None;
                        return;
                    }
                }
            };
            let (step, done) = next;
            let wanted = || !done.is_closed();
            let taken = tokio::time::timeout(
                CHECKPOINT_TIMEOUT,
                checkpoint_thread(&state, &thread, &step, &wanted),
            )
            .await
            .ok()
            .flatten();
            let _ = done.send(());
            // What the closed turns named is saved after the send goes ahead.
            if let Some((place, closed)) = taken.filter(|(_, closed)| !closed.is_empty()) {
                let _ = tokio::time::timeout(
                    CHECKPOINT_TIMEOUT,
                    attribute_closed(&state, &place, &closed),
                )
                .await;
            }
        }
    });
    finished
}

/// Before Cupola sends a runtime turn: the snapshot it starts from, so an
/// edit the runtime makes at once can never precede it. Waits a bounded
/// time; a snapshot not ready by then is not used.
pub(crate) async fn before_runtime_turn(state: &AppState, thread: &str) {
    let finished = queue_step(state, thread, Step::Baseline);
    let _ = tokio::time::timeout(BASELINE_WAIT, finished).await;
}

/// The runtime answered that send: the turn it started takes the baseline,
/// and a send that started none drops it.
pub(crate) fn runtime_turn_sent(state: &AppState, thread: &str, turn: Option<&str>) {
    let step = turn.map_or(Step::Unsent, |turn| Step::Began(turn.to_owned()));
    drop(queue_step(state, thread, step));
}

/// Before a message goes into a T3 Code thread: its run's starting
/// checkpoint, keyed by the message. Waits a bounded time so the send is
/// never held up for long.
pub(crate) async fn t3_sending(state: &AppState, conversation: &str, message: &str) {
    let Ok(Some(thread)) = state.store.conversation_thread(conversation).await else {
        return;
    };
    let finished = queue_step(state, &thread, Step::Sending(message.to_owned()));
    let _ = tokio::time::timeout(BASELINE_WAIT, finished).await;
}

/// Stops waiting for a T3 Code run when its watcher ends, however it ends.
struct Watch(String, String);

impl Drop for Watch {
    fn drop(&mut self) {
        WATCHED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(self.0.clone(), self.1.clone()));
    }
}

/// What T3 Code did with that send. A run it started is watched until T3
/// Code says it ended, which closes its checkpoint; a send that started no
/// run of its own drops the checkpoint (its turns fall back to their items).
pub(crate) async fn t3_sent(
    state: &AppState,
    conversation: &str,
    message: &str,
    t3_thread: &str,
    sent: &Result<Value, crate::t3_code::CallError>,
) {
    let Ok(Some(thread)) = state.store.conversation_thread(conversation).await else {
        return;
    };
    let run = match sent {
        Ok(result) if matches!(result["delivery"].as_str(), Some("started" | "restarted")) => {
            result["runId"].as_str().map(str::to_owned)
        }
        _ => None,
    };
    let Some(run) = run else {
        drop(queue_step(
            state,
            &thread,
            Step::Discard(message.to_owned()),
        ));
        return;
    };
    WATCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((thread.clone(), message.to_owned()));
    crate::t3_code::stream::watch(state, conversation, t3_thread);
    let watch = Watch(thread.clone(), message.to_owned());
    let (state, message, t3_thread) = (state.clone(), message.to_owned(), t3_thread.to_owned());
    let conversation = conversation.to_owned();
    tokio::spawn(async move {
        let _watch = watch;
        let deadline = tokio::time::Instant::now() + T3_RUN_LIMIT;
        loop {
            match state
                .t3_code
                .run_ended(&t3_thread, &run, T3_WAIT_STEP)
                .await
            {
                Ok(true) => break,
                // T3 Code answers at once for some runs: wait a moment.
                Ok(false) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_secs(1)).await
                }
                _ => return,
            }
        }
        // T3 Code says the reply is done: the open chat reads it now.
        crate::transcript_watch::wake(&state, &conversation).await;
        let _ = queue_step(&state, &thread, Step::Ended(message)).await;
    });
}

// -------------------------------------------------------------------------
// Reading a thread's changes
// -------------------------------------------------------------------------

/// One repository-relative path the thread changed, over all its turns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ThreadChange {
    pub(super) path: String,
    /// `added`, `modified` or `deleted`.
    pub(super) status: &'static str,
    pub(super) binary: bool,
    pub(super) additions: Option<u64>,
    pub(super) deletions: Option<u64>,
    /// The turns' hunks in turn order; None when too large to read here.
    pub(super) diff: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct ThreadChanges {
    pub(super) changes: Vec<ThreadChange>,
    /// Part of the thread could not be read; what is listed is still its own.
    pub(super) partial: bool,
    /// The thread's earliest items were not read (it is too long), so turns
    /// older than them that saved nothing of their own are not listed.
    pub(super) earlier: bool,
}

/// One turn's diff of one file, before the union.
#[derive(Debug)]
struct TurnFile {
    path: String,
    kind: &'static str,
    hunks: Option<String>,
    binary: bool,
    additions: Option<u64>,
    deletions: Option<u64>,
}

impl From<crate::turn_diff::FileDiff> for TurnFile {
    fn from(file: crate::turn_diff::FileDiff) -> Self {
        Self {
            path: file.path,
            kind: file.kind,
            binary: file.binary,
            additions: (!file.binary).then_some(file.additions),
            deletions: (!file.binary).then_some(file.deletions),
            hunks: Some(file.hunks),
        }
    }
}

/// Every path these tool calls named, absolute: files their edits touched
/// and the words of their shell commands that resolve to a path.
fn named_paths<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    cwd: &Path,
    home: Option<&Path>,
) -> BTreeSet<PathBuf> {
    let mut named = BTreeSet::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("fileChange") => {
                for path in change_paths(item) {
                    named.extend(repo_records::absolute(cwd, path, home));
                }
            }
            Some("mcpToolCall") | Some("dynamicToolCall") => {
                let action = item.get("toolAction");
                if action.and_then(|a| a.get("kind")).and_then(Value::as_str) == Some("edit") {
                    for path in action
                        .and_then(|a| a.get("paths"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        named.extend(repo_records::absolute(cwd, path, home));
                    }
                }
            }
            Some("commandExecution") => {
                let Some(command) = item.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let mut folder = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|dir| repo_records::absolute(cwd, dir, home))
                    .unwrap_or_else(|| cwd.to_path_buf());
                let tokens = repo_records::tokenize(command);
                for segment in tokens.split(|token| *token == Token::Break) {
                    let words = segment
                        .iter()
                        .filter_map(|token| match token {
                            Token::Word(word) => Some(word.as_str()),
                            Token::Break => None,
                        })
                        .collect::<Vec<_>>();
                    if let ["cd" | "pushd", target, ..] = words.as_slice() {
                        if let Some(next) = repo_records::plain(target)
                            .then(|| repo_records::absolute(&folder, target, home))
                            .flatten()
                        {
                            folder = next;
                        }
                        continue;
                    }
                    for word in words.iter().skip(1) {
                        let word = redirect_target(word);
                        let value = word.split_once('=').map_or(word, |(_, value)| value);
                        for candidate in [word, value] {
                            if repo_records::plain(candidate) && !candidate.starts_with('-') {
                                named.extend(repo_records::absolute(&folder, candidate, home));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    named
}

/// A shell word without the redirect it starts with (`>out.txt`, `2>>log`).
/// Digits are a file descriptor only right before `>` or `<`, so `2026.txt`
/// stays as it is.
fn redirect_target(word: &str) -> &str {
    let operator = word.trim_start_matches(|c: char| c.is_ascii_digit());
    if operator.starts_with(['>', '<']) {
        operator.trim_start_matches(['>', '<', '&', '|'])
    } else {
        word.trim_start_matches(['&', '|'])
    }
}

fn change_paths(item: &Value) -> impl Iterator<Item = &str> {
    item.get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|change| change.get("path").and_then(Value::as_str))
}

/// Whether the thread named this file, or a folder inside the repository
/// holding it (never the repository itself: `git add .` names everything).
fn was_named(named: &BTreeSet<PathBuf>, repository: &Path, relative: &str) -> bool {
    let full = repository.join(relative);
    named.contains(&full)
        || full
            .ancestors()
            .skip(1)
            .take_while(|folder| *folder != repository && folder.starts_with(repository))
            .any(|folder| named.contains(folder))
}

/// A checkpoint pair's changes in the named paths. An open checkpoint is
/// compared with a snapshot of the working tree now (no ref is written).
async fn checkpoint_files(
    repository: &Path,
    checkpoint: &Checkpoint,
    named: &BTreeSet<PathBuf>,
) -> Result<Vec<TurnFile>, ()> {
    let end = match &checkpoint.end {
        Some(end) => end.clone(),
        None => {
            let (view, _scratch) = checkpoint_view(repository).await.ok_or(())?;
            snapshot(&view).await.ok_or(())?
        }
    };
    let path = repository.to_str().ok_or(())?;
    let pathspecs = named
        .iter()
        .filter_map(|full| full.strip_prefix(repository).ok())
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| format!(":(literal){}", relative.to_string_lossy()))
        .collect::<Vec<_>>();
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        &checkpoint.start,
        &end,
        "--",
    ];
    if pathspecs.len() <= MAX_PATHSPECS {
        args.extend(pathspecs.iter().map(String::as_str));
    }
    let files = match read_only_git_bytes(path, &args).await {
        Ok(bytes) => crate::turn_diff::files(&String::from_utf8_lossy(&bytes))
            .into_iter()
            .map(TurnFile::from)
            .collect::<Vec<_>>(),
        // Too large to show: list the files with their counts only.
        Err(error) if error.contains("exceeded its limit") => {
            args[1] = "--numstat";
            args.insert(2, "-z");
            let bytes = read_only_git_bytes(path, &args).await.map_err(|_| ())?;
            numstat_files(&bytes)
        }
        Err(_) => return Err(()),
    };
    Ok(files
        .into_iter()
        .filter(|file| was_named(named, repository, &file.path))
        .collect())
}

/// `git diff --numstat -z --no-renames` records, without hunks. Whether a
/// file was added or deleted is not known here, so it reads as modified.
fn numstat_files(bytes: &[u8]) -> Vec<TurnFile> {
    bytes
        .split(|b| *b == 0)
        .filter_map(|record| {
            let mut parts = record.splitn(3, |b| *b == b'\t');
            let (added, deleted, path) = (parts.next()?, parts.next()?, parts.next()?);
            let number = |value: &[u8]| std::str::from_utf8(value).ok()?.parse().ok();
            let binary = added == b"-";
            Some(TurnFile {
                path: String::from_utf8_lossy(path).into_owned(),
                kind: "update",
                hunks: None,
                binary,
                additions: number(added),
                deletions: number(deleted),
            })
        })
        .filter(|file| !file.path.is_empty())
        .collect()
}

/// Joins each path's turn diffs, oldest turn first.
fn union(turns: Vec<Vec<TurnFile>>) -> Vec<ThreadChange> {
    let mut order = Vec::<String>::new();
    let mut joined = HashMap::<String, (&'static str, &'static str, TurnFile)>::new();
    for file in turns.into_iter().flatten() {
        match joined.get_mut(&file.path) {
            Some((_, last, total)) => {
                *last = file.kind;
                total.binary |= file.binary;
                total.additions = total.additions.zip(file.additions).map(|(a, b)| a + b);
                total.deletions = total.deletions.zip(file.deletions).map(|(a, b)| a + b);
                total.hunks = total.hunks.take().zip(file.hunks).map(|(mut a, b)| {
                    if !a.is_empty() && !b.is_empty() {
                        a.push('\n');
                    }
                    a.push_str(&b);
                    a
                });
            }
            None => {
                order.push(file.path.clone());
                joined.insert(file.path.clone(), (file.kind, file.kind, file));
            }
        }
    }
    let mut changes = order
        .into_iter()
        .filter_map(|path| {
            let (first, last, file) = joined.remove(&path)?;
            let status = match (first, last) {
                // Made and removed again by the thread: nothing remains.
                ("add", "delete") => return None,
                ("add", _) => "added",
                (_, "delete") => "deleted",
                _ => "modified",
            };
            Some(ThreadChange {
                path,
                status,
                binary: file.binary,
                additions: (!file.binary).then_some(file.additions).flatten(),
                deletions: (!file.binary).then_some(file.deletions).flatten(),
                diff: file.hunks.filter(|_| !file.binary),
            })
        })
        .collect::<Vec<_>>();
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

/// The thread's own changes in `repository` (canonical, verified), with
/// repository-relative paths.
pub(super) async fn read(
    state: &AppState,
    conversation: &str,
    repository: &Path,
) -> Result<ThreadChanges, ()> {
    let thread = match state.store.conversation_thread(conversation).await {
        Ok(Some(thread)) if !thread.is_empty() => thread,
        Ok(_) => return Ok(ThreadChanges::default()),
        Err(_) => return Err(()),
    };
    let cwd = state
        .store
        .conversation_execution_directory(conversation)
        .await
        .map_err(|_| ())?
        .map(PathBuf::from);
    let mut partial = false;
    let (items, more) =
        match thread_repos::thread_turn_items(state, conversation, &mut |_| false).await {
            Ok(read) => (read.items, read.more),
            Err(()) => {
                partial = true;
                (Vec::new(), false)
            }
        };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd_path = cwd.clone().unwrap_or_else(|| repository.to_path_buf());
    let directory = thread_directory(&state.data_dir, &thread);
    let in_cwd_repository = match &cwd {
        Some(cwd) => repository_top(cwd).await.as_deref() == Some(repository),
        None => false,
    };
    Ok(collect(
        &directory,
        repository,
        &items,
        more,
        &cwd_path,
        home.as_deref(),
        in_cwd_repository,
        partial,
    )
    .await)
}

/// One entry of the thread's timeline, oldest first: a turn with its items,
/// or an item that names no turn (it stands alone).
struct Entry<'a> {
    turn: Option<&'a str>,
    items: Vec<&'a Value>,
}

/// The thread's items (newest first) as its timeline, oldest first.
fn timeline(items: &[(Option<String>, Value)]) -> Vec<Entry<'_>> {
    let mut entries = Vec::<Entry>::new();
    let mut at = HashMap::<&str, usize>::new();
    for (turn, item) in items.iter().rev() {
        match turn.as_deref() {
            Some(turn) => match at.get(turn) {
                Some(&index) => entries[index].items.push(item),
                None => {
                    at.insert(turn, entries.len());
                    entries.push(Entry {
                        turn: Some(turn),
                        items: vec![item],
                    });
                }
            },
            None => entries.push(Entry {
                turn: None,
                items: vec![item],
            }),
        }
    }
    entries
}

/// The timeline entries a checkpoint covers: the turns saved with it, or its
/// own turn. None when that is not among the items read, and always for a
/// T3 Code run, whose turns are not known.
fn covered(entries: &[Entry], checkpoint: &Checkpoint) -> Option<Vec<usize>> {
    if checkpoint.t3 {
        return None;
    }
    let position = |turn: &str| entries.iter().position(|e| e.turn == Some(turn));
    if let Some(named) = &checkpoint.named {
        return Some(
            named
                .turns
                .iter()
                .filter_map(|turn| position(turn))
                .collect(),
        );
    }
    position(&checkpoint.turn).map(|index| vec![index])
}

/// The oldest turn read, when older items were not: its items may be cut.
fn cut_short(entries: &[Entry], more: bool) -> Option<usize> {
    more.then(|| entries.iter().position(|e| e.turn.is_some()))
        .flatten()
}

/// A turn's own edit items, as its diff.
fn item_files(entry: &Entry, repository: &Path, cwd: &Path, home: Option<&Path>) -> Vec<TurnFile> {
    let mut files = Vec::new();
    for item in &entry.items {
        if item.get("type").and_then(Value::as_str) != Some("fileChange") {
            continue;
        }
        for change in item
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(relative) = change
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| repo_records::absolute(cwd, path, home))
                .and_then(|full| {
                    full.strip_prefix(repository)
                        .ok()
                        .map(|relative| relative.to_string_lossy().into_owned())
                })
                .filter(|relative| !relative.is_empty())
            else {
                continue;
            };
            let kind = match change.pointer("/kind/type").and_then(Value::as_str) {
                Some("add") => "add",
                Some("delete") => "delete",
                _ => "update",
            };
            let hunks = change
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let count = |sign: char| {
                hunks
                    .lines()
                    .filter(|line| {
                        line.starts_with(sign)
                            && !line.starts_with("+++")
                            && !line.starts_with("---")
                    })
                    .count() as u64
            };
            files.push(TurnFile {
                path: relative,
                kind,
                binary: false,
                additions: Some(count('+')),
                deletions: Some(count('-')),
                hunks: Some(hunks),
            });
        }
    }
    files
}

// -------------------------------------------------------------------------
// Saved Codex diffs: bounded, and read off the async workers
// -------------------------------------------------------------------------

/// The largest single saved diff read: what `turn_diff` keeps (4 MiB).
const MAX_SAVED_DIFF_FILE: u64 = crate::turn_diff::MAX_DIFF_BYTES as u64;
/// Bytes read from disk for one collection, over all saved diffs. A thread
/// keeps up to 500 diffs of 4 MiB (2 GiB); past this the rest are not read.
const MAX_SAVED_DIFF_INPUT: u64 = 32 * 1024 * 1024;
/// Hunk bytes kept from parsed saved diffs. The union only concatenates
/// them, so this also bounds the aggregated diffs a collection returns.
const MAX_SAVED_DIFF_HUNKS: usize = 8 * 1024 * 1024;
/// Per-file entries kept from parsed saved diffs (path and counts, no hunks).
const MAX_SAVED_DIFF_ENTRIES: usize = 20_000;
/// Blocking workers reading saved diffs at once, for all requests together.
/// Each holds at most one diff's text and parse (about 4 MiB of text, its
/// hunks and, for a diff made only of headers, as many entries as fit in it)
/// beside what it keeps; an abandoned worker finishes its current file and
/// stops, so the process never has more of them than this.
const MAX_SAVED_DIFF_READERS: usize = 2;

#[derive(Clone, Copy, Debug)]
struct SavedLimits {
    file: u64,
    input: u64,
    hunks: usize,
    entries: usize,
}

impl SavedLimits {
    const DOCUMENTED: Self = Self {
        file: MAX_SAVED_DIFF_FILE,
        input: MAX_SAVED_DIFF_INPUT,
        hunks: MAX_SAVED_DIFF_HUNKS,
        entries: MAX_SAVED_DIFF_ENTRIES,
    };
}

/// A thread's saved Codex diffs as far as the limits allowed.
#[derive(Default)]
struct SavedDiffs {
    /// (first seen ms, turn, files), oldest first.
    diffs: Vec<(u64, String, Vec<TurnFile>)>,
    /// A diff was unreadable or too large, or a limit ended the read early:
    /// the turns after it fall back to their checkpoints and items.
    partial: bool,
    /// Bytes read, hunk bytes kept and entries kept.
    input: u64,
    hunks: usize,
    entries: usize,
}

/// Flushes, lists, reads and parses a thread's saved diffs, oldest first,
/// stopping before a diff that would pass a limit. Blocking.
fn load_saved_diffs(
    directory: &Path,
    limits: SavedLimits,
    cancelled: &AtomicBool,
    read: &dyn Fn(&Path, u64) -> crate::turn_diff::SavedRead,
) -> SavedDiffs {
    use crate::turn_diff::SavedRead;
    let mut loaded = SavedDiffs::default();
    if cancelled.load(Ordering::Relaxed) {
        loaded.partial = true;
        return loaded;
    }
    for saved in crate::turn_diff::saved_turns(&turns_directory(directory)) {
        if cancelled.load(Ordering::Relaxed) {
            loaded.partial = true;
            break;
        }
        let Some((at_ms, turn)) = saved
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.split_once('-'))
            .map(|(at, turn)| (at.parse().unwrap_or_default(), turn.to_owned()))
        else {
            continue;
        };
        let allowed = limits.file.min(limits.input - loaded.input);
        let text = match read(&saved, allowed) {
            SavedRead::Text(text) => text,
            // The input budget, not this file, is what ran out.
            SavedRead::TooLarge if allowed < limits.file => {
                loaded.partial = true;
                break;
            }
            SavedRead::TooLarge | SavedRead::Unreadable => {
                loaded.partial = true;
                continue;
            }
        };
        loaded.input += text.len() as u64;
        let files = crate::turn_diff::files(&text)
            .into_iter()
            .map(TurnFile::from)
            .collect::<Vec<_>>();
        drop(text);
        let hunks = files
            .iter()
            .map(|file| file.hunks.as_ref().map_or(0, String::len))
            .sum::<usize>();
        if loaded.hunks + hunks > limits.hunks || loaded.entries + files.len() > limits.entries {
            loaded.partial = true;
            break;
        }
        loaded.hunks += hunks;
        loaded.entries += files.len();
        loaded.diffs.push((at_ms, turn, files));
    }
    loaded
}

/// The blocking boundary for saved-diff reads: at most `workers` run at
/// once, each holding its permit until it ends, so a request that is dropped
/// mid-read leaves a worker that stops at the next file and cannot be joined
/// by an unbounded number of others.
struct SavedReads {
    gate: Arc<tokio::sync::Semaphore>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

static SAVED_READS: LazyLock<SavedReads> =
    LazyLock::new(|| SavedReads::new(MAX_SAVED_DIFF_READERS));

/// Marks a worker as running for the counters, even if it panics.
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Tells an abandoned worker to stop when its request is dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl SavedReads {
    fn new(workers: usize) -> Self {
        Self {
            gate: Arc::new(tokio::sync::Semaphore::new(workers)),
            active: Arc::default(),
            peak: Arc::default(),
        }
    }

    async fn load(
        &self,
        directory: &Path,
        limits: SavedLimits,
        read: impl Fn(&Path, u64) -> crate::turn_diff::SavedRead + Send + 'static,
    ) -> SavedDiffs {
        let failed = || SavedDiffs {
            partial: true,
            ..SavedDiffs::default()
        };
        let Ok(permit) = self.gate.clone().acquire_owned().await else {
            return failed();
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let _stop = StopOnDrop(cancelled.clone());
        let (directory, active, peak) = (
            directory.to_path_buf(),
            self.active.clone(),
            self.peak.clone(),
        );
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            let _running = Running(active);
            load_saved_diffs(&directory, limits, &cancelled, &read)
        });
        worker.await.unwrap_or_else(|_| failed())
    }
}

/// The union for one repository. Each turn counts once, oldest first, from
/// its best source: its saved Codex diff (which names paths relative to the
/// thread's own repository, so only when that is this one), else the
/// checkpoint covering it, limited to the paths that checkpoint's turns
/// named, else its own edit items. A checkpoint that can't be read leaves
/// its turns to their items. Items are newest first; `more`: older ones were
/// not read, and saved diffs and checkpoints of those turns come first.
/// Saved diffs are read on a bounded blocking worker within the
/// `MAX_SAVED_DIFF_*` limits; when one runs out the read is partial and the
/// turns not read fall back to their checkpoints and items.
#[allow(clippy::too_many_arguments)]
async fn collect(
    directory: &Path,
    repository: &Path,
    items: &[(Option<String>, Value)],
    more: bool,
    cwd: &Path,
    home: Option<&Path>,
    in_cwd_repository: bool,
    mut partial: bool,
) -> ThreadChanges {
    let entries = timeline(items);
    let cut = cut_short(&entries, more);
    // Saved Codex diffs as (first seen ms, turn, files), oldest first.
    let mut diffs = Vec::<(u64, String, Vec<TurnFile>)>::new();
    if in_cwd_repository {
        let loaded = SAVED_READS
            .load(directory, SavedLimits::DOCUMENTED, |path, limit| {
                crate::turn_diff::read_saved(path, limit)
            })
            .await;
        partial |= loaded.partial;
        diffs = loaded.diffs;
    }
    let has_diff = |turn: &str| {
        diffs
            .iter()
            .any(|(_, saved, _)| *saved == ref_component(turn))
    };

    let mut checkpoints = load(directory).checkpoints;
    checkpoints.retain(|checkpoint| checkpoint.repository == repository);
    // Which checkpoint stands for each entry, and what it read.
    let mut claimed = vec![None::<usize>; entries.len()];
    let mut read = HashMap::<usize, Vec<TurnFile>>::new();
    for (index, checkpoint) in checkpoints.iter().enumerate() {
        let Some(listed) = covered(&entries, checkpoint) else {
            // A T3 Code run's turns are unknown. Otherwise its turns are
            // older than the items read, and what they named was never saved
            // (a turn still running may list none yet).
            partial |= checkpoint.t3 || checkpoint.end.is_some();
            continue;
        };
        let turns = listed
            .iter()
            .copied()
            .filter(|&turn| claimed[turn].is_none())
            .collect::<Vec<_>>();
        if !listed.is_empty() && turns.is_empty() {
            continue;
        }
        // A Codex diff of its own stands for a turn instead.
        let saved_turns = checkpoint.named.iter().flat_map(|named| &named.turns);
        if turns
            .iter()
            .filter_map(|&turn| entries[turn].turn)
            .chain(saved_turns.map(String::as_str))
            .any(has_diff)
        {
            continue;
        }
        let mut named = match &checkpoint.named {
            Some(named) => named.paths.clone(),
            None if turns.is_empty() => continue,
            // What the turn named before the items read is not known.
            None if turns.iter().any(|&turn| Some(turn) == cut) => {
                partial = true;
                continue;
            }
            None => BTreeSet::new(),
        };
        named.extend(named_paths(
            turns
                .iter()
                .flat_map(|&turn| entries[turn].items.iter().copied()),
            cwd,
            home,
        ));
        match checkpoint_files(repository, checkpoint, &named).await {
            Ok(files) => {
                for &turn in &turns {
                    claimed[turn] = Some(index);
                }
                read.insert(index, files);
            }
            Err(()) => partial = true,
        }
    }

    // Turns older than the items read first, in the order they ran: saved
    // diffs no item names, and checkpoints none of whose turns are listed.
    let mut diffs = diffs.into_iter().map(Some).collect::<Vec<_>>();
    let mut take_diff = |turn: Option<&str>| {
        let turn = turn.map(ref_component);
        diffs
            .iter_mut()
            .find(|diff| {
                diff.as_ref()
                    .is_some_and(|(_, saved, _)| Some(saved) == turn.as_ref())
            })
            .and_then(Option::take)
            .map(|(_, _, files)| files)
    };
    let mut from_diffs = entries
        .iter()
        .map(|entry| take_diff(entry.turn))
        .collect::<Vec<_>>();
    let listed = claimed.iter().flatten().copied().collect::<HashSet<_>>();
    let mut older = diffs
        .into_iter()
        .flatten()
        .map(|(at_ms, _, files)| (at_ms, files))
        .collect::<Vec<_>>();
    let unlisted = read
        .keys()
        .copied()
        .filter(|index| !listed.contains(index))
        .collect::<Vec<_>>();
    for index in unlisted {
        if let Some(files) = read.remove(&index) {
            older.push((checkpoints[index].at_ms, files));
        }
    }
    older.sort_by_key(|(at_ms, _)| *at_ms);
    let mut turns = older
        .into_iter()
        .map(|(_, files)| files)
        .collect::<Vec<_>>();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(files) = from_diffs[index].take() {
            turns.push(files);
        } else if let Some(checkpoint) = claimed[index] {
            // A checkpoint counts once, at the first turn it covers.
            if let Some(files) = read.remove(&checkpoint) {
                turns.push(files);
            }
        } else {
            turns.push(item_files(entry, repository, cwd, home));
        }
    }
    ThreadChanges {
        changes: union(turns),
        partial,
        earlier: more,
    }
}

#[cfg(test)]
#[path = "thread_changes_tests.rs"]
mod tests;
