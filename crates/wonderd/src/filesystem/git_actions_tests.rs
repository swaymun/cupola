//! Each Git action is a pass/fail check against temporary repositories, local
//! bare remotes and a stand-in `gh` that logs its arguments. Nothing here
//! reaches GitHub or a real remote.

use super::*;
use std::os::unix::fs::PermissionsExt;

fn sh(dir: &Path, args: &[&str]) -> String {
    let output = crate::test_git::git()
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Repository-local identity so commits never depend on the Mac's settings.
fn identity(dir: &Path) {
    sh(dir, &["config", "user.name", "Cupola Test"]);
    sh(dir, &["config", "user.email", "test@example.com"]);
    sh(dir, &["config", "commit.gpgsign", "false"]);
}

/// A clone of a bare remote with one commit on main, origin/HEAD set.
fn cloned(dir: &Path) -> (PathBuf, PathBuf) {
    let remote = dir.join("remote.git");
    sh(
        dir,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    let seed = dir.join("seed");
    sh(dir, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
    identity(&seed);
    fs::write(seed.join("README"), "hi\n").unwrap();
    fs::write(seed.join(".gitignore"), "*.log\n").unwrap();
    sh(&seed, &["add", "."]);
    sh(&seed, &["commit", "-q", "-m", "first"]);
    sh(&seed, &["push", "-q", remote.to_str().unwrap(), "main"]);
    let work = dir.join("work");
    sh(
        dir,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    identity(&work);
    (crate::canonical_path::canonicalize(&work).unwrap(), remote)
}

fn request(action: &str) -> ActionRequest {
    ActionRequest {
        repository_id: "local:0000000000000000".into(),
        root: "workspace".into(),
        action: action.into(),
        ..Default::default()
    }
}

fn refused(outcome: &Outcome) -> (&'static str, &str) {
    assert!(!outcome.ok, "expected a refusal: {outcome:?}");
    (outcome.reason.unwrap(), outcome.message.as_str())
}

/// A stand-in `gh`: logs each call's arguments to `calls`, answers `pr view`
/// and `repo view` from `view.json` and `repo.json`, `pr create` with a new
/// pull request's address, and fails any call named in `fail` with its text.
fn fake_gh(dir: &Path) -> PathBuf {
    let gh = dir.join("gh");
    fs::write(
        &gh,
        r#"#!/bin/sh
here="$(dirname "$0")"
printf '%s\n' "$*" >> "$here/calls"
if [ -f "$here/fail" ] && grep -q "^$1 $2$" "$here/fail.cmd" 2>/dev/null; then cat "$here/fail" >&2; exit 1; fi
case "$1 $2" in
  "pr view") cat "$here/view.json" ;;
  "repo view") cat "$here/repo.json" ;;
  "pr create") echo "https://github.com/o/r/pull/41" ;;
  "pr merge") exit 0 ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        dir.join("repo.json"),
        r#"{"squashMergeAllowed":true,"mergeCommitAllowed":true,"rebaseMergeAllowed":true,"viewerDefaultMergeMethod":"SQUASH"}"#,
    )
    .unwrap();
    gh
}

fn calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn view(dir: &Path, state: &str, draft: bool, mergeable: &str, status: &str, checks: &str) {
    let rollup = match checks {
        "failing" => r#"[{"status":"COMPLETED","conclusion":"FAILURE"}]"#,
        "pending" => r#"[{"status":"IN_PROGRESS","conclusion":""}]"#,
        _ => r#"[{"status":"COMPLETED","conclusion":"SUCCESS"}]"#,
    };
    fs::write(
        dir.join("view.json"),
        format!(
            r#"{{"number":41,"title":"Add actions","url":"https://github.com/o/r/pull/41","state":"{state}","isDraft":{draft},"mergeable":"{mergeable}","mergeStateStatus":"{status}","statusCheckRollup":{rollup},"autoMergeRequest":null,"headRefName":"feature","baseRefName":"main","headRefOid":"{HEAD_OID}"}}"#
        ),
    )
    .unwrap();
}

const PR: &str = "https://github.com/o/r/pull/41";
const HEAD_OID: &str = "0123456789abcdef0123456789abcdef01234567";

fn slugs() -> Vec<String> {
    vec!["o/r".into()]
}

// Commit: staged only by default, everything .gitignore allows with Include
// unstaged; never an ignored file, never without a message.
#[tokio::test]
async fn commit_staged_only_then_with_unstaged_and_never_ignored_files() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    fs::write(work.join("README"), "edited\n").unwrap();
    fs::write(work.join("new.txt"), "new\n").unwrap();
    fs::write(work.join("debug.log"), "ignored\n").unwrap();
    let gh = Gh::Missing;

    let empty = perform(&work, &[], &request("commit"), &gh).await;
    assert_eq!(refused(&empty), ("invalid", "Write a commit message."));
    let mut staged_only = request("commit");
    staged_only.message = Some("Edit readme".into());
    let nothing = perform(&work, &[], &staged_only, &gh).await;
    assert_eq!(refused(&nothing).0, "nothingToCommit");

    sh(&work, &["add", "README"]);
    let outcome = perform(&work, &[], &staged_only, &gh).await;
    assert!(outcome.ok, "{outcome:?}");
    assert!(
        outcome.message.starts_with("Committed 1 file to main ("),
        "{}",
        outcome.message
    );
    assert_eq!(sh(&work, &["log", "-1", "--format=%s"]), "Edit readme");
    assert_eq!(
        sh(&work, &["show", "--name-only", "--format=", "HEAD"]),
        "README"
    );
    assert_eq!(
        outcome.commit.as_deref(),
        Some(sh(&work, &["rev-parse", "--short", "HEAD"]).as_str())
    );

    let mut everything = request("commit");
    everything.message = Some("Add the rest".into());
    everything.include_unstaged = Some(true);
    let outcome = perform(&work, &[], &everything, &gh).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        sh(&work, &["show", "--name-only", "--format=", "HEAD"]),
        "new.txt"
    );
    assert_eq!(
        sh(&work, &["status", "--porcelain", "--ignored"]),
        "!! debug.log"
    );

    // A file forced past .gitignore is refused, and nothing is committed.
    sh(&work, &["add", "-f", "debug.log"]);
    let before = sh(&work, &["rev-parse", "HEAD"]);
    let forced = perform(&work, &[], &everything, &gh).await;
    assert_eq!(refused(&forced).0, "ignoredFiles");
    assert!(forced.message.contains("debug.log"), "{}", forced.message);
    assert_eq!(sh(&work, &["rev-parse", "HEAD"]), before);
}

// Push: a new branch is pushed with its upstream set; a push the remote
// rejects is explained and never forced.
#[tokio::test]
async fn push_sets_upstream_on_a_new_branch_and_explains_a_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let (work, remote) = cloned(dir.path());
    sh(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("a.txt"), "a\n").unwrap();
    sh(&work, &["add", "a.txt"]);
    sh(&work, &["commit", "-q", "-m", "a"]);

    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        outcome.message,
        "Pushed feature to origin and set it to track origin/feature."
    );
    assert_eq!(
        sh(&work, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feature"
    );
    assert_eq!(
        sh(&remote, &["rev-parse", "refs/heads/feature"]),
        sh(&work, &["rev-parse", "HEAD"])
    );

    // Someone else pushes to feature; the local commit is rejected, not forced.
    let other = dir.path().join("other");
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            "-b",
            "feature",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    identity(&other);
    fs::write(other.join("b.txt"), "b\n").unwrap();
    sh(&other, &["add", "b.txt"]);
    sh(&other, &["commit", "-q", "-m", "b"]);
    sh(&other, &["push", "-q", "origin", "feature"]);
    let theirs = sh(&remote, &["rev-parse", "refs/heads/feature"]);
    fs::write(work.join("c.txt"), "c\n").unwrap();
    sh(&work, &["add", "c.txt"]);
    sh(&work, &["commit", "-q", "-m", "c"]);
    sh(&work, &["fetch", "-q"]);
    let rejected = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert_eq!(
        refused(&rejected),
        (
            "nonFastForward",
            "The remote has commits this branch doesn't. Pull first, then push again."
        )
    );
    assert_eq!(sh(&remote, &["rev-parse", "refs/heads/feature"]), theirs);
}

// Network receivers can still refuse pushes. Parse Git's porcelain response
// without running an untrusted local hook to manufacture that response.
#[test]
fn a_remote_hook_refusal_is_reported_in_plain_words() {
    let run = Run {
        success: false,
        stdout: "!\trefs/heads/second:refs/heads/second\t[remote rejected] (pre-receive hook declined)\n".into(),
        stderr: "remote: no pushes today\nerror: failed to push some refs\n".into(),
    };
    assert_eq!(
        explain(Kind::Push, &run),
        (
            "rejected",
            "The remote refused the push: pre-receive hook declined.".to_owned()
        )
    );
}

// Pushing to the default branch waits for the owner's confirmation; the
// refusal leaves the remote untouched.
#[tokio::test]
async fn push_to_the_default_branch_needs_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let (work, remote) = cloned(dir.path());
    fs::write(work.join("README"), "direct\n").unwrap();
    sh(&work, &["commit", "-q", "-am", "direct"]);
    let before = sh(&remote, &["rev-parse", "refs/heads/main"]);
    let unconfirmed = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert_eq!(
        refused(&unconfirmed),
        (
            "confirmDefaultBranch",
            "This pushes straight to main, the default branch, without a pull request."
        )
    );
    assert_eq!(sh(&remote, &["rev-parse", "refs/heads/main"]), before);
    let mut confirmed = request("push");
    confirmed.confirm_default_branch = Some(true);
    let outcome = perform(&work, &[], &confirmed, &Gh::Missing).await;
    assert_eq!(outcome.message, "Pushed 1 commit to origin/main.");
    assert_eq!(
        sh(&remote, &["rev-parse", "refs/heads/main"]),
        sh(&work, &["rev-parse", "HEAD"])
    );
}

// Pull only fast-forwards: a branch behind its remote catches up; one that
// diverged is refused and left as it was.
#[tokio::test]
async fn pull_fast_forwards_and_refuses_a_diverged_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (work, remote) = cloned(dir.path());
    let other = dir.path().join("other");
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    identity(&other);
    fs::write(other.join("b.txt"), "b\n").unwrap();
    sh(&other, &["add", "b.txt"]);
    sh(&other, &["commit", "-q", "-m", "b"]);
    sh(&other, &["push", "-q", "origin", "main"]);

    let outcome = perform(&work, &[], &request("pull"), &Gh::Missing).await;
    assert_eq!(outcome.message, "Pulled 1 commit from origin/main.");
    assert!(work.join("b.txt").exists());
    let again = perform(&work, &[], &request("pull"), &Gh::Missing).await;
    assert_eq!(again.message, "Already up to date with origin/main.");

    fs::write(other.join("c.txt"), "c\n").unwrap();
    sh(&other, &["add", "c.txt"]);
    sh(&other, &["commit", "-q", "-m", "c"]);
    sh(&other, &["push", "-q", "origin", "main"]);
    fs::write(work.join("d.txt"), "d\n").unwrap();
    sh(&work, &["add", "d.txt"]);
    sh(&work, &["commit", "-q", "-m", "d"]);
    let head = sh(&work, &["rev-parse", "HEAD"]);
    let diverged = perform(&work, &[], &request("pull"), &Gh::Missing).await;
    assert_eq!(refused(&diverged).0, "nonFastForward");
    assert!(diverged
        .message
        .starts_with("This branch and its remote both have new commits."));
    assert_eq!(sh(&work, &["rev-parse", "HEAD"]), head);

    sh(&work, &["checkout", "-q", "-b", "local-only"]);
    let untracked = perform(&work, &[], &request("pull"), &Gh::Missing).await;
    assert_eq!(refused(&untracked).0, "noUpstream");
}

