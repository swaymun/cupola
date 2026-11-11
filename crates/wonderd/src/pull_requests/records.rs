//! The pull requests a thread opened, from its own saved thread items (every
//! runtime saves its tool calls in the same item shapes): the addresses
//! `gh pr create` or a pull request tool printed, and the branches `git push`
//! reported, resolved to their pull requests through `gh`. A link the agent
//! only mentioned, viewed or merged is not counted.
//!
//! History is read incrementally from a remembered offset, so a thread with
//! hundreds of pull requests and tens of thousands of items costs one pass and
//! then only its new rows. Branch lookups run in the background, a bounded
//! number per pass, and are cached; a read never waits for GitHub.

use super::{run_gh, valid_branch, PullRef};
use crate::filesystem::Published;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Rows found per read (the rest wait for the next read), and rows whose
/// text is loaded at once.
const MAX_SCAN_ROWS: u32 = 5_000;
const SCAN_PAGE: usize = 200;
/// Records kept per thread.
const MAX_OPENED: usize = 2_000;
const MAX_PUSHED: usize = 1_000;
const MAX_THREADS: usize = 200;
/// Per background pass: repositories listed in one call each, then single
/// branches looked up on their own.
const MAX_REPOSITORY_LISTS: usize = 4;
const MAX_BRANCH_LOOKUPS: usize = 4;
/// Pull requests one repository listing covers, newest first.
const LIST_LIMIT: &str = "200";
/// A branch without a pull request is looked up again after this.
const RETRY_UNRESOLVED: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Thread {
    revision: i64,
    after: i64,
    /// Opened pull requests with the history sequence that recorded them.
    opened: Vec<(PullRef, i64)>,
    pushed: Vec<Push>,
    used: Option<Instant>,
}

struct Push {
    slug: String,
    branch: String,
    sequence: i64,
    /// Pull requests found for the branch; empty until one is.
    found: Vec<PullRef>,
    tried: Option<Instant>,
}

impl Push {
    fn due(&self) -> bool {
        self.found.is_empty() && self.tried.is_none_or(|at| at.elapsed() >= RETRY_UNRESOLVED)
    }
}

static THREADS: LazyLock<Mutex<HashMap<String, Thread>>> = LazyLock::new(Default::default);
/// Threads with a branch lookup pass in flight.
static RESOLVING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// A thread's opened pull requests, newest first.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Opened {
    pub(super) refs: Vec<PullRef>,
    /// Pushed branches still wait for a lookup.
    pub(super) unresolved: bool,
}

/// Reads the thread's new history rows and answers its opened pull requests.
pub(super) async fn opened(
    store: &wonder_store::Store,
    conversation: &str,
) -> Result<Opened, sqlx::Error> {
    for attempt in 0..2 {
        let after = threads().get(conversation).map_or(0, |thread| thread.after);
        let (revision, sequences) = store
            .conversation_pull_request_records(conversation, after, MAX_SCAN_ROWS)
            .await?;
        {
            let mut threads = threads();
            if threads
                .get(conversation)
                .is_some_and(|thread| thread.revision != revision)
            {
                // A native reset hid rows: start over from the beginning.
                threads.remove(conversation);
                if attempt == 0 && after > 0 {
                    continue;
                }
            }
            if threads.len() >= MAX_THREADS && !threads.contains_key(conversation) {
                evict(&mut threads);
            }
            threads
                .entry(conversation.to_owned())
                .or_insert_with(|| Thread {
                    revision,
                    ..Default::default()
                });
        }
        for chunk in sequences.chunks(SCAN_PAGE) {
            let rows = store.history_event_details(conversation, chunk).await?;
            // Parsing shell text is CPU work: keep it off the async workers.
            let rows: Vec<(i64, Published)> = tokio::task::spawn_blocking(move || {
                rows.into_iter()
                    .map(|(sequence, detail)| (sequence, read(&detail)))
                    .collect()
            })
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            let mut threads = threads();
            let Some(thread) = threads
                .get_mut(conversation)
                .filter(|thread| thread.revision == revision)
            else {
                break;
            };
            for (sequence, published) in rows {
                // A read running alongside may have recorded these already.
                if sequence > thread.after {
                    thread.after = sequence;
                    record(thread, sequence, published);
                }
            }
            // A row hidden between the two queries is not asked for again.
            if let Some(last) = chunk.last() {
                thread.after = thread.after.max(*last);
            }
        }
        break;
    }
    let mut threads = threads();
    let thread = threads.entry(conversation.to_owned()).or_default();
    thread.used = Some(Instant::now());
    Ok(answer(thread))
}

