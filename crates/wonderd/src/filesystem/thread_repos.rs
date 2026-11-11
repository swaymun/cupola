//! The repositories a thread worked with: the one its Project folder is in,
//! the ones it edited, created or cloned (inside the Project or anywhere else
//! on the Mac) and the pull requests it opened or reviewed.
//!
//! Derived on the Mac from the runtime's own raw thread items, never from the
//! redacted history the phone receives and never by guessing from the working
//! folder alone. Each path a record names is resolved to its repository with
//! read-only, time-bounded Git inside the deny list. A repository outside the
//! Project's folders is reported by name, branch and counts only: no path, no
//! file names and no diffs.

use super::repo_records::{self, Action, PullAddress, PullObservation, PullTarget, Relation};
use super::*;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

const VERSION: u32 = 1;
/// Local repositories listed per thread; pull-request-only entries are extra.
const MAX_REPOSITORIES: usize = 8;
const MAX_REMOTE_ONLY: usize = 4;
/// Distinct folders resolved with Git per read.
const MAX_FOLDERS: usize = 64;
const MAX_PULL_REQUESTS: usize = 20;
const MAX_GH_CALLS: usize = 10;
/// Counts stop here and say so.
const MAX_COUNT: u32 = 10_000;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const ITEMS_TIMEOUT: Duration = Duration::from_secs(20);
const ITEMS_PAGE: usize = 100;
/// A thread's index is reused this long unless its history changed.
const CACHE_TTL: Duration = Duration::from_secs(20);
/// An explicit refresh within this window still answers from the cache.
const MIN_REFRESH: Duration = Duration::from_secs(5);
const MAX_CACHED_THREADS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
enum Role {
    Workspace,
    Edited,
    Created,
    Cloned,
    Reviewed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RepositoryIndex {
    version: u32,
    /// False when the GitHub CLI is missing or signed out: pull requests are
    /// listed without their state.
    github_available: bool,
    /// The thread's records or repositories went past the bounds.
    truncated: bool,
    detail: Option<String>,
    repositories: Vec<Repository>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    /// Stable across reads; never a path.
    id: String,
    name: String,
    roles: Vec<Role>,
    /// False for a pull request whose repository is not on this Mac.
    local: bool,
    /// Where the existing Files and Code changes readers find it. Only for a
    /// repository inside one of the Project's folders.
    location: Option<Location>,
    /// Commit, push, pull and pull request actions are offered: the whole
    /// repository is inside one of the Project's folders. Repositories
    /// outside them, or larger than the Project folder, stay read only.
    writable: bool,
    /// The branch pull requests target and pushes to it need confirming.
    default_branch: Option<String>,
    /// `owner/name` on GitHub, from its remotes.
    remote: Option<String>,
    branch: Option<String>,
    detached: bool,
    no_commits: bool,
    base_ref: Option<String>,
    upstream: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
    changes: Option<Changes>,
    pull_requests: Vec<PullRequestEntry>,
    detail: Option<String>,
    /// A linked worktree's folder name; null for a main checkout or a
    /// repository without worktrees.
    worktree: Option<String>,
    /// The listed entry this worktree folds into: its repository's main
    /// checkout, else the first-listed worktree of that repository.
    worktree_of: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    root_id: String,
    /// Relative to that root; empty when the root is the repository or inside it.
    path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Changes {
    staged: u32,
    unstaged: u32,
    untracked: u32,
    limited: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestEntry {
    number: u64,
    repository: String,
    url: String,
    /// `opened` or `reviewing`.
    relation: &'static str,
    title: Option<String>,
    /// `open`, `draft`, `merged` or `closed`; null when GitHub was not read.
    state: Option<&'static str>,
    checks: Option<&'static str>,
    /// GitHub's last update (RFC 3339); null when GitHub was not read.
    updated_at: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryIndexQuery {
    refresh: Option<bool>,
}

type CacheEntry = (Instant, Option<(i64, i64)>, RepositoryIndex);
static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> = LazyLock::new(Default::default);

/// Drops a thread's cached index after a Git action changed its repositories.
pub(super) fn forget(conversation: &str) {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(conversation);
}

pub(crate) async fn thread_repositories(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<RepositoryIndexQuery>,
) -> Response {
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let denied = match denied_paths(&state.denied_roots) {
        Ok(denied) => denied,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let refresh = query.refresh == Some(true);
    let mark = state
        .store
        .conversation_history_mark(&conversation_id)
        .await
        .ok();
    if let Some(index) = cached(&conversation_id, mark, refresh) {
        return Json(index).into_response();
    }
    let (items, read_failed) = match thread_items(&state, &conversation_id).await {
        Ok(items) => (items, false),
        Err(()) => (Vec::new(), true),
    };
    // Pull requests opened from Code changes count as opened here, then the
    // ones the thread's records say it opened (no folder: the address's
    // repository decides where each belongs), newest first.
    let mut linked: Vec<(String, PathBuf)> = state
        .store
        .thread_pull_request_links(&conversation_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(url, path)| (url, PathBuf::from(path)))
        .collect();
    linked.extend(
        crate::pull_requests::opened_addresses(&state, &conversation_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|url| (url, PathBuf::new())),
    );
    let gh = crate::pull_requests::signed_in_gh(refresh).await;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut index = build(
        &items,
        &roots,
        &denied,
        &linked,
        gh.as_deref(),
        home.as_deref(),
    )
    .await;
    if read_failed {
        index.detail = Some(
            "This chat's history could not be read on your computer, so only its Project folder is shown."
                .into(),
        );
    } else {
        store_cached(&conversation_id, mark, &index);
    }
    Json(index).into_response()
}

fn cached(conversation: &str, mark: Option<(i64, i64)>, refresh: bool) -> Option<RepositoryIndex> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let (at, known, index) = cache.get(conversation)?;
    let limit = if refresh { MIN_REFRESH } else { CACHE_TTL };
    (mark.is_some() && *known == mark && at.elapsed() < limit).then(|| index.clone())
}

fn store_cached(conversation: &str, mark: Option<(i64, i64)>, index: &RepositoryIndex) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= MAX_CACHED_THREADS {
        cache.retain(|_, (at, _, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= MAX_CACHED_THREADS {
            cache.clear();
        }
    }
    cache.insert(
        conversation.to_owned(),
        (Instant::now(), mark, index.clone()),
    );
}

/// The thread's raw items, newest first, from the runtime that owns it. A
/// thread that has not started yet has none.
async fn thread_items(state: &crate::AppState, conversation: &str) -> Result<Vec<Value>, ()> {
    Ok(thread_turn_items(state, conversation, &mut |_| false)
        .await?
        .items
        .into_iter()
        .map(|(_, item)| item)
        .collect())
}

/// A thread's items, newest first, each with the turn it belongs to.
pub(super) struct TurnItems {
    pub(super) items: Vec<(Option<String>, Value)>,
    /// Older items were not read: the oldest turn listed may be cut short.
    pub(super) more: bool,
}

/// The thread's raw items, newest first, up to the limit or until `enough`
/// says (for an item's turn) that nothing older is needed.
pub(super) async fn thread_turn_items(
    state: &crate::AppState,
    conversation: &str,
    enough: &mut (dyn FnMut(Option<&str>) -> bool + Send),
) -> Result<TurnItems, ()> {
    let thread = match state.store.conversation_thread(conversation).await {
        Ok(Some(thread)) if !thread.is_empty() => thread,
        Ok(_) => {
            return Ok(TurnItems {
                items: Vec::new(),
                more: false,
            })
        }
        Err(_) => return Err(()),
    };
    let runtime = crate::claude::for_thread(state, &thread)
        .await
        .map_err(|_| ())?;
    tokio::time::timeout(ITEMS_TIMEOUT, async {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        while items.len() < repo_records::MAX_ITEMS + 1 {
            let mut params = serde_json::json!({
                "threadId": thread, "limit": ITEMS_PAGE, "sortDirection": "desc",
            });
            if let Some(cursor) = cursor.take() {
                params["cursor"] = cursor.into();
            }
            let response = runtime
                .request("thread/items/list", params)
                .await
                .map_err(|_| ())?;
            let result = response
                .result
                .filter(|_| response.error.is_none())
                .ok_or(())?;
            let data = result.get("data").and_then(Value::as_array).ok_or(())?;
            let mut done = false;
            items.extend(data.iter().take(ITEMS_PAGE).filter_map(|entry| {
                let item = entry.get("item").filter(|item| item.is_object())?;
                let turn = entry
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                done |= enough(turn.as_deref());
                Some((turn, item.clone()))
            }));
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() && next.len() <= 4096 => {
                    cursor = Some(next.to_owned())
                }
                _ => break,
            }
            if done {
                break;
            }
        }
        Ok(TurnItems {
            items,
            more: cursor.is_some(),
        })
    })
    .await
    .map_err(|_| ())?
}

/// One repository found on disk while reading the records.
struct Found {
    top: PathBuf,
    roles: BTreeSet<Role>,
    pulls: Vec<(PullAddress, Relation)>,
    wants_branch_pull: bool,
}

/// Builds the index from raw items (newest first), the pull requests the
/// thread opened (address, and its repository when known, else empty) and
/// the conversation's verified roots, the first being its working folder.
pub(super) async fn build(
    items: &[Value],
    roots: &[WorkspaceRoot],
    denied: &[PathBuf],
    linked: &[(String, PathBuf)],
    gh: Option<&Path>,
    home: Option<&Path>,
) -> RepositoryIndex {
    let cwd = roots
        .first()
        .map(|root| root.path.clone())
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut observations = repo_records::observe(items, &cwd, home);
    // Newest first, like the records: links lead.
    let links = linked.iter().filter_map(|(url, folder)| {
        let address = repo_records::pull_addresses(url).into_iter().next()?;
        Some(PullObservation {
            target: PullTarget::Address(address),
            relation: Relation::Opened,
            folder: folder.clone(),
        })
    });
    observations.pull_requests = links.chain(observations.pull_requests).collect();
    let mut truncated = observations.truncated;
    let mut resolver = Resolver {
        roots,
        denied,
        known: HashMap::new(),
    };
    let mut found: Vec<Found> = Vec::new();
    let add_role = |found: &mut Vec<Found>, top: PathBuf, role: Option<Role>| -> usize {
        let index = match found.iter().position(|repo| repo.top == top) {
            Some(index) => index,
            None => {
                found.push(Found {
                    top,
                    roles: BTreeSet::new(),
                    pulls: Vec::new(),
                    wants_branch_pull: false,
                });
                found.len() - 1
            }
        };
        if let Some(role) = role {
            found[index].roles.insert(role);
        }
        index
    };

    if let Some(root) = roots.first().filter(|root| root.is_directory) {
        if let Some(top) = resolver.repository(&root.path, false).await {
            add_role(&mut found, top, Some(Role::Workspace));
        }
    }
    for (path, action) in &observations.paths {
        let exact = matches!(action, Action::Created | Action::Cloned);
        let Some(top) = resolver.repository(path, exact).await else {
            truncated |= resolver.known.len() >= MAX_FOLDERS;
            continue;
        };
        let role = match action {
            Action::Edited => Role::Edited,
            Action::Created => Role::Created,
            Action::Cloned => Role::Cloned,
        };
        if found.iter().any(|repo| repo.top == top) || found.len() < MAX_REPOSITORIES {
            add_role(&mut found, top, Some(role));
        } else {
            truncated = true;
        }
    }

    // Remotes decide which local repository a pull request address belongs to.
    let mut facts =
        futures_util::future::join_all(found.iter().map(|repo| async { remotes(&repo.top).await }))
            .await;
    let mut remote_only: Vec<(String, Vec<(PullAddress, Relation)>)> = Vec::new();
    let mut pull_count = 0;
    for observation in &observations.pull_requests {
        if pull_count >= MAX_PULL_REQUESTS {
            truncated = true;
            break;
        }
        let PullObservation {
            target,
            relation,
            folder,
        } = observation;
        let folder_repo = match target {
            PullTarget::Address(_)
                if *relation == Relation::Reviewing || folder.as_os_str().is_empty() =>
            {
                None
            }
            _ => resolver.repository(folder, false).await,
        };
        let local = folder_repo.as_ref().map(|top| {
            let known = found.len();
            let index = add_role(&mut found, top.clone(), None);
            if index == known {
                facts.push(Remotes::default());
            }
            index
        });
        match target {
            PullTarget::Branch => {
                if let Some(index) = local {
                    found[index].wants_branch_pull = true;
                    found[index].roles.insert(Role::Edited);
                }
            }
            PullTarget::Number { number, .. } => {
                let Some(index) = local else { continue };
                if facts.len() <= index || facts[index].slugs.is_empty() {
                    let top = found[index].top.clone();
                    facts.resize_with(found.len(), Remotes::default);
                    facts[index] = remotes(&top).await;
                }
                let Some(slug) = facts[index].pull_base() else {
                    continue;
                };
                let Some((owner, repo)) = slug.split_once('/') else {
                    continue;
                };
                let address = PullAddress {
                    owner: owner.into(),
                    repo: repo.into(),
                    number: *number,
                };
                found[index].roles.insert(Role::Reviewed);
                push_pull(&mut found[index].pulls, address, *relation);
                pull_count += 1;
            }
            PullTarget::Address(address) => {
                let slug = address.slug();
                let matched = local.or_else(|| {
                    facts.iter().position(|remotes| {
                        remotes.slugs.iter().any(|s| s.eq_ignore_ascii_case(&slug))
                    })
                });
                pull_count += 1;
                match matched {
                    Some(index) => {
                        found[index].roles.insert(match relation {
                            Relation::Opened => Role::Edited,
                            Relation::Reviewing => Role::Reviewed,
                        });
                        push_pull(&mut found[index].pulls, address.clone(), *relation);
                    }
                    None => {
                        let known = remote_only
                            .iter()
                            .position(|(known, _)| known.eq_ignore_ascii_case(&slug));
                        match known {
                            Some(index) => {
                                push_pull(&mut remote_only[index].1, address.clone(), *relation)
                            }
                            None if remote_only.len() < MAX_REMOTE_ONLY => {
                                remote_only.push((slug, vec![(address.clone(), *relation)]))
                            }
                            None => truncated = true,
                        }
                    }
                }
            }
        }
    }
    facts.resize_with(found.len(), Remotes::default);

    let mut gh_calls = 0usize;
    let mut repositories = Vec::new();
    let local_facts = futures_util::future::join_all(
        found
            .iter()
            .map(|repo| async { git_facts(&repo.top, status_scope(roots, &repo.top)).await }),
    )
    .await;
    let layouts =
        futures_util::future::join_all(found.iter().map(|repo| checkout_layout(&repo.top))).await;
    for ((repo, remotes), git) in found.into_iter().zip(facts).zip(local_facts) {
        let mut roles: Vec<Role> = repo.roles.into_iter().collect();
        // A repository this thread made is its own work; "edited" adds nothing.
        if roles.contains(&Role::Created) || roles.contains(&Role::Cloned) {
            roles.retain(|role| *role != Role::Edited);
        }
        let mut pulls = Vec::new();
        let branch_pull = (repo.wants_branch_pull
            || roles.iter().any(|role| {
                matches!(
                    role,
                    Role::Workspace | Role::Edited | Role::Created | Role::Cloned
                )
            }))
        .then_some(git.branch.as_deref())
        .flatten()
        .filter(|branch| {
            crate::pull_requests::valid_branch(branch) && !matches!(*branch, "main" | "master")
        });
        if let (Some(gh), Some(branch)) = (gh, branch_pull) {
            if gh_calls < MAX_GH_CALLS {
                gh_calls += 1;
                for pr in branch_pull_requests(gh, &repo.top, branch).await {
                    if !pulls
                        .iter()
                        .any(|known: &PullRequestEntry| known.url == pr.url)
                    {
                        pulls.push(pr);
                    }
                }
            }
        }
        for (address, relation) in repo.pulls {
            let entry = pull_entry(gh, &address, relation, &mut gh_calls).await;
            match pulls.iter_mut().find(|known| known.url == entry.url) {
                Some(known) if relation == Relation::Reviewing => known.relation = "reviewing",
                Some(_) => {}
                None => pulls.push(entry),
            }
        }
        let location = resolver_location(roots, &repo.top);
        let writable = location.is_some()
            && roots
                .iter()
                .any(|root| root.is_directory && repo.top.starts_with(&root.path));
        let name = repo
            .top
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Repository".into());
        repositories.push(Repository {
            id: format!("local:{}", short_hash(&repo.top.to_string_lossy())),
            name,
            roles,
            local: true,
            location,
            writable,
            default_branch: git.default_branch,
            remote: remotes.origin,
            branch: git.branch,
            detached: git.detached,
            no_commits: git.no_commits,
            base_ref: git.base_ref,
            upstream: git.upstream,
            ahead: git.ahead,
            behind: git.behind,
            changes: git.changes,
            pull_requests: pulls,
            detail: git.detail,
            worktree: None,
            worktree_of: None,
        });
    }
    // Only local repositories so far, in the order they were found.
    let mut repositories = fold_worktrees(repositories, &layouts);
    for (slug, pulls) in remote_only {
        let mut entries = Vec::new();
        for (address, relation) in pulls {
            entries.push(pull_entry(gh, &address, relation, &mut gh_calls).await);
        }
        let roles = if entries.iter().any(|pr| pr.relation == "reviewing") {
            vec![Role::Reviewed]
        } else {
            Vec::new()
        };
        repositories.push(Repository {
            id: format!("github:{}", slug.to_ascii_lowercase()),
            name: slug.clone(),
            roles,
            local: false,
            location: None,
            writable: false,
            default_branch: None,
            remote: Some(slug),
            branch: None,
            detached: false,
            no_commits: false,
            base_ref: None,
            upstream: None,
            ahead: None,
            behind: None,
            changes: None,
            pull_requests: entries,
            detail: None,
            worktree: None,
            worktree_of: None,
        });
    }
    RepositoryIndex {
        version: VERSION,
        github_available: gh.is_some(),
        truncated,
        detail: None,
        repositories,
    }
}

/// Where a checkout keeps its Git data: its own Git folder and the folder it
/// shares with every worktree of the same repository.
struct CheckoutLayout {
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl CheckoutLayout {
    /// A linked worktree, not the repository's main checkout.
    fn linked(&self) -> bool {
        self.git_dir != self.common_dir
    }

    /// The repository's name: its main checkout's folder, or a bare
    /// repository's folder without ".git".
    fn repository_name(&self) -> Option<String> {
        let folder = self.common_dir.file_name()?.to_string_lossy().into_owned();
        if folder == ".git" {
            return Some(
                self.common_dir
                    .parent()?
                    .file_name()?
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        let name = folder.strip_suffix(".git").unwrap_or(&folder);
        (!name.is_empty()).then(|| name.to_owned())
    }
}

async fn checkout_layout(top: &Path) -> Option<CheckoutLayout> {
    let text = git_line(
        top,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .await?;
    let mut lines = text.lines();
    let git_dir = crate::canonical_path::canonicalize(lines.next()?).ok()?;
    let common_dir = crate::canonical_path::canonicalize(lines.next()?).ok()?;
    Some(CheckoutLayout {
        git_dir,
        common_dir,
    })
}

/// Linked worktrees are checkouts of one repository: each group sharing a
/// Git folder is listed together under the repository's name, led by its main
/// checkout (else its first-listed worktree), at the place its first member
/// was listed. Every entry keeps its own id, location and facts, so its
/// changes and Git actions still open.
fn fold_worktrees(
    mut repositories: Vec<Repository>,
    layouts: &[Option<CheckoutLayout>],
) -> Vec<Repository> {
    let mut groups: Vec<(&Path, Vec<usize>)> = Vec::new();
    for (index, layout) in layouts.iter().enumerate().take(repositories.len()) {
        let Some(layout) = layout else { continue };
        match groups
            .iter_mut()
            .find(|(common, _)| *common == layout.common_dir)
        {
            Some((_, members)) => members.push(index),
            None => groups.push((&layout.common_dir, vec![index])),
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(repositories.len());
    let mut placed = vec![false; repositories.len()];
    for index in 0..repositories.len() {
        if placed[index] {
            continue;
        }
        let members = groups
            .iter()
            .find(|(_, members)| members.contains(&index))
            .map(|(_, members)| members.clone())
            .unwrap_or_else(|| vec![index]);
        let lead = members
            .iter()
            .copied()
            .find(|&member| layouts[member].as_ref().is_some_and(|l| !l.linked()))
            .unwrap_or(members[0]);
        let lead_id = repositories[lead].id.clone();
        let name = layouts[lead]
            .as_ref()
            .and_then(CheckoutLayout::repository_name);
        for &member in std::iter::once(&lead).chain(members.iter().filter(|&&m| m != lead)) {
            placed[member] = true;
            order.push(member);
            let repo = &mut repositories[member];
            // A main checkout keeps its folder name, which is the repository's.
            if layouts[member].as_ref().is_some_and(CheckoutLayout::linked) {
                repo.worktree = Some(repo.name.clone());
                if let Some(name) = &name {
                    repo.name = name.clone();
                }
            }
            if member != lead {
                repo.worktree_of = Some(lead_id.clone());
            }
        }
    }
    let mut slots: Vec<Option<Repository>> = repositories.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}

fn push_pull(pulls: &mut Vec<(PullAddress, Relation)>, address: PullAddress, relation: Relation) {
    match pulls.iter_mut().find(|(known, _)| *known == address) {
        // Reviewing a pull request the thread opened still reads as opened.
        Some(_) => {}
        None => pulls.push((address, relation)),
    }
}

pub(super) fn short_hash(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(value.as_bytes())[..8])
}

fn relation_name(relation: Relation) -> &'static str {
    match relation {
        Relation::Opened => "opened",
        Relation::Reviewing => "reviewing",
    }
}

async fn pull_entry(
    gh: Option<&Path>,
    address: &PullAddress,
    relation: Relation,
    gh_calls: &mut usize,
) -> PullRequestEntry {
    let mut entry = PullRequestEntry {
        number: address.number,
        repository: address.slug(),
        url: address.url(),
        relation: relation_name(relation),
        title: None,
        state: None,
        checks: None,
        updated_at: None,
    };
    let url = address.url();
    // The thread's pull request list reads the same states; reuse them.
    let pr = match (gh, crate::pull_requests::cached_state(&url, false)) {
        (None, _) => None,
        (Some(_), Some(pr)) => Some(pr),
        (Some(gh), None) if *gh_calls < MAX_GH_CALLS => {
            *gh_calls += 1;
            crate::pull_requests::view(gh, &url).await.ok()
        }
        (Some(_), None) => None,
    };
    if let Some(pr) = pr {
        entry.title = Some(pr.title);
        entry.state = Some(pr.state);
        entry.checks = Some(pr.checks.state);
        entry.updated_at = pr.updated_at;
    }
    entry
}

async fn branch_pull_requests(gh: &Path, top: &Path, branch: &str) -> Vec<PullRequestEntry> {
    let Ok(bytes) = crate::pull_requests::run_gh(
        gh,
        Some(top),
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "3",
            "--json",
            crate::pull_requests::FIELDS,
        ],
    )
    .await
    else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<Value>>(&bytes)
        .unwrap_or_default()
        .iter()
        .filter_map(crate::pull_requests::pull_request)
        .inspect(crate::pull_requests::store_state)
        .map(|pr| PullRequestEntry {
            number: pr.number,
            repository: pr.repository,
            url: pr.url,
            relation: "opened",
            title: Some(pr.title),
            state: Some(pr.state),
            checks: Some(pr.checks.state),
            updated_at: pr.updated_at,
        })
        .collect()
}

/// A Project folder inside a larger repository counts only its own files.
fn status_scope(roots: &[WorkspaceRoot], top: &Path) -> Option<String> {
    roots
        .iter()
        .filter(|root| root.is_directory && root.path != top && root.path.starts_with(top))
        .find_map(|root| git_pathspec(top, &root.path))
}

/// Where a repository sits among the conversation's roots, when it does.
fn resolver_location(roots: &[WorkspaceRoot], top: &Path) -> Option<Location> {
    roots
        .iter()
        .filter(|root| root.is_directory)
        .find_map(|root| {
            if let Ok(relative) = top.strip_prefix(&root.path) {
                Some(Location {
                    root_id: root.id.clone(),
                    path: relative.to_string_lossy().into_owned(),
                })
            } else if root.path.starts_with(top) {
                Some(Location {
                    root_id: root.id.clone(),
                    path: String::new(),
                })
            } else {
                None
            }
        })
}

struct Resolver<'a> {
    roots: &'a [WorkspaceRoot],
    denied: &'a [PathBuf],
    known: HashMap<PathBuf, Option<PathBuf>>,
}

impl Resolver<'_> {
    /// The Project root a path is inside, which a broad deny may contain.
    fn own_root(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .map(|root| root.path.as_path())
            .find(|root| path.starts_with(root))
    }

    fn protected(&self, path: &Path) -> bool {
        protected_by_denies(path, self.denied, self.own_root(path))
    }

    /// The top of the repository holding `path`. With `exact`, only when the
    /// path itself is that top (a folder the thread created or cloned).
    async fn repository(&mut self, path: &Path, exact: bool) -> Option<PathBuf> {
        if self.protected(path) {
            return None;
        }
        // The nearest folder that still exists: an edited file may be gone.
        let folder = if exact {
            path.to_path_buf()
        } else {
            path.ancestors()
                .find(|candidate| fs::metadata(candidate).is_ok_and(|m| m.is_dir()))?
                .to_path_buf()
        };
        let canonical = crate::canonical_path::canonicalize(&folder).ok()?;
        if !fs::metadata(&canonical).is_ok_and(|m| m.is_dir()) || self.protected(&canonical) {
            return None;
        }
        let top = match self.known.get(&canonical) {
            Some(top) => top.clone(),
            None => {
                if self.known.len() >= MAX_FOLDERS {
                    return None;
                }
                let top = self.show_toplevel(&canonical).await;
                self.known.insert(canonical.clone(), top.clone());
                top
            }
        }?;
        if exact && top != canonical {
            return None;
        }
        Some(top)
    }

    async fn show_toplevel(&self, folder: &Path) -> Option<PathBuf> {
        let line = git_line(folder, &["rev-parse", "--show-toplevel"]).await?;
        let top = crate::canonical_path::canonicalize(line).ok()?;
        (top.parent().is_some() && !self.protected(&top)).then_some(top)
    }
}

async fn git_bytes(folder: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let path = folder.to_str().ok_or("unreadable path")?;
    tokio::time::timeout(GIT_TIMEOUT, read_only_git_bytes(path, args))
        .await
        .map_err(|_| "Git timed out".to_owned())?
}

async fn git_line(folder: &Path, args: &[&str]) -> Option<String> {
    let bytes = git_bytes(folder, args).await.ok()?;
    let line = String::from_utf8(bytes).ok()?.trim().to_owned();
    (!line.is_empty()).then_some(line)
}

#[derive(Default)]
struct Remotes {
    origin: Option<String>,
    upstream: Option<String>,
    slugs: Vec<String>,
}

impl Remotes {
    /// Where `gh` looks for a bare pull request number: the upstream of a
    /// fork, else origin.
    fn pull_base(&self) -> Option<&str> {
        self.upstream
            .as_deref()
            .or(self.origin.as_deref())
            .or(self.slugs.first().map(String::as_str))
    }
}

async fn remotes(top: &Path) -> Remotes {
    let mut found = Remotes::default();
    let Some(text) = git_line(top, &["config", "--get-regexp", r"^remote\..*\.url$"]).await else {
        return found;
    };
    for line in text.lines().take(32) {
        let Some((key, url)) = line.split_once(' ') else {
            continue;
        };
        let Some(slug) = repo_records::remote_slug(url.trim()) else {
            continue;
        };
        match key {
            "remote.origin.url" => found.origin = Some(slug.clone()),
            "remote.upstream.url" => found.upstream = Some(slug.clone()),
            _ => {}
        }
        if !found.slugs.contains(&slug) {
            found.slugs.push(slug);
        }
    }
    found
}

#[derive(Default)]
struct GitFacts {
    branch: Option<String>,
    detached: bool,
    no_commits: bool,
    base_ref: Option<String>,
    default_branch: Option<String>,
    upstream: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
    changes: Option<Changes>,
    detail: Option<String>,
}

async fn git_facts(top: &Path, pathspec: Option<String>) -> GitFacts {
    let mut facts = GitFacts {
        branch: git_line(top, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await,
        ..Default::default()
    };
    let head = git_line(
        top,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "HEAD^{commit}",
        ],
    )
    .await;
    facts.detached = facts.branch.is_none();
    facts.no_commits = head.is_none() && facts.branch.is_some();
    if head.is_some() {
        let base = workspace_git::base(top).await.map(|base| base.label);
        facts.default_branch = base.as_deref().map(default_branch_name);
        facts.base_ref = base.filter(|label| Some(label.as_str()) != facts.branch.as_deref());
        facts.upstream = git_line(
            top,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
        )
        .await;
        if facts.upstream.is_some() {
            if let Some(counts) = git_line(
                top,
                &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
            )
            .await
            {
                let mut counts = counts.split_whitespace().map(|value| value.parse().ok());
                facts.behind = counts.next().flatten();
                facts.ahead = counts.next().flatten();
            }
        }
    }
    let mut args = vec![
        "status",
        "--porcelain=v1",
        "-z",
        "--no-renames",
        "--untracked-files=normal",
        "--ignore-submodules=dirty",
    ];
    if let Some(pathspec) = pathspec.as_deref() {
        args.extend(["--", pathspec]);
    }
    match git_bytes(top, &args).await {
        Ok(bytes) => facts.changes = Some(count_status(&bytes)),
        Err(error) if error.contains("exceeded its limit") => {
            facts.detail = Some("Too many changes to count here.".into())
        }
        Err(_) => facts.detail = Some("Git could not read this repository's changes.".into()),
    }
    facts
}

/// "origin/main" names the default branch "main".
pub(super) fn default_branch_name(base: &str) -> String {
    base.split_once('/')
        .filter(|(remote, _)| *remote == "origin")
        .map(|(_, name)| name)
        .unwrap_or(base)
        .to_owned()
}

/// Counts `git status --porcelain=v1 -z --no-renames` records.
fn count_status(bytes: &[u8]) -> Changes {
    let mut changes = Changes::default();
    for record in bytes.split(|b| *b == 0).filter(|r| r.len() >= 3) {
        let (x, y) = (record[0], record[1]);
        let bump = |count: &mut u32, limited: &mut bool| {
            if *count >= MAX_COUNT {
                *limited = true;
            } else {
                *count += 1;
            }
        };
        if x == b'?' && y == b'?' {
            bump(&mut changes.untracked, &mut changes.limited);
            continue;
        }
        if x == b'!' {
            continue;
        }
        if x != b' ' {
            bump(&mut changes.staged, &mut changes.limited);
        }
        if y != b' ' {
            bump(&mut changes.unstaged, &mut changes.limited);
        }
    }
    changes
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::test_git::git()
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repo_with_commit(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("README"), "hi\n").unwrap();
        git(dir, &["add", "README"]);
        git(dir, &["commit", "-q", "-m", "first"]);
    }

    fn root(id: &str, path: &Path) -> WorkspaceRoot {
        WorkspaceRoot {
            id: id.into(),
            label: id.into(),
            path: crate::canonical_path::canonicalize(path).unwrap(),
            is_directory: true,
            kind: "workingDirectory",
        }
    }

    /// A stand-in `gh` that knows two pull requests and logs every call. It
    /// never reaches GitHub.
    fn fake_gh(dir: &Path) -> PathBuf {
        let gh = dir.join("gh");
        fs::write(
            &gh,
            r#"#!/bin/sh
echo "$@" >> "$(dirname "$0")/calls"
case "$1 $2" in
  "pr list") echo '[]' ;;
  "pr view")
    case "$3" in
      https://github.com/o/c/pull/7) echo '{"number":7,"title":"Fix the parser","state":"OPEN","isDraft":false,"url":"https://github.com/o/c/pull/7","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}' ;;
      https://github.com/o/lib/pull/12) echo '{"number":12,"title":"Lib work","state":"MERGED","isDraft":false,"url":"https://github.com/o/lib/pull/12","statusCheckRollup":[],"updatedAt":"2026-10-19T09:00:00Z"}' ;;
      *) echo 'not found' >&2; exit 1 ;;
    esac ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    fn command(command: &str, cwd: &Path) -> Value {
        json!({"type":"commandExecution","id":command,"command":command,"cwd":cwd,"status":"completed"})
    }

    #[allow(clippy::type_complexity)]
    fn summary(
        index: &RepositoryIndex,
    ) -> Vec<(
        String,
        Vec<Role>,
        bool,
        Vec<(u64, &'static str, Option<&'static str>)>,
    )> {
        index
            .repositories
            .iter()
            .map(|repo| {
                (
                    repo.name.clone(),
                    repo.roles.clone(),
                    repo.no_commits,
                    repo.pull_requests
                        .iter()
                        .map(|pr| (pr.number, pr.relation, pr.state))
                        .collect(),
                )
            })
            .collect()
    }

    // The done check for the Git pill: a thread that edits its Project repo A,
    // creates repo B with `git init` and edits a file in it, and reviews a
    // pull request in a repository that is not on this Mac lists exactly
    // those three, with the roles and states the records support.
    #[tokio::test]
    async fn fixture_thread_lists_its_workspace_new_repository_and_reviewed_pull_request() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A");
        repo_with_commit(&a);
        let gh = fake_gh(dir.path());
        // What the thread did, oldest first on disk; records arrive newest first.
        fs::write(a.join("README"), "edited\n").unwrap();
        fs::create_dir(a.join("B")).unwrap();
        git(&a.join("B"), &["init", "-q", "-b", "main"]);
        fs::write(a.join("B/x"), "new\n").unwrap();
        let a = crate::canonical_path::canonicalize(&a).unwrap();
        let mut items = vec![
            json!({"type":"fileChange","id":"e1","changes":[{"path":a.join("README"),"kind":{"type":"update"}}]}),
            command("/bin/zsh -lc 'mkdir B && cd B && git init'", &a),
            json!({"type":"fileChange","id":"e2","changes":[{"path":a.join("B/x"),"kind":{"type":"add"}}]}),
            command(
                "gh pr review https://github.com/o/c/pull/7 --comment -b 'Looks right'",
                &a,
            ),
        ];
        items.reverse();
        let index = build(&items, &[root("workspace", &a)], &[], &[], Some(&gh), None).await;
        assert_eq!(
            summary(&index),
            [
                (
                    "A".to_owned(),
                    vec![Role::Workspace, Role::Edited],
                    false,
                    vec![]
                ),
                ("B".to_owned(), vec![Role::Created], true, vec![]),
                (
                    "o/c".to_owned(),
                    vec![Role::Reviewed],
                    false,
                    vec![(7, "reviewing", Some("open"))]
                ),
            ]
        );
        let [repo_a, repo_b, reviewed] = &index.repositories[..] else {
            panic!("three entries")
        };
        assert_eq!(
            repo_a.location,
            Some(Location {
                root_id: "workspace".into(),
                path: String::new()
            })
        );
        assert_eq!(repo_a.branch.as_deref(), Some("main"));
        // README edited; B shows as an untracked folder of A.
        assert_eq!(
            repo_a.changes,
            Some(Changes {
                staged: 0,
                unstaged: 1,
                untracked: 1,
                limited: false
            })
        );
        assert_eq!(
            repo_b.location,
            Some(Location {
                root_id: "workspace".into(),
                path: "B".into()
            })
        );
        assert_eq!(
            repo_b.changes,
            Some(Changes {
                staged: 0,
                unstaged: 0,
                untracked: 1,
                limited: false
            })
        );
        assert!(!reviewed.local && reviewed.location.is_none());
        assert_eq!(
            reviewed.pull_requests[0].title.as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(reviewed.pull_requests[0].checks, Some("passing"));
        assert!(index.github_available && !index.truncated);
        crate::tests::validate_http_contract(
            "threadRepositoryIndex",
            &serde_json::to_value(&index).unwrap(),
        );
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../packages/protocol/fixtures/thread-repositories-v1.json"
        ))
        .unwrap();
        crate::tests::validate_http_contract("threadRepositoryIndex", &fixture);
    }

    // A clone and `git -C` outside the Project are listed by name, branch and
    // counts only. A pull request checked out by number belongs to the
    // repository it ran in, read through its remote.
    #[tokio::test]
    async fn clones_and_outside_repositories_expose_no_paths() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        repo_with_commit(&project);
        let outside = dir.path().join("elsewhere/lib");
        repo_with_commit(&outside);
        git(
            &outside,
            &[
                "remote",
                "add",
                "origin",
                "https://user:secret@github.com/o/lib.git",
            ],
        );
        git(&outside, &["checkout", "-q", "-b", "fix"]);
        fs::write(outside.join("new.txt"), "x").unwrap();
        let cloned = project.join("vendor/tool");
        repo_with_commit(&cloned);
        let gh = fake_gh(dir.path());
        let project = crate::canonical_path::canonicalize(&project).unwrap();
        let items = vec![
            command("git -C ../elsewhere/lib commit -am wip && cd ../elsewhere/lib && gh pr checkout 12", &project),
            command("git clone https://github.com/o/tool vendor/tool", &project),
        ];
        let index = build(
            &items,
            &[root("workspace", &project)],
            &[],
            &[],
            Some(&gh),
            None,
        )
        .await;
        let names: Vec<_> = index
            .repositories
            .iter()
            .map(|r| (r.name.as_str(), r.roles.clone()))
            .collect();
        assert_eq!(
            names,
            [
                ("project", vec![Role::Workspace]),
                ("lib", vec![Role::Edited, Role::Reviewed]),
                ("tool", vec![Role::Cloned]),
            ]
        );
        let lib = &index.repositories[1];
        assert_eq!(lib.location, None, "outside the Project: no path");
        assert_eq!(lib.remote.as_deref(), Some("o/lib"), "never the remote URL");
        assert_eq!(lib.branch.as_deref(), Some("fix"));
        assert_eq!(lib.changes.as_ref().map(|c| c.untracked), Some(1));
        assert_eq!(lib.pull_requests[0].url, "https://github.com/o/lib/pull/12");
        assert_eq!(lib.pull_requests[0].state, Some("merged"));
        assert_eq!(
            lib.pull_requests[0].updated_at.as_deref(),
            Some("2026-10-19T09:00:00Z"),
            "the app orders merged and closed pull requests by GitHub's update time"
        );
        let json = serde_json::to_string(&index).unwrap();
        assert!(
            !json.contains("elsewhere") && !json.contains("secret") && !json.contains("new.txt"),
            "{json}"
        );
        assert_eq!(
            index.repositories[2]
                .location
                .as_ref()
                .map(|l| l.path.as_str()),
            Some("vendor/tool")
        );
        crate::tests::validate_http_contract(
            "threadRepositoryIndex",
            &serde_json::to_value(&index).unwrap(),
        );
    }

