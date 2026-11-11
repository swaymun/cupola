//! Message search beyond what Wonder stored itself: threads and sessions
//! started on the Mac outside Wonder. Each provider's capability record says
//! how its history is reached (`ProviderCapabilities::history_search`):
//! Codex through the App Server's own index (`thread/search`, with
//! `thread/searchOccurrences` for the jump inside a thread), Claude Code
//! through a small local FTS5 cache of its JSONL transcripts. Results merge
//! with Wonder's own by recency under one opaque cursor.
//!
//! The transcript cache is owner-only (mode 0600, read through owner routes),
//! stays on this Mac and can be deleted at any time: it is rebuilt from the
//! transcripts. It indexes only words the owner and the agent wrote (prompts
//! and reply text), never tool input or output, and every bound below holds.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use wonder_store::{AgentFamily, HistorySearch, SearchCursor};

use crate::AppState;

/// Newest transcripts kept in the cache; older ones drop out of it.
const MAX_TRANSCRIPTS: usize = 2000;
/// A transcript line longer than this is skipped unread (pasted images and
/// large tool output live in such lines, not prompt or reply text).
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Text indexed per message block, per transcript and in the whole cache.
const MAX_BLOCK_TEXT: usize = 16 * 1024;
const MAX_TRANSCRIPT_TEXT: i64 = 2 * 1024 * 1024;
const MAX_INDEX_TEXT: i64 = 128 * 1024 * 1024;
/// One background refresh reads for at most this long, then resumes on the
/// next search from where it stopped.
const REFRESH_BUDGET: Duration = Duration::from_secs(20);
/// A search waits this long for an in-flight refresh before using the cache
/// as it is (and says the results may be incomplete).
const REFRESH_WAIT: Duration = Duration::from_millis(1500);
/// A complete cache younger than this is used without a refresh.
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
/// Runtime search pages read per request, their size and the request's time.
const RUNTIME_PAGES: usize = 3;
const RUNTIME_PAGE_SIZE: u32 = 50;
const SEARCH_BUDGET: Duration = Duration::from_secs(4);
const MAX_SNIPPET: usize = 600;
const MAX_TITLE: usize = 200;
/// Bump both when the cache's tables or extraction change; the cache is rebuilt.
const INDEX_VERSION: i64 = 1;
const SET_INDEX_VERSION: &str = "PRAGMA user_version=1";

/// One search result, from Wonder's store or a provider's native history.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchHit {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub snippet: Option<String>,
    pub conversation_id: Option<String>,
    pub updated_at: String,
    pub deep_link: String,
    /// The Project a native result opens in, with the reference its attach
    /// route takes (`codex:<thread>` / `claude:<session>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<AgentFamily>,
    /// The matching timeline item, when the source knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip)]
    pub sort_ms: i64,
}

/// Where a source continues after a result it returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub(crate) enum Position {
    #[serde(rename = "w")]
    Wonder(SearchCursor),
    /// A page of the runtime's own cursor and how many of its results were used.
    #[serde(rename = "r")]
    Runtime { page: Option<String>, skip: usize },
    #[serde(rename = "i")]
    Index { sort_ms: i64, rowid: i64 },
    /// A transcript in the cache, by its last change.
    #[serde(rename = "n")]
    Title { sort_ms: i64, rowid: i64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaneCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Position>,
    #[serde(default)]
    pub done: bool,
}

/// One cursor for every source; opaque to clients.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MergedCursor {
    pub wonder: LaneCursor,
    pub runtime: LaneCursor,
    pub index: LaneCursor,
    /// Claude sessions whose name matches, which message text may not.
    #[serde(default)]
    pub titles: LaneCursor,
}

/// The results one source fetched for this page, newest first in its order.
pub(crate) struct Lane {
    pub hits: VecDeque<(SearchHit, Position)>,
    /// Nothing exists beyond `hits`.
    pub exhausted: bool,
    /// The position after everything examined, including results filtered out.
    pub scanned: Option<Position>,
    pub cursor: LaneCursor,
}

impl Lane {
    fn finished(cursor: LaneCursor) -> Self {
        Self {
            hits: VecDeque::new(),
            exhausted: true,
            scanned: None,
            cursor: LaneCursor {
                done: true,
                ..cursor
            },
        }
    }
}

/// Merges sources newest first, ties going to the earlier lane, and returns
/// each lane's cursor after what was used. A source that may have more but
/// fetched nothing more ends the page early: a newer result could follow.
pub(crate) fn merge(limit: usize, lanes: &mut [Lane]) -> (Vec<SearchHit>, Vec<LaneCursor>) {
    let mut page = Vec::new();
    let mut last: Vec<Option<Position>> = vec![None; lanes.len()];
    while page.len() < limit {
        if lanes
            .iter()
            .any(|lane| !lane.cursor.done && !lane.exhausted && lane.hits.is_empty())
        {
            break;
        }
        let mut best: Option<(usize, i64)> = None;
        for (index, lane) in lanes.iter().enumerate() {
            if let Some((hit, _)) = lane.hits.front() {
                if best.is_none_or(|(_, newest)| hit.sort_ms > newest) {
                    best = Some((index, hit.sort_ms));
                }
            }
        }
        let Some((index, _)) = best else { break };
        let (hit, position) = lanes[index].hits.pop_front().expect("lane head");
        last[index] = Some(position);
        page.push(hit);
    }
    let cursors = lanes
        .iter()
        .zip(last)
        .map(|(lane, last)| {
            if lane.cursor.done {
                return lane.cursor.clone();
            }
            let after = if lane.hits.is_empty() {
                lane.scanned.clone().or(last)
            } else {
                last
            }
            .or_else(|| lane.cursor.after.clone());
            LaneCursor {
                after,
                done: lane.hits.is_empty() && lane.exhausted,
            }
        })
        .collect();
    (page, cursors)
}

/// The included Projects' folders and the native sessions bound to Wonder
/// conversations, which decide where native results open.
#[derive(Default)]
pub(crate) struct SearchScope {
    /// Folder (as listed or canonical) to Project.
    pub roots: HashMap<String, String>,
    pub bound: HashMap<(AgentFamily, String), Bound>,
}

pub(crate) struct Bound {
    pub conversation_id: String,
    /// Archived in Wonder or in an excluded Project: not a search result.
    pub hidden: bool,
}

impl SearchScope {
    pub(crate) async fn load(state: &AppState) -> Result<Self, sqlx::Error> {
        let mut scope = Self::default();
        for project in state.store.list_projects().await? {
            if project.is_included {
                for root in &project.roots {
                    scope.roots.insert(root.path.clone(), project.id.clone());
                    scope
                        .roots
                        .insert(root.canonical_path.clone(), project.id.clone());
                }
            }
            for conversation in state.store.project_conversations(&project.id).await? {
                let Some(native) = conversation.native_session_id else {
                    continue;
                };
                // A thread in an added account's home is not the Mac's own.
                if conversation.provider_store
                    != crate::providers::provider_store(state, conversation.family)
                {
                    continue;
                }
                scope.bound.insert(
                    (conversation.family, native),
                    Bound {
                        conversation_id: conversation.conversation_id,
                        hidden: conversation.wonder_archived || !project.is_included,
                    },
                );
            }
        }
        for family in AgentFamily::ALL {
            for native in state
                .store
                .deleted_project_sessions(family, crate::providers::provider_store(state, family))
                .await?
            {
                scope.bound.insert(
                    (family, native),
                    Bound {
                        conversation_id: String::new(),
                        hidden: true,
                    },
                );
            }
        }
        Ok(scope)
    }

    /// Native sessions that are not results of their own: hidden, or bound to
    /// a conversation Wonder's own results already cover for this query.
    fn excluded(&self, family: AgentFamily, covered: &BTreeSet<String>) -> Vec<String> {
        self.bound
            .iter()
            .filter(|((bound_family, _), bound)| {
                *bound_family == family
                    && (bound.hidden || covered.contains(&bound.conversation_id))
            })
            .map(|((_, native), _)| native.clone())
            .collect()
    }

