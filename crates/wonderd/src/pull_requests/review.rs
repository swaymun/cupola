//! One pull request's page: its description, the latest review of each
//! reviewer, the conversation and the first page of line comments, beside the
//! state, checks and files the list shows. Read-only. It is read through `gh`
//! like the list, with a larger but bounded output, and cached briefly per
//! pull request. A thread reads only pull requests it opened or listed.

use super::{
    associated, authorized_conversation, gh_access, login, parse_ref, pull_request, run_gh_limited,
    store_state, GhAccess, PullRef, PullRequest, MIN_REFRESH, OUTPUT_EXCEEDED, VERSION,
};
use crate::{AppState, OwnerAuthority};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path as FsPath;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Read beyond the list's own fields. The page refreshes the state the list
/// and auto-archive share, so it asks for every field they read too.
const PAGE_FIELDS: &str = "body,baseRefOid,files,latestReviews,comments";
/// `gh pr view` with every comment and review.
const VIEW_LIMIT: u64 = 4 * 1024 * 1024;
/// One page of line comments.
const INLINE_LIMIT: u64 = 2 * 1024 * 1024;
/// A page is reused this long before `gh` is asked again.
const TTL: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 64 * 1024;
const MAX_COMMENT_BODY: usize = 8 * 1024;
/// The newest conversation comments sent.
const MAX_COMMENTS: usize = 50;
/// Files, reviews and line comments: GitHub's first page.
const MAX_ROWS: usize = 100;
const MAX_CACHED: usize = 32;
const UNREACHABLE: &str = "GitHub could not be reached from your computer. Try again.";
const TOO_LARGE: &str = "This pull request is too large to show here. Open it on GitHub.";

struct Entry {
    at: Instant,
    /// Threads that read it; an action in any of them drops it.
    readers: HashSet<String>,
    review: PullRequestReview,
}

/// Per pull request address (lowercased).
static CACHE: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(Default::default);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetailQuery {
    url: String,
    refresh: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestReview {
    version: u32,
    /// The same fields the list sends for it.
    #[serde(flatten)]
    pull_request: PullRequest,
    /// Markdown, at most 64 KB.
    body: String,
    body_truncated: bool,
    /// The commit its base branch pointed at when GitHub compared them.
    base_oid: Option<String>,
    files: Vec<File>,
    /// More files changed than are listed.
    files_limited: bool,
    /// The latest review of each reviewer.
    reviews: Vec<Review>,
    /// The newest conversation comments, oldest first.
    comments: Vec<Comment>,
    comments_limited: bool,
    /// The first page of comments on lines of the diff, oldest first.
    inline_comments: Vec<InlineComment>,
    inline_limited: bool,
    /// When the computer read GitHub (RFC 3339).
    fetched_at: String,
    /// Set when part of the page could not be read.
    detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct File {
    path: String,
    additions: Option<u64>,
    deletions: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Review {
    author: Option<String>,
    /// GitHub's word: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, ...
    state: String,
    submitted_at: Option<String>,
    body: String,
    body_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    id: Option<String>,
    author: Option<String>,
    body: String,
    body_truncated: bool,
    created_at: Option<String>,
    url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct InlineComment {
    id: u64,
    path: String,
    /// The line in the current diff; none once the line is gone.
    line: Option<u64>,
    original_line: Option<u64>,
    /// `LEFT` or `RIGHT`.
    side: Option<String>,
    /// Its line changed since the comment, so it no longer shows in the diff.
    outdated: bool,
    in_reply_to: Option<u64>,
    author: Option<String>,
    body: String,
    body_truncated: bool,
    created_at: Option<String>,
    url: Option<String>,
}

pub(crate) async fn detail(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Response {
    if let Err(response) = authorized_conversation(&state, &conversation_id).await {
        return response;
    }
    // Only a canonical github.com address names a pull request here.
    let Some(reference) = query
        .url
        .strip_prefix("https://github.com/")
        .and_then(parse_ref)
        .filter(|reference| reference.url().eq_ignore_ascii_case(&query.url))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let url = reference.url();
    match associated(&state, &conversation_id, &url).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "This thread's history could not be read.",
            )
                .into_response()
        }
    }
    let refresh = query.refresh == Some(true);
    if let Some(review) = cached(&conversation_id, &url, refresh) {
        return Json(review).into_response();
    }
    let gh = match gh_access(refresh).await {
        GhAccess::Ready(gh) => gh,
        GhAccess::Missing | GhAccess::SignedOut => return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign in with the GitHub CLI (gh auth login) on your computer to see pull requests.",
        )
            .into_response(),
    };
    match read(&gh, &reference).await {
        Ok(review) => {
            store(&conversation_id, &url, &review);
            Json(review).into_response()
        }
        Err(detail) => (StatusCode::SERVICE_UNAVAILABLE, detail).into_response(),
    }
}

/// A page read within the TTL (within `MIN_REFRESH` for a refresh); the
/// thread is recorded as a reader so its actions drop it.
fn cached(conversation: &str, url: &str, refresh: bool) -> Option<PullRequestReview> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = cache.get_mut(&url.to_ascii_lowercase())?;
    let limit = if refresh { MIN_REFRESH } else { TTL };
    if entry.at.elapsed() >= limit {
        return None;
    }
    entry.readers.insert(conversation.to_owned());
    Some(entry.review.clone())
}

