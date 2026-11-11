//! `@` file search over a conversation's verified workspace roots, for the
//! composer's inline file list. Each root is listed the way the owner's Git
//! sees it (tracked plus untracked, ignored files left out), or by a bounded
//! walk that skips dependency and build folders when it is not a repository.
//! Listings are cached per root for a short time and ranked like T3 Code's
//! `searchRanking` (exact, prefix, word boundary, substring, then in-order
//! letters), basename first. Only regular files and folders inside the root and
//! outside Wonder's protected locations are returned, as paths; nothing is read.
use super::*;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, LazyLock, Mutex},
    time::Instant,
};
use tokio::io::AsyncBufReadExt;
use wonder_store::{AgentFamily, FileMention};

/// Entries kept per root (T3 Code indexes 25,000).
const MAX_INDEX_ENTRIES: usize = 25_000;
const DEFAULT_RESULTS: usize = 20;
const MAX_RESULTS: usize = 50;
const MAX_QUERY_BYTES: usize = 256;
/// A listing older than this is rebuilt on the next search, so new and
/// deleted files show up within seconds without watching the disk.
const INDEX_TTL: Duration = Duration::from_secs(15);
const MAX_CACHED_ROOTS: usize = 16;
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WALK_DEPTH: usize = 16;
static SEARCH_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Folders a non-repository walk never enters: version control, dependencies
/// and build output, which would crowd out the owner's own files.
const SKIPPED_FOLDERS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".build",
    "build",
    "dist",
    "DerivedData",
    "Pods",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cache",
    ".gradle",
];

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceSearchQuery {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
    /// The provider of a new chat that has no conversation yet. An existing
    /// conversation always uses its own provider.
    family: Option<AgentFamily>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceSearchEntry {
    root_id: String,
    root_label: String,
    /// Relative to the root.
    path: String,
    name: String,
    is_directory: bool,
    /// The text the composer inserts for this entry, in the provider's form.
    mention: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceSearchResponse {
    entries: Vec<WorkspaceSearchEntry>,
    /// A root had more entries than are indexed, so a match may be missing.
    truncated: bool,
}

#[derive(Debug)]
struct IndexedEntry {
    path: String,
    lower: String,
    /// Byte offset of the basename in `lower`.
    name_start: usize,
    is_directory: bool,
}

#[derive(Debug)]
struct RootIndex {
    entries: Vec<IndexedEntry>,
    truncated: bool,
    built: Instant,
}

static INDEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<RootIndex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached(path: &Path) -> Option<Arc<RootIndex>> {
    let indexes = INDEXES.lock().unwrap_or_else(|e| e.into_inner());
    indexes
        .get(path)
        .filter(|index| index.built.elapsed() < INDEX_TTL)
        .cloned()
}

fn remember(path: PathBuf, index: Arc<RootIndex>) {
    let mut indexes = INDEXES.lock().unwrap_or_else(|e| e.into_inner());
    indexes.retain(|_, index| index.built.elapsed() < INDEX_TTL);
    while indexes.len() >= MAX_CACHED_ROOTS {
        let Some(oldest) = indexes
            .iter()
            .min_by_key(|(_, index)| index.built)
            .map(|(path, _)| path.clone())
        else {
            break;
        };
        indexes.remove(&oldest);
    }
    indexes.insert(path, index);
}

/// Files Git reports for the root (paths relative to it), or `None` when the
/// root is not a usable repository. Reading stops at the index bound.
async fn git_files(root: &WorkspaceRoot, denied: &[PathBuf]) -> Option<(Vec<String>, bool)> {
    let GitScope::Ready { directory, .. } =
        verified_git_scope(root, denied, Some(root.path.as_path())).await
    else {
        return None;
    };
    let mut prepared = read_only_git_command(
        directory.to_str()?,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )
    .await
    .ok()?;
    let command = &mut prepared.command;
    command.stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let read = async {
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            if reader.read_until(0, &mut buffer).await.ok()? == 0 {
                return Some((files, false));
            }
            if buffer.last() == Some(&0) {
                buffer.pop();
            }
            // A conflicted file is listed once per stage.
            if let Ok(path) = std::str::from_utf8(&buffer) {
                if !path.is_empty() && seen.insert(path.to_owned()) {
                    files.push(path.to_owned());
                }
            }
            if files.len() >= MAX_INDEX_ENTRIES {
                return Some((files, true));
            }
        }
    };
    let listed = tokio::time::timeout(LIST_TIMEOUT, read)
        .await
        .ok()
        .flatten();
    let (files, truncated) = match listed {
        Some((files, true)) => {
            let _ = child.kill().await;
            (files, true)
        }
        Some((files, false)) => {
            let status = tokio::time::timeout(LIST_TIMEOUT, child.wait()).await;
            if !matches!(status, Ok(Ok(status)) if status.success()) {
                return None;
            }
            (files, false)
        }
        None => {
            let _ = child.kill().await;
            return None;
        }
    };
    Some((files, truncated))
}

/// A bounded breadth-first walk for a root that is not a repository. It never
/// follows symlinks and skips [`SKIPPED_FOLDERS`].
fn walk_files(root: &Path, denied: &[PathBuf]) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([(PathBuf::new(), 0usize)]);
    while let Some((relative, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(root.join(&relative)) else {
            continue;
        };
        let mut names: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let kind = entry.file_type().ok()?;
                Some((entry.file_name().to_str()?.to_owned(), kind))
            })
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, kind) in names {
            let path = relative.join(&name);
            if kind.is_dir() {
                if depth < MAX_WALK_DEPTH
                    && !SKIPPED_FOLDERS.contains(&name.as_str())
                    && !protected_by_denies(&root.join(&path), denied, Some(root))
                {
                    queue.push_back((path, depth + 1));
                }
            } else if kind.is_file() {
                files.push(path.to_string_lossy().into_owned());
                if files.len() >= MAX_INDEX_ENTRIES {
                    return (files, true);
                }
            }
        }
    }
    (files, false)
}

