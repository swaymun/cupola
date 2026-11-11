//! Authenticated metadata browsing for choosing Mac file-access locations.
use crate::{AppState, OwnerAuthority};
use axum::{
    extract::{Extension, Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    io::{AsRawFd, FromRawFd},
};
use std::{
    fs, io,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};
use wonder_store::{StoredConversationFile, StoredProject};

mod git_actions;
mod protected_roots;
mod repo_records;
pub(crate) mod thread_changes;
mod thread_repos;
#[cfg(windows)]
mod windows_open;
mod workspace_git;
mod workspace_search;
pub(super) use git_actions::{pull_request_merge_state, repository_action};
pub(crate) use git_actions::{refuse_repository_programs, repository_git};
pub use protected_roots::{claude_config_home, sensitive_roots, with_firmlink_aliases};
pub(crate) use repo_records::{published, Published};
pub(super) use thread_repos::thread_repositories;
#[cfg(windows)]
use windows_open::{open_workspace_directory, open_workspace_file, read_directory_entries};
pub use workspace_git::configure_git_reads;
pub(crate) use workspace_git::{read_only_git_bytes, read_only_git_command};
pub(super) use workspace_git::{
    workspace_git_changes, workspace_git_summary, workspace_thread_changes,
};
pub(super) use workspace_search::workspace_search;

const PAGE_SIZE: usize = 200;
const MAX_ENTRIES: usize = 20_000;
const MAX_WORKSPACE_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Previews stream from disk, so large PDFs and images need no host buffer.
const MAX_WORKSPACE_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;
const PREVIEW_CHUNK_BYTES: usize = 256 * 1024;
/// Upper bound for a text preview's initial prefix read (`prefix=`). The app
/// lays out at most 128 KB of text, so this leaves room for UTF-8 boundaries.
const MAX_PREVIEW_PREFIX_BYTES: u64 = 1024 * 1024;
const MAX_MEDIA_RANGE_BYTES: u64 = 1024 * 1024;
const MAX_DIFF_BYTES: usize = 1024 * 1024;
const TOO_MANY_ENTRIES_DETAIL: &str =
    "This folder has too many entries to browse. Open a more specific subfolder.";
static BROWSE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static MEDIA_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
static WORKSPACE_DISK_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct BrowseQuery {
    path: Option<String>,
    #[serde(default)]
    show_hidden: bool,
    #[serde(default)]
    offset: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DirectoryPage {
    path: String,
    parent_path: Option<String>,
    entries: Vec<DirectoryEntry>,
    next_offset: Option<usize>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DirectoryEntry {
    name: String,
    path: String,
    is_directory: bool,
}

type BrowseError = (StatusCode, &'static str);

fn too_many_entries_error() -> BrowseError {
    (StatusCode::PAYLOAD_TOO_LARGE, TOO_MANY_ENTRIES_DETAIL)
}

pub(super) async fn browse(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(query): Query<BrowseQuery>,
) -> Response {
    let path = match query.path {
        Some(path) if path.is_empty() || !Path::new(&path).is_absolute() => {
            return (
                StatusCode::BAD_REQUEST,
                "Choose an absolute folder path on your computer.",
            )
                .into_response();
        }
        Some(path) => PathBuf::from(path),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home),
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Your computer’s home folder is unavailable.",
                )
                    .into_response()
            }
        },
    };
    if query.offset > MAX_ENTRIES {
        return (
            StatusCode::BAD_REQUEST,
            "This folder page is invalid. Reload the folder.",
        )
            .into_response();
    }
    let permit = match BROWSE_SLOTS.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "Folders are still loading. Try again shortly.",
            )
                .into_response()
        }
    };
    let task = tokio::task::spawn_blocking(move || {
        // Keep the slot until disk work ends, even if the request is cancelled.
        let _permit = permit;
        list_directory(
            &path,
            &state.denied_roots,
            query.show_hidden,
            query.offset,
            MAX_ENTRIES,
        )
    });
    match tokio::time::timeout(Duration::from_secs(10), task).await {
        Ok(Ok(Ok(page))) => Json(page).into_response(),
        Ok(Ok(Err(error))) => error.into_response(),
        Ok(Err(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "This folder could not be loaded. Try again.",
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "This folder is taking too long to respond. Check that its disk is connected.",
        )
            .into_response(),
    }
}

/// Runs a Files request's disk work off the shared request workers, at most
/// `WORKSPACE_DISK_SLOTS` at a time. A timeout answers the phone but cannot
/// stop blocking work already running, so the slot is held until it ends.
async fn workspace_disk<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, BrowseError> + Send + 'static,
) -> Result<T, BrowseError> {
    const SLOW_DISK: BrowseError = (
        StatusCode::GATEWAY_TIMEOUT,
        "This location is taking too long to respond. Check that its disk is connected.",
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        let permit = WORKSPACE_DISK_SLOTS
            .acquire()
            .await
            .map_err(|_| SLOW_DISK)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "This file could not be read. Try again.",
            )
        })?
    })
    .await
    .unwrap_or(Err(SLOW_DISK))
}

fn disk_error(error: io::Error) -> BrowseError {
    match error.kind() {
        io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            "This folder is no longer available on your computer.",
        ),
        io::ErrorKind::PermissionDenied => (
            StatusCode::FORBIDDEN,
            "Your computer does not allow Cupola to browse this folder.",
        ),
        io::ErrorKind::InvalidInput | io::ErrorKind::NotADirectory => (
            StatusCode::BAD_REQUEST,
            "Choose an existing folder on your computer.",
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "This folder could not be read. Check that its disk is available.",
        ),
    }
}

/// Resolve existing ancestors too: a protected folder may not exist yet, and
/// /var versus /private/var must not change its protection when it appears.
pub(super) fn protected_root(path: &Path) -> Result<PathBuf, BrowseError> {
    if !path.is_absolute() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Protected computer locations could not be checked.",
        ));
    }
    let mut existing = path;
    let mut tail = Vec::new();
    loop {
        match crate::canonical_path::canonicalize(existing) {
            Ok(mut canonical) => {
                for part in tail.into_iter().rev() {
                    canonical.push(part);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name() else {
                    return Err(disk_error(error));
                };
                tail.push(name.to_os_string());
                existing = existing.parent().ok_or((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Protected computer locations could not be checked.",
                ))?;
            }
            Err(_) => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Protected computer locations could not be checked.",
                ))
            }
        }
    }
}

fn list_directory(
    path: &Path,
    denied_roots: &[String],
    show_hidden: bool,
    offset: usize,
    max_entries: usize,
) -> Result<DirectoryPage, BrowseError> {
    if path.to_str().is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "This folder name cannot be displayed. Choose another folder.",
        ));
    }
    let denied = denied_roots
        .iter()
        .map(|root| protected_root(Path::new(root)))
        .collect::<Result<Vec<_>, _>>()?;
    let canonical = crate::canonical_path::canonicalize(path).map_err(disk_error)?;
    let protected = |path: &Path| denied.iter().any(|root| path.starts_with(root));
    if protected(&canonical) {
        return Err((
            StatusCode::FORBIDDEN,
            "This location is protected by Cupola.",
        ));
    }
    if !fs::metadata(&canonical).map_err(disk_error)?.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "Choose a folder to browse."));
    }
    let canonical_string = canonical
        .to_str()
        .ok_or((
            StatusCode::BAD_REQUEST,
            "This folder name cannot be displayed. Choose another folder.",
        ))?
        .to_owned();
    let mut entries = Vec::new();
    for (index, entry) in fs::read_dir(&canonical).map_err(disk_error)?.enumerate() {
        if index >= max_entries {
            return Err(too_many_entries_error());
        }
        let entry = entry.map_err(disk_error)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let target = match crate::canonical_path::canonicalize(entry.path()) {
            Ok(target) => target,
            Err(_) => continue,
        };
        if protected(&target) {
            continue;
        }
        let Some(target_string) = target.to_str() else {
            continue;
        };
        let metadata = match fs::metadata(&target) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        entries.push(DirectoryEntry {
            name,
            path: target_string.to_owned(),
            is_directory: metadata.is_dir(),
        });
    }
    entries.sort_by_cached_key(|entry| {
        (
            !entry.is_directory,
            entry.name.to_lowercase(),
            entry.name.clone(),
        )
    });
    let next_offset =
        (entries.len() > offset.saturating_add(PAGE_SIZE)).then(|| offset + PAGE_SIZE);
    let entries = entries.into_iter().skip(offset).take(PAGE_SIZE).collect();
    Ok(DirectoryPage {
        path: canonical_string,
        parent_path: canonical
            .parent()
            .filter(|parent| !protected(parent))
            .and_then(Path::to_str)
            .map(str::to_owned),
        entries,
        next_offset,
    })
}