// Create PR: gh's exact arguments, draft included; the branch must be pushed
// and must not be the default branch; gh missing or signed out says so.
#[tokio::test]
async fn create_pull_request_runs_gh_with_exact_arguments_and_refuses_unpushed_work() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let tools = tempfile::tempdir().unwrap();
    let gh = Gh::Ready(fake_gh(tools.path()));
    let mut create = request("createPullRequest");
    create.title = Some("Add actions".into());
    create.body = Some("Body with $(not a shell) and \"quotes\"".into());
    create.draft = Some(true);

    let on_main = perform(&work, &[], &create, &gh).await;
    assert_eq!(refused(&on_main).0, "defaultBranch");
    sh(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("a.txt"), "a\n").unwrap();
    sh(&work, &["add", "a.txt"]);
    sh(&work, &["commit", "-q", "-m", "a"]);
    let unpushed = perform(&work, &[], &create, &gh).await;
    assert_eq!(
        refused(&unpushed),
        (
            "notPushed",
            "Push this branch first so GitHub has its commits."
        )
    );
    assert!(
        calls(tools.path()).is_empty(),
        "no gh call before the checks pass"
    );
    assert_eq!(
        refused(&perform(&work, &[], &create, &Gh::Missing).await).0,
        "ghMissing"
    );
    assert_eq!(
        refused(&perform(&work, &[], &create, &Gh::SignedOut).await),
        (
            "ghSignedOut",
            "Sign in to the GitHub CLI on your computer (gh auth login), then try again."
        )
    );

    assert!(perform(&work, &[], &request("push"), &Gh::Missing).await.ok);
    let outcome = perform(&work, &[], &create, &gh).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.message, "Opened draft pull request #41 into main.");
    assert_eq!(
        outcome.pull_request,
        Some(CreatedPull {
            number: 41,
            url: PR.into()
        })
    );
    assert_eq!(
        calls(tools.path()),
        ["pr create --title Add actions --body Body with $(not a shell) and \"quotes\" --head feature --base main --draft"]
    );
    crate::tests::validate_http_contract(
        "gitActionOutcome",
        &serde_json::to_value(&outcome).unwrap(),
    );
    crate::tests::validate_http_contract(
        "gitActionOutcome",
        &serde_json::to_value(&unpushed).unwrap(),
    );
}

// The pull request opened from Code changes is linked to the chat, and the
// chat's repository index lists it as opened here.
#[tokio::test]
async fn a_created_pull_request_is_linked_to_the_thread_index() {
    let (_fixture, state) = crate::ingestion::tests::fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    sh(
        &work,
        &["remote", "set-url", "origin", "https://github.com/o/r.git"],
    );
    crate::test_support::project_thread(
        &state.store,
        "linked-chat",
        "Linked",
        work.to_str().unwrap(),
        "owner",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    let mut outcome = Outcome::done(
        Kind::CreatePullRequest,
        "Opened pull request #41 into main.",
    );
    outcome.pull_request = Some(CreatedPull {
        number: 41,
        url: PR.into(),
    });
    record(&state, "linked-chat", &work, &outcome).await;
    let links = state
        .store
        .thread_pull_request_links("linked-chat")
        .await
        .unwrap();
    assert_eq!(
        links,
        [(PR.to_owned(), work.to_string_lossy().into_owned())]
    );

    let tools = tempfile::tempdir().unwrap();
    let gh = fake_gh(tools.path());
    view(tools.path(), "OPEN", false, "MERGEABLE", "CLEAN", "passing");
    let linked: Vec<_> = links
        .into_iter()
        .map(|(url, path)| (url, PathBuf::from(path)))
        .collect();
    let root = WorkspaceRoot {
        id: "workspace".into(),
        label: "Workspace".into(),
        path: work.clone(),
        is_directory: true,
        kind: "workingDirectory",
    };
    let index = thread_repos::build(&[], &[root], &[], &linked, Some(&gh), None).await;
    let value = serde_json::to_value(&index).unwrap();
    let repo = &value["repositories"][0];
    assert_eq!(repo["writable"], true);
    assert_eq!(repo["defaultBranch"], "main");
    assert_eq!(repo["pullRequests"][0]["url"], PR);
    assert_eq!(repo["pullRequests"][0]["relation"], "opened");
    crate::tests::validate_http_contract("threadRepositoryIndex", &value);
}

// Merge: each method reaches gh as its own flag; a draft, failing or pending
// checks, conflicts, a branch behind, a blocked or undecided merge and a
// method the repository forbids are refused with their reason, before gh merges.
#[tokio::test]
async fn merge_runs_each_method_and_refuses_with_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let tools = tempfile::tempdir().unwrap();
    let gh = Gh::Ready(fake_gh(tools.path()));
    let merge = |method: Option<&str>| {
        let mut request = request("merge");
        request.pull_request_url = Some(PR.into());
        request.method = method.map(str::to_owned);
        request
    };
    view(tools.path(), "OPEN", false, "MERGEABLE", "CLEAN", "passing");
    for (method, flag, words) in [
        (Some("squash"), "--squash", "squash and merge"),
        (Some("merge"), "--merge", "merge commit"),
        (Some("rebase"), "--rebase", "rebase and merge"),
        (None, "--squash", "squash and merge"),
    ] {
        fs::remove_file(tools.path().join("calls")).ok();
        let outcome = perform(&work, &slugs(), &merge(method), &gh).await;
        assert_eq!(
            outcome.message,
            format!("Merged #41 ({words})."),
            "{method:?}"
        );
        assert_eq!(
            calls(tools.path()),
            [
                format!("pr view {PR} --json {PR_VIEW_FIELDS}"),
                format!("repo view o/r --json {REPO_VIEW_FIELDS}"),
                format!("pr merge {PR} {flag}"),
            ]
        );
    }
    for (state, draft, mergeable, status, checks, reason) in [
        ("OPEN", true, "MERGEABLE", "DRAFT", "passing", "draft"),
        (
            "OPEN",
            false,
            "MERGEABLE",
            "UNSTABLE",
            "failing",
            "checksFailing",
        ),
        (
            "OPEN",
            false,
            "MERGEABLE",
            "UNSTABLE",
            "pending",
            "checksPending",
        ),
        (
            "OPEN",
            false,
            "CONFLICTING",
            "DIRTY",
            "passing",
            "conflicts",
        ),
        ("OPEN", false, "MERGEABLE", "BEHIND", "passing", "behind"),
        ("OPEN", false, "MERGEABLE", "BLOCKED", "passing", "blocked"),
        ("OPEN", false, "UNKNOWN", "UNKNOWN", "passing", "unknown"),
        ("MERGED", false, "UNKNOWN", "UNKNOWN", "passing", "merged"),
        ("CLOSED", false, "UNKNOWN", "UNKNOWN", "passing", "closed"),
    ] {
        view(tools.path(), state, draft, mergeable, status, checks);
        fs::remove_file(tools.path().join("calls")).ok();
        let outcome = perform(&work, &slugs(), &merge(Some("squash")), &gh).await;
        assert_eq!(refused(&outcome).0, reason);
        assert!(
            !calls(tools.path())
                .iter()
                .any(|c| c.starts_with("pr merge")),
            "{reason}: gh must not merge"
        );
    }
    view(tools.path(), "OPEN", false, "MERGEABLE", "CLEAN", "passing");
    fs::write(
        tools.path().join("repo.json"),
        r#"{"squashMergeAllowed":true,"mergeCommitAllowed":false,"rebaseMergeAllowed":false,"viewerDefaultMergeMethod":"MERGE"}"#,
    )
    .unwrap();
    let forbidden = perform(&work, &slugs(), &merge(Some("rebase")), &gh).await;
    assert_eq!(
        refused(&forbidden),
        (
            "methodNotAllowed",
            "This repository doesn't allow rebase and merge."
        )
    );
    // A pull request in a repository this one doesn't push to is refused.
    let mut elsewhere = merge(Some("squash"));
    elsewhere.pull_request_url = Some("https://github.com/x/y/pull/1".into());
    assert_eq!(
        refused(&perform(&work, &slugs(), &elsewhere, &gh).await).0,
        "otherRepository"
    );
    // gh's own refusal is put in plain words.
    fs::write(tools.path().join("fail.cmd"), "pr merge\n").unwrap();
    fs::write(
        tools.path().join("fail"),
        "GraphQL: Repository rule violations found: protected branch\n",
    )
    .unwrap();
    let protected = perform(&work, &slugs(), &merge(Some("squash")), &gh).await;
    assert_eq!(refused(&protected).0, "protectedBranch");

    let state = merge_state(
        &WriteView::prepare(&work, false).await.unwrap(),
        match &gh {
            Gh::Ready(gh) => gh,
            _ => unreachable!(),
        },
        &owned_address(Some(PR), &slugs()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        (state.methods.clone(), state.default_method),
        (vec!["squash"], Some("squash"))
    );
    crate::tests::validate_http_contract(
        "pullRequestMergeState",
        &serde_json::to_value(&state).unwrap(),
    );
}

// A merge pinned to the head the person reviewed passes the pin to GitHub,
// and is refused before gh merges once the head has moved or is malformed.
#[tokio::test]
async fn merge_pinned_to_a_head_commit_refuses_a_moved_head() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let tools = tempfile::tempdir().unwrap();
    let gh = Gh::Ready(fake_gh(tools.path()));
    view(tools.path(), "OPEN", false, "MERGEABLE", "CLEAN", "passing");
    let pinned = |oid: &str, action: &str| {
        let mut request = request(action);
        request.pull_request_url = Some(PR.into());
        request.method = Some("squash".into());
        request.enabled = (action == "autoMerge").then_some(true);
        request.expected_head_oid = Some(oid.into());
        request
    };
    let merged = perform(
        &work,
        &slugs(),
        &pinned(&HEAD_OID.to_uppercase(), "merge"),
        &gh,
    )
    .await;
    assert!(merged.ok, "{merged:?}");
    assert_eq!(
        calls(tools.path()).last().unwrap(),
        &format!("pr merge {PR} --squash --match-head-commit {HEAD_OID}")
    );
    fs::remove_file(tools.path().join("calls")).ok();
    let auto = perform(&work, &slugs(), &pinned(HEAD_OID, "autoMerge"), &gh).await;
    assert!(auto.ok, "{auto:?}");
    assert_eq!(
        calls(tools.path()).last().unwrap(),
        &format!("pr merge {PR} --auto --squash --match-head-commit {HEAD_OID}")
    );
    for (oid, reason) in [
        ("fedcba9876543210fedcba9876543210fedcba98", "headChanged"),
        ("--admin", "invalid"),
        ("abc123", "invalid"),
    ] {
        fs::remove_file(tools.path().join("calls")).ok();
        let outcome = perform(&work, &slugs(), &pinned(oid, "merge"), &gh).await;
        assert_eq!(refused(&outcome).0, reason, "{oid}");
        assert!(
            !calls(tools.path())
                .iter()
                .any(|c| c.starts_with("pr merge")),
            "{oid}: gh must not merge"
        );
    }
    crate::tests::validate_http_contract(
        "gitActionRequest",
        &serde_json::json!({"repositoryId": "local:0000000000000000", "root": "workspace",
            "action": "merge", "pullRequestUrl": PR, "expectedHeadOid": HEAD_OID}),
    );
}

