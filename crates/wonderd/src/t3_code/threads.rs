//! What the owner (and a chat's agent) can do to a T3 thread through T3
//! Code's own MCP tools, never its internal protocol: organize it (pin,
//! snooze, settle, archive, mark unread: `t3_thread_organize`), fork it
//! (`t3_thread_transfer`), rename it (`t3_thread_update`), search threads
//! (`t3_thread_search`) and read its worktree and pull request status
//! (`t3_thread_read`; `t3_worktree_status` answers only for the caller's
//! own thread, which Cupola's grant doesn't have). Each answers with a short
//! plain result or a plain reason, never T3 Code's codes.
//!
//! Not offered on purpose: scheduling, `delegate_task`, project create,
//! delete or clone, environment preferences and worktree handoff.
use super::*;

/// Organize actions T3 Code takes; `snooze` also needs a time.
pub(crate) const ORGANIZE_ACTIONS: [&str; 9] = [
    "pin",
    "unpin",
    "snooze",
    "unsnooze",
    "settle",
    "unsettle",
    "archive",
    "unarchive",
    "mark_unread",
];
const TITLE_CHARS: usize = 200;
const SEARCH_MAX: usize = 50;

impl T3Code {
    /// Pins, snoozes (until `snoozed_until`, an ISO time), settles, archives
    /// or marks unread a T3 thread, or undoes one of those.
    pub(crate) async fn organize(
        &self,
        thread: &str,
        action: &str,
        snoozed_until: Option<&str>,
    ) -> Result<Value, CallError> {
        if !ORGANIZE_ACTIONS.contains(&action) {
            return Err(refused(
                "invalid",
                "That isn't something T3 Code can do to a thread.",
            ));
        }
        let mut arguments = json!({"threadId": thread, "action": action});
        match (action, snoozed_until) {
            ("snooze", None) => {
                return Err(refused(
                    "invalid",
                    "Choose when the thread should come back.",
                ))
            }
            ("snooze", Some(until)) => {
                arguments["snoozedUntil"] = until.into();
            }
            _ => {}
        }
        let answer = self.call("t3_thread_organize", arguments).await?;
        self.reread_store().await;
        Ok(answer)
    }

    /// Copies the thread from `source_point` (T3 Code's default: the latest
    /// stable run) into a new T3 thread with the same setup. No turn runs
    /// there yet. Returns the new thread's id.
    pub(crate) async fn fork(
        &self,
        thread: &str,
        title: Option<&str>,
        source_point: Option<Value>,
    ) -> Result<String, CallError> {
        let mut arguments = json!({"action": "fork", "threadId": thread});
        if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
            arguments["title"] = bounded(title)?.into();
        }
        if let Some(point) = source_point {
            arguments["sourcePoint"] = point;
        }
        let answer = self.call("t3_thread_transfer", arguments).await?;
        self.reread_store().await;
        // T3 accepted the call, so the fork may exist: say "check there",
        // not "try again", which would duplicate it.
        answer
            .get("targetThreadId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(CallError::Lost)
    }

    /// Gives the thread a new title.
    pub(crate) async fn rename(&self, thread: &str, title: &str) -> Result<String, CallError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(refused("invalid", "Enter a name for the thread."));
        }
        let answer = self
            .call(
                "t3_thread_update",
                json!({"threadId": thread, "action": "rename", "title": bounded(title)?}),
            )
            .await?;
        self.reread_store().await;
        Ok(answer
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(title)
            .to_owned())
    }

    /// Threads whose title, branch, pull request or messages match `query`
    /// (T3 Code searches active threads and says it isn't exhaustive).
    pub(crate) async fn search(
        &self,
        query: &str,
        project: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, CallError> {
        let query = query.trim();
        if query.chars().count() < 2 {
            return Err(refused("invalid", "Type at least two letters to search."));
        }
        let mut arguments = json!({
            "query": bounded(query)?,
            "limit": limit.unwrap_or(20).clamp(1, SEARCH_MAX),
        });
        if let Some(project) = project {
            arguments["projectId"] = project.into();
        }
        self.call("t3_thread_search", arguments).await
    }

    /// Where the thread works: its branch, worktree folder and pull
    /// requests, plus whether it is pinned, settled, snoozed or archived.
    pub(crate) async fn placement(&self, thread: &str) -> Result<Value, CallError> {
        let read = self
            .call(
                "t3_thread_read",
                json!({"threadId": thread, "view": "messages", "limit": 1}),
            )
            .await?;
        let detail = read.get("thread").cloned().unwrap_or_default();
        let field = |name: &str| detail.get(name).cloned().unwrap_or(Value::Null);
        let worktree = field("worktreePath");
        Ok(json!({
            "threadId": thread,
            "title": field("title"),
            "branch": field("branch"),
            "worktreePath": worktree,
            "inWorktree": !worktree.is_null(),
            "linkedPullRequest": field("linkedPullRequest"),
            "pullRequests": detail.get("pullRequests").cloned().unwrap_or(json!([])),
            "parentThreadId": field("parentThreadId"),
            "relationshipToParent": field("relationshipToParent"),
            "settled": field("settled"),
            "snoozedUntil": field("snoozedUntil"),
            "archived": field("archived"),
        }))
    }
}

