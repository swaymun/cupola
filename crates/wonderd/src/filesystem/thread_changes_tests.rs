//! A thread's own changes against temporary repositories: checkpoints never
//! touch the owner's index, working tree, branch or HEAD, and the union keeps
//! only what the thread's tool calls named.

use super::*;
use serde_json::json;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = crate::test_git::git()
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with one commit, a staged change, an unstaged change and an
/// untracked file: everything a checkpoint could disturb.
fn busy_repository(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    for (name, text) in [
        ("app.js", "one\n"),
        ("other.txt", "other\n"),
        ("keep.txt", "keep\n"),
        ("staged.txt", "base\n"),
    ] {
        fs::write(dir.join(name), text).unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "first"]);
    fs::write(dir.join("staged.txt"), "staged by the owner\n").unwrap();
    git(dir, &["add", "staged.txt"]);
    fs::write(dir.join("staged.txt"), "staged, then edited again\n").unwrap();
    fs::write(dir.join("draft.txt"), "the owner's untracked draft\n").unwrap();
    crate::canonical_path::canonicalize(dir).unwrap()
}

/// What the owner sees of their repository: index bytes, status, branches,
/// HEAD and every working file.
#[derive(Debug, PartialEq)]
struct OwnerView(Vec<u8>, String, String, String, Vec<(String, Vec<u8>)>);

fn owner_view(dir: &Path) -> OwnerView {
    let mut files = fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            (name != ".git").then(|| (name, fs::read(entry.path()).unwrap()))
        })
        .collect::<Vec<_>>();
    files.sort();
    OwnerView(
        fs::read(dir.join(".git/index")).unwrap(),
        git(dir, &["status", "--porcelain=v1", "--untracked-files=all"]),
        git(
            dir,
            &["for-each-ref", "refs/heads", "refs/tags", "refs/remotes"],
        ),
        git(dir, &["rev-parse", "HEAD"]),
        files,
    )
}

#[tokio::test]
async fn checkpoints_never_touch_the_owners_index_or_working_tree() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let directory = scratch.path().join("data");
    let before = owner_view(&repo);
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("turn-1"), &repo).await;
    assert_eq!(
        owner_view(&repo),
        before,
        "a start checkpoint changed the repository"
    );

    fs::write(repo.join("app.js"), "two\n").unwrap();
    let during = owner_view(&repo);
    step(&directory, "thread", ended("turn-1"), &repo).await;
    assert_eq!(
        owner_view(&repo),
        during,
        "an end checkpoint changed the repository"
    );

    // Both boundaries sit under hidden refs, and the start snapshot holds the
    // working tree, the owner's untracked draft and unstaged edit included.
    let saved = load(&directory).checkpoints;
    assert_eq!(saved.len(), 1);
    let checkpoint = &saved[0];
    let refs = git(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            REF_ROOT,
        ],
    );
    let prefix = format!("{REF_ROOT}/{}/turn-1", key("thread"));
    assert!(
        refs.contains(&format!("{prefix}/start {}", checkpoint.start)),
        "{refs}"
    );
    assert!(
        refs.contains(&format!(
            "{prefix}/end {}",
            checkpoint.end.as_deref().unwrap()
        )),
        "{refs}"
    );
    assert_eq!(
        git(
            &repo,
            &["show", &format!("{}:staged.txt", checkpoint.start)]
        ),
        "staged, then edited again\n"
    );
    assert_eq!(
        git(&repo, &["show", &format!("{}:draft.txt", checkpoint.start)]),
        "the owner's untracked draft\n"
    );
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{}^", checkpoint.start)]),
        git(&repo, &["rev-parse", "HEAD"])
    );
    // An end with no start in this repository records nothing, and a
    // replayed start or end leaves the finished turn as it was.
    step(&directory, "thread", ended("never-started"), &repo).await;
    fs::write(repo.join("app.js"), "three\n").unwrap();
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("turn-1"), &repo).await;
    step(&directory, "thread", ended("turn-1"), &repo).await;
    assert_eq!(load(&directory).checkpoints, saved);
    // A turn with no baseline taken before it was sent (one started on the
    // Mac) is not checkpointed, and the baseline left by that replay is
    // dropped by a send that started nothing.
    step(&directory, "thread", Step::Unsent, &repo).await;
    step(&directory, "thread", began("from-the-mac"), &repo).await;
    assert_eq!(load(&directory).checkpoints, saved);
    assert!(load(&directory).baseline.is_none());
    assert_eq!(git(&repo, &["for-each-ref", BASELINE_ROOT]), "");
}

/// Takes one step for a send that is still waiting for it.
async fn step(directory: &Path, thread: &str, step: Step, repo: &Path) {
    record(directory, thread, &step, repo, &|| true).await;
}

fn began(turn: &str) -> Step {
    Step::Began(turn.into())
}

fn ended(turn: &str) -> Step {
    Step::Ended(turn.into())
}

#[tokio::test]
async fn a_repository_whose_own_filter_would_run_is_not_checkpointed() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let marker = scratch.path().join("filter-ran");
    // Git's shell on Windows takes `/` but drops `\`.
    let shown = marker.display().to_string().replace('\\', "/");
    git(
        &repo,
        &[
            "config",
            "filter.agent.clean",
            &format!("touch {shown} && cat"),
        ],
    );
    fs::write(repo.join(".gitattributes"), "* filter=agent\n").unwrap();
    let directory = scratch.path().join("data");
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("turn"), &repo).await;
    assert!(
        !marker.exists(),
        "a checkpoint ran the repository's own filter"
    );
    assert!(load(&directory).checkpoints.is_empty());
}