// Auto-merge: on with the chosen method, off with --disable-auto; a draft or
// conflicting pull request can't turn it on.
#[tokio::test]
async fn auto_merge_turns_on_and_off() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let tools = tempfile::tempdir().unwrap();
    let gh = Gh::Ready(fake_gh(tools.path()));
    let auto = |enabled: bool| {
        let mut request = request("autoMerge");
        request.pull_request_url = Some(PR.into());
        request.method = Some("merge".into());
        request.enabled = Some(enabled);
        request
    };
    view(
        tools.path(),
        "OPEN",
        false,
        "MERGEABLE",
        "BLOCKED",
        "pending",
    );
    let on = perform(&work, &slugs(), &auto(true), &gh).await;
    assert_eq!(
        on.message,
        "Auto-merge is on: #41 will merge when its requirements pass."
    );
    assert_eq!(
        calls(tools.path()).last().unwrap(),
        &format!("pr merge {PR} --auto --merge")
    );
    fs::remove_file(tools.path().join("calls")).ok();
    let off = perform(&work, &slugs(), &auto(false), &gh).await;
    assert_eq!(off.message, "Auto-merge is off for #41.");
    assert_eq!(
        calls(tools.path()),
        [format!("pr merge {PR} --disable-auto")]
    );

    view(tools.path(), "OPEN", true, "MERGEABLE", "DRAFT", "pending");
    assert_eq!(
        refused(&perform(&work, &slugs(), &auto(true), &gh).await).0,
        "draft"
    );
    view(
        tools.path(),
        "OPEN",
        false,
        "CONFLICTING",
        "DIRTY",
        "passing",
    );
    assert_eq!(
        refused(&perform(&work, &slugs(), &auto(true), &gh).await).0,
        "conflicts"
    );
    fs::write(tools.path().join("fail.cmd"), "pr merge\n").unwrap();
    fs::write(
        tools.path().join("fail"),
        "GraphQL: Auto merge is not allowed for this repository (enablePullRequestAutoMerge)\n",
    )
    .unwrap();
    view(
        tools.path(),
        "OPEN",
        false,
        "MERGEABLE",
        "BLOCKED",
        "pending",
    );
    assert_eq!(
        refused(&perform(&work, &slugs(), &auto(true), &gh).await).0,
        "autoMergeNotAllowed"
    );
}

// A local write is refused when its result would depend on a program or file
// the repository's own settings name; other program settings are not loaded,
// and only data settings are copied.
#[tokio::test]
async fn repository_settings_that_run_programs_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    assert!(
        WriteView::prepare(&work, true).await.is_ok(),
        "a clone's own settings are safe"
    );
    sh(
        &work,
        &["config", "core.sshCommand", "touch /tmp/owned; ssh"],
    );
    let refusal = WriteView::prepare(&work, true).await.err().unwrap();
    assert_eq!(refusal.0, "unsafeConfig");
    assert!(refusal.1.contains("(core.sshcommand)"), "{}", refusal.1);
    assert!(
        WriteView::prepare(&work, false).await.is_ok(),
        "GitHub-only actions never load it either"
    );
    for key in [
        "credential.https://x.helper",
        "filter.lfs.clean",
        "include.path",
        "remote.origin.uploadpack",
        // Commit signing runs this when no signing key is set.
        "gpg.ssh.defaultKeyCommand",
        "gpg.ssh.program",
    ] {
        assert_eq!(local_setting(key), Local::Refuse, "{key}");
    }
    for key in [
        "core.hooksPath",
        "core.fsmonitor",
        "diff.x.textconv",
        "merge.x.driver",
        "core.alternateRefsCommand",
        "core.bare",
        "remote.origin.proxy",
        "submodule.recurse",
    ] {
        assert_eq!(local_setting(key), Local::Drop, "{key}");
    }
    for key in [
        "user.name",
        "gpg.format",
        "commit.gpgSign",
        "remote.Up.Stream.url",
        "branch.main.merge",
        "url.git@github.com:.insteadOf",
    ] {
        assert_eq!(local_setting(key), Local::Copy, "{key}");
    }
}

/// A dirty submodule whose tracked attributes select a marker-writing filter.
/// Install the programs only after setup, so a marker proves the action ran it.
fn dirty_filtered_submodule(dir: &Path, path: &str) -> (PathBuf, PathBuf) {
    let seed = dir.join("sub-seed");
    sh(dir, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
    identity(&seed);
    fs::write(seed.join("file"), "sub\n").unwrap();
    fs::write(seed.join(".gitattributes"), "* filter=agent\n").unwrap();
    sh(&seed, &["add", "."]);
    sh(&seed, &["commit", "-q", "-m", "sub"]);
    let (work, _) = cloned(dir);
    sh(
        &work,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            "--name",
            "fixture-sub",
            seed.to_str().unwrap(),
            path,
        ],
    );
    sh(&work, &["commit", "-q", "-m", "add sub"]);
    sh(&work, &["config", "submodule.recurse", "true"]);
    sh(&work, &["config", "submodule.fixture-sub.ignore", "none"]);
    let marker = dir.join("ran");
    let sub = work.join(path);
    for key in ["filter.agent.clean", "filter.agent.smudge"] {
        sh(
            &sub,
            &["config", key, &format!("touch '{}'; cat", marker.display())],
        );
    }
    fs::write(sub.join("file"), "dirty submodule\n").unwrap();
    (work, marker)
}

// The real Commit action must stage parent files without inspecting the dirty
// submodule's contents or running the clean filter its own config selects.
#[tokio::test]
async fn commit_with_unstaged_never_runs_a_submodule_filter() {
    for path in ["sub", "nested/sub [*] ? :name\nline"] {
        let dir = tempfile::tempdir().unwrap();
        let (work, marker) = dirty_filtered_submodule(dir.path(), path);
        assert!(WriteView::prepare(&work, true).await.is_ok());
        fs::write(work.join("README"), "parent edit\n").unwrap();
        let mut commit = request("commit");
        commit.message = Some("Commit parent changes".into());
        commit.include_unstaged = Some(true);
        let outcome = perform(&work, &[], &commit, &Gh::Missing).await;
        assert!(
            !marker.exists(),
            "Commit ran a submodule's filter: {outcome:?}"
        );
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(sh(&work, &["show", "HEAD:README"]), "parent edit");
        assert_eq!(
            fs::read_to_string(work.join(path).join("file")).unwrap(),
            "dirty submodule\n"
        );
        let only_submodule = perform(&work, &[], &commit, &Gh::Missing).await;
        assert_eq!(refused(&only_submodule).0, "nothingToCommit");
        assert!(!marker.exists(), "an otherwise empty Commit ran the filter");
    }
}

// A staged gitlink can still be committed without examining its working tree;
// Include unstaged must neither update it nor discard the user's staged choice.
#[tokio::test]
async fn commit_preserves_staged_submodule_updates_without_running_filters() {
    for include in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (work, marker) = dirty_filtered_submodule(dir.path(), "sub");
        let sub = work.join("sub");
        identity(&sub);
        sh(
            &sub,
            &[
                "-c",
                "filter.agent.clean=cat",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "next sub",
            ],
        );
        let staged = sh(&sub, &["rev-parse", "HEAD"]);
        sh(
            &work,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{staged},sub"),
            ],
        );
        sh(
            &sub,
            &[
                "-c",
                "filter.agent.clean=cat",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "later sub",
            ],
        );
        assert!(!marker.exists(), "fixture setup ran the filter");
        let mut commit = request("commit");
        commit.message = Some("Commit staged submodule".into());
        commit.include_unstaged = Some(include);
        let outcome = perform(&work, &[], &commit, &Gh::Missing).await;
        assert!(
            !marker.exists(),
            "Commit ran a submodule's filter: {outcome:?}"
        );
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(sh(&work, &["rev-parse", "HEAD:sub"]), staged);
    }
}

// These local write operations aren't exposed by this action endpoint. A
// request must fail before invoking Git, even with a hostile dirty submodule.
#[tokio::test]
async fn unsupported_writes_never_enter_submodules() {
    let dir = tempfile::tempdir().unwrap();
    let (work, marker) = dirty_filtered_submodule(dir.path(), "sub");
    let head = sh(&work, &["rev-parse", "HEAD"]);
    fs::write(work.join("README"), "parent edit\n").unwrap();
    for action in ["stash", "checkout", "switch", "restore", "rebase"] {
        let outcome = perform(&work, &[], &request(action), &Gh::Missing).await;
        assert_eq!(refused(&outcome).0, "invalid", "{action}");
        assert!(!marker.exists(), "{action} ran a submodule's filter");
        assert_eq!(sh(&work, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(work.join("README")).unwrap(),
            "parent edit\n"
        );
    }
}

// Pull's fetch/fast-forward merge can update the parent gitlink while leaving
// the dirty submodule at its old commit; Push must not recurse either.
#[tokio::test]
async fn push_and_pull_leave_dirty_submodules_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (work, marker) = dirty_filtered_submodule(dir.path(), "sub");
    let sub_head = sh(&work.join("sub"), &["rev-parse", "HEAD"]);
    let mut push = request("push");
    push.confirm_default_branch = Some(true);
    let pushed = perform(&work, &[], &push, &Gh::Missing).await;
    assert!(
        !marker.exists(),
        "Push ran a submodule's filter: {pushed:?}"
    );
    assert!(pushed.ok, "{pushed:?}");

    let remote = dir.path().join("remote.git");
    let seed = dir.path().join("seed");
    sh(&seed, &["fetch", "-q", remote.to_str().unwrap(), "main"]);
    sh(&seed, &["reset", "--hard", "FETCH_HEAD"]);
    let sub_seed = dir.path().join("sub-seed");
    sh(
        &sub_seed,
        &["commit", "-q", "--allow-empty", "-m", "new sub"],
    );
    let next_sub = sh(&sub_seed, &["rev-parse", "HEAD"]);
    sh(
        &seed,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{next_sub},sub"),
        ],
    );
    fs::write(seed.join("README"), "remote edit\n").unwrap();
    sh(&seed, &["add", "README"]);
    sh(&seed, &["commit", "-q", "-m", "remote parent and gitlink"]);
    sh(&seed, &["push", "-q", remote.to_str().unwrap(), "main"]);
    assert!(!marker.exists(), "fixture setup ran the filter");

    let pulled = perform(&work, &[], &request("pull"), &Gh::Missing).await;
    assert!(
        !marker.exists(),
        "Pull ran a submodule's filter: {pulled:?}"
    );
    assert!(pulled.ok, "{pulled:?}");
    assert_eq!(sh(&work, &["rev-parse", "HEAD:sub"]), next_sub);
    assert_eq!(sh(&work.join("sub"), &["rev-parse", "HEAD"]), sub_head);
    assert_eq!(
        fs::read_to_string(work.join("README")).unwrap(),
        "remote edit\n"
    );
    assert_eq!(
        fs::read_to_string(work.join("sub/file")).unwrap(),
        "dirty submodule\n"
    );
}

