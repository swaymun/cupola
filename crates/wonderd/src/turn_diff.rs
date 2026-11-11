//! Codex's whole-turn diff (`turn/diff/updated`) is the authority for what a
//! response edited: it includes shell edits and sub-agent work that per-item
//! `fileChange` rows miss. Codex sends it live only (`thread/read` turns carry
//! no diff), so the latest one is held until `turn/completed` and saved once
//! on the turn. Every turn's latest diff is also kept on disk for the thread's
//! own changes (`filesystem::thread_changes`), including a turn still running
//! or one whose completion never arrived; those writes are coalesced.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Turns in flight across all threads; older entries are dropped first.
const MAX_TURNS: usize = 64;
/// A larger diff is not kept; the phone falls back to the turn's edit rows.
pub(crate) const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

static LATEST: LazyLock<Mutex<VecDeque<(String, String, String)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Turn diffs kept on disk per thread; older turns are dropped first.
const MAX_SAVED_TURNS: usize = 500;

/// Codex resends the whole diff on every edit. Saving waits this long so a
/// burst of updates becomes one write; a crash loses at most this much.
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// Diffs waiting to be saved, by saved directory and turn, with when the
/// turn's diff was first seen; a newer diff for a turn replaces the waiting
/// one. `None` removes a turn's diff (too large to keep).
#[derive(Default)]
struct Pending {
    diffs: HashMap<(PathBuf, String), (u128, Option<String>)>,
    scheduled: bool,
}

static PENDING: LazyLock<Mutex<Pending>> = LazyLock::new(Default::default);
/// Held while taking and writing, so an older diff never lands after a newer.
static WRITING: Mutex<()> = Mutex::new(());

pub(crate) fn record(data_dir: &Path, thread_id: &str, turn_id: &str, diff: &str) {
    let directory = saved_directory(data_dir, thread_id);
    let kept = (diff.len() <= MAX_DIFF_BYTES).then(|| diff.to_owned());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let schedule = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let key = (directory, turn_id.to_owned());
        let first_seen = pending.diffs.get(&key).map_or(now, |(seen, _)| *seen);
        pending.diffs.insert(key, (first_seen, kept));
        !std::mem::replace(&mut pending.scheduled, true)
    };
    // A plain thread, not a runtime task: the save must not depend on which
    // runtime delivered the update, and it blocks on disk.
    if schedule
        && std::thread::Builder::new()
            .name("turn-diff-save".into())
            .spawn(|| {
                std::thread::sleep(SAVE_DELAY);
                flush(None);
            })
            .is_err()
    {
        flush(None);
    }
    let mut latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    latest.retain(|(thread, turn, _)| !(thread == thread_id && turn == turn_id));
    if diff.len() > MAX_DIFF_BYTES {
        return;
    }
    latest.push_back((thread_id.to_owned(), turn_id.to_owned(), diff.to_owned()));
    while latest.len() > MAX_TURNS {
        latest.pop_front();
    }
}

pub(crate) fn take(thread_id: &str, turn_id: &str) -> Option<String> {
    let mut latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    let index = latest
        .iter()
        .position(|(thread, turn, _)| thread == thread_id && turn == turn_id)?;
    latest.remove(index).map(|(_, _, diff)| diff)
}

/// Where a thread's saved turn diffs live: `<first seen ms>-<turn>.diff`, so
/// a listing sorts in turn order.
pub(crate) fn saved_directory(data_dir: &Path, thread_id: &str) -> PathBuf {
    use crate::filesystem::thread_changes::{thread_directory, turns_directory};
    turns_directory(&thread_directory(data_dir, thread_id))
}