fn threads() -> std::sync::MutexGuard<'static, HashMap<String, Thread>> {
    THREADS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drops the least recently read threads.
fn evict(threads: &mut HashMap<String, Thread>) {
    let mut used: Vec<_> = threads
        .iter()
        .map(|(id, thread)| (thread.used, id.clone()))
        .collect();
    used.sort();
    for (_, id) in used.into_iter().take(MAX_THREADS / 4) {
        threads.remove(&id);
    }
}

/// What one saved item event's detail says the thread published.
fn read(detail: &str) -> Published {
    serde_json::from_str::<Value>(detail)
        .ok()
        .as_ref()
        .and_then(|detail| detail.get("item"))
        .filter(|item| item.is_object())
        .map(crate::filesystem::published)
        .unwrap_or_default()
}

#[cfg(all(test, unix))]
fn apply(thread: &mut Thread, sequence: i64, detail: &str) {
    record(thread, sequence, read(detail));
}

fn record(thread: &mut Thread, sequence: i64, published: Published) {
    for address in published.opened {
        let reference = PullRef {
            owner: address.owner,
            repo: address.repo,
            number: address.number,
        };
        if thread.opened.len() < MAX_OPENED
            && !thread.opened.iter().any(|(known, _)| *known == reference)
        {
            thread.opened.push((reference, sequence));
        }
    }
    for (slug, branch) in published.pushed {
        match thread
            .pushed
            .iter()
            .position(|push| push.slug == slug && push.branch == branch)
        {
            // Pushed again: newer work, and worth another look.
            Some(index) => {
                thread.pushed[index].sequence = sequence;
                thread.pushed[index].tried = None;
            }
            None if thread.pushed.len() < MAX_PUSHED => thread.pushed.push(Push {
                slug,
                branch,
                sequence,
                found: Vec::new(),
                tried: None,
            }),
            None => {}
        }
    }
}

/// Opened and branch pull requests, newest record first, each once.
fn answer(thread: &Thread) -> Opened {
    let mut dated: Vec<(i64, &PullRef)> = thread
        .opened
        .iter()
        .map(|(reference, sequence)| (*sequence, reference))
        .chain(thread.pushed.iter().flat_map(|push| {
            push.found
                .iter()
                .map(move |reference| (push.sequence, reference))
        }))
        .collect();
    dated.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut refs: Vec<PullRef> = Vec::new();
    for (_, reference) in dated {
        if !refs.contains(reference) {
            refs.push(reference.clone());
        }
    }
    Opened {
        refs,
        unresolved: thread.pushed.iter().any(Push::due),
    }
}

/// Starts one bounded pass of branch lookups for the thread, unless one is
/// already running. The next read includes what it finds.
pub(super) fn resolve_in_background(gh: PathBuf, conversation: String) {
    if !RESOLVING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation.clone())
    {
        return;
    }
    tokio::spawn(async move {
        resolve(&gh, &conversation).await;
        RESOLVING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conversation);
    });
}

