//! Project helper history, plus the commands Codex left running. Every request
//! rechecks the owning Project and the provider's exact parent/spawn source;
//! activity item IDs are hints.

use crate::{history, projects, subagents, AppState, OwnerAuthority};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use wonder_app_server::RpcClient;
use wonder_store::{
    AgentFamily, AgentTasks, BackgroundWork, StoredProject, StoredProjectConversation,
};

const MAX_CHILDREN: usize = 100;
// Claude lists every Bash run and agent in a session; the bridge sends the newest 1,000.
const MAX_CLAUDE_TASKS: usize = 1000;
const PAGE_SIZE: usize = 100;
// The bridge returns an agent's latest 200 steps plus its task and result text.
const CLAUDE_TASK_ITEMS: usize = 256;
const MAX_ARCHIVE_LOOKUP_PAGES: usize = 10;
// Commands Codex left running in one thread: every page, up to this many rows.
const MAX_COMMANDS: usize = 100;
const MAX_COMMAND_PAGES: usize = 10;
/// Roster IDs of Codex background commands; native thread IDs never carry it.
const COMMAND_ID_PREFIX: &str = "command:";
const COMMANDS_UNCHECKED: &str =
    "Commands Codex left running couldn't be checked. Refresh to try again.";

struct ProjectParent {
    conversation: StoredProjectConversation,
    project: StoredProject,
    thread_id: String,
    rpc: RpcClient,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectSubagentSummary {
    parent_conversation_id: String,
    thread_id: String,
    title: String,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    status: String,
    is_archived: bool,
    can_accept_direct_input: bool,
    /// Whether the provider names its subagents; without names the app shows
    /// the task and a status glyph instead of an avatar.
    named_subagents: bool,
    /// `agent`, `bash`, `command`, `monitor`, `session` or `task` for
    /// transcript-derived tasks, otherwise absent.
    kind: Option<String>,
    /// Wonder is driving the session and the task can be stopped from here.
    can_stop: bool,
    /// The task is running in another native process on the Mac.
    running_elsewhere: bool,
    updated_at: Option<String>,
    /// How long a finished Claude task ran, when the transcript dates both ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_activity: Option<Value>,
    /// Claude's native progress summary, separate from a task's final report.
    #[serde(skip_serializing_if = "Option::is_none")]
    progress_summary: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSubagentList {
    available: bool,
    detail: Option<String>,
    subagents: Vec<ProjectSubagentSummary>,
    next_current_cursor: Option<String>,
    next_archived_cursor: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSubagentTranscript {
    subagent: ProjectSubagentSummary,
    snapshot: history::ConversationSnapshot,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    archived: Option<bool>,
    cursor: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TranscriptQuery {
    cursor: Option<String>,
    /// The row's kind as the roster listed it (`t3Task`): a T3 Code task is
    /// read from T3 Code, and nothing else is asked of it.
    kind: Option<String>,
}

async fn request(rpc: &RpcClient, method: &str, params: Value) -> Result<Value, String> {
    let response = rpc
        .request(method, params)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(error) = response.error {
        return Err(error.message);
    }
    response
        .result
        .ok_or_else(|| format!("{method} returned no result"))
}

fn unavailable(detail: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, detail.to_owned()).into_response()
}

async fn parent(state: &AppState, conversation_id: &str) -> Result<ProjectParent, Box<Response>> {
    let conversation = state
        .store
        .project_conversation(conversation_id)
        .await
        .map_err(|_| Box::new(unavailable("The Project thread could not be checked.")))?
        .ok_or_else(|| Box::new(StatusCode::NOT_FOUND.into_response()))?;
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .map_err(|_| Box::new(unavailable("The Project could not be checked.")))?
        .ok_or_else(|| Box::new(StatusCode::NOT_FOUND.into_response()))?;
    let project = projects::thread_project(state, project, &conversation.cwd).await;
    if !project.is_included || project.root_for(&conversation.cwd).is_none() {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    let checked = project.clone();
    let denied = state.denied_roots.clone();
    let roots_valid =
        tokio::task::spawn_blocking(move || projects::validate_execution_roots(&checked, &denied))
            .await
            .map_err(|_| Box::new(unavailable("The Project folders could not be checked.")))?;
    if roots_valid.is_err() {
        return Err(Box::new(StatusCode::CONFLICT.into_response()));
    }
    if !crate::providers::is_default_store(state, conversation.family, &conversation.provider_store)
    {
        return Err(Box::new(StatusCode::CONFLICT.into_response()));
    }
    let native_session = conversation.native_session_id.clone().ok_or_else(|| {
        Box::new(
            (
                StatusCode::CONFLICT,
                "This Project thread has no agent history yet.",
            )
                .into_response(),
        )
    })?;
    let binding = state
        .store
        .runtime_binding(conversation_id)
        .await
        .map_err(|_| {
            Box::new(unavailable(
                "The Project runtime identity could not be checked.",
            ))
        })?
        .ok_or_else(|| Box::new(StatusCode::CONFLICT.into_response()))?;
    if !binding_matches(conversation.family, &native_session, &binding) {
        return Err(Box::new(StatusCode::CONFLICT.into_response()));
    }
    let thread_id = binding.thread_id.clone();
    let rpc =
        crate::providers::rpc_for_store(state, conversation.family, &conversation.provider_store)
            .await
            .map_err(|_| {
                Box::new(unavailable(
                    "The agent runtime is unavailable on your computer.",
                ))
            })?;
    let read = request(
        &rpc,
        "thread/read",
        json!({"threadId": thread_id, "includeTurns": false}),
    )
    .await
    .map_err(|_| Box::new(unavailable("The Project thread could not be verified.")))?;
    let thread = read.get("thread").unwrap_or(&read);
    if thread.get("id").and_then(Value::as_str) != Some(thread_id.as_str())
        || (conversation.family == AgentFamily::Claude
            && thread.get("sessionId").and_then(Value::as_str) != Some(native_session.as_str()))
        || thread.get("cwd").and_then(Value::as_str) != Some(conversation.cwd.as_str())
        || thread
            .get("parentThreadId")
            .and_then(Value::as_str)
            .is_some()
    {
        return Err(Box::new(StatusCode::CONFLICT.into_response()));
    }
    Ok(ProjectParent {
        conversation,
        project,
        thread_id,
        rpc,
    })
}

/// Claude-style agent tasks read from the session transcript.
fn transcript_tasks(parent: &ProjectParent) -> bool {
    parent
        .conversation
        .family
        .provider()
        .capabilities
        .agent_tasks
        == AgentTasks::Transcript
}

/// Commands a reply left running live in the runtime as background terminals.
fn keeps_terminals(parent: &ProjectParent) -> bool {
    parent
        .conversation
        .family
        .provider()
        .capabilities
        .background_work
        == BackgroundWork::Terminals
}

fn binding_matches(
    family: AgentFamily,
    native_session: &str,
    binding: &wonder_store::RuntimeBinding,
) -> bool {
    binding.execution_scope == "projects"
        && binding.family == family
        && crate::providers::binds_native_session(family, native_session, binding)
}

fn verified_child(
    parent: &ProjectParent,
    thread: &Value,
    archived: bool,
) -> Option<ProjectSubagentSummary> {
    let verified = subagents::verified_thread(&parent.thread_id, thread).ok()?;
    let cwd = thread.get("cwd").and_then(Value::as_str)?;
    parent.project.root_for(cwd)?;
    let title = verified
        .agent_nickname
        .as_deref()
        .or(verified.agent_role.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Agent")
        .to_owned();
    Some(ProjectSubagentSummary {
        parent_conversation_id: parent.conversation.conversation_id.clone(),
        thread_id: verified.child_thread_id,
        title,
        agent_nickname: verified.agent_nickname,
        agent_role: verified.agent_role,
        status: subagents::lifecycle_status(thread, None),
        is_archived: archived,
        can_accept_direct_input: false,
        named_subagents: parent
            .conversation
            .family
            .provider()
            .capabilities
            .named_subagents,
        kind: None,
        can_stop: false,
        running_elsewhere: thread.get("runningElsewhere").and_then(Value::as_bool) == Some(true),
        updated_at: None,
        duration_seconds: None,
        current_activity: None,
        progress_summary: None,
    })
}

// `thread/read` has no archive field. Scan bounded parent-list pages only when
// opening one child, without adding requests for every roster row.
async fn child_in_list(
    parent: &ProjectParent,
    thread_id: &str,
    archived: bool,
) -> Result<(bool, bool), Box<Response>> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    for _ in 0..MAX_ARCHIVE_LOOKUP_PAGES {
        let result = request(
            &parent.rpc,
            "thread/list",
            json!({
                "sourceKinds": ["subAgentThreadSpawn"],
                "parentThreadId": parent.thread_id,
                "archived": archived,
                "limit": MAX_CHILDREN,
                "cursor": cursor,
            }),
        )
        .await
        .map_err(|_| {
            Box::new(unavailable(
                "The agent task's archive state could not be checked.",
            ))
        })?;
        let threads = result
            .get("data")
            .and_then(Value::as_array)
            .filter(|threads| threads.len() <= MAX_CHILDREN)
            .ok_or_else(|| Box::new(unavailable("Codex returned an invalid agent task list.")))?;
        if let Some(thread) = threads
            .iter()
            .find(|thread| thread.get("id").and_then(Value::as_str) == Some(thread_id))
        {
            return verified_child(parent, thread, archived)
                .map(|_| (true, false))
                .ok_or_else(|| {
                    Box::new(unavailable(
                        "The agent task's archive state could not be verified.",
                    ))
                });
        }
        cursor = match result.get("nextCursor") {
            None | Some(Value::Null) => return Ok((false, false)),
            Some(Value::String(next))
                if !next.is_empty() && next.len() <= 2048 && seen_cursors.insert(next.clone()) =>
            {
                Some(next.clone())
            }
            _ => {
                return Err(Box::new(unavailable(
                    "Codex returned an invalid agent task cursor.",
                )))
            }
        };
    }
    Ok((false, true))
}

async fn child_archived(parent: &ProjectParent, thread_id: &str) -> Result<bool, Box<Response>> {
    let (current, more_current) = child_in_list(parent, thread_id, false).await?;
    if current {
        return Ok(false);
    }
    let (archived, more_archived) = child_in_list(parent, thread_id, true).await?;
    if archived {
        return Ok(true);
    }
    if more_current || more_archived {
        return Err(Box::new(unavailable(
            "The agent task's archive state could not be checked.",
        )));
    }
    Err(Box::new(StatusCode::NOT_FOUND.into_response()))
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Response {
    // Include ownership/runtime locks, native paging and every child read in
    // one deadline; a slow provider must not hold the phone's dock open.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        list_inner(&state, &conversation_id, query),
    )
    .await
    .unwrap_or_else(|_| unavailable("Agent tasks could not be loaded. Try again."))
}

/// The thread exists and its Project is still included.
async fn parent_included(state: &AppState, conversation_id: &str) -> bool {
    let Ok(Some(conversation)) = state.store.project_conversation(conversation_id).await else {
        return false;
    };
    matches!(
        state.store.project(&conversation.project_id).await,
        Ok(Some(project)) if project.is_included
    )
}

async fn list_inner(state: &AppState, conversation_id: &str, query: ListQuery) -> Response {
    if query
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2048 || query.archived.is_none())
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let parent = match parent(state, conversation_id).await {
        Ok(parent) => parent,
        Err(response) => {
            // Delegated threads come from Cupola's own store: a parent with no
            // agent history yet, or on a provider without agent tasks, still
            // lists them. Ownership failures stay failures.
            if !matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
            ) && query.archived != Some(true)
                && query.cursor.is_none()
                && parent_included(state, conversation_id).await
            {
                let rows = delegated_rows(state, conversation_id).await;
                if !rows.is_empty() {
                    return Json(ProjectSubagentList {
                        available: true,
                        detail: None,
                        subagents: rows,
                        next_current_cursor: None,
                        next_archived_cursor: None,
                    })
                    .into_response();
                }
            }
            if response.status() == StatusCode::NOT_IMPLEMENTED {
                return Json(ProjectSubagentList {
                    available: false,
                    detail: Some("Agent task history is available for Codex Projects.".into()),
                    subagents: Vec::new(),
                    next_current_cursor: None,
                    next_archived_cursor: None,
                })
                .into_response();
            }
            return *response;
        }
    };
    if transcript_tasks(&parent) {
        // Claude agent tasks and background commands come from the session
        // transcript itself; they have no archive or paging of their own.
        if query.archived == Some(true) || query.cursor.is_some() {
            return Json(ProjectSubagentList {
                available: true,
                detail: None,
                subagents: Vec::new(),
                next_current_cursor: None,
                next_archived_cursor: None,
            })
            .into_response();
        }
        return match claude_tasks(&parent).await {
            Ok(mut subagents) => Json(ProjectSubagentList {
                available: true,
                detail: None,
                subagents: {
                    subagents.extend(linked_tasks(state, &parent).await);
                    subagents
                },
                next_current_cursor: None,
                next_archived_cursor: None,
            })
            .into_response(),
            Err(response) => *response,
        };
    }
    let mut children = Vec::new();
    let mut seen = HashSet::new();
    let mut next_current_cursor = None;
    let mut next_archived_cursor = None;
    for archived in query
        .archived
        .map_or(vec![false, true], |value| vec![value])
    {
        let result = match request(
            &parent.rpc,
            "thread/list",
            json!({
                "sourceKinds": ["subAgentThreadSpawn"],
                "parentThreadId": parent.thread_id,
                "archived": archived,
                "limit": MAX_CHILDREN,
                "cursor": query.cursor,
            }),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => return unavailable("Agent tasks could not be loaded. Try again."),
        };
        let Some(threads) = result.get("data").and_then(Value::as_array) else {
            return unavailable("Codex returned an invalid agent task list.");
        };
        if threads.len() > MAX_CHILDREN {
            return unavailable("Codex returned too many agent tasks.");
        }
        let next_cursor = match result.get("nextCursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) if !cursor.is_empty() && cursor.len() <= 2048 => {
                Some(cursor.clone())
            }
            _ => return unavailable("Codex returned an invalid agent task cursor."),
        };
        if archived {
            next_archived_cursor = next_cursor;
        } else {
            next_current_cursor = next_cursor;
        }
        for thread in threads {
            if let Some(child) = verified_child(&parent, thread, archived) {
                if seen.insert(child.thread_id.clone()) {
                    children.push((thread.clone(), archived));
                }
            }
        }
    }
    // Commands have no archive or paging of their own; they lead the first page.
    let mut commands_detail = None;
    let mut commands = Vec::new();
    if keeps_terminals(&parent) && query.archived != Some(true) && query.cursor.is_none() {
        match codex_commands(&parent).await {
            Ok((rows, more)) => {
                commands = rows;
                commands_detail = more.then_some("More commands are running than Cupola shows.");
            }
            // An unanswered lookup is not "none running": say so beside the tasks.
            Err(()) => commands_detail = Some(COMMANDS_UNCHECKED),
        }
    }
    let linked = if query.archived != Some(true) && query.cursor.is_none() {
        linked_tasks(state, &parent).await
    } else {
        Vec::new()
    };
    let parent = &parent;
    let children = stream::iter(children)
        .map(|(thread, archived)| async move {
            let thread =
                subagents::with_lifecycle_status(thread, &parent.rpc, &state.codex_home).await;
            verified_child(parent, &thread, archived)
        })
        .buffered(4)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten();
    let detail = [
        (next_current_cursor.is_some() || next_archived_cursor.is_some())
            .then_some("More agent tasks are available."),
        commands_detail,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    Json(ProjectSubagentList {
        available: true,
        detail: (!detail.is_empty()).then_some(detail),
        subagents: commands.into_iter().chain(children).chain(linked).collect(),
        next_current_cursor,
        next_archived_cursor,
    })
    .into_response()
}

/// Tasks that are not the provider's own subagents but belong to the thread:
/// threads Cupola delegated from it (`cupola delegate`, `wonder_delegate`) and
/// subtasks T3 Code reports under it. Best effort: a lookup that fails leaves
/// those rows out rather than the provider's own tasks.
async fn linked_tasks(state: &AppState, parent: &ProjectParent) -> Vec<ProjectSubagentSummary> {
    let mut rows = delegated_rows(state, &parent.conversation.conversation_id).await;
    rows.extend(t3_rows(state, &parent.conversation).await);
    rows
}

/// One thread Cupola delegated from `parent_id`, as an agent task row.
async fn delegated_row(
    state: &AppState,
    parent_id: &str,
    child: String,
) -> Option<ProjectSubagentSummary> {
    let conversation = state.store.project_conversation(&child).await.ok()??;
    let run = state
        .store
        .thread_run_state(&child)
        .await
        .unwrap_or_default();
    let status = if run.question {
        "waitingOnUserInput"
    } else if run.waiting {
        "waitingOnApproval"
    } else if run.working || run.queued > 0 {
        "active"
    } else if let Some(result) = state.store.delegation_result(&child).await.ok().flatten() {
        match result.outcome.as_str() {
            "failed" => "failed",
            "stopped" => "interrupted",
            _ => "completed",
        }
    } else {
        // No recorded result yet: read the child's newest message, so a turn
        // that failed is not shown as completed and one not started is not done.
        match state
            .store
            .latest_message_state(&child)
            .await
            .ok()
            .flatten()
            .as_deref()
        {
            Some("completed") => "completed",
            Some("interrupted") => "interrupted",
            None => "active",
            Some(_) => "failed",
        }
    };
    Some(ProjectSubagentSummary {
        parent_conversation_id: parent_id.to_owned(),
        thread_id: child,
        title: conversation.title,
        agent_nickname: None,
        agent_role: None,
        status: status.into(),
        is_archived: false,
        can_accept_direct_input: true,
        named_subagents: false,
        kind: Some("agent".into()),
        can_stop: false,
        running_elsewhere: false,
        updated_at: Some(conversation.last_activity_at),
        duration_seconds: None,
        current_activity: None,
        progress_summary: None,
    })
}

/// Threads Cupola delegated from `parent_id`; they need only Cupola's own
/// store, so they list even when the parent's provider runtime does not.
async fn delegated_rows(state: &AppState, parent_id: &str) -> Vec<ProjectSubagentSummary> {
    let mut rows = Vec::new();
    for child in state
        .store
        .thread_delegation_children(parent_id)
        .await
        .unwrap_or_default()
    {
        rows.extend(delegated_row(state, parent_id, child).await);
    }
    rows
}

/// T3 Code's own subtasks of the parent. Bounded separately so a slow T3 Code
/// leaves these rows out instead of failing the whole roster.
pub(crate) async fn t3_rows(
    state: &AppState,
    conversation: &wonder_store::StoredProjectConversation,
) -> Vec<ProjectSubagentSummary> {
    let parent_id = &conversation.conversation_id;
    let mut rows = Vec::new();
    if let Some(t3) = crate::t3_code::route(state, conversation).await {
        let found = state
            .t3_code
            .subtasks(
                &t3,
                &conversation.cwd,
                std::time::Duration::from_millis(1500),
            )
            .await
            .map(|subtasks| subtasks.found)
            .unwrap_or_default();
        rows.extend(found.iter().filter_map(|child| t3_row(parent_id, child)));
    }
    rows
}

/// One T3 Code subtask (a `delegate_task` child) as an agent task row.
fn t3_row(parent_id: &str, child: &Value) -> Option<ProjectSubagentSummary> {
    let id = child.get("threadId").and_then(Value::as_str)?;
    let text = |key: &str| {
        child
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    };
    let status = match text("status").as_deref() {
        Some("running" | "active" | "working") => "active",
        Some("failed" | "error") => "failed",
        Some("interrupted" | "stopped") => "interrupted",
        Some("waiting") => "waitingOnApproval",
        _ => "completed",
    };
    Some(ProjectSubagentSummary {
        parent_conversation_id: parent_id.to_owned(),
        thread_id: id.to_owned(),
        title: text("title").unwrap_or_else(|| "T3 Code task".into()),
        agent_nickname: None,
        agent_role: text("relationshipToParent"),
        status: status.into(),
        is_archived: false,
        can_accept_direct_input: false,
        named_subagents: false,
        kind: Some("t3Task".into()),
        can_stop: false,
        running_elsewhere: true,
        updated_at: text("updatedAt"),
        duration_seconds: None,
        current_activity: None,
        progress_summary: None,
    })
}

/// A T3 Code subtask opens from T3 Code's own timeline of it (`t3_thread_read`),
/// once the parent's subtask list names it. The client names the row's kind,
/// so every outcome is an answer: the child's messages, or the plain reason
/// T3 Code can't be read (a 503 the sheet shows as it is).
async fn t3_transcript(state: &AppState, conversation_id: &str, thread_id: &str) -> Response {
    let Some(conversation) = state
        .store
        .project_conversation(conversation_id)
        .await
        .ok()
        .flatten()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !parent_included(state, conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state.t3_code.connected() {
        return unavailable(
            "T3 Code is not running or not connected to Cupola, so this task can't be read here.",
        );
    }
    let Some(t3) = crate::t3_code::route(state, &conversation).await else {
        return unavailable(
            "T3 Code no longer has this chat open, so this task can't be read here. Open it in T3 Code.",
        );
    };
    let listed = state
        .t3_code
        .subtasks(
            &t3,
            &conversation.cwd,
            std::time::Duration::from_millis(3000),
        )
        .await;
    let Ok(listed) = listed else {
        return unavailable("T3 Code did not answer in time, so this task can't be read here.");
    };
    let Some(mut row) = listed
        .found
        .iter()
        .filter(|child| child.get("threadId").and_then(Value::as_str) == Some(thread_id))
        .find_map(|child| t3_row(conversation_id, child))
    else {
        if !listed.complete {
            return unavailable("T3 Code did not answer in time, so this task can't be read here.");
        }
        return unavailable("T3 Code no longer lists this task under this chat.");
    };
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.t3_code.thread_messages(thread_id),
    )
    .await;
    let (detail, messages) = match read {
        Ok(Ok(read)) => read,
        Ok(Err(crate::t3_code::CallError::Refused { .. })) => {
            return unavailable("T3 Code has no readable record of this task any more.")
        }
        _ => {
            return unavailable(
                "T3 Code did not answer, so this task's messages can't be read here.",
            )
        }
    };
    if detail.get("activeRunId").and_then(Value::as_str).is_some() {
        row.status = "active".into();
    }
    let items: Vec<history::AppServerThreadItem> = messages
        .iter()
        // What the child inherited from its parent is not its own work.
        .filter(|message| message.get("visibility").and_then(Value::as_str) != Some("inherited"))
        .filter_map(|message| {
            let mut text = message.get("text").and_then(Value::as_str)?.to_owned();
            let position = message.get("position").and_then(Value::as_i64)?;
            if message.get("textTruncated").and_then(Value::as_bool) == Some(true) {
                text.push_str("\n\n(T3 Code shortened this message.)");
            }
            let id = format!("t3:{thread_id}:{position}");
            let item = match message.get("type").and_then(Value::as_str)? {
                "user_message" => {
                    json!({"type": "userMessage", "id": id, "content": [{"type": "text", "text": text}]})
                }
                "assistant_message" | "proposed_plan" => {
                    json!({"type": "agentMessage", "id": id, "text": text})
                }
                _ => return None,
            };
            Some(history::AppServerThreadItem {
                turn_id: thread_id.to_owned(),
                item,
            })
        })
        .collect();
    let mut projection = history::conversation_thread_projection_with_items(
        Some(thread_id.to_owned()),
        &[],
        &[],
        &[],
        &items,
        Some(&conversation.cwd),
    );
    if !history::shows_thinking(state, conversation_id).await {
        history::hide_thinking(&mut projection);
    }
    Json(ProjectSubagentTranscript {
        subagent: row,
        snapshot: history::ConversationSnapshot {
            conversation_id: format!("project-agent:{conversation_id}:{thread_id}"),
            host_epoch: state.host_epoch.clone(),
            last_sequence: 0,
            history_revision: 0,
            codex_thread_id: Some(thread_id.to_owned()),
            messages: Vec::new(),
            assistant_messages: Vec::new(),
            thread: projection,
            events: Vec::new(),
            message_records: Vec::new(),
        },
    })
    .into_response()
}

pub(crate) async fn transcript(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path((conversation_id, thread_id)): Path<(String, String)>,
    Query(query): Query<TranscriptQuery>,
) -> Response {
    if thread_id.is_empty()
        || thread_id.len() > 256
        || query
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2048)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // A thread Cupola delegated from this one opens as its own conversation.
    if let Ok(Some(link)) = state.store.thread_delegation(&thread_id).await {
        if link.parent_conversation_id == conversation_id
            && parent_included(&state, &conversation_id).await
        {
            let Some(row) = delegated_row(&state, &conversation_id, thread_id.clone()).await else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let response = history::conversation_snapshot(
                State(state.clone()),
                Path(thread_id.clone()),
                Query(history::HistoryQuery::default()),
                axum::http::HeaderMap::new(),
            )
            .await;
            if response.status() != StatusCode::OK {
                return response;
            }
            let Ok(body) = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024).await
            else {
                return unavailable("Agent task history could not be loaded.");
            };
            let Ok(snapshot) = serde_json::from_slice::<Value>(&body) else {
                return unavailable("Agent task history could not be loaded.");
            };
            return Json(json!({"subagent": row, "snapshot": snapshot})).into_response();
        }
    }
    if query.kind.as_deref() == Some("t3Task") {
        return t3_transcript(&state, &conversation_id, &thread_id).await;
    }
    let parent = match parent(&state, &conversation_id).await {
        Ok(parent) => parent,
        Err(response) => return *response,
    };
    if transcript_tasks(&parent) {
        return claude_transcript(&state, &parent, &conversation_id, &thread_id).await;
    }
    // A background command has no conversation of its own.
    if thread_id.starts_with(COMMAND_ID_PREFIX) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let read = match request(
        &parent.rpc,
        "thread/read",
        json!({"threadId": thread_id, "includeTurns": false}),
    )
    .await
    {
        Ok(read) => read,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let thread = read.get("thread").unwrap_or(&read);
    if thread.get("id").and_then(Value::as_str) != Some(thread_id.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(mut child) = verified_child(&parent, thread, false) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let lifecycle =
        subagents::with_lifecycle_status(thread.clone(), &parent.rpc, &state.codex_home).await;
    child.status = subagents::lifecycle_status(&lifecycle, None);
    child.running_elsewhere =
        lifecycle.get("runningElsewhere").and_then(Value::as_bool) == Some(true);
    child.is_archived = match child_archived(&parent, &thread_id).await {
        Ok(archived) => archived,
        Err(response) => return *response,
    };
    let result = match request(
        &parent.rpc,
        "thread/items/list",
        json!({
            "threadId": thread_id,
            "limit": PAGE_SIZE,
            "sortDirection": "desc",
            "cursor": query.cursor,
        }),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return unavailable("Agent task history could not be loaded."),
    };
    let Some(entries) = result.get("data").and_then(Value::as_array) else {
        return unavailable("Codex returned invalid agent task history.");
    };
    if entries.len() > PAGE_SIZE {
        return unavailable("Codex returned too much agent task history.");
    }
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries.iter().rev() {
        let (Some(turn_id), Some(item)) = (
            entry
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty()),
            entry.get("item").filter(|item| item.is_object()),
        ) else {
            return unavailable("Codex returned invalid agent task history.");
        };
        items.push(history::app_server_entry_item(turn_id, entry, item));
    }
    let next_cursor = match result.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) if !cursor.is_empty() && cursor.len() <= 2048 => {
            Some(cursor.clone())
        }
        _ => return unavailable("Codex returned an invalid history cursor."),
    };
    let mut projection = history::conversation_thread_projection_with_items(
        Some(thread_id.clone()),
        &[],
        &[],
        &[],
        &items,
        Some(&parent.conversation.cwd),
    );
    if !history::shows_thinking(&state, &conversation_id).await {
        history::hide_thinking(&mut projection);
    }
    projection.next_cursor = next_cursor;
    Json(ProjectSubagentTranscript {
        subagent: child,
        snapshot: history::ConversationSnapshot {
            conversation_id: format!("project-agent:{}:{}", conversation_id, thread_id),
            host_epoch: state.host_epoch,
            last_sequence: 0,
            history_revision: 0,
            codex_thread_id: Some(thread_id),
            messages: Vec::new(),
            assistant_messages: Vec::new(),
            thread: projection,
            events: Vec::new(),
            message_records: Vec::new(),
        },
    })
    .into_response()
}