fn store(conversation: &str, url: &str, review: &PullRequestReview) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= MAX_CACHED {
        cache.retain(|_, entry| entry.at.elapsed() < TTL);
        while cache.len() >= MAX_CACHED {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.remove(&oldest);
        }
    }
    let key = url.to_ascii_lowercase();
    let mut readers = cache
        .remove(&key)
        .map(|entry| entry.readers)
        .unwrap_or_default();
    readers.insert(conversation.to_owned());
    cache.insert(
        key,
        Entry {
            at: Instant::now(),
            readers,
            review: review.clone(),
        },
    );
}

/// Drops every page the thread read.
pub(super) fn forget(conversation: &str) {
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, entry| !entry.readers.contains(conversation));
}

/// Reads the pull request and its first page of line comments. The page
/// fails only when the pull request itself cannot be read.
async fn read(gh: &FsPath, reference: &PullRef) -> Result<PullRequestReview, String> {
    let url = reference.url();
    let inline_page = format!(
        "repos/{}/{}/pulls/{}/comments?per_page={MAX_ROWS}",
        reference.owner, reference.repo, reference.number
    );
    let fields = format!("{},{PAGE_FIELDS}", super::FIELDS);
    let view_args = ["pr", "view", &url, "--json", &fields];
    let inline_args = ["api", &inline_page];
    let (view, inline) = tokio::join!(
        run_gh_limited(gh, None, &view_args, VIEW_LIMIT),
        run_gh_limited(gh, None, &inline_args, INLINE_LIMIT),
    );
    let view = view.map_err(|error| {
        if error == OUTPUT_EXCEEDED {
            TOO_LARGE
        } else {
            UNREACHABLE
        }
    })?;
    let value: Value = serde_json::from_slice(&view).map_err(|_| UNREACHABLE)?;
    let pull_request = pull_request(&value)
        .filter(|pr| pr.url.eq_ignore_ascii_case(&url))
        .ok_or(UNREACHABLE)?;
    store_state(&pull_request);
    let inline = inline
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Value>>(&bytes).ok());
    Ok(project(&value, pull_request, inline.as_deref()))
}