/// Writes the waiting diffs: all of them, or one thread's (`directory`).
/// Blocking; call it off the async workers.
fn flush(directory: Option<&Path>) {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let waiting = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if directory.is_none() {
            pending.scheduled = false;
        }
        let keys = pending
            .diffs
            .keys()
            .filter(|(saved, _)| directory.is_none_or(|directory| saved == directory))
            .cloned()
            .collect::<Vec<_>>();
        let mut waiting = keys
            .into_iter()
            .filter_map(|key| pending.diffs.remove_entry(&key))
            .collect::<Vec<_>>();
        waiting.sort_by_key(|(_, (seen, _))| *seen);
        waiting
    };
    for ((saved, turn), (seen, diff)) in waiting {
        save(&saved, &turn, seen, diff.as_deref());
    }
}

/// Test instrument: holds the save lock for `duration`, as a slow disk write
/// would, so a reader's flush waits. Returns once the lock is held.
#[cfg(test)]
pub(crate) fn stall_saves(duration: Duration) -> std::thread::JoinHandle<()> {
    let (held, ready) = std::sync::mpsc::channel();
    let stall = std::thread::spawn(move || {
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        held.send(()).unwrap();
        std::thread::sleep(duration);
    });
    ready.recv().unwrap();
    stall
}

/// Saves a thread's waiting diffs now: its turn ended or was interrupted, so
/// no newer diff is coming to coalesce with.
pub(crate) fn flush_thread(data_dir: &Path, thread_id: &str) {
    flush(Some(&saved_directory(data_dir, thread_id)));
}

/// Replaces the turn's saved diff, keeping its place in turn order. A diff too
/// large to keep (`None`) removes the older one: the turn's edit rows stand in
/// for it.
fn save(directory: &Path, turn_id: &str, first_seen: u128, diff: Option<&str>) {
    let suffix = format!(
        "-{}.diff",
        crate::filesystem::thread_changes::ref_component(turn_id)
    );
    let mut saved = listed_turns(directory);
    let existing = saved
        .iter()
        .position(|path| path.to_string_lossy().ends_with(&suffix));
    let Some(diff) = diff else {
        if let Some(index) = existing {
            let _ = std::fs::remove_file(&saved[index]);
        }
        return;
    };
    let path = match existing {
        Some(index) => saved.remove(index),
        None => directory.join(format!("{first_seen:013}{suffix}")),
    };
    if std::fs::create_dir_all(directory).is_err() {
        return;
    }
    let temporary = path.with_extension("diff.tmp");
    if std::fs::write(&temporary, diff).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
    // `saved` no longer holds this turn: keep room for it.
    while saved.len() + 1 > MAX_SAVED_TURNS {
        let _ = std::fs::remove_file(saved.remove(0));
    }
}

/// A thread's saved turn diffs, oldest turn first, including diffs still
/// waiting to be written.
pub(crate) fn saved_turns(directory: &Path) -> Vec<PathBuf> {
    flush(Some(directory));
    listed_turns(directory)
}

/// What reading one saved diff gave.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SavedRead {
    Text(String),
    /// Longer than the caller's limit; nothing past the limit was read.
    TooLarge,
    /// Not a regular file, not text, or not readable.
    Unreadable,
}

/// Reads one saved diff of at most `limit` bytes. The size is checked before
/// the read and the read itself stops at the limit, so a file that grew or
/// was swapped in cannot be pulled into memory whole. Blocking.
pub(crate) fn read_saved(path: &Path, limit: u64) -> SavedRead {
    use std::io::Read;
    // A pipe or device would block the open; saves only write regular files.
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return SavedRead::Unreadable;
    };
    if !metadata.is_file() {
        return SavedRead::Unreadable;
    }
    if metadata.len() > limit {
        return SavedRead::TooLarge;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return SavedRead::Unreadable;
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if file
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return SavedRead::Unreadable;
    }
    if bytes.len() as u64 > limit {
        return SavedRead::TooLarge;
    }
    String::from_utf8(bytes).map_or(SavedRead::Unreadable, SavedRead::Text)
}

fn listed_turns(directory: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "diff")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