/// Lists each repository's newest pull requests once and matches the pushed
/// branches by name, then looks up a few remaining branches on their own.
pub(super) async fn resolve(gh: &std::path::Path, conversation: &str) {
    let due: Vec<(String, String)> = {
        let threads = threads();
        let Some(thread) = threads.get(conversation) else {
            return;
        };
        let mut due: Vec<&Push> = thread.pushed.iter().filter(|push| push.due()).collect();
        due.sort_by_key(|push| std::cmp::Reverse(push.sequence));
        due.into_iter()
            .map(|push| (push.slug.clone(), push.branch.clone()))
            .collect()
    };
    let mut slugs: Vec<&str> = Vec::new();
    for (slug, _) in &due {
        if !slugs.contains(&slug.as_str()) && slugs.len() < MAX_REPOSITORY_LISTS {
            slugs.push(slug);
        }
    }
    let mut found: HashMap<(String, String), Vec<PullRef>> = HashMap::new();
    let mut listed: HashSet<&str> = HashSet::new();
    for slug in slugs {
        let Some(rows) = list(gh, &["-R", slug, "--limit", LIST_LIMIT]).await else {
            continue;
        };
        listed.insert(slug);
        for (head, reference) in rows {
            found
                .entry((slug.to_owned(), head))
                .or_default()
                .push(reference);
        }
    }
    let mut lookups = 0;
    for (slug, branch) in &due {
        let key = (slug.clone(), branch.clone());
        if found.contains_key(&key) || lookups >= MAX_BRANCH_LOOKUPS {
            continue;
        }
        lookups += 1;
        if let Some(rows) = list(gh, &["-R", slug, "--head", branch, "--limit", "3"]).await {
            listed.insert(slug);
            let refs: Vec<PullRef> = rows.into_iter().map(|(_, reference)| reference).collect();
            if !refs.is_empty() {
                found.insert(key, refs);
            }
        }
    }
    let mut threads = threads();
    let Some(thread) = threads.get_mut(conversation) else {
        return;
    };
    for push in thread.pushed.iter_mut().filter(|push| push.due()) {
        match found.remove(&(push.slug.clone(), push.branch.clone())) {
            Some(refs) => push.found = refs,
            // Only a repository GitHub answered for says the branch has none.
            None if listed.contains(push.slug.as_str()) => push.tried = Some(Instant::now()),
            None => {}
        }
    }
}