/// The page from `gh pr view` and, when read, the line comments.
fn project(
    value: &Value,
    pull_request: PullRequest,
    inline: Option<&[Value]>,
) -> PullRequestReview {
    let (body, body_truncated) = bounded(value.get("body"), MAX_BODY);
    let rows = |key: &str| value.get(key).and_then(Value::as_array).map(Vec::as_slice);
    let files: Vec<File> = rows("files")
        .unwrap_or_default()
        .iter()
        .take(MAX_ROWS)
        .filter_map(|file| {
            Some(File {
                path: short(file.get("path"), 1000)?,
                additions: file.get("additions").and_then(Value::as_u64),
                deletions: file.get("deletions").and_then(Value::as_u64),
            })
        })
        .collect();
    let changed = value.get("changedFiles").and_then(Value::as_u64);
    let files_limited = rows("files").is_some_and(|rows| rows.len() > files.len())
        || changed.is_some_and(|changed| changed > files.len() as u64);
    let reviews = rows("latestReviews")
        .unwrap_or_default()
        .iter()
        .take(MAX_ROWS)
        .map(|review| {
            let (body, body_truncated) = bounded(review.get("body"), MAX_COMMENT_BODY);
            Review {
                author: login(review.get("author")),
                state: short(review.get("state"), 40).unwrap_or_else(|| "UNKNOWN".into()),
                submitted_at: short(review.get("submittedAt"), 40),
                body,
                body_truncated,
            }
        })
        .collect();
    let all_comments = rows("comments").unwrap_or_default();
    let comments = all_comments[all_comments.len().saturating_sub(MAX_COMMENTS)..]
        .iter()
        .map(|comment| {
            let (body, body_truncated) = bounded(comment.get("body"), MAX_COMMENT_BODY);
            Comment {
                id: short(comment.get("id"), 100).filter(|id| {
                    id.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                }),
                author: login(comment.get("author")),
                body,
                body_truncated,
                created_at: short(comment.get("createdAt"), 40),
                url: page(comment.get("url"), &pull_request.url),
            }
        })
        .collect();
    let inline_comments = inline
        .unwrap_or_default()
        .iter()
        .take(MAX_ROWS)
        .filter_map(|comment| {
            let (body, body_truncated) = bounded(comment.get("body"), MAX_COMMENT_BODY);
            let line = comment.get("line").and_then(Value::as_u64);
            Some(InlineComment {
                id: comment.get("id")?.as_u64()?,
                path: short(comment.get("path"), 1000)?,
                line,
                original_line: comment.get("original_line").and_then(Value::as_u64),
                side: short(comment.get("side"), 10),
                outdated: line.is_none() || comment.get("position").is_some_and(Value::is_null),
                in_reply_to: comment.get("in_reply_to_id").and_then(Value::as_u64),
                author: login(comment.get("user")),
                body,
                body_truncated,
                created_at: short(comment.get("created_at"), 40),
                url: page(comment.get("html_url"), &pull_request.url),
            })
        })
        .collect();
    PullRequestReview {
        version: VERSION,
        body,
        body_truncated,
        base_oid: value
            .get("baseRefOid")
            .and_then(Value::as_str)
            .filter(|oid| super::valid_oid(oid))
            .map(str::to_ascii_lowercase),
        files,
        files_limited,
        reviews,
        comments_limited: all_comments.len() > MAX_COMMENTS,
        comments,
        inline_limited: inline.is_some_and(|rows| rows.len() >= MAX_ROWS),
        inline_comments,
        fetched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        detail: inline
            .is_none()
            .then(|| "Some comments could not be loaded.".into()),
        pull_request,
    }
}

/// Text cut to at most `max` bytes at a character boundary, and whether it was cut.
fn bounded(value: Option<&Value>, max: usize) -> (String, bool) {
    let text = value.and_then(Value::as_str).unwrap_or_default();
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// A short field, dropped when it is longer than `max` bytes or empty.
fn short(value: Option<&Value>, max: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= max)
        .map(str::to_owned)
}