// An agent can write a nested repository's own config (inside the Project, so
// inside any workspace-write sandbox). Once the owner's index holds a gitlink
// for it, staging must not load that config and run its filter program. The
// marker sits inside the repository: the write set a confined filter has.
// Unix only: it needs a recorded checkpoint, which MinGit under Wine cannot
// take (the sibling checkpoint tests are Wine known failures); the staging code
// is platform-neutral and runs on the Linux and macOS lands.
#[cfg(unix)]
#[tokio::test]
async fn a_checkpoint_never_runs_a_nested_repositorys_filter() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let nested = repo.join("vendor/lib");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q", "-b", "main"]);
    fs::write(nested.join("file"), "lib\n").unwrap();
    fs::write(nested.join(".gitattributes"), "* filter=agent\n").unwrap();
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-q", "-m", "lib"]);
    git(&repo, &["add", "vendor/lib"]);
    assert!(
        git(&repo, &["ls-files", "--stage", "vendor/lib"]).starts_with("160000 "),
        "the fixture needs an indexed gitlink"
    );
    // Install the program only after setup, so a marker proves a checkpoint ran it.
    let marker = repo.join("filter-ran");
    let shown = marker.display().to_string().replace('\\', "/");
    git(
        &nested,
        &[
            "config",
            "filter.agent.clean",
            &format!("touch {shown} && cat"),
        ],
    );
    fs::write(nested.join("file"), "changed by the agent\n").unwrap();
    fs::write(repo.join("app.js"), "agent edit\n").unwrap();

    let directory = scratch.path().join("data");
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("turn"), &repo).await;
    assert!(
        !marker.exists(),
        "a checkpoint ran a nested repository's filter"
    );
    // The checkpoint is still taken, and still holds the parent's own files.
    let refs = git(
        &repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            REF_ROOT,
            BASELINE_ROOT,
        ],
    );
    let start = refs.lines().next().expect("no checkpoint was taken");
    assert_eq!(
        git(&repo, &["show", &format!("{start}:app.js")]),
        "agent edit\n"
    );
}

fn file_change(path: &str, kind: &str, diff: &str) -> Value {
    json!({"type": "fileChange", "changes": [{"path": path, "kind": {"type": kind}, "diff": diff}]})
}

#[test]
fn the_union_joins_turns_per_path_and_drops_what_the_thread_undid() {
    let file = |path: &str, kind: &'static str, hunks: &str, add: u64, del: u64| TurnFile {
        path: path.into(),
        kind,
        hunks: Some(hunks.into()),
        binary: false,
        additions: Some(add),
        deletions: Some(del),
    };
    let changes = union(vec![
        vec![
            file("made.txt", "add", "@@ -0,0 +1 @@\n+a", 1, 0),
            file("scratch.txt", "add", "@@ -0,0 +1 @@\n+tmp", 1, 0),
        ],
        vec![
            file("made.txt", "update", "@@ -1 +1,2 @@\n a\n+b", 1, 0),
            file("scratch.txt", "delete", "@@ -1 +0,0 @@\n-tmp", 0, 1),
            file("gone.txt", "delete", "@@ -1 +0,0 @@\n-x", 0, 1),
        ],
    ]);
    assert_eq!(
        changes,
        [
            ThreadChange {
                path: "gone.txt".into(),
                status: "deleted",
                binary: false,
                additions: Some(0),
                deletions: Some(1),
                diff: Some("@@ -1 +0,0 @@\n-x".into()),
            },
            ThreadChange {
                path: "made.txt".into(),
                status: "added",
                binary: false,
                additions: Some(2),
                deletions: Some(0),
                diff: Some("@@ -0,0 +1 @@\n+a\n@@ -1 +1,2 @@\n a\n+b".into()),
            },
        ]
    );
}

/// An absolute folder on this platform that the tests never touch.
fn work_repo() -> &'static Path {
    Path::new(if cfg!(windows) {
        r"C:\work\repo"
    } else {
        "/work/repo"
    })
}

#[test]
fn tool_calls_name_files_folders_and_redirect_targets() {
    let cwd = work_repo();
    let items: Vec<(Option<String>, Value)> = vec![
        (None, file_change("src/app.js", "update", "")),
        (
            None,
            json!({"type": "commandExecution",
                "command": "cd web && echo hi >notes.txt; rm -rf build/ && sed -i s/a/b/ --in-place=x ../README 2>>err.log"}),
        ),
        (
            None,
            json!({"type": "mcpToolCall", "toolAction": {"kind": "edit", "paths": [cwd.join("book.ipynb")]}}),
        ),
    ];
    let named = named_paths(items.iter().map(|(_, item)| item), cwd, None);
    for path in [
        "src/app.js",
        "web/notes.txt",
        "web/build",
        "README",
        "web/err.log",
        "book.ipynb",
    ] {
        assert!(named.contains(&cwd.join(path)), "{path} in {named:?}");
    }
    let repository = cwd;
    assert!(was_named(&named, repository, "web/build/out/app.js"));
    assert!(!was_named(&named, repository, "web/other.txt"));
    // `cd web` names the folder only as a working folder, and the
    // repository itself never stands for everything in it.
    let everything = named_paths(
        &[json!({"type": "commandExecution", "command": "git add ."})],
        cwd,
        None,
    );
    assert!(!was_named(&everything, repository, "src/app.js"));
}