    /// Where a native session opens, or `None` when it is not a result.
    fn place(
        &self,
        family: AgentFamily,
        native: &str,
        cwd: &str,
        excluded: &[String],
    ) -> Option<(String, Option<String>)> {
        let project = self.roots.get(cwd)?.clone();
        if excluded.iter().any(|id| id == native) {
            return None;
        }
        let conversation = self
            .bound
            .get(&(family, native.to_owned()))
            .map(|bound| bound.conversation_id.clone());
        Some((project, conversation))
    }
}

pub(crate) struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    pub next: Option<MergedCursor>,
    /// A source could not be read completely; results may be missing.
    pub partial: bool,
}

/// One page of merged results. A conversation-scoped search reads only
/// Wonder's store; `include_index` is false when the transcript provider's
/// runtime is not configured (its sessions could not be opened).
pub(crate) async fn search(
    state: &AppState,
    query: &str,
    conversation: Option<&str>,
    cursor: MergedCursor,
    limit: u32,
    include_index: bool,
) -> Result<SearchOutcome, sqlx::Error> {
    let deadline = Instant::now() + SEARCH_BUDGET;
    let mut cursor = cursor;
    if conversation.is_some() {
        cursor.runtime.done = true;
        cursor.index.done = true;
        cursor.titles.done = true;
    }
    let before = match &cursor.wonder.after {
        Some(Position::Wonder(position)) => Some(position.clone()),
        _ => None,
    };
    // A finished Wonder lane still decides which native sessions it covers.
    let page = state
        .store
        .search_page(
            query,
            conversation,
            before.as_ref(),
            if cursor.wonder.done { 1 } else { limit },
        )
        .await?;
    let covered = page.matched_conversations.clone();
    let wonder = if cursor.wonder.done {
        Lane::finished(cursor.wonder.clone())
    } else {
        let exhausted = page.next_cursor.is_none();
        let scanned = page.positions.last().cloned().map(Position::Wonder);
        let hits = page
            .results
            .into_iter()
            .zip(page.positions)
            .map(|(result, position)| {
                let hit = SearchHit {
                    deep_link: crate::search_result_deep_link(&result),
                    kind: result.kind,
                    id: result.id,
                    title: result.title,
                    snippet: result.snippet,
                    conversation_id: result.conversation_id,
                    updated_at: result.updated_at,
                    project_id: None,
                    reference: None,
                    family: None,
                    item_id: None,
                    sort_ms: position.sort_ms,
                };
                (hit, Position::Wonder(position))
            })
            .collect();
        Lane {
            hits,
            exhausted,
            scanned,
            cursor: cursor.wonder.clone(),
        }
    };
    let scope = if cursor.runtime.done && cursor.index.done && cursor.titles.done {
        SearchScope::default()
    } else {
        SearchScope::load(state).await?
    };
    let family_with = |kind: HistorySearch| {
        AgentFamily::ALL
            .into_iter()
            .find(|family| family.provider().capabilities.history_search == kind)
    };
    let runtime = async {
        match family_with(HistorySearch::Runtime) {
            Some(family) if !cursor.runtime.done => {
                let excluded = scope.excluded(family, &covered);
                runtime_lane(
                    state,
                    &scope,
                    family,
                    &excluded,
                    query,
                    cursor.runtime.clone(),
                    limit as usize,
                    deadline,
                )
                .await
            }
            _ => (Lane::finished(cursor.runtime.clone()), false),
        }
    };
    let index = async {
        match family_with(HistorySearch::TranscriptIndex) {
            Some(family) if include_index && !cursor.index.done => {
                let excluded = scope.excluded(family, &covered);
                index_lane(
                    state,
                    &scope,
                    family,
                    &excluded,
                    query,
                    cursor.index.clone(),
                    limit as usize,
                )
                .await
            }
            _ => (Lane::finished(cursor.index.clone()), false),
        }
    };
    let ((runtime, runtime_partial), (index, index_partial)) = tokio::join!(runtime, index);
    // After the index lane, which has refreshed the cache for this search.
    let (titles, titles_partial) = match family_with(HistorySearch::TranscriptIndex) {
        Some(family) if include_index && !cursor.titles.done => {
            let excluded = scope.excluded(family, &covered);
            title_lane(
                state,
                &scope,
                family,
                &excluded,
                query,
                cursor.titles.clone(),
                limit as usize,
            )
            .await
        }
        _ => (Lane::finished(cursor.titles.clone()), false),
    };
    let mut lanes = [wonder, runtime, index, titles];
    let (hits, cursors) = merge(limit as usize, &mut lanes);
    let [wonder, runtime, index, titles]: [LaneCursor; 4] = cursors.try_into().expect("four lanes");
    let next = MergedCursor {
        wonder,
        runtime,
        index,
        titles,
    };
    let more = !(next.wonder.done && next.runtime.done && next.index.done && next.titles.done);
    Ok(SearchOutcome {
        hits,
        next: more.then_some(next),
        partial: runtime_partial || index_partial || titles_partial,
    })
}

// ---------------------------------------------------------------------------
// Runtime search (Codex App Server)
// ---------------------------------------------------------------------------

fn unknown_method(error: &wonder_app_server::JsonRpcError) -> bool {
    error.code == -32601
}

#[allow(clippy::too_many_arguments)]
async fn runtime_lane(
    state: &AppState,
    scope: &SearchScope,
    family: AgentFamily,
    excluded: &[String],
    query: &str,
    cursor: LaneCursor,
    limit: usize,
    deadline: Instant,
) -> (Lane, bool) {
    let failed = |cursor: LaneCursor| (Lane::finished(cursor), true);
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let rpc = match tokio::time::timeout(remaining(), crate::projects::codex_rpc(state)).await {
        Ok(Ok(rpc)) => rpc,
        // Codex is not installed or cannot start: the Projects list says so;
        // there is no history here to call incomplete.
        Ok(Err(_)) => return (Lane::finished(cursor), false),
        Err(_) => return failed(cursor),
    };
    let (mut page, mut skip) = match &cursor.after {
        Some(Position::Runtime { page, skip }) => (page.clone(), *skip),
        _ => (None, 0),
    };
    let mut hits = VecDeque::new();
    let mut scanned = cursor.after.clone();
    let mut exhausted = false;
    let mut partial = false;
    for _ in 0..RUNTIME_PAGES {
        let request = rpc.request(
            "thread/search",
            json!({
                "searchTerm": query, "limit": RUNTIME_PAGE_SIZE, "cursor": page,
                "sortKey": "updated_at", "sortDirection": "desc",
                "sourceKinds": crate::projects::CODEX_SOURCE_KINDS,
            }),
        );
        let response = match tokio::time::timeout(remaining(), request).await {
            Ok(Ok(response)) => response,
            _ => {
                partial = true;
                break;
            }
        };
        if let Some(error) = &response.error {
            // An older runtime without search covers Wonder's own messages only.
            if unknown_method(error) && hits.is_empty() {
                return (Lane::finished(cursor), false);
            }
            partial = true;
            break;
        }
        let Some(data) = response
            .result
            .as_ref()
            .and_then(|result| result.get("data"))
            .and_then(Value::as_array)
            .filter(|data| data.len() <= 500)
        else {
            partial = true;
            break;
        };
        for (index, result) in data.iter().enumerate().skip(skip) {
            let position = Position::Runtime {
                page: page.clone(),
                skip: index + 1,
            };
            scanned = Some(position.clone());
            if let Some(hit) = runtime_hit(result, scope, family, excluded, query) {
                hits.push_back((hit, position));
            }
        }
        match response
            .result
            .as_ref()
            .and_then(|result| result.get("nextCursor"))
        {
            None | Some(Value::Null) => {
                exhausted = true;
                break;
            }
            Some(Value::String(next))
                if !next.is_empty() && next.len() <= 4096 && Some(next) != page.as_ref() =>
            {
                page = Some(next.clone());
                skip = 0;
                scanned = Some(Position::Runtime {
                    page: page.clone(),
                    skip: 0,
                });
            }
            _ => {
                partial = true;
                break;
            }
        }
        if hits.len() > limit {
            break;
        }
    }
    if partial && hits.is_empty() && scanned == cursor.after {
        // No progress is possible now; end this source rather than repeat it.
        return failed(cursor);
    }
    (
        Lane {
            hits,
            exhausted,
            scanned,
            cursor,
        },
        partial,
    )
}