/// Stops one running Claude agent task or background command, or a command
/// Codex left running. Ownership is rechecked like every Project read; the
/// bridge decides whether Wonder is driving a Claude session and answers each
/// case in user language.
pub(crate) async fn stop(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path((conversation_id, thread_id)): Path<(String, String)>,
) -> Response {
    if thread_id.is_empty() || thread_id.len() > 256 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let parent = match parent(&state, &conversation_id).await {
        Ok(parent) => parent,
        Err(response) => return *response,
    };
    if keeps_terminals(&parent) {
        if let Some(process_id) = thread_id
            .strip_prefix(COMMAND_ID_PREFIX)
            .filter(|id| !id.is_empty())
        {
            return stop_codex_command(&parent, process_id).await;
        }
    }
    if !transcript_tasks(&parent) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Cupola can't stop this kind of agent task.",
        )
            .into_response();
    }
    let result = match request(
        &parent.rpc,
        "thread/backgroundTask/stop",
        json!({"threadId": parent.thread_id, "taskId": thread_id}),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return unavailable("The task could not be stopped. Try again."),
    };
    let summary = result
        .get("task")
        .and_then(|task| {
            claude_task_summary(
                &parent.conversation.conversation_id,
                &parent.conversation.cwd,
                task,
            )
        })
        .filter(|task| task.thread_id == thread_id);
    match (result.get("outcome").and_then(Value::as_str), summary) {
        (Some("stopped"), Some(task)) => Json(task).into_response(),
        (Some("notFound"), _) => StatusCode::NOT_FOUND.into_response(),
        (Some("runningElsewhere"), _) => (
            StatusCode::CONFLICT,
            "This task is running in Claude on your computer. Stop it there.",
        )
            .into_response(),
        (Some("notRunning"), _) => (
            StatusCode::CONFLICT,
            "This task is no longer running. Refresh to see its latest state.",
        )
            .into_response(),
        (Some("unsupported"), _) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Cupola can't stop this task. It may have been started outside this session.",
        )
            .into_response(),
        _ => unavailable("Claude returned an invalid answer. Try again."),
    }
}