/// Waits for the thread's lane to finish its queued boundaries.
#[cfg(unix)]
async fn settled(thread: &str) {
    for _ in 0..400 {
        if !LANES.lock().unwrap().contains_key(thread) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("checkpoints for {thread} never finished");
}

/// A Project thread in `repo` bound to `thread`, routed to the fixture's
/// stand-in runtime when `family` is Codex.
#[cfg(unix)]
async fn project_thread(
    state: &AppState,
    fixture: &Path,
    conversation: &str,
    family: wonder_store::AgentFamily,
    thread: &str,
    repo: &Path,
) {
    use wonder_store::{ProjectConversationInsert, ProjectRootInput};
    let wonder_store::ProjectCreate::Created(project) = state
        .store
        .create_project(
            &format!("project-{conversation}"),
            &format!("request-{conversation}"),
            "hash",
            "Changes",
            &[ProjectRootInput {
                path: repo.to_string_lossy().into_owned(),
                canonical_path: repo.to_string_lossy().into_owned(),
            }],
            0,
            "now",
        )
        .await
        .unwrap()
    else {
        panic!("new project")
    };
    let provider = match family {
        wonder_store::AgentFamily::Codex => crate::projects::store_key("codex", fixture),
        _ => crate::providers::provider_store(state, family).to_owned(),
    };
    let claude = family == wonder_store::AgentFamily::Claude;
    state
        .store
        .create_project_conversation(ProjectConversationInsert {
            conversation_id: conversation,
            project_id: &project.id,
            family,
            provider_store: &provider,
            native_session_id: None,
            cwd: &repo.to_string_lossy(),
            roots_revision: project.roots_revision,
            title: "Changes",
            model: claude.then_some("claude:haiku"),
            effort: None,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now: "now",
        })
        .await
        .unwrap();
    state
        .store
        .bind_project_runtime(conversation, family, &provider, thread, None, "now")
        .await
        .unwrap();
    assert_eq!(
        state
            .store
            .conversation_for_thread(thread)
            .await
            .unwrap()
            .as_deref(),
        Some(conversation)
    );
}

#[cfg(unix)]
fn boundary_notification(thread: &str, method: &str, turn: &str) -> Value {
    json!({"method": method, "params": {"threadId": thread, "turnId": turn,
        "turn": {"id": turn, "status": if method == "turn/started" {"inProgress"} else {"completed"}}}})
}

// A runtime turn starts from the tree as it was before Cupola sent it, for
// Claude's shell edits and Codex alike: an edit the runtime makes before it
// reports `turn/started` is still the turn's own.
// Uses the app fixtures, which are Unix-only (see lib.rs).
#[cfg(unix)]
#[tokio::test]
async fn runtime_turns_start_from_the_tree_before_they_were_sent() {
    let (fixture, state) = crate::ingestion::tests::fixture().await;
    let scratch = tempfile::tempdir().unwrap();
    for (conversation, family, thread) in [
        (
            "claude-chat",
            wonder_store::AgentFamily::Claude,
            "claude-thread",
        ),
        (
            "codex-chat",
            wonder_store::AgentFamily::Codex,
            "codex-thread",
        ),
    ] {
        let repo = busy_repository(&scratch.path().join(conversation));
        project_thread(&state, fixture.path(), conversation, family, thread, &repo).await;
        before_runtime_turn(&state, thread).await;
        // The runtime edits at once, before Cupola hears the turn started.
        fs::write(repo.join("app.js"), "one\nfast\n").unwrap();
        let edited = owner_view(&repo);
        if family == wonder_store::AgentFamily::Codex {
            // The receipt names the turn first; the notification replays it.
            runtime_turn_sent(&state, thread, Some("t1"));
        }
        for method in ["turn/started", "turn/completed"] {
            crate::process_app_server_notification(
                &state,
                boundary_notification(thread, method, "t1"),
                false,
            )
            .await;
            settled(thread).await;
            assert_eq!(owner_view(&repo), edited);
        }
        let directory = thread_directory(&state.data_dir, thread);
        let saved = load(&directory).checkpoints;
        assert!(
            saved.len() == 1 && saved[0].turn == "t1" && saved[0].end.is_some(),
            "{thread}: {saved:?}"
        );
        assert_eq!(
            git(&repo, &["show", &format!("{}:app.js", saved[0].start)]),
            "one\n",
            "{thread}: the baseline already held the turn's edit"
        );
        let items = [(
            Some("t1".to_owned()),
            json!({"type": "commandExecution", "command": "sed -i s/x/y/ app.js"}),
        )];
        let read = collect(&directory, &repo, &items, false, &repo, None, false, false).await;
        assert_eq!(
            read.changes
                .iter()
                .map(|c| (c.path.as_str(), c.additions))
                .collect::<Vec<_>>(),
            [("app.js", Some(1))],
            "{thread}"
        );
    }
    // A snapshot finished after its send stopped waiting is never a start.
    let repo = scratch.path().join("claude-chat");
    let directory = scratch.path().join("late");
    record(&directory, "late", &Step::Baseline, &repo, &|| false).await;
    record(&directory, "late", &began("t1"), &repo, &|| true).await;
    assert!(load(&directory).checkpoints.is_empty());
}

/// Items as the runtime lists them, newest first.
fn turn_items(items: Value) -> Vec<(Option<String>, Value)> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["turnId"].as_str().map(str::to_owned),
                entry["item"].clone(),
            )
        })
        .collect()
}