// PR creation/merges/auto-merge go through gh without a local merge or checkout.
// Exercise their local preparation, including rebase, with the hostile submodule.
#[tokio::test]
async fn pull_request_writes_never_enter_dirty_submodules() {
    let dir = tempfile::tempdir().unwrap();
    let (work, marker) = dirty_filtered_submodule(dir.path(), "sub");
    let prepared = WriteView::prepare(&work, true).await.unwrap();
    let switched = git(&prepared, &["switch", "-q", "-c", "feature"], LOCAL_TIMEOUT)
        .await
        .unwrap();
    assert!(switched.success);
    // The switch reaches the repository's HEAD as the view closes.
    drop(prepared);
    let pushed = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(pushed.ok, "{pushed:?}");
    assert!(!marker.exists(), "fixture branch setup ran the filter");
    let head = sh(&work, &["rev-parse", "HEAD"]);
    let tools = tempfile::tempdir().unwrap();
    let gh = Gh::Ready(fake_gh(tools.path()));
    view(tools.path(), "OPEN", false, "MERGEABLE", "CLEAN", "passing");
    let mut create = request("createPullRequest");
    create.title = Some("Parent changes".into());
    let created = perform(&work, &slugs(), &create, &gh).await;
    assert!(created.ok, "{created:?}");
    assert!(!marker.exists(), "Create pull request ran the filter");
    for method in ["squash", "merge", "rebase"] {
        let mut merge = request("merge");
        merge.pull_request_url = Some(PR.into());
        merge.method = Some(method.into());
        let outcome = perform(&work, &slugs(), &merge, &gh).await;
        assert!(
            !marker.exists(),
            "{method} ran a submodule's filter: {outcome:?}"
        );
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(sh(&work, &["rev-parse", "HEAD"]), head);
    }
    for enabled in [true, false] {
        let mut auto = request("autoMerge");
        auto.pull_request_url = Some(PR.into());
        auto.enabled = Some(enabled);
        let outcome = perform(&work, &slugs(), &auto, &gh).await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(!marker.exists(), "Auto-merge ran the filter: {outcome:?}");
        assert_eq!(sh(&work, &["rev-parse", "HEAD"]), head);
    }
}

// A submodule's own config is never checked, so Cupola's Git never recurses
// into submodules: an agent that turns recursion on and puts a program in a
// submodule's settings gets nothing run by Pull.
#[tokio::test]
async fn git_never_runs_a_program_from_a_submodules_settings() {
    let dir = tempfile::tempdir().unwrap();
    let allow = ["-c", "protocol.file.allow=always"];
    let sub_remote = dir.path().join("sub.git");
    sh(
        dir.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            sub_remote.to_str().unwrap(),
        ],
    );
    let sub_seed = dir.path().join("sub-seed");
    sh(
        dir.path(),
        &["init", "-q", "-b", "main", sub_seed.to_str().unwrap()],
    );
    identity(&sub_seed);
    fs::write(sub_seed.join("file"), "sub\n").unwrap();
    sh(&sub_seed, &["add", "."]);
    sh(&sub_seed, &["commit", "-q", "-m", "sub"]);
    sh(
        &sub_seed,
        &["push", "-q", sub_remote.to_str().unwrap(), "main"],
    );
    let (work, _) = cloned(dir.path());
    sh(
        &work,
        &[
            allow[0],
            allow[1],
            "submodule",
            "add",
            "-q",
            sub_remote.to_str().unwrap(),
            "sub",
        ],
    );
    sh(&work, &["commit", "-q", "-m", "add sub"]);
    // What an agent can write: recursion in the repository's config (not a
    // program, so not refused) and anything in the submodule's own config.
    sh(&work, &["config", "fetch.recurseSubmodules", "true"]);
    sh(&work, &["config", "submodule.recurse", "true"]);
    let marker = dir.path().join("ran");
    let sub = work.join("sub");
    sh(&sub, &["config", "protocol.file.allow", "always"]);
    sh(
        &sub,
        &[
            "config",
            "remote.origin.uploadpack",
            &format!("touch {}; git-upload-pack", marker.display()),
        ],
    );
    let view = WriteView::prepare(&work, true).await.unwrap();
    let fetched = git(&view, &["fetch", "--quiet", "origin"], NETWORK_TIMEOUT)
        .await
        .unwrap();
    assert!(fetched.success);
    assert!(!marker.exists(), "a submodule's uploadpack ran");
}

// A push to a repository on this Mac runs Git there, with that repository's
// own hooks and settings and none of Cupola's: an agent that points the remote
// at a repository it prepared gets nothing run by Push.
#[tokio::test]
async fn push_never_runs_a_program_from_a_local_remotes_hooks_or_settings() {
    let dir = tempfile::tempdir().unwrap();
    let (work, original) = cloned(dir.path());
    let marker = dir.path().join("ran");
    let hook = format!("#!/bin/sh\ntouch {}\n", marker.display());
    let install_hooks = |git_dir: &Path| {
        for name in [
            "pre-receive",
            "update",
            "post-receive",
            "reference-transaction",
        ] {
            let path = git_dir.join("hooks").join(name);
            fs::write(&path, &hook).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    };

    // A bare repository with hooks; `remote.origin.url` is an ordinary setting.
    let bare = dir.path().join("agent.git");
    sh(
        dir.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    install_hooks(&bare);
    fs::write(
        bare.join("objects/info/alternates"),
        format!("{}\n", original.join("objects").display()),
    )
    .unwrap();
    sh(
        &bare,
        &[
            "config",
            "core.alternateRefsCommand",
            &format!("touch {}; git for-each-ref", marker.display()),
        ],
    );
    sh(
        &work,
        &["config", "remote.origin.url", bare.to_str().unwrap()],
    );
    sh(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("a.txt"), "a\n").unwrap();
    sh(&work, &["add", "a.txt"]);
    sh(&work, &["commit", "-q", "-m", "a"]);
    assert!(WriteView::prepare(&work, true).await.is_ok());
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(!marker.exists(), "the remote's hooks ran: {outcome:?}");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        sh(&bare, &["rev-parse", "refs/heads/feature"]),
        sh(&work, &["rev-parse", "HEAD"])
    );

    // The same named as `file://`, and as the branch's remote itself.
    sh(
        &work,
        &[
            "config",
            "remote.origin.url",
            &format!("file://{}", bare.display()),
        ],
    );
    fs::write(work.join("b.txt"), "b\n").unwrap();
    sh(&work, &["add", "b.txt"]);
    sh(&work, &["commit", "-q", "-m", "b"]);
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(!marker.exists(), "the remote's hooks ran: {outcome:?}");
    assert!(outcome.ok, "{outcome:?}");
    sh(
        &work,
        &["config", "branch.feature.remote", bare.to_str().unwrap()],
    );
    fs::write(work.join("c.txt"), "c\n").unwrap();
    sh(&work, &["add", "c.txt"]);
    sh(&work, &["commit", "-q", "-m", "c"]);
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(!marker.exists(), "the remote's hooks ran: {outcome:?}");
    assert!(outcome.ok, "{outcome:?}");

    // A checked-out repository that updates its files on push, through a
    // filter its own settings name.
    let checkout = dir.path().join("agent-checkout");
    sh(
        dir.path(),
        &["init", "-q", "-b", "feature", checkout.to_str().unwrap()],
    );
    sh(
        &checkout,
        &["config", "receive.denyCurrentBranch", "updateInstead"],
    );
    sh(
        &checkout,
        &[
            "config",
            "filter.agent.smudge",
            &format!("touch {}; cat", marker.display()),
        ],
    );
    fs::write(checkout.join(".git/info/attributes"), "* filter=agent\n").unwrap();
    sh(&work, &["config", "branch.feature.remote", "origin"]);
    sh(
        &work,
        &["config", "remote.origin.url", checkout.to_str().unwrap()],
    );
    sh(
        &work,
        &["config", "branch.feature.merge", "refs/heads/feature"],
    );
    fs::write(work.join("d.txt"), "d\n").unwrap();
    sh(&work, &["add", "d.txt"]);
    sh(&work, &["commit", "-q", "-m", "d"]);
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(!marker.exists(), "the remote's filter ran: {outcome:?}");
    assert!(!outcome.ok, "{outcome:?}");

    // Inspect Git's effective push destinations, including pushurl and URL
    // rewrites; neither may bypass local-receiver hardening.
    sh(
        &work,
        &[
            "config",
            "remote.origin.url",
            "https://unused.invalid/repo.git",
        ],
    );
    sh(
        &work,
        &["config", "remote.origin.pushurl", bare.to_str().unwrap()],
    );
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(!marker.exists(), "pushurl ran a program: {outcome:?}");
    assert!(outcome.ok, "{outcome:?}");
    sh(&work, &["config", "--unset-all", "remote.origin.pushurl"]);
    sh(
        &work,
        &[
            "config",
            &format!("url.{}.pushInsteadOf", bare.display()),
            "fixture:",
        ],
    );
    sh(&work, &["config", "remote.origin.url", "fixture:"]);
    fs::write(work.join("e.txt"), "e\n").unwrap();
    sh(&work, &["add", "e.txt"]);
    sh(&work, &["commit", "-q", "-m", "e"]);
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert!(
        !marker.exists(),
        "a rewritten URL ran a program: {outcome:?}"
    );
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        sh(&bare, &["rev-parse", "refs/heads/feature"]),
        sh(&work, &["rev-parse", "HEAD"])
    );

    // Do not partially push a remote with both local and network destinations.
    sh(
        &work,
        &[
            "config",
            "--add",
            "remote.origin.pushurl",
            bare.to_str().unwrap(),
        ],
    );
    sh(
        &work,
        &[
            "config",
            "--add",
            "remote.origin.pushurl",
            "https://unused.invalid/repo.git",
        ],
    );
    fs::write(work.join("f.txt"), "f\n").unwrap();
    sh(&work, &["add", "f.txt"]);
    sh(&work, &["commit", "-q", "-m", "f"]);
    let outcome = perform(&work, &[], &request("push"), &Gh::Missing).await;
    assert_eq!(refused(&outcome).0, "unsafeRemote", "{outcome:?}");
    assert_eq!(
        sh(&bare, &["rev-parse", "refs/heads/feature"]),
        sh(&work, &["rev-parse", "HEAD~1"])
    );
    assert!(!marker.exists());
}