fn runtime_hit(
    result: &Value,
    scope: &SearchScope,
    family: AgentFamily,
    excluded: &[String],
    query: &str,
) -> Option<SearchHit> {
    let thread = result.get("thread")?;
    let id = thread.get("id").and_then(Value::as_str)?;
    // Helper threads stay under their parent; ephemeral threads are not kept.
    if thread
        .get("parentThreadId")
        .and_then(Value::as_str)
        .is_some()
        || thread.get("ephemeral").and_then(Value::as_bool) == Some(true)
        || id.is_empty()
        || id.len() > 128
        || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return None;
    }
    let cwd = thread.get("cwd").and_then(Value::as_str)?;
    let (project, conversation) = scope.place(family, id, cwd, excluded)?;
    let seconds = thread
        .get("updatedAt")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let snippet = result
        .get("snippet")
        .and_then(Value::as_str)
        .map(|text| mark_terms(&clean(text, MAX_SNIPPET), query));
    Some(native_hit(
        "thread",
        id.to_owned(),
        clean(&crate::projects::thread_title(thread), MAX_TITLE),
        snippet,
        seconds.saturating_mul(1000),
        family,
        id,
        project,
        conversation,
        None,
    ))
}

#[allow(clippy::too_many_arguments)]
fn native_hit(
    kind: &str,
    id: String,
    title: String,
    snippet: Option<String>,
    sort_ms: i64,
    family: AgentFamily,
    native: &str,
    project: String,
    conversation: Option<String>,
    item: Option<String>,
) -> SearchHit {
    SearchHit {
        kind: kind.into(),
        id,
        title,
        snippet,
        deep_link: conversation
            .as_deref()
            .map(|conversation| format!("#/chats/{conversation}"))
            .unwrap_or_else(|| "#/chats".into()),
        conversation_id: conversation,
        updated_at: sort_ms.to_string(),
        project_id: Some(project),
        reference: Some(format!("{}:{native}", family.as_str())),
        family: Some(family),
        item_id: item,
        sort_ms,
    }
}

/// Native text as a snippet or title: one line of plain text without escape
/// sequences or secrets, and without text that would read as our own markers.
fn clean(text: &str, limit: usize) -> String {
    let text = text.replace("<mark>", "").replace("</mark>", "");
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::history::sanitize_ansi_and_secrets(&flat, limit)
}

/// [`clean`] for a snippet whose `<mark>` delimiters came from the index
/// (indexed text has none of its own).
fn clean_marked(text: &str, limit: usize) -> String {
    let private = text
        .replace("<mark>", "\u{E000}")
        .replace("</mark>", "\u{E001}");
    let flat = private.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::history::sanitize_ansi_and_secrets(&flat, limit)
        .replace('\u{E000}', "<mark>")
        .replace('\u{E001}', "</mark>")
}

/// Wraps each query term (ASCII case-insensitive) in `<mark>` like Wonder's
/// own snippets, for runtimes that return plain snippets.
pub(crate) fn mark_terms(text: &str, query: &str) -> String {
    let terms: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .take(8)
        .collect();
    let bytes = text.as_bytes();
    let mut marked = String::with_capacity(text.len() + 16);
    let mut index = 0;
    while index < text.len() {
        let found = terms.iter().find_map(|term| {
            let end = index + term.len();
            (end <= text.len()
                && text.is_char_boundary(end)
                && bytes[index..end].eq_ignore_ascii_case(term.as_bytes()))
            .then_some(end)
        });
        if let Some(end) = found {
            marked.push_str("<mark>");
            marked.push_str(&text[index..end]);
            marked.push_str("</mark>");
            index = end;
        } else {
            let next = text[index..].chars().next().map_or(1, char::len_utf8);
            marked.push_str(&text[index..index + next]);
            index += next;
        }
    }
    marked
}

/// One in-thread match, for opening a native result at its message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Occurrence {
    pub item_id: String,
    pub turn_id: Option<String>,
    pub snippet: String,
}

/// Matches inside one conversation's native history, oldest first. `None`
/// means the provider offers no in-thread search.
pub(crate) async fn occurrences(
    state: &AppState,
    conversation: &wonder_store::StoredProjectConversation,
    query: &str,
) -> Result<Option<Vec<Occurrence>>, String> {
    let Some(native) = conversation.native_session_id.as_deref() else {
        return Ok(Some(Vec::new()));
    };
    let family = conversation.family;
    if conversation.provider_store != crate::providers::provider_store(state, family) {
        return Ok(None);
    }
    match family.provider().capabilities.history_search {
        HistorySearch::None => Ok(None),
        HistorySearch::Runtime => {
            let rpc = crate::projects::codex_rpc(state).await?;
            let request = rpc.request(
                "thread/searchOccurrences",
                json!({"threadId": native, "searchTerm": query, "limit": 20}),
            );
            let response = tokio::time::timeout(SEARCH_BUDGET, request)
                .await
                .map_err(|_| "Search timed out.".to_owned())?
                .map_err(|e| e.to_string())?;
            if let Some(error) = response.error {
                return if unknown_method(&error) {
                    Ok(None)
                } else {
                    Err(error.message)
                };
            }
            let data = response
                .result
                .as_ref()
                .and_then(|result| result.get("data"))
                .and_then(Value::as_array)
                .ok_or("Invalid search response.")?;
            Ok(Some(
                data.iter()
                    .take(100)
                    .filter_map(|occurrence| {
                        let item = occurrence.get("itemId").and_then(Value::as_str)?;
                        let snippet = occurrence.get("snippet").and_then(Value::as_str)?;
                        let range = occurrence.get("snippetMatchRange");
                        let start = range?.get("start").and_then(Value::as_u64)? as usize;
                        let end = range?.get("end").and_then(Value::as_u64)? as usize;
                        Some(Occurrence {
                            item_id: item.to_owned(),
                            turn_id: occurrence
                                .get("turnId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            snippet: mark_utf16_range(snippet, start, end),
                        })
                    })
                    .collect(),
            ))
        }
        HistorySearch::TranscriptIndex => {
            let index = state
                .projects
                .native_search
                .index(&state.data_dir)
                .await
                .ok_or("The search cache is unavailable.")?;
            index
                .occurrences(native, query, 20)
                .await
                .map(Some)
                .map_err(|e| e.to_string())
        }
    }
}

/// Marks a UTF-16 range of a runtime snippet; an invalid range marks nothing.
fn mark_utf16_range(snippet: &str, start: usize, end: usize) -> String {
    let snippet = snippet.replace("<mark>", "").replace("</mark>", "");
    let mut units = 0;
    let (mut from, mut to) = (None, None);
    for (offset, character) in snippet.char_indices() {
        if units == start {
            from = Some(offset);
        }
        if units == end {
            to = Some(offset);
        }
        units += character.len_utf16();
    }
    if units == end {
        to = Some(snippet.len());
    }
    let marked = match (from, to) {
        (Some(from), Some(to)) if from < to => format!(
            "{}<mark>{}</mark>{}",
            &snippet[..from],
            &snippet[from..to],
            &snippet[to..]
        ),
        _ => snippet,
    };
    crate::history::sanitize_ansi_and_secrets(&marked, MAX_SNIPPET + 13)
}

// ---------------------------------------------------------------------------
// Transcript index (Claude Code)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn index_lane(
    state: &AppState,
    scope: &SearchScope,
    family: AgentFamily,
    excluded: &[String],
    query: &str,
    cursor: LaneCursor,
    limit: usize,
) -> (Lane, bool) {
    let search = &state.projects.native_search;
    let complete = search.refresh(state).await;
    let Some(index) = search.index(&state.data_dir).await else {
        return (Lane::finished(cursor), true);
    };
    let after = match &cursor.after {
        Some(Position::Index { sort_ms, rowid }) => Some((*sort_ms, *rowid)),
        _ => None,
    };
    let folders: Vec<&String> = scope.roots.keys().collect();
    let rows = match index
        .search(query, &folders, excluded, after, limit + 1)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return (Lane::finished(cursor), true),
    };
    let exhausted = rows.len() <= limit;
    let scanned = rows.last().map(|row| Position::Index {
        sort_ms: row.sort_ms,
        rowid: row.rowid,
    });
    let hits = rows
        .into_iter()
        .filter_map(|row| {
            let position = Position::Index {
                sort_ms: row.sort_ms,
                rowid: row.rowid,
            };
            let (project, conversation) =
                scope.place(family, &row.session_id, &row.cwd, excluded)?;
            let hit = native_hit(
                "native_message",
                row.item_id.clone(),
                clean(&row.title, MAX_TITLE),
                Some(clean_marked(&row.snippet, MAX_SNIPPET)),
                row.sort_ms,
                family,
                &row.session_id,
                project,
                conversation,
                Some(row.item_id),
            );
            Some((hit, position))
        })
        .collect();
    (
        Lane {
            hits,
            exhausted,
            scanned,
            cursor,
        },
        !complete,
    )
}