const COMMAND_ALREADY_STOPPED: &str =
    "This command has already stopped. Refresh to see what's still running.";

/// Stops one command Codex left running in this thread. The runtime scopes the
/// process to the verified thread; a process it no longer has has stopped.
async fn stop_codex_command(parent: &ProjectParent, process_id: &str) -> Response {
    let Ok((terminals, more)) = codex_terminals(parent).await else {
        return unavailable("The command could not be checked. Try again.");
    };
    let listed = terminals
        .iter()
        .find(|terminal| terminal.get("processId").and_then(Value::as_str) == Some(process_id));
    if listed.is_none() && !more {
        return (StatusCode::CONFLICT, COMMAND_ALREADY_STOPPED).into_response();
    }
    let fallback = json!({"processId": process_id, "command": "Background command"});
    let Some(stopped) = codex_command_summary(
        &parent.conversation.conversation_id,
        listed.unwrap_or(&fallback),
        "interrupted",
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let response = match parent
        .rpc
        .request(
            "thread/backgroundTerminals/terminate",
            json!({"threadId": parent.thread_id, "processId": process_id}),
        )
        .await
    {
        Ok(response) => response,
        Err(_) => return unavailable("The command could not be stopped. Try again."),
    };
    match (response.error, response.result) {
        (Some(error), _) if error.code == -32601 => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "The Codex version on your computer can't stop commands from Cupola.",
        )
            .into_response(),
        (None, Some(result)) if result.get("terminated").and_then(Value::as_bool) == Some(true) => {
            Json(stopped).into_response()
        }
        _ => (StatusCode::CONFLICT, COMMAND_ALREADY_STOPPED).into_response(),
    }
}