// -------------------------------------------------------------------------
// Conversation-scoped workspace browser
// -------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct WorkspaceRoot {
    id: String,
    label: String,
    path: PathBuf,
    is_directory: bool,
    kind: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRootResponse {
    id: String,
    label: String,
    path: String,
    is_directory: bool,
    kind: &'static str,
    read_only: bool,
    byte_size: Option<u64>,
    mime_type: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRootsResponse {
    available: bool,
    detail: Option<String>,
    roots: Vec<WorkspaceRootResponse>,
    attachments: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceEntry {
    name: String,
    path: String,
    is_directory: bool,
    byte_size: Option<u64>,
    mime_type: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDirectoryPage {
    root_id: String,
    path: String,
    parent_path: Option<String>,
    entries: Vec<WorkspaceEntry>,
    next_offset: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceGitChange {
    path: String,
    original_path: Option<String>,
    state: String,
    index_status: String,
    worktree_status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceGitStatusResponse {
    available: bool,
    detail: Option<String>,
    repository_path: Option<String>,
    changes: Vec<WorkspaceGitChange>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDiffResponse {
    path: String,
    staged: bool,
    diff: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotation_version: Option<u8>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WorkspaceQuery {
    root: Option<String>,
    path: Option<String>,
    #[serde(default)]
    show_hidden: bool,
    #[serde(default)]
    offset: usize,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WorkspaceFileQuery {
    root: Option<String>,
    path: Option<String>,
    /// Read only the first this-many bytes, answered as `206 Partial Content`.
    prefix: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WorkspaceDiffQuery {
    root: Option<String>,
    /// A repository inside the root (see `WorkspaceGitRootQuery`); scoped
    /// diffs only.
    repo: Option<String>,
    path: Option<String>,
    #[serde(default)]
    staged: bool,
    /// `uncommitted` (against HEAD) or `branch` (against the merge base),
    /// working tree included. Without it, `staged` picks one side as before.
    scope: Option<String>,
    /// A renamed file's previous path, from the change list.
    original_path: Option<String>,
}

fn workspace_error(status: StatusCode, message: impl Into<String>) -> Response {
    folder_refusal(status, message.into())
}

/// Refusal for a thread whose folder is a linked checkout the Project leaves
/// out. The status stays 403; the header lets a client offer to add it.
pub(crate) const WORKTREE_NOT_IN_PROJECT: &str =
    "This worktree is not one of the Project's folders. Add it to the Project on your computer to open it here.";

/// Every Files and Terminal refusal goes through here so the worktree case
/// carries its error code wherever the folder check runs.
pub(crate) fn folder_refusal(status: StatusCode, detail: String) -> Response {
    let code = (status == StatusCode::FORBIDDEN && detail == WORKTREE_NOT_IN_PROJECT)
        .then_some(crate::projects::WORKTREE_NOT_IN_PROJECT);
    let mut response = (status, detail).into_response();
    if let Some(code) = code {
        response.headers_mut().insert(
            crate::projects::ERROR_CODE_HEADER,
            axum::http::HeaderValue::from_static(code),
        );
    }
    response
}

/// 403 for a cwd outside the Project's effective folders, naming the
/// worktree case separately so it is not mistaken for revoked access.
async fn folder_missing(
    state: &crate::AppState,
    project: &StoredProject,
    cwd: &str,
    fallback: &str,
) -> (StatusCode, String) {
    let detail = if crate::projects::is_unlisted_worktree(state, project, cwd).await {
        WORKTREE_NOT_IN_PROJECT
    } else {
        fallback
    };
    (StatusCode::FORBIDDEN, detail.to_owned())
}

fn canonical_existing(path: &Path) -> Result<(PathBuf, fs::Metadata), BrowseError> {
    let canonical = crate::canonical_path::canonicalize(path).map_err(disk_error)?;
    let metadata = fs::symlink_metadata(&canonical).map_err(disk_error)?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err((
            StatusCode::FORBIDDEN,
            "Special files cannot be opened in Cupola.",
        ));
    }
    Ok((canonical, metadata))
}

fn denied_paths(denied_roots: &[String]) -> Result<Vec<PathBuf>, BrowseError> {
    denied_roots
        .iter()
        .map(|root| protected_root(Path::new(root.strip_suffix("/**").unwrap_or(root))))
        .collect()
}

fn protected_by_denies(path: &Path, denied: &[PathBuf], own_workspace: Option<&Path>) -> bool {
    denied.iter().any(|root| {
        let authorized_exception = own_workspace.is_some_and(|workspace| {
            // A broad daemon/data-root deny may contain the explicitly
            // authorized workspace. A deny nested inside that workspace must
            // still win; otherwise a protected child would be re-authorized.
            path.starts_with(workspace) && workspace.starts_with(root)
        });
        path.starts_with(root) && !authorized_exception
    })
}

fn add_root(
    roots: &mut Vec<WorkspaceRoot>,
    id: impl Into<String>,
    label: impl Into<String>,
    path: &Path,
    kind: &'static str,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
) -> Result<(), String> {
    let (canonical, metadata) = canonical_existing(path).map_err(|(_, message)| message)?;
    if protected_by_denies(&canonical, denied, own_workspace) {
        return Err("This location is protected by Cupola.".into());
    }
    if roots.iter().any(|root| root.path == canonical) {
        return Ok(());
    }
    roots.push(WorkspaceRoot {
        id: id.into(),
        label: label.into(),
        path: canonical,
        is_directory: metadata.is_dir(),
        kind,
    });
    Ok(())
}

fn applied_grant_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn child_cwd(thread: &serde_json::Value) -> Option<PathBuf> {
    thread
        .get("cwd")
        .or_else(|| thread.get("workingDirectory"))
        .or_else(|| thread.get("working_directory"))
        .and_then(serde_json::Value::as_str)
        .filter(|path| Path::new(path).is_absolute())
        .map(PathBuf::from)
}

fn attachment_summary(file: StoredConversationFile) -> serde_json::Value {
    serde_json::json!({
        "id": file.id,
        "kind": file.kind,
        "name": file.name,
        "mimeType": file.mime_type,
        "byteSize": file.byte_size,
        "sha256": file.sha256,
        "relativePath": file.relative_path,
        "state": file.state,
        "updatedAt": file.updated_at,
        "createdAt": file.created_at,
        "additions": file.additions,
        "deletions": file.deletions,
    })
}

async fn project_workspace_roots(
    state: &crate::AppState,
    project: StoredProject,
    cwd_root_id: &str,
    attachment_conversation_id: Option<&str>,
    denied: &[PathBuf],
) -> Result<(Vec<WorkspaceRoot>, Vec<StoredConversationFile>), (StatusCode, String)> {
    if !project.is_included {
        return Err((
            StatusCode::FORBIDDEN,
            "This project is no longer included in Cupola.".into(),
        ));
    }
    if !project.roots.iter().any(|root| root.id == cwd_root_id) {
        return Err((
            StatusCode::FORBIDDEN,
            "This working folder is no longer in the project.".to_owned(),
        ));
    }
    let checked = project.clone();
    let denied_roots = state.denied_roots.clone();
    tokio::task::spawn_blocking(move || {
        crate::projects::validate_execution_roots(&checked, &denied_roots)
    })
    .await
    .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
    .map_err(|message| (StatusCode::FORBIDDEN, message.to_owned()))?;
    let mut roots = Vec::new();
    for root in project
        .roots
        .iter()
        .filter(|root| root.id == cwd_root_id)
        .chain(project.roots.iter().filter(|root| root.id != cwd_root_id))
    {
        let working = root.id == cwd_root_id;
        let canonical = Path::new(&root.canonical_path);
        let (current, metadata) = canonical_existing(canonical)
            .map_err(|(_, message)| (StatusCode::FORBIDDEN, message.to_owned()))?;
        if current != canonical
            || !metadata.is_dir()
            || protected_by_denies(canonical, denied, None)
        {
            return Err((
                StatusCode::FORBIDDEN,
                "A project folder changed on your computer. Review its folders before browsing."
                    .into(),
            ));
        }
        // Keep the persisted canonical identity. A fresh add_root
        // canonicalization could otherwise switch to a different folder
        // if the selected path were replaced after validation.
        roots.push(WorkspaceRoot {
            id: if working {
                "workspace".to_owned()
            } else {
                format!("project-{}", root.id)
            },
            label: if working {
                "Workspace".to_owned()
            } else {
                applied_grant_label(canonical)
            },
            path: canonical.to_owned(),
            is_directory: true,
            kind: if working {
                "workingDirectory"
            } else {
                "projectRoot"
            },
        });
    }
    let attachments = if let Some(conversation_id) = attachment_conversation_id {
        state
            .store
            .list_conversation_files(conversation_id)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
    } else {
        Vec::new()
    };
    Ok((roots, attachments))
}

pub(crate) async fn verify_composer_scope(
    state: &crate::AppState,
    conversation_id: &str,
) -> Result<(), (StatusCode, String)> {
    conversation_workspace_roots(state, conversation_id, false)
        .await
        .map(|_| ())
}

async fn conversation_workspace_roots(
    state: &crate::AppState,
    conversation_id: &str,
    include_attachments: bool,
) -> Result<(Vec<WorkspaceRoot>, Vec<StoredConversationFile>), (StatusCode, String)> {
    let denied = denied_paths(&state.denied_roots)
        .map_err(|(_, message)| (StatusCode::SERVICE_UNAVAILABLE, message.to_owned()))?;
    if let Some(conversation) = state
        .store
        .project_conversation(conversation_id)
        .await
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
    {
        let project = state
            .store
            .project(&conversation.project_id)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
            .ok_or((
                StatusCode::NOT_FOUND,
                "This project was removed.".to_owned(),
            ))?;
        let project = crate::projects::thread_project(state, project, &conversation.cwd).await;
        let Some(root) = project.root_for(&conversation.cwd) else {
            return Err(folder_missing(
                state,
                &project,
                &conversation.cwd,
                "This conversation's working folder is no longer in the project.",
            )
            .await);
        };
        let root_id = root.id.clone();
        return project_workspace_roots(
            state,
            project,
            &root_id,
            include_attachments.then_some(conversation_id),
            &denied,
        )
        .await;
    }
    // A new Project draft has a selected folder but no conversation yet. This
    // read-only scope uses the same current Project checks and exposes no files
    // or runtime bindings from another conversation.
    if let Some(scope) = conversation_id.strip_prefix("project-files:") {
        let (project_id, root_id) = scope.split_once(':').ok_or((
            StatusCode::NOT_FOUND,
            "This project folder is unavailable.".to_owned(),
        ))?;
        let project = state
            .store
            .project(project_id)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
            .ok_or((
                StatusCode::NOT_FOUND,
                "This project was removed.".to_owned(),
            ))?;
        return project_workspace_roots(state, project, root_id, None, &denied).await;
    }
    let owner = state
        .store
        .owning_project_conversation(conversation_id)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "This Project conversation is unavailable.".to_owned(),
        ))?;
    let project = state
        .store
        .project(&owner.project_id)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "This Project is unavailable.".to_owned(),
        ))?;
    let project = crate::projects::thread_project(state, project, &owner.cwd).await;
    crate::projects::validate_execution_roots(&project, &state.denied_roots)
        .map_err(|message| (StatusCode::FORBIDDEN, message.to_owned()))?;
    let child = crate::subagents::runtime_for_conversation(state, conversation_id)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "This child workspace is unavailable.".to_owned(),
        ))?;
    let cwd = child_cwd(&child.thread).ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "This child did not provide a verified working directory.".to_owned(),
    ))?;
    let (canonical, metadata) = canonical_existing(&cwd)
        .map_err(|(_, m)| (StatusCode::SERVICE_UNAVAILABLE, m.to_owned()))?;
    if !metadata.is_dir()
        || protected_by_denies(&canonical, &denied, None)
        || !project
            .roots
            .iter()
            .any(|root| canonical.starts_with(&root.canonical_path))
    {
        return Err((
            StatusCode::FORBIDDEN,
            "This child working directory is outside the Project's verified access.".to_owned(),
        ));
    }
    let mut roots = Vec::new();
    add_root(
        &mut roots,
        "child-cwd",
        "Workspace",
        &canonical,
        "childWorkingDirectory",
        &denied,
        None,
    )
    .map_err(|e| (StatusCode::FORBIDDEN, e))?;
    let attachments = if include_attachments {
        state
            .store
            .list_conversation_files(conversation_id)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?
    } else {
        Vec::new()
    };
    Ok((roots, attachments))
}

/// The verified working folder of a Project conversation (or its child
/// task), with every check the workspace browser applies. Terminals start here.
pub(crate) async fn conversation_working_directory(
    state: &crate::AppState,
    conversation_id: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    if conversation_id.starts_with("project-files:") {
        return Err((
            StatusCode::NOT_FOUND,
            "Start the conversation before opening a terminal.".to_owned(),
        ));
    }
    let (roots, _) = conversation_workspace_roots(state, conversation_id, false).await?;
    roots
        .into_iter()
        .find(|root| {
            root.is_directory && matches!(root.kind, "workingDirectory" | "childWorkingDirectory")
        })
        .map(|root| root.path)
        .ok_or((
            StatusCode::FORBIDDEN,
            "This conversation has no verified working folder.".to_owned(),
        ))
}

fn root_for<'a>(roots: &'a [WorkspaceRoot], id: &str) -> Result<&'a WorkspaceRoot, BrowseError> {
    roots.iter().find(|root| root.id == id).ok_or((
        StatusCode::NOT_FOUND,
        "This workspace location is no longer available.",
    ))
}

fn relative_path(value: Option<&str>) -> Result<PathBuf, BrowseError> {
    let value = value.unwrap_or_default();
    if value.contains('\0') || Path::new(value).is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose a file inside this workspace.",
        ));
    }
    let mut relative = PathBuf::new();
    for component in Path::new(value).components() {
        match component {
            std::path::Component::Normal(component) => relative.push(component),
            std::path::Component::CurDir => {}
            _ => return Err((StatusCode::BAD_REQUEST, "This workspace path is invalid.")),
        }
    }
    Ok(relative)
}

fn resolve_workspace_path(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
    allow_missing_leaf: bool,
) -> Result<PathBuf, BrowseError> {
    let lexical = root.path.join(relative);
    let canonical = match crate::canonical_path::canonicalize(&lexical) {
        Ok(path) => path,
        // A deleted file may have lost its folders too (a whole folder was
        // removed). Resolve the nearest existing ancestor, never above the
        // root, and append the missing names: they cannot be links.
        Err(error) if allow_missing_leaf && error.kind() == io::ErrorKind::NotFound => {
            let mut existing = lexical.as_path();
            let mut missing = Vec::new();
            loop {
                missing.push(
                    existing
                        .file_name()
                        .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?,
                );
                existing = existing
                    .parent()
                    .filter(|parent| parent.starts_with(&root.path))
                    .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
                match crate::canonical_path::canonicalize(existing) {
                    Ok(mut canonical) => {
                        canonical.extend(missing.iter().rev());
                        break canonical;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(disk_error(error)),
                }
            }
        }
        Err(error) => return Err(disk_error(error)),
    };
    let canonical_root = crate::canonical_path::canonicalize(&root.path).map_err(disk_error)?;
    if canonical_root != root.path {
        return Err((
            StatusCode::FORBIDDEN,
            "This workspace location changed on your computer.",
        ));
    }
    if !canonical.starts_with(&canonical_root)
        || protected_by_denies(&canonical, denied, own_workspace)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    let mut cursor = canonical_root;
    for component in relative.components() {
        cursor.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&cursor) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing_leaf && error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(disk_error(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err((
                StatusCode::FORBIDDEN,
                "Symlinks cannot be opened in Cupola.",
            ));
        }
        if !metadata.is_file() && !metadata.is_dir() {
            return Err((
                StatusCode::FORBIDDEN,
                "Special files cannot be opened in Cupola.",
            ));
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
fn secure_open_error(error: io::Error) -> BrowseError {
    match error.raw_os_error() {
        Some(code) if code == libc::ELOOP => (
            StatusCode::FORBIDDEN,
            "Symlinks cannot be opened in Cupola.",
        ),
        Some(code) if code == libc::ENOTDIR => {
            (StatusCode::BAD_REQUEST, "Choose a folder or regular file.")
        }
        _ => disk_error(error),
    }
}

#[cfg(unix)]
fn openat_directory(parent: &fs::File, name: &std::ffi::OsStr) -> Result<fs::File, BrowseError> {
    let name = CString::new(name.as_bytes()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "This workspace path contains an invalid name.",
        )
    })?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(secure_open_error(io::Error::last_os_error()));
    }
    let metadata = unsafe { metadata.assume_init() };
    let kind = metadata.st_mode as libc::mode_t & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        return Err((
            StatusCode::FORBIDDEN,
            "Symlinks cannot be opened in Cupola.",
        ));
    }
    if kind != libc::S_IFDIR {
        return Err((StatusCode::BAD_REQUEST, "Choose a folder to browse."));
    }
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(secure_open_error(io::Error::last_os_error()));
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> Result<fs::File, BrowseError> {
    if !path.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            "This workspace path must be absolute.",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut directory = options.open(Path::new("/")).map_err(secure_open_error)?;
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            directory = openat_directory(&directory, name)?;
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_relative_directory(root: &Path, relative: &Path) -> Result<fs::File, BrowseError> {
    let mut directory = open_directory_nofollow(root)?;
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => {
                directory = openat_directory(&directory, name)?;
            }
            _ => return Err((StatusCode::BAD_REQUEST, "This workspace path is invalid.")),
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_file_at(directory: &fs::File, name: &std::ffi::OsStr) -> Result<fs::File, BrowseError> {
    let name = CString::new(name.as_bytes()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "This workspace path contains an invalid name.",
        )
    })?;
    // A FIFO named like a media file must not hold a blocking worker or its
    // admission slot while open(2) waits for an unrelated writer.
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(secure_open_error(io::Error::last_os_error()));
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_workspace_file(root: &WorkspaceRoot, relative: &Path) -> Result<fs::File, BrowseError> {
    if !root.is_directory && !relative.as_os_str().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Choose a regular file."));
    }
    if relative.as_os_str().is_empty() {
        let parent = root
            .path
            .parent()
            .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
        let name = root
            .path
            .file_name()
            .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
        let directory = open_directory_nofollow(parent)?;
        return open_file_at(&directory, name);
    }
    let directory = if root.is_directory {
        open_relative_directory(&root.path, relative.parent().unwrap_or(Path::new("")))?
    } else {
        return Err((StatusCode::BAD_REQUEST, "Choose a regular file."));
    };
    let name = relative
        .file_name()
        .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
    open_file_at(&directory, name)
}

#[cfg(unix)]
fn open_workspace_directory(
    root: &WorkspaceRoot,
    relative: &Path,
) -> Result<fs::File, BrowseError> {
    if !root.is_directory {
        return Err((StatusCode::BAD_REQUEST, "Choose a folder to browse."));
    }
    open_relative_directory(&root.path, relative)
}

#[cfg(unix)]
fn read_directory_entries(
    directory: &fs::File,
    max_entries: usize,
) -> Result<Vec<(String, bool, Option<u64>)>, BrowseError> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(disk_error(io::Error::last_os_error()));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(disk_error(io::Error::last_os_error()));
    }
    let directory_fd = unsafe { libc::dirfd(stream) };
    let mut entries = Vec::new();
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(not(target_os = "macos"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::closedir(stream) };
            if error.raw_os_error().unwrap_or(0) != 0 {
                return Err(disk_error(error));
            }
            return Ok(entries);
        }
        let name_bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let Ok(name) = String::from_utf8(name_bytes.to_vec()) else {
            continue;
        };
        let name_c = CString::new(name.as_bytes()).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "This workspace path contains an invalid name.",
            )
        })?;
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory_fd,
                name_c.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            continue;
        }
        let metadata = unsafe { metadata.assume_init() };
        let kind = metadata.st_mode as libc::mode_t & libc::S_IFMT;
        let is_directory = kind == libc::S_IFDIR;
        if kind != libc::S_IFREG && !is_directory {
            continue;
        }
        // Count entries before the UI's hidden/protected filtering. A hidden
        // entry must not let a large directory evade the descriptor scan cap.
        if entries.len() >= max_entries {
            unsafe { libc::closedir(stream) };
            return Err(too_many_entries_error());
        }
        entries.push((
            name,
            is_directory,
            (!is_directory).then_some(metadata.st_size.max(0) as u64),
        ));
    }
}

