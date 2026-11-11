//! Move a Project chat onto its own git worktree and carry on there: the
//! chat's folder becomes a new branch checked out beside the repository, so
//! its edits stop colliding with whatever else is happening in the main
//! checkout. The conversation, its history and its provider session stay;
//! only the folder the next message runs in changes.
//!
//! A Claude session file lives under the folder it ran in, so it is copied
//! to the worktree's folder; Codex is told the new folder when it resumes.
use crate::{filesystem, projects, AppState, OwnerAuthority};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path as FsPath, PathBuf};
use wonder_store::AgentFamily;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorktreeRequest {
    /// The new branch's name; made from the chat's title when absent.
    pub(crate) branch: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeResponse {
    /// The folder the chat now runs in.
    cwd: String,
    branch: String,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    projects::error(status, message)
}

/// A branch and folder name from free text: lowercase words joined by `-`.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed: String = out.trim_matches('-').chars().take(40).collect();
    trimmed.trim_end_matches('-').to_owned()
}

/// Undoes `add_worktree` when the move fails after it.
fn remove_worktree(repo: &FsPath, worktree: &FsPath, branch: &str) {
    remove_worktree_with(FsPath::new("git"), repo, worktree, branch)
}

fn remove_worktree_with(program: &FsPath, repo: &FsPath, worktree: &FsPath, branch: &str) {
    let _ = git(
        program,
        repo,
        &["worktree", "remove", "--force", &worktree.to_string_lossy()],
    );
    let _ = git(program, repo, &["branch", "-D", branch]);
}