/// A `turnDiff` item whose `changes` match `fileChange` so history counts and
/// sanitizes them the same way. An empty diff still records "no edits".
pub(crate) fn item(turn_id: &str, diff: &str) -> serde_json::Value {
    serde_json::json!({
        "id": format!("turn-diff:{turn_id}"),
        "type": "turnDiff",
        "status": "completed",
        "changes": changes(diff),
    })
}

/// One file of a `git diff`: its path, how it changed and only its hunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileDiff {
    pub(crate) path: String,
    /// `add`, `update` or `delete`, as `fileChange` names them.
    pub(crate) kind: &'static str,
    pub(crate) hunks: String,
    pub(crate) binary: bool,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
}

/// Split a `git diff` into one change per file, keeping only its hunks.
fn changes(diff: &str) -> Vec<serde_json::Value> {
    files(diff)
        .into_iter()
        .map(|file| {
            serde_json::json!({
                "path": file.path, "kind": {"type": file.kind}, "diff": file.hunks,
            })
        })
        .collect()
}

/// Split a `git diff` into its files, with line counts from their hunks.
pub(crate) fn files(diff: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut current: Option<(String, &'static str, Vec<&str>, bool)> = None;
    let mut finish = |file: Option<(String, &'static str, Vec<&str>, bool)>| {
        if let Some((path, kind, hunks, binary)) = file {
            if !path.is_empty() {
                let count =
                    |sign: char| hunks.iter().filter(|line| line.starts_with(sign)).count() as u64;
                files.push(FileDiff {
                    additions: count('+'),
                    deletions: count('-'),
                    path,
                    kind,
                    hunks: hunks.join("\n"),
                    binary,
                });
            }
        }
    };
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            finish(current.take());
            current = Some((header_path(header), "update", Vec::new(), false));
            continue;
        }
        let Some((path, kind, hunks, binary)) = current.as_mut() else {
            continue;
        };
        if !hunks.is_empty() || line.starts_with("@@") {
            hunks.push(line);
        } else if line.starts_with("new file mode") {
            *kind = "add";
        } else if line.starts_with("deleted file mode") {
            *kind = "delete";
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            *binary = true;
        } else if let Some(target) = line.strip_prefix("+++ b/") {
            *path = target.to_owned();
        } else if let Some(source) = line.strip_prefix("--- a/") {
            if *kind == "delete" {
                *path = source.to_owned();
            }
        } else if let Some(target) = line.strip_prefix("rename to ") {
            *path = target.to_owned();
        }
    }
    finish(current.take());
    files
}