pub(crate) fn mime_for_path(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(
        match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "pdf" => "application/pdf",
            "epub" => "application/epub+zip",
            "usdz" => "model/vnd.usdz+zip",
            "obj" => "model/obj",
            "stl" => "model/stl",
            "mp4" => "video/mp4",
            "mov" => "video/quicktime",
            "m4a" => "audio/mp4",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "html" | "htm" => "text/html",
            "md" => "text/markdown",
            "txt" | "log" | "json" | "toml" | "yaml" | "yml" | "swift" | "rs" | "ts" => {
                "text/plain"
            }
            _ => "application/octet-stream",
        }
        .to_owned(),
    )
}

fn list_workspace_directory(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
    show_hidden: bool,
    offset: usize,
) -> Result<WorkspaceDirectoryPage, BrowseError> {
    if !root.is_directory {
        return Err((StatusCode::BAD_REQUEST, "Choose a folder to browse."));
    }
    let lexical_directory = root.path.join(relative);
    if protected_by_denies(&lexical_directory, denied, own_workspace) {
        return Err((
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    let directory = open_workspace_directory(root, relative)?;
    let raw_entries = read_directory_entries(&directory, MAX_ENTRIES)?;
    let mut entries = Vec::new();
    for (name, is_directory, byte_size) in raw_entries {
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let child_relative = relative.join(&name);
        let child = root.path.join(&child_relative);
        if protected_by_denies(&child, denied, own_workspace) {
            continue;
        }
        entries.push(WorkspaceEntry {
            name,
            path: child_relative.to_string_lossy().into_owned(),
            is_directory,
            byte_size,
            mime_type: (!is_directory).then(|| mime_for_path(&child)).flatten(),
        });
    }
    entries.sort_by_cached_key(|entry| {
        (
            !entry.is_directory,
            entry.name.to_lowercase(),
            entry.name.clone(),
        )
    });
    let next_offset =
        (entries.len() > offset.saturating_add(PAGE_SIZE)).then(|| offset + PAGE_SIZE);
    let parent_path = relative.parent().and_then(|parent| {
        if parent.as_os_str().is_empty() {
            None
        } else {
            Some(parent.to_string_lossy().into_owned())
        }
    });
    Ok(WorkspaceDirectoryPage {
        root_id: root.id.clone(),
        path: relative.to_string_lossy().into_owned(),
        parent_path,
        entries: entries.into_iter().skip(offset).take(PAGE_SIZE).collect(),
        next_offset,
    })
}

fn read_bounded_file(mut file: fs::File) -> Result<Vec<u8>, BrowseError> {
    // Check the descriptor, not a path lookup performed before open. This
    // keeps a concurrent replacement from changing a validated regular file
    // into a special file or an oversized read between the checks.
    let metadata = file.metadata().map_err(disk_error)?;
    if !metadata.is_file() {
        return Err((StatusCode::BAD_REQUEST, "Choose a regular file to preview."));
    }
    if metadata.len() > MAX_WORKSPACE_FILE_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "This file is too large to preview in Cupola.",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_WORKSPACE_FILE_BYTES) as usize);
    io::Read::take(&mut file, MAX_WORKSPACE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(disk_error)?;
    if bytes.len() as u64 > MAX_WORKSPACE_FILE_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "This file is too large to preview in Cupola.",
        ));
    }
    Ok(bytes)
}

/// Opens a regular file for a streamed preview. The length comes from the
/// verified descriptor and bounds the response even if the file grows.
fn open_workspace_preview_file(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
) -> Result<(fs::File, fs::Metadata), BrowseError> {
    let lexical = root.path.join(relative);
    if protected_by_denies(&lexical, denied, own_workspace) {
        return Err((
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    let file = open_workspace_file(root, relative)?;
    let metadata = file.metadata().map_err(disk_error)?;
    if !metadata.is_file() {
        return Err((StatusCode::BAD_REQUEST, "Choose a regular file to preview."));
    }
    if metadata.len() > MAX_WORKSPACE_PREVIEW_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Files larger than 256 MB can't be previewed in Cupola. Open it on your computer.",
        ));
    }
    Ok((file, metadata))
}

/// An untracked file for its diff preview, refused by its size before any
/// byte is read when the diff could not be shown anyway.
fn read_untracked_for_diff(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
) -> Result<Vec<u8>, BrowseError> {
    const TOO_LARGE: BrowseError = (
        StatusCode::PAYLOAD_TOO_LARGE,
        "This diff is too large to preview in Cupola.",
    );
    let (file, metadata) =
        open_workspace_preview_file(root, relative, denied, Some(root.path.as_path()))?;
    let length = metadata.len();
    if length > MAX_DIFF_BYTES as u64 {
        return Err(TOO_LARGE);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    io::Read::take(file, MAX_DIFF_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(disk_error)?;
    if bytes.len() > MAX_DIFF_BYTES {
        return Err(TOO_LARGE);
    }
    Ok(bytes)
}

fn open_workspace_bounded_file(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
) -> Result<Vec<u8>, BrowseError> {
    let lexical = root.path.join(relative);
    if protected_by_denies(&lexical, denied, own_workspace) {
        return Err((
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    let file = open_workspace_file(root, relative)?;
    read_bounded_file(file)
}

/// Reopen the exact Project file the owner previewed. Annotation validation
/// uses the same current root, deny and no-follow boundary as Files, then
/// hashes the bytes from the verified descriptor rather than a path supplied
/// by the client.
pub(crate) async fn verified_project_preview_file(
    state: &crate::AppState,
    project_id: &str,
    conversation_id: &str,
    root_id: &str,
    path: &str,
) -> Result<(Vec<u8>, &'static str), (StatusCode, String)> {
    let conversation = state
        .store
        .project_conversation(conversation_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project could not be checked.".into(),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            "This Project chat is unavailable.".into(),
        ))?;
    if conversation.project_id != project_id {
        return Err((
            StatusCode::FORBIDDEN,
            "This file belongs to another Project.".into(),
        ));
    }
    let project = state
        .store
        .project(project_id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project could not be checked.".into(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "This Project was removed.".into()))?;
    let project = crate::projects::thread_project(state, project, &conversation.cwd).await;
    let Some(root) = project.root_for(&conversation.cwd) else {
        return Err(folder_missing(
            state,
            &project,
            &conversation.cwd,
            "This conversation's working folder is no longer in the Project.",
        )
        .await);
    };
    let cwd_root_id = root.id.clone();
    let denied = denied_paths(&state.denied_roots)
        .map_err(|(status, detail)| (status, detail.to_owned()))?;
    // Preview validation needs only the verified Project roots. Loading every
    // historical attachment here made each annotation send grow with chat age.
    let (roots, _) = project_workspace_roots(state, project, &cwd_root_id, None, &denied).await?;
    let root = root_for(&roots, root_id)
        .map_err(|(status, detail)| (status, detail.to_owned()))?
        .clone();
    let relative =
        relative_path(Some(path)).map_err(|(status, detail)| (status, detail.to_owned()))?;
    if relative.as_os_str().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Choose a file to annotate.".into()));
    }
    tokio::task::spawn_blocking(move || {
        let bytes =
            open_workspace_bounded_file(&root, &relative, &denied, Some(root.path.as_path()))
                .map_err(|(status, detail)| {
                    if status == StatusCode::PAYLOAD_TOO_LARGE {
                        (status, "Notes can be sent for files up to 8 MB. Describe this part of the file in your message instead.".to_owned())
                    } else {
                        (status, detail.to_owned())
                    }
                })?;
        let name = relative.file_name().and_then(|name| name.to_str()).ok_or((
            StatusCode::BAD_REQUEST,
            "This file name cannot be annotated.".to_owned(),
        ))?;
        let mime = crate::artifact_mime_type(name, &bytes).ok_or((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "This file format cannot be annotated.".to_owned(),
        ))?;
        Ok((bytes, mime))
    })
    .await
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The preview could not be checked.".into(),
        )
    })?
}

fn git_state(index: u8, worktree: u8) -> &'static str {
    if index == b'?' && worktree == b'?' {
        "untracked"
    } else if index == b'U' || worktree == b'U' || (index == b'A' && worktree == b'A') {
        "conflicted"
    } else if index == b'R' || worktree == b'R' || index == b'C' || worktree == b'C' {
        "renamed"
    } else if index == b'D' || worktree == b'D' {
        "deleted"
    } else if index != b' ' {
        "staged"
    } else if worktree != b' ' {
        "unstaged"
    } else {
        "modified"
    }
}

fn parse_git_status(bytes: &[u8]) -> Vec<(String, Option<String>, String, String, String)> {
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut output = Vec::new();
    while let Some(record) = fields.next() {
        if record.len() < 4 {
            continue;
        }
        let index = record[0];
        let worktree = record[1];
        if record[2] != b' ' {
            continue;
        }
        let path = String::from_utf8_lossy(&record[3..]).into_owned();
        let original = if index == b'R' || index == b'C' || worktree == b'R' || worktree == b'C' {
            fields
                .next()
                .map(|value| String::from_utf8_lossy(value).into_owned())
        } else {
            None
        };
        output.push((
            path,
            original,
            git_state(index, worktree).to_owned(),
            (index as char).to_string(),
            (worktree as char).to_string(),
        ));
    }
    output
}

async fn git_repository(root: &Path) -> Option<PathBuf> {
    let bytes =
        crate::filesystem::read_only_git_bytes(root.to_str()?, &["rev-parse", "--show-toplevel"])
            .await
            .ok()?;
    let path = PathBuf::from(String::from_utf8_lossy(&bytes).trim());
    crate::canonical_path::canonicalize(path).ok()
}

fn git_pathspec(repository: &Path, root: &Path) -> Option<String> {
    let relative = root.strip_prefix(repository).ok()?.to_string_lossy();
    if relative.is_empty() {
        Some(":(top,literal)".into())
    } else {
        Some(format!(":(top,literal){relative}/"))
    }
}

fn synthetic_untracked_diff(path: &str, bytes: Vec<u8>) -> Result<String, BrowseError> {
    if bytes.contains(&0) {
        return Ok(format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\nBinary files /dev/null and b/{path} differ\n"
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "This binary file has no textual diff preview.",
        )
    })?;
    let line_count = text.lines().count();
    let mut diff = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{line_count} @@\n"
    );
    for line in text.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    Ok(diff)
}