async fn post(
    state: &crate::AppState,
    conversation: &str,
    body: serde_json::Value,
    owner: bool,
) -> (StatusCode, serde_json::Value) {
    use axum::{body::to_bytes, body::Body, http::Request};
    use tower::ServiceExt;
    let mut request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/conversations/{conversation}/repos/actions"
        ))
        .header("content-type", "application/json")
        .header("origin", &state.public_origin);
    if owner {
        request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

// Over HTTP: the owner only; the repository must be the index's and wholly
// inside the Project; nothing runs while the agent works, while another
// action holds the repository, or where its settings run programs.
#[tokio::test]
async fn route_admits_only_project_repositories_one_at_a_time_and_never_during_a_turn() {
    let (fixture, state) = crate::ingestion::tests::fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    crate::test_support::project_thread(
        &state.store,
        "actions-chat",
        "Actions",
        work.to_str().unwrap(),
        "owner",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    // A Project folder inside a larger repository: that repository stays read only.
    let nested = work.join("docs");
    fs::create_dir(&nested).unwrap();
    crate::test_support::project_thread(
        &state.store,
        "nested-chat",
        "Nested",
        nested.to_str().unwrap(),
        "owner",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    let id = format!(
        "local:{}",
        thread_repos::short_hash(&work.to_string_lossy())
    );
    fs::write(work.join("README"), "route\n").unwrap();
    let commit = serde_json::json!({
        "repositoryId": id, "root": "workspace", "action": "commit",
        "message": "From the phone", "includeUnstaged": true,
    });

    let (status, _) = post(&state, "actions-chat", commit.clone(), false).await;
    assert_ne!(status, StatusCode::OK, "owner only");
    let (status, body) = post(&state, "nested-chat", commit.clone(), true).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (StatusCode::CONFLICT, Some("outsideProject"))
    );
    let mut stale = commit.clone();
    stale["repositoryId"] = "local:0123456789abcdef".into();
    let (status, body) = post(&state, "actions-chat", stale, true).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (StatusCode::CONFLICT, Some("stale"))
    );

    {
        let _held = RepoLock::acquire(&work).unwrap();
        let (status, body) = post(&state, "actions-chat", commit.clone(), true).await;
        assert_eq!(
            (status, body["reason"].as_str()),
            (StatusCode::CONFLICT, Some("busy"))
        );
        assert!(
            RepoLock::acquire(&work).is_none(),
            "one action per repository"
        );
    }

    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        fixture.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) VALUES('running','owner','running','work','hash','actions-chat','streaming','1')")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = post(&state, "actions-chat", commit.clone(), true).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (StatusCode::CONFLICT, Some("agentRunning"))
    );
    crate::tests::validate_http_contract("gitActionOutcome", &body);
    sqlx::query("UPDATE messages SET state='completed' WHERE id='running'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    sh(&work, &["config", "core.sshCommand", "ssh -v"]);
    let (_, body) = post(&state, "actions-chat", commit.clone(), true).await;
    assert_eq!(body["reason"], "unsafeConfig");
    sh(&work, &["config", "--unset", "core.sshCommand"]);

    // Exercise signing refusal over the real route, not just key classification.
    let signing_marker = dir.path().join("signing-command-ran");
    let before = sh(&work, &["rev-parse", "HEAD"]);
    sh(&work, &["config", "gpg.format", "ssh"]);
    sh(
        &work,
        &[
            "config",
            "gpg.ssh.defaultKeyCommand",
            &format!("touch {}; echo invalid-key", signing_marker.display()),
        ],
    );
    sh(&work, &["config", "commit.gpgsign", "true"]);
    let (status, body) = post(&state, "actions-chat", commit.clone(), true).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (StatusCode::CONFLICT, Some("unsafeConfig"))
    );
    assert!(!signing_marker.exists());
    assert_eq!(sh(&work, &["rev-parse", "HEAD"]), before);
    sh(&work, &["config", "--unset", "gpg.format"]);
    sh(&work, &["config", "--unset", "gpg.ssh.defaultKeyCommand"]);
    sh(&work, &["config", "commit.gpgsign", "false"]);

    let (status, body) = post(&state, "actions-chat", commit, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(sh(&work, &["log", "-1", "--format=%s"]), "From the phone");
    crate::tests::validate_http_contract("gitActionOutcome", &body);
    crate::tests::validate_http_contract(
        "gitActionRequest",
        &serde_json::json!({"repositoryId": id, "root": "workspace", "action": "push", "confirmDefaultBranch": true}),
    );
}

// The shared fixture the app decodes matches the HTTP contract.
#[test]
fn shared_fixture_matches_the_contract() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../packages/protocol/fixtures/git-actions-v1.json"
    ))
    .unwrap();
    for outcome in fixture["outcomes"].as_array().unwrap() {
        crate::tests::validate_http_contract("gitActionOutcome", outcome);
    }
    let mut state = fixture["mergeState"].clone();
    // The app skips a method it doesn't know; the Mac sends only these.
    state["methods"] = serde_json::json!(["squash", "rebase"]);
    crate::tests::validate_http_contract("pullRequestMergeState", &state);
}