fn bounded(text: &str) -> Result<&str, CallError> {
    if text.chars().count() > TITLE_CHARS {
        return Err(refused(
            "invalid",
            format!("Use at most {TITLE_CHARS} characters."),
        ));
    }
    Ok(text)
}

fn refused(code: &str, message: impl Into<String>) -> CallError {
    CallError::Refused {
        code: code.into(),
        message: message.into(),
    }
}

/// What to tell the owner (or the agent) when a T3 tool didn't work.
pub(crate) fn plain(error: &CallError) -> String {
    match error {
        CallError::NotConnected => "Connect T3 Code in Settings first.".into(),
        CallError::Unauthorized => {
            "T3 Code no longer accepts Cupola. Connect it again in Settings.".into()
        }
        CallError::Forbidden | CallError::Offline => {
            "T3 Code isn't running on your computer. Open it and try again.".into()
        }
        CallError::Lost => {
            "T3 Code didn't answer. It may have done it; check there before trying again.".into()
        }
        CallError::Protocol(_) => "T3 Code couldn't take that request. Try again.".into(),
        CallError::Refused { code, message } => match code.as_str() {
            "thread_not_found" => "T3 Code doesn't have that thread any more.".into(),
            "capability_denied" => {
                "T3 Code didn't allow Cupola to do that. Connect it again in Settings.".into()
            }
            "orchestration_error" if message.contains("no completed run to mark unread") => {
                "This thread has no finished reply to mark unread yet.".into()
            }
            // Only Cupola's own plain wording is passed on; T3's is technical.
            "invalid" if !message.is_empty() => message.clone(),
            _ => "T3 Code refused that.".into(),
        },
    }
}

/// The HTTP status for a failed T3 tool call.
pub(crate) fn status_for(error: &CallError) -> StatusCode {
    match error {
        CallError::NotConnected | CallError::Unauthorized => StatusCode::CONFLICT,
        CallError::Offline | CallError::Forbidden => StatusCode::SERVICE_UNAVAILABLE,
        CallError::Lost | CallError::Protocol(_) => StatusCode::BAD_GATEWAY,
        CallError::Refused { code, .. } => match code.as_str() {
            "thread_not_found" => StatusCode::NOT_FOUND,
            "invalid" | "invalid_request" => StatusCode::BAD_REQUEST,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadAction {
    action: Option<String>,
    snoozed_until: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchQuery {
    q: String,
    project_id: Option<String>,
    limit: Option<usize>,
}

fn failed(error: CallError) -> Response {
    (status_for(&error), Json(json!({ "detail": plain(&error) }))).into_response()
}

pub(crate) async fn organize_thread(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    axum::extract::Path(thread): axum::extract::Path<String>,
    Json(request): Json<ThreadAction>,
) -> Response {
    let action = request.action.unwrap_or_default();
    match state
        .t3_code
        .organize(&thread, &action, request.snoozed_until.as_deref())
        .await
    {
        Ok(_) => Json(json!({"threadId": thread, "action": action})).into_response(),
        Err(error) => failed(error),
    }
}

pub(crate) async fn fork_thread(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    axum::extract::Path(thread): axum::extract::Path<String>,
    Json(request): Json<ThreadAction>,
) -> Response {
    match state
        .t3_code
        .fork(&thread, request.title.as_deref(), None)
        .await
    {
        Ok(forked) => Json(json!({"threadId": forked, "forkedFrom": thread})).into_response(),
        Err(error) => failed(error),
    }
}

pub(crate) async fn rename_thread(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    axum::extract::Path(thread): axum::extract::Path<String>,
    Json(request): Json<ThreadAction>,
) -> Response {
    match state
        .t3_code
        .rename(&thread, request.title.as_deref().unwrap_or_default())
        .await
    {
        Ok(title) => Json(json!({"threadId": thread, "title": title})).into_response(),
        Err(error) => failed(error),
    }
}

pub(crate) async fn thread_status(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    axum::extract::Path(thread): axum::extract::Path<String>,
) -> Response {
    match state.t3_code.placement(&thread).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => failed(error),
    }
}

pub(crate) async fn search_threads(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(query): Query<SearchQuery>,
) -> Response {
    match state
        .t3_code
        .search(&query.q, query.project_id.as_deref(), query.limit)
        .await
    {
        Ok(found) => Json(found).into_response(),
        Err(error) => failed(error),
    }
}
