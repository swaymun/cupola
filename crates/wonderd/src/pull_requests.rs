//! Pull requests a Project thread worked with, read through the GitHub CLI the
//! owner already signed in to on this Mac. Wonder never reads or forwards the
//! GitHub token: `gh` keeps it, and the phone receives titles, states and
//! check results and review metadata only.
//!
//! A thread's pull requests are the ones it opened, from its own records
//! (see `records`): what `gh pr create` printed, the branches it pushed, and
//! pull requests opened from Code changes. A link the agent only mentioned or
//! viewed is not one of them. `gh pr list --head` adds the pull request of the
//! branch its folder is on, for the pill only: auto-archive settles a thread
//! by the pull requests it opened (`merged_since`). The newest are read with their state, listed open
//! ones first and then by GitHub's last update, and the total is reported
//! with per-state counts of the ones read. States are cached per pull request (a merged or closed
//! one for longer) and `gh` calls are bounded, so opening a thread repeatedly
//! does not spend the owner's GitHub rate limit. One pull request's page
//! (description, reviews and comments) is read on its own (see `review`).

mod details;
mod records;
mod review;

pub(crate) use review::detail;

use crate::{projects, AppState, OwnerAuthority};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Bumped when the response changes meaning; clients check it.
const VERSION: u32 = 1;
/// A thread's list is reused this long before `gh` is asked again.
const CACHE_TTL: Duration = Duration::from_secs(60);
/// An explicit refresh within this window returns the cached list.
const MIN_REFRESH: Duration = Duration::from_secs(10);
/// Sign-in state changes rarely; checking it costs a process launch.
const AUTH_TTL: Duration = Duration::from_secs(300);
/// Pull requests read with their state per list.
const MAX_PULL_REQUESTS: usize = 20;
/// A merged or closed pull request's state is reused this long.
const TERMINAL_TTL: Duration = Duration::from_secs(3600);
const MAX_CACHED_STATES: usize = 2_000;
const GH_TIMEOUT: Duration = Duration::from_secs(15);
const GH_OUTPUT_LIMIT: u64 = 1024 * 1024;
const OUTPUT_EXCEEDED: &str = "gh output exceeded its limit";
pub(crate) const FIELDS: &str = "number,title,state,isDraft,url,statusCheckRollup,reviewDecision,reviewRequests,mergeStateStatus,mergeable,changedFiles,updatedAt,mergedAt,headRefName,baseRefName,headRefOid,additions,deletions,author";
const MAX_CACHED_THREADS: usize = 200;
/// Pull requests asked for per branch lookup.
const BRANCH_LIMIT: usize = 5;

/// At most two `gh` processes at once across every thread.
static GH_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// Per thread: when it was read, what named its pull requests, and the list.
/// A new link or branch reads GitHub again at once; otherwise the TTL holds.
#[allow(clippy::type_complexity)]
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Sources, PullRequestList)>>> =
    LazyLock::new(Default::default);
static AUTH: LazyLock<Mutex<Option<(Instant, GhAccess)>>> = LazyLock::new(Default::default);
/// Per pull request address: when its state was read, and the state.
static STATES: LazyLock<Mutex<HashMap<String, (Instant, PullRequest)>>> =
    LazyLock::new(Default::default);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestList {
    version: u32,
    /// False when `gh` is not installed or not signed in; the app hides the pill.
    available: bool,
    detail: Option<String>,
    pull_requests: Vec<PullRequest>,
    /// Every pull request the thread opened; more than are listed when the
    /// list keeps only the newest.
    total: usize,
    /// The listed pull requests by state.
    counts: StateCounts,
    /// Every pull request the thread names was read, so `counts` covers all
    /// `total` of them. False while pushed branches await their lookup.
    complete: bool,
    /// The feature branch the thread's folder is on; none on a default branch,
    /// a detached HEAD, outside Git, or when GitHub could not be asked about it.
    branch: Option<BranchState>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BranchState {
    name: String,
    /// It tracks an upstream branch, so a pull request can be opened from it.
    pushed: bool,
    /// The listed open or draft pull request from this branch.
    pull_request: Option<BranchPullRequest>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BranchPullRequest {
    number: u64,
    url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct StateCounts {
    open: usize,
    draft: usize,
    merged: usize,
    closed: usize,
}

impl StateCounts {
    fn of(prs: &[PullRequest]) -> Self {
        let mut counts = Self::default();
        for pr in prs {
            match pr.state {
                "open" => counts.open += 1,
                "draft" => counts.draft += 1,
                "merged" => counts.merged += 1,
                _ => counts.closed += 1,
            }
        }
        counts
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequest {
    pub(crate) number: u64,
    /// `owner/name`.
    pub(crate) repository: String,
    pub(crate) title: String,
    /// `open`, `draft`, `merged` or `closed`.
    pub(crate) state: &'static str,
    pub(crate) checks: CheckSummary,
    pub(crate) url: String,
    /// GitHub's last update (RFC 3339), which picks the newest pull request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
    /// When it merged (RFC 3339); auto-archive compares it with the thread's
    /// last activity. Not sent.
    #[serde(skip)]
    pub(crate) merged_at: Option<String>,
    /// The branch it merges from and into, when Git would accept the names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) head_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base_ref: Option<String>,
    /// The commit its head branch points at on GitHub.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) head_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deletions: Option<u64>,
    /// The author's GitHub login.
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    details: details::Details,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckSummary {
    /// `passing`, `failing`, `pending`, `none` or `unknown`.
    pub(crate) state: &'static str,
    passed: u32,
    /// Finished neutral or skipped: neither passed nor failed, and never a
    /// merge blocker.
    neutral: u32,
    failed: u32,
    pending: u32,
    unknown: u32,
    limited: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    refresh: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
enum GhAccess {
    Missing,
    SignedOut,
    Ready(PathBuf),
}

/// A pull request named by its canonical GitHub address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PullRef {
    owner: String,
    repo: String,
    number: u64,
}

impl PullRef {
    fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }
}

/// Why the computer can't read pull requests: its GitHub CLI is missing or
/// signed out. The app shows the sentence and offers Try again.
fn unavailable_list(installed: bool) -> PullRequestList {
    let detail = if installed {
        "Sign in with the GitHub CLI (gh auth login) on your computer to see pull requests."
    } else {
        "Install the GitHub CLI (gh) on your computer and sign in with gh auth login to see pull requests."
    };
    PullRequestList {
        version: VERSION,
        available: false,
        detail: Some(detail.into()),
        pull_requests: Vec::new(),
        total: 0,
        counts: StateCounts::default(),
        complete: false,
        branch: None,
    }
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    let conversation = match authorized_conversation(&state, &conversation_id).await {
        Ok(conversation) => conversation,
        Err(response) => return response,
    };
    let refresh = query.refresh == Some(true);
    let Ok((opened, unresolved)) = thread_opened(&state, &conversation_id).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "This thread's history could not be read.",
        )
            .into_response();
    };
    let cwd = FsPath::new(&conversation.cwd);
    let sources = Sources::read(cwd, opened, unresolved).await;
    if let Some(cached) = cached(&conversation_id, &sources, refresh) {
        return Json(cached).into_response();
    }
    let gh = match gh_access(refresh).await {
        GhAccess::Ready(gh) => gh,
        GhAccess::Missing => return Json(unavailable_list(false)).into_response(),
        GhAccess::SignedOut => return Json(unavailable_list(true)).into_response(),
    };
    if unresolved {
        records::resolve_in_background(gh.clone(), conversation_id.clone());
    }
    match fetch(&gh, cwd, &sources, refresh).await {
        Ok(list) => {
            store_cached(&conversation_id, sources, &list);
            Json(list).into_response()
        }
        Err(detail) => (StatusCode::SERVICE_UNAVAILABLE, detail).into_response(),
    }
}

/// A Project thread whose folder is included and allowed to run, or the
/// response that refuses it.
async fn authorized_conversation(
    state: &AppState,
    conversation_id: &str,
) -> Result<wonder_store::StoredProjectConversation, Response> {
    let Ok(Some(conversation)) = state.store.project_conversation(conversation_id).await else {
        return Err(StatusCode::NOT_FOUND.into_response());
    };
    let project = match state.store.project(&conversation.project_id).await {
        Ok(Some(project)) => project,
        Ok(None) => return Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project could not be checked.",
            )
                .into_response())
        }
    };
    let project = projects::thread_project(state, project, &conversation.cwd).await;
    if !project.is_included || project.root_for(&conversation.cwd).is_none() {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let denied = state.denied_roots.clone();
    match tokio::task::spawn_blocking(move || projects::validate_execution_roots(&project, &denied))
        .await
    {
        Ok(Ok(_)) => Ok(conversation),
        Ok(Err(_)) => Err(StatusCode::CONFLICT.into_response()),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The Project folders could not be checked.",
        )
            .into_response()),
    }
}