/// Commands Codex left running in the Project thread, and whether more exist
/// than were read.
async fn codex_commands(parent: &ProjectParent) -> Result<(Vec<ProjectSubagentSummary>, bool), ()> {
    let (terminals, more) = codex_terminals(parent).await?;
    Ok((
        terminals
            .iter()
            .filter_map(|terminal| {
                codex_command_summary(&parent.conversation.conversation_id, terminal, "running")
            })
            .collect(),
        more,
    ))
}

/// Every page of the thread's background commands, bounded. Only a thread
/// loaded in Cupola's own runtime can hold commands Cupola sees and stops; an
/// unanswered or malformed read is an error, never "none".
async fn codex_terminals(parent: &ProjectParent) -> Result<(Vec<Value>, bool), ()> {
    if !codex_thread_loaded(parent).await? {
        return Ok((Vec::new(), false));
    }
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    for _ in 0..MAX_COMMAND_PAGES {
        let page = request(
            &parent.rpc,
            "thread/backgroundTerminals/list",
            json!({"threadId": parent.thread_id, "cursor": cursor}),
        )
        .await
        .map_err(|_| ())?;
        rows.extend(
            page.get("data")
                .and_then(Value::as_array)
                .ok_or(())?
                .iter()
                .cloned(),
        );
        if rows.len() > MAX_COMMANDS {
            rows.truncate(MAX_COMMANDS);
            return Ok((rows, true));
        }
        cursor = match page.get("nextCursor") {
            None | Some(Value::Null) => return Ok((rows, false)),
            Some(Value::String(next))
                if !next.is_empty() && next.len() <= 2048 && seen_cursors.insert(next.clone()) =>
            {
                Some(next.clone())
            }
            _ => return Err(()),
        };
    }
    Ok((rows, true))
}

async fn codex_thread_loaded(parent: &ProjectParent) -> Result<bool, ()> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    for _ in 0..MAX_COMMAND_PAGES {
        let page = request(&parent.rpc, "thread/loaded/list", json!({"cursor": cursor}))
            .await
            .map_err(|_| ())?;
        let loaded = page.get("data").and_then(Value::as_array).ok_or(())?;
        if loaded.iter().any(|row| {
            row.as_str()
                .or_else(|| row.get("id").and_then(Value::as_str))
                == Some(parent.thread_id.as_str())
        }) {
            return Ok(true);
        }
        cursor = match page.get("nextCursor") {
            None | Some(Value::Null) => return Ok(false),
            Some(Value::String(next))
                if !next.is_empty() && next.len() <= 2048 && seen_cursors.insert(next.clone()) =>
            {
                Some(next.clone())
            }
            _ => return Err(()),
        };
    }
    Err(())
}