// End to end over HTTP: the route lists the files each turn's tool calls
// named, from that turn's checkpoint or, for a turn without one, its edit
// items, oldest turn first. Another writer's edit stays out even on a path
// the thread touched in an earlier turn, and the Uncommitted view still shows
// the whole checkout.
// Uses the app fixtures, which are Unix-only (see lib.rs).
#[cfg(unix)]
#[tokio::test]
async fn this_thread_lists_only_the_threads_own_changes_over_its_turns() {
    let route = super::super::tests::workspace_route;
    let (fixture, state) = crate::ingestion::tests::fixture().await;
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("source"));
    project_thread(
        &state,
        fixture.path(),
        "changes-chat",
        wonder_store::AgentFamily::Codex,
        "changes-thread",
        &repo,
    )
    .await;
    let turn = |turn: &'static str, edit: Box<dyn FnOnce()>| {
        let state = state.clone();
        async move {
            before_runtime_turn(&state, "changes-thread").await;
            runtime_turn_sent(&state, "changes-thread", Some(turn));
            edit();
            queue_step(&state, "changes-thread", ended(turn))
                .await
                .unwrap();
        }
    };
    let before = owner_view(&repo);
    // Turn 1 edits app.js, writes notes.txt and removes keep.txt from the
    // shell; meanwhile someone else edits other.txt.
    let root = repo.clone();
    turn(
        "t1",
        Box::new(move || {
            fs::write(root.join("app.js"), "one\ntwo\n").unwrap();
            fs::write(root.join("notes.txt"), "hi\n").unwrap();
            fs::remove_file(root.join("keep.txt")).unwrap();
            fs::write(root.join("other.txt"), "someone else\n").unwrap();
        }),
    )
    .await;
    assert_ne!(owner_view(&repo), before);
    // Turn 2 touches only b.txt while another writer edits app.js, which the
    // thread named in turn 1: that edit is not turn 2's.
    let root = repo.clone();
    turn(
        "t2",
        Box::new(move || {
            fs::write(root.join("b.txt"), "bee\n").unwrap();
            fs::write(root.join("app.js"), "one\ntwo\nsomeone else\n").unwrap();
        }),
    )
    .await;
    let saved = load(&thread_directory(&state.data_dir, "changes-thread")).checkpoints;
    assert!(
        saved.len() == 2 && saved.iter().all(|c| c.end.is_some()),
        "{saved:?}"
    );

    let items = json!({"threads": {}, "items": {"changes-thread": [
        // Turn 3 has no checkpoint (it ran on the Mac): its own edit item
        // removing app.js still counts, after turn 1's edit of it.
        {"turnId": "t3", "item": file_change("app.js", "delete", "@@ -1,2 +0,0 @@\n-one\n-two")},
        {"turnId": "t2", "item": file_change("b.txt", "add", "@@ -0,0 +1 @@\n+bee")},
        {"turnId": "t1", "item": {"type": "commandExecution", "command": "echo hi > notes.txt && rm keep.txt"}},
        {"turnId": "t1", "item": file_change("app.js", "update", "@@ -1 +1,2 @@\n one\n+two")},
        // An older turn from the Mac: only its edit item.
        {"turnId": "t0", "item": file_change(&repo.join("old.txt").to_string_lossy(), "add", "@@ -0,0 +1 @@\n+old")},
    ]}});
    fs::write(
        fixture.path().join("project-subagents-fixture.json"),
        items.to_string(),
    )
    .unwrap();

    let uri = "/api/v1/conversations/changes-chat/workspace/git/thread-changes?root=workspace";
    assert_ne!(route(&state, uri, false).await.0, StatusCode::OK);
    let (status, bytes) = route(&state, uri, true).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let changes: Value = serde_json::from_slice(&bytes).unwrap();
    crate::tests::validate_http_contract("workspaceThreadChanges", &changes);
    assert_eq!(changes["detail"], Value::Null, "{changes}");
    let rows = changes["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap(),
                file["status"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            ("app.js", "deleted"),
            ("b.txt", "added"),
            ("keep.txt", "deleted"),
            ("notes.txt", "added"),
            ("old.txt", "added"),
        ],
        "{changes}"
    );
    assert_eq!(
        changes["files"][0]["diff"],
        "@@ -1 +1,2 @@\n one\n+two\n@@ -1,2 +0,0 @@\n-one\n-two"
    );
    assert_eq!(
        changes["totals"],
        json!({"files": 5, "additions": 4, "deletions": 3})
    );

    // Uncommitted still shows the whole checkout, the other writer included.
    let uncommitted = "/api/v1/conversations/changes-chat/workspace/git/changes?root=workspace";
    let (_, bytes) = route(&state, uncommitted, true).await;
    let whole: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(whole["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "other.txt"));
    // The owner's staged work survived every checkpoint.
    assert_eq!(
        git(&repo, &["diff", "--cached", "--name-only"]),
        "staged.txt\n"
    );
    assert_eq!(
        git(&repo, &["show", ":staged.txt"]),
        "staged by the owner\n"
    );
}

// A checkpoint is used for its turn only once it was read: one that can't be
// read leaves the turn to its edit items, and the read says it is partial.
#[tokio::test]
async fn an_unreadable_checkpoint_leaves_its_turn_to_its_items() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let directory = scratch.path().join("data");
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("t1"), &repo).await;
    fs::write(repo.join("app.js"), "two\n").unwrap();
    step(&directory, "thread", ended("t1"), &repo).await;
    let mut saved = load(&directory);
    saved.checkpoints[0].start = "0".repeat(40);
    save(&directory, &saved);
    let items = turn_items(json!([
        {"turnId": "t1", "item": file_change("app.js", "update", "@@ -1 +1 @@\n-one\n+two")},
    ]));
    let read = collect(&directory, &repo, &items, false, &repo, None, false, false).await;
    assert!(read.partial);
    assert_eq!(
        read.changes
            .iter()
            .map(|c| (c.path.as_str(), c.status))
            .collect::<Vec<_>>(),
        [("app.js", "modified")]
    );
}

// A T3 Code run's checkpoint starts before the send and ends when T3 Code
// says the run ended, not at the next send. Which runtime turns it covers is
// not known, so it saves nothing they named, the read says it is partial,
// and its turns keep their own edit items. A send that started no run of its
// own (queued) keeps no checkpoint.
// Uses the app fixtures, which are Unix-only (see lib.rs).
#[cfg(unix)]
#[tokio::test]
async fn t3_code_runs_end_their_checkpoint_when_the_run_ends() {
    let (fixture, mut state) = crate::ingestion::tests::fixture().await;
    let fake = crate::t3_code::tests::Fake::start().await;
    state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    project_thread(
        &state,
        fixture.path(),
        "t3-chat",
        wonder_store::AgentFamily::Codex,
        "t3-native",
        &repo,
    )
    .await;
    let directory = thread_directory(&state.data_dir, "t3-native");
    t3_sending(&state, "t3-chat", "message-1").await;
    // While the run goes on, its coverage is already unknown.
    let open = load(&directory).checkpoints;
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(open[0].t3 && open[0].end.is_none());
    let read = collect(&directory, &repo, &[], false, &repo, None, false, false).await;
    assert!(read.partial);
    // T3 Code's run edits app.js through the shell; another writer edits
    // other.txt.
    fs::write(repo.join("app.js"), "one\nfrom t3\n").unwrap();
    fs::write(repo.join("other.txt"), "someone else\n").unwrap();
    let started = json!({"delivery": "started", "runId": "run-7", "status": "running"});
    let mut events = state.events.subscribe();
    t3_sent(&state, "t3-chat", "message-1", "t3-thread", &Ok(started)).await;
    for _ in 0..400 {
        if load(&directory)
            .checkpoints
            .first()
            .is_some_and(|c| c.end.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    settled("t3-native").await;
    let saved = load(&directory).checkpoints;
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(saved[0].end.is_some(), "the run's end closed nothing");
    assert!(saved[0].named.is_none(), "{saved:?}");
    assert_eq!(fake.waits.lock().unwrap()[0]["runId"], "run-7");
    // T3 Code's word that the run ended tells the open chat to read the
    // reply, with no transcript watch involved.
    let woken = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("event stream closed");
            if event.conversation_id.as_deref() == Some("t3-chat")
                && matches!(&event.event, wonder_api::WonderEvent::Activity { category, .. } if category == "native_history")
            {
                break;
            }
        }
    })
    .await;
    assert!(woken.is_ok(), "the chat was not told the run ended");
    let items = turn_items(json!([
        {"turnId": "n2", "item": {"type": "commandExecution", "command": "echo 'from t3' >> app.js"}},
        {"turnId": "n1", "item": {"type": "commandExecution", "command": "cat other.txt"}},
    ]));
    let read = collect(&directory, &repo, &items, false, &repo, None, false, false).await;
    assert!(read.partial);
    // Neither file is listed from the checkpoint: no turn's edit item names
    // them, and the run's tree diff would hold other.txt too.
    assert!(read.changes.is_empty(), "{:?}", read.changes);

    t3_sending(&state, "t3-chat", "message-2").await;
    assert_eq!(load(&directory).checkpoints.len(), 2);
    let queued = json!({"delivery": "queued", "runId": null, "status": "queued"});
    t3_sent(&state, "t3-chat", "message-2", "t3-thread", &Ok(queued)).await;
    settled("t3-native").await;
    assert_eq!(load(&directory).checkpoints, saved);
    let refs = git(&repo, &["for-each-ref", "--format=%(refname)", REF_ROOT]);
    assert!(!refs.contains("message-2"), "{refs}");
}