/// The pull requests a thread opened, newest first: those opened from Code
/// changes, then its records. Also whether pushed branches await a lookup.
async fn thread_opened(
    state: &AppState,
    conversation: &str,
) -> Result<(Vec<PullRef>, bool), sqlx::Error> {
    let mut refs: Vec<PullRef> = state
        .store
        .thread_pull_request_links(conversation)
        .await?
        .iter()
        .filter_map(|(url, _)| url.strip_prefix("https://github.com/").and_then(parse_ref))
        .collect();
    let opened = records::opened(&state.store, conversation).await?;
    for reference in opened.refs {
        if !refs.contains(&reference) {
            refs.push(reference);
        }
    }
    Ok((refs, opened.unresolved))
}

/// Whether the thread may read this pull request's page: it opened it, or
/// its last list showed it (the pull request of its folder's branch).
async fn associated(state: &AppState, conversation: &str, url: &str) -> Result<bool, sqlx::Error> {
    let listed = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(conversation)
        .is_some_and(|(_, _, list)| {
            list.pull_requests
                .iter()
                .any(|pr| pr.url.eq_ignore_ascii_case(url))
        });
    Ok(listed
        || thread_opened(state, conversation)
            .await?
            .0
            .iter()
            .any(|r| r.url().eq_ignore_ascii_case(url)))
}

/// Addresses of the pull requests a thread opened from its records, newest
/// first, for its repository index.
pub(crate) async fn opened_addresses(
    state: &AppState,
    conversation: &str,
) -> Result<Vec<String>, sqlx::Error> {
    Ok(records::opened(&state.store, conversation)
        .await?
        .refs
        .iter()
        .map(PullRef::url)
        .collect())
}

/// A pull request's state read moments ago, or a merged or closed one read
/// within the hour; `refresh` asks for a fresher open one.
pub(crate) fn cached_state(url: &str, refresh: bool) -> Option<PullRequest> {
    let states = STATES.lock().unwrap_or_else(|e| e.into_inner());
    let (at, pr) = states.get(url)?;
    let limit = if matches!(pr.state, "merged" | "closed") {
        TERMINAL_TTL
    } else if refresh {
        MIN_REFRESH
    } else {
        CACHE_TTL
    };
    (at.elapsed() < limit).then(|| pr.clone())
}

pub(crate) fn store_state(pr: &PullRequest) {
    let mut states = STATES.lock().unwrap_or_else(|e| e.into_inner());
    if states.len() >= MAX_CACHED_STATES {
        states.retain(|_, (at, pr)| {
            at.elapsed() < CACHE_TTL
                || (matches!(pr.state, "merged" | "closed") && at.elapsed() < TERMINAL_TTL)
        });
        if states.len() >= MAX_CACHED_STATES {
            states.clear();
        }
    }
    states.insert(pr.url.clone(), (Instant::now(), pr.clone()));
}

/// Reads one pull request's state through `gh` and caches it.
pub(crate) async fn view(gh: &FsPath, url: &str) -> Result<PullRequest, String> {
    let bytes = run_gh(gh, None, &["pr", "view", url, "--json", FIELDS]).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let pr = pull_request(&value)
        .filter(|pr| pr.url.eq_ignore_ascii_case(url))
        .ok_or_else(|| "gh returned an invalid pull request".to_owned())?;
    store_state(&pr);
    Ok(pr)
}

/// Whether a merge settled the thread's work: it opened a pull request, none
/// it opened is still open, and one merged after the thread's last activity
/// (`after`, RFC 3339; none means any merge counts). Only the pull requests
/// the thread opened count, never the one of whichever branch its folder is on
/// now, so another thread's merge in a shared checkout settles nothing. `None`
/// when GitHub could not be read or the thread's pull requests could not all
/// be read, which never archives.
pub(crate) async fn merged_since(
    state: &AppState,
    conversation: &wonder_store::StoredProjectConversation,
    after: Option<&str>,
) -> Option<bool> {
    let (refs, unresolved) = thread_opened(state, &conversation.conversation_id)
        .await
        .ok()?;
    if refs.is_empty() && !unresolved {
        return Some(false);
    }
    let GhAccess::Ready(gh) = gh_access(false).await else {
        return None;
    };
    if unresolved {
        // Pushed branches may name more; the next sweep decides.
        records::resolve_in_background(gh.clone(), conversation.conversation_id.clone());
    }
    owned_settlement(&gh, FsPath::new(&conversation.cwd), refs, unresolved, after).await
}

/// See `merged_since`: reads the state of every pull request the thread
/// opened, apart from the pill's list and its cache, and never the folder's
/// branch.
async fn owned_settlement(
    gh: &FsPath,
    cwd: &FsPath,
    refs: Vec<PullRef>,
    unresolved: bool,
    after: Option<&str>,
) -> Option<bool> {
    let owned = Sources {
        refs,
        unresolved,
        ..Sources::default()
    };
    let list = fetch(gh, cwd, &owned, false).await.ok()?;
    settled_by_merge(&list, after)
}