/// A comment's own address, which lies under its pull request's.
fn page(value: Option<&Value>, pull_request: &str) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|url| {
            url.len() <= 500
                && url.bytes().all(|b| b.is_ascii_graphic())
                && url.get(..pull_request.len()).is_some_and(|prefix| {
                    prefix.eq_ignore_ascii_case(pull_request)
                        && matches!(url.as_bytes().get(pull_request.len()), Some(b'#' | b'/'))
                })
        })
        .map(str::to_owned)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    const ADDRESS: &str = "https://github.com/o/review-fixture/pull/5";

    /// A stand-in `gh` that logs its calls and answers `pr view` with the
    /// fields of `view.json` that `--json` asks for, as gh does, and `api`
    /// with `inline.json` from its folder; a missing file fails that call.
    fn stand_in(dir: &FsPath) -> std::path::PathBuf {
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
here="$(dirname "$0")"
echo "$@" >> "$here/calls"
case "$1" in
  pr) python3 -I -c 'import json, sys
row = json.load(open(sys.argv[1]))
print(json.dumps({k: row[k] for k in sys.argv[2].split(",") if k in row}))' "$here/view.json" "$5" || exit 1 ;;
  api) cat "$here/inline.json" || exit 1 ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    fn view(body: &str, comments: usize) -> Value {
        json!({"number":5,"url":ADDRESS,"state":"OPEN","isDraft":false,"title":"Review me",
            "body":body,"author":{"login":"alex"},"headRefName":"agent/x","baseRefName":"main",
            "headRefOid":"a".repeat(40),"baseRefOid":"B".repeat(40),"additions":3,"deletions":1,
            "changedFiles":120,
            "files":(0..100).map(|i| json!({"path":format!("src/{i}.rs"),"additions":1,"deletions":0})).collect::<Vec<_>>(),
            "reviewDecision":"CHANGES_REQUESTED",
            "reviewRequests":[{"__typename":"User","login":"sam"}],
            "latestReviews":[
                {"author":{"login":"sam"},"state":"CHANGES_REQUESTED","submittedAt":"2026-10-25T10:00:00Z",
                 "body":"Please handle the empty list."},
                {"author":{"login":"bugbot[bot]"},"state":"COMMENTED","submittedAt":"2026-10-25T11:00:00Z",
                 "body":"é".repeat(5000)}],
            "comments":(0..comments).map(|i| json!({"id":format!("IC_{i}"),"author":{"login":"alex"},
                "body":format!("comment {i}"),"createdAt":"2026-10-25T09:00:00Z",
                "url":format!("{ADDRESS}#issuecomment-{i}")})).collect::<Vec<_>>(),
            "statusCheckRollup":[{"name":"Unit tests","status":"COMPLETED","conclusion":"FAILURE"}],
            "mergeStateStatus":"BLOCKED","mergeable":"MERGEABLE"})
    }

    fn inline() -> Value {
        json!([
            {"id":11,"path":"src/lib.rs","line":42,"original_line":40,"side":"RIGHT","position":3,
             "user":{"login":"sam"},"body":"This panics on an empty list.",
             "created_at":"2026-10-25T10:00:00Z","html_url":format!("{ADDRESS}#discussion_r11")},
            {"id":12,"path":"src/lib.rs","line":null,"original_line":7,"side":"RIGHT","position":null,
             "in_reply_to_id":11,"user":{"login":"alex"},"body":"Fixed.",
             "created_at":"2026-10-25T10:30:00Z","html_url":"https://evil.example/o/review-fixture/pull/5"}
        ])
    }

    fn reference() -> PullRef {
        parse_ref(ADDRESS.strip_prefix("https://github.com/").unwrap()).unwrap()
    }

    // A pull request with a 2 MB description, more than the list's 1 MB
    // bound, is read; what reaches the phone is bounded: the description at
    // 64 KB, the newest 50 comments, review bodies at 8 KB on a character
    // boundary, the first 100 files with the rest admitted, and line
    // comments that say when their line is gone.
    #[tokio::test]
    async fn a_large_pull_request_is_read_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let gh = stand_in(dir.path());
        let body = "x".repeat(2 * 1024 * 1024);
        std::fs::write(dir.path().join("view.json"), view(&body, 60).to_string()).unwrap();
        std::fs::write(dir.path().join("inline.json"), inline().to_string()).unwrap();
        let review = read(&gh, &reference()).await.unwrap();
        assert_eq!((review.body.len(), review.body_truncated), (MAX_BODY, true));
        assert_eq!(review.comments.len(), 50);
        assert!(review.comments_limited);
        assert_eq!(review.comments[0].body, "comment 10", "the newest 50");
        assert_eq!(review.comments[0].id.as_deref(), Some("IC_10"));
        assert_eq!(review.reviews[0].body, "Please handle the empty list.");
        assert_eq!(review.reviews[0].state, "CHANGES_REQUESTED");
        assert!(review.reviews[1].body_truncated);
        assert!(review.reviews[1].body.len() <= MAX_COMMENT_BODY);
        assert_eq!(review.reviews[1].author.as_deref(), Some("bugbot[bot]"));
        assert_eq!((review.files.len(), review.files_limited), (100, true));
        assert_eq!(review.base_oid, Some("b".repeat(40)));
        let (current, gone) = (&review.inline_comments[0], &review.inline_comments[1]);
        assert_eq!((current.line, current.outdated), (Some(42), false));
        assert_eq!((gone.original_line, gone.outdated), (Some(7), true));
        assert_eq!(gone.in_reply_to, Some(11));
        assert_eq!(gone.url, None, "a link off the pull request is dropped");
        assert!(!review.inline_limited && review.detail.is_none());
        let sent = serde_json::to_value(&review).unwrap();
        assert_eq!(sent["details"]["reviewDecision"], "CHANGES_REQUESTED");
        assert_eq!(sent["checks"]["state"], "failing");
        assert_eq!(sent["headRef"], "agent/x");
        assert_eq!(sent["version"], VERSION);
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(
            calls.contains("api repos/o/review-fixture/pulls/5/comments?per_page=100"),
            "{calls}"
        );
    }

    // Output past 4 MB is refused, not cut into broken JSON; and when only
    // the line comments fail, the rest of the page still comes back.
    #[tokio::test]
    async fn oversized_pages_fail_and_missing_line_comments_are_partial() {
        let dir = tempfile::tempdir().unwrap();
        let gh = stand_in(dir.path());
        let body = "x".repeat(VIEW_LIMIT as usize);
        std::fs::write(dir.path().join("view.json"), view(&body, 0).to_string()).unwrap();
        assert_eq!(read(&gh, &reference()).await.unwrap_err(), TOO_LARGE);

        std::fs::write(dir.path().join("view.json"), view("Short", 2).to_string()).unwrap();
        let review = read(&gh, &reference()).await.unwrap();
        assert_eq!(
            review.detail.as_deref(),
            Some("Some comments could not be loaded.")
        );
        assert!(review.inline_comments.is_empty());
        assert_eq!((review.body.as_str(), review.comments.len()), ("Short", 2));
    }

    // Opening a merged pull request's page refreshes the state the list and
    // auto-archive share, and keeps its merge time: without it a page read
    // would make the merge invisible and the chat would never archive.
    #[tokio::test]
    async fn a_page_read_keeps_the_merge_time_auto_archive_needs() {
        let dir = tempfile::tempdir().unwrap();
        let gh = stand_in(dir.path());
        let url = "https://github.com/o/merge-fixture/pull/8";
        let mut merged = view("Body", 0);
        merged["url"] = json!(url);
        merged["number"] = json!(8);
        merged["state"] = json!("MERGED");
        merged["mergedAt"] = json!("2026-10-25T12:00:00Z");
        std::fs::write(dir.path().join("view.json"), merged.to_string()).unwrap();
        std::fs::write(dir.path().join("inline.json"), "[]").unwrap();
        let reference = parse_ref(url.strip_prefix("https://github.com/").unwrap()).unwrap();
        read(&gh, &reference).await.unwrap();
        let state = super::super::cached_state(url, false).unwrap();
        assert_eq!(state.state, "merged");
        assert_eq!(state.merged_at.as_deref(), Some("2026-10-25T12:00:00Z"));
    }

    fn sample() -> PullRequestReview {
        let value = view("Body", 1);
        project(&value, pull_request(&value).unwrap(), Some(&[]))
    }

    // One read serves every thread that may see the address, whatever its
    // case; a refresh after ten seconds reads again; an action in any thread
    // that read it drops it.
    #[test]
    fn pages_are_cached_per_address_and_forgotten_with_their_readers() {
        let url = "https://github.com/o/cache-fixture/pull/1";
        store("cache-a", url, &sample());
        assert!(cached("cache-b", &url.to_uppercase(), false).is_some());
        assert!(cached("cache-b", url, true).is_some());
        CACHE.lock().unwrap().get_mut(url).unwrap().at -= MIN_REFRESH;
        assert!(cached("cache-a", url, true).is_none());
        assert!(cached("cache-a", url, false).is_some());
        super::super::forget("cache-b");
        assert!(cached("cache-a", url, false).is_none(), "cache-b read it");
        store("cache-a", url, &sample());
        super::super::forget("cache-other");
        assert!(cached("cache-a", url, false).is_some());
    }

    // The route answers only for a pull request the thread opened or
    // listed: any other address, or a look-alike, is not found without
    // asking GitHub.
    #[tokio::test]
    async fn only_the_threads_own_pull_requests_are_read() {
        let (dir, state) = crate::test_support::fixture().await;
        let gh = stand_in(dir.path());
        std::fs::write(dir.path().join("view.json"), view("Body", 1).to_string()).unwrap();
        std::fs::write(dir.path().join("inline.json"), inline().to_string()).unwrap();
        crate::pull_requests::use_test_gh(gh);
        crate::test_support::project_thread(
            &state.store,
            "pr-review",
            "Project",
            state.data_dir.to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        let get = |url: &str| {
            let path = format!(
                "/api/v1/project-conversations/pr-review/pull-requests/detail?url={}",
                url.replace('#', "%23")
            );
            let state = state.clone();
            async move { crate::test_support::call(&state, "GET", &path, Value::Null).await }
        };
        for url in [
            ADDRESS,
            "https://github.com/o/review-fixture/pull/5/files",
            "https://github.example.com/o/review-fixture/pull/5",
            "https://github.com/o/review-fixture/issues/5",
        ] {
            assert_eq!(get(url).await.0, StatusCode::NOT_FOUND, "{url}");
        }
        assert!(!dir.path().join("calls").exists(), "GitHub was not asked");
        let (status, _) = crate::test_support::call(
            &state,
            "GET",
            &format!("/api/v1/project-conversations/unknown/pull-requests/detail?url={ADDRESS}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        state
            .store
            .link_thread_pull_request(
                "pr-review",
                ADDRESS,
                state.data_dir.to_str().unwrap(),
                "now",
            )
            .await
            .unwrap();
        let (status, page) = get(ADDRESS).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["body"], "Body");
        assert_eq!(page["reviews"][0]["author"], "sam");
        assert_eq!(page["inlineComments"][0]["path"], "src/lib.rs");
        assert_eq!(page["url"], ADDRESS);
        assert!(page["fetchedAt"].as_str().is_some());
        let (status, _) = get(ADDRESS).await;
        assert_eq!(status, StatusCode::OK);
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(
            calls.lines().filter(|l| l.starts_with("pr view")).count(),
            1,
            "{calls}"
        );
        crate::pull_requests::forget("pr-review");
        assert!(cached("pr-review", ADDRESS, false).is_none());
    }
}