// Codex keeps its whole-turn diff instead of checkpoints, saved on disk per
// turn, and the route reads it back for the thread's own repository.
#[tokio::test]
async fn codex_turn_diffs_are_saved_and_stand_for_their_turns() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let data = scratch.path().join("data");
    let directory = thread_directory(&data, "codex-thread");
    step(&directory, "codex-thread", Step::Baseline, &repo).await;
    step(&directory, "codex-thread", began("turn-a"), &repo).await;
    fs::write(repo.join("app.js"), "two\n").unwrap();
    step(&directory, "codex-thread", ended("turn-a"), &repo).await;
    assert_eq!(load(&directory).checkpoints.len(), 1);
    // The diff holds a file the shell wrote, which no edit item names.
    let diff = "diff --git a/app.js b/app.js\n--- a/app.js\n+++ b/app.js\n@@ -1 +1 @@\n-one\n+two\ndiff --git a/gen.txt b/gen.txt\nnew file mode 100644\n--- /dev/null\n+++ b/gen.txt\n@@ -0,0 +1 @@\n+generated\n";
    crate::turn_diff::record(&data, "codex-thread", "turn-a", diff);
    let items = vec![
        // The same turn's checkpoint and edit item are not counted twice.
        (
            Some("turn-a".to_owned()),
            file_change("app.js", "update", "@@ -1 +1 @@\n-one\n+two"),
        ),
    ];
    let read = collect(&directory, &repo, &items, false, &repo, None, true, false).await;
    let rows = read
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.status, c.additions, c.deletions))
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            ("app.js", "modified", Some(1), Some(1)),
            ("gen.txt", "added", Some(1), Some(0)),
        ]
    );
    // Another repository the thread did not run in reads none of it.
    let elsewhere = collect(&directory, &repo, &[], false, &repo, None, false, false).await;
    assert!(elsewhere.changes.is_empty());
}

// A long thread's oldest items are not read, but a checkpoint whose turn
// ended saved what that turn named, so a Claude shell edit there still
// counts. A checkpoint that saved nothing and whose turn is no longer listed
// is unknown: the read says it is partial. Either way the read says the
// earliest turns were not read.
#[tokio::test]
async fn old_checkpoints_count_from_what_their_turns_named() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let directory = scratch.path().join("data");
    let shell = |turn: &str, command: &str| {
        (
            Some(turn.to_owned()),
            json!({"type": "commandExecution", "command": command}),
        )
    };
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("t1"), &repo).await;
    fs::write(repo.join("app.js"), "one\nfrom the shell\n").unwrap();
    let closed = record(&directory, "thread", &ended("t1"), &repo, &|| true).await;
    assert_eq!(closed, ["t1"]);
    // At its end the turn's items are read back whole (an older item shows).
    let at_end = [
        shell("t1", "echo 'from the shell' >> app.js"),
        shell("t0", "ls"),
    ];
    let mut saved = load(&directory);
    assert!(attribute(
        &mut saved, &repo, &closed, &at_end, true, &repo, None
    ));
    save(&directory, &saved);
    let named = load(&directory).checkpoints[0].named.clone().unwrap();
    assert_eq!(named.turns, ["t1"]);
    assert!(named.paths.contains(&repo.join("app.js")), "{named:?}");
    // t2's items are not read whole at its end: nothing is saved for it.
    step(&directory, "thread", Step::Baseline, &repo).await;
    step(&directory, "thread", began("t2"), &repo).await;
    fs::write(repo.join("notes.txt"), "hi\n").unwrap();
    let closed = record(&directory, "thread", &ended("t2"), &repo, &|| true).await;
    let mut saved = load(&directory);
    let cut = [shell("t2", "echo hi > notes.txt")];
    assert!(!attribute(
        &mut saved, &repo, &closed, &cut, true, &repo, None
    ));

    // Much later only the newest items are read: neither turn is listed.
    let newest = [shell("t9", "ls")];
    let read = collect(&directory, &repo, &newest, true, &repo, None, false, false).await;
    assert_eq!(
        read.changes
            .iter()
            .map(|c| (c.path.as_str(), c.additions))
            .collect::<Vec<_>>(),
        [("app.js", Some(1))]
    );
    assert!(read.partial, "t2's checkpoint is unknown");
    assert!(read.earlier);
    // Every item read: nothing is said to be missing from the start.
    let whole = [
        shell("t2", "echo hi > notes.txt"),
        shell("t1", "echo 'from the shell' >> app.js"),
    ];
    let read = collect(&directory, &repo, &whole, false, &repo, None, false, false).await;
    assert!(!read.partial && !read.earlier);
    assert_eq!(
        read.changes
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        ["app.js", "notes.txt"]
    );
}

// Which runtime turns a T3 Code run covers is unknown, whatever the items
// read hold: whether the turn it ended on is missing (items n1 only) or a
// later turn is listed (n1, n3). It saves no attribution (which would hide
// later edits for good), claims no turn, so n3's edit item still counts, and
// the read says it is partial.
#[tokio::test]
async fn a_t3_code_run_is_unknown_and_claims_no_turns() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    let directory = scratch.path().join("data");
    step(
        &directory,
        "thread",
        Step::Sending("message-1".into()),
        &repo,
    )
    .await;
    step(&directory, "thread", ended("message-1"), &repo).await;
    let checkpoint = load(&directory).checkpoints.remove(0);
    assert!(checkpoint.t3 && checkpoint.end.is_some());
    let closed = ["message-1".to_owned()];
    // n3 ran later (from the Mac) and added b.txt.
    fs::write(repo.join("b.txt"), "bee\n").unwrap();
    let only_start = turn_items(json!([
        {"turnId": "n1", "item": {"type": "commandExecution", "command": "ls"}},
    ]));
    let later = turn_items(json!([
        {"turnId": "n3", "item": file_change("b.txt", "add", "@@ -0,0 +1 @@\n+bee")},
        {"turnId": "n1", "item": {"type": "commandExecution", "command": "ls"}},
    ]));
    for (items, kept) in [(&only_start, vec![]), (&later, vec![("b.txt", "added")])] {
        assert_eq!(covered(&timeline(items), &checkpoint), None);
        let mut saved = load(&directory);
        assert!(!attribute(
            &mut saved, &repo, &closed, items, false, &repo, None
        ));
        assert!(saved.checkpoints[0].named.is_none());
        let read = collect(&directory, &repo, items, false, &repo, None, false, false).await;
        assert!(read.partial);
        assert_eq!(
            read.changes
                .iter()
                .map(|c| (c.path.as_str(), c.status))
                .collect::<Vec<_>>(),
            kept
        );
    }
}