/// Sessions named (custom or generated title) with every query word, as
/// thread results: a renamed session's name is often in none of its messages.
async fn title_lane(
    state: &AppState,
    scope: &SearchScope,
    family: AgentFamily,
    excluded: &[String],
    query: &str,
    cursor: LaneCursor,
    limit: usize,
) -> (Lane, bool) {
    let Some(index) = state.projects.native_search.index(&state.data_dir).await else {
        return (Lane::finished(cursor), true);
    };
    let after = match &cursor.after {
        Some(Position::Title { sort_ms, rowid }) => Some((*sort_ms, *rowid)),
        _ => None,
    };
    let folders: Vec<&String> = scope.roots.keys().collect();
    let rows = match index
        .search_titles(query, &folders, excluded, after, limit + 1)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return (Lane::finished(cursor), true),
    };
    let exhausted = rows.len() <= limit;
    let scanned = rows.last().map(|row| Position::Title {
        sort_ms: row.sort_ms,
        rowid: row.rowid,
    });
    let hits = rows
        .into_iter()
        .filter_map(|row| {
            let position = Position::Title {
                sort_ms: row.sort_ms,
                rowid: row.rowid,
            };
            let (project, conversation) =
                scope.place(family, &row.session_id, &row.cwd, excluded)?;
            let hit = native_hit(
                "thread",
                row.session_id.clone(),
                clean(&row.title, MAX_TITLE),
                None,
                row.sort_ms,
                family,
                &row.session_id,
                project,
                conversation,
                None,
            );
            Some((hit, position))
        })
        .collect();
    (
        Lane {
            hits,
            exhausted,
            scanned,
            cursor,
        },
        false,
    )
}

/// Lowercased words, as the message index splits them.
fn title_words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
}

/// The transcript cache and its refresh, owned by the Projects runtime.
pub struct NativeSearch {
    transcripts: PathBuf,
    index: tokio::sync::OnceCell<Option<TranscriptIndex>>,
    refreshing: Arc<tokio::sync::Mutex<()>>,
    refreshed: std::sync::Mutex<Option<(Instant, bool)>>,
}

impl NativeSearch {
    /// `transcripts` is Claude Code's `projects` folder.
    pub fn new(transcripts: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            transcripts,
            index: tokio::sync::OnceCell::new(),
            refreshing: Arc::new(tokio::sync::Mutex::new(())),
            refreshed: std::sync::Mutex::new(None),
        })
    }

    pub(crate) async fn index(&self, data_dir: &Path) -> Option<TranscriptIndex> {
        self.index
            .get_or_init(|| async {
                TranscriptIndex::open(&data_dir.join("native-search.sqlite3"))
                    .await
                    .ok()
            })
            .await
            .clone()
    }

    /// Hold off an in-flight refresh while clearing derived copies. Source
    /// transcripts belong to the provider and are never edited by Delete.
    pub(crate) async fn purge_deleted(&self, state: &AppState) -> Result<(), sqlx::Error> {
        let _refresh = self.refreshing.lock().await;
        // No derived copy exists yet. A future refresh reads tombstones under
        // this same gate before importing, including a racing first open.
        if !state.data_dir.join("native-search.sqlite3").exists() {
            return Ok(());
        }
        let Some(family) = AgentFamily::ALL.into_iter().find(|family| {
            family.provider().capabilities.history_search == HistorySearch::TranscriptIndex
        }) else {
            return Ok(());
        };
        let deleted = state
            .store
            .deleted_project_sessions(family, crate::providers::provider_store(state, family))
            .await?;
        if deleted.is_empty() {
            return Ok(());
        }
        let index = self
            .index(&state.data_dir)
            .await
            .ok_or(sqlx::Error::PoolClosed)?;
        let mut tx = index.pool.begin().await?;
        for native in deleted {
            sqlx::query("DELETE FROM docs WHERE session_id=?")
                .bind(&native)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM files WHERE session_id=?")
                .bind(&native)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    /// Starts a refresh when the cache is stale and waits briefly for one in
    /// flight. True when the cache covered every transcript when it returned.
    pub(crate) async fn refresh(self: &Arc<Self>, state: &AppState) -> bool {
        let Some(family) = AgentFamily::ALL.into_iter().find(|family| {
            family.provider().capabilities.history_search == HistorySearch::TranscriptIndex
        }) else {
            return false;
        };
        let Some(index) = self.index(&state.data_dir).await else {
            return false;
        };
        let stale = self
            .refreshed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|(at, complete)| !complete || at.elapsed() >= REFRESH_INTERVAL);
        if stale {
            if let Ok(guard) = self.refreshing.clone().try_lock_owned() {
                let search = self.clone();
                let store = state.store.clone();
                let provider_store = crate::providers::provider_store(state, family).to_owned();
                tokio::spawn(async move {
                    let _guard = guard;
                    // Read suppression under the same gate as deletion's purge:
                    // an older search must never reimport a deleted transcript.
                    let complete = match store
                        .deleted_project_sessions(family, &provider_store)
                        .await
                    {
                        Ok(deleted) => index
                            .refresh_excluding(
                                &search.transcripts,
                                Instant::now() + REFRESH_BUDGET,
                                &deleted.into_iter().collect(),
                            )
                            .await
                            .unwrap_or(false),
                        Err(_) => false,
                    };
                    *search.refreshed.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((Instant::now(), complete));
                });
            }
        }
        match tokio::time::timeout(REFRESH_WAIT, self.refreshing.lock()).await {
            Ok(_) => self
                .refreshed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|(_, complete)| complete),
            Err(_) => false,
        }
    }
}

#[derive(Clone)]
pub(crate) struct TranscriptIndex {
    pool: SqlitePool,
}

pub(crate) struct IndexRow {
    pub rowid: i64,
    pub sort_ms: i64,
    pub item_id: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub snippet: String,
}

/// Where reading a transcript resumes, with the turn and reply block it was
/// in, so appended lines get the same item IDs a full read gives them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TranscriptState {
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub title_rank: i64,
    pub size: i64,
    pub mtime_ms: i64,
    pub offset: i64,
    pub turn_id: Option<String>,
    pub message_id: Option<String>,
    pub block_index: i64,
    pub text_bytes: i64,
}

#[derive(Debug, PartialEq)]
pub(crate) struct IndexedText {
    pub item_id: String,
    pub sort_ms: i64,
    pub body: String,
}