fn validated_git_path(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
) -> Result<PathBuf, BrowseError> {
    let lexical = root.path.join(relative);
    if protected_by_denies(&lexical, denied, own_workspace) {
        return Err((
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    resolve_workspace_path(root, relative, denied, own_workspace, true)
}

/// The verified Git scope of a workspace root: the repository that contains
/// it and the literal pathspec limiting Git to that root. Status, change lists
/// and diffs all start here so they share one containment and deny boundary.
enum GitScope {
    NotRepository,
    Unavailable(&'static str),
    Ready {
        directory: PathBuf,
        repository: PathBuf,
        pathspec: String,
    },
}

async fn verified_git_scope(
    root: &WorkspaceRoot,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
) -> GitScope {
    if !root.is_directory {
        return GitScope::Unavailable("Git status is available for folders only.");
    }
    let Ok(directory) = resolve_workspace_path(root, Path::new(""), denied, own_workspace, false)
    else {
        return GitScope::Unavailable("This workspace location is unavailable.");
    };
    let Some(repository) = git_repository(&directory).await else {
        return GitScope::NotRepository;
    };
    if protected_by_denies(&repository, denied, own_workspace)
        || !repository.starts_with(&directory) && !directory.starts_with(&repository)
    {
        return GitScope::Unavailable(
            "This repository is outside the conversation's verified workspace.",
        );
    }
    let Some(pathspec) = git_pathspec(&repository, &directory) else {
        return GitScope::Unavailable("Git scope could not be verified.");
    };
    GitScope::Ready {
        directory,
        repository,
        pathspec,
    }
}

async fn read_workspace_git_status(
    root: &WorkspaceRoot,
    denied: &[PathBuf],
    own_workspace: Option<&Path>,
) -> WorkspaceGitStatusResponse {
    let (directory, repository, pathspec) =
        match verified_git_scope(root, denied, own_workspace).await {
            GitScope::Ready {
                directory,
                repository,
                pathspec,
            } => (directory, repository, pathspec),
            GitScope::NotRepository => {
                return WorkspaceGitStatusResponse {
                    available: true,
                    detail: None,
                    repository_path: None,
                    changes: Vec::new(),
                }
            }
            GitScope::Unavailable(detail) => {
                return WorkspaceGitStatusResponse {
                    available: false,
                    detail: Some(detail.into()),
                    repository_path: None,
                    changes: Vec::new(),
                }
            }
        };
    let args = [
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--",
        pathspec.as_str(),
    ];
    let bytes = match crate::filesystem::read_only_git_bytes(
        repository.to_str().unwrap_or_default(),
        &args,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            return WorkspaceGitStatusResponse {
                available: false,
                detail: Some(workspace_git_status_error_detail(&error).to_owned()),
                repository_path: Some(repository.to_string_lossy().into_owned()),
                changes: Vec::new(),
            }
        }
    };
    let mut changes = Vec::new();
    for (path, original, state, index_status, worktree_status) in parse_git_status(&bytes) {
        let Ok(path) = relative_path(Some(&path)) else {
            continue;
        };
        let full = repository.join(&path);
        let Ok(relative) = full.strip_prefix(&directory) else {
            continue;
        };
        if validated_git_path(root, relative, denied, own_workspace).is_err() {
            continue;
        }
        let display_path = relative.to_string_lossy().into_owned();
        let original_path = if let Some(original) = original {
            let Ok(original) = relative_path(Some(&original)) else {
                continue;
            };
            let original_full = repository.join(original);
            let Ok(original_relative) = original_full.strip_prefix(&directory) else {
                continue;
            };
            if validated_git_path(root, original_relative, denied, own_workspace).is_err() {
                continue;
            }
            Some(original_relative.to_string_lossy().into_owned())
        } else {
            None
        };
        changes.push(WorkspaceGitChange {
            path: display_path,
            original_path,
            state,
            index_status,
            worktree_status,
        });
    }
    WorkspaceGitStatusResponse {
        available: true,
        detail: None,
        repository_path: Some(repository.to_string_lossy().into_owned()),
        changes,
    }
}

fn workspace_git_status_error_detail(error: &str) -> &'static str {
    if error.contains("timed out") {
        "Git status took too long to finish. Try again on the computer."
    } else if error.contains("exceeded its limit") {
        "This workspace has too much Git status data to preview."
    } else {
        "Git status is temporarily unavailable for this workspace."
    }
}

pub(super) async fn workspace_roots(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
) -> Response {
    match conversation_workspace_roots(&state, &conversation_id, true).await {
        Ok((roots, attachments)) => Json(WorkspaceRootsResponse {
            available: true,
            detail: None,
            roots: roots
                .into_iter()
                .map(|root| {
                    let byte_size = if root.is_directory {
                        None
                    } else {
                        open_workspace_file(&root, Path::new(""))
                            .ok()
                            .and_then(|file| file.metadata().ok())
                            .map(|metadata| metadata.len())
                    };
                    let mime_type = (!root.is_directory)
                        .then(|| mime_for_path(&root.path))
                        .flatten();
                    WorkspaceRootResponse {
                        id: root.id,
                        label: root.label,
                        path: root.path.to_string_lossy().into_owned(),
                        is_directory: root.is_directory,
                        kind: root.kind,
                        read_only: true,
                        byte_size,
                        mime_type,
                    }
                })
                .collect(),
            attachments: attachments.into_iter().map(attachment_summary).collect(),
        })
        .into_response(),
        Err((status, detail)) => workspace_error(status, detail),
    }
}

pub(super) async fn workspace_directory(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceQuery>,
) -> Response {
    if query.offset > MAX_ENTRIES {
        return workspace_error(
            StatusCode::BAD_REQUEST,
            "This folder page is invalid. Reload the folder.",
        );
    }
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Some(root_id) = query.root.as_deref() else {
        return workspace_error(StatusCode::BAD_REQUEST, "Choose a workspace location.");
    };
    let root = match root_for(&roots, root_id) {
        Ok(root) => root,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let denied = match denied_paths(&state.denied_roots) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let relative = match relative_path(query.path.as_deref()) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let root = root.clone();
    match workspace_disk(move || {
        list_workspace_directory(
            &root,
            &relative,
            &denied,
            Some(root.path.as_path()),
            query.show_hidden,
            query.offset,
        )
    })
    .await
    {
        Ok(page) => Json(page).into_response(),
        Err((status, detail)) => workspace_error(status, detail),
    }
}

/// Files changed this recently get no entity tag: a second write within the
/// file system's timestamp granularity (1 s on HFS+, a clock tick elsewhere)
/// could leave the stat revision unchanged.
const RACY_PREVIEW_STAMP: Duration = Duration::from_secs(3);

/// Weak entity tag for a file preview. It is derived from the descriptor's
/// stat revision, so it says the file has not changed rather than naming its
/// content; clients keep validating length and hashing the bytes they get.
/// `None` while the file's last change is too recent to trust (see above),
/// and wherever no change time exists that writers cannot set back.
fn preview_etag(metadata: &fs::Metadata, now: std::time::SystemTime) -> Option<String> {
    let age = now.duration_since(preview_change_time(metadata)?).ok()?;
    (age >= RACY_PREVIEW_STAMP).then(|| format!("W/\"{}\"", media_revision(metadata)))
}

/// The later of the modification and status-change times. Every write moves
/// the status-change time to now and no program can set it back, so a copy
/// that restores an old modification time still counts as a fresh change.
#[cfg(unix)]
fn preview_change_time(metadata: &fs::Metadata) -> Option<std::time::SystemTime> {
    use std::os::unix::fs::MetadataExt;
    let changed = std::time::SystemTime::UNIX_EPOCH.checked_add(Duration::new(
        u64::try_from(metadata.ctime()).ok()?,
        u32::try_from(metadata.ctime_nsec()).ok()?,
    ))?;
    Some(metadata.modified().ok()?.max(changed))
}

/// Windows exposes only the last write time, which a copy or an editor can
/// restore after a same-length rewrite; its tag would then match forever.
/// Previews there get no tag, so every check reads and hashes the file.
#[cfg(not(unix))]
fn preview_change_time(_metadata: &fs::Metadata) -> Option<std::time::SystemTime> {
    None
}

/// `If-None-Match` uses weak comparison (RFC 9110 13.1.2), so `W/"x"` and
/// `"x"` both match the preview's weak tag.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_owned();
    let current = opaque(etag);
    if_none_match
        .split(',')
        .any(|candidate| candidate.trim() == "*" || opaque(candidate) == current)
}

/// Streams one verified workspace file for preview.
///
/// Every `200` and `206` carries a weak `ETag` unless the file changed in the
/// last few seconds or the host is Windows; such an answer is always the full
/// read. A request whose
/// `If-None-Match` names the current tag gets `304 Not Modified` without a
/// body, so an open preview can check for changes without downloading the
/// file again. `prefix=N` (1 to 1 MiB) on a longer file returns only the first
/// N bytes as `206 Partial Content` with `Content-Range: bytes 0-(N-1)/total`;
/// that body is not the file's content, so its hash must not stand in for the
/// file's (annotations and exports hash a full read). When the prefix covers
/// the file the answer is an ordinary `200` with the whole file. `Range` headers are
/// ignored here; `/workspace/media` owns byte-range seeking.
pub(super) async fn workspace_file(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceFileQuery>,
    headers: HeaderMap,
) -> Response {
    if query
        .prefix
        .is_some_and(|prefix| prefix == 0 || prefix > MAX_PREVIEW_PREFIX_BYTES)
    {
        return workspace_error(
            StatusCode::BAD_REQUEST,
            "Choose a preview length of at most 1 MB.",
        );
    }
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Some(root_id) = query.root.as_deref() else {
        return workspace_error(StatusCode::BAD_REQUEST, "Choose a workspace location.");
    };
    let root = match root_for(&roots, root_id) {
        Ok(root) => root,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let denied = match denied_paths(&state.denied_roots) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let relative = match relative_path(query.path.as_deref()) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let mime = mime_for_path(&root.path.join(&relative))
        .unwrap_or_else(|| "application/octet-stream".into());
    let root = root.clone();
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let prefix = query.prefix;
    let opened = workspace_disk(move || {
        // The length, the tag and the snapshot check all come from the
        // opener's one metadata snapshot: a second stat could describe a later
        // revision and tag a truncated body as current, making polls answer 304.
        let (file, metadata) =
            open_workspace_preview_file(&root, &relative, &denied, Some(root.path.as_path()))?;
        let etag = preview_etag(&metadata, std::time::SystemTime::now());
        if let (Some(etag), Some(asked)) = (etag.as_deref(), if_none_match.as_deref()) {
            if etag_matches(asked, etag) {
                return Ok((PreviewBody::Unchanged, 0, 0, Some(etag.to_owned())));
            }
        }
        let total = metadata.len();
        let length = prefix.map_or(total, |prefix| prefix.min(total));
        let body = if length <= MAX_WORKSPACE_FILE_BYTES {
            PreviewBody::Snapshot(snapshot_preview_file(file, &metadata, length)?)
        } else {
            PreviewBody::Stream(file)
        };
        Ok((body, length, total, etag))
    })
    .await;
    let (body, length, total, etag) = match opened {
        Ok(opened) => opened,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let etag_header = etag
        .as_deref()
        .map(|etag| HeaderValue::from_str(etag).unwrap());
    let body = match body {
        PreviewBody::Unchanged => {
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            if let Some(etag_header) = etag_header {
                response.headers_mut().insert(header::ETAG, etag_header);
            }
            return response;
        }
        PreviewBody::Snapshot(bytes) => axum::body::Body::from(bytes),
        // Larger files stream with their open-time length; a file shortened
        // meanwhile ends the response short rather than passing as complete.
        PreviewBody::Stream(file) => {
            let reader = tokio::io::AsyncReadExt::take(tokio::fs::File::from_std(file), length);
            axum::body::Body::from_stream(futures_util::stream::unfold(
                reader,
                |mut reader| async move {
                    let mut buffer = vec![0; PREVIEW_CHUNK_BYTES];
                    match tokio::io::AsyncReadExt::read(&mut reader, &mut buffer).await {
                        Ok(0) => None,
                        Ok(count) => {
                            buffer.truncate(count);
                            Some((Ok::<_, io::Error>(axum::body::Bytes::from(buffer)), reader))
                        }
                        Err(error) => Some((Err(error), reader)),
                    }
                },
            ))
        }
    };
    let mut response = preview_response(body, length, &mime);
    if let Some(etag_header) = etag_header {
        response.headers_mut().insert(header::ETAG, etag_header);
    }
    if length < total {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes 0-{}/{total}", length - 1)).unwrap(),
        );
    }
    response
}

/// A preview's body: unchanged since the phone's copy, read whole, or
/// streamed when too large to hold.
enum PreviewBody {
    Unchanged,
    Snapshot(Vec<u8>),
    Stream(fs::File),
}

const FILE_CHANGED_DURING_PREVIEW: BrowseError = (
    StatusCode::CONFLICT,
    "This file changed while it was loading. Try again.",
);

/// The first `length` bytes of a file of at most `MAX_WORKSPACE_FILE_BYTES`,
/// accepted only if the descriptor's revision still matches the opener's
/// snapshot after the read. An agent editing the file meanwhile gets a clear
/// retry instead of a cut-off response.
fn snapshot_preview_file(
    mut file: fs::File,
    opened: &fs::Metadata,
    length: u64,
) -> Result<Vec<u8>, BrowseError> {
    let mut bytes = Vec::with_capacity(length as usize);
    io::Read::take(&mut file, length)
        .read_to_end(&mut bytes)
        .map_err(disk_error)?;
    let after = file.metadata().map_err(disk_error)?;
    if bytes.len() as u64 != length || media_revision(opened) != media_revision(&after) {
        return Err(FILE_CHANGED_DURING_PREVIEW);
    }
    Ok(bytes)
}

/// Previews declare their exact length and type. The type comes from the
/// extension, so a web page is marked as a download: nothing may render it
/// as a page from this origin.
fn preview_response(body: axum::body::Body, length: u64, mime: &str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_LENGTH,
        axum::http::HeaderValue::from(length),
    );
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_str(mime)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    if mime == "text/html" {
        headers.insert(
            axum::http::header::CONTENT_DISPOSITION,
            axum::http::HeaderValue::from_static("attachment"),
        );
    }
    response
}

#[derive(Debug)]
struct MediaRange {
    bytes: Vec<u8>,
    start: u64,
    end: u64,
    total: u64,
    mime: &'static str,
    revision: String,
}

fn media_revision(metadata: &fs::Metadata) -> String {
    use sha2::{Digest, Sha256};
    let stamp = crate::platform::file_stamp(metadata);
    let fields = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        stamp.device,
        stamp.inode,
        metadata.len(),
        stamp.modified.0,
        stamp.modified.1,
        stamp.changed.0,
        stamp.changed.1
    );
    hex::encode(Sha256::digest(fields.as_bytes()))
}

fn media_mime(path: &Path, head: &[u8]) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "mp4" if head.get(4..8) == Some(b"ftyp") => Some("video/mp4"),
        "mov" if head.get(4..8) == Some(b"ftyp") => Some("video/quicktime"),
        "m4a" if head.get(4..8) == Some(b"ftyp") => Some("audio/mp4"),
        "mp3"
            if head.starts_with(b"ID3")
                || head.len() >= 2 && head[0] == 0xff && head[1] & 0xe0 == 0xe0 =>
        {
            Some("audio/mpeg")
        }
        "wav" if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") => Some("audio/wav"),
        _ => None,
    }
}