/// Git in `cwd` with the Project Git actions' fixed settings and reduced
/// environment, so the repository's hooks and fsmonitor program never run and
/// the host's tokens never reach Git or anything it starts.
fn git(program: &FsPath, cwd: &FsPath, args: &[&str]) -> Result<String, String> {
    let output = filesystem::repository_git(program, cwd)
        .args(args)
        .output()
        .map_err(|_| "Git isn’t available on your computer.".to_owned())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Claude names a folder's session directory by replacing every character
/// that is not a letter or digit with `-`.
fn claude_session_dir(config: &FsPath, cwd: &FsPath) -> PathBuf {
    let name: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config.join("projects").join(name)
}

/// A worktree just made: its top (what Git removes on rollback), the folder
/// the chat runs in inside it, and the branch.
#[derive(Debug)]
struct Made {
    root: PathBuf,
    folder: PathBuf,
    branch: String,
}

/// Makes the worktree.
fn add_worktree(cwd: &FsPath, wanted: &str) -> Result<Made, (StatusCode, String)> {
    add_worktree_with(FsPath::new("git"), cwd, wanted)
}

fn add_worktree_with(
    program: &FsPath,
    cwd: &FsPath,
    wanted: &str,
) -> Result<Made, (StatusCode, String)> {
    let refused = |message: String| (StatusCode::UNPROCESSABLE_ENTITY, message);
    let top = git(program, cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| refused("This chat’s folder isn’t in a git repository.".into()))?;
    let top = PathBuf::from(top);
    let name = slug(wanted);
    if name.is_empty() {
        return Err(refused("Name the branch with letters or numbers.".into()));
    }
    let repo_name = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let parent = top
        .parent()
        .ok_or_else(|| refused("This repository has no folder beside it.".into()))?;
    let taken = |path: &PathBuf, branch: &str| {
        path.exists()
            || git(
                program,
                &top,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_ok()
    };
    let mut path = parent.join(format!("{repo_name}-{name}"));
    let mut branch = format!("cupola/{name}");
    let mut n = 2;
    while taken(&path, &branch) {
        path = parent.join(format!("{repo_name}-{name}-{n}"));
        branch = format!("cupola/{name}-{n}");
        n += 1;
    }
    // Hooks and fsmonitor are fixed off; a content filter would run on checkout.
    filesystem::refuse_repository_programs(program, &top).map_err(refused)?;
    let path_text = path.to_string_lossy().into_owned();
    git(
        program,
        &top,
        &["worktree", "add", "-b", &branch, &path_text, "HEAD"],
    )
    .map_err(|e| refused(format!("Git couldn’t make the worktree: {e}")))?;
    // The chat keeps its place inside the repository.
    let relative = cwd.strip_prefix(&top).unwrap_or(FsPath::new(""));
    let folder = if relative.as_os_str().is_empty() {
        path.clone()
    } else {
        path.join(relative)
    };
    Ok(Made {
        root: path,
        folder,
        branch,
    })
}

pub(crate) async fn handoff(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<WorktreeRequest>,
) -> Response {
    let Ok(Some(source)) = state.store.project_conversation(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match projects::is_archived(&state, &source).await {
        Ok(false) => {}
        Ok(true) => {
            return error(
                StatusCode::CONFLICT,
                "This chat is archived. Restore it first.",
            )
        }
        Err(message) => return error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
    if matches!(source.family, AgentFamily::OpenCode | AgentFamily::Cursor) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{} chats can’t move to a worktree yet.",
                source.family.provider().display_name
            ),
        );
    }
    let busy = || {
        error(
            StatusCode::CONFLICT,
            "The agent is still working in this chat. Move it when the reply finishes.",
        )
    };
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return error(
            StatusCode::CONFLICT,
            "Cupola is preparing to update. Try again shortly.",
        );
    };
    if state
        .store
        .conversation_has_archive_blocking_work(&id)
        .await
        .unwrap_or(true)
        || projects::held_on_mac(&state, &id, None).await
        || projects::claude_reply_running(&state, &id).await
    {
        return busy();
    }
    // New messages wait in the queue while the folder changes.
    let _holding = {
        let _dispatch = state
            .update_admission
            .dispatch_guard(&state.dispatch_lock)
            .await;
        if state
            .store
            .conversation_has_archive_blocking_work(&id)
            .await
            .unwrap_or(true)
            || !state.projects.begin_rewind(&id)
        {
            return busy();
        }
        Hold(&state, id.clone())
    };
    if !state.store.include_worktrees().await.unwrap_or(false) {
        return error(
            StatusCode::CONFLICT,
            "Turn on worktree chats in Settings first, so the moved chat stays in its Project.",
        );
    }
    let wanted = request.branch.unwrap_or_else(|| source.title.clone());
    let old = PathBuf::from(&source.cwd);
    let added = tokio::task::spawn_blocking({
        let old = old.clone();
        move || add_worktree(&old, &wanted)
    })
    .await;
    let Made {
        root,
        folder: new,
        branch,
    } = match added {
        Ok(Ok(made)) => made,
        Ok(Err((status, message))) => return error(status, message),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The worktree couldn’t be made. Try again.",
            )
        }
    };
    if source.family == AgentFamily::Claude {
        if let Some(session) = &source.native_session_id {
            let config = crate::claude_config_home(
                &PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
                &old,
                &|name| std::env::var_os(name),
            );
            let from = claude_session_dir(&config, &old).join(format!("{session}.jsonl"));
            let to_dir = claude_session_dir(&config, &new);
            let copied = std::fs::create_dir_all(&to_dir)
                .and_then(|()| std::fs::copy(&from, to_dir.join(format!("{session}.jsonl"))));
            if copied.is_err() && from.exists() {
                remove_worktree(&old, &root, &branch);
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Claude’s session couldn’t be carried over. Nothing changed.",
                );
            }
        }
    }
    let new_text = new.to_string_lossy().into_owned();
    if state
        .store
        .move_project_conversation_folder(&id, &new_text, &projects::now_text())
        .await
        .is_err()
    {
        let _ = state.logger.record(
            "error",
            "worktree_handoff_not_saved",
            json!({"conversationId": id}),
        );
        remove_worktree(&old, &root, &branch);
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The chat couldn’t be moved onto a new worktree. Nothing changed.",
        );
    }
    // Claude's runtime pins a session to the folder it started in until told
    // otherwise; Codex is told the folder with every turn.
    if source.family == AgentFamily::Claude {
        if let Ok(Some(binding)) = state.store.runtime_binding(&id).await {
            let told = async {
                let project = state.store.project(&source.project_id).await.ok()??;
                let project = projects::thread_project(&state, project, &new_text).await;
                let rpc = crate::claude::for_thread(&state, &binding.thread_id)
                    .await
                    .ok()?;
                projects::result(
                    &rpc,
                    "thread/resume",
                    json!({"threadId": binding.thread_id, "excludeTurns": true, "cwd": new_text,
                           "wonderProject": projects::claude_project(&project, &new_text)}),
                )
                .await
                .ok()
            }
            .await;
            if told.is_none() {
                let _ = state.logger.record(
                    "error",
                    "worktree_handoff_runtime_not_told",
                    json!({"conversationId": id}),
                );
                // The chat did move; the next send resumes Claude in the new folder.
            }
        }
    }
    Json(WorktreeResponse {
        cwd: new_text,
        branch,
    })
    .into_response()
}