/// Keep regular files that still exist inside the root and outside protected
/// locations, then add their folders (T3 Code's `withDirectoryAncestors`).
fn index_entries(root: &Path, files: Vec<String>, denied: &[PathBuf]) -> Vec<IndexedEntry> {
    let mut entries = Vec::new();
    let mut folders = HashSet::new();
    for path in files {
        let relative = Path::new(&path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            continue;
        }
        let absolute = root.join(relative);
        if protected_by_denies(&absolute, denied, Some(root)) {
            continue;
        }
        if !fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let mut parent = relative.parent();
        while let Some(folder) = parent.filter(|folder| !folder.as_os_str().is_empty()) {
            if !folders.insert(folder.to_string_lossy().into_owned()) {
                break;
            }
            parent = folder.parent();
        }
        entries.push(indexed(path, false));
    }
    entries.extend(folders.into_iter().map(|folder| indexed(folder, true)));
    entries
}

fn indexed(path: String, is_directory: bool) -> IndexedEntry {
    let lower = path.to_lowercase();
    let name_start = lower.rfind('/').map_or(0, |slash| slash + 1);
    IndexedEntry {
        path,
        lower,
        name_start,
        is_directory,
    }
}

async fn root_index(root: &WorkspaceRoot, denied: &[PathBuf]) -> Arc<RootIndex> {
    if let Some(index) = cached(&root.path) {
        return index;
    }
    let (files, truncated) = match git_files(root, denied).await {
        Some(listed) => listed,
        None => {
            let path = root.path.clone();
            let denied = denied.to_vec();
            tokio::task::spawn_blocking(move || walk_files(&path, &denied))
                .await
                .unwrap_or_default()
        }
    };
    let path = root.path.clone();
    let denied = denied.to_vec();
    let entries = tokio::task::spawn_blocking(move || index_entries(&path, files, &denied))
        .await
        .unwrap_or_default();
    let index = Arc::new(RootIndex {
        entries,
        truncated,
        built: Instant::now(),
    });
    remember(root.path.clone(), index.clone());
    index
}

/// T3 Code's `scoreQueryMatch`: lower is better; `None` is no match.
#[derive(Clone, Copy)]
struct Bases {
    exact: usize,
    prefix: usize,
    boundary: usize,
    includes: usize,
    fuzzy: usize,
}

const NAME: Bases = Bases {
    exact: 0,
    prefix: 10,
    boundary: 20,
    includes: 30,
    fuzzy: 100,
};
const PATH: Bases = Bases {
    exact: 0,
    prefix: 40,
    boundary: 50,
    includes: 60,
    fuzzy: 200,
};

fn length_penalty(value: &str, query: &str) -> usize {
    value.len().saturating_sub(query.len()).min(64)
}