fn parse_media_range(value: &str, total: u64) -> Result<(u64, u64), StatusCode> {
    let value = value
        .strip_prefix("bytes=")
        .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?;
    if value.contains(',') || total == 0 {
        return Err(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    let (first, last) = value
        .split_once('-')
        .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?;
    let (start, end) = if first.is_empty() {
        let count = last
            .parse::<u64>()
            .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
        if count == 0 || count > MAX_MEDIA_RANGE_BYTES {
            return Err(StatusCode::RANGE_NOT_SATISFIABLE);
        }
        (total.saturating_sub(count), total - 1)
    } else {
        let start = first
            .parse::<u64>()
            .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
        if start >= total {
            return Err(StatusCode::RANGE_NOT_SATISFIABLE);
        }
        let end = if last.is_empty() {
            start
                .saturating_add(MAX_MEDIA_RANGE_BYTES - 1)
                .min(total - 1)
        } else {
            last.parse::<u64>()
                .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?
                .min(total - 1)
        };
        (start, end)
    };
    if end < start || end - start + 1 > MAX_MEDIA_RANGE_BYTES {
        return Err(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    Ok((start, end))
}

fn read_media_range(
    root: &WorkspaceRoot,
    relative: &Path,
    denied: &[PathBuf],
    range: Option<&str>,
    expected_revision: Option<&str>,
) -> Result<MediaRange, (StatusCode, &'static str, Option<u64>)> {
    let failure = |status, detail| (status, detail, None);
    let lexical = root.path.join(relative);
    if protected_by_denies(&lexical, denied, Some(root.path.as_path())) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    let mut file =
        open_workspace_file(root, relative).map_err(|(status, detail)| failure(status, detail))?;
    let metadata = file.metadata().map_err(|_| {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "The media file could not be read.",
        )
    })?;
    if !metadata.is_file() {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "Choose a regular media file.",
        ));
    }
    let total = metadata.len();
    let mut head = [0_u8; 16];
    let head_len = file.read(&mut head).map_err(|_| {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "The media file could not be read.",
        )
    })?;
    let mime = media_mime(&lexical, &head[..head_len]).ok_or_else(|| {
        failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "This media format cannot be previewed here.",
        )
    })?;
    let range = range.ok_or_else(|| {
        failure(
            StatusCode::BAD_REQUEST,
            "A byte range is required for media.",
        )
    })?;
    let (start, end) = parse_media_range(range, total).map_err(|status| {
        (
            status,
            "Choose a valid media byte range of at most 1 MiB.",
            Some(total),
        )
    })?;
    let revision = media_revision(&metadata);
    if (start, end) != (0, 0) && expected_revision.is_none() {
        return Err(failure(
            StatusCode::PRECONDITION_REQUIRED,
            "Refresh this media preview before seeking.",
        ));
    }
    if expected_revision.is_some_and(|value| value != revision) {
        return Err(failure(
            StatusCode::CONFLICT,
            "This media file changed. Reopen its preview.",
        ));
    }
    file.seek(SeekFrom::Start(start)).map_err(|_| {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "The media file could not be read.",
        )
    })?;
    let mut bytes = vec![0_u8; (end - start + 1) as usize];
    file.read_exact(&mut bytes).map_err(|_| {
        failure(
            StatusCode::CONFLICT,
            "This media file changed while loading.",
        )
    })?;
    let after = file.metadata().map_err(|_| {
        failure(
            StatusCode::CONFLICT,
            "This media file changed while loading.",
        )
    })?;
    if media_revision(&after) != revision {
        return Err(failure(
            StatusCode::CONFLICT,
            "This media file changed while loading.",
        ));
    }
    Ok(MediaRange {
        bytes,
        start,
        end,
        total,
        mime,
        revision,
    })
}

pub(super) async fn workspace_media(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceQuery>,
    headers: HeaderMap,
) -> Response {
    let permit = match MEDIA_SLOTS.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return workspace_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Media is still loading. Try again shortly.",
            )
        }
    };
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Some(root_id) = query.root.as_deref() else {
        return workspace_error(StatusCode::BAD_REQUEST, "Choose a workspace location.");
    };
    let root = match root_for(&roots, root_id) {
        Ok(root) => root.clone(),
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let denied = match denied_paths(&state.denied_roots) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let relative = match relative_path(query.path.as_deref()) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let revision = headers
        .get("x-wonder-revision")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_media_range(
                &root,
                &relative,
                &denied,
                range.as_deref(),
                revision.as_deref(),
            )
        }),
    )
    .await;
    match read {
        Ok(Ok(Ok(media))) => {
            let mut response = (StatusCode::PARTIAL_CONTENT, media.bytes).into_response();
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(media.mime));
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!(
                    "bytes {}-{}/{}",
                    media.start, media.end, media.total
                ))
                .unwrap(),
            );
            headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&(media.end - media.start + 1).to_string()).unwrap(),
            );
            headers.insert(
                "x-wonder-revision",
                HeaderValue::from_str(&media.revision).unwrap(),
            );
            response
        }
        Ok(Ok(Err((status, detail, total)))) => {
            let mut response = workspace_error(status, detail);
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                if let Some(total) = total {
                    response.headers_mut().insert(
                        header::CONTENT_RANGE,
                        HeaderValue::from_str(&format!("bytes */{total}")).unwrap(),
                    );
                }
            }
            response
        }
        Ok(Err(_)) => workspace_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The media file could not be read.",
        ),
        Err(_) => workspace_error(
            StatusCode::GATEWAY_TIMEOUT,
            "This media file is taking too long to respond.",
        ),
    }
}