/// A Project thread on `work`, for exercising actions over the real route.
async fn routed(work: &Path, chat: &str) -> (tempfile::TempDir, crate::AppState, String) {
    let (fixture, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::project_thread(
        &state.store,
        chat,
        "Actions",
        work.to_str().unwrap(),
        "owner",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    let id = format!(
        "local:{}",
        thread_repos::short_hash(&work.to_string_lossy())
    );
    (fixture, state, id)
}

/// A config value that appends one byte to `marker` each time Git runs it.
fn counting(marker: &Path, then: &str) -> String {
    format!("\"printf x >> '{}'; {then}\"", marker.display())
}

// An agent's process can keep rewriting the repository's config while Cupola
// checks it and runs Git. The route's refusal and every Git process an action
// starts must use one snapshot, so an armed setting is refused or never
// loaded: exercise the real route 40 times per action against atomic swaps.
#[tokio::test]
async fn actions_never_run_a_program_the_config_gains_after_cupola_checks_it() {
    config_race_over_the_route(40).await;
}

// The same on a computer that can't make a symbolic link (Windows without
// Developer Mode): the view reaches packed-refs and shallow by copy instead.
#[tokio::test]
async fn actions_without_symbolic_links_never_run_a_program_the_config_gains() {
    force(&[Reach::Copies]);
    config_race_over_the_route(20).await;
}

/// Takes the given ways to build a write view for the rest of this test.
fn force(reaches: &'static [Reach]) {
    FORCED_REACHES.with(|forced| forced.set(Some(reaches)));
}

async fn config_race_over_the_route(rounds: usize) {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    fs::write(work.join(".gitattributes"), "* filter=agent\n").unwrap();
    sh(&work, &["add", ".gitattributes"]);
    sh(&work, &["commit", "-q", "-m", "attributes"]);
    let (_fixture, state, id) = routed(&work, "race-chat").await;
    let marker = dir.path().join("ran");
    let config = work.join(".git/config");
    let safe = fs::read_to_string(&config).unwrap();
    let armed = format!(
        "{safe}[filter \"agent\"]\n\tclean = {clean}\n\tsmudge = {clean}\n[remote \"origin\"]\n\tuploadpack = {upload}\n[gpg]\n\tprogram = {sign}\n[commit]\n\tgpgsign = true\n",
        clean = counting(&marker, "cat"),
        upload = counting(&marker, "git-upload-pack \\\"$@\\\""),
        sign = counting(&marker, "false"),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let swapped = config.clone();
    let writer = std::thread::spawn(move || {
        let next = swapped.with_extension("next");
        while !stopped.load(Ordering::Relaxed) {
            for bytes in [&safe, &armed] {
                fs::write(&next, bytes).unwrap();
                fs::rename(&next, &swapped).unwrap();
                std::thread::sleep(Duration::from_micros(300));
            }
        }
    });
    let mut tally = std::collections::BTreeMap::<String, usize>::new();
    for round in 0..rounds {
        fs::write(work.join("README"), format!("round {round}\n")).unwrap();
        for body in [
            serde_json::json!({
                "repositoryId": id, "root": "workspace", "action": "commit",
                "message": format!("Round {round}"), "includeUnstaged": true,
            }),
            serde_json::json!({"repositoryId": id, "root": "workspace", "action": "pull"}),
        ] {
            let (_, outcome) = post(&state, "race-chat", body.clone(), true).await;
            let result = if outcome["ok"] == true {
                "ok".to_owned()
            } else {
                outcome["reason"].as_str().unwrap_or("none").to_owned()
            };
            *tally
                .entry(format!("{} {result}", body["action"].as_str().unwrap()))
                .or_default() += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let executions = fs::read(&marker).unwrap_or_default().len();
    eprintln!("config race over the route: {tally:?}, {executions} program executions");
    assert_eq!(executions, 0, "an action loaded a racing program setting");
    for action in ["commit", "pull"] {
        assert!(
            tally.get(&format!("{action} ok")).copied().unwrap_or(0) > 0,
            "{action} never ran, so the race was not exercised: {tally:?}"
        );
    }
    let unexpected: Vec<_> = tally
        .keys()
        .filter(|key| !key.ends_with(" ok") && !key.ends_with(" unsafeConfig"))
        .collect();
    assert!(unexpected.is_empty(), "{tally:?}");
}

// The view is fixed when it is prepared: every way the repository's config can
// name a program, written afterwards (directly, included, per worktree or
// through attributes), is never loaded by a commit, fetch or fast-forward.
#[tokio::test]
async fn a_prepared_write_view_never_loads_settings_written_after_it() {
    for reach in [Reach::Links, Reach::Copies] {
        later_settings_never_load(reach).await;
    }
}

async fn later_settings_never_load(reach: Reach) {
    let dir = tempfile::tempdir().unwrap();
    let (work, remote) = cloned(dir.path());
    let other = dir.path().join("other");
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    identity(&other);
    fs::write(other.join("b.txt"), "b\n").unwrap();
    sh(&other, &["add", "b.txt"]);
    sh(&other, &["commit", "-q", "-m", "b"]);
    sh(&other, &["push", "-q", "origin", "main"]);
    let view = WriteView::prepare_reaching(&work, true, &[reach])
        .await
        .unwrap();

    let marker = dir.path().join("ran");
    let program = |then: &str| counting(&marker, then);
    let hooks = dir.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    for name in ["pre-commit", "commit-msg", "post-commit", "post-merge"] {
        let hook = hooks.join(name);
        fs::write(
            &hook,
            format!("#!/bin/sh\nprintf x >> '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let drivers = format!(
        "[filter \"agent\"]\n\tclean = {cat}\n\tsmudge = {cat}\n\tprocess = {cat}\n[diff \"agent\"]\n\ttextconv = {cat}\n\tcommand = {cat}\n[merge \"agent\"]\n\tdriver = {cat}\n",
        cat = program("cat")
    );
    let included = dir.path().join("included.config");
    fs::write(&included, &drivers).unwrap();
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(work.join(".git/config"))
        .unwrap();
    io::Write::write_all(
        &mut config,
        format!(
            "[include]\n\tpath = {}\n{drivers}[core]\n\thooksPath = {}\n\tfsmonitor = {fsmonitor}\n\talternateRefsCommand = {alternates}\n\tsshCommand = {fsmonitor}\n[remote \"origin\"]\n\tuploadpack = {upload}\n[gpg]\n\tprogram = {fails}\n[commit]\n\tgpgsign = true\n[extensions]\n\tworktreeConfig = true\n",
            included.display(),
            hooks.display(),
            fsmonitor = program("true"),
            alternates = program("git --git-dir=\\\"$1\\\" for-each-ref --format='%(objectname)'"),
            upload = program("git-upload-pack \\\"$@\\\""),
            fails = program("false"),
        )
        .as_bytes(),
    )
    .unwrap();
    fs::write(work.join(".git/config.worktree"), &drivers).unwrap();
    fs::write(
        work.join(".git/objects/info/alternates"),
        format!("{}\n", remote.join("objects").display()),
    )
    .unwrap();
    for attributes in [
        work.join(".gitattributes"),
        work.join(".git/info/attributes"),
    ] {
        fs::write(attributes, "* filter=agent diff=agent merge=agent\n").unwrap();
    }
    fs::write(work.join("README"), "edited\n").unwrap();

    for args in [
        &["add", "--all"][..],
        &["commit", "--quiet", "-m", "after the view"],
        &["fetch", "--quiet", "--no-tags", "origin", "refs/heads/main"],
        &["status", "--porcelain"],
    ] {
        let ran = git(&view, args, NETWORK_TIMEOUT).await.unwrap();
        assert!(!marker.exists(), "git {args:?} loaded a later setting");
        assert!(ran.success, "git {args:?}: {}", ran.stderr);
    }
    // A diverged branch: the fast-forward is refused, not merged by a driver.
    let merged = git(&view, &["merge", "--ff-only", "FETCH_HEAD"], LOCAL_TIMEOUT)
        .await
        .unwrap();
    assert!(!merged.success);
    assert!(!marker.exists(), "merge loaded a later setting");
    drop(view);
    assert_eq!(sh(&work, &["log", "-1", "--format=%s"]), "after the view");
    assert_eq!(
        sh(&work, &["show", "HEAD:README"]),
        "edited",
        "the commit holds the file's own bytes"
    );
    // Outside the view, the same repository's own config does run them.
    fs::write(work.join("README"), "edited again\n").unwrap();
    let plain = crate::test_git::git()
        .arg("-C")
        .arg(&work)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        marker.exists(),
        "the fixture never ran a program: {plain:?}"
    );
}

// Husky keeps hooks in the repository's config and SSH signing names its key
// and format there: both are ordinary. Commit (signed, hooks not run), push a
// new branch with its tracking recorded, and pull, all over the real route.
#[tokio::test]
async fn husky_and_ssh_signing_repositories_commit_push_and_pull_normally() {
    ordinary_round_trip().await;
}

#[tokio::test]
async fn without_symbolic_links_husky_and_ssh_signing_repositories_work_normally() {
    force(&[Reach::Copies]);
    ordinary_round_trip().await;
}

async fn ordinary_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (work, remote) = cloned(dir.path());
    let (_fixture, state, id) = routed(&work, "ordinary-chat").await;
    let marker = dir.path().join("hook-ran");
    let husky = work.join(".husky/_");
    fs::create_dir_all(&husky).unwrap();
    for name in ["pre-commit", "commit-msg", "pre-push", "post-merge"] {
        let hook = husky.join(name);
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    sh(&work, &["config", "core.hooksPath", ".husky/_"]);
    let key = dir.path().join("signing");
    let keygen = std::process::Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "test@example.com",
            "-f",
        ])
        .arg(&key)
        .output()
        .unwrap();
    assert!(keygen.status.success(), "{keygen:?}");
    sh(&work, &["config", "gpg.format", "ssh"]);
    sh(&work, &["config", "user.signingKey", key.to_str().unwrap()]);
    sh(&work, &["config", "commit.gpgSign", "true"]);
    sh(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("a.txt"), "a\n").unwrap();

    let action = |action: &str| {
        let mut body =
            serde_json::json!({"repositoryId": id, "root": "workspace", "action": action});
        if action == "commit" {
            body["message"] = "Signed from the phone".into();
            body["includeUnstaged"] = true.into();
        }
        body
    };
    let (status, body) = post(&state, "ordinary-chat", action("commit"), true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let raw = sh(&work, &["cat-file", "commit", "HEAD"]);
    assert!(raw.contains("-----BEGIN SSH SIGNATURE-----"), "{raw}");
    assert!(
        raw.contains("author Cupola Test <test@example.com>"),
        "{raw}"
    );
    let signers = dir.path().join("allowed_signers");
    let public = fs::read_to_string(key.with_extension("pub")).unwrap();
    fs::write(&signers, format!("test@example.com {public}")).unwrap();
    sh(
        &work,
        &[
            "-c",
            &format!("gpg.ssh.allowedSignersFile={}", signers.display()),
            "verify-commit",
            "HEAD",
        ],
    );

    let (status, body) = post(&state, "ordinary-chat", action("push"), true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["message"],
        "Pushed feature to origin and set it to track origin/feature."
    );
    assert_eq!(
        sh(&work, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feature"
    );
    assert_eq!(
        sh(&remote, &["rev-parse", "refs/heads/feature"]),
        sh(&work, &["rev-parse", "HEAD"])
    );

    let other = dir.path().join("other");
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            "-b",
            "feature",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    identity(&other);
    fs::write(other.join("b.txt"), "b\n").unwrap();
    sh(&other, &["add", "b.txt"]);
    sh(&other, &["commit", "-q", "-m", "b"]);
    sh(&other, &["push", "-q", "origin", "feature"]);
    let (status, body) = post(&state, "ordinary-chat", action("pull"), true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], "Pulled 1 commit from origin/feature.");
    assert!(work.join("b.txt").exists());
    assert!(!marker.exists(), "a Husky hook ran");
    assert!(
        !work.join(".git/shallow.lock").exists(),
        "an action left Git's lock behind"
    );
}

// gh runs Git in the repository too (for example `git status` before opening
// a pull request). Its Git must see the write view, never the repository's
// own programs, while still seeing the repository's files and remotes.
#[tokio::test]
async fn pull_request_actions_give_ghs_git_the_write_view() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    fs::write(work.join(".gitattributes"), "* filter=agent\n").unwrap();
    sh(&work, &["add", ".gitattributes"]);
    sh(&work, &["commit", "-q", "-m", "attributes"]);
    sh(&work, &["checkout", "-q", "-b", "feature"]);
    sh(&work, &["push", "-q", "-u", "origin", "feature"]);
    let marker = dir.path().join("ran");
    sh(
        &work,
        &[
            "config",
            "filter.agent.clean",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    sh(
        &work,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch '{}'; true", marker.display()),
        ],
    );
    fs::write(work.join("README"), "uncommitted\n").unwrap();
    let tools = tempfile::tempdir().unwrap();
    let gh = tools.path().join("gh");
    fs::write(
        &gh,
        r#"#!/bin/sh
here="$(dirname "$0")"
git status --porcelain >> "$here/git-seen"
git config --get remote.origin.url >> "$here/git-seen"
case "$1 $2" in
  "pr create") echo "https://github.com/o/r/pull/41" ;;
  "pr view") cat "$here/view.json" ;;
  "repo view") cat "$here/repo.json" ;;
  "pr merge") exit 0 ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        tools.path().join("repo.json"),
        r#"{"squashMergeAllowed":true,"mergeCommitAllowed":true,"rebaseMergeAllowed":true,"viewerDefaultMergeMethod":"SQUASH"}"#,
    )
    .unwrap();
    view(tools.path(), "OPEN", false, "MERGEABLE", "CLEAN", "passing");
    let gh = Gh::Ready(gh);
    let mut create = request("createPullRequest");
    create.title = Some("Feature".into());
    let mut merge = request("merge");
    merge.pull_request_url = Some(PR.into());
    merge.method = Some("squash".into());
    let mut auto = request("autoMerge");
    auto.pull_request_url = Some(PR.into());
    auto.enabled = Some(false);
    for action in [create, merge, auto] {
        let outcome = perform(&work, &slugs(), &action, &gh).await;
        assert!(
            !marker.exists(),
            "gh's Git ran a repository program: {outcome:?}"
        );
        assert!(outcome.ok, "{outcome:?}");
    }
    let seen = fs::read_to_string(tools.path().join("git-seen")).unwrap();
    assert!(seen.contains(" M README"), "{seen}");
    assert!(seen.contains("remote.git"), "{seen}");
}

// Copied settings are written as quoted config text that Git reads back
// exactly; a subsection or value can never start another section or setting.
#[test]
fn copied_settings_are_written_back_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let entries = [
        (
            "remote.we\"ird \\ [name] # x.url",
            " a \"quoted\" \\ value ; # kept\tend ",
        ),
        ("user.name", "\"\n[core]\n\thooksPath = /tmp/evil"),
        ("url.git@github.com:.insteadof", "https://github.com/"),
        ("branch.feature/x.merge", "refs/heads/feature/x"),
    ];
    let text: String = entries
        .iter()
        .map(|(key, value)| config_entry(key, value).unwrap())
        .collect();
    let file = dir.path().join("config");
    fs::write(&file, text).unwrap();
    let listed = crate::test_git::git()
        .args(["config", "--null", "--list", "--file"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let read: Vec<(String, String)> = String::from_utf8(listed.stdout)
        .unwrap()
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (key, value) = entry.split_once('\n').unwrap();
            (key.to_owned(), value.to_owned())
        })
        .collect();
    let expected: Vec<(String, String)> = entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    assert_eq!(read, expected);
    for (key, value) in [
        ("remote.line\nbreak.url", "x"),
        ("user.name", "carriage\rreturn"),
        ("user name.x", "x"),
    ] {
        assert_eq!(config_entry(key, value), None, "{key:?} = {value:?}");
    }
}