/// See `merged_since`. A list GitHub only partly answered, or one missing some
/// of the thread's pull requests (cut off at `MAX_PULL_REQUESTS`, or with
/// pushed branches not yet looked up), decides nothing: an omitted one may
/// still be open.
fn settled_by_merge(list: &PullRequestList, after: Option<&str>) -> Option<bool> {
    if list.detail.is_some() || !list.complete {
        return None;
    }
    let after = after.and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
    let open = list
        .pull_requests
        .iter()
        .any(|pr| matches!(pr.state, "open" | "draft"));
    let merged_after = list.pull_requests.iter().any(|pr| {
        pr.state == "merged"
            && pr
                .merged_at
                .as_deref()
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .is_some_and(|merged| after.is_none_or(|after| merged > after))
    });
    Some(!open && merged_after)
}

fn cached(conversation: &str, sources: &Sources, refresh: bool) -> Option<PullRequestList> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let (at, known, list) = cache.get(conversation)?;
    let limit = if refresh { MIN_REFRESH } else { CACHE_TTL };
    (known == sources && at.elapsed() < limit).then(|| list.clone())
}

fn store_cached(conversation: &str, sources: Sources, list: &PullRequestList) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= MAX_CACHED_THREADS {
        cache.retain(|_, (at, _, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= MAX_CACHED_THREADS {
            cache.clear();
        }
    }
    cache.insert(
        conversation.to_owned(),
        (Instant::now(), sources, list.clone()),
    );
}

/// The `gh` binary. A Mac app's daemon often runs without the shell's PATH,
/// so Homebrew's locations are checked first.
fn find_gh() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("gh")));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// The signed-in `gh`, or None when it is missing or signed out.
pub(crate) async fn signed_in_gh(refresh: bool) -> Option<PathBuf> {
    match gh_access(refresh).await {
        GhAccess::Ready(gh) => Some(gh),
        GhAccess::Missing | GhAccess::SignedOut => None,
    }
}

/// The `gh` for an action the owner asked for, or why there is none:
/// `Err(false)` when it is not installed, `Err(true)` when it is signed out.
pub(crate) async fn gh_for_action() -> Result<PathBuf, bool> {
    match gh_access(true).await {
        GhAccess::Ready(gh) => Ok(gh),
        GhAccess::Missing => Err(false),
        GhAccess::SignedOut => Err(true),
    }
}

/// Drops a thread's cached pull requests and the pages it read after an
/// action changed them.
pub(crate) fn forget(conversation: &str) {
    review::forget(conversation);
    if let Some((_, _, list)) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(conversation)
    {
        let mut states = STATES.lock().unwrap_or_else(|e| e.into_inner());
        for pr in list.pull_requests {
            states.remove(&pr.url);
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_GH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Answers this test thread's `gh` lookups with a stand-in, signed in.
#[cfg(test)]
pub(crate) fn use_test_gh(gh: PathBuf) {
    TEST_GH.with(|slot| *slot.borrow_mut() = Some(gh));
}

async fn gh_access(refresh: bool) -> GhAccess {
    #[cfg(test)]
    if let Some(gh) = TEST_GH.with(|slot| slot.borrow().clone()) {
        return GhAccess::Ready(gh);
    }
    if let Some((at, access)) = AUTH.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        // A refresh rechecks a missing sign-in sooner, after the owner fixes it.
        let limit = if refresh && !matches!(access, GhAccess::Ready(_)) {
            MIN_REFRESH
        } else {
            AUTH_TTL
        };
        if at.elapsed() < limit {
            return access;
        }
    }
    let access = match find_gh() {
        None => GhAccess::Missing,
        Some(gh) => {
            match run_gh(&gh, None, &["auth", "status", "--hostname", "github.com"]).await {
                Ok(_) => GhAccess::Ready(gh),
                Err(_) => GhAccess::SignedOut,
            }
        }
    };
    *AUTH.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), access.clone()));
    access
}

/// Runs `gh` without a shell, prompts or update checks, bounded in time and
/// output. Arguments are validated pull request addresses and branch names.
pub(crate) async fn run_gh(
    gh: &FsPath,
    cwd: Option<&FsPath>,
    args: &[&str],
) -> Result<Vec<u8>, String> {
    run_gh_limited(gh, cwd, args, GH_OUTPUT_LIMIT).await
}

/// `run_gh` with its own output limit, for reads larger than a list.
async fn run_gh_limited(
    gh: &FsPath,
    cwd: Option<&FsPath>,
    args: &[&str],
    limit: u64,
) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let _slot = GH_SLOTS.acquire().await.map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new(gh);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // `gh` finds the repository through git, which also needs a usable PATH.
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if !path.is_empty() {
        path.push(":");
    }
    path.push("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
    command
        .env("PATH", path)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("gh output unavailable")?;
    let stderr = child.stderr.take().ok_or("gh error output unavailable")?;
    let read = |stream: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>| async move {
        let mut bytes = Vec::new();
        stream
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(bytes)
    };
    tokio::time::timeout(GH_TIMEOUT, async {
        let (out, err, status) =
            tokio::try_join!(read(Box::pin(stdout)), read(Box::pin(stderr)), async {
                child.wait().await.map_err(|e| e.to_string())
            })?;
        if out.len() as u64 > limit {
            return Err(OUTPUT_EXCEEDED.to_owned());
        }
        if !status.success() {
            return Err(String::from_utf8_lossy(&err).chars().take(500).collect());
        }
        Ok(out)
    })
    .await
    .map_err(|_| "gh timed out".to_owned())?
}

/// What names a thread's pull requests: its folder's feature branch and the
/// pull requests it opened, newest first.
#[derive(Clone, Debug, Default, PartialEq)]
struct Sources {
    branch: Option<String>,
    /// The branch tracks an upstream; pushing it changes the list at once.
    pushed: bool,
    refs: Vec<PullRef>,
    /// Pushed branches still wait for their pull request lookup, so `refs`
    /// may miss some; resolving them changes the list at once.
    unresolved: bool,
}

impl Sources {
    async fn read(cwd: &FsPath, refs: Vec<PullRef>, unresolved: bool) -> Self {
        let branch = current_branch(cwd).await;
        let pushed = match &branch {
            Some(_) => has_upstream(cwd).await,
            None => false,
        };
        Self {
            branch,
            pushed,
            refs,
            unresolved,
        }
    }
}

/// Finds the thread's pull requests and reads their current state.
#[cfg(all(test, unix))]
async fn collect(gh: &FsPath, cwd: &FsPath, refs: Vec<PullRef>) -> Result<PullRequestList, String> {
    let sources = Sources::read(cwd, refs, false).await;
    fetch(gh, cwd, &sources, false).await
}