pub(super) async fn workspace_git_status(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceQuery>,
) -> Response {
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Some(root_id) = query.root.as_deref() else {
        return workspace_error(StatusCode::BAD_REQUEST, "Choose a workspace location.");
    };
    let root = match root_for(&roots, root_id) {
        Ok(root) => root,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let denied = match denied_paths(&state.denied_roots) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    Json(read_workspace_git_status(root, &denied, Some(root.path.as_path())).await).into_response()
}

pub(super) async fn workspace_git_diff(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceDiffQuery>,
) -> Response {
    if query.scope.is_some() {
        return workspace_git::scoped_diff(&state, &conversation_id, query).await;
    }
    if query.repo.is_some() {
        return workspace_error(
            StatusCode::BAD_REQUEST,
            "Choose uncommitted or branch changes.",
        );
    }
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Some(root_id) = query.root.as_deref() else {
        return workspace_error(StatusCode::BAD_REQUEST, "Choose a workspace location.");
    };
    let root = match root_for(&roots, root_id) {
        Ok(root) => root,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let denied = match denied_paths(&state.denied_roots) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let relative = match relative_path(query.path.as_deref()) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let path = match validated_git_path(root, &relative, &denied, Some(root.path.as_path())) {
        Ok(path) => path,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let status = read_workspace_git_status(root, &denied, Some(root.path.as_path())).await;
    if !status.available {
        return workspace_error(
            StatusCode::NOT_FOUND,
            status
                .detail
                .unwrap_or_else(|| "This workspace location is not a Git repository.".into()),
        );
    }
    let requested_path = query.path.as_deref().unwrap_or_default();
    let Some(change) = status
        .changes
        .iter()
        .find(|change| change.path == requested_path)
    else {
        return workspace_error(
            StatusCode::CONFLICT,
            "This file is no longer a modified file in the selected workspace. Reload Modified.",
        );
    };
    let staged_change = change.index_status != " " && change.index_status != "?";
    let worktree_change = change.worktree_status != " " || change.index_status == "?";
    if (query.staged && !staged_change) || (!query.staged && !worktree_change) {
        return workspace_error(
            StatusCode::CONFLICT,
            "This file changed sides. Reload Modified before opening its diff.",
        );
    }
    let git_base = if root.is_directory {
        root.path.as_path()
    } else {
        root.path.parent().unwrap_or(root.path.as_path())
    };
    let Some(repository) = git_repository(git_base).await else {
        return workspace_error(
            StatusCode::NOT_FOUND,
            "This workspace location is not a Git repository.",
        );
    };
    let repo_relative = match path.strip_prefix(&repository) {
        Ok(value) => value.to_string_lossy().into_owned(),
        Err(_) => {
            return workspace_error(
                StatusCode::FORBIDDEN,
                "This file is outside the verified repository.",
            )
        }
    };
    let pathspec = format!(":(literal,top){repo_relative}");
    let original_pathspec = if let Some(original) = change.original_path.as_deref() {
        let original = match relative_path(Some(original)).and_then(|relative| {
            validated_git_path(root, &relative, &denied, Some(root.path.as_path()))
        }) {
            Ok(path) => path,
            Err((status, detail)) => return workspace_error(status, detail),
        };
        let Ok(original) = original.strip_prefix(&repository) else {
            return workspace_error(
                StatusCode::FORBIDDEN,
                "This file is outside the verified repository.",
            );
        };
        Some(format!(":(literal,top){}", original.to_string_lossy()))
    } else {
        None
    };
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--find-renames",
    ];
    if query.staged {
        args.push("--cached");
    }
    args.extend(["--", pathspec.as_str()]);
    if let Some(original) = original_pathspec.as_deref() {
        args.push(original);
    }
    let output = match crate::filesystem::read_only_git_bytes(
        repository.to_str().unwrap_or_default(),
        &args,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => return workspace_error(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let mut diff = String::from_utf8_lossy(&output).into_owned();
    if diff.is_empty() && !query.staged {
        let (root, name) = (root.clone(), repo_relative.clone());
        diff = match workspace_disk(move || {
            if !path.exists() {
                return Ok(format!(
                    "diff --git a/{name} b/{name}\n--- a/{name}\n+++ /dev/null\n"
                ));
            }
            let bytes = read_untracked_for_diff(&root, &relative, &denied)?;
            synthetic_untracked_diff(&name, bytes)
        })
        .await
        {
            Ok(diff) => diff,
            Err((status, detail)) => return workspace_error(status, detail),
        };
    }
    if diff.len() > MAX_DIFF_BYTES {
        return workspace_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "This diff is too large to preview in Cupola.",
        );
    }
    Json(WorkspaceDiffResponse {
        annotation_version: None,
        path: query.path.unwrap_or_default(),
        staged: query.staged,
        diff,
    })
    .into_response()
}

/// A diff note reuses the verified workspace Git owner, not file-preview bytes.
pub(crate) async fn verified_project_diff(
    state: &crate::AppState,
    conversation_id: &str,
    root: &str,
    path: &str,
    scope: &str,
    original_path: Option<&str>,
) -> Result<Vec<u8>, (StatusCode, String)> {
    workspace_git::verified_diff(state, conversation_id, root, path, scope, original_path).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::{ffi::OsStringExt, fs::symlink, net::UnixListener};
    use wonder_store::{AgentFamily, ProjectConversationInsert, ProjectRootInput};

    #[test]
    fn publication_and_model_files_advertise_registered_media_types() {
        for (name, mime) in [
            ("book.epub", "application/epub+zip"),
            ("object.usdz", "model/vnd.usdz+zip"),
            ("object.obj", "model/obj"),
            ("object.stl", "model/stl"),
            ("object.ply", "application/octet-stream"),
        ] {
            assert_eq!(mime_for_path(Path::new(name)).as_deref(), Some(mime));
        }
    }

    #[tokio::test]
    async fn filesystem_browse_http_requires_owner_and_respects_protection() {
        use axum::{
            body::{to_bytes, Body},
            http::Request,
        };
        use tower::ServiceExt;
        let (folder, mut state) = crate::ingestion::tests::fixture().await;
        let requested = folder.path().join("browse");
        fs::create_dir(&requested).unwrap();
        fs::write(
            requested.join("visible.txt"),
            b"contents are never returned",
        )
        .unwrap();
        let uri = format!("/api/v1/filesystem?path={}", requested.display());
        let unauthorized = crate::router(state.clone())
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(!unauthorized.status().is_success());
        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let page: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(page["entries"][0]["name"], "visible.txt");
        assert_eq!(page["entries"][0]["isDirectory"], false);
        assert!(!String::from_utf8_lossy(&bytes).contains("contents are never returned"));
        state
            .denied_roots
            .push(requested.to_string_lossy().into_owned());
        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn filesystem_browse_sorts_directories_first_and_paginates() {
        let folder = tempfile::tempdir().unwrap();
        fs::create_dir(folder.path().join("z-folder")).unwrap();
        for number in 0..205 {
            fs::write(folder.path().join(format!("file-{number:03}")), b"").unwrap();
        }
        let first = list_directory(folder.path(), &[], false, 0, MAX_ENTRIES).unwrap();
        assert_eq!(first.entries.len(), 200);
        assert_eq!(first.entries[0].name, "z-folder");
        assert!(first.entries[0].is_directory);
        assert_eq!(first.next_offset, Some(200));
        let second = list_directory(folder.path(), &[], false, 200, MAX_ENTRIES).unwrap();
        assert_eq!(second.entries.len(), 6);
        assert_eq!(second.entries[0].name, "file-199");
        assert_eq!(second.next_offset, None);
        assert_eq!(
            first.path,
            crate::canonical_path::canonicalize(folder.path())
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert!(list_directory(folder.path(), &[], false, 0, 10)
            .unwrap_err()
            .1
            .contains("too many"));
    }

    #[test]
    fn filesystem_browse_hides_protected_targets_hidden_and_unsupported_entries() {
        let folder = tempfile::tempdir().unwrap();
        let protected = folder.path().join("private");
        fs::create_dir(&protected).unwrap();
        fs::write(protected.join("secret"), b"private").unwrap();
        fs::create_dir(folder.path().join("visible")).unwrap();
        fs::write(folder.path().join(".hidden"), b"").unwrap();
        fs::write(folder.path().join("allowed"), b"").unwrap();
        symlink(&protected, folder.path().join("private-alias")).unwrap();
        symlink(protected.join("secret"), folder.path().join("secret-alias")).unwrap();
        symlink(
            folder.path().join("visible"),
            folder.path().join("visible-alias"),
        )
        .unwrap();
        symlink(
            folder.path().join("missing"),
            folder.path().join("broken-alias"),
        )
        .unwrap();
        symlink(
            folder.path().join("loop-alias"),
            folder.path().join("loop-alias"),
        )
        .unwrap();
        let _socket = UnixListener::bind(folder.path().join("socket")).unwrap();
        #[cfg(not(target_os = "macos"))]
        fs::write(
            folder.path().join(std::ffi::OsString::from_vec(vec![0xff])),
            b"",
        )
        .unwrap();
        let denied = vec![protected.to_string_lossy().into_owned()];
        let page = list_directory(folder.path(), &denied, false, 0, MAX_ENTRIES).unwrap();
        assert_eq!(
            page.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["visible", "visible-alias", "allowed"]
        );
        assert_eq!(page.entries[0].path, page.entries[1].path);
        assert_eq!(
            list_directory(&protected, &denied, false, 0, MAX_ENTRIES)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            list_directory(
                &folder.path().join("private-alias"),
                &denied,
                false,
                0,
                MAX_ENTRIES
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let shown = list_directory(folder.path(), &denied, true, 0, MAX_ENTRIES).unwrap();
        assert!(shown.entries.iter().any(|entry| entry.name == ".hidden"));
        assert!(!shown
            .entries
            .iter()
            .any(|entry| entry.name.starts_with("private") || entry.name == "secret-alias"));
        let missing = folder.path().join("visible/future");
        assert_eq!(
            protected_root(&missing).unwrap(),
            crate::canonical_path::canonicalize(folder.path())
                .unwrap()
                .join("visible/future")
        );
    }

    #[test]
    fn filesystem_browse_rejects_files_and_invalid_display_paths() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("file");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            list_directory(&file, &[], false, 0, MAX_ENTRIES)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            list_directory(&folder.path().join("missing"), &[], false, 0, MAX_ENTRIES)
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let invalid = folder.path().join(std::ffi::OsString::from_vec(vec![0xff]));
        assert_eq!(
            list_directory(&invalid, &[], false, 0, MAX_ENTRIES)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn workspace_scope_enforces_lexical_canonical_and_protected_boundaries() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root_dir.path().join("inside")).unwrap();
        fs::write(root_dir.path().join("inside/file.txt"), b"ok").unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            root_dir.path().join("escape.txt"),
        )
        .unwrap();
        symlink(
            root_dir.path().join("inside"),
            root_dir.path().join("alias"),
        )
        .unwrap();
        let _socket = UnixListener::bind(root_dir.path().join("socket")).unwrap();
        let root_path = crate::canonical_path::canonicalize(root_dir.path()).unwrap();
        let root = WorkspaceRoot {
            id: "workspace".into(),
            label: "Workspace".into(),
            path: root_path.clone(),
            is_directory: true,
            kind: "workingDirectory",
        };
        assert_eq!(
            relative_path(Some("../secret.txt")).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert!(resolve_workspace_path(
            &root,
            Path::new("inside/file.txt"),
            &[],
            Some(&root_path),
            false
        )
        .is_ok());
        assert_eq!(
            resolve_workspace_path(&root, Path::new("escape.txt"), &[], Some(&root_path), false)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            resolve_workspace_path(
                &root,
                Path::new("alias/file.txt"),
                &[],
                Some(&root_path),
                false
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            resolve_workspace_path(&root, Path::new("socket"), &[], Some(&root_path), false)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        // A path deleted with its folders resolves through the nearest
        // existing ancestor, which keeps the link and containment checks.
        assert_eq!(
            resolve_workspace_path(
                &root,
                Path::new("gone/deep/a.txt"),
                &[],
                Some(&root_path),
                true
            )
            .unwrap(),
            root_path.join("gone/deep/a.txt")
        );
        symlink(outside.path(), root_dir.path().join("outward")).unwrap();
        for linked in ["outward/gone/a.txt", "alias/gone/a.txt"] {
            assert_eq!(
                resolve_workspace_path(&root, Path::new(linked), &[], Some(&root_path), true)
                    .unwrap_err()
                    .0,
                StatusCode::FORBIDDEN,
                "{linked}"
            );
        }
        assert_eq!(
            resolve_workspace_path(
                &root,
                Path::new("inside/gone/a.txt"),
                &[root_path.join("inside")],
                Some(&root_path),
                true
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );

        let protected_parent = root_path.parent().unwrap().to_path_buf();
        let denied = vec![protected_parent.clone()];
        assert!(resolve_workspace_path(
            &root,
            Path::new("inside/file.txt"),
            &denied,
            Some(&root_path),
            false
        )
        .is_ok());
        let nested_denied = vec![root_path.join("inside")];
        assert_eq!(
            resolve_workspace_path(
                &root,
                Path::new("inside/file.txt"),
                &nested_denied,
                Some(&root_path),
                false,
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let sibling_dir = tempfile::Builder::new()
            .prefix("sibling-")
            .tempdir_in(&protected_parent)
            .unwrap();
        let sibling = sibling_dir.path().to_path_buf();
        fs::write(sibling.join("private.txt"), b"private").unwrap();
        let sibling_root = WorkspaceRoot {
            id: "sibling".into(),
            label: "Sibling".into(),
            path: crate::canonical_path::canonicalize(&sibling).unwrap(),
            is_directory: true,
            kind: "appliedGrant",
        };
        assert_eq!(
            resolve_workspace_path(
                &sibling_root,
                Path::new("private.txt"),
                &denied,
                Some(&root_path),
                false
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn preview_snapshot_refuses_a_file_changed_after_open() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("notes.txt");
        fs::write(&path, b"complete contents").unwrap();
        let file = fs::File::open(&path).unwrap();
        let opened = file.metadata().unwrap();
        assert_eq!(
            snapshot_preview_file(file, &opened, opened.len()).unwrap(),
            b"complete contents"
        );
        // An agent shortens the file between open and read.
        let file = fs::File::open(&path).unwrap();
        let opened = file.metadata().unwrap();
        fs::write(&path, b"short").unwrap();
        assert_eq!(
            snapshot_preview_file(file, &opened, opened.len()).unwrap_err(),
            FILE_CHANGED_DURING_PREVIEW
        );
    }

    #[test]
    fn workspace_directory_is_bounded_and_supports_hidden_files() {
        let folder = tempfile::tempdir().unwrap();
        for number in 0..205 {
            fs::write(folder.path().join(format!("file-{number:03}")), b"").unwrap();
        }
        fs::write(folder.path().join(".hidden"), b"").unwrap();
        let root = WorkspaceRoot {
            id: "workspace".into(),
            label: "Workspace".into(),
            path: crate::canonical_path::canonicalize(folder.path()).unwrap(),
            is_directory: true,
            kind: "workingDirectory",
        };
        let first = list_workspace_directory(&root, Path::new(""), &[], Some(&root.path), false, 0)
            .unwrap();
        assert_eq!(first.entries.len(), 200);
        assert_eq!(first.next_offset, Some(200));
        assert!(!first.entries.iter().any(|entry| entry.name == ".hidden"));
        let second =
            list_workspace_directory(&root, Path::new(""), &[], Some(&root.path), false, 200)
                .unwrap();
        assert_eq!(second.entries.len(), 5);
        assert!(
            list_workspace_directory(&root, Path::new(""), &[], Some(&root.path), true, 0)
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry.name == ".hidden")
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_descriptor_scan_caps_before_collecting_hidden_entries() {
        let folder = tempfile::tempdir().unwrap();
        fs::write(folder.path().join("visible-a"), b"").unwrap();
        fs::write(folder.path().join("visible-b"), b"").unwrap();
        fs::write(folder.path().join(".hidden"), b"").unwrap();
        let directory =
            open_directory_nofollow(&crate::canonical_path::canonicalize(folder.path()).unwrap())
                .unwrap();
        let error = read_directory_entries(&directory, 2).unwrap_err();
        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.1, TOO_MANY_ENTRIES_DETAIL);
    }

    #[test]
    fn workspace_git_status_parses_every_user_visible_state() {
        let bytes = b" M unstaged.txt\0M  staged.txt\0?? untracked.txt\0R  renamed-new.txt\0renamed-old.txt\0UU conflict.txt\0 D deleted.txt\0";
        let changes = parse_git_status(bytes);
        let states = changes
            .iter()
            .map(|(_, _, state, _, _)| state.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            [
                "unstaged",
                "staged",
                "untracked",
                "renamed",
                "conflicted",
                "deleted"
            ]
        );
        assert_eq!(changes[3].1.as_deref(), Some("renamed-old.txt"));
        assert_eq!(
            git_pathspec(Path::new("/repo"), Path::new("/repo/src")),
            Some(":(top,literal)src/".into())
        );
        assert_eq!(
            git_pathspec(Path::new("/repo"), Path::new("/repo")),
            Some(":(top,literal)".into())
        );
    }

    #[test]
    fn workspace_git_status_errors_are_product_facing() {
        for raw in [
            "fatal: not a git repository: '/private/workspace'",
            "Git operation timed out; inspect this workspace on the computer.",
            "Git output exceeded its limit. Inspect this change on the computer.",
        ] {
            let detail = workspace_git_status_error_detail(raw);
            assert!(!detail.contains("fatal"));
            assert!(!detail.contains("/private/workspace"));
            assert!(!detail.contains("git repository"));
        }
    }

    #[test]
    fn workspace_file_grants_can_target_regular_files() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("notes.txt");
        fs::write(&file, b"read-only").unwrap();
        let root = WorkspaceRoot {
            id: "grant-0".into(),
            label: "notes.txt".into(),
            path: crate::canonical_path::canonicalize(&file).unwrap(),
            is_directory: false,
            kind: "appliedGrant",
        };
        assert_eq!(
            open_workspace_bounded_file(&root, Path::new(""), &[], Some(&root.path)).unwrap(),
            b"read-only"
        );
        assert!(
            open_workspace_bounded_file(&root, Path::new("child.txt"), &[], Some(&root.path))
                .is_err()
        );
        assert!(
            list_workspace_directory(&root, Path::new(""), &[], Some(&root.path), false, 0)
                .is_err()
        );
    }

    #[test]
    fn workspace_descriptor_access_rejects_intermediate_symlink_swaps() {
        let root_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root_dir.path().join("nested")).unwrap();
        fs::write(root_dir.path().join("nested/file.txt"), b"inside").unwrap();
        fs::write(outside.path().join("file.txt"), b"outside").unwrap();
        let root_path = crate::canonical_path::canonicalize(root_dir.path()).unwrap();
        let root = WorkspaceRoot {
            id: "workspace".into(),
            label: "Workspace".into(),
            path: root_path.clone(),
            is_directory: true,
            kind: "workingDirectory",
        };
        let validated = resolve_workspace_path(
            &root,
            Path::new("nested/file.txt"),
            &[],
            Some(&root_path),
            false,
        )
        .unwrap();
        assert_eq!(fs::read(&validated).unwrap(), b"inside");

        fs::rename(
            root_dir.path().join("nested"),
            root_dir.path().join("nested-real"),
        )
        .unwrap();
        symlink(outside.path(), root_dir.path().join("nested")).unwrap();

        assert_eq!(
            open_workspace_bounded_file(
                &root,
                Path::new("nested/file.txt"),
                &[],
                Some(&root_path),
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            list_workspace_directory(&root, Path::new("nested"), &[], Some(&root_path), false, 0,)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
    }

    pub(super) async fn workspace_route(
        state: &crate::AppState,
        uri: &str,
        authenticated: bool,
    ) -> (StatusCode, Vec<u8>) {
        use axum::{body::to_bytes, body::Body, http::Request};
        use tower::ServiceExt;
        let mut request = Request::builder().uri(uri);
        if authenticated {
            request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
        }
        let response = crate::router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    async fn media_route(
        state: &crate::AppState,
        uri: &str,
        authenticated: bool,
        range: &str,
        revision: Option<&str>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        use axum::{body::to_bytes, body::Body, http::Request};
        use tower::ServiceExt;
        let mut request = Request::builder().uri(uri).header(header::RANGE, range);
        if authenticated {
            request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
        }
        if let Some(revision) = revision {
            request = request.header("x-wonder-revision", revision);
        }
        let response = crate::router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    async fn file_route(
        state: &crate::AppState,
        uri: &str,
        authenticated: bool,
        if_none_match: Option<&str>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        use axum::{body::to_bytes, body::Body, http::Request};
        use tower::ServiceExt;
        let mut request = Request::builder().uri(uri);
        if authenticated {
            request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
        }
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        let response = crate::router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    #[tokio::test]
    async fn workspace_file_revalidates_with_etags_and_reads_bounded_prefixes() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("notes.txt");
        fs::write(&file, b"first revision").unwrap();
        fs::write(workspace.path().join("long.txt"), b"later revision").unwrap();
        // Tags appear only once the change time (which no write can set
        // back) has settled.
        tokio::time::sleep(RACY_PREVIEW_STAMP.mul_f32(1.1)).await;
        crate::test_support::project_thread(
            &state.store,
            "etag-chat",
            "Workspace",
            workspace.path().to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        let uri = "/api/v1/conversations/etag-chat/workspace/file?root=workspace&path=notes.txt";

        let (status, headers, bytes) = file_route(&state, uri, true, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"first revision");
        let etag = headers[header::ETAG].to_str().unwrap().to_owned();
        assert!(etag.starts_with("W/\"") && etag.ends_with('"'), "{etag}");

        // An unchanged file answers 304 without a body, for the weak tag,
        // its strong spelling, a list and `*`.
        let strong = etag.trim_start_matches("W/").to_owned();
        for tag in [
            etag.clone(),
            strong,
            format!("\"other\", {etag}"),
            "*".to_owned(),
        ] {
            let (status, headers, bytes) = file_route(&state, uri, true, Some(&tag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{tag}");
            assert!(bytes.is_empty());
            assert_eq!(headers[header::ETAG], etag.as_str());
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        }
        let (status, _, bytes) = file_route(&state, uri, true, Some("W/\"other\"")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"first revision");
        // Revalidation never skips the owner and workspace checks.
        let (status, _, _) = file_route(&state, uri, false, Some(&etag)).await;
        assert!(!status.is_success() && status != StatusCode::NOT_MODIFIED);
        let (status, _, _) = file_route(
            &state,
            "/api/v1/conversations/etag-chat/workspace/file?root=workspace&path=missing.txt",
            true,
            Some("*"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A change, even one keeping the length and restoring the old
        // modification time, returns the new bytes. Until it settles it has
        // no tag, since another write in the same timestamp tick would not
        // change the revision; afterwards its tag differs from the old one.
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, b"later revision").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let (status, headers, bytes) = file_route(&state, uri, true, Some(&etag)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"later revision");
        assert!(headers.get(header::ETAG).is_none());
        let (status, _, bytes) = file_route(&state, uri, true, Some("*")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"later revision");
        let settled = std::time::SystemTime::now() + RACY_PREVIEW_STAMP;
        let rewritten = preview_etag(&fs::metadata(&file).unwrap(), settled).unwrap();
        assert!(!etag_matches(&etag, &rewritten), "{etag} {rewritten}");

        // A prefix read is partial content with the whole length, never a
        // body that could pass for the complete file.
        let uri = "/api/v1/conversations/etag-chat/workspace/file?root=workspace&path=long.txt";
        let (_, headers, _) = file_route(&state, uri, true, None).await;
        let changed = headers[header::ETAG].to_str().unwrap().to_owned();
        let prefix = format!("{uri}&prefix=5");
        let (status, headers, bytes) = file_route(&state, &prefix, true, None).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(bytes, b"later");
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 0-4/14");
        assert_eq!(headers[header::CONTENT_LENGTH], "5");
        assert_eq!(headers[header::ETAG], changed.as_str());
        let (status, _, bytes) = file_route(&state, &prefix, true, Some(&changed)).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(bytes.is_empty());
        let (status, headers, bytes) =
            file_route(&state, &format!("{uri}&prefix=14"), true, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"later revision");
        assert!(headers.get(header::CONTENT_RANGE).is_none());
        for invalid in ["0", "1048577"] {
            let (status, _, _) =
                file_route(&state, &format!("{uri}&prefix={invalid}"), true, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
        }
        let (status, headers, bytes) = file_route(
            &state,
            &format!("{uri}&prefix={MAX_PREVIEW_PREFIX_BYTES}"),
            true,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"later revision");
        assert!(headers.get(header::CONTENT_RANGE).is_none());
    }

    #[test]
    fn a_restored_modification_time_does_not_make_a_fresh_rewrite_look_settled() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("notes.txt");
        fs::write(&path, b"second").unwrap();
        // A copy that keeps the original's timestamps: the modification time
        // is a minute old, but the change itself happened just now.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert!(preview_etag(&metadata, std::time::SystemTime::now()).is_none());
        let settled = std::time::SystemTime::now() + RACY_PREVIEW_STAMP;
        assert!(preview_etag(&metadata, settled).is_some());
    }

    #[test]
    fn preview_length_and_tag_describe_the_same_revision_when_the_file_grows() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("notes.txt");
        fs::write(&path, b"new").unwrap();
        let settled = std::time::SystemTime::now() - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(settled)
            .unwrap();
        let root = WorkspaceRoot {
            id: "root".into(),
            label: "Root".into(),
            path: crate::canonical_path::canonicalize(folder.path()).unwrap(),
            is_directory: true,
            kind: "workingDirectory",
        };
        // Tags are compared as a later poll would see them, once every
        // change below has settled.
        let now = std::time::SystemTime::now() + RACY_PREVIEW_STAMP * 2;
        let (file, metadata) =
            open_workspace_preview_file(&root, Path::new("notes.txt"), &[], None).unwrap();
        // A concurrent in-place copy grows the file and restores the old
        // modification time after the open but before the body is sent.
        let mut writer = fs::File::options().write(true).open(&path).unwrap();
        io::Write::write_all(&mut writer, b"new data").unwrap();
        writer.set_modified(settled).unwrap();
        let mut body = Vec::new();
        io::Read::take(file, metadata.len())
            .read_to_end(&mut body)
            .unwrap();
        let sent_tag = preview_etag(&metadata, now).unwrap();
        // The answer is self-consistent (3 bytes tagged as the 3-byte
        // revision), so the next poll sees a different tag and downloads the
        // grown file instead of keeping the truncated body as current.
        assert_eq!((metadata.len(), body.as_slice()), (3, b"new".as_slice()));
        let (_, grown) =
            open_workspace_preview_file(&root, Path::new("notes.txt"), &[], None).unwrap();
        assert_eq!(grown.len(), 8);
        let current = preview_etag(&grown, now).unwrap();
        assert!(!etag_matches(&sent_tag, &current), "{sent_tag} {current}");
    }

    #[test]
    fn media_ranges_are_bounded_and_descriptor_revision_detects_changes() {
        let folder = tempfile::tempdir().unwrap();
        let bytes = b"\0\0\0\x18ftypisomabcdefghijkl";
        fs::write(folder.path().join("clip.mp4"), bytes).unwrap();
        fs::write(folder.path().join("false.mp4"), b"not a movie").unwrap();
        let fifo = folder.path().join("pipe.mp4");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.mp4"), bytes).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.mp4"),
            folder.path().join("link.mp4"),
        )
        .unwrap();
        let root = WorkspaceRoot {
            id: "root".into(),
            label: "Root".into(),
            path: crate::canonical_path::canonicalize(folder.path()).unwrap(),
            is_directory: true,
            kind: "workingDirectory",
        };
        // If the read-only open regresses to blocking, it waits for a writer.
        // This one opens only after the read returned (or after a hang guard),
        // so a blocking open fails by elapsed time instead of hanging the suite.
        let delayed_fifo = fifo.clone();
        let (returned, read_returned) = std::sync::mpsc::channel::<()>();
        let delayed_writer = std::thread::spawn(move || {
            let _ = read_returned.recv_timeout(Duration::from_secs(60));
            let mut options = fs::OpenOptions::new();
            options.write(true).custom_flags(libc::O_NONBLOCK);
            let _ = options.open(delayed_fifo);
        });
        let started = std::time::Instant::now();
        assert_eq!(
            read_media_range(&root, Path::new("pipe.mp4"), &[], Some("bytes=0-0"), None)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert!(started.elapsed() < Duration::from_secs(30));
        returned.send(()).unwrap();
        delayed_writer.join().unwrap();
        let first =
            read_media_range(&root, Path::new("clip.mp4"), &[], Some("bytes=0-0"), None).unwrap();
        assert_eq!(first.bytes, bytes[..1]);
        assert_eq!(first.mime, "video/mp4");
        let tail = read_media_range(
            &root,
            Path::new("clip.mp4"),
            &[],
            Some("bytes=-4"),
            Some(&first.revision),
        )
        .unwrap();
        assert_eq!(tail.bytes, bytes[bytes.len() - 4..]);
        assert_eq!(
            parse_media_range("bytes=0-1048576", 2 * MAX_MEDIA_RANGE_BYTES).unwrap_err(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        for invalid in ["bytes=0-1,3-4", "bytes=999-", "bytes=-0", "units=0-1"] {
            assert_eq!(
                read_media_range(
                    &root,
                    Path::new("clip.mp4"),
                    &[],
                    Some(invalid),
                    Some(&first.revision)
                )
                .unwrap_err()
                .0,
                StatusCode::RANGE_NOT_SATISFIABLE
            );
        }
        assert_eq!(
            read_media_range(&root, Path::new("clip.mp4"), &[], Some("bytes=1-2"), None)
                .unwrap_err()
                .0,
            StatusCode::PRECONDITION_REQUIRED
        );
        assert_eq!(
            read_media_range(&root, Path::new("link.mp4"), &[], Some("bytes=0-0"), None)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            read_media_range(&root, Path::new("false.mp4"), &[], Some("bytes=0-0"), None)
                .unwrap_err()
                .0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        fs::write(
            folder.path().join("clip.mp4"),
            b"\0\0\0\x18ftypisomchanged-content",
        )
        .unwrap();
        assert_eq!(
            read_media_range(
                &root,
                Path::new("clip.mp4"),
                &[],
                Some("bytes=1-2"),
                Some(&first.revision)
            )
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn workspace_routes_are_authenticated_and_bound_to_project_roots() {
        let (_dir, mut state) = crate::ingestion::tests::fixture().await;
        let selected_workspace = tempfile::tempdir().unwrap();
        fs::write(selected_workspace.path().join("visible.txt"), b"visible").unwrap();
        fs::write(
            selected_workspace.path().join("clip.mp4"),
            b"\0\0\0\x18ftypisommedia-tail",
        )
        .unwrap();
        fs::write(selected_workspace.path().join(".hidden"), b"hidden").unwrap();
        crate::test_support::project_thread(
            &state.store,
            "workspace-chat",
            "Workspace",
            selected_workspace.path().to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        let (status, _) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/roots",
            false,
        )
        .await;
        assert!(!status.is_success());

        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/roots",
            true,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let roots: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        crate::tests::validate_http_contract("workspaceRoots", &roots);
        assert_eq!(roots["roots"][0]["id"], "workspace");
        assert_eq!(roots["roots"][0]["label"], "Workspace");
        assert_eq!(roots["roots"][0]["kind"], "workingDirectory");
        assert_eq!(roots["roots"][0]["readOnly"], true);
        let selected_workspace_path =
            crate::canonical_path::canonicalize(selected_workspace.path()).unwrap();
        assert!(roots["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root["path"].as_str() == selected_workspace_path.to_str()));

        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/directory?root=workspace&offset=0",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "visible.txt"));
        assert!(!page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == ".hidden"));

        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/file?root=workspace&path=visible.txt",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"visible");
        // Large previews stream from disk with their verified length; only
        // files over the preview limit are refused.
        let large: Vec<u8> = (0..9 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        fs::write(selected_workspace.path().join("large.bin"), &large).unwrap();
        let response = {
            use axum::{body::Body, http::Request};
            use tower::ServiceExt;
            crate::router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/conversations/workspace-chat/workspace/file?root=workspace&path=large.bin")
                        .header("x-wonder-loopback-capability", &state.loopback_capability)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        };
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            large.len().to_string()
        );
        let streamed = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        assert!(streamed.as_ref() == large.as_slice());
        fs::File::create(selected_workspace.path().join("huge.bin"))
            .unwrap()
            .set_len(MAX_WORKSPACE_PREVIEW_BYTES + 1)
            .unwrap();
        let (status, _) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/file?root=workspace&path=huge.bin",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (status, headers, bytes) = media_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/file?root=workspace&path=visible.txt",
            true,
            "bytes=0-0",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(bytes, b"visible");
        let media_uri =
            "/api/v1/conversations/workspace-chat/workspace/media?root=workspace&path=clip.mp4";
        assert_ne!(
            media_route(&state, media_uri, false, "bytes=0-0", None)
                .await
                .0,
            StatusCode::PARTIAL_CONTENT
        );
        let (status, headers, bytes) =
            media_route(&state, media_uri, true, "bytes=0-0", None).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(bytes, [0]);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 0-0/22");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let revision = headers["x-wonder-revision"].to_str().unwrap().to_owned();
        assert_eq!(
            media_route(&state, media_uri, true, "bytes=-4", None)
                .await
                .0,
            StatusCode::PRECONDITION_REQUIRED
        );
        let (status, headers, bytes) =
            media_route(&state, media_uri, true, "bytes=-4", Some(&revision)).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(bytes, b"tail");
        fs::write(
            selected_workspace.path().join("clip.mp4"),
            b"\0\0\0\x18ftypisomchanged-tail",
        )
        .unwrap();
        assert_eq!(
            media_route(&state, media_uri, true, "bytes=1-4", Some(&revision))
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/workspace-chat/workspace/directory?root=unrelated",
                true,
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );

        let grant_dir = tempfile::tempdir().unwrap();
        fs::write(grant_dir.path().join("granted.txt"), b"grant").unwrap();
        let grant = crate::canonical_path::canonicalize(grant_dir.path()).unwrap();
        state
            .store
            .update_project_roots(
                "project-workspace-chat",
                1,
                &[
                    ProjectRootInput {
                        path: selected_workspace_path.to_string_lossy().into_owned(),
                        canonical_path: selected_workspace_path.to_string_lossy().into_owned(),
                    },
                    ProjectRootInput {
                        path: grant.to_string_lossy().into_owned(),
                        canonical_path: grant.to_string_lossy().into_owned(),
                    },
                ],
                0,
                "now",
            )
            .await
            .unwrap();
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/roots",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let roots: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(roots["roots"].as_array().unwrap().len(), 2);
        assert!(roots["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root["path"].as_str() == grant.to_str()
                && root["label"] == grant.file_name().unwrap().to_str().unwrap()));

        state
            .denied_roots
            .push(grant.to_string_lossy().into_owned());
        let (status, _) = workspace_route(
            &state,
            "/api/v1/conversations/workspace-chat/workspace/roots",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn project_workspace_uses_current_verified_roots_without_a_bot() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("project-source");
        let other = workspace.path().join("other-source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(source.join("note.txt"), b"project note").unwrap();
        fs::write(other.join("other.txt"), b"other root").unwrap();
        fs::write(other.join("other.mp4"), b"\0\0\0\x18ftypisomproject-media").unwrap();
        assert!(crate::test_git::git()
            .args(["init", "-q"])
            .current_dir(&source)
            .status()
            .unwrap()
            .success());
        let source_path = source.to_string_lossy().into_owned();
        let other_path = other.to_string_lossy().into_owned();
        let source_canonical = crate::canonical_path::canonicalize(&source)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let other_canonical = crate::canonical_path::canonicalize(&other)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let inputs = [
            ProjectRootInput {
                path: source_path.clone(),
                canonical_path: source_canonical.clone(),
            },
            ProjectRootInput {
                path: other_path.clone(),
                canonical_path: other_canonical.clone(),
            },
        ];
        let project = state
            .store
            .create_project(
                "files-project",
                "request",
                "hash",
                "Files",
                &inputs,
                0,
                "now",
            )
            .await
            .unwrap();
        let project = match project {
            wonder_store::ProjectCreate::Created(project) => project,
            _ => panic!("new project"),
        };
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "project-files",
                project_id: &project.id,
                family: AgentFamily::Codex,
                provider_store: "owner",
                native_session_id: Some("thread"),
                cwd: &source_path,
                roots_revision: project.roots_revision,
                title: "Files",
                model: None,
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

        let (preview, mime) = verified_project_preview_file(
            &state,
            &project.id,
            "project-files",
            "workspace",
            "note.txt",
        )
        .await
        .unwrap();
        assert_eq!(preview, b"project note");
        assert_eq!(mime, "text/plain");
        assert_eq!(
            verified_project_preview_file(
                &state,
                "another-project",
                "project-files",
                "workspace",
                "note.txt"
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(other.join("other.txt"), source.join("escaped.txt"))
                .unwrap();
            assert_ne!(
                verified_project_preview_file(
                    &state,
                    &project.id,
                    "project-files",
                    "workspace",
                    "escaped.txt"
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::OK
            );
        }

        assert_ne!(
            workspace_route(
                &state,
                "/api/v1/conversations/project-files/workspace/roots",
                false
            )
            .await
            .0,
            StatusCode::OK
        );
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/roots",
            true,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let roots: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        crate::tests::validate_http_contract("workspaceRoots", &roots);
        assert_eq!(roots["roots"].as_array().unwrap().len(), 2);
        assert_eq!(roots["roots"][0]["id"], "workspace");
        assert_eq!(roots["roots"][0]["path"], source_canonical);
        assert_eq!(roots["roots"][1]["kind"], "projectRoot");
        let other_id = roots["roots"][1]["id"].as_str().unwrap();
        let draft_scope = format!(
            "/api/v1/conversations/project-files:{}:{}/workspace/roots",
            project.id, project.primary_root_id
        );
        assert_eq!(
            workspace_route(&state, &draft_scope, true).await.0,
            StatusCode::OK
        );
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/project-files:files-project:wrong-root/workspace/roots",
                true
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/directory?root=workspace",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "note.txt"));
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/file?root=workspace&path=note.txt",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"project note");
        // Previews keep their declared type; a web page downloads instead of
        // rendering from the daemon's origin.
        fs::write(source.join("page.html"), b"<p>page</p>").unwrap();
        for (path, attachment) in [("note.txt", false), ("page.html", true)] {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .uri(format!(
                    "/api/v1/conversations/project-files/workspace/file?root=workspace&path={path}"
                ))
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .body(axum::body::Body::empty())
                .unwrap();
            let response = crate::router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let headers = response.headers();
            assert_eq!(
                headers[axum::http::header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(
                headers
                    .get(axum::http::header::CONTENT_DISPOSITION)
                    .is_some_and(|value| value == "attachment"),
                attachment,
                "{path}"
            );
        }
        fs::remove_file(source.join("page.html")).unwrap();
        let (status, bytes) = workspace_route(
            &state,
            &format!(
                "/api/v1/conversations/project-files/workspace/file?root={other_id}&path=other.txt"
            ),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"other root");
        let media_uri = format!(
            "/api/v1/conversations/project-files/workspace/media?root={other_id}&path=other.mp4"
        );
        assert_eq!(
            media_route(&state, &media_uri, true, "bytes=0-0", None)
                .await
                .0,
            StatusCode::PARTIAL_CONTENT
        );
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/git/status?root=workspace",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let git: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(git["available"], true);
        assert!(
            git["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path"] == "note.txt"),
            "{git}"
        );
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/git/diff?root=workspace&path=note.txt",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8(bytes).unwrap().contains("+project note"));

        // A real staged rename needs both verified paths. With only the new
        // pathspec Git reports a new file and loses its source and edited lines.
        fs::create_dir(source.join("private")).unwrap();
        let original = (0..20)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(source.join("private/rename-old.txt"), &original).unwrap();
        let git = |args: &[&str]| {
            let output = crate::test_git::git()
                .args(args)
                .current_dir(&source)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["add", "--", "private/rename-old.txt"]);
        git(&[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Cupola Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ]);
        fs::rename(
            source.join("private/rename-old.txt"),
            source.join("rename-new.txt"),
        )
        .unwrap();
        let staged = original.replace("line 10\n", "line ten changed\n");
        fs::write(source.join("rename-new.txt"), &staged).unwrap();
        git(&[
            "add",
            "-A",
            "--",
            "private/rename-old.txt",
            "rename-new.txt",
        ]);
        fs::write(
            source.join("rename-new.txt"),
            staged.replace("line 11\n", "line eleven changed\n"),
        )
        .unwrap();
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/git/status?root=workspace",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let git_status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            git_status["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path"] == "rename-new.txt"
                    && change["originalPath"] == "private/rename-old.txt"),
            "{git_status}"
        );
        let rename_uri = "/api/v1/conversations/project-files/workspace/git/diff?root=workspace&path=rename-new.txt";
        let (status, bytes) =
            workspace_route(&state, &format!("{rename_uri}&staged=true"), true).await;
        assert_eq!(status, StatusCode::OK);
        let staged_diff: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let staged_diff = staged_diff["diff"].as_str().unwrap();
        assert!(
            staged_diff.contains("rename from private/rename-old.txt"),
            "{staged_diff}"
        );
        assert!(
            staged_diff.contains("rename to rename-new.txt"),
            "{staged_diff}"
        );
        assert!(staged_diff.contains("+line ten changed"), "{staged_diff}");
        let (status, bytes) = workspace_route(&state, rename_uri, true).await;
        assert_eq!(status, StatusCode::OK);
        let unstaged_diff: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(unstaged_diff["diff"]
            .as_str()
            .unwrap()
            .contains("+line eleven changed"));

        // Deleting a whole folder of tracked files lists each deletion in
        // Modified, and each one opens as a diff.
        fs::create_dir_all(source.join("gone/deep")).unwrap();
        fs::write(source.join("gone/deep/removed.txt"), b"removed line\n").unwrap();
        git(&["add", "--", "gone/deep/removed.txt"]);
        git(&[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Cupola Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "folder",
            "--",
            "gone/deep/removed.txt",
        ]);
        fs::remove_dir_all(source.join("gone")).unwrap();
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/git/status?root=workspace",
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let git_status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            git_status["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path"] == "gone/deep/removed.txt"
                    && change["state"] == "deleted"),
            "{git_status}"
        );
        let (status, bytes) = workspace_route(
            &state,
            "/api/v1/conversations/project-files/workspace/git/diff?root=workspace&path=gone/deep/removed.txt",
            true,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let deleted: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            deleted["diff"].as_str().unwrap().contains("-removed line"),
            "{deleted}"
        );

        // A denied rename source must not be exposed by status or used in a diff.
        let git_root = WorkspaceRoot {
            id: "workspace".into(),
            label: "Workspace".into(),
            path: crate::canonical_path::canonicalize(&source).unwrap(),
            is_directory: true,
            kind: "workingDirectory",
        };
        let denied = vec![crate::canonical_path::canonicalize(source.join("private")).unwrap()];
        let hidden = read_workspace_git_status(&git_root, &denied, Some(&git_root.path)).await;
        assert!(hidden.available);
        assert!(!hidden
            .changes
            .iter()
            .any(|change| change.path == "rename-new.txt"));
        assert_eq!(
            validated_git_path(
                &git_root,
                Path::new("private/rename-old.txt"),
                &denied,
                Some(&git_root.path)
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        symlink(
            other.join("other.txt"),
            source.join("private/rename-old.txt"),
        )
        .unwrap();
        assert_eq!(
            validated_git_path(
                &git_root,
                Path::new("private/rename-old.txt"),
                &[],
                Some(&git_root.path)
            )
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let symlink_status = read_workspace_git_status(&git_root, &[], Some(&git_root.path)).await;
        assert!(!symlink_status
            .changes
            .iter()
            .any(|change| change.path == "rename-new.txt"));
        fs::remove_file(source.join("private/rename-old.txt")).unwrap();
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/not-project-files/workspace/roots",
                true
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );

        // A later Project folder edit does not invalidate the retained cwd.
        let updated = state
            .store
            .update_project_roots(
                &project.id,
                project.roots_revision,
                &inputs[..1],
                0,
                "later",
            )
            .await
            .unwrap()
            .unwrap();
        assert!(updated.roots_revision > project.roots_revision);
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/project-files/workspace/roots",
                true
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(workspace_route(&state, &format!("/api/v1/conversations/project-files/workspace/file?root={other_id}&path=other.txt"), true).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            media_route(&state, &media_uri, true, "bytes=0-0", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let updated = state
            .store
            .update_project_roots(
                &project.id,
                updated.roots_revision,
                &inputs[1..],
                0,
                "later",
            )
            .await
            .unwrap()
            .unwrap();
        assert!(updated.root_for(&source_path).is_none());
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/project-files/workspace/roots",
                true
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );

        let restored = state
            .store
            .update_project_roots(&project.id, updated.roots_revision, &inputs, 0, "later")
            .await
            .unwrap()
            .unwrap();
        assert!(restored.root_for(&source_path).is_some());
        let mut denied_state = state.clone();
        denied_state.denied_roots.push(source_path.clone());
        assert_eq!(
            workspace_route(
                &denied_state,
                "/api/v1/conversations/project-files/workspace/roots",
                true
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        fs::rename(&source, workspace.path().join("moved-source")).unwrap();
        symlink(&other, &source).unwrap();
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/project-files/workspace/roots",
                true
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let prior_root = WorkspaceRoot {
            id: "workspace".into(),
            label: "Workspace".into(),
            path: PathBuf::from(source_canonical),
            is_directory: true,
            kind: "workingDirectory",
        };
        assert_eq!(
            resolve_workspace_path(&prior_root, Path::new("other.txt"), &[], None, false)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn workspace_child_route_uses_verified_child_cwd_and_rejects_unrelated_ids() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let child_cwd = dir.path().join("child-workspace");
        fs::create_dir(&child_cwd).unwrap();
        crate::test_support::project_thread(
            &state.store,
            "parent",
            "Parent",
            dir.path().to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        let runtime_path = dir.path().join("runtime.py");
        let source = fs::read_to_string(&runtime_path).unwrap();
        fs::write(
            &runtime_path,
            source.replace(
                "'cwd':'/child/work'",
                &format!("'cwd':'{}'", child_cwd.display()),
            ),
        )
        .unwrap();
        let config = state
            .projects
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        state
            .projects
            .codex
            .lock()
            .await
            .restart(config)
            .await
            .unwrap();
        let health = state.projects.codex.lock().await.health();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, health.clone());
        crate::test_support::bind_project_thread(
            &state,
            "parent",
            wonder_store::AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let child = crate::subagents::register_from_thread(
            &state,
            "thread",
            "parent",
            &serde_json::json!({
                "id":"child-thread",
                "parentThreadId":"thread",
                "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thread","depth":1,"agent_nickname":"Scout","agent_role":"research"}}},
                "status":{"type":"idle"},
                "canAcceptDirectInput":true,
                "cwd":child_cwd.to_string_lossy()
            }),
            health.id(),
        )
        .await
        .unwrap();
        let (status, bytes) = workspace_route(
            &state,
            &format!(
                "/api/v1/conversations/{}/workspace/roots",
                child.conversation_id
            ),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let roots: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(roots["roots"][0]["id"], "child-cwd");
        assert_eq!(roots["roots"][0]["label"], "Workspace");
        assert_eq!(roots["roots"][0]["kind"], "childWorkingDirectory");
        assert_eq!(
            roots["roots"][0]["path"],
            crate::canonical_path::canonicalize(&child_cwd)
                .unwrap()
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(roots["roots"].as_array().unwrap().len(), 1);
        assert!(!roots["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|root| root["kind"] == "appliedGrant"));
        state
            .denied_roots
            .push(child_cwd.to_string_lossy().into_owned());
        assert_eq!(
            workspace_route(
                &state,
                &format!(
                    "/api/v1/conversations/{}/workspace/roots",
                    child.conversation_id
                ),
                true,
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        state.denied_roots.pop();
        let allowed = dir.path().join("other-root");
        fs::create_dir(&allowed).unwrap();
        state
            .store
            .update_project_roots(
                "project-parent",
                1,
                &[ProjectRootInput {
                    path: allowed.to_string_lossy().into_owned(),
                    canonical_path: allowed.to_string_lossy().into_owned(),
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        assert_eq!(
            workspace_route(
                &state,
                &format!(
                    "/api/v1/conversations/{}/workspace/roots",
                    child.conversation_id
                ),
                true,
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            workspace_route(
                &state,
                "/api/v1/conversations/not-a-child/workspace/roots",
                true,
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_preview_tests {
    use super::*;

    /// A saved Project root is the plain `C:\…` path; the browser's checks
    /// canonicalize the same folder and must agree with it (both used to
    /// disagree, so a Project folder opened as "changed").
    #[test]
    fn saved_plain_roots_match_the_canonical_form_used_by_browsing() {
        let folder = tempfile::tempdir().unwrap();
        let plain = crate::canonical_path::canonicalize(folder.path()).unwrap();
        let verbatim = std::fs::canonicalize(folder.path()).unwrap(); // canonical-path: verbatim on purpose
        assert!(!plain.to_string_lossy().starts_with(r"\\?\"));
        for spelling in [&plain, &verbatim] {
            let (current, metadata) = canonical_existing(spelling).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(current, plain, "{}", spelling.display());
        }
        let child = plain.join("src");
        fs::create_dir(&child).unwrap();
        let (child, _) = canonical_existing(&child).unwrap();
        assert!(child.starts_with(plain.to_string_lossy().as_ref()));
        // Protected-folder checks see both spellings as one folder.
        assert_eq!(
            protected_root(&plain).unwrap(),
            protected_root(&verbatim).unwrap()
        );
    }

    #[test]
    fn previews_get_no_tag_without_a_change_time_writers_cannot_restore() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("notes.txt");
        fs::write(&path, b"first").unwrap();
        let settled = std::time::SystemTime::now() + Duration::from_secs(60);
        assert!(preview_etag(&fs::metadata(&path).unwrap(), settled).is_none());
    }
}