// Without symbolic links, shallow and packed-refs are private copies kept
// current by holding Git's own lock on each: no other Git process can rewrite
// them during the action, a rewrite made in the view reaches the repository
// through that lock, and the locks are gone afterwards.
#[tokio::test]
async fn without_symbolic_links_the_view_holds_gits_shallow_lock_and_writes_back() {
    let dir = tempfile::tempdir().unwrap();
    let (_, remote) = cloned(dir.path());
    let seed = dir.path().join("seed");
    for round in ["two", "three"] {
        fs::write(seed.join("README"), format!("{round}\n")).unwrap();
        sh(&seed, &["commit", "-q", "-am", round]);
    }
    sh(&seed, &["push", "-q", remote.to_str().unwrap(), "main"]);
    sh(&remote, &["branch", "stale", "main"]);
    let work = dir.path().join("shallow-work");
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--no-single-branch",
            &format!("file://{}", remote.display()),
            work.to_str().unwrap(),
        ],
    );
    let work = crate::canonical_path::canonicalize(work).unwrap();
    identity(&work);
    sh(&work, &["pack-refs", "--all"]);
    let lock = work.join(".git/shallow.lock");
    let shallow = || fs::read_to_string(work.join(".git/shallow")).unwrap();
    let packed = || fs::read_to_string(work.join(".git/packed-refs")).unwrap();
    let boundary = shallow();
    assert!(packed().contains("refs/remotes/origin/stale"));

    let view = WriteView::prepare_reaching(&work, true, &[Reach::Copies])
        .await
        .unwrap();
    let private = &view._private;
    let copy = private.common.path().join("shallow");
    assert!(!fs::symlink_metadata(&copy)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&copy).unwrap(), boundary);
    assert!(lock.exists(), "Git's shallow lock is not held");
    let snapshot = private.common.path().join("packed-refs");
    assert!(!fs::symlink_metadata(&snapshot)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&snapshot).unwrap(), packed());
    let packed_lock = work.join(".git/packed-refs.lock");
    assert!(packed_lock.exists(), "Git's packed-refs lock is not held");
    let outside = |args: &[&str]| {
        crate::test_git::git()
            .arg("-C")
            .arg(&work)
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(
        !outside(&["fetch", "-q", "--depth", "2", "origin", "main"]),
        "shallow changed under the view"
    );
    // A second view (another action) waits for the lock, then says why.
    let busy = WriteView::prepare_reaching(&work, true, &[Reach::Copies])
        .await
        .err()
        .unwrap();
    assert_eq!(busy.0, "busy");
    assert!(busy.1.contains("packed-refs.lock"), "{}", busy.1);

    fs::write(work.join("README"), "in the view\n").unwrap();
    for args in [
        &["update-ref", "-d", "refs/remotes/origin/stale"][..],
        &["commit", "-q", "-am", "in the view"],
        &["fetch", "-q", "--depth", "2", "origin", "main"],
    ] {
        let ran = git(&view, args, NETWORK_TIMEOUT).await.unwrap();
        assert!(ran.success, "git {args:?}: {}", ran.stderr);
    }
    assert!(
        !outside(&["pack-refs", "--all"]),
        "packed-refs changed under the view"
    );
    assert!(packed().contains("refs/remotes/origin/stale"));
    assert_eq!(
        shallow(),
        boundary,
        "the view wrote the repository's shallow"
    );
    drop(view);
    assert!(!lock.exists(), "the lock outlived the view");
    assert!(!packed_lock.exists(), "the lock outlived the view");
    assert!(
        !packed().contains("refs/remotes/origin/stale"),
        "{}",
        packed()
    );
    assert_ne!(shallow(), boundary, "the deepened boundary was lost");
    assert_eq!(sh(&work, &["log", "-1", "--format=%s"]), "in the view");
    assert_eq!(sh(&work, &["rev-list", "--count", "origin/main"]), "2");
    sh(&work, &["fsck", "--connectivity-only", "--no-progress"]);

    // An unchanged copy leaves the file alone; a lock another Git process
    // holds turns an action away with the file to remove if it stays.
    let before = shallow();
    drop(
        WriteView::prepare_reaching(&work, true, &[Reach::Copies])
            .await
            .unwrap(),
    );
    assert_eq!(shallow(), before);
    fs::write(&lock, "").unwrap();
    force(&[Reach::Copies]);
    let mut commit = request("commit");
    commit.message = Some("Blocked".into());
    commit.include_unstaged = Some(true);
    fs::write(work.join("README"), "blocked\n").unwrap();
    let outcome = perform(&work, &slugs(), &commit, &Gh::Missing).await;
    assert_eq!(refused(&outcome).0, "busy");
    assert!(lock.exists(), "Cupola removed another process's lock");
    fs::remove_file(&lock).unwrap();
    let outcome = perform(&work, &slugs(), &commit, &Gh::Missing).await;
    assert!(outcome.ok, "{outcome:?}");
    assert!(!lock.exists());

    // A shallow file swapped for a link (here to a device that never ends)
    // is refused, never read or traded for the checked path.
    let boundaries = work.join(".git/shallow");
    fs::remove_file(&boundaries).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &boundaries).unwrap();
    force(&[Reach::Copies]);
    let outcome = perform(&work, &slugs(), &commit, &Gh::Missing).await;
    assert_eq!(refused(&outcome).0, "unavailable", "{outcome:?}");
    assert!(!lock.exists());
}

// Git never runs in the repository's own Git directory, so where no private
// one can be made at all, nothing is written.
#[tokio::test]
async fn without_any_private_view_actions_refuse() {
    force(&[]);
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let before = untouched(&work);
    fs::write(work.join("README"), "checked\n").unwrap();
    let mut commit = request("commit");
    commit.message = Some("Checked".into());
    commit.include_unstaged = Some(true);
    let outcome = perform(&work, &slugs(), &commit, &Gh::Missing).await;
    assert_eq!(refused(&outcome).0, "unavailable", "{outcome:?}");
    assert_eq!(untouched(&work).0, before.0);
}

// The junction Windows makes without a privilege is set from this buffer:
// REPARSE_DATA_BUFFER's mount-point layout, an NT substitute name and a drive
// path print name. Anything a junction can't point at is refused.
#[test]
fn junction_buffers_follow_the_mount_point_layout() {
    let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let field = |buffer: &[u8], at: usize| u16::from_le_bytes([buffer[at], buffer[at + 1]]);
    let name = |buffer: &[u8], offset: u16, length: u16| {
        let start = 16 + usize::from(offset);
        let units: Vec<u16> = buffer[start..start + usize::from(length)]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    };
    for (target, print) in [
        ("C:/Users/me/repo/.git/refs", r"C:\Users\me\repo\.git\refs"),
        (r"\\?\D:\work\.git\lfs", r"D:\work\.git\lfs"),
    ] {
        let buffer = mount_point(&wide(target)).unwrap();
        assert_eq!(
            u32::from_le_bytes(buffer[..4].try_into().unwrap()),
            0xA000_0003
        );
        assert_eq!(usize::from(field(&buffer, 4)), buffer.len() - 8);
        let (substitute_at, substitute_len) = (field(&buffer, 8), field(&buffer, 10));
        let (print_at, print_len) = (field(&buffer, 12), field(&buffer, 14));
        assert_eq!(substitute_at, 0);
        assert_eq!(print_at, substitute_len + 2);
        assert_eq!(
            name(&buffer, substitute_at, substitute_len),
            format!(r"\??\{print}")
        );
        assert_eq!(name(&buffer, print_at, print_len), print);
        assert_eq!(
            buffer.len(),
            16 + usize::from(substitute_len) + 2 + usize::from(print_len) + 2
        );
    }
    for target in [
        r"\\server\share\repo\.git\refs",
        r"\\?\UNC\server\share\refs",
        r"relative\refs",
        "C:",
        "",
    ] {
        assert_eq!(mount_point(&wide(target)), None, "{target}");
    }
}

/// Everything another repository's owner could lose to a stray write: its
/// refs, HEAD, index and status.
fn untouched(work: &Path) -> (String, String, Vec<u8>, String) {
    (
        sh(work, &["for-each-ref"]),
        sh(work, &["rev-parse", "HEAD"]),
        fs::read(work.join(".git/index")).unwrap(),
        sh(work, &["status", "--porcelain=v1"]),
    )
}

/// Ways an agent writing only inside a Project folder could point Git at
/// another repository: each makes Git describe a folder whose writes land in
/// `victim`, and none may be admitted.
fn hostile_layouts(dir: &Path, victim: &Path) -> Vec<(&'static str, PathBuf)> {
    let theirs = victim.join(".git");
    let mut cases = Vec::new();
    let mut folder = |name: &'static str, repository: bool| {
        let folder = dir.join(name);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("README"), "planted\n").unwrap();
        if repository {
            sh(&folder, &["init", "-q", "-b", "main"]);
            identity(&folder);
        }
        let folder = crate::canonical_path::canonicalize(folder).unwrap();
        cases.push((name, folder.clone()));
        folder
    };
    let planted = folder("a .git file naming another repository", false);
    fs::write(
        planted.join(".git"),
        format!("gitdir: {}\n", theirs.display()),
    )
    .unwrap();
    let linked = folder("a .git link to another repository", false);
    std::os::unix::fs::symlink(&theirs, linked.join(".git")).unwrap();
    // Another repository's real linked worktree: its back-link names that
    // worktree, not this folder.
    sh(
        victim,
        &["worktree", "add", "-q", "-b", "elsewhere", "../elsewhere"],
    );
    let borrowed = folder("another repository's worktree", false);
    fs::write(
        borrowed.join(".git"),
        format!("gitdir: {}\n", theirs.join("worktrees/elsewhere").display()),
    )
    .unwrap();
    let common = folder("a commondir naming another repository", true);
    fs::write(
        common.join(".git/commondir"),
        format!("{}\n", theirs.display()),
    )
    .unwrap();
    let refs = folder("a linked refs directory", true);
    std::os::unix::fs::symlink(theirs.join("refs/heads"), refs.join(".git/refs/cupola")).unwrap();
    let objects = folder("linked objects", true);
    fs::remove_dir_all(objects.join(".git/objects")).unwrap();
    std::os::unix::fs::symlink(theirs.join("objects"), objects.join(".git/objects")).unwrap();
    let alternates = folder("borrowed objects", true);
    fs::write(
        alternates.join(".git/objects/info/alternates"),
        format!("{}\n", theirs.join("objects").display()),
    )
    .unwrap();
    cases
}

// A Project folder's Git data must be its own. Whatever an agent plants in
// the folder (a .git file, link, commondir, linked refs or objects, or
// alternates), Commit, Push and Pull never write another repository.
#[tokio::test]
async fn actions_never_write_a_repository_outside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("victim")).unwrap();
    let (victim, _) = cloned(&dir.path().join("victim"));
    let cases = hostile_layouts(dir.path(), &victim);
    let before = untouched(&victim);
    for (name, folder) in &cases {
        for refuse in [true, false] {
            let refusal = WriteView::prepare(folder, refuse).await.err();
            assert_eq!(
                refusal.as_ref().map(|r| r.0),
                Some("outsideProject"),
                "{name}: {refusal:?}"
            );
        }
        let mut commit = request("commit");
        commit.message = Some("Planted".into());
        commit.include_unstaged = Some(true);
        let mut push = request("push");
        push.confirm_default_branch = Some(true);
        for request in [commit, push, request("pull")] {
            let outcome = perform(folder, &slugs(), &request, &Gh::Missing).await;
            assert_eq!(refused(&outcome).0, "outsideProject", "{name}: {outcome:?}");
        }
    }
    // Over the real route too, where the folder is the Project.
    let planted = &cases[0].1;
    let (_fixture, state, id) = routed(planted, "planted-chat").await;
    for action in ["commit", "push", "pull"] {
        let body = serde_json::json!({
            "repositoryId": id, "root": "workspace", "action": action,
            "message": "Planted", "includeUnstaged": true, "confirmDefaultBranch": true,
        });
        let (status, body) = post(&state, "planted-chat", body, true).await;
        assert_eq!(
            (status, body["reason"].as_str()),
            (StatusCode::CONFLICT, Some("outsideProject")),
            "{action}: {body}"
        );
        crate::tests::validate_http_contract("gitActionOutcome", &body);
    }
    assert_eq!(untouched(&victim), before);
}