/// `a/x b/x` → `x`; quoted or spaced paths are corrected by the `+++`/`---` lines.
fn header_path(header: &str) -> String {
    header
        .rsplit_once(" b/")
        .map(|(_, path)| path.to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_turn_diff_into_files_with_their_hunks() {
        let diff = "diff --git a/src/app.js b/src/app.js\nindex 1..2 100644\n--- a/src/app.js\n+++ b/src/app.js\n@@ -1,3 +1,4 @@\n line\n-old\n+new\n+more\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+a\n+b\ndiff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\ndiff --git a/logo.png b/logo.png\nnew file mode 100644\nBinary files /dev/null and b/logo.png differ\n";
        let files = changes(diff);
        let summary = files
            .iter()
            .map(|f| {
                (
                    f["path"].as_str().unwrap(),
                    f["kind"]["type"].as_str().unwrap(),
                    f["diff"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                (
                    "src/app.js",
                    "update",
                    "@@ -1,3 +1,4 @@\n line\n-old\n+new\n+more"
                ),
                ("new.txt", "add", "@@ -0,0 +1,2 @@\n+a\n+b"),
                ("gone.txt", "delete", "@@ -1 +0,0 @@\n-x"),
                ("logo.png", "add", ""),
            ]
        );
    }

    #[test]
    fn keeps_only_the_latest_diff_per_turn_and_takes_it_once() {
        let data = tempfile::tempdir().unwrap();
        record(data.path(), "thread-td", "turn-td", "first");
        record(data.path(), "thread-td", "turn-td", "second");
        assert_eq!(take("thread-td", "turn-td").as_deref(), Some("second"));
        assert_eq!(take("thread-td", "turn-td"), None);
        record(
            data.path(),
            "thread-td",
            "too-big",
            &"x".repeat(MAX_DIFF_BYTES + 1),
        );
        assert_eq!(take("thread-td", "too-big"), None);
    }

    #[test]
    fn every_turn_diff_is_saved_in_turn_order_and_survives_the_memory_copy() {
        let data = tempfile::tempdir().unwrap();
        record(data.path(), "thread-save", "turn-1", "one");
        std::thread::sleep(std::time::Duration::from_millis(2));
        record(data.path(), "thread-save", "turn-2", "two");
        // A later update replaces the turn's diff in place, keeping its order.
        record(data.path(), "thread-save", "turn-1", "one, updated");
        // Taken at completion (or lost with a restart) the disk copy remains.
        assert!(take("thread-save", "turn-1").is_some());
        let saved = saved_turns(&saved_directory(data.path(), "thread-save"));
        let contents = saved
            .iter()
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(contents, ["one, updated", "two"]);
        // A diff too large to keep drops the older copy.
        record(
            data.path(),
            "thread-save",
            "turn-2",
            &"x".repeat(MAX_DIFF_BYTES + 1),
        );
        assert_eq!(
            saved_turns(&saved_directory(data.path(), "thread-save")).len(),
            1
        );
        // Another thread's diffs are separate.
        assert!(saved_turns(&saved_directory(data.path(), "thread-other")).is_empty());
    }

    /// Codex resends the whole diff on every edit: a burst is written once,
    /// shortly after, or at once when the turn ends or the diffs are read.
    #[test]
    fn turn_diff_updates_are_coalesced_into_one_write() {
        let data = tempfile::tempdir().unwrap();
        let directory = saved_directory(data.path(), "thread-burst");
        for update in ["one", "one two", "one two three"] {
            record(data.path(), "thread-burst", "turn-burst", update);
        }
        assert!(
            listed_turns(&directory).is_empty(),
            "written before the burst ended"
        );
        flush_thread(data.path(), "thread-burst");
        let saved = listed_turns(&directory);
        assert_eq!(saved.len(), 1);
        assert_eq!(std::fs::read_to_string(&saved[0]).unwrap(), "one two three");
        // Without a reader or a turn end, the delayed save still lands.
        record(data.path(), "thread-burst", "turn-later", "later");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while listed_turns(&directory).len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the delayed save never ran"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        take("thread-burst", "turn-burst");
        take("thread-burst", "turn-later");
    }

    #[test]
    fn a_saved_diff_is_read_only_up_to_its_limit() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("0000000000001-turn.diff");
        std::fs::write(&path, "twelve bytes").unwrap();
        assert_eq!(
            read_saved(&path, 12),
            SavedRead::Text("twelve bytes".into())
        );
        assert_eq!(read_saved(&path, 11), SavedRead::TooLarge);
        assert_eq!(
            read_saved(&path.with_extension("none"), 12),
            SavedRead::Unreadable
        );
        // Not text, and not a file at all.
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(read_saved(&path, 12), SavedRead::Unreadable);
        assert_eq!(read_saved(data.path(), 12), SavedRead::Unreadable);
    }

    #[test]
    fn files_count_lines_and_mark_binaries() {
        let diff = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n same\n-old\n+new\n+more\ndiff --git a/logo.png b/logo.png\nindex 1..2 100644\nBinary files a/logo.png and b/logo.png differ\n";
        let files = files(diff);
        assert_eq!(
            (files[0].additions, files[0].deletions, files[0].binary),
            (2, 1, false)
        );
        assert_eq!(
            (files[1].path.as_str(), files[1].binary),
            ("logo.png", true)
        );
    }
}