/// One background command as a roster row titled by its command line.
fn codex_command_summary(
    parent_conversation_id: &str,
    terminal: &Value,
    status: &str,
) -> Option<ProjectSubagentSummary> {
    let process_id = terminal
        .get("processId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() + COMMAND_ID_PREFIX.len() <= 256)?;
    let title = command_title(terminal.get("command").and_then(Value::as_str)?)?;
    let running = status == "running";
    Some(ProjectSubagentSummary {
        parent_conversation_id: parent_conversation_id.to_owned(),
        thread_id: format!("{COMMAND_ID_PREFIX}{process_id}"),
        title,
        agent_nickname: None,
        agent_role: Some("Background command".into()),
        status: status.to_owned(),
        is_archived: false,
        can_accept_direct_input: false,
        // A command has no agent name; the app shows a status glyph instead.
        named_subagents: false,
        kind: Some("command".into()),
        // Listed only from Cupola's own runtime, which can stop it.
        can_stop: running,
        running_elsewhere: false,
        updated_at: None,
        duration_seconds: None,
        current_activity: None,
        progress_summary: None,
    })
}

/// The command's first non-blank line, whitespace collapsed, with an ellipsis
/// when more lines or characters follow.
fn command_title(command: &str) -> Option<String> {
    const MAX_TITLE: usize = 200;
    let mut lines = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let line = lines
        .next()?
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let truncated = line.chars().count() > MAX_TITLE;
    let mut title: String = line.chars().take(MAX_TITLE).collect();
    if truncated || lines.next().is_some() {
        title.push('…');
    }
    Some(title)
}

fn claude_task_summary(
    parent_conversation_id: &str,
    workspace: &str,
    task: &Value,
) -> Option<ProjectSubagentSummary> {
    let provider = AgentFamily::Claude.provider();
    let id = task
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256)?;
    let title = task
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())?;
    let kind = task.get("kind").and_then(Value::as_str)?;
    let role = match kind {
        "agent" => task
            .get("role")
            .and_then(Value::as_str)
            .filter(|role| !role.trim().is_empty())
            .unwrap_or("Agent"),
        "command" => "Background command",
        "monitor" => "Monitor",
        "task" => "Background task",
        _ => return None,
    };
    let status = match task.get("status").and_then(Value::as_str)? {
        status @ ("running" | "completed" | "failed" | "interrupted") => status,
        _ => "unknown",
    };
    let flag = |name: &str| task.get(name).and_then(Value::as_bool) == Some(true);
    Some(ProjectSubagentSummary {
        parent_conversation_id: parent_conversation_id.to_owned(),
        thread_id: id.to_owned(),
        title: title.chars().take(200).collect(),
        agent_nickname: None,
        agent_role: Some(role.chars().take(80).collect()),
        status: status.to_owned(),
        is_archived: false,
        can_accept_direct_input: false,
        named_subagents: provider.capabilities.named_subagents,
        kind: Some(kind.to_owned()),
        can_stop: status == "running" && flag("canStop"),
        running_elsewhere: status == "running" && flag("runningElsewhere"),
        updated_at: task
            .get("updatedAt")
            .and_then(Value::as_str)
            .filter(|at| !at.is_empty() && at.len() <= 64)
            .map(str::to_owned),
        current_activity: (status == "running")
            .then(|| task.get("currentActivity"))
            .flatten()
            .and_then(|item| {
                history::compact_current_activity(history::typed_thread_item_projection(
                    &history::AppServerThreadItem {
                        turn_id: id.to_owned(),
                        item: item.clone(),
                    },
                    "",
                    Some(workspace),
                ))
            }),
        progress_summary: (status == "running")
            .then(|| task.get("progressSummary").and_then(Value::as_str))
            .flatten()
            .map(|summary| {
                summary
                    .chars()
                    .take(2048)
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(240)
                    .collect::<String>()
            })
            .filter(|summary| !summary.is_empty()),
        duration_seconds: (status != "running")
            .then(|| task.get("durationSeconds").and_then(Value::as_u64))
            .flatten(),
    })
}

async fn claude_tasks(
    parent: &ProjectParent,
) -> Result<Vec<ProjectSubagentSummary>, Box<Response>> {
    let result = request(
        &parent.rpc,
        "thread/backgroundTasks/list",
        json!({"threadId": parent.thread_id}),
    )
    .await
    .map_err(|_| Box::new(unavailable("Agent tasks could not be loaded. Try again.")))?;
    let tasks = result
        .get("data")
        .and_then(Value::as_array)
        .filter(|tasks| tasks.len() <= MAX_CLAUDE_TASKS)
        .ok_or_else(|| Box::new(unavailable("Claude returned an invalid agent task list.")))?;
    Ok(tasks
        .iter()
        .filter_map(|task| {
            claude_task_summary(
                &parent.conversation.conversation_id,
                &parent.conversation.cwd,
                task,
            )
        })
        .collect())
}

/// The agent's own work, shaped like Codex's: its messages plus the commands,
/// file changes, tool calls and plans it ran. Items without an ID, or messages
/// without text, have nothing to show.
fn claude_transcript_items(task_id: &str, entries: &[Value]) -> Vec<history::AppServerThreadItem> {
    entries
        .iter()
        .filter(|item| {
            item.get("id").and_then(Value::as_str).is_some()
                && match item.get("type").and_then(Value::as_str) {
                    Some("agentMessage") => item.get("text").and_then(Value::as_str).is_some(),
                    Some("commandExecution" | "fileChange" | "mcpToolCall" | "plan") => true,
                    _ => false,
                }
        })
        .map(|item| history::AppServerThreadItem {
            turn_id: task_id.to_owned(),
            item: item.clone(),
        })
        .collect()
}