fn subsequence(value: &str, query: &str) -> Option<usize> {
    let query: Vec<char> = query.chars().collect();
    let mut next = 0;
    let (mut first, mut previous, mut gaps) = (None, 0usize, 0usize);
    for (index, character) in value.chars().enumerate() {
        if next == query.len() {
            break;
        }
        if character != query[next] {
            continue;
        }
        if first.is_none() {
            first = Some(index);
        } else {
            gaps += index - previous - 1;
        }
        previous = index;
        next += 1;
    }
    if next < query.len() {
        return None;
    }
    let first = first.unwrap_or(0);
    let span = previous + 1 - first - query.len();
    Some(first * 2 + gaps * 3 + span + length_penalty(value, &query.iter().collect::<String>()))
}

/// A normalized query with its word-boundary patterns built once per search.
struct SearchQuery {
    text: String,
    boundaries: Vec<String>,
}

impl SearchQuery {
    fn new(text: String) -> Self {
        let boundaries = [" ", "-", "_", "/", "."]
            .iter()
            .map(|marker| format!("{marker}{text}"))
            .collect();
        Self { text, boundaries }
    }
}

fn score(value: &str, search: &SearchQuery, bases: Bases) -> Option<usize> {
    let query = search.text.as_str();
    if value.is_empty() || query.is_empty() {
        return None;
    }
    if value == query {
        return Some(bases.exact);
    }
    if value.starts_with(query) {
        return Some(bases.prefix + length_penalty(value, query));
    }
    // Every marker is one byte.
    let boundary = search
        .boundaries
        .iter()
        .filter_map(|pattern| value.find(pattern.as_str()).map(|index| index + 1))
        .min();
    if let Some(index) = boundary {
        return Some(bases.boundary + index * 2 + length_penalty(value, query));
    }
    if let Some(index) = value.find(query) {
        return Some(bases.includes + index * 2 + length_penalty(value, query));
    }
    subsequence(value, query).map(|score| bases.fuzzy + score)
}

fn entry_score(entry: &IndexedEntry, query: &SearchQuery) -> Option<usize> {
    if query.text.is_empty() {
        // Without a query, the shallowest entries come first.
        return Some(entry.lower.matches('/').count() * 2 + usize::from(!entry.is_directory));
    }
    let name = score(&entry.lower[entry.name_start..], query, NAME);
    let path = score(&entry.lower, query, PATH);
    match (name, path) {
        (Some(name), Some(path)) => Some(name.min(path)),
        (name, path) => name.or(path),
    }
}

/// T3 Code trims a leading `@`, `.` or `/` and lowercases the query.
fn normalized_query(query: &str) -> String {
    query
        .trim()
        .trim_start_matches(['@', '.', '/'])
        .to_lowercase()
}

/// The provider's mention for `path`. `None` when the provider cannot express
/// it (a quoted `@` path cannot contain a quote).
fn mention(style: FileMention, path: &str) -> Option<String> {
    match style {
        FileMention::None => None,
        FileMention::PlainText if path.chars().any(char::is_whitespace) => {
            serde_json::to_string(path).ok()
        }
        FileMention::PlainText => Some(path.to_owned()),
        FileMention::AtPath if path.chars().any(char::is_whitespace) => {
            (!path.contains('"')).then(|| format!("@\"{path}\""))
        }
        FileMention::AtPath => Some(format!("@{path}")),
    }
}

fn ranked<'a>(
    roots: &'a [(WorkspaceRoot, Arc<RootIndex>)],
    query: &SearchQuery,
    limit: usize,
) -> Vec<(&'a WorkspaceRoot, &'a IndexedEntry)> {
    let mut scored: Vec<(usize, usize, &str, &WorkspaceRoot, &IndexedEntry)> = Vec::new();
    for (order, (root, index)) in roots.iter().enumerate() {
        for entry in &index.entries {
            if let Some(score) = entry_score(entry, query) {
                scored.push((score, order, entry.path.as_str(), root, entry));
            }
        }
    }
    scored.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, _, root, entry)| (root, entry))
        .collect()
}