/// Reads the current state of the newest pull requests the sources name.
async fn fetch(
    gh: &FsPath,
    cwd: &FsPath,
    sources: &Sources,
    refresh: bool,
) -> Result<PullRequestList, String> {
    let mut found: Vec<PullRequest> = Vec::new();
    let mut failures = 0;
    let mut attempts = 0;
    // A full page of branch pull requests may not be all of them.
    let mut branch_limited = false;
    // The branch's own open pull request, from this lookup only: it runs in
    // the folder's repository, so a same-named branch in another repository
    // the thread opened a pull request in is never mistaken for it. None when
    // the lookup failed, since then nothing is known about the branch.
    let mut branch_state = None;
    if let Some(branch) = &sources.branch {
        attempts += 1;
        match run_gh(
            gh,
            Some(cwd),
            &[
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "all",
                "--limit",
                &BRANCH_LIMIT.to_string(),
                "--json",
                FIELDS,
            ],
        )
        .await
        {
            Ok(bytes) => match serde_json::from_slice::<Vec<Value>>(&bytes) {
                Ok(rows) => {
                    branch_limited = rows.len() >= BRANCH_LIMIT;
                    for pr in rows.iter().filter_map(pull_request) {
                        store_state(&pr);
                        found.push(pr);
                    }
                    let pull_request = found
                        .iter()
                        .find(|pr| {
                            matches!(pr.state, "open" | "draft")
                                && pr.head_ref.as_deref() == Some(branch)
                        })
                        .map(|pr| BranchPullRequest {
                            number: pr.number,
                            url: pr.url.clone(),
                        });
                    branch_state = Some(BranchState {
                        name: branch.clone(),
                        pushed: sources.pushed,
                        pull_request,
                    });
                }
                Err(_) => failures += 1,
            },
            Err(_) => failures += 1,
        }
    }
    let total = sources.refs.len()
        + found
            .iter()
            .filter(|pr| {
                !sources
                    .refs
                    .iter()
                    .any(|r| r.url().eq_ignore_ascii_case(&pr.url))
            })
            .count();
    // Newest first; a state read moments ago (or a merged one) is reused.
    let mut slots: Vec<Result<PullRequest, String>> = Vec::new();
    let mut pending = Vec::new();
    for reference in &sources.refs {
        if found.len() + slots.len() >= MAX_PULL_REQUESTS {
            break;
        }
        let url = reference.url();
        if found.iter().any(|pr| pr.url.eq_ignore_ascii_case(&url)) {
            continue;
        }
        match cached_state(&url, refresh) {
            Some(pr) => slots.push(Ok(pr)),
            None => {
                pending.push((slots.len(), url));
                slots.push(Err(String::new()));
            }
        }
    }
    let views = futures_util::future::join_all(pending.iter().map(|(_, url)| view(gh, url))).await;
    attempts += views.len();
    for ((slot, _), result) in pending.iter().zip(views) {
        slots[*slot] = result;
    }
    for slot in slots {
        match slot {
            Ok(pr)
                if !found
                    .iter()
                    .any(|known| known.url.eq_ignore_ascii_case(&pr.url)) =>
            {
                found.push(pr)
            }
            Ok(_) => {}
            Err(_) => failures += 1,
        }
    }
    if attempts > 0 && failures == attempts && found.is_empty() {
        return Err("GitHub could not be reached from your computer. Try again.".into());
    }
    sort_open_then_newest(&mut found);
    found.truncate(MAX_PULL_REQUESTS);
    let total = total.max(found.len());
    Ok(PullRequestList {
        version: VERSION,
        available: true,
        detail: (failures > 0).then(|| "Some pull requests could not be loaded.".into()),
        counts: StateCounts::of(&found),
        complete: failures == 0 && !branch_limited && !sources.unresolved && found.len() == total,
        total,
        branch: branch_state,
        pull_requests: found,
    })
}

/// Open and draft pull requests first, then the most recently updated; ties
/// and ones without an update time keep their order, after the dated ones.
fn sort_open_then_newest(prs: &mut [PullRequest]) {
    let updated = |pr: &PullRequest| {
        pr.updated_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
    };
    prs.sort_by(|a, b| {
        let closed = |pr: &PullRequest| matches!(pr.state, "merged" | "closed");
        closed(a)
            .cmp(&closed(b))
            .then_with(|| updated(b).cmp(&updated(a)))
    });
}