// Digits are a file descriptor only right before a redirect: a file whose
// name starts with digits is still named.
#[test]
fn shell_words_keep_leading_digits_unless_they_redirect() {
    let cwd = work_repo();
    let named = named_paths(
        &[
            json!({"type": "commandExecution", "command": "rm 2026.txt"}),
            json!({"type": "commandExecution", "command": "echo x 2>err.log; touch 123 && cat <in.txt"}),
        ],
        cwd,
        None,
    );
    for path in ["2026.txt", "err.log", "123", "in.txt"] {
        assert!(named.contains(&cwd.join(path)), "{path} in {named:?}");
    }
    for wrong in [".txt", "2>err.log"] {
        assert!(!named.contains(&cwd.join(wrong)), "{wrong} in {named:?}");
    }
}

// A `.git` file an agent plants in a Project folder never makes checkpoints
// write another repository: no refs, objects or index changes land there.
#[tokio::test]
async fn checkpoints_never_write_a_repository_outside_the_folder() {
    let scratch = tempfile::tempdir().unwrap();
    let victim = busy_repository(&scratch.path().join("victim"));
    let folder = scratch.path().join("project");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("app.js"), "planted\n").unwrap();
    fs::write(
        folder.join(".git"),
        format!("gitdir: {}\n", victim.join(".git").display()),
    )
    .unwrap();
    let folder = crate::canonical_path::canonicalize(folder).unwrap();
    let before = owner_view(&victim);
    let objects = git(&victim, &["count-objects"]);
    let directory = scratch.path().join("data");
    step(&directory, "thread", Step::Baseline, &folder).await;
    step(&directory, "thread", began("turn"), &folder).await;
    fs::write(folder.join("app.js"), "edited\n").unwrap();
    step(&directory, "thread", ended("turn"), &folder).await;
    assert!(load(&directory).checkpoints.is_empty());
    assert!(load(&directory).baseline.is_none());
    assert_eq!(owner_view(&victim), before);
    assert_eq!(git(&victim, &["for-each-ref", REF_ROOT, BASELINE_ROOT]), "");
    assert_eq!(git(&victim, &["count-objects"]), objects);
}

// A linked worktree its repository names back is checkpointed as usual, with
// the refs in the repository's shared Git directory.
#[tokio::test]
async fn linked_worktrees_are_still_checkpointed() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = busy_repository(&scratch.path().join("repo"));
    git(&repo, &["worktree", "add", "-q", "-b", "side", "../linked"]);
    let linked = crate::canonical_path::canonicalize(scratch.path().join("linked")).unwrap();
    let directory = scratch.path().join("data");
    step(&directory, "thread", Step::Baseline, &linked).await;
    step(&directory, "thread", began("turn"), &linked).await;
    fs::write(linked.join("app.js"), "two\n").unwrap();
    step(&directory, "thread", ended("turn"), &linked).await;
    let saved = load(&directory).checkpoints;
    assert_eq!(saved.len(), 1);
    assert!(git(&repo, &["for-each-ref", REF_ROOT]).contains(&saved[0].start));
}

// A checkpoint's Git is held to the checked repository by the operating
// system: `.git` swapped for a link to another repository after the view was
// prepared takes none of the checkpoint's objects, index or refs.
#[cfg(unix)]
#[tokio::test]
#[cfg(unix)]
async fn a_git_directory_swapped_after_the_check_takes_no_checkpoint() {
    let scratch = tempfile::tempdir().unwrap();
    let victim = busy_repository(&scratch.path().join("victim"));
    let repo = busy_repository(&scratch.path().join("repo"));
    let before = owner_view(&victim);
    let objects = git(&victim, &["count-objects", "-v"]);
    let (view, _scratch) = checkpoint_view(&repo).await.unwrap();
    fs::rename(repo.join(".git"), scratch.path().join("aside")).unwrap();
    std::os::unix::fs::symlink(victim.join(".git"), repo.join(".git")).unwrap();
    let commit = snapshot(&view).await;
    if let Some(commit) = &commit {
        assert!(!point(&view, &baseline_reference("thread"), commit).await);
    }
    assert_eq!(owner_view(&victim), before);
    assert_eq!(git(&victim, &["for-each-ref", REF_ROOT, BASELINE_ROOT]), "");
    assert_eq!(git(&victim, &["count-objects", "-v"]), objects);
}

// -------------------------------------------------------------------------
// Saved-diff aggregation: bounded, partial when a budget runs out, and kept
// off the async workers.
// -------------------------------------------------------------------------

/// The retained-hunk budget `collect` documents (`MAX_SAVED_DIFF_HUNKS`),
/// repeated so these checks also state the limit they hold the code to.
const RETAINED_BUDGET: usize = 8 * 1024 * 1024;

/// A saved turn diff of one file near the 4 MiB save limit: `bytes` long, one
/// hunk of added lines.
fn near_limit_diff(path: &str, bytes: usize) -> String {
    let line = format!("+{}\n", "x".repeat(78));
    let lines = bytes / line.len();
    let mut diff = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1,{lines} @@\n"
    );
    for _ in 0..lines {
        diff.push_str(&line);
    }
    diff
}

/// A thread with `turns` saved turn diffs, oldest first. Every turn is a hard
/// link to one file, so a 500-turn near-limit history costs one file of disk
/// yet every read is a real read. Returns the thread directory.
fn saved_history(root: &Path, turns: usize, bytes: usize) -> PathBuf {
    let directory = thread_directory(root, "history-thread");
    let saved = turns_directory(&directory);
    fs::create_dir_all(&saved).unwrap();
    let template = saved.join("template.tmp");
    fs::write(&template, near_limit_diff("big.txt", bytes)).unwrap();
    for turn in 0..turns {
        let link = saved.join(format!("{:013}-turn-{turn:04}.diff", 1_000 + turn));
        fs::hard_link(&template, link).unwrap();
    }
    fs::remove_file(&template).unwrap();
    directory
}