// Git's own layouts outside the work tree stay usable: a linked worktree that
// its repository names back, and an absorbed submodule. A protected location
// still refuses them.
#[tokio::test]
async fn linked_worktrees_and_submodules_still_take_actions() {
    let dir = tempfile::tempdir().unwrap();
    let (work, remote) = cloned(dir.path());
    sh(&work, &["worktree", "add", "-q", "-b", "side", "../linked"]);
    let linked = crate::canonical_path::canonicalize(dir.path().join("linked")).unwrap();
    let (_fixture, state, id) = routed(&linked, "linked-chat").await;
    fs::write(linked.join("README"), "from the worktree\n").unwrap();
    let body = serde_json::json!({
        "repositoryId": id, "root": "workspace", "action": "commit",
        "message": "In the worktree", "includeUnstaged": true,
    });
    let (status, body) = post(&state, "linked-chat", body, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        sh(&work, &["log", "-1", "--format=%s", "side"]),
        "In the worktree"
    );

    // A `.git` file rewritten after the check is never followed.
    let view = WriteView::prepare(&linked, true).await.unwrap();
    let pointer = fs::read(linked.join(".git")).unwrap();
    fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", dir.path().join("seed/.git").display()),
    )
    .unwrap();
    let pinned = checkpoint_git(&view, &["rev-parse", "--git-path", "index"]).await;
    let common = crate::canonical_path::canonicalize(work.join(".git")).unwrap();
    assert_eq!(
        pinned,
        Some(common.join("worktrees/linked/index").display().to_string())
    );
    drop(view);
    fs::write(linked.join(".git"), pointer).unwrap();

    assert!(workspace_git::admit_write(&linked, &linked, &[])
        .await
        .is_ok());
    assert_eq!(
        workspace_git::admit_write(&linked, &linked, std::slice::from_ref(&common)).await,
        Err(workspace_git::Uncontained::Outside)
    );
    // A folder outside the repository it names is never admitted.
    let outside = crate::canonical_path::canonicalize(dir.path().join("seed")).unwrap();
    assert_eq!(
        workspace_git::admit_write(&linked, &outside, &[]).await,
        Err(workspace_git::Uncontained::Outside)
    );

    sh(
        &work,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            remote.to_str().unwrap(),
            "sub",
        ],
    );
    let sub = work.join("sub");
    assert!(sub.join(".git").is_file());
    assert!(WriteView::prepare(&sub, true).await.is_ok());
    assert!(workspace_git::admit_write(&sub, &work, &[]).await.is_ok());
}

/// Swaps the repository's `.git` for a link to `victim`'s, as an agent could
/// at any moment after Cupola checks it.
fn swap_for_victim(work: &Path, victim: &Path, aside: &Path) {
    fs::rename(work.join(".git"), aside).unwrap();
    std::os::unix::fs::symlink(victim.join(".git"), work.join(".git")).unwrap();
}

// The operating system, not a path check, holds Git to the checked
// repository: with `.git` swapped for a link to another repository after the
// view was prepared, Commit, Pull and a checkpoint's writes are all denied.
#[tokio::test]
async fn a_git_directory_swapped_after_the_check_is_never_written() {
    for (reach, reaches) in [
        (Reach::Links, &[Reach::Links][..]),
        (Reach::Copies, &[Reach::Copies][..]),
    ] {
        force(reaches);
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("victim")).unwrap();
        let (victim, _) = cloned(&dir.path().join("victim"));
        let (work, _) = cloned(dir.path());
        fs::write(work.join("README"), "agent\n").unwrap();
        let before = untouched(&victim);
        let objects = sh(&victim, &["count-objects", "-v"]);
        let view = WriteView::prepare(&work, true).await.unwrap();
        swap_for_victim(&work, &victim, &dir.path().join("aside"));
        let mut request = request("commit");
        request.message = Some("Swapped".into());
        request.include_unstaged = Some(true);
        let outcome = match commit(&view, &request).await {
            Ok(outcome) => outcome,
            Err((reason, message)) => Outcome::refused(Kind::Commit, reason, message),
        };
        assert!(!outcome.ok, "{reach:?}: {outcome:?}");
        let _ = pull(&view).await;
        assert_eq!(
            checkpoint_git(&view, &["update-ref", "refs/cupola/planted", "HEAD"]).await,
            None,
            "{reach:?}"
        );
        // Nothing is carried back through the link either.
        drop(view);
        assert_eq!(untouched(&victim), before, "{reach:?}");
        assert_eq!(sh(&victim, &["count-objects", "-v"]), objects, "{reach:?}");
        assert!(!victim.join(".git/FETCH_HEAD").exists(), "{reach:?}");
    }
}

// Git writes FETCH_HEAD, ORIG_HEAD, COMMIT_EDITMSG and reflogs in place, so
// one hard-linked to another repository's file would carry the write there,
// beneath any folder confinement. Git writes them only in the action's own
// Git directory, and Cupola carries them back by replacing the Project's
// file: the other repository's is untouched and the Project keeps its history.
#[tokio::test]
async fn a_git_file_hard_linked_elsewhere_is_never_written() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("victim")).unwrap();
    let (victim, _) = cloned(&dir.path().join("victim"));
    let (work, remote) = cloned(dir.path());
    // The remote moves on, so Pull fast-forwards.
    let other = dir.path().join("other");
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    identity(&other);
    fs::write(other.join("b.txt"), "b\n").unwrap();
    sh(&other, &["add", "b.txt"]);
    sh(&other, &["commit", "-q", "-m", "b"]);
    sh(&other, &["push", "-q", "origin", "main"]);
    let config = victim.join(".git/config");
    let before = fs::read(&config).unwrap();
    let names = [
        "FETCH_HEAD",
        "ORIG_HEAD",
        "COMMIT_EDITMSG",
        "logs/HEAD",
        "logs/refs/heads/main",
        "logs/refs/remotes/origin/main",
    ];
    for name in names {
        let linked = work.join(".git").join(name);
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&linked);
        fs::hard_link(&config, &linked).unwrap();
    }

    let pulled = perform(&work, &slugs(), &request("pull"), &Gh::Missing).await;
    assert_eq!(
        fs::read(&config).unwrap(),
        before,
        "Pull wrote the other repository"
    );
    assert!(pulled.ok, "{pulled:?}");
    fs::write(work.join("README"), "agent\n").unwrap();
    let mut commit = request("commit");
    commit.message = Some("Linked".into());
    commit.include_unstaged = Some(true);
    let committed = perform(&work, &slugs(), &commit, &Gh::Missing).await;
    assert!(committed.ok, "{committed:?}");
    let mut push = request("push");
    push.confirm_default_branch = Some(true);
    let pushed = perform(&work, &slugs(), &push, &Gh::Missing).await;
    assert!(pushed.ok, "{pushed:?}");

    assert_eq!(fs::read(&config).unwrap(), before);
    for name in names {
        let file = work.join(".git").join(name);
        let metadata = fs::metadata(&file).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::nlink(&metadata),
            1,
            "{name}"
        );
    }
    let read = |name: &str| fs::read_to_string(work.join(".git").join(name)).unwrap();
    assert_eq!(read("COMMIT_EDITMSG").trim(), "Linked");
    assert!(
        read("FETCH_HEAD").contains("branch 'main'"),
        "{}",
        read("FETCH_HEAD")
    );
    assert!(
        read("logs/HEAD").contains("commit: Linked"),
        "{}",
        read("logs/HEAD")
    );
    assert!(read("logs/refs/heads/main").contains("Fast-forward"));
    assert!(read("logs/refs/remotes/origin/main").contains("update by push"));
    assert_eq!(sh(&remote, &["log", "-1", "--format=%s", "main"]), "Linked");
}

// A merge in progress is finished by Commit as Git would finish it: a merge
// commit with both parents, and the repository no longer in the merge.
#[tokio::test]
async fn commit_finishes_a_merge_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    sh(&work, &["switch", "-q", "-c", "side"]);
    fs::write(work.join("README"), "side\n").unwrap();
    sh(&work, &["commit", "-q", "-am", "side"]);
    sh(&work, &["switch", "-q", "main"]);
    fs::write(work.join("README"), "main\n").unwrap();
    sh(&work, &["commit", "-q", "-am", "main"]);
    let merging = crate::test_git::git()
        .arg("-C")
        .arg(&work)
        .args(["-c", "core.hooksPath=/dev/null", "merge", "-q", "side"])
        .output()
        .unwrap();
    assert!(
        !merging.status.success(),
        "the fixture merge did not conflict"
    );
    assert!(work.join(".git/MERGE_HEAD").exists());
    fs::write(work.join("README"), "both\n").unwrap();

    let mut commit = request("commit");
    commit.message = Some("Merged side".into());
    commit.include_unstaged = Some(true);
    let outcome = perform(&work, &slugs(), &commit, &Gh::Missing).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        sh(&work, &["rev-list", "--parents", "-1", "HEAD"])
            .split(' ')
            .count(),
        3
    );
    for name in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
        assert!(!work.join(".git").join(name).exists(), "{name}");
    }
    assert_eq!(sh(&work, &["status", "--porcelain=v1"]), "");
}

// Where the computer can't confine Git (no Landlock or Seatbelt), nothing is
// written, and the reason is plain.
#[tokio::test]
async fn without_confinement_git_actions_refuse() {
    FORCED_UNCONFINED.with(|forced| forced.set(true));
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let refusal = WriteView::prepare(&work, true).await.err().unwrap();
    assert_eq!(refusal.0, "unavailable");
    assert!(
        refusal.1.contains("inside this Project's folder"),
        "{}",
        refusal.1
    );
}

// Carrying an action's reflog entries back never loses an entry another Git
// process adds meanwhile: someone moving the branch on the computer (a reset
// to recover work, say) either waits for Cupola or is told the branch is
// busy, and every entry it does write is kept.
#[tokio::test]
async fn reflog_entries_written_meanwhile_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (work, _) = cloned(dir.path());
    let base = sh(&work, &["rev-parse", "HEAD"]);
    let view = WriteView::prepare(&work, true).await.unwrap();
    fs::write(work.join("README"), "carried\n").unwrap();
    let mut request = request("commit");
    request.message = Some("Carried".into());
    request.include_unstaged = Some(true);
    assert!(commit(&view, &request).await.unwrap().ok);
    let moved = std::rc::Rc::new(std::cell::Cell::new(None::<bool>));
    let seen = moved.clone();
    let repository = work.clone();
    BETWEEN_REFLOG_READ_AND_REPLACE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |relative: &Path| {
            if relative != Path::new("refs/heads/main") || seen.get().is_some() {
                return;
            }
            let status = crate::test_git::git()
                .arg("-C")
                .arg(&repository)
                .arg("update-ref")
                .args(["-m", "recovery", "refs/heads/main", &base])
                .output()
                .unwrap()
                .status;
            seen.set(Some(status.success()));
        }));
    });
    drop(view);
    BETWEEN_REFLOG_READ_AND_REPLACE.with(|hook| hook.borrow_mut().take());
    let reflog = fs::read_to_string(work.join(".git/logs/refs/heads/main")).unwrap();
    assert!(reflog.contains("Carried"), "{reflog}");
    let moved = moved.get().expect("the other Git ran");
    assert!(
        !moved || reflog.contains("recovery"),
        "the other Git's entry was lost:\n{reflog}"
    );
    for lock in [".git/HEAD.lock", ".git/refs/heads/main.lock"] {
        assert!(!work.join(lock).exists(), "{lock} was left behind");
    }
}