async fn claude_transcript(
    state: &AppState,
    parent: &ProjectParent,
    conversation_id: &str,
    task_id: &str,
) -> Response {
    let result = match request(
        &parent.rpc,
        "thread/backgroundTask/read",
        json!({"threadId": parent.thread_id, "taskId": task_id}),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let Some(child) = result
        .get("task")
        .and_then(|task| {
            claude_task_summary(
                &parent.conversation.conversation_id,
                &parent.conversation.cwd,
                task,
            )
        })
        .filter(|child| child.thread_id == task_id)
    else {
        return unavailable("Claude returned an invalid agent task.");
    };
    let Some(entries) = result
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= CLAUDE_TASK_ITEMS)
    else {
        return unavailable("Claude returned invalid agent task history.");
    };
    let items = claude_transcript_items(task_id, entries);
    let projection = history::conversation_thread_projection_with_items(
        Some(task_id.to_owned()),
        &[],
        &[],
        &[],
        &items,
        Some(&parent.conversation.cwd),
    );
    Json(ProjectSubagentTranscript {
        subagent: child,
        snapshot: history::ConversationSnapshot {
            conversation_id: format!("project-agent:{}:{}", conversation_id, task_id),
            host_epoch: state.host_epoch.clone(),
            last_sequence: 0,
            history_revision: 0,
            codex_thread_id: Some(task_id.to_owned()),
            messages: Vec::new(),
            assistant_messages: Vec::new(),
            thread: projection,
            events: Vec::new(),
            message_records: Vec::new(),
        },
    })
    .into_response()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // A Claude Project conversation stores the Claude Code session; its binding
    // names Wonder's bridge thread. Both families must accept their own shape.
    #[test]
    fn project_bindings_match_each_family_identity() {
        let binding = |family, thread: &str, session: Option<&str>| wonder_store::RuntimeBinding {
            conversation_id: "chat".into(),
            family,
            thread_id: thread.into(),
            session_id: session.map(str::to_owned),
            execution_scope: "projects".into(),
        };
        let claude = binding(AgentFamily::Claude, "claude-bridge", Some("native-session"));
        assert!(binding_matches(
            AgentFamily::Claude,
            "native-session",
            &claude
        ));
        assert!(!binding_matches(
            AgentFamily::Claude,
            "other-session",
            &claude
        ));
        assert!(!binding_matches(
            AgentFamily::Codex,
            "native-session",
            &claude
        ));
        let codex = binding(AgentFamily::Codex, "thread", None);
        assert!(binding_matches(AgentFamily::Codex, "thread", &codex));
        assert!(!binding_matches(AgentFamily::Codex, "other", &codex));
        let mut project = codex.clone();
        project.execution_scope = "unsupported".into();
        assert!(!binding_matches(AgentFamily::Codex, "thread", &project));
    }

    // A T3 Code delegate child lists as a `t3Task`, running on T3 Code's side,
    // and carries the relationship T3 Code reports; a row without an ID is dropped.
    #[test]
    fn t3_children_map_to_openable_rows() {
        let done = t3_row(
            "chat",
            &json!({"threadId":"t3-child","title":"Duo feedback videos","status":"completed","relationshipToParent":"subagent"}),
        )
        .unwrap();
        assert_eq!(
            (
                done.kind.as_deref(),
                done.status.as_str(),
                done.agent_role.as_deref()
            ),
            (Some("t3Task"), "completed", Some("subagent"))
        );
        let running = t3_row("chat", &json!({"threadId":"t3-child","status":"running"})).unwrap();
        assert_eq!(
            (running.status.as_str(), running.title.as_str()),
            ("active", "T3 Code task")
        );
        assert!(t3_row("chat", &json!({"title":"no id"})).is_none());
    }

    // Claude agent tasks and background commands share the read-only roster;
    // unknown kinds are dropped and unrecognized states are not claimed.
    #[test]
    fn claude_tasks_map_to_read_only_roster_rows() {
        let row = |task: Value| claude_task_summary("chat", "/workspace/project", &task);
        let command = row(
            json!({"id":"toolu_1","kind":"command","title":"Build the app","status":"running"}),
        )
        .unwrap();
        assert_eq!(
            (command.agent_role.as_deref(), command.status.as_str()),
            (Some("Background command"), "running")
        );
        assert!(!command.can_accept_direct_input);
        // Claude subagents carry no names, so the app shows no avatar for them.
        assert!(!command.named_subagents);
        assert_eq!(command.kind.as_deref(), Some("command"));
        assert!(!command.can_stop, "Stop needs the bridge to confirm it");
        let stoppable = row(json!({"id":"toolu_5","kind":"agent","title":"Run","status":"running","canStop":true,"updatedAt":"2026-10-08T00:00:00Z"})).unwrap();
        assert!(stoppable.can_stop);
        assert_eq!(
            stoppable.updated_at.as_deref(),
            Some("2026-10-08T00:00:00Z")
        );
        let finished = row(json!({"id":"toolu_6","kind":"agent","title":"Done","status":"completed","canStop":true,"runningElsewhere":true})).unwrap();
        assert!(
            !finished.can_stop && !finished.running_elsewhere,
            "only running tasks are controllable"
        );
        let agent = row(json!({"id":"toolu_2","kind":"agent","title":"Review","role":"Explore","status":"completed"})).unwrap();
        assert_eq!(agent.agent_role.as_deref(), Some("Explore"));
        assert_eq!(
            row(json!({"id":"toolu_3","kind":"agent","title":"Old","status":"lost"}))
                .unwrap()
                .status,
            "unknown"
        );
        assert!(
            row(json!({"id":"toolu_4","kind":"shell","title":"x","status":"running"})).is_none()
        );
        assert!(row(json!({"id":"","kind":"command","title":"x","status":"running"})).is_none());
        // A finished run says how long it took; a running one has no duration yet.
        let ran = |status: &str| {
            row(json!({"id":"b","kind":"command","title":"ls","status":status,"durationSeconds":294}))
                .unwrap()
                .duration_seconds
        };
        assert_eq!((ran("completed"), ran("running")), (Some(294), None));
        for status in ["running", "completed", "failed", "interrupted", "unknown"] {
            let task = row(json!({"id":"current","kind":"agent","title":"Review","status":status,
                "progressSummary":"  Comparing\nrelease requirements  ","summary":"Final report stays separate",
                "currentActivity":{"id":"read","type":"mcpToolCall","status":"inProgress","server":"Claude","tool":"Read",
                    "toolAction":{"kind":"read","paths":["/workspace/project/Sources/AppShell.swift", "/private/outside.txt"]},"arguments":{"secret":"private"}}})).unwrap();
            assert_eq!(task.current_activity.is_some(), status == "running");
            assert_eq!(
                task.progress_summary.as_deref(),
                (status == "running").then_some("Comparing release requirements")
            );
            if let Some(activity) = task.current_activity {
                assert_eq!(activity["payload"]["toolAction"]["kind"], "read");
                assert_eq!(
                    activity["payload"]["toolAction"]["paths"],
                    json!(["Sources/AppShell.swift", "[path outside workspace]"])
                );
                assert!(activity["payload"].get("arguments").is_none());
            }
        }
        for summary in ["", " \n\t", "x"] {
            let task = row(json!({"id":"mcp","kind":"task","title":"Deploy","status":"running","progressSummary":summary})).unwrap();
            assert_eq!(task.progress_summary.is_some(), summary == "x");
        }
        let bounded = row(json!({"id":"mcp","kind":"task","title":"Deploy","status":"running","progressSummary":"é".repeat(3000)})).unwrap();
        assert_eq!(bounded.progress_summary.unwrap().chars().count(), 240);
        // Monitors and other background tasks Claude lists.
        for (kind, role) in [("monitor", "Monitor"), ("task", "Background task")] {
            let task = row(json!({"id":"t","kind":kind,"title":"x","status":"running"})).unwrap();
            assert_eq!(
                (task.kind.as_deref(), task.agent_role.as_deref()),
                (Some(kind), Some(role))
            );
        }
    }

    // An agent task's transcript keeps the commands and edits the agent ran
    // beside its messages, in order; internal or malformed entries are dropped.
    #[test]
    fn claude_agent_transcript_keeps_tool_work_in_order() {
        let entries = [
            json!({"type":"agentMessage","id":"m1","text":"Looking","status":"completed"}),
            json!({"type":"commandExecution","id":"c1","command":"ls","status":"completed","success":true,"aggregatedOutput":"a"}),
            json!({"type":"fileChange","id":"f1","status":"failed","changes":[]}),
            json!({"type":"mcpToolCall","id":"t1","server":"Claude","tool":"Grep","status":"inProgress"}),
            json!({"type":"userMessage","id":"u1","content":[]}),
            json!({"type":"commandExecution","command":"no id"}),
            json!({"type":"agentMessage","id":"m2"}),
        ];
        let items = claude_transcript_items("task", &entries);
        let ids: Vec<_> = items
            .iter()
            .map(|i| i.item["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["m1", "c1", "f1", "t1"]);
        assert!(items.iter().all(|i| i.turn_id == "task"));
    }

    async fn body(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    // A running agent task's thinking shows only that it is thinking: its
    // partial summary never reaches the transcript at any detail level. A
    // finished one shows its summary only at the Thinking level.
    #[tokio::test]
    async fn running_agent_task_transcript_keeps_partial_thinking_back() {
        let (dir, state, _) = crate::projects::tests::handoff_fixture().await;
        let cwd = dir
            .path()
            .join("project-source")
            .to_string_lossy()
            .into_owned();
        std::fs::write(dir.path().join("project-subagents-fixture.json"), json!({
            "threads":{
                "thread":{"id":"thread","cwd":cwd,"parentThreadId":null},
                "child-thread":{
                    "id":"child-thread", "parentThreadId":"thread", "cwd":cwd,
                    "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thread","depth":1}}},
                    "status":{"type":"active","activeFlags":[]}
                }
            },
            "turns":{"child-thread":[{"id":"child-turn","status":"inProgress","completedAt":null}]},
            "items":{"child-thread":[
                {"turnId":"child-turn","startedAtMs":1000,"completedAtMs":2000,
                    "item":{"id":"done","type":"reasoning","summary":["Finished plan"]}},
                {"turnId":"child-turn","startedAtMs":3000,"completedAtMs":null,
                    "item":{"id":"running","type":"reasoning","summary":["Partial plan"]}},
                {"turnId":"child-turn",
                    "item":{"id":"unclocked","type":"reasoning","summary":["Unclocked plan"]}}
            ]}
        }).to_string()).unwrap();
        let read = || async {
            let (status, read) = body(
                transcript(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path(("project-chat".into(), "child-thread".into())),
                    Query(TranscriptQuery::default()),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{read}");
            crate::tests::validate_http_contract("projectSubagentTranscript", &read);
            read
        };
        let row = |read: &Value, id: &str| {
            read["snapshot"]["thread"]["turns"][0]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .cloned()
                .unwrap()
        };

        let hidden = read().await;
        assert_eq!(hidden["subagent"]["status"], "active");
        assert_eq!(row(&hidden, "running")["state"], "started");
        assert_eq!(row(&hidden, "done")["state"], "completed");
        assert!(!hidden.to_string().contains(" plan"), "{hidden}");

        state
            .store
            .set_agent_chat_detail(
                wonder_store::AgentFamily::Codex,
                wonder_store::ChatDetail::Thinking,
                "now",
            )
            .await
            .unwrap();
        let shown = read().await;
        assert_eq!(row(&shown, "done")["text"], "Finished plan");
        assert_eq!(
            row(&shown, "done")["payload"]["summary"],
            json!(["Finished plan"])
        );
        let running = row(&shown, "running");
        assert_eq!(running["state"], "started");
        assert!(running.get("text").is_none_or(Value::is_null), "{running}");
        assert!(running["payload"].get("summary").is_none(), "{running}");
        let text = shown.to_string();
        assert!(
            !text.contains("Partial plan") && !text.contains("Unclocked plan"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn project_child_history_requires_exact_spawn_and_current_project_roots() {
        let (dir, state, _) = crate::projects::tests::handoff_fixture().await;
        let cwd = dir
            .path()
            .join("project-source")
            .to_string_lossy()
            .into_owned();
        let child = json!({
            "id":"child-thread", "parentThreadId":"thread", "cwd":cwd,
            "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thread","depth":1,"agent_nickname":"Scout"}}},
            "status":{"type":"notLoaded"}, "isArchived":true
        });
        let forged = json!({
            "id":"forged-thread", "parentThreadId":"thread", "cwd":cwd,
            "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"other-thread","depth":1}}},
            "status":{"type":"idle"}
        });
        let outside = json!({
            "id":"outside-thread", "parentThreadId":"thread", "cwd":"/outside",
            "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thread","depth":1}}},
            "status":{"type":"idle"}
        });
        std::fs::write(dir.path().join("project-subagents-fixture.json"), json!({
            "listNextCursor":"older-tasks",
            "threads":{
                "thread":{"id":"thread","cwd":cwd,"parentThreadId":null},
                "child-thread":child,"forged-thread":forged,"outside-thread":outside
            },
            "turns":{"child-thread":[{"id":"child-turn","status":"completed","completedAt":1700000001}]},
            "items":{"child-thread":[
                {"turnId":"child-turn","item":{"id":"answer-item","type":"agentMessage","text":"Found the issue"}},
                {"turnId":"child-turn","item":{"id":"user-item","type":"userMessage","content":[{"type":"text","text":"Investigate the parser"}]}}
            ]}
        }).to_string()).unwrap();

        let (status, roster) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        crate::tests::validate_http_contract("projectSubagentList", &roster);
        assert_eq!(roster["subagents"].as_array().unwrap().len(), 1, "{roster}");
        assert_eq!(roster["subagents"][0]["threadId"], "child-thread");
        assert_eq!(roster["subagents"][0]["status"], "completed");
        assert_eq!(roster["subagents"][0]["isArchived"], true);
        assert_eq!(roster["subagents"][0]["canAcceptDirectInput"], false);
        assert!(roster["detail"]
            .as_str()
            .unwrap()
            .contains("More agent tasks"));
        assert_eq!(roster["nextCurrentCursor"], "older-tasks");

        let (status, read) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "child-thread".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        crate::tests::validate_http_contract("projectSubagentTranscript", &read);
        assert_eq!(read["subagent"]["isArchived"], true);
        assert_eq!(read["subagent"]["status"], "completed");
        assert_eq!(read["snapshot"]["thread"]["threadId"], "child-thread");
        assert_eq!(
            read["snapshot"]["thread"]["turns"][0]["items"][1]["text"],
            "Found the issue"
        );
        let fixture_path = dir.path().join("project-subagents-fixture.json");
        let mut fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture_path).unwrap()).unwrap();
        // The catalog's notLoaded is not an outcome. Native turns and active
        // flags must survive in both the roster and the opened child summary.
        for (native, turn, expected) in [
            (
                json!({"type":"notLoaded"}),
                json!({"id":"child-turn","status":"failed","completedAt":1700000001}),
                "failed",
            ),
            (
                json!({"type":"idle"}),
                json!({"id":"child-turn","status":"interrupted","completedAt":1700000001}),
                "interrupted",
            ),
            (
                json!({"type":"notLoaded"}),
                json!({"id":"child-turn","status":"interrupted","completedAt":null}),
                "unknown",
            ),
            (
                json!({"type":"notLoaded"}),
                json!({"id":"child-turn","status":"inProgress","completedAt":null}),
                "inProgress",
            ),
            (
                json!({"type":"active","activeFlags":["waitingOnApproval"]}),
                json!({"id":"child-turn","status":"completed"}),
                "waitingOnApproval",
            ),
            (
                json!({"type":"active","activeFlags":["waitingOnUserInput"]}),
                json!({"id":"child-turn","status":"completed"}),
                "waitingOnUserInput",
            ),
            (
                json!({"type":"systemError"}),
                json!({"id":"child-turn","status":"completed"}),
                "failed",
            ),
        ] {
            fixture["threads"]["child-thread"]["status"] = native;
            fixture["turns"]["child-thread"] = json!([turn]);
            std::fs::write(&fixture_path, fixture.to_string()).unwrap();
            let (status, roster) = body(
                list(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path("project-chat".into()),
                    Query(ListQuery::default()),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(roster["subagents"][0]["status"], expected);
        }
        // An unreadable latest turn is unknown, never a fabricated completion.
        fixture["threads"]["child-thread"]["status"] = json!({"type":"notLoaded"});
        fixture["turns"]["child-thread"] = json!({});
        std::fs::write(&fixture_path, fixture.to_string()).unwrap();
        let (_, roster) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(roster["subagents"][0]["status"], "unknown");
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        for line in requests.lines() {
            let request: Value = serde_json::from_str(line).unwrap();
            if request["method"] == "thread/turns/list" {
                assert!(!matches!(
                    request["params"]["threadId"].as_str(),
                    Some("forged-thread" | "outside-thread")
                ));
                assert_eq!(request["params"]["limit"], 1);
                assert_eq!(request["params"]["itemsView"], "notLoaded");
            }
        }
        // Reuse the indexed-lifecycle fixture that proves another Codex
        // process's unfinished turn isn't an interruption.
        let mut indexed =
            crate::codex_activity::tests::Fixture::new_for_thread("child-thread").await;
        let mut external_state = state.clone();
        external_state.codex_home = indexed.dir.path().to_owned();
        fixture["threads"]["child-thread"]["path"] = json!(indexed.path);
        fixture["turns"]["child-thread"] =
            json!([{"id":"turn","status":"interrupted","completedAt":null}]);
        std::fs::write(&fixture_path, fixture.to_string()).unwrap();
        let (_, roster) = body(
            list(
                State(external_state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(roster["subagents"][0]["status"], "inProgress");
        assert_eq!(roster["subagents"][0]["runningElsewhere"], true);
        indexed.finish("completed").await;
        let (_, roster) = body(
            list(
                State(external_state),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(roster["subagents"][0]["status"], "completed");
        assert_eq!(roster["subagents"][0]["runningElsewhere"], false);
        fixture["listPageSize"] = json!(1);
        fixture["threads"]["aaa-archived"] = json!({
            "id":"aaa-archived", "parentThreadId":"thread", "cwd":"/outside",
            "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thread","depth":1}}},
            "status":{"type":"idle"}, "isArchived":true
        });
        std::fs::write(&fixture_path, fixture.to_string()).unwrap();
        let (status, first_page) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        crate::tests::validate_http_contract("projectSubagentList", &first_page);
        assert_eq!(first_page["subagents"].as_array().unwrap().len(), 0);
        assert_eq!(first_page["nextArchivedCursor"], "1");
        let (status, older_page) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery {
                    archived: Some(true),
                    cursor: Some("1".into()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        crate::tests::validate_http_contract("projectSubagentList", &older_page);
        assert_eq!(older_page["subagents"][0]["threadId"], "child-thread");
        assert_eq!(older_page["nextArchivedCursor"], Value::Null);
        let (status, paged) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "child-thread".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "an archived child on page two must open: {paged}"
        );
        assert_eq!(paged["subagent"]["isArchived"], true);
        fixture["repeatListCursor"] = json!(true);
        std::fs::write(&fixture_path, fixture.to_string()).unwrap();
        let (status, _) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "child-thread".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "a cyclic cursor cannot establish archive state"
        );
        fixture.as_object_mut().unwrap().remove("repeatListCursor");
        fixture["threads"]["child-thread"]["isArchived"] = Value::Bool(false);
        std::fs::write(&fixture_path, fixture.to_string()).unwrap();
        let (status, unarchived) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "child-thread".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(unarchived["subagent"]["isArchived"], false);
        for id in ["forged-thread", "outside-thread"] {
            let (status, _) = body(
                transcript(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path(("project-chat".into(), id.into())),
                    Query(TranscriptQuery::default()),
                )
                .await,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{id} must not inherit Project ownership"
            );
        }

        // The deadline includes waiting for the shared native runtime lock.
        let held_runtime = state.projects.codex.lock().await;
        let bounded = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            ),
        )
        .await
        .expect("the dock must not wait indefinitely on a runtime lock");
        assert_eq!(bounded.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(held_runtime);

        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("DELETE FROM runtime_bindings WHERE conversation_id='project-chat'")
            .execute(&pool)
            .await
            .unwrap();
        let (status, _) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a stale runtime binding cannot authorize discovery"
        );
        let (status, _) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "child-thread".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        state
            .store
            .bind_project_runtime(
                "project-chat",
                AgentFamily::Codex,
                &state.projects.codex_store,
                "thread",
                None,
                "later",
            )
            .await
            .unwrap();
        sqlx::query("UPDATE runtime_bindings SET runtime_thread_id='another-thread' WHERE conversation_id='project-chat'")
            .execute(&pool)
            .await
            .unwrap();
        let (status, _) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a retargeted binding cannot expose helpers"
        );
        sqlx::query("UPDATE runtime_bindings SET runtime_thread_id='thread' WHERE conversation_id='project-chat'")
            .execute(&pool)
            .await
            .unwrap();

        state
            .store
            .update_project_metadata("project", None, Some(false), None, "later")
            .await
            .unwrap();
        let (status, _) = body(
            list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "child-thread".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        state
            .store
            .update_project_metadata("project", None, Some(true), None, "later")
            .await
            .unwrap();
        let replacement = dir.path().join("different-root");
        std::fs::create_dir(&replacement).unwrap();
        let roots = [wonder_store::ProjectRootInput {
            path: replacement.to_string_lossy().into_owned(),
            canonical_path: crate::canonical_path::canonicalize(&replacement)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        }];
        state
            .store
            .update_project_roots("project", 1, &roots, 0, "later")
            .await
            .unwrap();
        let (status, _) = body(
            list(
                State(state),
                Extension(OwnerAuthority),
                Path("project-chat".into()),
                Query(ListQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // Contract: the Codex roster names each command left running in the thread
    // (every page, titled by its first line), Stop ends exactly that process,
    // and an unanswered lookup is reported, never shown as "none running".
    #[tokio::test]
    async fn codex_commands_left_running_are_listed_and_stopped_one_by_one() {
        let (dir, state, _) = crate::projects::tests::handoff_fixture().await;
        let cwd = dir
            .path()
            .join("project-source")
            .to_string_lossy()
            .into_owned();
        std::fs::write(
            dir.path().join("project-subagents-fixture.json"),
            json!({"threads":{"thread":{"id":"thread","cwd":cwd,"parentThreadId":null}}})
                .to_string(),
        )
        .unwrap();
        let idle_path = dir.path().join("idle-fixture.json");
        let terminal = |id: &str, command: &str| json!({"processId":id,"itemId":format!("item-{id}"),"cwd":cwd,"command":command});
        let mut idle = json!({"thread":{"id":"thread","status":{"type":"idle"},
            "backgroundPageSize":1,
            "background":[terminal("101", "  npm run dev\n# then watch"), terminal("102", "cargo watch -x test")]}});
        std::fs::write(&idle_path, idle.to_string()).unwrap();
        let roster = || async {
            let (status, roster) = body(
                list(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path("project-chat".into()),
                    Query(ListQuery::default()),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{roster}");
            crate::tests::validate_http_contract("projectSubagentList", &roster);
            roster
        };
        let stop_command = |id: &'static str| {
            stop(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), id.into())),
            )
        };

        let listed = roster().await;
        let rows = listed["subagents"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "both pages are read: {listed}");
        assert_eq!(rows[0]["threadId"], "command:101");
        assert_eq!(rows[0]["title"], "npm run dev…");
        assert_eq!(rows[1]["title"], "cargo watch -x test");
        for row in rows {
            assert_eq!(
                (
                    row["kind"].as_str(),
                    row["status"].as_str(),
                    row["canStop"].as_bool(),
                    row["runningElsewhere"].as_bool(),
                    row["namedSubagents"].as_bool(),
                ),
                (
                    Some("command"),
                    Some("running"),
                    Some(true),
                    Some(false),
                    Some(false)
                ),
            );
        }
        assert!(listed["detail"].is_null(), "{listed}");

        let (status, stopped) = body(stop_command("command:101").await).await;
        assert_eq!(status, StatusCode::OK, "{stopped}");
        crate::tests::validate_http_contract("projectSubagentSummary", &stopped);
        assert_eq!(
            (
                stopped["threadId"].as_str(),
                stopped["title"].as_str(),
                stopped["status"].as_str(),
                stopped["canStop"].as_bool()
            ),
            (
                Some("command:101"),
                Some("npm run dev…"),
                Some("interrupted"),
                Some(false)
            )
        );
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        let terminate = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|request| request["method"] == "thread/backgroundTerminals/terminate")
            .collect::<Vec<_>>();
        assert_eq!(terminate.len(), 1);
        assert_eq!(
            terminate[0]["params"],
            json!({"threadId":"thread","processId":"101"})
        );
        let listed = roster().await;
        assert_eq!(listed["subagents"].as_array().unwrap().len(), 1);
        assert_eq!(listed["subagents"][0]["threadId"], "command:102");

        // A command that already ended is explained, and the runtime is not asked.
        let response = stop_command("command:101").await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let text = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&text).contains("already stopped"));
        // A command has no conversation to open.
        let (status, _) = body(
            transcript(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(("project-chat".into(), "command:102".into())),
                Query(TranscriptQuery::default()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A failed read keeps the roster and says the commands are unchecked.
        idle["thread"]["backgroundError"] = json!(true);
        std::fs::write(&idle_path, idle.to_string()).unwrap();
        let partial = roster().await;
        assert!(partial["subagents"].as_array().unwrap().is_empty());
        assert_eq!(partial["detail"], COMMANDS_UNCHECKED);
        assert_eq!(
            stop_command("command:102").await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        // A thread not loaded in Cupola's runtime holds no commands it can see.
        std::fs::write(&idle_path, json!({}).to_string()).unwrap();
        let unloaded = roster().await;
        assert!(unloaded["subagents"].as_array().unwrap().is_empty());
        assert!(unloaded["detail"].is_null());
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        assert_eq!(
            requests
                .matches("thread/backgroundTerminals/terminate")
                .count(),
            1,
            "refused stops never reach the runtime"
        );
    }

    #[test]
    fn command_titles_are_one_bounded_line() {
        assert_eq!(command_title("\n  ls\t-la  \n").as_deref(), Some("ls -la"));
        assert_eq!(command_title("a\nb").as_deref(), Some("a…"));
        assert_eq!(command_title(" \n\t").as_deref(), None);
        let long = command_title(&"x".repeat(500)).unwrap();
        assert_eq!(long.chars().count(), 201);
    }

    // Contract: Stop rechecks Project ownership, reaches only a Claude task the
    // bridge confirms it can stop, and explains every refusal in user language.
    #[tokio::test]
    async fn stopping_a_claude_task_rechecks_ownership_and_maps_each_outcome() {
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use wonder_app_server::{AppServerClient, BridgeLaunchConfig};

        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        let roots =
            crate::projects::validate_folders(&[source.to_string_lossy().into_owned()], &[])
                .unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        state.projects = crate::projects::ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let session = "11111111-2222-4333-8444-555555555555";
        let entrypoint = dir.path().join("claude-bridge.py");
        std::fs::write(
            &entrypoint,
            format!(
                r#"import json, sys, os
root = os.path.dirname(__file__)
task = lambda id, status: {{'id': id, 'kind': 'agent', 'title': 'Audit', 'role': 'Explore', 'status': status, 'canStop': False, 'runningElsewhere': False, 'updatedAt': '2026-10-08T00:00:00Z'}}
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {{}}); result = {{}}
    with open(root + '/claude-requests', 'a') as log: log.write(json.dumps({{'method': m, 'params': p}}) + '\n')
    if m == 'initialize': result = {{'wonderBridge': {{'protocolVersion': 1, 'family': 'claude'}}, 'capabilities': {{'experimentalApi': True}}}}
    elif m == 'thread/read': result = {{'thread': {{'id': 'claude-{session}', 'sessionId': '{session}', 'cwd': '{cwd}'}}}}
    elif m == 'thread/backgroundTask/stop':
        outcome = {{'stop-me': 'stopped', 'finished': 'notRunning', 'on-mac': 'runningElsewhere', 'odd': 'unsupported', 'gone': 'notFound'}}[p['taskId']]
        result = {{'outcome': outcome}}
        if outcome != 'notFound': result['task'] = task(p['taskId'], 'interrupted' if outcome == 'stopped' else 'running')
        if outcome == 'stopped': result['task']['kind'] = 'agent'
    print(json.dumps({{'id': r['id'], 'result': result}}), flush=True)
"#,
                cwd = source.to_str().unwrap()
            ),
        )
        .unwrap();
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
        state
            .store
            .create_project_conversation(wonder_store::ProjectConversationInsert {
                conversation_id: "claude-chat",
                project_id: "project",
                family: AgentFamily::Claude,
                provider_store: &state.projects.claude_store,
                native_session_id: Some(session),
                cwd: source.to_str().unwrap(),
                roots_revision: 1,
                title: "Refactor",
                model: Some("claude:sonnet"),
                effort: None,
                service_tier: None,
                access_mode: "workspace",
                claude_approval: "accept_edits",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                "claude-chat",
                AgentFamily::Claude,
                &state.projects.claude_store,
                &format!("claude-{session}"),
                Some(session),
                "now",
            )
            .await
            .unwrap();
        let stop_task = |chat: &'static str, task: &'static str| {
            stop(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path((chat.into(), task.into())),
            )
        };
        let stop_calls = || {
            std::fs::read_to_string(dir.path().join("claude-requests"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("thread/backgroundTask/stop"))
                .count()
        };

        let first = stop_task("claude-chat", "stop-me").await;
        let first_status = first.status();
        let first_bytes = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            first_status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&first_bytes)
        );
        let stopped: Value = serde_json::from_slice(&first_bytes).unwrap();
        assert_eq!(
            (
                stopped["status"].as_str(),
                stopped["kind"].as_str(),
                stopped["namedSubagents"].as_bool()
            ),
            (Some("interrupted"), Some("agent"), Some(false))
        );
        assert_eq!(stopped["canStop"], false);
        crate::tests::validate_http_contract("projectSubagentSummary", &stopped);
        for (task, expected, words) in [
            ("finished", StatusCode::CONFLICT, "no longer running"),
            (
                "on-mac",
                StatusCode::CONFLICT,
                "running in Claude on your computer",
            ),
            ("odd", StatusCode::UNPROCESSABLE_ENTITY, "can't stop"),
            ("gone", StatusCode::NOT_FOUND, ""),
        ] {
            let response = stop_task("claude-chat", task).await;
            let status = response.status();
            let text = String::from_utf8(
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert_eq!(status, expected, "{task}: {text}");
            assert!(text.contains(words), "{task}: {text}");
        }
        assert_eq!(stop_calls(), 5);

        // Ownership is rechecked on every call, before the bridge is reached.
        assert_eq!(
            stop_task("missing-chat", "stop-me").await.status(),
            StatusCode::NOT_FOUND
        );
        state
            .store
            .update_project_metadata("project", None, Some(false), None, "later")
            .await
            .unwrap();
        assert_eq!(
            stop_task("claude-chat", "stop-me").await.status(),
            StatusCode::FORBIDDEN,
            "an excluded Project cannot stop tasks"
        );
        state
            .store
            .update_project_metadata("project", None, Some(true), None, "later")
            .await
            .unwrap();
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("DELETE FROM runtime_bindings WHERE conversation_id='claude-chat'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            stop_task("claude-chat", "stop-me").await.status(),
            StatusCode::CONFLICT,
            "a stale binding cannot authorize a stop"
        );
        assert_eq!(stop_calls(), 5, "refused requests never reach Claude");
    }
}