fn retained(read: &ThreadChanges) -> usize {
    read.changes
        .iter()
        .map(|change| change.diff.as_ref().map_or(0, String::len))
        .sum()
}

/// The audited defect: every saved diff was read and its hunks concatenated
/// with no aggregate limit. A history far past the limits now ends partial
/// with at most the retained budget, however many turns remain unread.
#[tokio::test]
async fn a_history_past_the_budget_ends_partial_without_retaining_it_all() {
    let scratch = tempfile::tempdir().unwrap();
    let directory = saved_history(scratch.path(), 24, 3_900_000);
    let read = collect(
        &directory,
        scratch.path(),
        &[],
        false,
        scratch.path(),
        None,
        true,
        false,
    )
    .await;
    assert!(read.partial, "a history past the budget must say partial");
    assert!(
        retained(&read) <= RETAINED_BUDGET,
        "retained {} bytes of diff, past the {RETAINED_BUDGET} byte budget",
        retained(&read)
    );
    assert_eq!(read.changes.len(), 1, "the oldest turns' path still lists");
    assert_eq!(read.changes[0].path, "big.txt");
}

/// While saved diffs are flushed, read and parsed the runtime keeps serving:
/// an independent heartbeat and a small unrelated request finish promptly. The
/// single-threaded runtime makes any blocking on it visible as one long gap;
/// the flush is delayed the way a slow disk write would delay it.
#[tokio::test]
async fn reading_saved_diffs_does_not_stall_the_runtime() {
    let scratch = tempfile::tempdir().unwrap();
    let directory = saved_history(scratch.path(), 24, 3_900_000);
    let beat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let heartbeat = {
        let beat = beat.clone();
        tokio::spawn(async move {
            let mut last = std::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let gap = last.elapsed().as_millis() as u64;
                beat.fetch_max(gap, std::sync::atomic::Ordering::Relaxed);
                last = std::time::Instant::now();
            }
        })
    };
    // Let the heartbeat start before the read does.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stall = crate::turn_diff::stall_saves(Duration::from_millis(400));
    let started = std::time::Instant::now();
    let request = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        started.elapsed()
    });
    let read = collect(
        &directory,
        scratch.path(),
        &[],
        false,
        scratch.path(),
        None,
        true,
        false,
    )
    .await;
    let request = request.await.unwrap();
    stall.join().unwrap();
    heartbeat.abort();
    let longest_gap = beat.load(std::sync::atomic::Ordering::Relaxed);
    assert!(!read.changes.is_empty());
    assert!(
        longest_gap < 200,
        "the heartbeat stalled for {longest_gap} ms while saved diffs were read"
    );
    assert!(
        request < Duration::from_millis(200),
        "a small request waited {request:?} behind the saved-diff read"
    );
}

/// A small saved diff of `path` whose hunk is `body`.
fn small_diff(path: &str, body: &str) -> String {
    format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n{body}\n")
}

/// Saves `diffs` as the thread's turns, oldest first, and returns its
/// directory.
fn saved_turns_of(root: &Path, diffs: &[String]) -> PathBuf {
    let directory = thread_directory(root, "small-thread");
    let saved = turns_directory(&directory);
    fs::create_dir_all(&saved).unwrap();
    for (turn, diff) in diffs.iter().enumerate() {
        fs::write(
            saved.join(format!("{:013}-turn-{turn:04}.diff", 1_000 + turn)),
            diff,
        )
        .unwrap();
    }
    directory
}

fn load_with(directory: &Path, limits: SavedLimits) -> SavedDiffs {
    load_saved_diffs(
        directory,
        limits,
        &AtomicBool::new(false),
        &|path, limit| crate::turn_diff::read_saved(path, limit),
    )
}

#[test]
fn the_documented_limits_are_the_ones_these_checks_hold() {
    assert_eq!(MAX_SAVED_DIFF_HUNKS, RETAINED_BUDGET);
    assert_eq!(MAX_SAVED_DIFF_FILE, 4 * 1024 * 1024);
    assert_eq!(MAX_SAVED_DIFF_INPUT, 32 * 1024 * 1024);
    assert_eq!(MAX_SAVED_DIFF_READERS, 2);
    // The thread keeps 500 diffs of at most 4 MiB; the input budget must be
    // far below what that could be, or it would limit nothing.
    const { assert!(MAX_SAVED_DIFF_INPUT < 500 * MAX_SAVED_DIFF_FILE / 32) };
}

/// Below every limit the aggregation is what it was: the same paths in the
/// same order, the same joined hunks and counts, and not partial.
#[tokio::test]
async fn a_history_below_the_limits_reads_as_it_always_did() {
    let scratch = tempfile::tempdir().unwrap();
    let diffs = [
        small_diff("b.txt", "-one\n+two"),
        format!(
            "{}{}",
            small_diff("a.txt", "-x\n+y"),
            small_diff("b.txt", "-two\n+three")
        ),
        "diff --git a/logo.png b/logo.png\nnew file mode 100644\nBinary files /dev/null and b/logo.png differ\n"
            .to_owned(),
        small_diff("c.txt", "+made"),
    ];
    let directory = saved_turns_of(scratch.path(), &diffs);
    // What the union of these turns' parsed files gives, computed directly.
    let expected = union(
        diffs
            .iter()
            .map(|diff| {
                crate::turn_diff::files(diff)
                    .into_iter()
                    .map(TurnFile::from)
                    .collect()
            })
            .collect(),
    );
    let read = collect(
        &directory,
        scratch.path(),
        &[],
        false,
        scratch.path(),
        None,
        true,
        false,
    )
    .await;
    assert!(!read.partial);
    assert_eq!(read.changes, expected);
    let paths = read
        .changes
        .iter()
        .map(|c| c.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["a.txt", "b.txt", "c.txt", "logo.png"]);
    assert_eq!(
        read.changes[1].diff.as_deref(),
        Some("@@ -1 +1 @@\n-one\n+two\n@@ -1 +1 @@\n-two\n+three")
    );
}