    // Linked worktrees are checkouts of one repository, not repositories of
    // their own: each folds into the listed main checkout under the
    // repository's name, keeping its own id, branch, counts and location so
    // its changes and Git actions still open. One outside the Project is
    // still listed without a path.
    #[tokio::test]
    async fn linked_worktrees_fold_into_their_main_repository() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A");
        repo_with_commit(&a);
        let inside = a.join(".local/wt-x");
        fs::create_dir_all(a.join(".local")).unwrap();
        fs::write(a.join(".gitignore"), ".local/\n").unwrap();
        git(&a, &["add", ".gitignore"]);
        git(&a, &["commit", "-q", "-m", "ignore"]);
        git(
            &a,
            &[
                "worktree",
                "add",
                "-q",
                inside.to_str().unwrap(),
                "-b",
                "agent/x",
            ],
        );
        let outside = dir.path().join("elsewhere-wt");
        git(
            &a,
            &[
                "worktree",
                "add",
                "-q",
                outside.to_str().unwrap(),
                "-b",
                "agent/y",
            ],
        );
        fs::write(inside.join("README"), "inside\n").unwrap();
        fs::write(outside.join("new.txt"), "outside\n").unwrap();
        let a = crate::canonical_path::canonicalize(&a).unwrap();
        let inside = crate::canonical_path::canonicalize(&inside).unwrap();
        let outside = crate::canonical_path::canonicalize(&outside).unwrap();
        let items = vec![
            json!({"type":"fileChange","id":"i","changes":[{"path":inside.join("README"),"kind":{"type":"update"}}]}),
            json!({"type":"fileChange","id":"o","changes":[{"path":outside.join("new.txt"),"kind":{"type":"add"}}]}),
        ];
        let index = build(&items, &[root("workspace", &a)], &[], &[], None, None).await;
        let shape: Vec<_> = index
            .repositories
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.worktree.as_deref(),
                    r.worktree_of.clone(),
                    r.branch.as_deref(),
                )
            })
            .collect();
        let main_id = index.repositories[0].id.clone();
        assert_eq!(
            shape,
            [
                ("A", None, None, Some("main")),
                ("A", Some("wt-x"), Some(main_id.clone()), Some("agent/x")),
                ("A", Some("elsewhere-wt"), Some(main_id), Some("agent/y")),
            ]
        );
        let [_, wt_inside, wt_outside] = &index.repositories[..] else {
            panic!("three entries")
        };
        assert_eq!(
            wt_inside.id,
            format!("local:{}", short_hash(&inside.to_string_lossy())),
            "Git actions admit a worktree by its own top"
        );
        assert_eq!(
            wt_inside.location.as_ref().map(|l| l.path.as_str()),
            Some(".local/wt-x")
        );
        assert!(wt_inside.writable);
        assert_eq!(wt_inside.changes.as_ref().map(|c| c.unstaged), Some(1));
        assert_eq!(wt_outside.location, None, "outside the Project: no path");
        assert!(!wt_outside.writable);
        assert_eq!(wt_outside.changes.as_ref().map(|c| c.untracked), Some(1));
        let json = serde_json::to_string(&index).unwrap();
        assert!(!json.contains(&*dir.path().to_string_lossy()), "{json}");
        crate::tests::validate_http_contract(
            "threadRepositoryIndex",
            &serde_json::to_value(&index).unwrap(),
        );
    }

    // Without the main checkout, the first-listed worktree leads the group
    // and still carries the repository's name.
    #[tokio::test]
    async fn worktrees_without_their_main_checkout_fold_into_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("Main");
        repo_with_commit(&main);
        let project = dir.path().join("project");
        repo_with_commit(&project);
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        git(
            &main,
            &["worktree", "add", "-q", one.to_str().unwrap(), "-b", "a"],
        );
        git(
            &main,
            &["worktree", "add", "-q", two.to_str().unwrap(), "-b", "b"],
        );
        let (project, one, two) = (
            crate::canonical_path::canonicalize(&project).unwrap(),
            crate::canonical_path::canonicalize(&one).unwrap(),
            crate::canonical_path::canonicalize(&two).unwrap(),
        );
        let items = vec![
            command("git -C ../one commit --allow-empty -m x", &project),
            command("git -C ../two commit --allow-empty -m x", &project),
        ];
        let index = build(&items, &[root("workspace", &project)], &[], &[], None, None).await;
        let shape: Vec<_> = index
            .repositories
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.worktree.as_deref(),
                    r.worktree_of.is_some(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("project", None, false),
                ("Main", Some("one"), false),
                ("Main", Some("two"), true),
            ]
        );
        assert_eq!(
            index.repositories[2].worktree_of.as_deref(),
            Some(index.repositories[1].id.as_str())
        );
        let _ = (one, two);
    }

    // A protected folder is never resolved, even when the thread edited it,
    // and a created folder that is gone does not credit its parent.
    #[tokio::test]
    async fn deny_list_and_missing_folders_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        repo_with_commit(&project);
        let secret = dir.path().join("secret");
        repo_with_commit(&secret);
        let project = crate::canonical_path::canonicalize(&project).unwrap();
        let secret = crate::canonical_path::canonicalize(&secret).unwrap();
        let items = vec![
            json!({"type":"fileChange","changes":[{"path":secret.join("README")}]}),
            command("git -C ../secret commit -am x", &project),
            command("git init gone", &project),
        ];
        let index = build(
            &items,
            &[root("workspace", &project)],
            std::slice::from_ref(&secret),
            &[],
            None,
            None,
        )
        .await;
        assert_eq!(index.repositories.len(), 1);
        assert_eq!(index.repositories[0].roles, [Role::Workspace]);
        assert!(!index.github_available);
    }

    // Pull requests the thread's records say it opened carry no folder: each
    // joins the local repository whose remote its address names, or is listed
    // by its repository's name, and stays "opened" when later viewed.
    #[tokio::test]
    async fn opened_pull_requests_join_the_repository_their_address_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A");
        repo_with_commit(&a);
        git(
            &a,
            &["remote", "add", "origin", "https://github.com/o/c.git"],
        );
        let gh = fake_gh(dir.path());
        let linked = [
            (
                "https://github.com/o/lib/pull/12".to_owned(),
                PathBuf::new(),
            ),
            ("https://github.com/o/c/pull/7".to_owned(), PathBuf::new()),
        ];
        let items = [command("gh pr view 7", &a)];
        let index = build(
            &items,
            &[root("workspace", &a)],
            &[],
            &linked,
            Some(&gh),
            None,
        )
        .await;
        assert_eq!(
            summary(&index),
            [
                (
                    "A".to_owned(),
                    vec![Role::Workspace, Role::Edited, Role::Reviewed],
                    false,
                    vec![(7, "opened", Some("open"))]
                ),
                (
                    "o/lib".to_owned(),
                    vec![],
                    false,
                    vec![(12, "opened", Some("merged"))]
                ),
            ]
        );
    }

    // Many repositories and pull requests stop at their bounds and say so;
    // without gh, pull requests are still listed, without state.
    #[tokio::test]
    async fn repositories_and_pull_requests_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        repo_with_commit(&project);
        let mut items = Vec::new();
        for i in 0..MAX_REPOSITORIES + 2 {
            let repo = dir.path().join(format!("r{i}"));
            fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            items.push(json!({"type":"fileChange","changes":[{"path":repo.join("f")}]}));
        }
        for n in 1..=MAX_PULL_REQUESTS as u64 + 3 {
            items.push(command(
                &format!("gh pr view https://github.com/o/p{n}/pull/{n}"),
                &project,
            ));
        }
        let index = build(&items, &[root("workspace", &project)], &[], &[], None, None).await;
        let local = index.repositories.iter().filter(|r| r.local).count();
        let pulls: usize = index
            .repositories
            .iter()
            .map(|r| r.pull_requests.len())
            .sum();
        assert_eq!(local, MAX_REPOSITORIES);
        assert!(pulls <= MAX_REMOTE_ONLY);
        assert!(index.truncated);
        assert!(index
            .repositories
            .iter()
            .flat_map(|r| &r.pull_requests)
            .all(|pr| pr.state.is_none()));
    }

    // Over HTTP the index needs the owner, is bound to the conversation's
    // Project, and a nested repository's location opens its changes through
    // the existing readers; a path out of the root is refused.
    #[tokio::test]
    async fn index_route_is_owned_and_nested_locations_open_their_changes() {
        use wonder_store::{AgentFamily, ProjectConversationInsert, ProjectRootInput};
        let route = super::super::tests::workspace_route;
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        repo_with_commit(&source);
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        fs::write(nested.join("new.txt"), "x\n").unwrap();
        let canonical = crate::canonical_path::canonicalize(&source).unwrap();
        let input = ProjectRootInput {
            path: source.to_string_lossy().into_owned(),
            canonical_path: canonical.to_string_lossy().into_owned(),
        };
        let wonder_store::ProjectCreate::Created(project) = state
            .store
            .create_project(
                "repos-project",
                "request",
                "hash",
                "Repos",
                &[input],
                0,
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("new project")
        };
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "repos-chat",
                project_id: &project.id,
                family: AgentFamily::Codex,
                provider_store: "owner",
                native_session_id: None,
                cwd: &source.to_string_lossy(),
                roots_revision: project.roots_revision,
                title: "Repos",
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
        let uri = "/api/v1/conversations/repos-chat/repos";
        assert_ne!(route(&state, uri, false).await.0, StatusCode::OK);
        assert_eq!(
            route(&state, "/api/v1/conversations/unknown-chat/repos", true)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let (status, bytes) = route(&state, uri, true).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let index: Value = serde_json::from_slice(&bytes).unwrap();
        crate::tests::validate_http_contract("threadRepositoryIndex", &index);
        assert_eq!(index["repositories"][0]["name"], "source");
        assert_eq!(index["repositories"][0]["roles"], json!(["workspace"]));

        let changes = "/api/v1/conversations/repos-chat/workspace/git/changes?root=workspace&repo=nested&scope=uncommitted";
        let (status, bytes) = route(&state, changes, true).await;
        assert_eq!(status, StatusCode::OK);
        let changes: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(changes["files"][0]["path"], "new.txt", "{changes}");
        let diff = "/api/v1/conversations/repos-chat/workspace/git/diff?root=workspace&repo=nested&scope=uncommitted&path=new.txt";
        let (status, bytes) = route(&state, diff, true).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&bytes).contains("+x"));
        for refused in ["..", "../source", "/tmp"] {
            let uri = format!("/api/v1/conversations/repos-chat/workspace/git/summary?root=workspace&repo={refused}");
            assert_ne!(
                route(&state, &uri, true).await.0,
                StatusCode::OK,
                "{refused}"
            );
        }
        let unscoped = "/api/v1/conversations/repos-chat/workspace/git/diff?root=workspace&repo=nested&path=new.txt";
        assert_eq!(
            route(&state, unscoped, true).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn status_counts_are_bounded() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"M  staged\0 M unstaged\0MM both\0?? new\0!! ignored\0");
        assert_eq!(
            count_status(&bytes),
            Changes {
                staged: 2,
                unstaged: 2,
                untracked: 1,
                limited: false
            }
        );
        let many = "?? f\0".repeat(MAX_COUNT as usize + 1);
        let counted = count_status(many.as_bytes());
        assert_eq!((counted.untracked, counted.limited), (MAX_COUNT, true));
    }
}