pub(crate) async fn workspace_search(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceSearchQuery>,
) -> Response {
    if query.q.len() > MAX_QUERY_BYTES || query.q.contains('\0') {
        return workspace_error(StatusCode::BAD_REQUEST, "This search is too long.");
    }
    let family = match state.store.project_conversation(&conversation_id).await {
        Ok(Some(conversation)) => Some(conversation.family),
        Ok(None) => query.family,
        Err(_) => {
            return workspace_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The conversation could not be checked.",
            )
        }
    };
    let (roots, _) = match conversation_workspace_roots(&state, &conversation_id, false).await {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Some(family) = family else {
        return workspace_error(StatusCode::BAD_REQUEST, "Choose an agent for this chat.");
    };
    let style = family.provider().file_mention;
    if style == FileMention::None {
        return workspace_error(
            StatusCode::NOT_FOUND,
            "This provider has no file mentions Cupola can insert.",
        );
    }
    let denied = match denied_paths(&state.denied_roots) {
        Ok(value) => value,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let Ok(_slot) = SEARCH_SLOTS.acquire().await else {
        return workspace_error(StatusCode::SERVICE_UNAVAILABLE, "Search is unavailable.");
    };
    let mut indexed = Vec::new();
    for root in roots.into_iter().filter(|root| root.is_directory) {
        let index = root_index(&root, &denied).await;
        indexed.push((root, index));
    }
    let limit = query.limit.unwrap_or(DEFAULT_RESULTS).clamp(1, MAX_RESULTS);
    let search = SearchQuery::new(normalized_query(&query.q));
    let entries = ranked(&indexed, &search, limit * 2)
        .into_iter()
        .filter_map(|(root, entry)| {
            // The working folder is the agent's cwd, so its paths stay
            // relative; another Project folder needs its full path.
            let target = if root.kind == "projectRoot" {
                root.path.join(&entry.path).to_string_lossy().into_owned()
            } else {
                entry.path.clone()
            };
            Some(WorkspaceSearchEntry {
                root_id: root.id.clone(),
                root_label: root.label.clone(),
                name: entry.path[entry.path.rfind('/').map_or(0, |i| i + 1)..].to_owned(),
                path: entry.path.clone(),
                is_directory: entry.is_directory,
                mention: mention(style, &target)?,
            })
        })
        .take(limit)
        .collect();
    Json(WorkspaceSearchResponse {
        entries,
        truncated: indexed.iter().any(|(_, index)| index.truncated),
    })
    .into_response()
}

#[cfg(all(test, unix))]
pub(in crate::filesystem) fn forget_indexes() {
    INDEXES.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> Arc<RootIndex> {
        Arc::new(RootIndex {
            entries: paths
                .iter()
                .map(|path| indexed(path.trim_end_matches('/').into(), path.ends_with('/')))
                .collect(),
            truncated: false,
            built: Instant::now(),
        })
    }

    fn root(id: &str) -> WorkspaceRoot {
        WorkspaceRoot {
            id: id.into(),
            label: id.into(),
            path: PathBuf::from("/w"),
            is_directory: true,
            kind: "workingDirectory",
        }
    }

    fn top(paths: &[&str], query: &str, limit: usize) -> Vec<String> {
        let roots = vec![(root("workspace"), index(paths))];
        ranked(&roots, &SearchQuery::new(normalized_query(query)), limit)
            .into_iter()
            .map(|(_, entry)| entry.path.clone())
            .collect()
    }

    // Contract: ranking follows T3 Code's tiers on the basename first, then
    // the whole path, with ties broken by path; the result count is bounded.
    #[test]
    fn search_ranks_like_t3_and_stays_bounded() {
        let paths = [
            "docs/readme-old.md",
            "README.md",
            "src/reader.rs",
            "src/bin/read.rs",
            "src/",
            "src/bin/",
            "apps/ios/Wonder/ComposerEditor.swift",
            "apps/ios/",
        ];
        assert_eq!(
            top(&paths, "read", 3),
            ["src/bin/read.rs", "README.md", "src/reader.rs"]
        );
        // Exact basename beats a longer prefix; a folder matches by name.
        assert_eq!(top(&paths, "@bin", 1), ["src/bin"]);
        // A path query reaches through folders; fuzzy letters still match.
        assert_eq!(
            top(&paths, "ios/wonder/comp", 1),
            ["apps/ios/Wonder/ComposerEditor.swift"]
        );
        assert_eq!(
            top(&paths, "cmpsredt", 1),
            ["apps/ios/Wonder/ComposerEditor.swift"]
        );
        assert!(top(&paths, "zzz", 10).is_empty());
        // No query: shallow entries first, folders before files at one depth.
        assert_eq!(top(&paths, "", 2), ["src", "README.md"]);
        let many: Vec<String> = (0..500).map(|i| format!("file{i}.txt")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(top(&many, "file", MAX_RESULTS).len(), MAX_RESULTS);
    }

    // Contract: the inserted text is the provider's own mention form, quoted
    // where Claude Code's `@"…"` needs it, and never an unrepresentable path.
    #[test]
    fn mentions_use_the_provider_form() {
        assert_eq!(
            mention(FileMention::AtPath, "src/a.rs").as_deref(),
            Some("@src/a.rs")
        );
        assert_eq!(
            mention(FileMention::AtPath, "My Docs/a b.md").as_deref(),
            Some("@\"My Docs/a b.md\"")
        );
        assert_eq!(mention(FileMention::AtPath, "a \"b\".md"), None);
        assert_eq!(
            mention(FileMention::PlainText, "src/a b.rs").as_deref(),
            Some("\"src/a b.rs\"")
        );
        assert_eq!(mention(FileMention::None, "src/a.rs"), None);
    }

    // Contract: a non-repository walk is bounded, skips dependency/build
    // folders and symlinks, and never lists protected locations.
    #[test]
    fn walk_skips_heavy_folders_symlinks_and_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::canonical_path::canonicalize(dir.path()).unwrap();
        for path in [
            "src/main.rs",
            "node_modules/x/index.js",
            "target/debug/out",
            "secret/key.pem",
            ".github/ci.yml",
        ] {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), "x").unwrap();
        }
        std::os::unix::fs::symlink("/etc/hosts", root.join("hosts")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("etc-link")).unwrap();
        let denied = vec![root.join("secret")];
        let (files, truncated) = walk_files(&root, &denied);
        assert!(!truncated);
        let entries = index_entries(&root, files, &denied);
        let mut paths: Vec<_> = entries
            .iter()
            .map(|e| format!("{}{}", e.path, if e.is_directory { "/" } else { "" }))
            .collect();
        paths.sort();
        assert_eq!(paths, [".github/", ".github/ci.yml", "src/", "src/main.rs"]);
        // Paths Git might report are re-checked the same way.
        let entries = index_entries(
            &root,
            vec![
                "secret/key.pem".into(),
                "../outside".into(),
                "hosts".into(),
                "gone.rs".into(),
                "src/main.rs".into(),
            ],
            &denied,
        );
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths.contains(&"src/main.rs") && paths.contains(&"src"));
    }

    // Contract: the route searches only the conversation's verified Project
    // roots as the owner's Git sees them, inserts each provider's own mention
    // (relative in the working folder, absolute in another Project folder),
    // refuses providers without mentions, bounds every input and result, and
    // follows Project access changes.
    #[tokio::test]
    async fn search_route_is_scoped_bounded_and_speaks_each_provider() {
        use wonder_store::{ProjectConversationInsert, ProjectRootInput};
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        forget_indexes();
        let workspace = tempfile::tempdir().unwrap();
        let source = crate::canonical_path::canonicalize(workspace.path())
            .unwrap()
            .join("source");
        let other = crate::canonical_path::canonicalize(workspace.path())
            .unwrap()
            .join("other");
        for (root, path) in [
            (&source, "src/main.rs"),
            (&source, "My Notes.md"),
            (&source, "ignored/secret-main.txt"),
            (&source, "untracked-main.txt"),
            (&other, "notes/plan.md"),
            (&other, "node_modules/plan/index.js"),
        ] {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), "x").unwrap();
        }
        fs::write(source.join(".gitignore"), "ignored/\n").unwrap();
        for args in [&["init", "-q"][..], &["add", "src", ".gitignore"]] {
            assert!(crate::test_git::git()
                .args(args)
                .current_dir(&source)
                .status()
                .unwrap()
                .success());
        }
        let inputs: Vec<_> = [&source, &other]
            .iter()
            .map(|path| ProjectRootInput {
                path: path.to_string_lossy().into_owned(),
                canonical_path: path.to_string_lossy().into_owned(),
            })
            .collect();
        let wonder_store::ProjectCreate::Created(project) = state
            .store
            .create_project(
                "search-project",
                "request",
                "hash",
                "Search",
                &inputs,
                0,
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("new project")
        };
        for (id, family) in [
            ("search-claude", AgentFamily::Claude),
            ("search-codex", AgentFamily::Codex),
            ("search-opencode", AgentFamily::OpenCode),
        ] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: &project.id,
                    family,
                    provider_store: "owner",
                    native_session_id: None,
                    cwd: source.to_str().unwrap(),
                    roots_revision: project.roots_revision,
                    title: "Search",
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
        }
        let search = |id: String, query: &str| {
            let state = state.clone();
            let uri = format!("/api/v1/conversations/{id}/workspace/search?{query}");
            async move {
                let (status, bytes) =
                    crate::filesystem::tests::workspace_route(&state, &uri, true).await;
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                if status == StatusCode::OK {
                    crate::tests::validate_http_contract("workspaceSearch", &value);
                }
                (status, value)
            }
        };
        let mentions = |value: &serde_json::Value| -> Vec<String> {
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["mention"].as_str().unwrap().to_owned())
                .collect()
        };

        let unauthenticated = crate::filesystem::tests::workspace_route(
            &state,
            "/api/v1/conversations/search-claude/workspace/search?q=main",
            false,
        )
        .await;
        assert_ne!(unauthenticated.0, StatusCode::OK);

        // Git's view: tracked and untracked files, ignored ones left out.
        let (status, main) = search("search-claude".into(), "q=main").await;
        assert_eq!(status, StatusCode::OK, "{main}");
        assert_eq!(
            mentions(&main),
            ["@src/main.rs", "@untracked-main.txt"],
            "{main}"
        );
        assert_eq!(main["entries"][0]["rootId"], "workspace");
        assert_eq!(main["entries"][0]["name"], "main.rs");
        let (_, spaced) = search("search-claude".into(), "q=my%20notes").await;
        assert_eq!(mentions(&spaced), ["@\"My Notes.md\""]);
        // Another Project folder (not a repository) is walked without its
        // dependencies and named by full path, which the agent can open.
        let (_, plan) = search("search-claude".into(), "q=plan").await;
        let plan_path = other.join("notes/plan.md");
        assert_eq!(
            mentions(&plan),
            [format!("@{}", plan_path.display())],
            "{plan}"
        );
        assert_eq!(plan["entries"][0]["path"], "notes/plan.md");
        // Codex input has no file item: the path is plain text.
        let (_, codex) = search("search-codex".into(), "q=main.rs").await;
        assert_eq!(mentions(&codex)[0], "src/main.rs");
        // OpenCode's native mention cannot be sent yet, so there is no list.
        assert_eq!(
            search("search-opencode".into(), "q=main").await.0,
            StatusCode::NOT_FOUND
        );
        // Bounds.
        let (_, one) = search("search-claude".into(), "q=&limit=1").await;
        assert_eq!(one["entries"].as_array().unwrap().len(), 1);
        let (status, _) = search("search-claude".into(), "q=&limit=5000").await;
        assert_eq!(status, StatusCode::OK);
        let long = "a".repeat(MAX_QUERY_BYTES + 1);
        assert_eq!(
            search("search-claude".into(), &format!("q={long}")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            search("missing".into(), "q=main").await.0,
            StatusCode::NOT_FOUND
        );
        // A new chat names its provider; the Project checks are the same.
        let draft = format!("project-files:{}:{}", project.id, project.roots[0].id);
        assert_eq!(
            search(draft.clone(), "q=main").await.0,
            StatusCode::BAD_REQUEST
        );
        let (status, drafted) = search(draft.clone(), "q=main.rs&family=claude").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mentions(&drafted)[0], "@src/main.rs");
        // Listings are reused briefly, then rebuilt to show new files.
        fs::write(source.join("src/fresh.rs"), "x").unwrap();
        let (_, stale) = search("search-claude".into(), "q=fresh").await;
        assert!(mentions(&stale).is_empty());
        forget_indexes();
        let (_, fresh) = search("search-claude".into(), "q=fresh").await;
        assert_eq!(mentions(&fresh), ["@src/fresh.rs"]);
        // A Project no longer included in Wonder cannot be searched.
        state
            .store
            .update_project_metadata(&project.id, None, Some(false), None, "later")
            .await
            .unwrap();
        assert_eq!(
            search("search-claude".into(), "q=main").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            search(draft, "q=main&family=claude").await.0,
            StatusCode::FORBIDDEN
        );
    }
}