impl TranscriptIndex {
    pub(crate) async fn open(path: &Path) -> Result<Self, sqlx::Error> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.as_os_str().to_owned();
                file.push(suffix);
                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
            }
        }
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await?;
        if version != INDEX_VERSION {
            for statement in [
                "DROP TABLE IF EXISTS docs",
                "DROP TABLE IF EXISTS files",
                "CREATE TABLE files(path TEXT PRIMARY KEY, session_id TEXT NOT NULL, cwd TEXT, title TEXT, title_rank INTEGER NOT NULL DEFAULT 0, size INTEGER NOT NULL, mtime_ms INTEGER NOT NULL, offset INTEGER NOT NULL, turn_id TEXT, message_id TEXT, block_index INTEGER NOT NULL DEFAULT 0, text_bytes INTEGER NOT NULL DEFAULT 0)",
                "CREATE VIRTUAL TABLE docs USING fts5(body, path UNINDEXED, session_id UNINDEXED, item_id UNINDEXED, sort_ms UNINDEXED, tokenize='unicode61 remove_diacritics 2')",
            ] {
                sqlx::query(statement).execute(&pool).await?;
            }
            sqlx::query(SET_INDEX_VERSION).execute(&pool).await?;
        }
        Ok(Self { pool })
    }

    /// Brings the cache up to date with the transcripts, newest first, until
    /// `deadline`. True when every transcript is covered.
    #[cfg(test)]
    pub(crate) async fn refresh(&self, dir: &Path, deadline: Instant) -> Result<bool, String> {
        self.refresh_excluding(dir, deadline, &BTreeSet::new())
            .await
    }

    async fn refresh_excluding(
        &self,
        dir: &Path,
        deadline: Instant,
        deleted: &BTreeSet<String>,
    ) -> Result<bool, String> {
        let dir = dir.to_owned();
        let (listing, truncated) = tokio::task::spawn_blocking(move || list_transcripts(&dir))
            .await
            .map_err(|e| e.to_string())?;
        let known: HashMap<String, TranscriptState> = sqlx::query("SELECT * FROM files")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(|row| (row.get::<String, _>("path"), state_from_row(row)))
            .collect();
        let listed: BTreeSet<&str> = listing.iter().map(|(path, ..)| path.as_str()).collect();
        for path in known.keys().filter(|path| !listed.contains(path.as_str())) {
            self.forget(path).await.map_err(|e| e.to_string())?;
        }
        let mut total: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(text_bytes),0) FROM files")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut complete = !truncated;
        for (path, session, size, mtime_ms) in listing {
            if deleted.contains(&session) {
                self.forget(&path).await.map_err(|e| e.to_string())?;
                continue;
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let previous = known.get(&path);
            if previous.is_some_and(|state| state.size == size && state.mtime_ms == mtime_ms) {
                continue;
            }
            // Transcripts only grow. Anything else is read again from the start.
            let resume = previous
                .filter(|state| {
                    size >= state.offset && size >= state.size && mtime_ms >= state.mtime_ms
                })
                .cloned();
            if previous.is_some() && resume.is_none() {
                total -= previous.map_or(0, |state| state.text_bytes);
                self.forget(&path).await.map_err(|e| e.to_string())?;
            }
            if resume.is_none() && total >= MAX_INDEX_TEXT {
                complete = false;
                continue;
            }
            let start = resume.unwrap_or_else(|| TranscriptState {
                session_id: session.clone(),
                ..Default::default()
            });
            let before = start.text_bytes;
            let file = PathBuf::from(&path);
            let parsed =
                tokio::task::spawn_blocking(move || read_transcript(&file, start, size, mtime_ms))
                    .await
                    .map_err(|e| e.to_string())?;
            let Ok((state, texts)) = parsed else {
                complete = false;
                continue;
            };
            total += state.text_bytes - before;
            self.save(&path, &state, &texts)
                .await
                .map_err(|e| e.to_string())?;
            tokio::task::yield_now().await;
        }
        Ok(complete)
    }

    async fn forget(&self, path: &str) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM docs WHERE path=?")
            .bind(path)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM files WHERE path=?")
            .bind(path)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await
    }

    async fn save(
        &self,
        path: &str,
        state: &TranscriptState,
        texts: &[IndexedText],
    ) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        for text in texts {
            sqlx::query("INSERT INTO docs(body,path,session_id,item_id,sort_ms) VALUES(?,?,?,?,?)")
                .bind(&text.body)
                .bind(path)
                .bind(&state.session_id)
                .bind(&text.item_id)
                .bind(text.sort_ms)
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query("INSERT INTO files(path,session_id,cwd,title,title_rank,size,mtime_ms,offset,turn_id,message_id,block_index,text_bytes) VALUES(?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(path) DO UPDATE SET cwd=excluded.cwd,title=excluded.title,title_rank=excluded.title_rank,size=excluded.size,mtime_ms=excluded.mtime_ms,offset=excluded.offset,turn_id=excluded.turn_id,message_id=excluded.message_id,block_index=excluded.block_index,text_bytes=excluded.text_bytes")
            .bind(path)
            .bind(&state.session_id)
            .bind(&state.cwd)
            .bind(&state.title)
            .bind(state.title_rank)
            .bind(state.size)
            .bind(state.mtime_ms)
            .bind(state.offset)
            .bind(&state.turn_id)
            .bind(&state.message_id)
            .bind(state.block_index)
            .bind(state.text_bytes)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await
    }

    /// Matches in the given folders, newest first, after `after`.
    pub(crate) async fn search(
        &self,
        query: &str,
        folders: &[&String],
        excluded: &[String],
        after: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<IndexRow>, sqlx::Error> {
        let matching = wonder_store::search_match_query(query);
        if matching.is_empty() || folders.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(r#"
            SELECT docs.rowid AS rid, CAST(docs.sort_ms AS INTEGER) AS ms, docs.item_id, docs.session_id,
                files.cwd, COALESCE(files.title, 'Claude session') AS title,
                snippet(docs,0,'<mark>','</mark>','…',16) AS snippet
            FROM docs JOIN files ON files.path=docs.path
            WHERE docs MATCH ?1 AND files.cwd IN (SELECT value FROM json_each(?2))
                AND docs.session_id NOT IN (SELECT value FROM json_each(?3))
                AND (?4 IS NULL OR (CAST(docs.sort_ms AS INTEGER), docs.rowid) < (?4, ?5))
            ORDER BY ms DESC, rid DESC LIMIT ?6
        "#)
        .bind(matching)
        .bind(serde_json::to_string(folders).unwrap_or_default())
        .bind(serde_json::to_string(excluded).unwrap_or_default())
        .bind(after.map(|(ms, _)| ms))
        .bind(after.map(|(_, rowid)| rowid))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| IndexRow {
                rowid: row.get("rid"),
                sort_ms: row.get("ms"),
                item_id: row.get("item_id"),
                session_id: row.get("session_id"),
                cwd: row.get("cwd"),
                title: row.get("title"),
                snippet: row.get("snippet"),
            })
            .collect())
    }

    /// Sessions in the given folders whose custom or generated title has a
    /// word starting with each query word (as the message index matches),
    /// most recently changed first, after `after`.
    pub(crate) async fn search_titles(
        &self,
        query: &str,
        folders: &[&String],
        excluded: &[String],
        after: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<IndexRow>, sqlx::Error> {
        let wanted: Vec<String> = title_words(query).take(8).collect();
        if wanted.is_empty() || folders.is_empty() {
            return Ok(Vec::new());
        }
        // Titles are bounded (MAX_TRANSCRIPTS rows of MAX_TITLE characters),
        // so words are matched here rather than with SQLite's ASCII lower().
        let rows = sqlx::query(
            r#"
            SELECT rowid AS rid, mtime_ms AS ms, session_id, cwd, title FROM files
            WHERE title_rank >= 2 AND title IS NOT NULL
                AND cwd IN (SELECT value FROM json_each(?1))
                AND session_id NOT IN (SELECT value FROM json_each(?2))
                AND (?3 IS NULL OR (mtime_ms, rowid) < (?3, ?4))
            ORDER BY ms DESC, rid DESC
        "#,
        )
        .bind(serde_json::to_string(folders).unwrap_or_default())
        .bind(serde_json::to_string(excluded).unwrap_or_default())
        .bind(after.map(|(ms, _)| ms))
        .bind(after.map(|(_, rowid)| rowid))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter(|row| {
                let title: Vec<String> = title_words(&row.get::<String, _>("title")).collect();
                wanted
                    .iter()
                    .all(|word| title.iter().any(|have| have.starts_with(word.as_str())))
            })
            .take(limit)
            .map(|row| IndexRow {
                rowid: row.get("rid"),
                sort_ms: row.get("ms"),
                item_id: String::new(),
                session_id: row.get("session_id"),
                cwd: row.get("cwd"),
                title: row.get("title"),
                snippet: String::new(),
            })
            .collect())
    }

    async fn occurrences(
        &self,
        session: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Occurrence>, sqlx::Error> {
        let matching = wonder_store::search_match_query(query);
        if matching.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query("SELECT item_id, snippet(docs,0,'<mark>','</mark>','…',16) AS snippet FROM docs WHERE docs MATCH ? AND session_id=? ORDER BY CAST(sort_ms AS INTEGER), rowid LIMIT ?")
            .bind(matching)
            .bind(session)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let item: String = row.get("item_id");
                Occurrence {
                    // Reply items are `<turn>:<message>:<block>`; a prompt is its turn.
                    turn_id: item.split(':').next().map(str::to_owned),
                    item_id: item,
                    snippet: clean_marked(&row.get::<String, _>("snippet"), MAX_SNIPPET),
                }
            })
            .collect())
    }
}