#[test]
fn each_saved_diff_limit_ends_the_read_before_the_diff_that_passes_it() {
    let scratch = tempfile::tempdir().unwrap();
    let diffs = (0..6)
        .map(|n| small_diff(&format!("f{n}.txt"), &"+".repeat(100)))
        .collect::<Vec<_>>();
    let directory = saved_turns_of(scratch.path(), &diffs);
    let size = diffs[0].len() as u64;
    let hunk = crate::turn_diff::files(&diffs[0])[0].hunks.len();
    let open = SavedLimits {
        file: size,
        input: u64::MAX,
        hunks: usize::MAX,
        entries: usize::MAX,
    };
    let all = load_with(&directory, open);
    assert!(!all.partial);
    assert_eq!((all.diffs.len(), all.input), (6, 6 * size));

    // Aggregate input: three diffs fit, the fourth is not read.
    let by_input = load_with(
        &directory,
        SavedLimits {
            input: 3 * size + 1,
            ..open
        },
    );
    assert!(by_input.partial);
    assert_eq!((by_input.diffs.len(), by_input.input), (3, 3 * size));

    // Retained hunks: two fit, the third is parsed but not kept.
    let by_hunks = load_with(
        &directory,
        SavedLimits {
            hunks: 2 * hunk + 1,
            ..open
        },
    );
    assert!(by_hunks.partial);
    assert_eq!((by_hunks.diffs.len(), by_hunks.hunks), (2, 2 * hunk));

    // Entries kept.
    let by_entries = load_with(&directory, SavedLimits { entries: 4, ..open });
    assert!(by_entries.partial);
    assert_eq!((by_entries.diffs.len(), by_entries.entries), (4, 4));

    // One diff over the per-file limit is skipped; the rest still read.
    fs::write(
        turns_directory(&directory).join("0000000001002-turn-0002.diff"),
        small_diff("big.txt", &"+".repeat(400)),
    )
    .unwrap();
    let by_file = load_with(&directory, open);
    assert!(by_file.partial);
    assert_eq!(by_file.diffs.len(), 5);
    assert!(by_file
        .diffs
        .iter()
        .all(|(_, _, files)| files[0].path != "big.txt"));
}

/// Requests dropped mid-read leave workers that stop at the next file, and
/// all of them together never exceed the worker limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_saved_reads_stay_within_the_worker_limit() {
    let scratch = tempfile::tempdir().unwrap();
    let diffs = (0..30)
        .map(|n| small_diff(&format!("f{n}.txt"), "+x"))
        .collect::<Vec<_>>();
    let directory = saved_turns_of(scratch.path(), &diffs);
    let reads = Arc::new(SavedReads::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    let request = |reads: Arc<SavedReads>, directory: PathBuf, calls: Arc<AtomicUsize>| {
        tokio::spawn(async move {
            reads
                .load(&directory, SavedLimits::DOCUMENTED, move |path, limit| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    crate::turn_diff::read_saved(path, limit)
                })
                .await
        })
    };
    let mut requests = (0..12)
        .map(|_| request(reads.clone(), directory.clone(), calls.clone()))
        .collect::<Vec<_>>();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let survivors = requests.split_off(10);
    for abandoned in requests {
        abandoned.abort();
    }
    for survivor in survivors {
        let loaded = survivor.await.unwrap();
        assert_eq!((loaded.diffs.len(), loaded.partial), (30, false));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while reads.active.load(Ordering::SeqCst) > 0 || reads.gate.available_permits() < 2 {
        assert!(std::time::Instant::now() < deadline, "workers never ended");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        reads.peak.load(Ordering::SeqCst) <= 2,
        "{} workers ran at once",
        reads.peak.load(Ordering::SeqCst)
    );
    // The two survivors read all 30 diffs each. The ten abandoned requests
    // add the few reads before each was dropped, not 30 apiece.
    let total = calls.load(Ordering::SeqCst);
    assert!(
        total <= 2 * 30 + 12,
        "{total} reads: abandoned workers ran on"
    );
}

/// An unrelated request is served while a saved-diff read waits on its disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_saved_diff_reads_do_not_hold_up_other_requests() {
    let scratch = tempfile::tempdir().unwrap();
    let diffs = (0..5)
        .map(|n| small_diff(&format!("f{n}.txt"), "+x"))
        .collect::<Vec<_>>();
    let directory = saved_turns_of(scratch.path(), &diffs);
    let reads = SavedReads::new(2);
    let slow = reads.load(&directory, SavedLimits::DOCUMENTED, |path, limit| {
        std::thread::sleep(Duration::from_millis(200));
        crate::turn_diff::read_saved(path, limit)
    });
    let unrelated = async {
        let started = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(10)).await;
        started.elapsed()
    };
    let (loaded, waited) = tokio::join!(slow, unrelated);
    assert_eq!(loaded.diffs.len(), 5);
    assert!(waited < Duration::from_millis(150), "waited {waited:?}");
}

/// Controlled large-history measurement, run by hand with
/// `cargo test -p wonderd --release -- --ignored --nocapture a_large_history`
/// (`DIFF_BOUNDS_TURNS` sets the turn count, 500 by default). Prints peak and
/// current memory, heartbeat gap and the request wait; asserts nothing.
#[tokio::test]
#[ignore = "measurement, not a check"]
async fn a_large_history_measurement() {
    fn kilobytes(field: &str) -> u64 {
        fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix(field)?
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or_default()
    }
    let turns = std::env::var("DIFF_BOUNDS_TURNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    let scratch = tempfile::tempdir().unwrap();
    let directory = saved_history(scratch.path(), turns, 3_900_000);
    let before = (kilobytes("VmHWM:"), kilobytes("VmRSS:"));
    let gap = Arc::new(AtomicUsize::new(0));
    let heartbeat = {
        let gap = gap.clone();
        tokio::spawn(async move {
            let mut last = std::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                gap.fetch_max(last.elapsed().as_millis() as usize, Ordering::Relaxed);
                last = std::time::Instant::now();
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let started = std::time::Instant::now();
    let request = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        started.elapsed()
    });
    let read = collect(
        &directory,
        scratch.path(),
        &[],
        false,
        scratch.path(),
        None,
        true,
        false,
    )
    .await;
    let elapsed = started.elapsed();
    let request = request.await.unwrap();
    heartbeat.abort();
    let after = (kilobytes("VmHWM:"), kilobytes("VmRSS:"));
    println!(
        "MEASURE turns={turns} bytes_per_turn=3900000 partial={} retained_bytes={} \
         changes={} elapsed_ms={} heartbeat_max_gap_ms={} request_wait_ms={} \
         hwm_kb_before={} hwm_kb_after={} rss_kb_before={} rss_kb_after={}",
        read.partial,
        retained(&read),
        read.changes.len(),
        elapsed.as_millis(),
        gap.load(Ordering::Relaxed),
        request.as_millis(),
        before.0,
        after.0,
        before.1,
        after.1,
    );
}