/// Whether the folder's branch tracks an upstream branch.
async fn has_upstream(cwd: &FsPath) -> bool {
    let Some(cwd) = cwd.to_str() else {
        return false;
    };
    crate::filesystem::read_only_git_bytes(
        cwd,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .await
    .is_ok()
}

/// The branch the thread's folder is on, when it is a feature branch whose
/// name is safe to hand to `gh`.
async fn current_branch(cwd: &FsPath) -> Option<String> {
    let bytes = crate::filesystem::read_only_git_bytes(
        cwd.to_str()?,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await
    .ok()?;
    let branch = String::from_utf8(bytes).ok()?.trim().to_owned();
    (valid_branch(&branch) && !matches!(branch.as_str(), "main" | "master")).then_some(branch)
}

/// A branch name Git accepts (`git check-ref-format --branch`), bounded, that
/// cannot be read as an option or a revision expression. `#`, `+` and
/// non-English letters are allowed; every caller passes the name as its own
/// argument or inside `refs/heads/`.
pub(crate) fn valid_branch(branch: &str) -> bool {
    branch.len() <= 200
        && !branch.starts_with('-')
        && branch != "HEAD"
        && branch != "@"
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.ends_with('.')
        && !branch.chars().any(|c| {
            c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

fn parse_ref(text: &str) -> Option<PullRef> {
    let take = |s: &str, allowed: fn(u8) -> bool, max: usize| -> Option<usize> {
        let len = s.bytes().take_while(|b| allowed(*b)).count();
        (1..=max).contains(&len).then_some(len)
    };
    let owner_len = take(text, |b| b.is_ascii_alphanumeric() || b == b'-', 39)?;
    let owner = &text[..owner_len];
    let text = text[owner_len..].strip_prefix('/')?;
    let repo_len = take(
        text,
        |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'),
        100,
    )?;
    let repo = &text[..repo_len];
    let text = text[repo_len..].strip_prefix("/pull/")?;
    let digits = take(text, |b| b.is_ascii_digit(), 9)?;
    if owner.starts_with('-') || repo.starts_with('.') {
        return None;
    }
    Some(PullRef {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: text[..digits].parse().ok().filter(|n| *n > 0)?,
    })
}

/// One `gh pr view/list --json` row, or nothing when it is not a github.com
/// pull request with the fields Wonder shows.
pub(crate) fn pull_request(value: &Value) -> Option<PullRequest> {
    let url = value.get("url")?.as_str()?;
    let reference = url
        .strip_prefix("https://github.com/")
        .and_then(parse_ref)?;
    let number = value.get("number")?.as_u64()?;
    if number != reference.number {
        return None;
    }
    let state = match (
        value.get("state")?.as_str()?,
        value.get("isDraft").and_then(Value::as_bool),
    ) {
        ("OPEN", Some(true)) => "draft",
        ("OPEN", _) => "open",
        ("MERGED", _) => "merged",
        ("CLOSED", _) => "closed",
        _ => return None,
    };
    let title: String = value.get("title")?.as_str()?.chars().take(300).collect();
    Some(PullRequest {
        number,
        repository: format!("{}/{}", reference.owner, reference.repo),
        title,
        state,
        checks: checks(value.get("statusCheckRollup")),
        url: reference.url(),
        updated_at: value
            .get("updatedAt")
            .and_then(Value::as_str)
            .filter(|stamp| stamp.len() <= 40)
            .map(str::to_owned),
        merged_at: value
            .get("mergedAt")
            .and_then(Value::as_str)
            .filter(|stamp| stamp.len() <= 40)
            .map(str::to_owned),
        head_ref: branch_name(value.get("headRefName")),
        base_ref: branch_name(value.get("baseRefName")),
        head_oid: value
            .get("headRefOid")
            .and_then(Value::as_str)
            .filter(|oid| valid_oid(oid))
            .map(str::to_ascii_lowercase),
        additions: value.get("additions").and_then(Value::as_u64),
        deletions: value.get("deletions").and_then(Value::as_u64),
        author: login(value.get("author")),
        details: details::Details::read(value),
    })
}

/// A GitHub login (`app/dependabot` and `bot[bot]` included) from an
/// `author` or `user` object.
fn login(person: Option<&Value>) -> Option<String> {
    person
        .and_then(|person| person.get("login"))
        .and_then(Value::as_str)
        .filter(|login| {
            (1..=100).contains(&login.len())
                && login.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/' | b'[' | b']')
                })
        })
        .map(str::to_owned)
}

fn branch_name(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|name| valid_branch(name))
        .map(str::to_owned)
}

/// A full SHA-1 or SHA-256 commit id.
pub(crate) fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Counts check runs (status/conclusion) and commit statuses (state).
fn checks(rollup: Option<&Value>) -> CheckSummary {
    let mut summary = CheckSummary::default();
    let rows = details::read_checks(rollup);
    // gh exports at most its first 100 contexts without page metadata. A full
    // page or an unknown result cannot justify a claim that all checks passed.
    summary.limited = rows.as_ref().is_some_and(|rows| rows.len() >= 100);
    let mut unknown = rows.is_none() || summary.limited;
    for check in rows.into_iter().flatten() {
        match check.state {
            "passing" if check.neutral => summary.neutral += 1,
            "passing" => summary.passed += 1,
            "pending" => summary.pending += 1,
            "failing" => summary.failed += 1,
            _ => {
                unknown = true;
                summary.unknown += 1;
            }
        }
    }
    summary.state = if summary.failed > 0 {
        "failing"
    } else if summary.pending > 0 {
        "pending"
    } else if unknown {
        "unknown"
    } else if summary.passed + summary.neutral > 0 {
        "passing"
    } else {
        "none"
    };
    summary
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn refs(urls: &[&str]) -> Vec<PullRef> {
        urls.iter()
            .map(|url| parse_ref(url.strip_prefix("https://github.com/").unwrap()).unwrap())
            .collect()
    }

    // Only canonical github.com pull request addresses parse; look-alikes
    // and other GitHub pages do not.
    #[test]
    fn pull_request_addresses_parse_strictly() {
        let parse = |url: &str| url.strip_prefix("https://github.com/").and_then(parse_ref);
        assert_eq!(
            parse("https://github.com/other-org/repo.name/pull/7/files").map(|r| r.url()),
            Some("https://github.com/other-org/repo.name/pull/7".to_owned())
        );
        for bad in [
            "https://github.com/swaymun/wonder/issues/3",
            "https://github.com/a/b/pull/0",
            "https://github.com/-x/b/pull/2",
            "https://github.com.evil/a/b/pull/1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    // The cache spares GitHub for an unchanged thread, but a pull request
    // linked since the last read is fetched at once, and a refresh inside ten
    // seconds is answered from the cache.
    #[test]
    fn cache_follows_the_thread_sources() {
        let sources = Sources {
            branch: Some("agent/x".into()),
            pushed: false,
            refs: refs(&["https://github.com/o/r/pull/1"]),
            unresolved: true,
        };
        let list = unavailable_list(true);
        store_cached("cache-test", sources.clone(), &list);
        assert_eq!(cached("cache-test", &sources, false), Some(list.clone()));
        assert_eq!(cached("cache-test", &sources, true), Some(list));
        let linked = Sources {
            refs: refs(&[
                "https://github.com/o/r/pull/2",
                "https://github.com/o/r/pull/1",
            ]),
            ..sources.clone()
        };
        assert_eq!(cached("cache-test", &linked, false), None);
        let pushed = Sources {
            pushed: true,
            ..sources.clone()
        };
        assert_eq!(
            cached("cache-test", &pushed, false),
            None,
            "a push reads again"
        );
        let resolved = Sources {
            unresolved: false,
            ..sources.clone()
        };
        assert_eq!(
            cached("cache-test", &resolved, false),
            None,
            "a resolved branch lookup reads again"
        );
        assert_eq!(cached("other-thread", &sources, false), None);
    }

    // Every name Git accepts as a branch is a branch here, so Commit, Push and
    // the branch's pull request work for `fix#12`; a name Git refuses, or one
    // that reads as an option, never reaches git or gh.
    #[test]
    fn branch_names_follow_git_and_never_read_as_options() {
        let git_accepts = |name: &str| {
            crate::test_git::git()
                .args(["check-ref-format", "--branch", name])
                .output()
                .unwrap()
                .status
                .success()
        };
        for ok in [
            "agent/pr-pill",
            "feature_1.2",
            "claude/pr-pill-8ebce2",
            "fix#12",
            "c++",
            "+plus",
            "a;rm",
            "名前",
            "café/über",
        ] {
            assert!(valid_branch(ok), "{ok}");
            assert!(git_accepts(ok), "git disagrees: {ok}");
        }
        for bad in [
            "",
            "a..b",
            "a b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[b",
            "a\\b",
            "a@{1}",
            "/a",
            "a/",
            "a//b",
            ".a",
            "a/.b",
            "a.",
            "a.lock",
            "a/b.lock/c",
            "tab\there",
        ] {
            assert!(!valid_branch(bad), "{bad}");
            assert!(!git_accepts(bad), "git disagrees: {bad}");
        }
        // Legal to Git, but read as an option or as the current branch.
        for option in ["-flag", "--force", "@"] {
            assert!(!valid_branch(option), "{option}");
        }
        assert!(!valid_branch("HEAD"));
        assert!(!valid_branch(&"a".repeat(201)));
    }

    // GitHub's branch, head and author fields come through when they are what
    // Git and GitHub allow, and are dropped (not guessed) otherwise.
    #[test]
    fn branch_head_and_author_are_validated_or_dropped() {
        let row = json!({"number":5,"title":"t","state":"OPEN","isDraft":false,
            "url":"https://github.com/o/r/pull/5","headRefName":"fix#12","baseRefName":"main",
            "headRefOid":"0123456789ABCDEF0123456789ABCDEF01234567","additions":10,"deletions":2,
            "author":{"login":"app/dependabot","is_bot":true}});
        let pr = serde_json::to_value(pull_request(&row).unwrap()).unwrap();
        assert_eq!(
            (&pr["headRef"], &pr["baseRef"], &pr["headOid"]),
            (
                &json!("fix#12"),
                &json!("main"),
                &json!("0123456789abcdef0123456789abcdef01234567")
            )
        );
        assert_eq!(
            (&pr["additions"], &pr["deletions"], &pr["author"]),
            (&json!(10), &json!(2), &json!("app/dependabot"))
        );
        let mut bad = row.clone();
        bad["headRefName"] = json!("-x");
        bad["baseRefName"] = json!("a..b");
        bad["headRefOid"] = json!("abc123");
        bad["additions"] = json!(-1);
        bad["author"] = json!({"login":"a b"});
        let pr = serde_json::to_value(pull_request(&bad).unwrap()).unwrap();
        for key in ["headRef", "baseRef", "headOid", "additions", "author"] {
            assert!(pr.get(key).is_none(), "{key}: {pr}");
        }
        assert_eq!(pr["deletions"], 2);
    }

    // Check runs report status and conclusion; commit statuses report state.
    #[test]
    fn check_rollups_summarize_failures_first() {
        let rollup = json!([
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SKIPPED"},
            {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
            {"__typename":"StatusContext","state":"PENDING"},
        ]);
        let summary = checks(Some(&rollup));
        assert_eq!(
            (
                summary.state,
                summary.passed,
                summary.neutral,
                summary.pending,
                summary.failed
            ),
            ("pending", 1, 1, 2, 0)
        );
        let failing = json!([{"status":"COMPLETED","conclusion":"FAILURE"},{"state":"SUCCESS"}]);
        assert_eq!(checks(Some(&failing)).state, "failing");
        assert_eq!(checks(Some(&json!([{"state":"SUCCESS"}]))).state, "passing");
        assert_eq!(checks(None).state, "unknown");
        assert_eq!(checks(Some(&json!([]))).state, "none");
    }

    // A neutral or skipped run (a review bot that found something) is not
    // counted as passed, yet never blocks a merge: alone it reads as passing.
    #[test]
    fn neutral_checks_are_counted_apart_and_never_block() {
        let neutral_only = checks(Some(&json!([
            {"status":"COMPLETED","conclusion":"NEUTRAL","name":"Bugbot"},
            {"status":"COMPLETED","conclusion":"SKIPPED"},
        ])));
        assert_eq!(
            (
                neutral_only.state,
                neutral_only.passed,
                neutral_only.neutral
            ),
            ("passing", 0, 2)
        );
        let mixed = checks(Some(&json!([
            {"status":"COMPLETED","conclusion":"SUCCESS"},
            {"state":"SUCCESS"},
            {"status":"COMPLETED","conclusion":"NEUTRAL"},
        ])));
        assert_eq!(
            (mixed.state, mixed.passed, mixed.neutral, mixed.unknown),
            ("passing", 2, 1, 0)
        );
        let failing = checks(Some(&json!([
            {"status":"COMPLETED","conclusion":"NEUTRAL"},
            {"status":"COMPLETED","conclusion":"FAILURE"},
        ])));
        assert_eq!((failing.state, failing.neutral), ("failing", 1));
    }

    // A check's own page comes through only as a plain https address.
    #[test]
    fn check_links_are_https_pages_only() {
        let row = |url: Value| {
            let rollup = json!([{"status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":url}]);
            serde_json::to_value(details::read_checks(Some(&rollup)).unwrap()).unwrap()[0]
                .get("url")
                .cloned()
        };
        let run = "https://github.com/o/r/actions/runs/1/job/2";
        assert_eq!(row(json!(run)), Some(json!(run)));
        let status = json!([{"state":"FAILURE","targetUrl":"https://ci.example.com/b/3?x=1"}]);
        assert_eq!(
            serde_json::to_value(details::read_checks(Some(&status)).unwrap()).unwrap()[0]["url"],
            "https://ci.example.com/b/3?x=1"
        );
        for bad in [
            json!("http://ci.example.com/1"),
            json!("javascript:alert(1)"),
            json!("https://user:pw@ci.example.com/1"),
            json!("https://ci.example.com/a b"),
            json!("https:///path"),
            json!(""),
            json!(7),
        ] {
            assert_eq!(row(bad.clone()), None, "{bad}");
        }
    }

    // Contract: a merge settles a thread for auto-archive only when no pull
    // request is still open and one merged after the thread's last activity; a
    // partly read list decides nothing.
    #[test]
    fn a_merge_after_the_last_activity_settles_the_thread() {
        let pr = |number: u64, state: &str, merged_at: Option<&str>| {
            pull_request(
                &json!({"number":number,"title":"t","state":state,"isDraft":false,
                "url":format!("https://github.com/o/r/pull/{number}"),"mergedAt":merged_at}),
            )
            .unwrap()
        };
        let list = |prs: Vec<PullRequest>| PullRequestList {
            version: VERSION,
            available: true,
            detail: None,
            total: prs.len(),
            counts: StateCounts::of(&prs),
            complete: true,
            pull_requests: prs,
            branch: None,
        };
        let merged = pr(1, "MERGED", Some("2026-10-20T12:00:00Z"));
        let before = Some("2026-10-20T11:00:00.000Z");
        let after = Some("2026-10-20T13:00:00.000Z");
        assert_eq!(merged.merged_at.as_deref(), Some("2026-10-20T12:00:00Z"));
        assert_eq!(
            settled_by_merge(&list(vec![merged.clone()]), before),
            Some(true)
        );
        assert_eq!(
            settled_by_merge(&list(vec![merged.clone()]), None),
            Some(true)
        );
        assert_eq!(
            settled_by_merge(&list(vec![merged.clone()]), after),
            Some(false)
        );
        assert_eq!(settled_by_merge(&list(vec![]), None), Some(false));
        let open = pr(2, "OPEN", None);
        assert_eq!(
            settled_by_merge(&list(vec![open, merged.clone()]), before),
            Some(false)
        );
        let closed = pr(3, "CLOSED", None);
        assert_eq!(settled_by_merge(&list(vec![closed]), None), Some(false));
        let mut partial = list(vec![merged]);
        partial.detail = Some("Some pull requests could not be loaded.".into());
        assert_eq!(settled_by_merge(&partial, before), None);
    }

    // Contract: a list that may miss some of the thread's pull requests (more
    // than are listed, or pushed branches not yet looked up) settles nothing,
    // even when every listed one merged after the last activity.
    #[test]
    fn a_cut_off_or_unresolved_list_settles_nothing() {
        let merged = pull_request(&json!({"number":1,"title":"t","state":"MERGED",
            "isDraft":false,"url":"https://github.com/o/r/pull/1",
            "mergedAt":"2026-10-20T12:00:00Z"}))
        .unwrap();
        let list = PullRequestList {
            version: VERSION,
            available: true,
            detail: None,
            total: 1,
            counts: StateCounts::of(std::slice::from_ref(&merged)),
            complete: true,
            pull_requests: vec![merged],
            branch: None,
        };
        let before = Some("2026-10-20T11:00:00Z");
        assert_eq!(settled_by_merge(&list, before), Some(true));
        let cut_off = PullRequestList {
            total: 2,
            complete: false,
            ..list.clone()
        };
        assert_eq!(settled_by_merge(&cut_off, before), None);
        let unresolved = PullRequestList {
            complete: false,
            ..list
        };
        assert_eq!(settled_by_merge(&unresolved, before), None);
    }

    /// A stand-in `gh` whose branch list names a merged pull request and
    /// whose `pr view` answers #1 as open and every other number as merged.
    fn settlement_gh(dir: &FsPath) -> PathBuf {
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/calls"
case "$1 $2" in
  "pr list") echo '[{"number":50,"title":"Another chat","state":"MERGED","isDraft":false,"url":"https://github.com/o/settle/pull/50","mergedAt":"2026-10-22T10:00:00Z","headRefName":"agent/shared","baseRefName":"main","statusCheckRollup":[]}]' ;;
  "pr view")
    n="${3##*/}"
    if [ "$n" = 1 ]; then
      echo '{"number":1,"title":"Oldest","state":"OPEN","isDraft":false,"url":"https://github.com/o/settle/pull/1","statusCheckRollup":[]}'
    else
      echo "{\"number\":$n,\"title\":\"Done\",\"state\":\"MERGED\",\"isDraft\":false,\"url\":\"https://github.com/o/settle/pull/$n\",\"mergedAt\":\"2026-10-22T10:00:00Z\",\"statusCheckRollup\":[]}"
    fi ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    fn settle_refs(numbers: impl IntoIterator<Item = u64>) -> Vec<PullRef> {
        numbers
            .into_iter()
            .map(|n| parse_ref(&format!("o/settle/pull/{n}")).unwrap())
            .collect()
    }

    // Contract: a chat sharing a checkout is never settled by another chat's
    // merged pull request on the folder's branch. The pill still shows that
    // pull request; only the ones the chat opened settle it.
    #[tokio::test]
    async fn auto_archive_ignores_the_folders_branch_pull_request() {
        let dir = tempfile::tempdir().unwrap();
        let gh = settlement_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "agent/shared");
        let before = Some("2026-10-21T00:00:00Z");
        let display = collect(&gh, &repo, Vec::new()).await.unwrap();
        assert_eq!(display.pull_requests.len(), 1, "the pill lists it");
        assert_eq!(
            settled_by_merge(&display, before),
            Some(true),
            "the folder's branch alone would settle the chat"
        );
        std::fs::remove_file(dir.path().join("calls")).unwrap();
        assert_eq!(
            owned_settlement(&gh, &repo, Vec::new(), false, before).await,
            Some(false)
        );
        assert_eq!(
            owned_settlement(&gh, &repo, settle_refs([2]), false, before).await,
            Some(true),
            "its own merged pull request settles it"
        );
        assert_eq!(
            owned_settlement(&gh, &repo, settle_refs([2, 1]), false, before).await,
            Some(false),
            "one it opened is still open"
        );
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(!calls.contains("pr list"), "{calls}");
    }

    // Contract: with more pull requests than one read covers, the omitted
    // oldest may still be open, so nothing is settled; nor while pushed
    // branches wait for their lookup.
    #[tokio::test]
    async fn auto_archive_never_settles_from_a_cut_off_set() {
        let dir = tempfile::tempdir().unwrap();
        let gh = settlement_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "main");
        let before = Some("2026-10-21T00:00:00Z");
        // Newest first: #21 to #2 merged, the oldest (#1) still open.
        let all = settle_refs((1..=MAX_PULL_REQUESTS as u64 + 1).rev());
        let listed = collect(&gh, &repo, all.clone()).await.unwrap();
        assert_eq!(
            (listed.pull_requests.len(), listed.total),
            (MAX_PULL_REQUESTS, MAX_PULL_REQUESTS + 1)
        );
        assert!(listed.pull_requests.iter().all(|pr| pr.state == "merged"));
        assert_eq!(owned_settlement(&gh, &repo, all, false, before).await, None);
        let newest = settle_refs((2..=MAX_PULL_REQUESTS as u64 + 1).rev());
        assert_eq!(
            owned_settlement(&gh, &repo, newest.clone(), false, before).await,
            Some(true),
            "a set read whole settles"
        );
        assert_eq!(
            owned_settlement(&gh, &repo, newest, true, before).await,
            None
        );
        assert_eq!(
            owned_settlement(&gh, &repo, Vec::new(), true, before).await,
            None
        );
    }

    #[test]
    fn gh_rows_map_to_user_states() {
        let row = |state: &str, draft: bool| {
            json!({"number":5,"title":"Add the pill","state":state,"isDraft":draft,
                   "url":"https://github.com/o/r/pull/5","statusCheckRollup":[]})
        };
        let states: Vec<_> = [
            ("OPEN", false),
            ("OPEN", true),
            ("MERGED", false),
            ("CLOSED", false),
        ]
        .iter()
        .map(|(state, draft)| pull_request(&row(state, *draft)).unwrap().state)
        .collect();
        assert_eq!(states, ["open", "draft", "merged", "closed"]);
        assert!(pull_request(&row("UNKNOWN", false)).is_none());
        let mut mismatched = row("OPEN", false);
        mismatched["number"] = json!(6);
        assert!(
            pull_request(&mismatched).is_none(),
            "number must match the URL"
        );
        let mut enterprise = row("OPEN", false);
        enterprise["url"] = json!("https://github.example.com/o/r/pull/5");
        assert!(pull_request(&enterprise).is_none());
    }

    /// A stand-in `gh` that answers `pr list` for the branch and `pr view` for
    /// one linked pull request, and fails for another.
    fn fake_gh(dir: &FsPath) -> PathBuf {
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/calls"
case "$1 $2" in
  "pr list") echo '[{"number":12,"title":"Branch work","state":"OPEN","isDraft":true,"url":"https://github.com/o/r/pull/12","updatedAt":"2026-10-20T10:00:00Z","headRefName":"agent/pr-pill","baseRefName":"main","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}]' ;;
  "pr view")
    case "$3" in
      */pull/9) echo '{"number":9,"title":"Linked work","state":"MERGED","isDraft":false,"url":"https://github.com/o/r/pull/9","updatedAt":"2026-10-24T10:00:00Z","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"}]}' ;;
      */pull/6) echo '{"number":6,"title":"Older work","state":"MERGED","isDraft":false,"url":"https://github.com/o/r/pull/6","statusCheckRollup":[]}' ;;
      */pull/7) echo '{"number":7,"title":"Newer open work","state":"OPEN","isDraft":false,"url":"https://github.com/o/r/pull/7","updatedAt":"2026-10-22T10:00:00Z","statusCheckRollup":[]}' ;;
      *) echo 'not found' >&2; exit 1 ;;
    esac ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    fn repo_on_branch(dir: &FsPath, branch: &str) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir(&repo).unwrap();
        assert!(crate::test_git::git()
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q", "-b", branch])
            .status()
            .unwrap()
            .success());
        repo
    }

    // Open pull requests come first, newest update first, then merged and
    // closed ones, whatever named them; one the branch already named is not
    // read again, and one that fails is reported, not hidden. The total counts
    // each once, and the per-state counts admit they miss the failed one.
    #[tokio::test]
    async fn thread_pull_requests_combine_branch_and_records_and_match_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "agent/pr-pill");
        let opened = refs(&[
            "https://github.com/o/r/pull/9",
            "https://github.com/o/r/pull/12",
            "https://github.com/o/r/pull/404",
            "https://github.com/o/r/pull/7",
        ]);
        let list = collect(&gh, &repo, opened).await.unwrap();
        let summary: Vec<_> = list
            .pull_requests
            .iter()
            .map(|pr| (pr.number, pr.state, pr.checks.state))
            .collect();
        assert_eq!(
            summary,
            [
                (7, "open", "none"),
                (12, "draft", "passing"),
                (9, "merged", "failing")
            ]
        );
        assert_eq!(
            list.counts,
            StateCounts {
                open: 1,
                draft: 1,
                merged: 1,
                closed: 0
            }
        );
        assert!(!list.complete, "#404 could not be read");
        assert_eq!(
            list.branch,
            Some(BranchState {
                name: "agent/pr-pill".into(),
                pushed: false,
                pull_request: Some(BranchPullRequest {
                    number: 12,
                    url: "https://github.com/o/r/pull/12".into()
                }),
            }),
            "not pushed without an upstream; its draft is the branch's pull request"
        );
        assert_eq!(
            list.pull_requests[1].head_ref.as_deref(),
            Some("agent/pr-pill")
        );
        assert_eq!(
            list.detail.as_deref(),
            Some("Some pull requests could not be loaded.")
        );
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(
            calls.lines().filter(|l| l.contains("/pull/9")).count(),
            1,
            "{calls}"
        );
        assert!(!calls.contains("/pull/12"), "{calls}");
        assert_eq!(list.total, 4);
        assert!(
            calls.contains("pr list --head agent/pr-pill --state all"),
            "{calls}"
        );
        crate::tests::validate_http_contract(
            "projectPullRequestList",
            &serde_json::to_value(&list).unwrap(),
        );
        // Missing and signed out are told apart, so the app's reason is right.
        for installed in [true, false] {
            let unavailable = unavailable_list(installed);
            crate::tests::validate_http_contract(
                "projectPullRequestList",
                &serde_json::to_value(&unavailable).unwrap(),
            );
            let detail = unavailable.detail.unwrap();
            assert_eq!(detail.starts_with("Install"), !installed, "{detail}");
        }
        // The shared fixture the app's decoder is tested against.
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/project-pull-requests-v1.json"
        ))
        .unwrap();
        crate::tests::validate_http_contract("projectPullRequestList", &fixture);
    }

    // A default branch is not a thread's own work, and with nothing linked
    // `gh` is not called at all.
    #[tokio::test]
    async fn default_branch_without_links_needs_no_gh_call() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "main");
        let list = collect(&gh, &repo, Vec::new()).await.unwrap();
        assert!(list.available && list.pull_requests.is_empty() && list.detail.is_none());
        assert!(!dir.path().join("calls").exists());
        assert_eq!(list.branch, None, "main is not the thread's branch");
        assert!(list.complete);
        // Read on main, a linked pull request is still listed and counted.
        let list = collect(&gh, &repo, refs(&["https://github.com/o/r/pull/6"]))
            .await
            .unwrap();
        assert_eq!((list.branch.is_none(), list.total), (true, 1));
        assert!(list.complete && list.counts.merged == 1);
        crate::tests::validate_http_contract(
            "projectPullRequestList",
            &serde_json::to_value(&list).unwrap(),
        );
    }

    // A pushed feature branch says so, which lets the app offer to open a pull
    // request from it, and the contract carries the branch state.
    #[tokio::test]
    async fn a_pushed_branch_reports_its_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "agent/pr-pill");
        let remote = dir.path().join("remote.git");
        let git = |args: &[&str]| {
            let out = crate::test_git::git()
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        assert!(crate::test_git::git()
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .status()
            .unwrap()
            .success());
        git(&["commit", "-q", "--allow-empty", "-m", "start"]);
        git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&["push", "-q", "-u", "origin", "agent/pr-pill"]);
        let list = collect(&gh, &repo, Vec::new()).await.unwrap();
        let branch = list.branch.clone().unwrap();
        assert!(branch.pushed);
        assert_eq!(branch.pull_request.map(|pr| pr.number), Some(12));
        crate::tests::validate_http_contract(
            "projectPullRequestList",
            &serde_json::to_value(&list).unwrap(),
        );
    }

    // The branch's pull request comes from the folder's own repository. A
    // pull request the thread opened elsewhere from a same-named branch is
    // listed, but never taken for this branch's, so Create stays offered; and
    // when GitHub cannot be asked about the branch, nothing is claimed for it.
    #[tokio::test]
    async fn the_branch_pull_request_is_from_the_folders_repository() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