/// `gh pr list` rows as (head branch, pull request), or None when it failed.
async fn list(gh: &std::path::Path, args: &[&str]) -> Option<Vec<(String, PullRef)>> {
    if !args
        .iter()
        .all(|arg| valid_branch(arg) || arg.starts_with('-'))
    {
        return None;
    }
    let mut command = vec![
        "pr",
        "list",
        "--state",
        "all",
        "--json",
        "number,url,headRefName",
    ];
    command.extend_from_slice(args);
    let bytes = run_gh(gh, None, &command).await.ok()?;
    let rows: Vec<Value> = serde_json::from_slice(&bytes).ok()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let head = row.get("headRefName")?.as_str()?;
                let reference = row
                    .get("url")?
                    .as_str()?
                    .strip_prefix("https://github.com/")
                    .and_then(super::parse_ref)?;
                (row.get("number")?.as_u64()? == reference.number)
                    .then(|| (head.to_owned(), reference))
            })
            .collect(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail(command: &str, output: &str) -> String {
        json!({"item":{"type":"commandExecution","command":command,"output":output}}).to_string()
    }

    fn urls(opened: &Opened) -> Vec<String> {
        opened.refs.iter().map(PullRef::url).collect()
    }

    // The done check for which pull requests a thread opened: what
    // `gh pr create` printed counts; links the agent named, viewed or merged
    // do not; pushed branches wait for a lookup; newest first.
    #[test]
    fn opened_pull_requests_come_from_create_output_not_mentions() {
        let mut thread = Thread::default();
        let rows = [
            (1, detail(
                "git push -u origin agent/a && gh pr create --fill",
                "To https://github.com/o/r.git\n * [new branch]      agent/a -> agent/a\nbranch 'agent/a' set up to track 'origin/agent/a'.\nhttps://github.com/o/r/pull/5\n",
            )),
            // Viewing, merging and quoting are not opening.
            (2, detail("gh pr view https://github.com/o/r/pull/3", "https://github.com/o/r/pull/3 OPEN")),
            (3, detail("gh pr merge 5 --squash", "Merged https://github.com/o/r/pull/5")),
            (4, detail("rg 'gh pr create' notes.md", "opened https://github.com/o/r/pull/9")),
            (5, detail("echo https://github.com/o/r/pull/8", "https://github.com/o/r/pull/8")),
            // The body names an earlier pull request; only the printed one counts.
            (6, detail(
                "gh pr create --title T --body 'Follows https://github.com/o/r/pull/5' && gh pr view https://github.com/o/r/pull/4 --json title",
                "https://github.com/o/r/pull/6\nhttps://github.com/o/r/pull/4 Earlier work\n",
            )),
            (7, detail("git push origin HEAD:agent/b main", "To github.com:o/r.git\n   1a2b3c4..5d6e7f8  HEAD -> agent/b\n   1a2b3c4..5d6e7f8  main -> main\n ! [rejected]        agent/c -> agent/c (fetch first)\n")),
            (8, json!({"item":{"type":"mcpToolCall","tool":"create_pull_request","result":{"content":[{"type":"text","text":"https://github.com/o/other/pull/2"}]}}}).to_string()),
        ];
        for (sequence, row) in &rows {
            apply(&mut thread, *sequence, row);
        }
        let opened = answer(&thread);
        assert_eq!(
            urls(&opened),
            [
                "https://github.com/o/other/pull/2",
                "https://github.com/o/r/pull/6",
                "https://github.com/o/r/pull/5",
            ]
        );
        let pushed: Vec<_> = thread
            .pushed
            .iter()
            .map(|push| (push.slug.as_str(), push.branch.as_str(), push.sequence))
            .collect();
        assert_eq!(pushed, [("o/r", "agent/a", 1), ("o/r", "agent/b", 7)]);
        assert!(opened.unresolved);
    }

    /// A stand-in `gh` whose repository listing knows branch agent/b and
    /// whose single-branch lookup knows agent/c.
    fn fake_gh(dir: &std::path::Path) -> PathBuf {
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/calls"
case "$*" in
  *--head\ agent/c*) echo '[{"number":11,"url":"https://github.com/o/r/pull/11","headRefName":"agent/c"}]' ;;
  *--head*) echo '[]' ;;
  *) echo '[{"number":10,"url":"https://github.com/o/r/pull/10","headRefName":"agent/b"},{"number":4,"url":"https://github.com/o/r/pull/4","headRefName":"other"}]' ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    fn push_rows(thread: &mut Thread, branches: &[&str], first: i64) {
        for (offset, branch) in branches.iter().enumerate() {
            let output = format!(
                "To https://github.com/o/r.git\n * [new branch]      {branch} -> {branch}\n"
            );
            apply(
                thread,
                first + offset as i64,
                &detail(&format!("git push -u origin {branch}"), &output),
            );
        }
    }

    // Pushed branches resolve through one repository listing, then a few
    // single lookups; a branch with no pull request is not asked about again
    // until the retry interval passes.
    #[tokio::test]
    async fn pushed_branches_resolve_in_bounded_cached_passes() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let conversation = "records-resolve-test";
        let mut thread = Thread::default();
        apply(
            &mut thread,
            1,
            &detail("gh pr create", "https://github.com/o/r/pull/3"),
        );
        push_rows(
            &mut thread,
            &[
                "agent/b", "agent/d", "agent/e", "agent/f", "agent/g", "agent/c",
            ],
            2,
        );
        threads().insert(conversation.into(), thread);

        resolve(&gh, conversation).await;
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(calls.lines().count(), 1 + MAX_BRANCH_LOOKUPS, "{calls}");
        assert_eq!(
            calls.lines().filter(|l| l.contains("--head")).count(),
            MAX_BRANCH_LOOKUPS
        );
        let opened = answer(&threads()[conversation]);
        assert_eq!(
            urls(&opened),
            [
                "https://github.com/o/r/pull/11",
                "https://github.com/o/r/pull/10",
                "https://github.com/o/r/pull/3"
            ]
        );
        // The listing found agent/b. Newest first, agent/c was looked up and
        // found; agent/g, agent/f and agent/e have none, and agent/d was
        // covered by the listing. None is due again until the retry interval.
        assert!(!opened.unresolved);
        std::fs::remove_file(dir.path().join("calls")).unwrap();
        resolve(&gh, conversation).await;
        assert!(!dir.path().join("calls").exists(), "nothing was due");
        threads().remove(conversation);
    }

    async fn save(store: &wonder_store::Store, index: usize, item: Value) {
        let detail = crate::thread_item_upsert_detail("turn", &item, "completed", None).unwrap();
        let mut event = wonder_api::HostEventEnvelope {
            event_id: format!("event-{index}"),
            host_epoch: "epoch".into(),
            sequence: 0,
            occurred_at: format!("2026-10-13T00:00:0{index}Z"),
            request_id: None,
            device_id: None,
            conversation_id: Some("chat".into()),
            message_id: None,
            thread_id: Some("thread".into()),
            turn_id: Some("turn".into()),
            item_id: item.get("id").and_then(Value::as_str).map(str::to_owned),
            approval_id: None,
            event: wonder_api::WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(detail),
            },
        };
        store.commit_event(&mut event).await.unwrap();
    }

    // Items saved the way every runtime's items are saved are read once, then
    // only the rows saved since; a reply that names a pull request adds none.
    #[tokio::test]
    async fn saved_history_is_read_from_a_remembered_offset() {
        let store = wonder_store::Store::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::test_support::project_thread(
            &store,
            "chat",
            "Project",
            "/w",
            "fixture-store",
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        store.start_event_epoch("epoch").await.unwrap();
        let conversation = "chat";
        threads().remove(conversation);
        save(
            &store,
            0,
            json!({"id":"c1","type":"commandExecution","status":"completed",
            "command":"/bin/zsh -lc 'gh pr create --fill'",
            "aggregatedOutput":"https://github.com/o/r/pull/1\n"}),
        )
        .await;
        assert_eq!(
            urls(&opened(&store, conversation).await.unwrap()),
            ["https://github.com/o/r/pull/1"]
        );
        let after = threads()[conversation].after;
        assert!(after > 0);

        save(&store, 1, json!({"id":"c2","type":"commandExecution","status":"completed",
            "command":"git push -u origin agent/x && gh pr create --fill",
            "aggregatedOutput":"To https://github.com/o/r.git\n * [new branch]      agent/x -> agent/x\nhttps://github.com/o/r/pull/2\n"})).await;
        save(&store, 2, json!({"id":"m1","type":"agentMessage","text":"I will git push; see https://github.com/o/r/pull/3 and gh pr create"})).await;
        let second = opened(&store, conversation).await.unwrap();
        assert_eq!(
            urls(&second),
            [
                "https://github.com/o/r/pull/2",
                "https://github.com/o/r/pull/1"
            ]
        );
        assert!(second.unresolved, "agent/x waits for a lookup");
        assert!(threads()[conversation].after > after);
        threads().remove(conversation);
    }

    // A thread that opened 200 pull requests and pushed 200 branches is read
    // in full, newest first, with every pull request once.
    #[test]
    fn large_threads_are_read_whole_and_bounded() {
        let mut thread = Thread::default();
        for number in 1..=200 {
            let branch = format!("agent/b{number}");
            let output = format!("To https://github.com/o/r.git\n * [new branch]      {branch} -> {branch}\nhttps://github.com/o/r/pull/{number}\n");
            apply(
                &mut thread,
                number as i64,
                &detail(
                    &format!("git push -u origin {branch} && gh pr create --fill"),
                    &output,
                ),
            );
        }
        let opened = answer(&thread);
        assert_eq!(opened.refs.len(), 200);
        assert_eq!(opened.refs.first().map(|r| r.number), Some(200));
        assert_eq!(opened.refs.last().map(|r| r.number), Some(1));
        for number in 201..=(MAX_OPENED as u64 + 50) {
            apply(
                &mut thread,
                number as i64,
                &detail(
                    "gh pr create",
                    &format!("https://github.com/o/r/pull/{number}"),
                ),
            );
        }
        assert_eq!(thread.opened.len(), MAX_OPENED);
    }
}