fn state_from_row(row: &sqlx::sqlite::SqliteRow) -> TranscriptState {
    TranscriptState {
        session_id: row.get("session_id"),
        cwd: row.get("cwd"),
        title: row.get("title"),
        title_rank: row.get("title_rank"),
        size: row.get("size"),
        mtime_ms: row.get("mtime_ms"),
        offset: row.get("offset"),
        turn_id: row.get("turn_id"),
        message_id: row.get("message_id"),
        block_index: row.get("block_index"),
        text_bytes: row.get("text_bytes"),
    }
}

fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// `<projects>/<folder>/<session>.jsonl`, newest first and at most
/// [`MAX_TRANSCRIPTS`]; subagent transcripts in deeper folders are not read.
fn list_transcripts(dir: &Path) -> (Vec<(String, String, i64, i64)>, bool) {
    let mut found = Vec::new();
    for folder in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if !folder.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for file in std::fs::read_dir(folder.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = file.path();
            let Some(session) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".jsonl"))
                .filter(|session| valid_session(session))
                .map(str::to_owned)
            else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let mtime_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_millis() as i64);
            found.push((
                path.to_string_lossy().into_owned(),
                session,
                metadata.len() as i64,
                mtime_ms,
            ));
        }
    }
    found.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    let truncated = found.len() > MAX_TRANSCRIPTS;
    found.truncate(MAX_TRANSCRIPTS);
    (found, truncated)
}

/// Reads whole lines from `state.offset` to `size`; a partly written last
/// line is left for the next refresh.
pub(crate) fn read_transcript(
    path: &Path,
    mut state: TranscriptState,
    size: i64,
    mtime_ms: i64,
) -> std::io::Result<(TranscriptState, Vec<IndexedText>)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(state.offset as u64))?;
    let mut reader = std::io::BufReader::with_capacity(
        64 * 1024,
        file.take((size - state.offset).max(0) as u64),
    );
    let mut texts = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut consumed = 0usize;
        let mut oversized = false;
        let mut ended = false;
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                break;
            }
            let (chunk, newline) = match buffer.iter().position(|byte| *byte == b'\n') {
                Some(at) => (at, true),
                None => (buffer.len(), false),
            };
            if !oversized && line.len() + chunk <= MAX_LINE_BYTES {
                line.extend_from_slice(&buffer[..chunk]);
            } else {
                oversized = true;
                line.clear();
            }
            let used = chunk + usize::from(newline);
            reader.consume(used);
            consumed += used;
            if newline {
                ended = true;
                break;
            }
        }
        if !ended {
            break;
        }
        state.offset += consumed as i64;
        if !oversized {
            index_line(&line, &mut state, &mut texts, mtime_ms);
        }
    }
    state.size = size;
    state.mtime_ms = mtime_ms;
    Ok((state, texts))
}

fn index_line(
    line: &[u8],
    state: &mut TranscriptState,
    texts: &mut Vec<IndexedText>,
    fallback_ms: i64,
) {
    let relevant = [
        &b"\"type\":\"user\""[..],
        b"\"type\":\"assistant\"",
        b"\"type\":\"custom-title\"",
        b"\"type\":\"ai-title\"",
    ];
    if !relevant
        .iter()
        .any(|needle| line.windows(needle.len()).any(|window| window == *needle))
    {
        return;
    }
    let Ok(entry) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let text_field = |key: &str| entry.get(key).and_then(Value::as_str);
    match text_field("type") {
        Some("custom-title") => {
            if let Some(title) = text_field("customTitle").filter(|t| !t.trim().is_empty()) {
                state.title = Some(title.chars().take(MAX_TITLE).collect());
                state.title_rank = 3;
            }
            return;
        }
        Some("ai-title") => {
            if state.title_rank <= 2 {
                if let Some(title) = text_field("aiTitle").filter(|t| !t.trim().is_empty()) {
                    state.title = Some(title.chars().take(MAX_TITLE).collect());
                    state.title_rank = 2;
                }
            }
            return;
        }
        Some("user" | "assistant") => {}
        _ => return,
    }
    if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if state.cwd.is_none() {
        state.cwd = text_field("cwd").map(str::to_owned);
    }
    let sort_ms = text_field("timestamp")
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map_or(fallback_ms, |at| at.timestamp_millis());
    let content = entry.pointer("/message/content");
    let mut push = |state: &mut TranscriptState, item_id: String, text: &str| {
        let mut end = text.len().min(MAX_BLOCK_TEXT);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let body = text[..end].replace("<mark>", "").replace("</mark>", "");
        let body = body.trim();
        if body.is_empty() || state.text_bytes + body.len() as i64 > MAX_TRANSCRIPT_TEXT {
            return;
        }
        state.text_bytes += body.len() as i64;
        texts.push(IndexedText {
            item_id,
            sort_ms,
            body: body.to_owned(),
        });
    };
    if text_field("type") == Some("user") {
        // A turn opens at a prompt the owner wrote (see claude-runtime's
        // project-history.mjs `opensTurn`): not a tool result, harness note,
        // background notice, interruption or command output.
        let blocks = content.and_then(Value::as_array);
        if blocks.is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        }) {
            return;
        }
        let meta = ["isMeta", "is_meta", "isCompactSummary"]
            .iter()
            .any(|key| entry.get(*key).and_then(Value::as_bool) == Some(true));
        let synthetic = entry
            .pointer("/origin/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "human");
        let raw = match content {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return,
        };
        let text = strip_harness(&raw);
        let trimmed = text.trim_start();
        let command_output = [
            "<bash-stdout>",
            "<bash-stderr>",
            "<local-command-stdout>",
            "<local-command-stderr>",
        ]
        .iter()
        .any(|tag| trimmed.starts_with(tag));
        if meta
            || synthetic
            || trimmed.is_empty()
            || trimmed.starts_with("<task-notification>")
            || trimmed.starts_with("[Request interrupted by user")
            || command_output
        {
            return;
        }
        let Some(uuid) = text_field("uuid").filter(|id| !id.is_empty() && id.len() <= 200) else {
            return;
        };
        state.turn_id = Some(uuid.to_owned());
        if state.title_rank < 1 {
            let first = trimmed.lines().next().unwrap_or_default();
            state.title = Some(first.chars().take(120).collect());
            state.title_rank = 1;
        }
        push(state, uuid.to_owned(), &text);
        return;
    }
    // A reply block's item is `<turn>:<message>:<block index>`, the index
    // counting every block of that message across its transcript lines.
    let Some(turn) = state.turn_id.clone() else {
        return;
    };
    let Some(blocks) = content.and_then(Value::as_array) else {
        return;
    };
    let Some(message) = entry
        .pointer("/message/id")
        .and_then(Value::as_str)
        .or_else(|| text_field("uuid"))
        .map(str::to_owned)
    else {
        return;
    };
    for block in blocks {
        let index = if state.message_id.as_deref() == Some(message.as_str()) {
            state.block_index
        } else {
            0
        };
        state.message_id = Some(message.clone());
        state.block_index = index + 1;
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                push(state, format!("{turn}:{message}:{index}"), text);
            }
        }
    }
}