case "$1 $2" in
  "pr list") [ -e "$(dirname "$0")/offline" ] && exit 1; echo '[]' ;;
  "pr view") echo '{"number":3,"title":"Elsewhere","state":"OPEN","isDraft":false,"url":"https://github.com/o/b/pull/3","headRefName":"feature","baseRefName":"main","statusCheckRollup":[]}' ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let repo = repo_on_branch(dir.path(), "feature");
        let elsewhere = refs(&["https://github.com/o/b/pull/3"]);
        let list = collect(&gh, &repo, elsewhere.clone()).await.unwrap();
        assert_eq!(list.pull_requests.len(), 1);
        assert_eq!(
            list.pull_requests[0].head_ref.as_deref(),
            Some("feature"),
            "the same branch name, in another repository"
        );
        let branch = list.branch.unwrap();
        assert_eq!(
            (branch.name.as_str(), branch.pull_request),
            ("feature", None)
        );

        std::fs::write(dir.path().join("offline"), "").unwrap();
        let list = collect(&gh, &repo, elsewhere).await.unwrap();
        assert_eq!(
            list.branch, None,
            "an unanswered branch lookup claims nothing"
        );
        assert!(!list.complete && list.detail.is_some());
    }

    // While pushed branches wait for their pull request lookup, the list may
    // miss some, so it never says it is complete, even an empty one on main.
    #[tokio::test]
    async fn pending_branch_lookups_keep_the_list_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "main");
        let sources = Sources::read(&repo, Vec::new(), true).await;
        let list = fetch(&gh, &repo, &sources, false).await.unwrap();
        assert!(list.pull_requests.is_empty() && list.total == 0);
        assert!(!list.complete);
        let sources = Sources::read(&repo, Vec::new(), false).await;
        assert!(fetch(&gh, &repo, &sources, false).await.unwrap().complete);
    }

    #[tokio::test]
    async fn unreachable_github_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let repo = repo_on_branch(dir.path(), "main");
        let error = collect(&gh, &repo, refs(&["https://github.com/o/r/pull/404"]))
            .await
            .unwrap_err();
        assert!(error.contains("GitHub could not be reached"));
    }
}