/// Holds a conversation's dispatch (the rewind hold) until dropped.
struct Hold<'a>(&'a AppState, String);
impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.0.projects.end_rewind(&self.1);
        self.0.dispatch_wake.notify_one();
    }
}

// Uses projects::tests, which is unix-only.
#[cfg(all(test, unix))]
mod tests {
    //! Disposable repositories whose own settings try to run a program while a
    //! worktree is made. Every marker is a file only that program writes.
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn real_git() -> PathBuf {
        [
            "/usr/bin/git",
            "/usr/local/bin/git",
            "/opt/homebrew/bin/git",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("git is installed")
    }

    fn sh(dir: &FsPath, args: &[&str]) -> String {
        let mut command = std::process::Command::new(real_git());
        crate::test_git::hermetic(&mut command);
        let output = command
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn script(path: &FsPath, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `<dir>/repo` with one commit holding a tracked `data.txt`, and a
    /// sibling folder `<dir>/markers` that scripts write into.
    fn repository(dir: &FsPath) -> (PathBuf, PathBuf) {
        let repo = dir.join("repo");
        let markers = dir.join("markers");
        std::fs::create_dir_all(&markers).unwrap();
        sh(dir, &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
        sh(&repo, &["config", "user.name", "Cupola Test"]);
        sh(&repo, &["config", "user.email", "test@example.com"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("data.txt"), "tracked\n").unwrap();
        std::fs::write(repo.join(".gitattributes"), "data.txt filter=mark\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "first"]);
        (crate::canonical_path::canonicalize(&repo).unwrap(), markers)
    }

    fn markers_in(dir: &FsPath) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn checkout_hook_and_fsmonitor_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, markers) = repository(dir.path());
        let hook = repo.join(".git/hooks/post-checkout");
        script(&hook, &format!("echo ran > '{}/hook'", markers.display()));
        let monitor = dir.path().join("monitor.sh");
        script(
            &monitor,
            &format!("echo ran > '{}/fsmonitor'", markers.display()),
        );
        sh(
            &repo,
            &["config", "core.fsmonitor", monitor.to_str().unwrap()],
        );
        sh(
            &repo,
            &[
                "config",
                "core.hooksPath",
                repo.join(".git/hooks").to_str().unwrap(),
            ],
        );

        let made = add_worktree(&repo, "feature").expect("worktree is made");

        assert_eq!(markers_in(&markers), Vec::<String>::new());
        assert_eq!(made.branch, "cupola/feature");
        assert!(made.folder.join("data.txt").is_file());
    }

    #[test]
    fn rollback_with_a_hostile_hook_and_fsmonitor_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, markers) = repository(dir.path());
        let Made { root, branch, .. } = add_worktree(&repo, "feature").expect("worktree is made");
        let hook = repo.join(".git/hooks/post-checkout");
        script(&hook, &format!("echo ran > '{}/hook'", markers.display()));
        let monitor = dir.path().join("monitor.sh");
        script(
            &monitor,
            &format!("echo ran > '{}/fsmonitor'", markers.display()),
        );
        sh(
            &repo,
            &["config", "core.fsmonitor", monitor.to_str().unwrap()],
        );
        sh(
            &repo,
            &[
                "config",
                "core.hooksPath",
                repo.join(".git/hooks").to_str().unwrap(),
            ],
        );

        remove_worktree(&repo, &root, &branch);

        assert_eq!(markers_in(&markers), Vec::<String>::new());
        assert!(!root.exists());
        assert!(sh(&repo, &["branch", "--list", &branch]).is_empty());
    }

    #[test]
    fn content_filters_are_refused_before_anything_is_made() {
        for (setting, shape) in [("smudge", "cat"), ("clean", "cat"), ("process", "protocol")] {
            let dir = tempfile::tempdir().unwrap();
            let (repo, markers) = repository(dir.path());
            let filter = dir.path().join("filter.sh");
            script(
                &filter,
                &format!(
                    "echo ran > '{}/filter-{setting}'\n{shape}",
                    markers.display()
                ),
            );
            sh(
                &repo,
                &[
                    "config",
                    &format!("filter.mark.{setting}"),
                    filter.to_str().unwrap(),
                ],
            );

            let refusal = add_worktree(&repo, "feature").expect_err("a filter is refused");

            assert_eq!(refusal.0, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(
                refusal.1.contains(&format!("filter.mark.{setting}")),
                "{}",
                refusal.1
            );
            assert!(!refusal.1.contains("filter.sh"), "{}", refusal.1);
            assert_eq!(markers_in(&markers), Vec::<String>::new(), "{setting}");
            assert!(!dir.path().join("repo-feature").exists());
            assert!(sh(&repo, &["branch", "--list", "cupola/*"]).is_empty());
        }
    }

    /// Runs this test again in a fresh process whose environment holds the
    /// synthetic secrets, so no test sets a variable in the shared process.
    /// The child is told to do the check by `CUPOLA_CREDENTIAL_CHILD`.
    const CHILD: &str = "CUPOLA_CREDENTIAL_CHILD";
    const SECRETS: [(&str, &str); 7] = [
        (
            "WONDER_LOOPBACK_CAPABILITY",
            "synthetic-loopback-capability",
        ),
        ("TS_AUTHKEY", "synthetic-tunnel-key"),
        ("GH_TOKEN", "synthetic-gh-token"),
        ("GITHUB_TOKEN", "synthetic-github-token"),
        ("OPENAI_API_KEY", "synthetic-provider-key"),
        ("SSH_AUTH_SOCK", "/synthetic/agent.sock"),
        ("GIT_ASKPASS", "/synthetic/askpass"),
    ];

    #[test]
    fn git_children_see_no_control_plane_credentials() {
        if std::env::var_os(CHILD).is_none() {
            let name = format!(
                "{}::git_children_see_no_control_plane_credentials",
                module_path!().split_once("::").unwrap().1
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--test-threads=1"])
                .env(CHILD, "1")
                .envs(SECRETS)
                .output()
                .unwrap();
            let report = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && report.contains("1 passed"),
                "{report}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (repo, _markers) = repository(dir.path());
        let seen = dir.path().join("seen");
        std::fs::create_dir_all(&seen).unwrap();
        let wrapper = dir.path().join("git-wrapper");
        script(
            &wrapper,
            &format!(
                "env > '{}'/\"$$\"\nexec '{}' \"$@\"",
                seen.display(),
                real_git().display()
            ),
        );
        for (name, value) in SECRETS {
            assert_eq!(std::env::var(name).as_deref(), Ok(value), "{name} is set");
        }

        let made = add_worktree_with(&wrapper, &repo, "feature");
        let rolled = {
            let made = made.as_ref().expect("worktree is made");
            remove_worktree_with(&wrapper, &repo, &made.root, &made.branch);
            made.root.exists()
        };

        let calls = markers_in(&seen);
        assert!(calls.len() >= 4, "the wrapper saw {} calls", calls.len());
        for call in calls {
            let text = std::fs::read_to_string(seen.join(call)).unwrap();
            for (name, value) in SECRETS {
                // Never print `text`: it is the host's real environment.
                assert!(!text.contains(value), "{name}'s value reached a Git child");
                assert!(
                    !text.contains(&format!("{name}=")),
                    "{name} reached a Git child"
                );
            }
        }
        assert!(!rolled);
    }

    /// A partial clone whose missing file Git would fetch while it checks the
    /// worktree out, from a remote whose `uploadpack` is a marker script. A
    /// wrapper strips `GIT_NO_LAZY_FETCH` as a Git older than 2.45 ignores it.
    #[test]
    fn lazy_fetch_never_runs_a_repository_program_on_old_git() {
        let dir = tempfile::tempdir().unwrap();
        let (source, markers) = repository(dir.path());
        sh(&source, &["config", "uploadpack.allowFilter", "true"]);
        sh(
            &source,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let clone = dir.path().join("clone");
        sh(
            dir.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "clone",
                "-q",
                "--no-checkout",
                "--filter=blob:none",
                &format!("file://{}", source.display()),
                clone.to_str().unwrap(),
            ],
        );
        let upload = dir.path().join("upload.sh");
        script(
            &upload,
            &format!(
                "echo ran > '{}/uploadpack'\nexec '{}' upload-pack \"$@\"",
                markers.display(),
                real_git().display()
            ),
        );
        sh(
            &clone,
            &[
                "config",
                "remote.origin.uploadpack",
                upload.to_str().unwrap(),
            ],
        );
        sh(&clone, &["config", "protocol.file.allow", "always"]);
        sh(&clone, &["config", "protocol.allow", "always"]);
        let old_git = dir.path().join("old-git");
        script(
            &old_git,
            &format!(
                "unset GIT_NO_LAZY_FETCH\nexec '{}' \"$@\"",
                real_git().display()
            ),
        );

        // Whether the blob could be fetched is not the point; nothing ran.
        let _ = add_worktree_with(&old_git, &clone, "feature");

        assert_eq!(markers_in(&markers), Vec::<String>::new());
    }

    /// The chat `project-chat` lives in `repo/sub`, a subfolder of a repository
    /// that already has another worktree with unsaved work.
    async fn subfolder_chat() -> (tempfile::TempDir, AppState, PathBuf, PathBuf) {
        let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
        let repo = crate::canonical_path::canonicalize(dir.path().join("project-source")).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.name", "Cupola Test"]);
        sh(&repo, &["config", "user.email", "test@example.com"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/data.txt"), "tracked\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "first"]);
        sh(&repo, &["branch", "cupola/other"]);
        let other = dir.path().join("project-source-other");
        sh(
            &repo,
            &["worktree", "add", other.to_str().unwrap(), "cupola/other"],
        );
        std::fs::write(other.join("unsaved.txt"), "keep\n").unwrap();
        state
            .store
            .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
            .await
            .unwrap();
        state
            .store
            .move_project_conversation_folder(
                "project-chat",
                repo.join("sub").to_str().unwrap(),
                "now",
            )
            .await
            .unwrap();
        state.store.set_include_worktrees(true).await.unwrap();
        (dir, state, repo, other)
    }

    async fn move_chat(state: &AppState, branch: &str) -> (StatusCode, serde_json::Value) {
        let response = handoff(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path("project-chat".to_owned()),
            Json(serde_json::from_value(json!({ "branch": branch })).unwrap()),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
        )
    }

    #[tokio::test]
    async fn handoff_keeps_the_conversation_and_session_and_makes_the_worktree() {
        let (dir, state, repo, other) = subfolder_chat().await;
        let before = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();

        let (status, reply) = move_chat(&state, "My Feature!").await;

        assert_eq!(status, StatusCode::OK, "{reply}");
        let root = crate::canonical_path::canonicalize(dir.path())
            .unwrap()
            .join("project-source-my-feature");
        assert_eq!(reply["branch"], "cupola/my-feature");
        assert_eq!(reply["cwd"], root.join("sub").to_str().unwrap());
        assert_eq!(
            sh(&root, &["branch", "--show-current"]),
            "cupola/my-feature"
        );
        let after = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.cwd, root.join("sub").to_str().unwrap());
        assert_eq!(after.native_session_id, before.native_session_id);
        assert_eq!(after.native_session_id.as_deref(), Some("thread"));
        assert_eq!(after.project_id, before.project_id);
        assert_eq!(after.title, before.title);
        let binding = state
            .store
            .runtime_binding("project-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.thread_id, "thread");
        assert_eq!(
            std::fs::read_to_string(other.join("unsaved.txt")).unwrap(),
            "keep\n"
        );
        assert!(repo.join("sub/data.txt").is_file());
    }

    #[tokio::test]
    async fn a_failed_handoff_from_a_subfolder_removes_only_its_own_worktree() {
        let (dir, state, repo, other) = subfolder_chat().await;
        let before = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        // The move is the last step that can fail: make the store refuse it.
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER injected_failure BEFORE UPDATE OF cwd ON project_conversations
             BEGIN SELECT RAISE(ABORT, 'injected'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let (status, reply) = move_chat(&state, "doomed").await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply}");
        assert!(reply.to_string().contains("Nothing changed"), "{reply}");
        let made = crate::canonical_path::canonicalize(dir.path())
            .unwrap()
            .join("project-source-doomed");
        assert!(!made.exists(), "the new worktree was left behind");
        assert!(sh(&repo, &["branch", "--list", "cupola/doomed"]).is_empty());
        assert!(!sh(&repo, &["worktree", "list"]).contains("doomed"));
        assert_eq!(
            std::fs::read_to_string(other.join("unsaved.txt")).unwrap(),
            "keep\n"
        );
        assert!(!sh(&repo, &["branch", "--list", "cupola/other"]).is_empty());
        let after = state
            .store
            .project_conversation("project-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.cwd, before.cwd);
        assert_eq!(after.native_session_id.as_deref(), Some("thread"));
    }

    #[test]
    fn handoff_makes_the_branch_and_rollback_leaves_other_work() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, _markers) = repository(dir.path());
        // Git reports real paths (macOS's temporary folder is a link).
        let base = crate::canonical_path::canonicalize(dir.path()).unwrap();
        sh(&repo, &["branch", "cupola/other"]);
        let other = base.join("repo-other");
        sh(
            &repo,
            &["worktree", "add", other.to_str().unwrap(), "cupola/other"],
        );
        std::fs::write(other.join("unsaved.txt"), "keep\n").unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();

        let made = add_worktree(&repo.join("sub"), "My Feature!").unwrap();

        assert_eq!(made.branch, "cupola/my-feature");
        assert_eq!(made.root, base.join("repo-my-feature"));
        assert_eq!(made.folder, base.join("repo-my-feature/sub"));
        assert_eq!(
            sh(&made.root, &["branch", "--show-current"]),
            "cupola/my-feature"
        );
        remove_worktree(&repo, &made.root, &made.branch);
        assert!(!made.root.exists());
        assert!(sh(&repo, &["branch", "--list", &made.branch]).is_empty());
        assert_eq!(
            std::fs::read_to_string(other.join("unsaved.txt")).unwrap(),
            "keep\n"
        );
        assert!(!sh(&repo, &["branch", "--list", "cupola/other"]).is_empty());
        assert!(repo.join("data.txt").is_file());
    }
}