/// Removes whole harness blocks Claude Code adds to a prompt (reminders,
/// messages from other sessions); the owner did not write them.
fn strip_harness(text: &str) -> String {
    const TAGS: [&str; 6] = [
        "system-reminder",
        "cross-session-message",
        "side-sessions-event",
        "ci-monitor-event",
        "session-mentions",
        "local-command-caveat",
    ];
    let mut text = text.to_owned();
    for tag in TAGS {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        while let Some(start) = text.find(&open) {
            let Some(end) = text[start..].find(&close) else {
                break;
            };
            text.replace_range(start..start + end + close.len(), "");
        }
    }
    text
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn hit(id: &str, sort_ms: i64) -> SearchHit {
        SearchHit {
            kind: "fixture".into(),
            id: id.into(),
            title: String::new(),
            snippet: None,
            conversation_id: None,
            updated_at: sort_ms.to_string(),
            deep_link: "#/chats".into(),
            project_id: None,
            reference: None,
            family: None,
            item_id: None,
            sort_ms,
        }
    }

    /// A keyset source like Wonder's store and the transcript index.
    fn keyset(data: &[(i64, &str)], cursor: &LaneCursor, limit: usize, index: bool) -> Lane {
        if cursor.done {
            return Lane::finished(cursor.clone());
        }
        let position = |ms: i64, id: &str| {
            if index {
                Position::Index {
                    sort_ms: ms,
                    rowid: id[1..].parse().unwrap(),
                }
            } else {
                Position::Wonder(SearchCursor {
                    sort_ms: ms,
                    kind: "fixture".into(),
                    id: id.into(),
                })
            }
        };
        let rest: Vec<_> = data
            .iter()
            .filter(|(ms, id)| match &cursor.after {
                None => true,
                Some(after) => {
                    position(*ms, id) != *after && {
                        let (after_ms, after_id) = match after {
                            Position::Wonder(c) => (c.sort_ms, c.id.clone()),
                            Position::Index { sort_ms, rowid } => {
                                (*sort_ms, format!("{}{rowid}", &id[..1]))
                            }
                            Position::Runtime { .. } | Position::Title { .. } => unreachable!(),
                        };
                        (*ms, id.to_string()) < (after_ms, after_id)
                    }
                }
            })
            .collect();
        let hits: VecDeque<_> = rest
            .iter()
            .take(limit + 1)
            .map(|(ms, id)| (hit(id, *ms), position(*ms, id)))
            .collect();
        Lane {
            exhausted: rest.len() <= limit + 1,
            scanned: hits.back().map(|(_, p)| p.clone()),
            hits,
            cursor: cursor.clone(),
        }
    }

    /// A runtime that pages by its own cursor, reads one page per request and
    /// returns results the host filters out.
    fn runtime(data: &[(i64, &str, bool)], cursor: &LaneCursor) -> Lane {
        if cursor.done {
            return Lane::finished(cursor.clone());
        }
        let (page, skip) = match &cursor.after {
            Some(Position::Runtime { page, skip }) => (page.clone(), *skip),
            _ => (None, 0),
        };
        let start: usize = page.as_deref().map_or(0, |p| p.parse().unwrap());
        let rows = &data[start..(start + 3).min(data.len())];
        let mut hits = VecDeque::new();
        let mut scanned = cursor.after.clone();
        for (index, (ms, id, kept)) in rows.iter().enumerate().skip(skip) {
            let position = Position::Runtime {
                page: page.clone(),
                skip: index + 1,
            };
            scanned = Some(position.clone());
            if *kept {
                hits.push_back((hit(id, *ms), position));
            }
        }
        let exhausted = start + 3 >= data.len();
        if !exhausted {
            scanned = Some(Position::Runtime {
                page: Some((start + 3).to_string()),
                skip: 0,
            });
        }
        Lane {
            hits,
            exhausted,
            scanned,
            cursor: cursor.clone(),
        }
    }

    #[test]
    fn merged_pages_are_newest_first_without_gaps_or_repeats() {
        let wonder = [(90, "w9"), (70, "w7"), (70, "w6"), (40, "w4"), (10, "w1")];
        let index = [(95, "i5"), (70, "i4"), (50, "i3"), (20, "i2")];
        // Rows the host filters (helpers, other folders) still advance it.
        let native = [
            (99, "r9", false),
            (80, "r8", true),
            (75, "r7", false),
            (74, "r6", false),
            (73, "r5", false),
            (72, "r4", false),
            (60, "r3", true),
            (30, "r2", false),
            (5, "r1", true),
        ];
        for limit in [1, 2, 3, 10] {
            let mut cursor = MergedCursor::default();
            let mut seen = Vec::new();
            for _ in 0..40 {
                let mut lanes = [
                    keyset(&wonder, &cursor.wonder, limit, false),
                    runtime(&native, &cursor.runtime),
                    keyset(&index, &cursor.index, limit, true),
                ];
                let (page, cursors) = merge(limit, &mut lanes);
                assert!(page.len() <= limit);
                seen.extend(page.into_iter().map(|hit| (hit.sort_ms, hit.id)));
                let [w, r, i]: [LaneCursor; 3] = cursors.try_into().unwrap();
                cursor = MergedCursor {
                    wonder: w,
                    runtime: r,
                    index: i,
                    ..MergedCursor::default()
                };
                if cursor.wonder.done && cursor.runtime.done && cursor.index.done {
                    break;
                }
            }
            assert!(cursor.wonder.done && cursor.runtime.done && cursor.index.done);
            let ids: Vec<&str> = seen.iter().map(|(_, id)| id.as_str()).collect();
            assert_eq!(
                ids,
                ["i5", "w9", "r8", "w7", "w6", "i4", "r3", "i3", "w4", "i2", "w1", "r1"],
                "limit {limit}: newest first, ties to the earlier source"
            );
        }
    }

    #[test]
    fn a_source_with_unread_results_ends_the_page_before_older_results() {
        let wonder = [(10, "w1")];
        let native = [
            (99, "r9", false),
            (98, "r8", false),
            (97, "r7", false),
            (50, "r5", true),
        ];
        let mut lanes = [
            keyset(&wonder, &LaneCursor::default(), 5, false),
            runtime(&native, &LaneCursor::default()),
            Lane::finished(LaneCursor::default()),
        ];
        let (page, cursors) = merge(5, &mut lanes);
        // The runtime's next page could hold something newer than `w1`.
        assert!(page.is_empty());
        assert_eq!(
            cursors[1].after,
            Some(Position::Runtime {
                page: Some("3".into()),
                skip: 0
            }),
            "filtered rows are passed over, so the next request makes progress"
        );
        assert!(!cursors[0].done && cursors[0].after.is_none());
    }

    #[test]
    fn runtime_snippets_mark_terms_like_wonder_snippets() {
        assert_eq!(
            mark_terms("Fix the Search page; search é", "search"),
            "Fix the <mark>Search</mark> page; <mark>search</mark> é"
        );
        assert_eq!(mark_terms("naïve café", "café"), "naïve <mark>café</mark>");
        assert_eq!(clean("a <mark>b</mark>\n c", 100), "a b c");
        // UTF-16 offsets: the emoji is two units.
        assert_eq!(
            mark_utf16_range("😀 find me", 3, 7),
            "😀 <mark>find</mark> me"
        );
        assert_eq!(mark_utf16_range("short", 2, 99), "short");
    }

    fn line(value: Value) -> String {
        format!("{value}\n")
    }

    #[tokio::test]
    async fn title_search_matches_every_word_of_a_long_name() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects/-work-app");
        std::fs::create_dir_all(&folder).unwrap();
        let session = "0a1b2c3d-0000-4000-8000-00000000000b";
        let transcript = line(json!({"type":"user","uuid":"p1","cwd":"/work/app",
            "sessionId":session,"timestamp":"2026-10-01T10:00:00.000Z",
            "message":{"role":"user","content":"Start the report"}}))
            + &line(json!({"type":"custom-title","sessionId":session,
                "customTitle":"Review the project plan and prepare the quarterly walrus report"}));
        std::fs::write(folder.join(format!("{session}.jsonl")), transcript).unwrap();
        let index = TranscriptIndex::open(&dir.path().join("cache.sqlite3"))
            .await
            .unwrap();
        let far = Instant::now() + Duration::from_secs(30);
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        let folders = ["/work/app".to_owned()];
        let folders: Vec<&String> = folders.iter().collect();
        // The ninth and tenth words of the name still match.
        let named = index
            .search_titles("walrus report", &folders, &[], None, 10)
            .await
            .unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].session_id, session);
    }

    #[tokio::test]
    async fn transcript_index_reads_appended_lines_once_and_skips_tool_text() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects/-work-app");
        std::fs::create_dir_all(&folder).unwrap();
        let session = "0a1b2c3d-0000-4000-8000-00000000000a";
        let path = folder.join(format!("{session}.jsonl"));
        let user = |uuid: &str, content: Value| {
            line(
                json!({"type":"user","uuid":uuid,"cwd":"/work/app","sessionId":session,
                "timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":content}}),
            )
        };
        let assistant = |uuid: &str, message: &str, content: Value| {
            line(json!({"type":"assistant","uuid":uuid,"cwd":"/work/app",
                "timestamp":"2026-10-01T10:00:01.000Z","message":{"id":message,"content":content}}))
        };
        let mut transcript = String::new();
        transcript += &user(
            "p1",
            json!(
                "Please rename the walrus module <system-reminder>secret harness</system-reminder>"
            ),
        );
        transcript += &assistant(
            "a1",
            "m1",
            json!([{"type":"thinking","thinking":"walrus thoughts"}]),
        );
        transcript += &assistant(
            "a2",
            "m1",
            json!([{"type":"text","text":"Renamed the walrus module."}]),
        );
        transcript += &assistant(
            "a3",
            "m1",
            json!([{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"grep walrus"}}]),
        );
        transcript += &user(
            "tr",
            json!([{"type":"tool_result","tool_use_id":"t1","content":"walrus.rs"}]),
        );
        transcript += &line(
            json!({"type":"user","uuid":"meta","isMeta":true,"message":{"content":"walrus caveat"}}),
        );
        transcript += &line(
            json!({"type":"user","uuid":"side","isSidechain":true,"message":{"content":"walrus subagent"}}),
        );
        transcript +=
            &line(json!({"type":"custom-title","customTitle":"Walrus rename","sessionId":session}));
        transcript += &format!(
            "{{\"type\":\"user\",\"pad\":\"{}\"}}\n",
            "walrus".repeat(MAX_LINE_BYTES / 6 + 1)
        );
        std::fs::write(&path, &transcript).unwrap();

        let index = TranscriptIndex::open(&dir.path().join("cache.sqlite3"))
            .await
            .unwrap();
        let mode = std::fs::metadata(dir.path().join("cache.sqlite3"))
            .unwrap()
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let far = Instant::now() + Duration::from_secs(30);
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        let folders = ["/work/app".to_owned()];
        let folders: Vec<&String> = folders.iter().collect();
        let found = index
            .search("walrus", &folders, &[], None, 10)
            .await
            .unwrap();
        let mut items: Vec<_> = found.iter().map(|row| row.item_id.as_str()).collect();
        items.sort();
        // The prompt is its turn; the reply text is the second block of m1.
        assert_eq!(items, ["p1", "p1:m1:1"]);
        assert!(found
            .iter()
            .all(|row| row.title == "Walrus rename" && row.cwd == "/work/app"));
        assert!(index
            .search("harness", &folders, &[], None, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(index
            .search("walrus", &folders, &[session.to_owned()], None, 10)
            .await
            .unwrap()
            .is_empty());
        // A renamed session is found by every word of its name.
        let named = index
            .search_titles("REN walr", &folders, &[], None, 10)
            .await
            .unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].session_id, session);
        assert_eq!(named[0].title, "Walrus rename");
        for (query, excluded) in [
            ("walrus module", vec![]),
            ("walrus", vec![session.to_owned()]),
        ] {
            assert!(index
                .search_titles(query, &folders, &excluded, None, 10)
                .await
                .unwrap()
                .is_empty());
        }
        let elsewhere = ["/elsewhere".to_owned()];
        assert!(index
            .search_titles("walrus", &[&elsewhere[0]], &[], None, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(index
            .search("walrus", &[&elsewhere[0]], &[], None, 10)
            .await
            .unwrap()
            .is_empty());

        // Appending reads only the new lines; a partly written line waits.
        let reply = assistant(
            "a4",
            "m2",
            json!([{"type":"text","text":"Walrus follow-up"}]),
        );
        let (complete, partial) = reply.split_at(reply.len() / 2);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, user("p2", json!("Now the narwhal")).as_bytes())
            .unwrap();
        std::io::Write::write_all(&mut file, complete.as_bytes()).unwrap();
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        assert_eq!(
            index
                .search("narwhal", &folders, &[], None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            index
                .search("follow", &folders, &[], None, 10)
                .await
                .unwrap()
                .len(),
            0
        );
        std::io::Write::write_all(&mut file, partial.as_bytes()).unwrap();
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        let follow = index
            .search("follow", &folders, &[], None, 10)
            .await
            .unwrap();
        assert_eq!(
            follow[0].item_id, "p2:m2:0",
            "the new turn continues across reads"
        );
        assert_eq!(
            index
                .search("walrus", &folders, &[], None, 10)
                .await
                .unwrap()
                .len(),
            3
        );
        let occurrences = index.occurrences(session, "walrus", 10).await.unwrap();
        assert_eq!(occurrences[0].turn_id.as_deref(), Some("p1"));

        // A rewritten (shorter) transcript is read again; a removed one is dropped.
        std::fs::write(&path, user("p9", json!("Only the orca now"))).unwrap();
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        assert!(index
            .search("walrus", &folders, &[], None, 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            index
                .search("orca", &folders, &[], None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_file(&path).unwrap();
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        assert!(index
            .search("orca", &folders, &[], None, 10)
            .await
            .unwrap()
            .is_empty());
        // A past deadline leaves the cache incomplete rather than blocking.
        std::fs::write(&path, user("p10", json!("late"))).unwrap();
        assert!(!index
            .refresh(&dir.path().join("projects"), Instant::now())
            .await
            .unwrap());
        // Delete purges the derived copy, including the title, and repeated
        // refreshes cannot reimport it. The provider transcript stays byte-exact.
        let original = std::fs::read(&path).unwrap();
        assert!(index
            .refresh(&dir.path().join("projects"), far)
            .await
            .unwrap());
        let deleted = BTreeSet::from([session.to_owned()]);
        for _ in 0..2 {
            assert!(index
                .refresh_excluding(&dir.path().join("projects"), far, &deleted)
                .await
                .unwrap());
            let docs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM docs")
                .fetch_one(&index.pool)
                .await
                .unwrap();
            let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files")
                .fetch_one(&index.pool)
                .await
                .unwrap();
            assert_eq!((docs, files), (0, 0));
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn indexed_text_is_bounded_per_block_and_per_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let big = "x ".repeat(MAX_BLOCK_TEXT);
        let mut transcript = String::new();
        for n in 0..(MAX_TRANSCRIPT_TEXT as usize / MAX_BLOCK_TEXT + 4) {
            transcript +=
                &line(json!({"type":"user","uuid":format!("u{n}"),"message":{"content":big}}));
        }
        std::fs::write(&path, &transcript).unwrap();
        let (state, texts) = read_transcript(
            &path,
            TranscriptState::default(),
            transcript.len() as i64,
            1,
        )
        .unwrap();
        assert!(texts.iter().all(|text| text.body.len() <= MAX_BLOCK_TEXT));
        assert!(state.text_bytes <= MAX_TRANSCRIPT_TEXT);
        assert!(texts.len() < MAX_TRANSCRIPT_TEXT as usize / MAX_BLOCK_TEXT + 4);
        assert_eq!(
            state.offset,
            transcript.len() as i64,
            "every whole line is read past"
        );
        assert_eq!(
            state.turn_id.as_deref(),
            Some(format!("u{}", MAX_TRANSCRIPT_TEXT as usize / MAX_BLOCK_TEXT + 3).as_str())
        );
    }
}
