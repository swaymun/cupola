//! Agents in a Project work with its other threads through Wonder's own tools.
//! Codex receives them as dynamic tools, Claude through the bridge's `wonder`
//! tool server and OpenCode through a per-session MCP server the ACP bridge
//! adds to the session (services/acp-runtime/tool-server.mjs); all reach this
//! module as the same correlated runtime request.
//! Every call is re-scoped to the caller's own Project, so a thread can never
//! see or message another Project, and no tool starts work except `send`,
//! `delegate` and `schedule` (work at a later time), which the agent
//! initiates on purpose; `notify` only pings the owner's phone.
use super::*;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use wonder_store::{
    MessageInsert, MessageSource, NewMessageSource, ProjectConversationCreate,
    ProjectConversationInsert, StoredProject, StoredProjectConversation,
};

pub(super) const LIST: &str = "wonder_thread_list";
pub(super) const READ: &str = "wonder_thread_read";
pub(super) const SEND: &str = "wonder_thread_send";
pub(super) const WAIT: &str = "wonder_thread_wait";
pub(super) const DELEGATE: &str = "wonder_delegate";
pub(super) const STOP: &str = "wonder_thread_stop";
pub(super) const MODELS: &str = "wonder_models";
pub(super) const SHOW_VISUAL: &str = visual::NAME;
pub(super) const SCHEDULE: &str = schedule::SCHEDULE;
pub(super) const SCHEDULES: &str = schedule::SCHEDULES;
pub(super) const SCHEDULE_UPDATE: &str = schedule::SCHEDULE_UPDATE;
pub(super) const NOTIFY: &str = notify::NOTIFY;
pub(super) const PREFERENCES: &str = preferences::NAME;
pub(super) const T3_THREAD: &str = t3::T3_THREAD;
pub(super) const REQUEST_SECRET: &str = secret::NAME;
pub(super) const TAKEOVER: &str = "wonder_request_takeover";
pub(super) const STREAM_SCREEN: &str = stream::NAME;
/// Not offered to agents: the Claude and ACP bridges ask it before running a
/// computer-use tool, so agents pause while the owner has phone control.
pub(super) const CONTROL_CHECK: &str = "wonder_computer_control_check";

mod notify;
mod preferences;
pub(crate) mod schedule;
pub(crate) mod secret;
pub(crate) mod stream;
mod t3;

/// Every tool name a Project agent receives, shared with the Claude and ACP
/// bridges' allowlists; `specs()` must offer exactly these.
static NAMES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    let file: Value =
        serde_json::from_str(include_str!("../../../packages/protocol/thread-tools.json"))
            .expect("thread-tools.json is valid JSON");
    serde_json::from_value(file["names"].clone()).expect("thread-tools.json lists names")
});

pub(crate) mod visual;

const CALLS_PER_TURN: i64 = 200;
const MAX_DEPTH: i64 = 3;
const MAX_CHILDREN: i64 = 16;
/// The most children one thread may have, whatever is asked for.
const CHILD_CAP_CEILING: i64 = 64;
/// Host environment setting for the default cap.
const CHILD_CAP_ENV: &str = "CUPOLA_DELEGATE_MAX_CHILDREN";
/// A delegated child still without its task this long after it was created
/// was left by an interrupted request (`remove_abandoned_delegations`).
const ABANDONED_AFTER: chrono::Duration = chrono::Duration::minutes(10);
const MODEL_CHOICES: usize = 10;
const DEFAULT_WAIT: u64 = 600;
const MAX_WAIT: u64 = 1800;
const MAX_PAGES: usize = 200;
const POLL: Duration = Duration::from_millis(400);
/// What a delegated thread may be asked to act as (T3 Code's task roles).
const ROLES: [&str; 6] = [
    "implementation",
    "research",
    "review",
    "design",
    "test",
    "general",
];
const RESULT_CHARS: usize = 6000;

pub(super) fn arguments(params: &Value) -> Result<Value, String> {
    // A visual carries its whole page; every other tool's arguments are small.
    let limit = if params.get("tool").and_then(Value::as_str) == Some(SHOW_VISUAL) {
        visual::ARGUMENT_BYTES
    } else {
        40 * 1024
    };
    let value = match params.get("arguments") {
        Some(Value::Object(value)) => Value::Object(value.clone()),
        Some(Value::String(value)) => {
            serde_json::from_str(value).map_err(|_| "Invalid project tool arguments.")?
        }
        _ => return Err("Project tool arguments must be an object.".into()),
    };
    if !value.is_object() || value.to_string().len() > limit {
        return Err("Project tool arguments exceed the supported size.".into());
    }
    Ok(value)
}

pub(super) fn registered(name: &str) -> bool {
    name == CONTROL_CHECK || NAMES.iter().any(|known| known == name)
}

/// Providers a Project agent may delegate to: those whose threads get these
/// tools (see `ProviderCapabilities::thread_tools`).
fn delegate_families() -> impl Iterator<Item = AgentFamily> {
    AgentFamily::ALL
        .into_iter()
        .filter(|family| family.provider().capabilities.thread_tools)
}

pub(super) fn specs() -> Vec<Value> {
    let providers: Vec<_> = delegate_families().map(AgentFamily::as_str).collect();
    let fixed: Vec<_> = delegate_families()
        .filter(|family| !family.provider().capabilities.access_choices)
        .map(|family| family.provider().display_name)
        .collect();
    let access = match fixed.as_slice() {
        [] => "Defaults to yours; cannot be wider.".to_owned(),
        [one] => format!("Defaults to yours; cannot be wider. {one} uses its own permission settings on the computer, recorded as workspace; omit accessMode for it."),
        many => format!("Defaults to yours; cannot be wider. {} use their own permission settings on the computer, recorded as workspace; omit accessMode for them.", many.join(" and ")),
    };
    let object = |properties: Value, required: Value| json!({"type":"object","additionalProperties":false,"properties":properties,"required":required});
    let spec = |name: &str, description: &str, schema: Value| json!({"type":"function","name":name,"description":description,"inputSchema":schema});
    let request_id = json!({"type":"string","format":"uuid","description":"A new UUID for this action (8-4-4-4-12 hexadecimal). Reuse it when you retry the same action; never reuse it for a different one."});
    let request_id_schema = request_id.clone();
    vec![
        spec(LIST, "List the threads of this Project (not other Projects) with their id, title, provider, model, status (working, waiting or idle), waitingOn (approval, question, queue, computer or null), queued message count and last activity. Read-only.",
            object(json!({"limit":{"type":"integer","minimum":1,"maximum":50},"cursor":{"type":"string","description":"nextCursor from the previous page."}}), json!([]))),
        spec(READ, "Read messages from another thread of this Project: user and agent text plus one-line summaries of commands and tool use, never raw output. Pages forward: pass the previous nextPosition as afterPosition. A long item is cut at maxCharsPerItem; continue it with textOffset (applies to the first returned item). Read-only.",
            object(json!({"threadId":{"type":"string"},"afterPosition":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":50},"maxCharsPerItem":{"type":"integer","minimum":200,"maximum":20000},"textOffset":{"type":"integer","minimum":0}}), json!(["threadId"]))),
        spec(SEND, "Send a message to another thread of this Project. mode queue (default) runs it after the thread's current work; steer adds it to the thread's running response and works only on providers that support it. The thread sees who wrote it. The target's access cannot be wider than yours. Reuse clientRequestId on retries. Returns outcome: delivered (the thread started on it), pending (queued behind its current work or waiting for the computer; follow it with wonder_thread_wait and the messageId), failed (with error, the reason it was not delivered) or uncertain.",
            object(json!({"threadId":{"type":"string"},"message":{"type":"string","maxLength":32768},"mode":{"type":"string","enum":["queue","steer"]},"clientRequestId":request_id.clone()}), json!(["threadId","message","clientRequestId"]))),
        spec(WAIT, "Wait for a thread's current work to finish. Returns finished, idle (nothing was running) or still_running after the timeout (default 600 s, at most 1800 s). A timeout never stops the thread; wait again or read it later. With messageId, messageState and error say how that message was delivered. For a thread you delegated, taskResult is its task's recorded outcome and final reply, kept by Cupola across restarts. waitingOn says what a running thread is waiting for: approval, question (the owner must answer), queue, computer or null.",
            object(json!({"threadId":{"type":"string"},"messageId":{"type":"string","description":"Wait for this message only, as returned by wonder_thread_send or wonder_delegate."},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":1800}}), json!(["threadId"]))),
        spec(DELEGATE, "Create a new thread in this Project's folder on the chosen provider and model, and give it a task. It receives only the prompt (after one line naming its role, unless the role is general), not your conversation, so include everything it needs. Its access is at most yours. mode async returns at once; Cupola tells you when it finishes. mode wait blocks like wonder_thread_wait. Reuse clientRequestId on retries. Use wonder_models to see valid models, efforts and speeds.",
            object(json!({"clientRequestId":request_id.clone(),"prompt":{"type":"string","maxLength":32768},"role":{"type":"string","enum":ROLES},"provider":{"type":"string","enum":providers},"model":{"type":"string","description":"A model id from wonder_models; omit for the provider's default."},"effort":{"type":"string","description":"An effort the model lists in wonder_models; omit for its default."},"serviceTier":{"type":"string","description":"A speed the model lists in wonder_models; omit for its default. A faster speed than yours is not allowed."},"accessMode":{"type":"string","enum":["read_only","workspace","full_access"],"description":access},"claudeApproval":{"type":"string","enum":["ask","accept_edits","auto"]},"title":{"type":"string","maxLength":80},"folder":{"type":"string","description":"Absolute path of one of this Project's folders or a Git worktree of one, for the thread to work in; omit for your own folder."},"maxChildren":{"type":"integer","minimum":1,"maximum":64,"description":"Most threads you may have delegated, counting this one; can only lower the owner's limit (default 16)."},"mode":{"type":"string","enum":["async","wait"]},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":1800}}), json!(["clientRequestId","prompt","provider"]))),
        spec(MODELS, "List the providers and models you can delegate to with wonder_delegate: whether each provider is available (with the reason when not), each model's efforts, speeds (serviceTiers) and defaults, and the settings a delegated thread inherits from you. Read-only.",
            object(json!({"provider":{"type":"string","enum":providers},"search":{"type":"string","maxLength":100,"description":"Only models whose id or name contains this text."},"limit":{"type":"integer","minimum":1,"maximum":50}}), json!([]))),
        spec(STOP, "Stop the current response of a thread you delegated (directly or through your delegated threads), never your own or anyone else's. With cancelQueued, its queued messages are cancelled too. Its task result reads stopped. Threads it delegated keep running; stop them separately. Reuse clientRequestId on retries.",
            object(json!({"threadId":{"type":"string"},"cancelQueued":{"type":"boolean"},"clientRequestId":request_id.clone()}), json!(["threadId","clientRequestId"]))),
        visual::spec(),
        secret::spec(),
        secret::run_spec(),
        spec(TAKEOVER, "Ask the owner to take over the computer for something only they should do, such as signing in, a passkey or a payment. Cupola notifies their phone; they take control there, then give it back. Blocks until returned (the owner gave control back: look again before continuing, as the screen may have changed), declined (do without it or ask in your reply) or expired (default 600 s, at most 1800 s). While the owner has control, Cupola refuses the computer-use actions it runs for you; Codex's own computer use cannot be paused, so do not use the computer until this returns. Reuse clientRequestId on retries.",
            object(json!({"reason":{"type":"string","maxLength":500,"description":"What the owner should do, in plain words, e.g. Sign in to GitHub in Chrome."},"app":{"type":"string","maxLength":120,"description":"The app to do it in, e.g. Google Chrome."},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":1800},"clientRequestId":request_id_schema}), json!(["reason","clientRequestId"]))),
    ]
    .into_iter()
    .chain(schedule::specs())
    .chain([notify::spec(), preferences::spec(), t3::spec(), stream::spec()])
    .collect()
}

/// A failure the agent can act on: `code` is stable, `message` is for the model.
#[derive(Debug)]
pub(super) struct ToolError {
    pub(super) code: &'static str,
    pub(super) message: String,
}
impl ToolError {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl From<sqlx::Error> for ToolError {
    fn from(_: sqlx::Error) -> Self {
        Self::new(
            "unavailable",
            "Cupola could not read its data. Try again shortly.",
        )
    }
}

fn response(result: Result<Value, ToolError>) -> Value {
    match result {
        Ok(mut value) => {
            let images = visual::take_images(&mut value);
            let text = value.to_string();
            if text.len() > 1024 * 1024 {
                return response(Err(ToolError::new(
                    "too_large",
                    "This result is too large. Ask for fewer items or a smaller maxCharsPerItem.",
                )));
            }
            let mut items = vec![json!({"type":"inputText","text":text})];
            items.extend(images);
            json!({"success":true,"contentItems":items})
        }
        Err(error) => {
            let text = json!({"error":{"code":error.code,"message":error.message}}).to_string();
            json!({"success":false,"contentItems":[{"type":"inputText","text":text}]})
        }
    }
}

struct Caller {
    /// The provider runtime that made the call; `None` outside one.
    runtime: Option<String>,
    message: wonder_store::StoredMessage,
    conversation: StoredProjectConversation,
    project: StoredProject,
}

async fn caller(
    state: &AppState,
    runtime: &str,
    thread: &str,
    message: &wonder_store::StoredMessage,
) -> Result<Caller, ToolError> {
    if !state.ingestion.runtime_alive(runtime) {
        return Err(ToolError::new(
            "wrong_runtime",
            "The request does not belong to this live runtime.",
        ));
    }
    if message.codex_thread_id.as_deref() != Some(thread) {
        return Err(ToolError::new(
            "wrong_turn",
            "The request targets different work.",
        ));
    }
    let conversation = state
        .store
        .project_conversation(&message.conversation_id)
        .await?
        .ok_or_else(|| {
            ToolError::new(
                "not_project",
                "Thread tools are available only in Project threads.",
            )
        })?;
    let project = state
        .store
        .project(&conversation.project_id)
        .await?
        .filter(|project| project.is_included)
        .ok_or_else(|| ToolError::new("project_unavailable", "This Project is not available."))?;
    let project = projects::thread_project(state, project, &conversation.cwd).await;
    Ok(Caller {
        runtime: Some(runtime.to_owned()),
        message: message.clone(),
        conversation,
        project,
    })
}

/// The target must belong to the caller's own Project. Another Project's
/// thread is reported exactly like a missing one.
async fn target(
    state: &AppState,
    caller: &Caller,
    thread: &str,
) -> Result<StoredProjectConversation, ToolError> {
    state
        .store
        .project_conversation(thread)
        .await?
        .filter(|found| found.project_id == caller.project.id)
        .ok_or_else(|| ToolError::new("not_found", "No such thread in this Project."))
}

fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A UUID derived from the caller's identity and request, so a retried call
/// finds the message or thread the first attempt made.
fn derived_uuid(parts: &[&str]) -> String {
    let digest = Sha256::digest(json!(parts).to_string().as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn request_id(args: &Value) -> Result<String, ToolError> {
    let id = args
        .get("clientRequestId")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("invalid", "clientRequestId is required."))?;
    uuid::Uuid::parse_str(id)
        .map(|id| id.to_string())
        .map_err(|_| ToolError::new("invalid", "clientRequestId must be a UUID."))
}

fn text_arg(args: &Value, name: &str, max: usize) -> Result<String, ToolError> {
    let value = args
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::new("invalid", format!("{name} is required.")))?;
    if value.len() > max {
        return Err(ToolError::new(
            "invalid",
            format!("{name} is longer than {max} bytes."),
        ));
    }
    Ok(value.to_owned())
}

fn int_arg(args: &Value, name: &str, default: u64, min: u64, max: u64) -> Result<u64, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|value| (min..=max).contains(value))
            .ok_or_else(|| ToolError::new("invalid", format!("{name} must be {min} to {max}."))),
    }
}

fn only_known(args: &Value, known: &[&str]) -> Result<(), ToolError> {
    match args.as_object() {
        Some(object) if object.keys().all(|key| known.contains(&key.as_str())) => Ok(()),
        _ => Err(ToolError::new(
            "invalid",
            format!("Use only these fields: {}.", known.join(", ")),
        )),
    }
}

fn access_rank(mode: &str) -> u8 {
    match mode {
        "read_only" => 0,
        "workspace" => 1,
        _ => 2,
    }
}

fn approval_rank(mode: &str) -> u8 {
    match mode {
        "auto" => 2,
        "accept_edits" => 1,
        _ => 0,
    }
}

/// The widest approval a thread started by `parent` may have. Codex threads
/// always ask, so a Codex parent allows only Ask.
fn approval_ceiling(parent: &StoredProjectConversation) -> u8 {
    if parent.family.allows_approval_choice() {
        approval_rank(&parent.claude_approval)
    } else {
        0
    }
}

/// The provider's default approval, lowered to the parent's own level.
fn default_approval(family: AgentFamily, parent: &StoredProjectConversation) -> String {
    let default = agent_defaults::claude_approval(family);
    if !family.allows_approval_choice() || approval_rank(default) <= approval_ceiling(parent) {
        return default.to_owned();
    }
    family
        .provider()
        .approval_modes
        .iter()
        .find(|mode| approval_rank(mode) == approval_ceiling(parent))
        .copied()
        .unwrap_or("ask")
        .to_owned()
}

/// A thread under a provider's own settings (OpenCode, Cursor) may act with
/// whatever the owner allowed there, which Cupola cannot lower. Only a thread
/// at Full access, or one under such settings itself, may start or direct it.
fn may_use_own_settings(
    parent: &StoredProjectConversation,
    family: AgentFamily,
) -> Result<(), ToolError> {
    if !family.permissions_enforced()
        && parent.family.permissions_enforced()
        && parent.access_mode != "full_access"
    {
        return Err(ToolError::new(
            "permission_escalation",
            format!(
                "{} runs under its own permission settings on the computer, which can be wider than this thread's. Only a thread with Full access can start or direct it.",
                family.provider().display_name
            ),
        ));
    }
    Ok(())
}

/// A thread may only direct a thread whose access is no wider than its own.
fn within_authority(
    from: &StoredProjectConversation,
    to: &StoredProjectConversation,
) -> Result<(), ToolError> {
    let wider_access = access_rank(&to.access_mode) > access_rank(&from.access_mode);
    // Approval refines Workspace only; Read only and Full access ignore it.
    let wider_approval = to.family.allows_approval_choice()
        && to.access_mode == "workspace"
        && approval_rank(&to.claude_approval) > approval_ceiling(from);
    if wider_access || wider_approval {
        return Err(ToolError::new(
            "permission_escalation",
            "That thread has wider access than this one. An agent can only direct threads with the same or narrower access.",
        ));
    }
    may_use_own_settings(from, to.family)
}

fn forbid_plan_mode(caller: &Caller) -> Result<(), ToolError> {
    forbid_plan_mode_with(
        caller,
        "This thread is planning. Finish the plan before starting work in other threads.",
    )
}

fn forbid_plan_mode_with(caller: &Caller, message: &str) -> Result<(), ToolError> {
    if caller.conversation.plan_mode {
        return Err(ToolError::new("plan_mode", message));
    }
    Ok(())
}

fn provider_name(family: AgentFamily) -> &'static str {
    family.as_str()
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

struct Status {
    status: &'static str,
    queued: i64,
    /// What a thread that is not making progress waits for. `computer` is
    /// reserved for work held until the computer reconnects.
    waiting_on: Option<&'static str>,
}

async fn status(state: &AppState, conversation: &str) -> Result<Status, ToolError> {
    let run = state.store.thread_run_state(conversation).await?;
    let status = if run.waiting {
        "waiting"
    } else if run.working || run.queued > 0 {
        "working"
    } else {
        "idle"
    };
    let waiting_on = if run.question {
        Some("question")
    } else if run.waiting {
        Some("approval")
    } else if !run.working && run.queued > 0 {
        Some("queue")
    } else {
        None
    };
    Ok(Status {
        status,
        queued: run.queued,
        waiting_on,
    })
}

async fn summary(
    state: &AppState,
    caller: &Caller,
    thread: &StoredProjectConversation,
) -> Result<Value, ToolError> {
    let Status {
        status,
        queued,
        waiting_on,
    } = status(state, &thread.conversation_id).await?;
    let parent = state
        .store
        .thread_delegation(&thread.conversation_id)
        .await?
        .map(|link| link.parent_conversation_id);
    Ok(json!({
        "id": thread.conversation_id, "title": thread.title,
        "provider": provider_name(thread.family), "model": thread.model,
        "accessMode": thread.access_mode, "status": status, "waitingOn": waiting_on, "queuedMessages": queued,
        "lastActivityAt": thread.last_activity_at,
        "isCurrent": thread.conversation_id == caller.conversation.conversation_id,
        "delegatedBy": parent,
    }))
}

async fn list(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    only_known(&args, &["limit", "cursor"])?;
    let limit = int_arg(&args, "limit", 20, 1, 50)? as usize;
    let mut threads = state
        .store
        .project_conversations(&caller.project.id)
        .await?;
    // Newest activity first, with the id breaking ties, so a cursor is a
    // position in that order and pages stay put while threads run.
    threads.sort_by(|a, b| {
        b.last_activity_at
            .cmp(&a.last_activity_at)
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
    });
    if let Some(cursor) = args.get("cursor").and_then(Value::as_str) {
        let (at, id) = cursor
            .split_once('|')
            .ok_or_else(|| ToolError::new("invalid", "cursor is not valid."))?;
        threads.retain(|t| {
            t.last_activity_at.as_str() < at
                || (t.last_activity_at == at && t.conversation_id.as_str() > id)
        });
    }
    let more = threads.len() > limit;
    threads.truncate(limit);
    let mut items = Vec::with_capacity(threads.len());
    for thread in &threads {
        items.push(summary(state, caller, thread).await?);
    }
    let next = if more {
        threads
            .last()
            .map(|t| format!("{}|{}", t.last_activity_at, t.conversation_id))
    } else {
        None
    };
    Ok(json!({"threads": items, "nextCursor": next}))
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

pub(crate) fn clip(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

pub(crate) fn content_text(item: &Value) -> String {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !crate::wonder_skills::is_phone_context(text))
                .map(|text| crate::provider_switch::without_handoff(text).trim_start())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The same operation name used by the phone's ToolDisplayName, without
/// inferring effects from a tool's identifier.
fn tool_display_name(server: &str, tool: &str) -> String {
    fn words(raw: &str) -> Option<String> {
        let characters: Vec<char> = raw.chars().take(256).collect();
        let mut value = String::new();
        for (index, &character) in characters.iter().enumerate() {
            if matches!(character, '_' | '-' | '.') || character.is_whitespace() {
                value.push(' ');
            } else if character.is_alphanumeric() {
                if character.is_uppercase()
                    && index > 0
                    && (characters[index - 1].is_lowercase()
                        || (characters[index - 1].is_uppercase()
                            && characters.get(index + 1).is_some_and(|c| c.is_lowercase())))
                {
                    value.push(' ');
                }
                value.push(character);
            } else {
                return None;
            }
        }
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        (!value.is_empty()).then_some(value)
    }
    let (server, tool) = match tool.strip_prefix("mcp__") {
        Some(rest) => match rest.split_once("__") {
            Some(parts) => parts,
            None => return "Tool".into(),
        },
        None => (server, tool),
    };
    let Some(operation) = words(tool) else {
        return "Tool".into();
    };
    let server = server.strip_prefix("claude_ai_").unwrap_or(server);
    let source = words(server)
        .map(|source| {
            if server.eq_ignore_ascii_case("github") {
                return "GitHub".into();
            }
            source
                .split_whitespace()
                .map(|word| {
                    if word.eq_ignore_ascii_case("github") {
                        "GitHub".into()
                    } else if word == word.to_uppercase() {
                        word.to_owned()
                    } else {
                        let mut chars = word.chars();
                        chars.next().unwrap().to_uppercase().collect::<String>()
                            + &chars.as_str().to_lowercase()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| "Tool".into());
    format!("{source}: {}", operation.to_lowercase())
}

/// One timeline entry as an agent should read it: messages in full, work as
/// a single line. Reasoning and raw tool output are never included.
pub(crate) fn summarize(item: &Value) -> Option<(&'static str, String)> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let field = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or_default();
    let line = match kind {
        "userMessage" => return Some(("user", content_text(item))),
        "agentMessage" => return Some(("agent", content_text(item))),
        "plan" => return Some(("agent", format!("Plan: {}", content_text(item)))),
        "commandExecution" => {
            let exit = item
                .get("exitCode")
                .and_then(Value::as_i64)
                .map(|code| format!(", exit {code}"))
                .unwrap_or_default();
            format!("Ran `{}` ({}{exit})", clip(field("command"), 200), {
                match field("status") {
                    "" => "done",
                    status => status,
                }
            })
        }
        "fileChange" => {
            let paths = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|c| c.get("path").and_then(Value::as_str))
                        .map(|path| path.rsplit('/').next().unwrap_or(path).to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            match paths.len() {
                0 => "Edited files".into(),
                1..=5 => format!("Edited {}", paths.join(", ")),
                more => format!(
                    "Edited {} and {} more files",
                    paths[..5].join(", "),
                    more - 5
                ),
            }
        }
        "mcpToolCall" | "dynamicToolCall" => tool_display_name(field("server"), field("tool")),
        "webSearch" => format!("Searched the web for {}", clip(field("query"), 120)),
        "imageView" | "imageGeneration" => "Worked with an image".into(),
        "subAgentActivity" | "collabAgentToolCall" => "Ran an agent task".into(),
        _ => return None,
    };
    Some(("tool", line))
}

/// The provider thread behind a conversation, once its first message started
/// one. The store reports a thread that has none as an empty id.
async fn native_thread(state: &AppState, conversation: &str) -> Option<String> {
    state
        .store
        .conversation_thread(conversation)
        .await
        .ok()
        .flatten()
        .filter(|thread| !thread.is_empty())
}

/// Walks a thread's visible entries in order and keeps one page of them.
struct Page {
    after: u64,
    limit: usize,
    position: u64,
    items: Vec<(u64, &'static str, String, Option<String>)>,
    more: bool,
}
impl Page {
    fn new(after: u64, limit: usize) -> Self {
        Self {
            after,
            limit,
            position: 0,
            items: Vec::new(),
            more: false,
        }
    }
    /// True once the page is full and a further entry proves there is more.
    fn push(&mut self, item: &Value) -> bool {
        let Some((role, text)) = summarize(item) else {
            return false;
        };
        self.position += 1;
        if self.position <= self.after {
            return false;
        }
        if self.items.len() == self.limit {
            self.more = true;
            return true;
        }
        let client = item
            .get("clientId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.items.push((self.position, role, text, client));
        false
    }
}

/// Runtime entries for a thread, oldest first, until `visit` says stop.
async fn walk(
    state: &AppState,
    thread: &str,
    visit: impl FnMut(&Value) -> bool,
) -> Result<(), ToolError> {
    let rpc = claude::for_thread(state, thread)
        .await
        .map_err(|_| unavailable())?;
    walk_with(&rpc, thread, visit).await
}

fn unavailable() -> ToolError {
    ToolError::new(
        "unavailable",
        "That thread's history is unavailable right now.",
    )
}

/// Walks one provider session's entries through `rpc`. Sessions a thread left
/// behind when it changed provider no longer have a binding to route by.
pub(crate) async fn walk_with(
    rpc: &wonder_app_server::RpcClient,
    thread: &str,
    mut visit: impl FnMut(&Value) -> bool,
) -> Result<(), ToolError> {
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_PAGES {
        let mut params = json!({"threadId":thread,"limit":100,"sortDirection":"asc"});
        if let Some(cursor) = cursor.take() {
            params["cursor"] = json!(cursor);
        }
        let result = rpc
            .request("thread/items/list", params)
            .await
            .ok()
            .filter(|response| response.error.is_none())
            .and_then(|response| response.result)
            .ok_or_else(unavailable)?;
        for entry in result
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(item) = entry.get("item") {
                if visit(item) {
                    return Ok(());
                }
            }
        }
        match result.get("nextCursor").and_then(Value::as_str) {
            Some(next) if !next.is_empty() && seen.insert(next.to_owned()) => {
                cursor = Some(next.to_owned())
            }
            _ => return Ok(()),
        }
    }
    Err(ToolError::new(
        "too_long",
        "That thread is too long to scan from the start. Read its most recent work in Cupola.",
    ))
}

/// Every provider session behind a thread, oldest first: the ones it left when
/// it changed provider, then the active one. Positions count across all of them.
#[cfg(all(test, unix))]
pub(crate) async fn segments_for_test(
    state: &AppState,
    conversation: &str,
) -> Vec<(wonder_store::AgentFamily, String, bool)> {
    segments(state, conversation).await.unwrap_or_default()
}

pub(crate) async fn segments(
    state: &AppState,
    conversation: &str,
) -> Result<Vec<(wonder_store::AgentFamily, String, bool)>, ToolError> {
    let mut found = Vec::new();
    for old in state.store.project_runtime_history(conversation).await? {
        if let Some(thread) = old.runtime_thread_id.or(old.native_session_id) {
            found.push((old.family, thread, false));
        }
    }
    if let Some(native) = native_thread(state, conversation).await {
        let family = match state.store.runtime_binding(conversation).await? {
            Some(binding) => binding.family,
            None => AgentFamily::issuing_thread(&native),
        };
        found.push((family, native, true));
    }
    Ok(found)
}

pub(crate) async fn read_composer_reference(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    id: &str,
) -> Result<Value, ToolError> {
    let conversation = match state
        .store
        .project_conversation(&message.conversation_id)
        .await?
    {
        Some(conversation) => conversation,
        None => {
            let project = message
                .conversation_id
                .strip_prefix("project-files:")
                .and_then(|s| s.split_once(':'))
                .map(|(id, _)| id)
                .ok_or_else(|| ToolError::new("not_found", "This Project is unavailable."))?;
            let mut scope = state
                .store
                .project_conversation(id)
                .await?
                .filter(|t| t.project_id == project)
                .ok_or_else(|| ToolError::new("not_found", "This thread is unavailable."))?;
            scope.conversation_id = message.conversation_id.clone();
            scope
        }
    };
    let project = state
        .store
        .project(&conversation.project_id)
        .await?
        .filter(|p| p.is_included)
        .ok_or_else(|| ToolError::new("not_found", "This Project is unavailable."))?;
    let caller = Caller {
        runtime: None,
        message: message.clone(),
        conversation,
        project,
    };
    read(
        state,
        &caller,
        json!({"threadId": id, "limit": 8, "maxCharsPerItem": 1000}),
    )
    .await
}

async fn read(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    only_known(
        &args,
        &[
            "threadId",
            "afterPosition",
            "limit",
            "maxCharsPerItem",
            "textOffset",
        ],
    )?;
    let thread = target(state, caller, &text_arg(&args, "threadId", 128)?).await?;
    let after = int_arg(&args, "afterPosition", 0, 0, u64::MAX / 2)?;
    let limit = int_arg(&args, "limit", 20, 1, 50)? as usize;
    let max_chars = int_arg(&args, "maxCharsPerItem", 4000, 200, 20000)? as usize;
    let offset = int_arg(&args, "textOffset", 0, 0, u64::MAX / 2)? as usize;
    let mut page = Page::new(after, limit);
    for (family, native, active) in segments(state, &thread.conversation_id).await? {
        let mut full = false;
        let stop = |item: &Value| {
            full = page.push(item);
            full
        };
        if active {
            walk(state, &native, stop).await?;
        } else {
            let rpc = crate::projects::rpc_for(state, family)
                .await
                .map_err(|_| unavailable())?;
            walk_with(&rpc, &native, stop).await?;
        }
        if full {
            break;
        }
    }
    let labels = state
        .store
        .message_source_labels(&thread.conversation_id)
        .await?;
    let mut items = Vec::new();
    for (index, (position, role, text, client)) in page.items.iter().enumerate() {
        let total = text.chars().count();
        let start = if index == 0 { offset.min(total) } else { 0 };
        let end = (start + max_chars).min(total);
        let shown = crate::history::sanitize_ansi_and_secrets(
            &text
                .chars()
                .skip(start)
                .take(end - start)
                .collect::<String>(),
            max_chars * 4,
        );
        let mut item = json!({"position":position,"role":role,"text":shown,"totalChars":total});
        if end < total {
            item["truncated"] = json!(true);
            item["nextTextOffset"] = json!(end);
        }
        if let Some(source) = client.as_deref().and_then(|client| labels.get(client)) {
            item["from"] = json!(source.source_title);
        }
        items.push(item);
    }
    // Messages queued behind the thread's work are not in its history yet.
    let mut pending = Vec::new();
    if !page.more {
        for (body, source) in state
            .store
            .queued_message_bodies(&thread.conversation_id)
            .await?
        {
            pending.push(
                json!({"role":"user","text":clip(&body, max_chars),"queued":true,
                "from": source.map(|s| s.source_title)}),
            );
        }
    }
    let status = status(state, &thread.conversation_id).await?.status;
    if !page.more && status == "idle" {
        observe(state, caller, &thread).await;
    }
    let next = page.items.last().map(|i| i.0).unwrap_or(after);
    Ok(
        json!({"threadId":thread.conversation_id,"title":thread.title,"status":status,
        "items":items,"queuedMessages":pending,"nextPosition":next,"hasMore":page.more}),
    )
}

/// A parent that has read its idle child's result needs no wake-up for it.
async fn observe(state: &AppState, caller: &Caller, child: &StoredProjectConversation) {
    let _ = state
        .store
        .observe_thread_delegation(
            &caller.conversation.conversation_id,
            &child.conversation_id,
            &now_text(),
        )
        .await;
}

// ---------------------------------------------------------------------------
// send
// ---------------------------------------------------------------------------

fn source_for<'a>(caller: &'a Caller) -> NewMessageSource<'a> {
    NewMessageSource {
        kind: "thread",
        conversation: Some(&caller.conversation.conversation_id),
        title: &caller.conversation.title,
        wake_children: &[],
        wake_parent: None,
    }
}

async fn send(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    send_from(state, caller, args, source_for(caller)).await
}

async fn send_from(
    state: &AppState,
    caller: &Caller,
    args: Value,
    source: NewMessageSource<'_>,
) -> Result<Value, ToolError> {
    only_known(&args, &["threadId", "message", "mode", "clientRequestId"])?;
    forbid_plan_mode(caller)?;
    let thread = target(state, caller, &text_arg(&args, "threadId", 128)?).await?;
    if thread.conversation_id == caller.conversation.conversation_id {
        return Err(ToolError::new("invalid", "A thread cannot message itself."));
    }
    let body = text_arg(&args, "message", 32 * 1024)?;
    let request = request_id(&args)?;
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("queue");
    within_authority(&caller.conversation, &thread)?;
    let id = derived_uuid(&[
        "thread-send",
        &caller.conversation.conversation_id,
        &thread.conversation_id,
        &request,
    ]);
    let device = &caller.message.device_id;
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return Err(ToolError::new(
            "updating",
            "Cupola is preparing to update. Try again shortly.",
        ));
    };
    let insert = match mode {
        "queue" => {
            state
                .store
                .insert_agent_message(
                    device,
                    &id,
                    &body,
                    &thread.conversation_id,
                    &now_text(),
                    &source,
                )
                .await?
        }
        "steer" => {
            if !thread.family.provider().capabilities.agent_steer {
                return Err(ToolError::new(
                    "steer_unsupported",
                    format!(
                        "{} threads cannot take a message during a response. Use mode \"queue\" to run it next.",
                        thread.family.provider().display_name
                    ),
                ));
            }
            // A retry that already steered must not need the turn to still run.
            let prior = state
                .store
                .message_by_device_and_client_message_id(device, &id)
                .await?;
            if let Some(prior) = prior {
                MessageInsert::Existing(prior)
            } else {
                let running = state
                    .store
                    .running_message(&thread.conversation_id)
                    .await?
                    .and_then(|m| m.codex_turn_id)
                    .ok_or_else(|| {
                        ToolError::new(
                            "not_running",
                            "That thread is not in a response right now. Use mode \"queue\".",
                        )
                    })?;
                state
                    .store
                    .insert_agent_guide(
                        device,
                        &id,
                        &body,
                        &thread.conversation_id,
                        &now_text(),
                        &running,
                        &source,
                    )
                    .await?
            }
        }
        _ => return Err(ToolError::new("invalid", "mode must be queue or steer.")),
    };
    let (message, duplicate) = match insert {
        MessageInsert::Inserted(message) => (message, false),
        MessageInsert::Existing(message) => (message, true),
        MessageInsert::Conflict => {
            return Err(ToolError::new(
                "request_reused",
                "That clientRequestId was already used for a different message. Use a new one.",
            ))
        }
    };
    if !duplicate {
        // Work sent to a delegated child is reported to its parent again.
        if let Some(link) = state
            .store
            .thread_delegation(&thread.conversation_id)
            .await?
        {
            if link.parent_conversation_id == caller.conversation.conversation_id {
                state
                    .store
                    .rearm_thread_delegation(&thread.conversation_id)
                    .await?;
            }
        }
    }
    let mut result = json!({"threadId":thread.conversation_id,"messageId":message.id,"mode":mode,
        "duplicate":duplicate});
    // Nothing ahead of it in the thread: it starts now, so wait briefly to
    // report whether it did, or why the Mac refused it.
    let run = state
        .store
        .thread_run_state(&thread.conversation_id)
        .await?;
    if !run.working && !run.waiting && run.queued <= 1 {
        state.dispatch_wake.notify_one();
        let deadline = tokio::time::Instant::now() + SEND_SETTLE;
        while tokio::time::Instant::now() < deadline
            && state
                .store
                .message_by_id(&message.id)
                .await?
                .is_some_and(|m| UNSTARTED.contains(&m.state.as_str()))
        {
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }
    report_delivery(state, &message.id, &mut result).await?;
    Ok(result)
}

/// How long a send waits to see its message start, when nothing runs ahead
/// of it, before answering that it is still pending.
#[cfg(not(test))]
const SEND_SETTLE: Duration = Duration::from_secs(20);
#[cfg(test)]
const SEND_SETTLE: Duration = Duration::from_millis(1000);
const SETTLE_POLL: Duration = Duration::from_millis(100);
const UNSTARTED: [&str; 2] = ["accepted_by_wonder", "dispatching_to_codex"];

/// Adds a message's delivery to a tool result: `deliveryState`, an
/// `outcome` (delivered, pending, failed or uncertain), the Mac's reason
/// when it was not delivered, and how to follow a pending one.
async fn report_delivery(state: &AppState, id: &str, result: &mut Value) -> Result<(), ToolError> {
    let Some(message) = state.store.message_by_id(id).await? else {
        return Ok(());
    };
    let outcome = match message.state.as_str() {
        "safe_to_retry" | "failed" => "failed",
        "uncertain" => "uncertain",
        s if UNSTARTED.contains(&s) => "pending",
        _ => "delivered",
    };
    result["deliveryState"] = json!(message.state);
    result["outcome"] = json!(outcome);
    match outcome {
        "failed" | "uncertain" => {
            let reason = state.store.message_delivery_error(id).await?;
            result["error"] = json!(reason.unwrap_or_else(|| if outcome == "failed" {
                "Cupola could not start the thread's reply.".into()
            } else {
                "Cupola could not confirm the thread received it.".into()
            }));
        }
        "pending" => {
            result["check"] = json!(format!(
                "It has not started yet (the thread is busy or waiting for the computer). Call {WAIT} with this messageId to learn whether it was delivered."
            ));
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// wait
// ---------------------------------------------------------------------------

const OPEN_STATES: [&str; 4] = [
    "accepted_by_wonder",
    "dispatching_to_codex",
    "accepted_by_codex",
    "streaming",
];

/// Live wait requests, shared by clones of one host state, never persisted.
#[derive(Default)]
pub struct ThreadWaits {
    entries: std::sync::Mutex<HashMap<uuid::Uuid, WaitDependency>>,
}

#[derive(Clone)]
struct WaitOwner {
    conversation: String,
    message: String,
    thread: Option<String>,
    turn: Option<String>,
}

impl WaitOwner {
    fn new(caller: &Caller) -> Self {
        Self {
            conversation: caller.message.conversation_id.clone(),
            message: caller.message.id.clone(),
            thread: caller.message.codex_thread_id.clone(),
            turn: caller.message.codex_turn_id.clone(),
        }
    }

    fn owns(&self, message: &wonder_store::StoredMessage) -> bool {
        self.thread.is_some()
            && self.turn.is_some()
            && message.conversation_id == self.conversation
            && message.codex_thread_id == self.thread
            && message.codex_turn_id == self.turn
            && matches!(message.state.as_str(), "accepted_by_codex" | "streaming")
    }

    async fn live(&self, state: &AppState) -> Result<bool, ToolError> {
        if !state
            .store
            .message_by_id(&self.message)
            .await?
            .is_some_and(|m| self.owns(&m))
        {
            return Ok(false);
        }
        // A reused thread (or a message rebound to another turn) must not
        // carry an older response's dependency into the new work.
        Ok(state
            .store
            .running_message(&self.conversation)
            .await?
            .is_some_and(|m| self.owns(&m)))
    }
}

#[derive(Clone)]
struct WaitDependency {
    owner: WaitOwner,
    target: String,
    messages: Vec<String>,
}

impl WaitDependency {
    async fn live(&self, state: &AppState) -> Result<bool, ToolError> {
        if !self.owner.live(state).await? {
            return Ok(false);
        }
        for id in &self.messages {
            if state.store.message_by_id(id).await?.is_some_and(|m| {
                m.conversation_id == self.target && OPEN_STATES.contains(&m.state.as_str())
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

struct WaitRegistration {
    waits: Arc<ThreadWaits>,
    request: uuid::Uuid,
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        // Synchronous cleanup also runs when a sleeping future is dropped or
        // its task is aborted. Only this request's edge may be removed.
        self.waits.entries.lock().unwrap().remove(&self.request);
    }
}

fn wait_cancelled() -> ToolError {
    ToolError::new(
        "cancelled",
        "The calling response ended before the wait did.",
    )
}

impl ThreadWaits {
    async fn register(
        self: &Arc<Self>,
        state: &AppState,
        owner: &WaitOwner,
        target: &str,
        messages: &[String],
    ) -> Result<WaitRegistration, ToolError> {
        let snapshot: Vec<_> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(id, edge)| (*id, edge.clone()))
            .collect();
        let mut ended = Vec::new();
        // No graph lock spans a store await. Revalidate actual message/turn
        // ownership, even if an older wait has not been polled since it ended.
        for (id, edge) in snapshot {
            if !edge.live(state).await? {
                ended.push(id);
            }
        }
        if !owner.live(state).await? {
            return Err(wait_cancelled());
        }
        let mut entries = self.entries.lock().unwrap();
        for id in ended {
            entries.remove(&id);
        }
        // Check and insert in one critical section: simultaneous closing
        // attempts cannot both pass. Newly registered edges are included too.
        let mut pending = vec![target];
        let mut visited = HashSet::new();
        while let Some(thread) = pending.pop() {
            if thread == owner.conversation {
                return Err(ToolError::new(
                    "wait_cycle",
                    "This wait would make threads wait on one another. Read the thread instead, or let an existing wait finish before waiting again.",
                ));
            }
            if visited.insert(thread) {
                pending.extend(
                    entries
                        .values()
                        .filter(|edge| edge.owner.conversation == thread)
                        .map(|edge| edge.target.as_str()),
                );
            }
        }
        let request = uuid::Uuid::new_v4();
        entries.insert(
            request,
            WaitDependency {
                owner: owner.clone(),
                target: target.to_owned(),
                messages: messages.to_vec(),
            },
        );
        Ok(WaitRegistration {
            waits: self.clone(),
            request,
        })
    }
}

/// Polls the store until every watched message has ended, the timeout passes
/// or the calling turn ends. A timeout is a normal answer, never a stop.
async fn wait_for(
    state: &AppState,
    caller: &Caller,
    thread: &StoredProjectConversation,
    watch: Vec<String>,
    timeout: Duration,
    focus: Option<&str>,
) -> Result<Value, ToolError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut open = watch;
    let had_work = !open.is_empty();
    let owner = WaitOwner::new(caller);
    let mut registration = None;
    loop {
        let mut still = Vec::new();
        for id in &open {
            match state.store.message_by_id(id).await? {
                Some(message) if OPEN_STATES.contains(&message.state.as_str()) => {
                    still.push(id.clone())
                }
                _ => {}
            }
        }
        open = still;
        if open.is_empty() {
            break;
        }
        if registration.is_none() {
            registration = Some(
                state
                    .thread_waits
                    .register(state, &owner, &thread.conversation_id, &open)
                    .await?,
            );
        } else if !owner.live(state).await? {
            return Err(wait_cancelled());
        }
        if tokio::time::Instant::now() >= deadline {
            let now = status(state, &thread.conversation_id).await?;
            return Ok(
                json!({"status":"still_running","threadId":thread.conversation_id,
                "threadStatus":now.status,"waitingOn":now.waiting_on,
                "note":"The thread is still working; nothing was stopped. Wait again or read it later."}),
            );
        }
        tokio::time::sleep(POLL).await;
    }
    drop(registration);
    let now = status(state, &thread.conversation_id).await?;
    let thread_status = now.status;
    let latest = state.store.latest_message(&thread.conversation_id).await?;
    let mut result = json!({"status": if had_work {"finished"} else {"idle"},
        "threadId":thread.conversation_id,"threadStatus":thread_status,"waitingOn":now.waiting_on,
        "outcome": latest.as_ref().map(|m| m.state.clone())});
    // Waiting on one message says how that message itself ended.
    if let Some(id) = focus {
        let mut delivery = json!({});
        report_delivery(state, id, &mut delivery).await?;
        result["messageState"] = delivery["deliveryState"].clone();
        if let Some(error) = delivery.get("error") {
            result["error"] = error.clone();
        }
    }
    let task = task_result(state, &thread.conversation_id).await;
    if had_work && thread_status == "idle" {
        if let Some((text, total)) = last_agent_message(state, &thread.conversation_id).await {
            // The task's own reply is already in taskResult.
            if task.as_ref().and_then(|t| t.text.as_deref()) != Some(&clip(&text, RESULT_CHARS)) {
                result["lastAgentMessage"] = json!(clip(&text, RESULT_CHARS));
                if total > RESULT_CHARS {
                    result["lastAgentMessageTruncated"] = json!(true);
                }
            }
        }
        observe(state, caller, thread).await;
    }
    if let Some(task) = task {
        result["taskResult"] = json!({"outcome": task.outcome, "finalReply": task.text,
            "finalReplyTruncated": task.truncated, "finishedAt": task.at});
    }
    Ok(result)
}

/// A delegated thread's result: recorded the first time its work is found
/// finished (by a wait or a wake-up), then read from the store, so it survives
/// a restart and no longer needs the provider's history. None for a thread
/// that was not delegated or is still working.
pub(crate) async fn task_result(
    state: &AppState,
    child: &str,
) -> Option<wonder_store::DelegationResult> {
    let store = &state.store;
    store.thread_delegation(child).await.ok()??;
    if let Some(recorded) = store.delegation_result(child).await.ok()? {
        return Some(recorded);
    }
    if !store.open_message_ids(child).await.ok()?.is_empty() {
        return None;
    }
    let latest = store.latest_message(child).await.ok()??;
    let outcome = match latest.state.as_str() {
        "completed" => "completed",
        "interrupted" => "stopped",
        _ => "failed",
    };
    let reply = last_agent_message(state, child).await;
    let result = wonder_store::DelegationResult {
        outcome: outcome.into(),
        truncated: reply
            .as_ref()
            .is_some_and(|(_, total)| *total > RESULT_CHARS),
        text: reply.map(|(text, _)| clip(&text, RESULT_CHARS)),
        at: now_text(),
    };
    store.record_delegation_result(child, &result).await.ok()?;
    store.delegation_result(child).await.ok()?
}

async fn last_agent_message(state: &AppState, conversation: &str) -> Option<(String, usize)> {
    let native = native_thread(state, conversation).await?;
    let mut last = None;
    walk(state, &native, |item| {
        if let Some(("agent", text)) = summarize(item) {
            if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                last = Some(text);
            }
        }
        false
    })
    .await
    .ok()?;
    last.map(|text| {
        let total = text.chars().count();
        (text, total)
    })
}

async fn wait(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    only_known(&args, &["threadId", "messageId", "timeoutSeconds"])?;
    let thread = target(state, caller, &text_arg(&args, "threadId", 128)?).await?;
    if thread.conversation_id == caller.conversation.conversation_id {
        return Err(ToolError::new(
            "invalid",
            "A thread cannot wait for itself.",
        ));
    }
    let timeout = int_arg(&args, "timeoutSeconds", DEFAULT_WAIT, 1, MAX_WAIT)?;
    let focus = args.get("messageId").and_then(Value::as_str);
    let watch = match focus {
        Some(id) => {
            let message = state
                .store
                .message_by_id(id)
                .await?
                .filter(|m| m.conversation_id == thread.conversation_id)
                .ok_or_else(|| ToolError::new("not_found", "No such message in that thread."))?;
            vec![message.id]
        }
        None => {
            state
                .store
                .open_message_ids(&thread.conversation_id)
                .await?
        }
    };
    let focus = watch.first().cloned().filter(|_| focus.is_some());
    wait_for(
        state,
        caller,
        &thread,
        watch,
        Duration::from_secs(timeout),
        focus.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// delegate
// ---------------------------------------------------------------------------

async fn delegate(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    only_known(
        &args,
        &[
            "clientRequestId",
            "prompt",
            "role",
            "provider",
            "model",
            "effort",
            "serviceTier",
            "accessMode",
            "claudeApproval",
            "title",
            "mode",
            "timeoutSeconds",
            "folder",
            "maxChildren",
        ],
    )?;
    forbid_plan_mode(caller)?;
    let cap = child_cap(&args, caller.runtime.is_some())?;
    let request = request_id(&args)?;
    let prompt = text_arg(&args, "prompt", 32 * 1024)?;
    // Validate supplied timeouts in both modes before creating or queueing
    // anything. Only omission selects the default; null is not an integer.
    let timeout = match args.get("timeoutSeconds") {
        None => DEFAULT_WAIT,
        Some(value) => value
            .as_u64()
            .filter(|value| (1..=MAX_WAIT).contains(value))
            .ok_or_else(|| {
                ToolError::new(
                    "invalid",
                    format!("timeoutSeconds must be 1 to {MAX_WAIT}."),
                )
            })?,
    };
    // The child gets the task and, unless it is general, a role: nothing of
    // this conversation. The stored message is exactly what the child reads.
    let task = match args.get("role") {
        None => prompt.clone(),
        Some(Value::String(role)) if ROLES.contains(&role.as_str()) => {
            if role == "general" {
                prompt.clone()
            } else {
                format!("Act as the {role} sub-agent for this task.\n\n{prompt}")
            }
        }
        Some(_) => return Err(ToolError::new("invalid", "role is not valid.")),
    };
    let provider = args.get("provider").and_then(Value::as_str);
    let Some(family) = delegate_families().find(|family| Some(family.as_str()) == provider) else {
        let names: Vec<_> = delegate_families().map(AgentFamily::as_str).collect();
        return Err(ToolError::new(
            "invalid",
            format!("provider must be {}.", names.join(" or ")),
        ));
    };
    let parent = &caller.conversation;
    let id = derived_uuid(&["delegate", &parent.conversation_id, &request]);
    let existing = state
        .store
        .thread_delegation_by_request(&parent.conversation_id, &request)
        .await?;
    if existing.is_none() {
        if state
            .store
            .thread_delegation_depth(&parent.conversation_id)
            .await?
            >= MAX_DEPTH
        {
            return Err(ToolError::new(
                "depth_limit",
                "Delegated threads can delegate at most three levels deep.",
            ));
        }
        // An early answer for the common case; the reservation below is the
        // check that holds when several requests arrive at once.
        if state
            .store
            .thread_delegation_count(&parent.conversation_id)
            .await?
            >= cap
        {
            return Err(too_many_threads(cap));
        }
    }
    // Access: the default is the parent's own, and nothing may be wider. A
    // provider without access choices (OpenCode) runs under its own settings
    // on the Mac, recorded as Workspace, so it needs at least that much.
    let fixed = !family.provider().capabilities.access_choices;
    let requested = args.get("accessMode").and_then(Value::as_str);
    if fixed && requested.is_some_and(|mode| mode != "workspace") {
        return Err(ToolError::new(
            "invalid",
            format!(
                "{} uses its own permission settings on the computer; omit accessMode.",
                family.provider().display_name
            ),
        ));
    }
    let access = if fixed {
        "workspace"
    } else {
        requested.unwrap_or(&parent.access_mode)
    };
    if !["read_only", "workspace", "full_access"].contains(&access) {
        return Err(ToolError::new("invalid", "accessMode is not valid."));
    }
    if access_rank(access) > access_rank(&parent.access_mode) {
        return Err(ToolError::new(
            "permission_escalation",
            format!(
                "A delegated thread cannot have wider access than this one ({}).",
                parent.access_mode
            ),
        ));
    }
    let approval = match args.get("claudeApproval").and_then(Value::as_str) {
        Some(value) => {
            if !family.allows_approval_choice() {
                return Err(ToolError::new(
                    "invalid",
                    "Approval modes are available for Claude threads.",
                ));
            }
            if !family.provider().approval_modes.contains(&value) {
                return Err(ToolError::new("invalid", "claudeApproval is not valid."));
            }
            if access == "workspace" && approval_rank(value) > approval_ceiling(parent) {
                return Err(ToolError::new(
                    "permission_escalation",
                    "A delegated thread cannot ask for approval less often than this one.",
                ));
            }
            value.to_owned()
        }
        None => default_approval(family, parent),
    };
    may_use_own_settings(parent, family)?;
    if !crate::claude::available(state, family) {
        return Err(ToolError::new(
            "unavailable_provider",
            format!(
                "{} is not installed on this computer.",
                family.provider().display_name
            ),
        ));
    }
    let readiness = state.ingestion.project_readiness(&state.store).await;
    if !readiness.ready {
        return Err(ToolError::new("unavailable_provider", readiness.detail));
    }
    let catalog = state.runtime_catalog.read().await.clone();
    let requested_tier = args.get("serviceTier").and_then(Value::as_str);
    if requested_tier
        .is_some_and(|tier| tier_rank(Some(tier)) > tier_rank(parent.service_tier.as_deref()))
    {
        return Err(ToolError::new(
            "permission_escalation",
            "A delegated thread cannot use a faster speed than this one. Omit serviceTier or choose default.",
        ));
    }
    let settings = |tier: Option<&str>| {
        projects::execution_settings(
            &catalog,
            family,
            args.get("model").and_then(Value::as_str),
            args.get("effort").and_then(Value::as_str),
            tier,
        )
    };
    let parent_rank = tier_rank(parent.service_tier.as_deref());
    let (model, effort, tier) = settings(requested_tier)
        .map_err(|detail| {
        let choices: Vec<_> = listed_models(&catalog, Some(family))
            .take(MODEL_CHOICES)
            .map(|option| option.id.as_str())
            .collect();
        let choices = match choices.as_slice() {
            [] => format!("{} lists no models right now.", family.provider().display_name),
            ids => format!("Valid models include: {}. wonder_models lists every model with its efforts and speeds.", ids.join(", ")),
        };
        ToolError::new("unavailable_model", format!("{detail} {choices}"))
    })?;
    // An omitted speed resolves to the owner's preferred one, which may be
    // faster than this thread's: take the first the model offers within it.
    let (model, effort, tier) = if requested_tier.is_none()
        && tier_rank(tier.as_deref()) > parent_rank
    {
        [
            parent.service_tier.as_deref(),
            Some("default"),
            Some("standard"),
            Some("flex"),
        ]
        .into_iter()
        .flatten()
        .filter(|tier| tier_rank(Some(tier)) <= parent_rank)
        .find_map(|tier| {
            settings(Some(tier))
                .ok()
                .filter(|resolved| resolved.2.as_deref() == Some(tier))
        })
        .ok_or_else(|| {
            ToolError::new(
                "permission_escalation",
                format!("{model} offers no speed as slow as this thread's. Choose another model."),
            )
        })?
    } else {
        (model, effort, tier)
    };
    // A supplied speed the model does not offer is replaced by its default
    // when the model is resolved; say so rather than ignore it.
    if let Some(requested) = requested_tier {
        if tier.as_deref() != Some(requested) {
            return Err(ToolError::new(
                "unavailable_model",
                format!(
                    "{model} does not offer the speed {requested}. wonder_models lists its speeds."
                ),
            ));
        }
    }
    let title: String = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            prompt
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Delegated task")
                .trim()
                .to_owned()
        })
        .chars()
        .take(80)
        .collect();
    let project = &caller.project;
    // The child works in the parent's folder unless a folder is named: one of
    // the Project's saved folders or a Git worktree of one.
    let wanted = match args.get("folder") {
        None => parent.cwd.clone(),
        Some(Value::String(folder)) => folder.trim().to_owned(),
        Some(_) => return Err(ToolError::new("invalid", "folder must be a path.")),
    };
    let in_project = if args.get("folder").is_some() {
        crate::projects::thread_project(state, project.clone(), &wanted).await
    } else {
        project.clone()
    };
    let project = &in_project;
    let root = project.root_for(&wanted).ok_or_else(|| {
        if args.get("folder").is_some() {
            ToolError::new(
                "unknown_folder",
                format!("{wanted} is not a folder or worktree of this Project."),
            )
        } else {
            ToolError::new(
                "project_unavailable",
                "This thread's folder is no longer in the Project.",
            )
        }
    })?;
    let checked = project.clone();
    let denied = state.denied_roots.clone();
    tokio::task::spawn_blocking(move || projects::validate_execution_roots(&checked, &denied))
        .await
        .map_err(|_| ToolError::new("unavailable", "Project folders could not be checked."))?
        .map_err(|detail| ToolError::new("project_unavailable", detail))?;
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return Err(ToolError::new(
            "updating",
            "Cupola is preparing to update. Try again shortly.",
        ));
    };
    let now = now_text();
    let digest = hex::encode(Sha256::digest(
        json!([
            "delegate",
            parent.conversation_id,
            family.as_str(),
            model,
            effort,
            tier,
            access,
            approval,
            title,
            root.path,
            cap,
            hex::encode(Sha256::digest(task.as_bytes()))
        ])
        .to_string()
        .as_bytes(),
    ));
    let created = state
        .store
        .create_delegated_conversation(
            ProjectConversationInsert {
                conversation_id: &id,
                project_id: &project.id,
                family,
                provider_store: crate::providers::provider_store(state, family),
                native_session_id: None,
                cwd: &root.path,
                roots_revision: project.roots_revision,
                title: &title,
                model: Some(&model),
                effort: effort.as_deref(),
                service_tier: tier.as_deref(),
                access_mode: access,
                claude_approval: &approval,
                plan_mode: false,
                creation_request_id: Some(&id),
                now: &now,
            },
            &digest,
            &parent.conversation_id,
            &request,
            &caller.message.device_id,
            cap,
        )
        .await;
    let (child, new_thread) =
        match created {
            Ok(Some(ProjectConversationCreate::Created(child))) => (child, true),
            Ok(Some(ProjectConversationCreate::Existing(child))) => (child, false),
            Ok(None) => return Err(too_many_threads(cap)),
            Err(sqlx::Error::Protocol(_)) => return Err(ToolError::new(
                "request_reused",
                "That clientRequestId was already used for a different delegation. Use a new one.",
            )),
            Err(error) => return Err(error.into()),
        };
    let source = NewMessageSource {
        kind: "delegation",
        conversation: Some(&parent.conversation_id),
        title: &parent.title,
        wake_children: &[],
        wake_parent: None,
    };
    let message =
        match state
            .store
            .insert_agent_message(
                &caller.message.device_id,
                &id,
                &task,
                &child.conversation_id,
                &now,
                &source,
            )
            .await?
        {
            MessageInsert::Inserted(message) | MessageInsert::Existing(message) => message,
            MessageInsert::Conflict => return Err(ToolError::new(
                "request_reused",
                "That clientRequestId was already used for a different delegation. Use a new one.",
            )),
        };
    let _ = state.store.touch_project(&project.id, family, &now).await;
    let mut result = json!({"threadId":child.conversation_id,"messageId":message.id,
        "title":child.title,"provider":provider_name(family),"model":child.model,
        "effort":child.effort,"serviceTier":child.service_tier,"workspace":child.cwd,
        "accessMode":child.access_mode,"created":new_thread,"status":"queued"});
    if args.get("mode").and_then(Value::as_str) == Some("wait") {
        drop(_admission);
        let waited = wait_for(
            state,
            caller,
            &child,
            vec![message.id.clone()],
            Duration::from_secs(timeout),
            Some(&message.id),
        )
        .await?;
        if let (Some(into), Some(from)) = (result.as_object_mut(), waited.as_object()) {
            into.extend(from.clone());
        }
    }
    Ok(result)
}

fn too_many_threads(cap: i64) -> ToolError {
    let noun = if cap == 1 { "task" } else { "tasks" };
    ToolError::new(
        "too_many_threads",
        format!("This thread has already delegated {cap} {noun}. Reuse a finished thread with wonder_thread_send, or ask the owner to raise the limit."),
    )
}

/// How many children a thread may have: the owner's `maxChildren` (CLI or
/// app), else the host's `CUPOLA_DELEGATE_MAX_CHILDREN`, else 16; at most 64.
/// An agent's own `maxChildren` can only lower the host limit, never raise it.
fn child_cap(args: &Value, from_agent: bool) -> Result<i64, ToolError> {
    let invalid = || {
        ToolError::new(
            "invalid",
            format!("maxChildren must be 1 to {CHILD_CAP_CEILING}."),
        )
    };
    let host = std::env::var(CHILD_CAP_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|cap| (1..=CHILD_CAP_CEILING).contains(cap))
        .unwrap_or(MAX_CHILDREN);
    let cap = match args.get("maxChildren") {
        Some(value) => {
            let asked = value.as_i64().ok_or_else(invalid)?;
            if !(1..=CHILD_CAP_CEILING).contains(&asked) {
                return Err(invalid());
            }
            if from_agent {
                asked.min(host)
            } else {
                asked
            }
        }
        None => host,
    };
    Ok(cap)
}

/// Speeds by cost: flex is slower than the default, any other named speed
/// (priority, fast) is faster. No speed is the default.
fn tier_rank(tier: Option<&str>) -> u8 {
    match tier {
        Some("flex") => 0,
        None | Some("default") | Some("standard") => 1,
        Some(_) => 2,
    }
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

/// The models an agent may choose: the catalog without hidden models, for
/// one provider or all. The outside MCP listing uses the same filter.
pub(super) fn listed_models(
    catalog: &crate::RuntimeCatalog,
    family: Option<AgentFamily>,
) -> impl Iterator<Item = &crate::ModelOption> {
    catalog
        .models
        .iter()
        .filter(move |option| !option.hidden && family.is_none_or(|f| option.agent_family == f))
}

async fn models(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    only_known(&args, &["provider", "search", "limit"])?;
    let limit = int_arg(&args, "limit", 20, 1, 50)? as usize;
    let family = match args.get("provider") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            delegate_families()
                .find(|family| value.as_str() == Some(family.as_str()))
                .ok_or_else(|| {
                    let names: Vec<_> = delegate_families().map(AgentFamily::as_str).collect();
                    ToolError::new(
                        "invalid",
                        format!("provider must be {}.", names.join(" or ")),
                    )
                })?,
        ),
    };
    let search = match args.get("search") {
        None | Some(Value::Null) => None,
        Some(_) => Some(text_arg(&args, "search", 100)?.to_lowercase()),
    };
    let readiness = state.ingestion.project_readiness(&state.store).await;
    let providers: Vec<_> = delegate_families()
        .filter(|f| family.is_none_or(|only| only == *f))
        .map(|f| {
            let reason = if !crate::claude::available(state, f) {
                Some(format!("{} is not installed on this computer.", f.provider().display_name))
            } else if !readiness.ready {
                Some(readiness.detail.to_owned())
            } else {
                None
            };
            json!({"id": f.as_str(), "name": f.provider().display_name,
                "available": reason.is_none(), "unavailableReason": reason,
                "accessChoices": f.provider().capabilities.access_choices,
                "needsFullAccess": !f.permissions_enforced(),
                "approvalModes": if f.allows_approval_choice() { json!(f.provider().approval_modes) } else { json!([]) }})
        })
        .collect();
    let catalog = state.runtime_catalog.read().await.clone();
    let ids =
        |choices: &[crate::ChoiceOption]| choices.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
    let mut matching = listed_models(&catalog, family)
        .filter(|option| delegate_families().any(|f| f == option.agent_family))
        .filter(|option| {
            search.as_deref().is_none_or(|text| {
                option.id.to_lowercase().contains(text)
                    || option.display_name.to_lowercase().contains(text)
            })
        });
    let mut items = Vec::new();
    for option in matching.by_ref().take(limit) {
        // The settings wonder_delegate picks when only the model is given.
        let defaults =
            agent_defaults::resolve(&catalog, option.agent_family, Some(&option.id), None, None);
        items.push(json!({"provider": option.agent_family.as_str(), "id": option.id,
            "name": option.display_name, "description": option.description,
            "isDefault": option.is_default,
            "efforts": ids(&option.reasoning_efforts), "defaultEffort": defaults.effort,
            "serviceTiers": ids(&option.service_tiers), "defaultServiceTier": defaults.service_tier}));
    }
    let parent = &caller.conversation;
    Ok(
        json!({"providers": providers, "models": items, "hasMore": matching.next().is_some(),
        "inherited": {"provider": provider_name(parent.family), "model": parent.model,
            "effort": parent.effort, "serviceTier": parent.service_tier,
            "accessMode": parent.access_mode,
            "claudeApproval": parent.family.allows_approval_choice().then_some(&parent.claude_approval),
            "note": "A delegated thread's access and approval default to yours and cannot be wider; its speed cannot be faster than yours."}}),
    )
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// Synthetic stop receipts belong to this daemon process, not a provider
/// runtime. A new process can close abandoned receipts without redoing them.
static STOP_OWNER: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("agent-stop:{}", uuid::Uuid::new_v4()));

/// Stops a thread's running response through the same path as the phone's
/// Stop. Shared with the outside MCP stop tool.
pub(super) async fn stop_running(state: &AppState, conversation: &str) -> Result<Value, ToolError> {
    let Some(turn) = state
        .store
        .running_message(conversation)
        .await?
        .and_then(|m| m.codex_turn_id)
    else {
        return Ok(json!({"stopped":false,"status":"idle"}));
    };
    let response =
        crate::interrupt_turn(State(state.clone()), Path((conversation.to_owned(), turn))).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .map_err(|_| ToolError::new("unavailable", "The result was too large."))?;
    if !status.is_success() {
        return Err(ToolError::new(
            "unavailable",
            crate::history::sanitize_ansi_and_secrets(&String::from_utf8_lossy(&bytes), 2000),
        ));
    }
    Ok(serde_json::from_slice(&bytes).unwrap_or(json!({"stopped":true})))
}

/// Whether `ancestor` delegated `thread`, directly or through its children.
async fn delegated_by(state: &AppState, ancestor: &str, thread: &str) -> Result<bool, ToolError> {
    let mut current = thread.to_owned();
    for _ in 0..=MAX_DEPTH {
        match state.store.thread_delegation(&current).await? {
            Some(link) if link.parent_conversation_id == ancestor => return Ok(true),
            Some(link) => current = link.parent_conversation_id,
            None => return Ok(false),
        }
    }
    Ok(false)
}

async fn stop(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
    only_known(&args, &["threadId", "cancelQueued", "clientRequestId"])?;
    let request = request_id(&args)?;
    let thread = target(state, caller, &text_arg(&args, "threadId", 128)?).await?;
    if thread.conversation_id == caller.conversation.conversation_id {
        return Err(ToolError::new(
            "invalid",
            "A thread cannot stop itself. Finish your response instead.",
        ));
    }
    if !delegated_by(
        state,
        &caller.conversation.conversation_id,
        &thread.conversation_id,
    )
    .await?
    {
        return Err(ToolError::new(
            "not_delegated",
            "You can stop only threads you delegated, directly or through your delegated threads.",
        ));
    }
    let cancel_queued = match args.get("cancelQueued") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(ToolError::new(
                "invalid",
                "cancelQueued must be true or false.",
            ))
        }
    };
    // The stop runs once per clientRequestId: a retry replays its result
    // rather than stop whatever the thread is doing by then.
    let key = hex::encode(Sha256::digest(
        json!(["stop", caller.conversation.conversation_id, request]).to_string(),
    ));
    let hash = hex::encode(Sha256::digest(
        json!([thread.conversation_id, cancel_queued]).to_string(),
    ));
    let reused = || {
        ToolError::new(
            "request_reused",
            "That clientRequestId was already used for a different request. Use a new one.",
        )
    };
    let abandoned = json!({"error":{
        "code":"unavailable",
        "message":"Cupola restarted before saving this stop's result, so its outcome is unknown. Check the thread with wonder_thread_list or wonder_thread_read. To stop its current work, use a new clientRequestId."
    }});
    state
        .store
        .recover_abandoned_stop_call(&key, &hash, &STOP_OWNER, &abandoned.to_string())
        .await?;
    // One atomic claim: only the attempt that inserted the row stops the
    // thread; a retry, even one overlapping the first, gets that row.
    let claimed = state
        .store
        .claim_tool_call(
            &key,
            &hash,
            &caller.message.id,
            // No runtime made this record; the label names what did.
            &STOP_OWNER,
            STOP,
            &now_text(),
        )
        .await?;
    match claimed {
        Some((stored, _)) if stored != hash => return Err(reused()),
        Some((_, Some(response))) => {
            let stored: Value = serde_json::from_str(&response).unwrap_or_default();
            return match stored.get("error") {
                // Only a failed stop is stored as an error.
                Some(error) => Err(ToolError::new(
                    "unavailable",
                    error["message"].as_str().unwrap_or_default().to_owned(),
                )),
                None => Ok(stored),
            };
        }
        Some((_, None)) => {
            return Err(ToolError::new(
                "unavailable",
                "That stop is still in progress. Wait for it, or check the thread with wonder_thread_list.",
            ))
        }
        None => {}
    }
    let outcome = stop_thread(state, &thread.conversation_id, cancel_queued).await;
    let stored = match &outcome {
        Ok(result) => result.clone(),
        Err(error) => json!({"error":{"code":error.code,"message":error.message}}),
    };
    state
        .store
        .complete_tool_call(&key, &hash, &stored.to_string())
        .await?;
    outcome
}

/// How long a stop waits for a task that is being started to get its turn.
const STOP_START_WAIT: Duration = Duration::from_secs(30);

async fn stop_thread(
    state: &AppState,
    conversation: &str,
    cancel_queued: bool,
) -> Result<Value, ToolError> {
    // Cancel first so the queue cannot start its next message once the
    // running one stops.
    let mut cancelled = 0;
    if cancel_queued {
        cancelled += cancel_queue(state, conversation).await?;
    }
    // A task being started has no turn to stop yet: wait until its start
    // ends, then stop the turn it started or cancel it if it was put back.
    let deadline = tokio::time::Instant::now() + STOP_START_WAIT;
    while state.store.starting_message(conversation).await? {
        if tokio::time::Instant::now() >= deadline {
            return Err(ToolError::new(
                "unavailable",
                "The thread is still starting its task. Try again shortly.",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if cancel_queued {
        cancelled += cancel_queue(state, conversation).await?;
    }
    let mut result = stop_running(state, conversation).await?;
    result["threadId"] = json!(conversation);
    if cancel_queued {
        result["cancelledQueuedMessages"] = json!(cancelled);
    }
    Ok(result)
}

async fn cancel_queue(state: &AppState, conversation: &str) -> Result<u64, ToolError> {
    let mut cancelled = 0;
    for item in state.store.pending_queue(conversation).await? {
        if state
            .store
            .edit_pending(conversation, &item.id, item.revision, None)
            .await?
        {
            cancelled += 1;
        }
    }
    Ok(cancelled)
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Exposes existing Project handlers to the authenticated network transport.
/// This ephemeral caller is never a stored thread, runtime, phone or owner
/// identity. The transport checks OAuth scopes before entering this adapter.
pub(super) async fn outside_call(
    state: &AppState,
    grant: &wonder_store::mcp::Grant,
    project: StoredProject,
    tool: &str,
    args: Value,
) -> Result<Value, ToolError> {
    let identity = format!("mcp:{}", grant.client_id);
    let root = project.primary_root().path.clone();
    let conversation = StoredProjectConversation {
        conversation_id: identity.clone(),
        project_id: project.id.clone(),
        family: AgentFamily::Claude,
        provider_store: String::new(),
        native_session_id: None,
        cwd: root,
        roots_revision: project.roots_revision,
        title: grant.client_name.clone(),
        title_chosen: true,
        model: None,
        effort: None,
        service_tier: None,
        access_mode: grant.permission_mode.clone(),
        claude_approval: if grant.permission_mode == "full_access" {
            "auto"
        } else {
            "ask"
        }
        .into(),
        plan_mode: false,
        native_settings_turn: None,
        is_pinned: false,
        has_unread: false,
        wonder_archived: false,
        creation_request_id: None,
        created_at: String::new(),
        updated_at: String::new(),
        last_activity_at: String::new(),
    };
    let caller = Caller {
        runtime: None,
        message: wonder_store::StoredMessage {
            id: identity.clone(),
            device_id: "wonder-desktop".into(),
            client_message_id: identity,
            body: String::new(),
            body_sha256: String::new(),
            conversation_id: conversation.conversation_id.clone(),
            state: String::new(),
            created_at: String::new(),
            codex_thread_id: None,
            codex_turn_id: None,
        },
        conversation,
        project,
    };
    match tool {
        LIST => list(state, &caller, args).await,
        READ => read(state, &caller, args).await,
        SEND => {
            let thread = target(state, &caller, &text_arg(&args, "threadId", 128)?).await?;
            outside_authority(grant, &thread)?;
            state.store.ensure_local_desktop(&now_text()).await?;
            send_from(
                state,
                &caller,
                args,
                NewMessageSource {
                    kind: "thread",
                    conversation: None,
                    title: &grant.client_name,
                    wake_children: &[],
                    wake_parent: None,
                },
            )
            .await
        }
        _ => Err(ToolError::new(
            "unknown_tool",
            "That tool is not exposed to outside agents.",
        )),
    }
}

/// The owner (`cupola delegate`, `cupola-worker`) hands a task to a new child
/// of `parent`, exactly as the parent's own agent would with `wonder_delegate`:
/// the child appears in the parent's thread as an agent task, and the same
/// access, depth and child-count limits apply.
pub(crate) async fn owner_delegate(
    state: &AppState,
    parent: &str,
    args: Value,
) -> Result<Value, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let missing = || (StatusCode::NOT_FOUND, "No such project thread.".to_owned());
    let conversation = state
        .store
        .project_conversation(parent)
        .await
        .map_err(|_| missing())?
        .ok_or_else(missing)?;
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .map_err(|_| missing())?
        .filter(|project| project.is_included)
        .ok_or_else(missing)?;
    let project = projects::thread_project(state, project, &conversation.cwd).await;
    state
        .store
        .ensure_local_desktop(&now_text())
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cupola could not read its data.".to_owned(),
            )
        })?;
    let identity = uuid::Uuid::new_v4().to_string();
    let caller = Caller {
        runtime: None,
        message: wonder_store::StoredMessage {
            id: identity.clone(),
            device_id: "wonder-desktop".into(),
            client_message_id: identity,
            body: String::new(),
            body_sha256: String::new(),
            conversation_id: conversation.conversation_id.clone(),
            state: String::new(),
            created_at: String::new(),
            codex_thread_id: None,
            codex_turn_id: None,
        },
        conversation,
        project,
    };
    delegate(state, &caller, args).await.map_err(|error| {
        let status = match error.code {
            "unavailable" | "unavailable_provider" | "updating" => StatusCode::SERVICE_UNAVAILABLE,
            "too_many_threads" | "depth_limit" | "request_reused" => StatusCode::CONFLICT,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, error.message)
    })
}

/// Fixed-permission providers run under settings outside Wonder's control;
/// only an explicit Full access grant may direct them.
pub(super) fn outside_authority(
    grant: &wonder_store::mcp::Grant,
    thread: &StoredProjectConversation,
) -> Result<(), ToolError> {
    if access_rank(&thread.access_mode) > access_rank(&grant.permission_mode)
        || (!thread.family.provider().capabilities.access_choices
            && grant.permission_mode != "full_access")
        || (thread.access_mode == "workspace"
            && approval_rank(&thread.claude_approval) > 0
            && grant.permission_mode != "full_access")
    {
        return Err(ToolError::new(
            "permission_escalation",
            "That thread can act with wider permissions than this agent was granted.",
        ));
    }
    Ok(())
}

/// A tool call by the agent working on `message` (tests in other modules).
#[cfg(test)]
pub(crate) async fn call_for_test(
    state: &AppState,
    runtime: &str,
    message: &str,
    tool: &str,
    args: Value,
) -> Value {
    let message = state.store.message_by_id(message).await.unwrap().unwrap();
    let conversation = state
        .store
        .project_conversation(&message.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .unwrap()
        .unwrap();
    let caller = Caller {
        runtime: Some(runtime.to_owned()),
        message,
        conversation,
        project,
    };
    response(execute(state, &caller, tool, args).await)
}

/// A tool call outside a runtime request (tests): a fresh call identity.
#[cfg(test)]
async fn execute(
    state: &AppState,
    caller: &Caller,
    tool: &str,
    args: Value,
) -> Result<Value, ToolError> {
    let call = uuid::Uuid::new_v4().to_string();
    execute_call(state, caller, tool, &call, args).await
}

async fn execute_call(
    state: &AppState,
    caller: &Caller,
    tool: &str,
    call_id: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match tool {
        SHOW_VISUAL => visual::show(state, caller, call_id, args).await,
        REQUEST_SECRET => secret::request(state, caller, call_id, args).await,
        secret::RUN => secret::run(state, caller, args).await,
        LIST => list(state, caller, args).await,
        READ => read(state, caller, args).await,
        SEND => send(state, caller, args).await,
        WAIT => wait(state, caller, args).await,
        DELEGATE => delegate(state, caller, args).await,
        SCHEDULE => schedule::schedule(state, caller, args).await,
        SCHEDULES => schedule::schedules(state, caller, args).await,
        SCHEDULE_UPDATE => schedule::update(state, caller, args).await,
        NOTIFY => notify::notify(state, caller, args).await,
        PREFERENCES => preferences::run(state, caller, args).await,
        T3_THREAD => t3::run(state, caller, args).await,
        MODELS => models(state, caller, args).await,
        STOP => stop(state, caller, args).await,
        TAKEOVER => request_takeover(state, caller, args).await,
        STREAM_SCREEN => stream::run(state, caller, args).await,
        _ => Err(ToolError::new(
            "unknown_tool",
            "That tool is not registered.",
        )),
    }
}

/// Acknowledges the runtime's request at once and answers from a task. A wait
/// can last half an hour and a history read can be slow; neither may hold the
/// shared notification consumer, which also delivers the finish they wait for.
pub(super) async fn handle(
    state: &AppState,
    notification: &Value,
    params: &Value,
    message: Option<&wonder_store::StoredMessage>,
) -> bool {
    let Some(runtime) = notification.get("_wonderRuntimeId").and_then(Value::as_str) else {
        return true;
    };
    let mut actual = params.clone();
    actual["_wonderRuntimeId"] = json!(runtime);
    let Some(client) = state.ingestion.approval_client(&actual) else {
        return true;
    };
    let Some(request_id) = notification.get("id").cloned() else {
        return true;
    };
    let rpc = client.lock().await.rpc();
    if rpc.health().id() != runtime {
        return true;
    }
    let (state, runtime, params, message) = (
        state.clone(),
        runtime.to_owned(),
        params.clone(),
        message.cloned(),
    );
    tokio::spawn(async move {
        let value = handle_call(&state, &runtime, &params, message.as_ref()).await;
        if rpc.health().id() == runtime {
            let _ = rpc.respond_value(request_id, Some(value), None).await;
        }
    });
    true
}

async fn handle_call(
    state: &AppState,
    runtime: &str,
    params: &Value,
    message: Option<&wonder_store::StoredMessage>,
) -> Value {
    let fail = |error: ToolError| response(Err(error));
    let Some(message) = message else {
        return fail(ToolError::new(
            "uncorrelated",
            "The request is not correlated with accepted work.",
        ));
    };
    let (Some(thread), Some(turn)) = (
        params.get("threadId").and_then(Value::as_str),
        params.get("turnId").and_then(Value::as_str),
    ) else {
        return fail(ToolError::new(
            "uncorrelated",
            "Thread and turn identity are required.",
        ));
    };
    if message.codex_turn_id.as_deref() != Some(turn) {
        return fail(ToolError::new(
            "wrong_turn",
            "The request targets different work.",
        ));
    }
    let tool = params
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Asked before every computer-use action, so it is neither counted
    // against the turn's calls nor saved, and works outside Projects too.
    if tool == CONTROL_CHECK {
        return response(control_check(state, runtime, message).await);
    }
    if !registered(tool) {
        return fail(ToolError::new(
            "unknown_tool",
            "That tool is not registered.",
        ));
    }
    let caller = match caller(state, runtime, thread, message).await {
        Ok(caller) => caller,
        Err(error) => return fail(error),
    };
    // A Codex thread keeps the tools it started with; while the owner has
    // them off for its provider, Wonder declines every call.
    if !crate::agent_settings::settings(state, caller.conversation.family)
        .await
        .thread_tools
    {
        return fail(ToolError::new(
            "turned_off",
            format!(
                "Cupola's thread tools are turned off for {} in Cupola's settings.",
                caller.conversation.family.provider().display_name
            ),
        ));
    }
    let Some(call_id) = params
        .get("callId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256)
    else {
        return fail(ToolError::new(
            "uncorrelated",
            "A stable runtime call identity is required.",
        ));
    };
    let key = hex::encode(Sha256::digest(
        json!([runtime, thread, turn, call_id]).to_string(),
    ));
    let hash = hex::encode(Sha256::digest(
        json!({"tool":tool,"arguments":params.get("arguments")}).to_string(),
    ));
    match state.store.tool_call(&key).await {
        Ok(Some((saved, _))) if saved != hash => {
            return fail(ToolError::new(
                "call_reused",
                "This runtime call was already used with different arguments.",
            ))
        }
        // A finished call replays its saved answer, errors included.
        Ok(Some((_, Some(answer)))) => {
            if let Ok(value) = serde_json::from_str(&answer) {
                return value;
            }
        }
        Ok(_) => {}
        Err(error) => return fail(error.into()),
    }
    if !matches!(message.state.as_str(), "accepted_by_codex" | "streaming") {
        return fail(ToolError::new(
            "turn_ended",
            "This turn is no longer active.",
        ));
    }
    match state
        .store
        .reserve_tool_call(
            &key,
            &hash,
            &message.id,
            runtime,
            tool,
            &now_text(),
            CALLS_PER_TURN,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return fail(ToolError::new(
                "call_limit",
                "This turn reached its limit of thread tool calls. Finish your response first.",
            ))
        }
        Err(error) => return fail(error.into()),
    }
    let args = match arguments(params) {
        Ok(args) => args,
        Err(detail) => return fail(ToolError::new("invalid", detail)),
    };
    let value = response(execute_call(state, &caller, tool, call_id, args).await);
    // Sends and delegations are idempotent by their own request ids, so a
    // crash before this save repeats nothing.
    let _ = state
        .store
        .complete_tool_call(&key, &hash, &value.to_string())
        .await;
    value
}

// ---------------------------------------------------------------------------
// takeover
// ---------------------------------------------------------------------------

/// Whether the owner holds phone control of this computer now. Answered only
/// for a live runtime's running work.
async fn control_check(
    state: &AppState,
    runtime: &str,
    message: &wonder_store::StoredMessage,
) -> Result<Value, ToolError> {
    if !state.ingestion.runtime_alive(runtime) {
        return Err(ToolError::new(
            "wrong_runtime",
            "The request does not belong to this live runtime.",
        ));
    }
    if !matches!(message.state.as_str(), "accepted_by_codex" | "streaming") {
        return Err(ToolError::new(
            "turn_ended",
            "This turn is no longer active.",
        ));
    }
    let held = computer_sessions::owner_has_control(state).await?;
    Ok(json!({"ownerHasControl": held}))
}

/// Runtime routing: Claude and the ACP agents run computer use through
/// Cupola's bridges, which ask `control_check` first. Codex runs it itself,
/// outside Cupola, so Cupola cannot pause it.
fn computer_use_fenced(family: AgentFamily) -> bool {
    family != AgentFamily::Codex
}

async fn request_takeover(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &["reason", "app", "timeoutSeconds", "clientRequestId"],
    )?;
    let reason = text_arg(&args, "reason", 500)?;
    let app = match args.get("app") {
        None | Some(Value::Null) => None,
        Some(_) => Some(text_arg(&args, "app", 120)?),
    };
    let timeout = int_arg(&args, "timeoutSeconds", DEFAULT_WAIT, 1, MAX_WAIT)?;
    let request = request_id(&args)?;
    let id = derived_uuid(&["takeover", &caller.conversation.conversation_id, &request]);
    let family = caller.conversation.family;
    computer_sessions::request_takeover(
        state,
        computer_sessions::TakeoverOpen {
            id: &id,
            conversation_id: &caller.conversation.conversation_id,
            agent: family.provider().display_name,
            reason: &reason,
            app: app.as_deref(),
            timeout: Duration::from_secs(timeout),
        },
    )
    .await;
    let owner = WaitOwner::new(caller);
    let outcome = loop {
        match computer_sessions::takeover_state(state, &id).await {
            Some(next) if next.ended() => break next,
            Some(_) => {}
            // Evicted or lost: nothing more can answer it.
            None => break computer_sessions::TakeoverState::Expired,
        }
        if !owner.live(state).await? {
            if state.computer_supervisor.cancel_takeover(&id, Utc::now()) {
                computer_sessions::withdraw_takeover_push(state, &id).await;
            }
            return Err(wait_cancelled());
        }
        tokio::time::sleep(POLL).await;
    };
    let mut result = json!({"status": outcome.as_str(), "requestId": id});
    let note = match outcome {
        computer_sessions::TakeoverState::Returned => {
            "The owner gave control back. Take a fresh look before continuing; the screen may have changed."
        }
        computer_sessions::TakeoverState::Declined => {
            "The owner declined. Continue without it, or explain in your reply what you need."
        }
        computer_sessions::TakeoverState::Cancelled => {
            "The reply that requested control ended, so this request was cancelled."
        }
        computer_sessions::TakeoverState::Expired
            if computer_sessions::owner_has_control(state)
                .await
                .unwrap_or(false) =>
        {
            result["ownerHasControl"] = json!(true);
            "The owner still has control. Wait before using the computer, or ask again."
        }
        _ => "The takeover request expired before control was given back. Ask again later, or continue without it.",
    };
    result["note"] = json!(note);
    if !computer_use_fenced(family) {
        result["computerUseFenced"] = json!(false);
        result["computerUseNote"] = json!("Cupola cannot pause Codex's own computer use while the owner has control. Do not use the computer until this tool returns.");
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// wake-ups
// ---------------------------------------------------------------------------

fn wake_body(children: &[wonder_store::WakeChild]) -> String {
    let named = children
        .iter()
        .map(|c| format!("\"{}\" ({})", c.title, c.outcome))
        .collect::<Vec<_>>()
        .join(", ");
    let ids = children
        .iter()
        .map(|c| c.conversation_id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Automatic message from Cupola, not from the user. Delegated tasks finished: {named}. \
         Review each result with wonder_thread_read (thread ids: {ids}) and continue your work."
    )
}

/// Tells an idle parent, once, that delegated threads finished. Run from the
/// dispatch loop: it reads only the database, so it never interrupts a turn,
/// and a parent that is running or has queued messages is simply not picked.
pub(crate) async fn wake_parents(state: &AppState) {
    let cutoff = (Utc::now() - ABANDONED_AFTER).to_rfc3339_opts(SecondsFormat::Millis, true);
    let _ = state.store.remove_abandoned_delegations(&cutoff).await;
    let Ok(candidates) = state.store.wake_candidates().await else {
        return;
    };
    // A thread about to be told of its own finished tasks has more work coming;
    // its parent hears of it once that is done.
    let waking: HashSet<String> = candidates
        .iter()
        .map(|c| c.parent_conversation_id.clone())
        .collect();
    for mut candidate in candidates {
        candidate
            .children
            .retain(|child| !waking.contains(&child.conversation_id));
        // A parent the Mac is running keeps the wake-up queued like any message.
        if candidate.children.is_empty() {
            continue;
        }
        // Record each result before telling the parent, so it outlives a restart.
        for child in &candidate.children {
            task_result(state, &child.conversation_id).await;
        }
        let ids = candidate
            .children
            .iter()
            .map(|c| c.conversation_id.clone())
            .collect::<Vec<_>>();
        // The compact row in the timeline reads this title.
        let title = format!(
            "Agent tasks finished: {}",
            candidate
                .children
                .iter()
                .map(|c| format!("{} ({})", c.title, c.outcome))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let source = NewMessageSource {
            kind: "wake",
            conversation: None,
            title: &title,
            wake_children: &ids,
            wake_parent: Some(&candidate.parent_conversation_id),
        };
        let _ = state
            .store
            .insert_agent_message(
                &candidate.device_id,
                &uuid::Uuid::new_v4().to_string(),
                &wake_body(&candidate.children),
                &candidate.parent_conversation_id,
                &now_text(),
                &source,
            )
            .await;
    }
}

/// How the agent is told who wrote a message. The stored body stays exactly
/// what was sent, so the timeline can show its own "From" label instead.
pub(crate) fn provider_text(source: Option<&MessageSource>, body: &str) -> String {
    match source {
        Some(source) if source.kind == "thread" => format!(
            "[Message from the thread \"{}\"{}. Reply with wonder_thread_send if it needs an answer.]\n\n{body}",
            source.source_title,
            source
                .source_conversation_id
                .as_deref()
                .map(|id| format!(", id {id}"))
                .unwrap_or_default()
        ),
        _ => body.to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::projects::tests::handoff_fixture;
    use wonder_store::{ProjectConversationPatch, ProjectRootInput};

    struct Fx {
        dir: tempfile::TempDir,
        state: AppState,
        parent: Caller,
        _service: crate::ingestion::NotificationService,
    }

    async fn caller_of(state: &AppState, message: &str) -> Caller {
        let message = state.store.message_by_id(message).await.unwrap().unwrap();
        let conversation = state
            .store
            .project_conversation(&message.conversation_id)
            .await
            .unwrap()
            .unwrap();
        let project = state
            .store
            .project(&conversation.project_id)
            .await
            .unwrap()
            .unwrap();
        Caller {
            runtime: None,
            message,
            conversation,
            project,
        }
    }

    // The parent is a Workspace Codex thread with a running turn, in a Project
    // whose catalog offers one Codex model.
    async fn fx() -> Fx {
        let (dir, state, message) = handoff_fixture().await;
        state
            .runtime_catalog
            .write()
            .await
            .models
            .push(ModelOption {
                agent_family: AgentFamily::Codex,
                capabilities: ModelCapabilities::for_family(AgentFamily::Codex),
                id: "fake".into(),
                display_name: "Fake".into(),
                description: None,
                model_specialty: None,
                hidden: false,
                reasoning_efforts: vec![],
                default_reasoning_effort: None,
                service_tiers: vec![],
                default_service_tier: Some("default".into()),
                is_default: false,
                provider_default: None,
                native_ids: vec![],
            });
        state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    access_mode: Some("workspace"),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let service = crate::ingestion::spawn(state.clone()).await;
        for _ in 0..6000 {
            if state.ingestion.project_readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let parent = caller_of(&state, &message.id).await;
        Fx {
            dir,
            state,
            parent,
            _service: service,
        }
    }

    async fn add_thread(
        fx: &Fx,
        id: &str,
        title: &str,
        family: AgentFamily,
        access: &str,
        at: &str,
    ) -> StoredProjectConversation {
        let parent = &fx.parent;
        let created = fx
            .state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: id,
                project_id: &parent.project.id,
                family,
                provider_store: crate::providers::provider_store(&fx.state, family),
                native_session_id: None,
                cwd: &parent.conversation.cwd,
                roots_revision: parent.project.roots_revision,
                title,
                model: (family == AgentFamily::Codex).then_some("fake"),
                effort: None,
                service_tier: None,
                access_mode: access,
                claude_approval: if family == AgentFamily::Claude {
                    "ask"
                } else {
                    agent_defaults::claude_approval(family)
                },
                plan_mode: false,
                creation_request_id: None,
                now: at,
            })
            .await
            .unwrap();
        match created {
            ProjectConversationCreate::Created(thread)
            | ProjectConversationCreate::Existing(thread) => thread,
        }
    }

    // A thread in a second Project, which no tool of the first may reach.
    async fn add_foreign_thread(fx: &Fx) {
        let source = fx.dir.path().join("other-source");
        std::fs::create_dir(&source).unwrap();
        let path = crate::canonical_path::canonicalize(&source)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let root = ProjectRootInput {
            path: path.clone(),
            canonical_path: path.clone(),
        };
        fx.state
            .store
            .create_project("other", "r2", "h2", "Other", &[root], 0, "now")
            .await
            .unwrap();
        fx.state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "foreign",
                project_id: "other",
                family: AgentFamily::Codex,
                provider_store: &fx.state.projects.codex_store,
                native_session_id: None,
                cwd: &path,
                roots_revision: 1,
                title: "Foreign",
                model: Some("fake"),
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

    async fn queue(fx: &Fx, conversation: &str, text: &str) -> wonder_store::StoredMessage {
        let MessageInsert::Inserted(message) = fx
            .state
            .store
            .insert_dispatch_message(
                "owner",
                &uuid::Uuid::new_v4().to_string(),
                text,
                "hash",
                conversation,
                &[],
                "2026-01-01T00:00:00Z",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        message
    }

    async fn settle(fx: &Fx, message: &str, state: &str) {
        fx.state
            .store
            .update_message_delivery(message, state, None, None)
            .await
            .unwrap();
    }

    async fn running_peer(fx: &Fx, id: &str) -> Caller {
        add_thread(fx, id, id, AgentFamily::Codex, "workspace", "now").await;
        let message = queue(fx, id, "running fixture").await;
        fx.state
            .store
            .update_message_delivery(
                &message.id,
                "streaming",
                Some(id),
                Some(&format!("turn-{id}")),
            )
            .await
            .unwrap();
        caller_of(&fx.state, &message.id).await
    }

    fn wait_count(state: &AppState) -> usize {
        state.thread_waits.entries.lock().unwrap().len()
    }

    // Drive the actual wait until admission, without depending on a fixed
    // polling delay or on any state left by another test.
    async fn wait_registered<F: std::future::Future + Unpin>(
        state: &AppState,
        waiting: &mut F,
        count: usize,
    ) {
        let before: HashSet<_> = state
            .thread_waits
            .entries
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        tokio::time::timeout(Duration::from_secs(60), async {
            tokio::select! {
                _ = waiting => panic!("wait ended before registration"),
                _ = async {
                    loop {
                        let admitted = {
                            let entries = state.thread_waits.entries.lock().unwrap();
                            entries.len() == count && entries.keys().any(|id| !before.contains(id))
                        };
                        if admitted {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                } => {}
            }
        })
        .await
        .expect("wait must register promptly");
    }

    fn fresh() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn run(fx: &Fx, tool: &str, args: Value) -> Result<Value, String> {
        execute(&fx.state, &fx.parent, tool, args)
            .await
            .map_err(|e| e.code.to_owned())
    }

    async fn db(fx: &Fx) -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            fx.dir.path().join("state.db").display()
        ))
        .await
        .unwrap()
    }

    // Include the creation-request records and message provenance, so an
    // invalid call cannot leave behind an idempotency key or queued task.
    async fn delegation_rows(db: &sqlx::SqlitePool) -> (i64, i64, i64, i64, i64) {
        sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM project_conversations),
                    (SELECT COUNT(*) FROM thread_delegations),
                    (SELECT COUNT(*) FROM messages),
                    (SELECT COUNT(*) FROM project_conversations WHERE creation_request_id IS NOT NULL),
                    (SELECT COUNT(*) FROM message_sources)",
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[test]
    fn every_registered_tool_has_one_object_schema() {
        let specs = specs();
        let mut names: Vec<_> = specs.iter().map(|s| s["name"].as_str().unwrap()).collect();
        let mut shared: Vec<_> = NAMES.iter().map(String::as_str).collect();
        names.sort();
        shared.sort();
        // The bridges' allowlists are tested against the same file.
        assert_eq!(
            names, shared,
            "specs() must offer exactly the names in packages/protocol/thread-tools.json"
        );
        for spec in &specs {
            assert!(registered(spec["name"].as_str().unwrap()));
            assert_eq!(spec["inputSchema"]["type"], "object");
        }
        assert!(!registered("wonder_assign_project"));
        // Delegation offers exactly the providers whose threads get these
        // tools, OpenCode and Cursor included through their ACP MCP server.
        let delegate = specs.iter().find(|s| s["name"] == DELEGATE).unwrap();
        assert_eq!(
            delegate["inputSchema"]["properties"]["provider"]["enum"],
            json!(["codex", "claude", "opencode", "cursor"])
        );
    }

    // Every shared name reaches a handler, not the unknown-tool answer.
    #[tokio::test]
    async fn every_shared_tool_name_dispatches_to_a_handler() {
        let fx = fx().await;
        for name in NAMES.iter() {
            let answer = execute(&fx.state, &fx.parent, name, json!({"unknownField": true})).await;
            assert!(
                answer
                    .as_ref()
                    .err()
                    .is_none_or(|e| e.code != "unknown_tool"),
                "{name} has no handler"
            );
        }
    }

    fn model(id: &str, hidden: bool, efforts: &[&str], tiers: &[&str]) -> ModelOption {
        let choice = |id: &&str| crate::ChoiceOption {
            id: (*id).into(),
            label: (*id).into(),
            description: None,
        };
        ModelOption {
            agent_family: AgentFamily::Codex,
            capabilities: ModelCapabilities::for_family(AgentFamily::Codex),
            id: id.into(),
            display_name: format!("Model {id}"),
            description: None,
            model_specialty: None,
            hidden,
            reasoning_efforts: efforts.iter().map(choice).collect(),
            default_reasoning_effort: efforts.first().map(|e| (*e).into()),
            service_tiers: tiers.iter().map(choice).collect(),
            default_service_tier: Some("default".into()),
            is_default: false,
            provider_default: None,
            native_ids: vec![],
        }
    }

    async fn with_models(fx: &Fx) {
        let mut catalog = fx.state.runtime_catalog.write().await;
        catalog.models.push(model(
            "quick",
            false,
            &["low", "high"],
            &["default", "priority"],
        ));
        catalog.models.push(model("secret", true, &["low"], &[]));
    }

    // Contract (B-050): an agent can list what it may delegate to, built from
    // the same catalog filter as the outside listing, without hidden models.
    #[tokio::test]
    async fn models_lists_delegation_choices_and_what_a_child_inherits() {
        let fx = fx().await;
        with_models(&fx).await;
        let all = run(&fx, MODELS, json!({})).await.unwrap();
        let ids: Vec<_> = all["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["fake", "quick"]);
        let quick = &all["models"][1];
        assert_eq!(quick["provider"], "codex");
        assert_eq!(quick["efforts"], json!(["low", "high"]));
        assert_eq!(quick["serviceTiers"], json!(["default", "priority"]));
        assert_eq!(quick["defaultEffort"], "low");
        assert_eq!(quick["defaultServiceTier"], "default");
        let providers = all["providers"].as_array().unwrap();
        let codex = providers.iter().find(|p| p["id"] == "codex").unwrap();
        assert_eq!(
            (codex["available"].as_bool(), &codex["unavailableReason"]),
            (Some(true), &Value::Null)
        );
        let claude = providers.iter().find(|p| p["id"] == "claude").unwrap();
        assert_eq!(claude["available"], false);
        assert!(claude["unavailableReason"]
            .as_str()
            .unwrap()
            .contains("not installed"));
        assert_eq!(all["inherited"]["provider"], "codex");
        assert_eq!(all["inherited"]["accessMode"], "workspace");
        // No capability, plugin or tool inventories.
        assert!(!all.to_string().contains("tools"));
        let found = run(&fx, MODELS, json!({"search":"QUICK"})).await.unwrap();
        assert_eq!(found["models"].as_array().unwrap().len(), 1);
        let page = run(&fx, MODELS, json!({"provider":"codex","limit":1}))
            .await
            .unwrap();
        assert_eq!(
            (
                page["models"].as_array().unwrap().len(),
                page["hasMore"].as_bool()
            ),
            (1, Some(true))
        );
        assert_eq!(
            run(&fx, MODELS, json!({"limit":51})).await.unwrap_err(),
            "invalid"
        );
        assert_eq!(
            run(&fx, MODELS, json!({"provider":"gemini"}))
                .await
                .unwrap_err(),
            "invalid"
        );
        // The outside MCP listing shares the filter.
        let catalog = fx.state.runtime_catalog.read().await;
        let outside: Vec<_> = listed_models(&catalog, None)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(outside, ["fake", "quick"]);
    }

    // Contract (B-050): delegate honours a requested speed no faster than the
    // parent's, reports the settings it used, and names valid models when
    // the model is wrong; a retry creates no second child.
    #[tokio::test]
    async fn delegate_uses_the_requested_speed_within_the_parents_and_lists_valid_models() {
        let fx = fx().await;
        with_models(&fx).await;
        let faster = json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"quick","serviceTier":"priority"});
        assert_eq!(
            run(&fx, DELEGATE, faster.clone()).await.unwrap_err(),
            "permission_escalation"
        );
        let args = json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"quick","effort":"high","serviceTier":"default"});
        let made = run(&fx, DELEGATE, args.clone()).await.unwrap();
        assert_eq!(
            (&made["effort"], &made["serviceTier"], &made["workspace"]),
            (
                &json!("high"),
                &json!("default"),
                &json!(fx.parent.conversation.cwd)
            )
        );
        let again = run(&fx, DELEGATE, args).await.unwrap();
        assert_eq!(
            (&again["threadId"], again["created"].as_bool()),
            (&made["threadId"], Some(false))
        );
        // A parent on the faster speed may hand it on.
        let mut fast = caller_of(&fx.state, &fx.parent.message.id).await;
        fast.conversation.service_tier = Some("priority".into());
        let child = execute(&fx.state, &fast, DELEGATE, faster).await.unwrap();
        let stored = fx
            .state
            .store
            .project_conversation(child["threadId"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.service_tier.as_deref(), Some("priority"));
        let unknown = json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake","serviceTier":"turbo"});
        assert_eq!(
            execute(&fx.state, &fast, DELEGATE, unknown)
                .await
                .unwrap_err()
                .code,
            "unavailable_model"
        );
        assert_eq!(
            fx.state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            3
        );
        let wrong =
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"nope"});
        let error = execute(&fx.state, &fx.parent, DELEGATE, wrong)
            .await
            .unwrap_err();
        assert_eq!(error.code, "unavailable_model");
        assert!(error.message.contains("fake, quick"), "{}", error.message);
        assert!(!error.message.contains("secret"));
        // The owner prefers the faster speed: an omitted speed still stays
        // within this thread's.
        fx.state.runtime_catalog.write().await.preferences.insert(
            AgentFamily::Codex,
            wonder_store::AgentDefaultPreference {
                model: Some("quick".into()),
                effort: None,
                service_tier: Some("priority".into()),
            },
        );
        let omitted =
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"quick"});
        assert_eq!(
            execute(&fx.state, &fast, DELEGATE, omitted.clone())
                .await
                .unwrap()["serviceTier"],
            "priority"
        );
        let mut omitted = omitted;
        omitted["clientRequestId"] = json!(fresh());
        assert_eq!(
            run(&fx, DELEGATE, omitted).await.unwrap()["serviceTier"],
            "default"
        );
    }

    // Contract (B-207): requests that arrive together cannot pass the child
    // limit, and each child is recorded with its parent.
    #[tokio::test]
    async fn parallel_delegations_stay_within_the_child_limit() {
        let fx = fx().await;
        let calls = (0..MAX_CHILDREN + 4).map(|n| {
            let args = json!({"clientRequestId":fresh(),"prompt":format!("Task {n}"),"provider":"codex","model":"fake"});
            run(&fx, DELEGATE, args)
        });
        let results = futures_util::future::join_all(calls).await;
        let made = results.iter().filter(|r| r.is_ok()).count() as i64;
        let refused = results
            .iter()
            .filter(|r| r.as_ref().err().map(String::as_str) == Some("too_many_threads"))
            .count();
        assert_eq!((made, refused), (MAX_CHILDREN, 4), "{results:?}");
        let (_, links, ..) = delegation_rows(&db(&fx).await).await;
        let threads = fx
            .state
            .store
            .project_conversations("project")
            .await
            .unwrap()
            .len() as i64;
        assert_eq!((threads, links), (MAX_CHILDREN + 1, MAX_CHILDREN));
    }

    // A child a crash left without its task is removed once it is stale; a
    // child that has its task stays.
    #[tokio::test]
    async fn a_delegated_child_left_without_its_task_is_cleaned_up() {
        let fx = fx().await;
        for id in ["orphan", "fresh-orphan"] {
            add_thread(
                &fx,
                id,
                id,
                AgentFamily::Codex,
                "workspace",
                "2026-01-01T00:00:00Z",
            )
            .await;
        }
        let old = "2026-01-01T00:00:00.000Z";
        fx.state
            .store
            .record_thread_delegation("orphan", "project-chat", &fresh(), "owner", old)
            .await
            .unwrap();
        fx.state
            .store
            .record_thread_delegation(
                "fresh-orphan",
                "project-chat",
                &fresh(),
                "owner",
                &now_text(),
            )
            .await
            .unwrap();
        let kept = run(
            &fx,
            DELEGATE,
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake"}),
        )
        .await
        .unwrap();
        let kept = kept["threadId"].as_str().unwrap();
        sqlx::query("UPDATE thread_delegations SET created_at=? WHERE child_conversation_id=?")
            .bind(old)
            .bind(kept)
            .execute(&db(&fx).await)
            .await
            .unwrap();
        wake_parents(&fx.state).await;
        let store = &fx.state.store;
        assert!(store
            .project_conversation("orphan")
            .await
            .unwrap()
            .is_none());
        assert!(store.thread_delegation("orphan").await.unwrap().is_none());
        assert!(store
            .project_conversation("fresh-orphan")
            .await
            .unwrap()
            .is_some());
        assert!(store.project_conversation(kept).await.unwrap().is_some());
    }

    // Contract (B-166): a parent stops only threads it delegated, directly or
    // through its children, never itself; cancelled work reads as stopped.
    #[tokio::test]
    async fn a_parent_stops_only_its_delegated_threads() {
        let fx = fx().await;
        let made = run(
            &fx,
            DELEGATE,
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake"}),
        )
        .await
        .unwrap();
        let child = made["threadId"].as_str().unwrap().to_owned();
        let listed = run(&fx, LIST, json!({})).await.unwrap();
        let row = listed["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == child.as_str())
            .unwrap()
            .clone();
        assert_eq!(row["waitingOn"], "queue");
        let stop_args = |thread: &str, cancel: bool| json!({"threadId":thread,"cancelQueued":cancel,"clientRequestId":fresh()});
        assert_eq!(
            run(&fx, STOP, stop_args("project-chat", false))
                .await
                .unwrap_err(),
            "invalid"
        );
        add_thread(
            &fx,
            "peer",
            "Peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert_eq!(
            run(&fx, STOP, stop_args("peer", false)).await.unwrap_err(),
            "not_delegated"
        );
        assert_eq!(
            run(&fx, STOP, json!({"threadId":child})).await.unwrap_err(),
            "invalid"
        );
        // A child of the child counts as delegated by the parent.
        add_thread(
            &fx,
            "grandchild",
            "Grandchild",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        fx.state
            .store
            .record_thread_delegation("grandchild", &child, &fresh(), "owner", "now")
            .await
            .unwrap();
        let idle = run(&fx, STOP, stop_args("grandchild", false))
            .await
            .unwrap();
        assert_eq!(
            (&idle["stopped"], &idle["status"]),
            (&json!(false), &json!("idle"))
        );
        // The child cannot stop its parent.
        let mut as_child = caller_of(&fx.state, &fx.parent.message.id).await;
        as_child.conversation = fx
            .state
            .store
            .project_conversation(&child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            execute(&fx.state, &as_child, STOP, stop_args("project-chat", false))
                .await
                .unwrap_err()
                .code,
            "not_delegated"
        );
        let stopped = run(&fx, STOP, stop_args(&child, true)).await.unwrap();
        assert_eq!(stopped["cancelledQueuedMessages"], 1);
        assert!(fx
            .state
            .store
            .pending_queue(&child)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            task_result(&fx.state, &child).await.unwrap().outcome,
            "stopped"
        );
    }

    // Contract: retries of one stop that overlap, before any has recorded a
    // result, still stop the thread once; the others replay or wait.
    #[tokio::test]
    async fn overlapping_retries_of_one_stop_run_it_once() {
        let fx = fx().await;
        let made = run(
            &fx,
            DELEGATE,
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake"}),
        )
        .await
        .unwrap();
        let child = made["threadId"].as_str().unwrap().to_owned();
        for n in 0..2 {
            run(
                &fx,
                SEND,
                json!({"threadId":child,"message":format!("more {n}"),"clientRequestId":fresh()}),
            )
            .await
            .unwrap();
        }
        // The same request sent several times at once, before any finished.
        let args = json!({"threadId":child,"cancelQueued":true,"clientRequestId":fresh()});
        let results = futures_util::future::join_all(
            (0..4).map(|_| execute(&fx.state, &fx.parent, STOP, args.clone())),
        )
        .await
        .into_iter()
        .map(|r| r.map_err(|e| e.message))
        .collect::<Vec<_>>();
        // A second run would find nothing left and report 0 cancelled; a
        // replay repeats the first result exactly.
        let ran: Vec<&Value> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert!(!ran.is_empty(), "{results:?}");
        for result in &results {
            match result {
                Ok(value) => assert_eq!(value, ran[0], "one stop, replayed: {results:?}"),
                Err(message) => assert!(
                    message.contains("still in progress"),
                    "only a retry that overlaps the stop may fail: {results:?}"
                ),
            }
        }
        assert_eq!(ran[0]["cancelledQueuedMessages"], 3);
        assert!(fx
            .state
            .store
            .pending_queue(&child)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(run(&fx, STOP, args).await.unwrap(), *ran[0]);
    }

    // A crash can happen on either side of the interruption. A retry must
    // finish with an honest, durable answer and never stop newer work.
    #[tokio::test]
    async fn a_stop_abandoned_by_a_previous_process_does_not_run_again() {
        for (previous_owner, already_stopped) in [
            ("agent-stop", false),
            ("agent-stop", true),
            ("agent-stop:previous-process", false),
            ("agent-stop:previous-process", true),
        ] {
            let fx = fx().await;
            let made = run(
                &fx,
                DELEGATE,
                json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake"}),
            )
            .await
            .unwrap();
            let child = made["threadId"].as_str().unwrap();
            let request = fresh();
            let args = json!({"threadId":child,"cancelQueued":true,"clientRequestId":request});
            let key = hex::encode(Sha256::digest(
                json!(["stop", fx.parent.conversation.conversation_id, request]).to_string(),
            ));
            let hash = hex::encode(Sha256::digest(json!([child, true]).to_string()));
            assert!(fx
                .state
                .store
                .claim_tool_call(
                    &key,
                    &hash,
                    &fx.parent.message.id,
                    previous_owner,
                    STOP,
                    &now_text(),
                )
                .await
                .unwrap()
                .is_none());
            if already_stopped {
                stop_thread(&fx.state, child, true).await.unwrap();
            }
            run(
                &fx,
                SEND,
                json!({"threadId":child,"message":"new work","clientRequestId":fresh()}),
            )
            .await
            .unwrap();
            let before = fx.state.store.pending_queue(child).await.unwrap();
            let first = execute(&fx.state, &fx.parent, STOP, args.clone())
                .await
                .unwrap_err();
            assert!(first.message.contains("before saving"), "{first:?}");
            let saved = fx.state.store.tool_call(&key).await.unwrap().unwrap().1;
            assert!(
                saved.is_some(),
                "the abandoned claim must have a terminal answer"
            );
            let replay = execute(&fx.state, &fx.parent, STOP, args)
                .await
                .unwrap_err();
            assert_eq!(first.message, replay.message);
            let after = fx.state.store.pending_queue(child).await.unwrap();
            assert_eq!(
                before.iter().map(|m| &m.id).collect::<Vec<_>>(),
                after.iter().map(|m| &m.id).collect::<Vec<_>>()
            );
        }
    }

    // A stop waits for a task that is being started instead of reporting the
    // thread idle, and a retry of the same request replays its result rather
    // than stop the thread's next task.
    #[tokio::test]
    async fn a_stop_waits_for_a_starting_task_and_runs_once_per_request() {
        let fx = fx().await;
        let made = run(
            &fx,
            DELEGATE,
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake"}),
        )
        .await
        .unwrap();
        let (child, task) = (
            made["threadId"].as_str().unwrap().to_owned(),
            made["messageId"].as_str().unwrap().to_owned(),
        );
        let pool = db(&fx).await;
        let set_state = |state: &'static str| {
            sqlx::query("UPDATE messages SET state=? WHERE id=?")
                .bind(state)
                .bind(task.clone())
                .execute(&pool)
        };
        // The dispatch has claimed the task and holds it before its receipt.
        set_state("dispatching_to_codex").await.unwrap();
        let args = json!({"threadId":child,"cancelQueued":true,"clientRequestId":fresh()});
        let stopping = run(&fx, STOP, args.clone());
        tokio::pin!(stopping);
        // Once the stop has claimed its request it is waiting for the start;
        // finishing first would mean it reported the thread idle instead.
        let claimed = async {
            loop {
                let open: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM tool_calls WHERE tool_name=? AND response_json IS NULL",
                )
                .bind(STOP)
                .fetch_one(&pool)
                .await
                .unwrap();
                if open == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            done = &mut stopping => panic!("the stop must wait for the start to end: {done:?}"),
            waited = tokio::time::timeout(Duration::from_secs(10), claimed) => {
                waited.expect("the stop claims its request")
            }
        }
        // The start fails and puts the task back in the queue.
        set_state("accepted_by_wonder").await.unwrap();
        let stopped = stopping.await.unwrap();
        assert_eq!(stopped["cancelledQueuedMessages"], 1, "{stopped}");
        assert_eq!(
            task_result(&fx.state, &child).await.unwrap().outcome,
            "stopped"
        );
        // The parent hands the child a new task; the retried stop leaves it.
        let next = run(
            &fx,
            SEND,
            json!({"threadId":child,"message":"next","clientRequestId":fresh()}),
        )
        .await
        .unwrap();
        assert_eq!(run(&fx, STOP, args.clone()).await.unwrap(), stopped);
        assert_eq!(
            fx.state.store.pending_queue(&child).await.unwrap().len(),
            1,
            "{next}"
        );
        let mut other = args;
        other["cancelQueued"] = json!(false);
        assert_eq!(run(&fx, STOP, other).await.unwrap_err(), "request_reused");
    }

    #[tokio::test]
    async fn list_is_limited_to_the_callers_project_and_pages_without_repeats() {
        let fx = fx().await;
        for (n, title) in ["One", "Two", "Three", "Four"].into_iter().enumerate() {
            let at = format!("2026-01-01T00:00:0{n}Z");
            add_thread(
                &fx,
                &format!("t{n}"),
                title,
                AgentFamily::Codex,
                "workspace",
                &at,
            )
            .await;
        }
        add_foreign_thread(&fx).await;
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut args = json!({"limit": 2});
            if let Some(cursor) = &cursor {
                args["cursor"] = json!(cursor);
            }
            let page = run(&fx, LIST, args).await.unwrap();
            pages += 1;
            assert!(page["threads"].as_array().unwrap().len() <= 2);
            seen.extend(
                page["threads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["id"].as_str().unwrap().to_owned()),
            );
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(seen, ["project-chat", "t3", "t2", "t1", "t0"]);
        let first = run(&fx, LIST, json!({})).await.unwrap();
        assert_eq!(first["threads"][0]["isCurrent"], true);
        assert_eq!(first["threads"][0]["status"], "working");
        assert_eq!(first["threads"][1]["status"], "idle");
    }

    #[tokio::test]
    async fn no_tool_reaches_a_thread_of_another_project() {
        let fx = fx().await;
        add_foreign_thread(&fx).await;
        for (tool, args) in [
            (READ, json!({"threadId":"foreign"})),
            (
                SEND,
                json!({"threadId":"foreign","message":"hi","clientRequestId":fresh()}),
            ),
            (WAIT, json!({"threadId":"foreign","timeoutSeconds":1})),
            (READ, json!({"threadId":"missing"})),
        ] {
            assert_eq!(
                run(&fx, tool, args).await.unwrap_err(),
                "not_found",
                "{tool}"
            );
        }
        assert!(fx
            .state
            .store
            .open_message_ids("foreign")
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_page_of_history_resumes_after_the_last_position_and_hides_work_output() {
        let items: Vec<Value> = (0..7)
            .map(|n| match n % 3 {
                0 => json!({"type":"userMessage","content":[{"type":"text","text":format!("ask {n}")}],"clientId":format!("c{n}")}),
                1 => json!({"type":"reasoning","text":"private thoughts"}),
                _ => json!({"type":"agentMessage","text":format!("answer {n}")}),
            })
            .collect();
        let read = |after| {
            let mut page = Page::new(after, 2);
            for item in &items {
                if page.push(item) {
                    break;
                }
            }
            page
        };
        let first = read(0);
        assert_eq!(first.items.iter().map(|i| i.0).collect::<Vec<_>>(), [1, 2]);
        assert!(first.more);
        let second = read(2);
        assert_eq!(
            second.items.iter().map(|i| i.2.clone()).collect::<Vec<_>>(),
            ["ask 3", "answer 5"]
        );
        let last = read(4);
        assert_eq!(last.items.len(), 1);
        assert_eq!(last.items[0].3.as_deref(), Some("c6"));
        assert!(!last.more);
        let command = json!({"type":"commandExecution","command":"cargo test","status":"completed","exitCode":0,"aggregatedOutput":"SECRET OUTPUT"});
        let (role, line) = summarize(&command).unwrap();
        assert_eq!(
            (role, line.as_str()),
            ("tool", "Ran `cargo test` (completed, exit 0)")
        );
        assert!(summarize(&json!({"type":"reasoning","text":"x"})).is_none());
        for (server, tool, expected) in [
            ("roblox-studio", "run_code", "Roblox Studio: run code"),
            (
                "Claude",
                "mcp__Roblox_Studio__run_code",
                "Roblox Studio: run code",
            ),
            (
                "Claude",
                "mcp__claude_ai_Figma__get_design_context",
                "Figma: get design context",
            ),
            ("github", "getPullRequest", "GitHub: get pull request"),
            ("GitHub", "getPullRequest", "GitHub: get pull request"),
        ] {
            for kind in ["mcpToolCall", "dynamicToolCall"] {
                let item = json!({"type":kind,"server":server,"tool":tool,"arguments":{"code":"PRIVATE INPUT"}});
                assert_eq!(summarize(&item), Some(("tool", expected.into())));
            }
        }
    }

    #[tokio::test]
    async fn read_shows_queued_messages_and_who_sent_them() {
        let fx = fx().await;
        add_thread(
            &fx,
            "peer",
            "Peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let sent =
            json!({"threadId":"peer","message":"look at the tests","clientRequestId":fresh()});
        run(&fx, SEND, sent).await.unwrap();
        let read = run(&fx, READ, json!({"threadId":"peer"})).await.unwrap();
        assert_eq!(read["items"], json!([]));
        assert_eq!(read["queuedMessages"][0]["text"], "look at the tests");
        assert_eq!(read["queuedMessages"][0]["from"], "Recovery");
        assert_eq!(read["status"], "working");
    }

    #[tokio::test]
    async fn send_queues_with_its_source_and_a_retry_changes_nothing() {
        let fx = fx().await;
        add_thread(
            &fx,
            "peer",
            "Peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let request = fresh();
        let args = json!({"threadId":"peer","message":"please review","clientRequestId":request});
        let first = run(&fx, SEND, args.clone()).await.unwrap();
        assert_eq!(first["duplicate"], false);
        assert_eq!(first["mode"], "queue");
        let again = run(&fx, SEND, args).await.unwrap();
        assert_eq!(again["messageId"], first["messageId"]);
        assert_eq!(again["duplicate"], true);
        let queued = fx.state.store.pending_queue("peer").await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].body, "please review");
        let id = first["messageId"].as_str().unwrap();
        let source = fx.state.store.message_source(id).await.unwrap().unwrap();
        assert_eq!(
            (source.kind.as_str(), source.source_title.as_str()),
            ("thread", "Recovery")
        );
        assert_eq!(
            source.source_conversation_id.as_deref(),
            Some("project-chat")
        );
        // The agent reading it is told who wrote it; the stored text is untouched.
        assert!(provider_text(Some(&source), "please review").contains("\"Recovery\""));
        assert_eq!(provider_text(None, "plain"), "plain");
        // The same request id with another message is refused, not queued.
        let reused =
            json!({"threadId":"peer","message":"something else","clientRequestId":request});
        assert_eq!(run(&fx, SEND, reused).await.unwrap_err(), "request_reused");
        assert_eq!(fx.state.store.pending_queue("peer").await.unwrap().len(), 1);
    }

    // Owner (Oct 12): a send returned "accepted_by_wonder", then the Mac
    // refused it and nobody was told. The tool reports the real outcome.
    #[tokio::test]
    async fn send_reports_a_dispatch_the_mac_refused_with_its_reason() {
        let fx = fx().await;
        add_thread(
            &fx,
            "peer",
            "Peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let reason = "Claude is still running background tasks in this conversation.";
        let store = fx.state.store.clone();
        let refuse = tokio::spawn(async move {
            // Stands in for the dispatcher: it picks the message up and fails it.
            loop {
                if let Some(queued) = store.pending_queue("peer").await.unwrap().first() {
                    store
                        .fail_message_delivery(&queued.id, "safe_to_retry", reason)
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let sent = run(
            &fx,
            SEND,
            json!({"threadId":"peer","message":"please review","clientRequestId":fresh()}),
        )
        .await
        .unwrap();
        refuse.await.unwrap();
        assert_eq!(sent["outcome"], "failed", "{sent}");
        assert_eq!(sent["deliveryState"], "safe_to_retry");
        assert_eq!(sent["error"], reason);
        // Waiting on that message reports the same outcome and reason.
        let waited = run(
            &fx,
            WAIT,
            json!({"threadId":"peer","messageId":sent["messageId"],"timeoutSeconds":1}),
        )
        .await
        .unwrap();
        assert_eq!(waited["messageState"], "safe_to_retry", "{waited}");
        assert_eq!(waited["error"], reason);
    }

    #[tokio::test]
    async fn send_that_has_not_started_says_how_to_learn_the_outcome() {
        let fx = fx().await;
        add_thread(
            &fx,
            "peer",
            "Peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let sent = run(
            &fx,
            SEND,
            json!({"threadId":"peer","message":"later","clientRequestId":fresh()}),
        )
        .await
        .unwrap();
        assert_eq!(sent["outcome"], "pending", "{sent}");
        assert!(sent["check"].as_str().unwrap().contains(WAIT));
        assert!(sent.get("error").is_none());
    }

    #[tokio::test]
    async fn send_cannot_direct_a_thread_with_wider_access_or_itself() {
        let fx = fx().await;
        add_thread(
            &fx,
            "full",
            "Full",
            AgentFamily::Codex,
            "full_access",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let to_full = json!({"threadId":"full","message":"rm it","clientRequestId":fresh()});
        assert_eq!(
            run(&fx, SEND, to_full).await.unwrap_err(),
            "permission_escalation"
        );
        add_thread(
            &fx,
            "reader",
            "Reader",
            AgentFamily::Codex,
            "read_only",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let down = json!({"threadId":"reader","message":"summarize","clientRequestId":fresh()});
        assert!(run(&fx, SEND, down).await.is_ok());
        let itself = json!({"threadId":"project-chat","message":"x","clientRequestId":fresh()});
        assert_eq!(run(&fx, SEND, itself).await.unwrap_err(), "invalid");
        assert!(fx
            .state
            .store
            .open_message_ids("full")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn steer_is_refused_where_unsupported_and_needs_a_running_response() {
        let fx = fx().await;
        let claude = add_thread(
            &fx,
            "claude",
            "Claude peer",
            AgentFamily::Claude,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let steer = |thread: &str| json!({"threadId":thread,"message":"change course","mode":"steer","clientRequestId":fresh()});
        assert_eq!(
            run(&fx, SEND, steer(&claude.conversation_id))
                .await
                .unwrap_err(),
            "steer_unsupported"
        );
        add_thread(
            &fx,
            "codex",
            "Codex peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        assert_eq!(
            run(&fx, SEND, steer("codex")).await.unwrap_err(),
            "not_running"
        );
        let running = queue(&fx, "codex", "work").await;
        fx.state
            .store
            .update_message_delivery(
                &running.id,
                "streaming",
                Some("peer-thread"),
                Some("peer-turn"),
            )
            .await
            .unwrap();
        let sent = run(&fx, SEND, steer("codex")).await.unwrap();
        assert_eq!(sent["mode"], "steer");
        let guides = fx.state.store.pending_guides().await.unwrap();
        assert_eq!(guides.len(), 1);
        assert_eq!(
            (guides[0].0.id.as_str(), guides[0].1.as_str()),
            (sent["messageId"].as_str().unwrap(), "peer-turn")
        );
        assert!(fx
            .state
            .store
            .message_source(&guides[0].0.id)
            .await
            .unwrap()
            .is_some());
    }

    // The phone's Guide button posts to the steer route; a Project thread's
    // running turn takes it, Codex or Claude (whose bridge keeps the process).
    #[tokio::test]
    async fn phone_guide_steers_a_running_project_turn_codex_or_claude() {
        let fx = fx().await;
        for (id, family) in [
            ("codex", AgentFamily::Codex),
            ("claude", AgentFamily::Claude),
        ] {
            add_thread(&fx, id, id, family, "workspace", "2026-01-01T00:00:00Z").await;
            let running = queue(&fx, id, "busy").await;
            let (thread, turn) = (format!("{id}-thread"), format!("{id}-turn"));
            fx.state
                .store
                .update_message_delivery(&running.id, "streaming", Some(&thread), Some(&turn))
                .await
                .unwrap();
            let (status, body) = crate::test_support::call(
                &fx.state,
                "POST",
                &format!("/api/v1/conversations/{id}/turns/{turn}/steer"),
                json!({"deviceId":"owner","clientMessageId":fresh(),"body":"use the new API","expectedTurnId":turn}),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{family:?}: {body}");
            let guides = fx.state.store.pending_guides().await.unwrap();
            assert!(guides
                .iter()
                .any(|(m, t)| m.conversation_id == id && t == &turn));
        }
    }

    // A Project thread's running turn takes the steer through its own provider runtime.
    #[tokio::test]
    async fn a_steer_reaches_the_running_turn_of_a_project_thread() {
        let fx = fx().await;
        add_thread(
            &fx,
            "sender",
            "Sender",
            AgentFamily::Codex,
            "full_access",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let working = queue(&fx, "sender", "coordinate").await;
        fx.state
            .store
            .update_message_delivery(
                &working.id,
                "streaming",
                Some("sender-thread"),
                Some("sender-turn"),
            )
            .await
            .unwrap();
        let sender = caller_of(&fx.state, &working.id).await;
        // The target is mid-turn on a runtime thread of its own.
        let target = fresh();
        add_thread(
            &fx,
            &target,
            "Target",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        fx.state
            .store
            .bind_project_runtime(
                &target,
                AgentFamily::Codex,
                &fx.state.projects.codex_store,
                "target-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        let running = queue(&fx, &target, "busy").await;
        fx.state
            .store
            .update_message_delivery(
                &running.id,
                "streaming",
                Some("target-thread"),
                Some("target-turn"),
            )
            .await
            .unwrap();
        let steer = json!({"threadId":target,"message":"use the new API","mode":"steer","clientRequestId":fresh()});
        let sent = execute(&fx.state, &sender, SEND, steer).await.unwrap();
        let guide = fx.state.store.pending_guides().await.unwrap().remove(0);
        assert_eq!(guide.0.id, sent["messageId"].as_str().unwrap());
        let response = crate::dispatch_guide(fx.state.clone(), guide.0.clone(), guide.1).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let stored = fx
            .state
            .store
            .message_by_id(&guide.0.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, "accepted_by_codex");
        // Recovery keeps a Guide its live runtime accepted, as it keeps the
        // reply it steers; it used to mark the Guide uncertain.
        crate::projects::recover(&fx.state).await;
        let recovered = fx
            .state
            .store
            .message_by_id(&guide.0.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, "accepted_by_codex");
        let requests = std::fs::read_to_string(fx.dir.path().join("requests-jsonl")).unwrap();
        let steer: Value = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|request| request["method"] == "turn/steer")
            .unwrap();
        assert_eq!(steer["params"]["threadId"], "target-thread");
        let text = steer["params"]["input"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("\"Sender\"") && text.ends_with("use the new API"),
            "{text}"
        );
    }

    // Observable contract: a live dependency cycle is rejected rather than
    // leaving all its turns blocked until timeout. The real-store wait fixture
    // owns this regression; no provider turn is started.
    async fn assert_wait_cycle_rejected(size: usize) {
        let fx = fx().await;
        let mut callers = vec![caller_of(&fx.state, &fx.parent.message.id).await];
        for index in 1..size {
            let id = format!("peer-{index}");
            add_thread(&fx, &id, &id, AgentFamily::Codex, "workspace", "now").await;
            let message = queue(&fx, &id, "running fixture").await;
            fx.state
                .store
                .update_message_delivery(
                    &message.id,
                    "streaming",
                    Some(&id),
                    Some(&format!("turn-{index}")),
                )
                .await
                .unwrap();
            callers.push(caller_of(&fx.state, &message.id).await);
        }
        let mut pending = Vec::new();
        for index in 0..size - 1 {
            let mut waiting = Box::pin(execute(
                &fx.state,
                &callers[index],
                WAIT,
                json!({"threadId":callers[index + 1].conversation.conversation_id,"timeoutSeconds":20}),
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                    .await
                    .is_err()
            );
            pending.push(waiting);
        }
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            execute(
                &fx.state,
                &callers[size - 1],
                WAIT,
                json!({"threadId":callers[0].conversation.conversation_id,"timeoutSeconds":20}),
            ),
        )
        .await
        .expect("cycle-closing wait must fail promptly, not block")
        .unwrap_err();
        assert_eq!(error.code, "wait_cycle");
        assert!(
            error.message.contains("read") && error.message.contains("wait"),
            "{error:?}"
        );
        println!(
            "{size}-thread cycle rejected in {:?}: {error:?}",
            started.elapsed()
        );
        drop(pending);
    }

    #[tokio::test]
    async fn wait_rejects_two_thread_cycle_promptly() {
        assert_wait_cycle_rejected(2).await;
    }

    #[tokio::test]
    async fn wait_rejects_three_thread_cycle_promptly() {
        assert_wait_cycle_rejected(3).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn simultaneous_waits_admit_only_one_direction_and_abort_cleans_up() {
        let fx = fx().await;
        let peer = running_peer(&fx, "peer").await;
        for round in 0..16 {
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let spawn = |from: Caller, to: String| {
                let state = fx.state.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    execute(
                        &state,
                        &from,
                        WAIT,
                        json!({"threadId":to,"timeoutSeconds":20}),
                    )
                    .await
                })
            };
            let mut first = spawn(
                caller_of(&fx.state, &fx.parent.message.id).await,
                "peer".into(),
            );
            let mut second = spawn(
                caller_of(&fx.state, &peer.message.id).await,
                "project-chat".into(),
            );
            barrier.wait().await;
            let winner = tokio::time::timeout(Duration::from_secs(60), async {
                tokio::select! {
                    result = &mut first => (0, result),
                    result = &mut second => (1, result),
                }
            })
            .await;
            let admitted = wait_count(&fx.state);
            first.abort();
            second.abort();
            let result = match winner {
                Ok((0, result)) => {
                    let _ = second.await;
                    result
                }
                Ok((_, result)) => {
                    let _ = first.await;
                    result
                }
                Err(error) => {
                    let _ = tokio::join!(first, second);
                    panic!("concurrent waits blocked each other: {error}");
                }
            };
            assert_eq!(result.unwrap().unwrap_err().code, "wait_cycle");
            assert_eq!(admitted, 1);
            assert_eq!(wait_count(&fx.state), 0);
            println!("concurrent round {round}: one admitted, one wait_cycle; abort left 0 registrations");
        }
    }

    #[tokio::test]
    async fn parallel_waits_release_only_their_own_dependencies() {
        let fx = fx().await;
        let peer = running_peer(&fx, "peer").await;
        let other = running_peer(&fx, "other").await;
        let mut first = Box::pin(run(&fx, WAIT, json!({"threadId":"peer"})));
        wait_registered(&fx.state, &mut first, 1).await;
        let mut same = Box::pin(run(&fx, WAIT, json!({"threadId":"peer"})));
        wait_registered(&fx.state, &mut same, 2).await;
        let mut different = Box::pin(run(&fx, WAIT, json!({"threadId":"other"})));
        wait_registered(&fx.state, &mut different, 3).await;
        drop(first);
        assert_eq!(wait_count(&fx.state), 2);
        let args = json!({"threadId":"project-chat"});
        assert_eq!(
            execute(&fx.state, &peer, WAIT, args.clone())
                .await
                .unwrap_err()
                .code,
            "wait_cycle"
        );
        assert_eq!(wait_count(&fx.state), 2);
        drop(same);
        let mut reverse = Box::pin(execute(&fx.state, &peer, WAIT, args.clone()));
        wait_registered(&fx.state, &mut reverse, 2).await;
        drop(reverse);
        assert_eq!(
            execute(&fx.state, &other, WAIT, args)
                .await
                .unwrap_err()
                .code,
            "wait_cycle"
        );
        drop(different);
        assert_eq!(wait_count(&fx.state), 0);
        println!("parallel duplicate and distinct waits retained their other edges; dropped futures left 0 registrations");
    }

    #[tokio::test]
    async fn ended_or_replaced_callers_do_not_lend_dependencies_to_fresh_turns() {
        for replacement in ["ended", "rebound", "replaced"] {
            let fx = fx().await;
            let peer = running_peer(&fx, "peer").await;
            let mut old = Box::pin(run(&fx, WAIT, json!({"threadId":"peer"})));
            wait_registered(&fx.state, &mut old, 1).await;
            if replacement == "ended" {
                settle(&fx, &fx.parent.message.id, "completed").await;
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(60), old)
                        .await
                        .unwrap()
                        .unwrap_err(),
                    "cancelled"
                );
                assert_eq!(wait_count(&fx.state), 0);
                println!("ended caller returned cancelled and released its registration");
                continue;
            }
            let message = if replacement == "replaced" {
                settle(&fx, &fx.parent.message.id, "completed").await;
                queue(&fx, "project-chat", "fresh turn").await
            } else {
                fx.parent.message.clone()
            };
            fx.state
                .store
                .update_message_delivery(
                    &message.id,
                    "streaming",
                    Some("thread"),
                    Some("fresh-turn"),
                )
                .await
                .unwrap();
            let fresh = caller_of(&fx.state, &message.id).await;
            // The older future has not been polled since its owner ended.
            let mut reverse = Box::pin(execute(
                &fx.state,
                &peer,
                WAIT,
                json!({"threadId":"project-chat"}),
            ));
            wait_registered(&fx.state, &mut reverse, 1).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(60), old)
                    .await
                    .unwrap()
                    .unwrap_err(),
                "cancelled"
            );
            assert_eq!(wait_count(&fx.state), 1);
            assert_eq!(
                execute(&fx.state, &fresh, WAIT, json!({"threadId":"peer"}))
                    .await
                    .unwrap_err()
                    .code,
                "wait_cycle"
            );
            drop(reverse);
            assert_eq!(wait_count(&fx.state), 0);
            println!("fresh turn {replacement}: stale edge pruned; cancelled old owner did not erase new wait");
        }
    }

    #[tokio::test]
    async fn completed_watched_work_is_pruned_and_never_registers_a_new_wait() {
        let fx = fx().await;
        let peer = running_peer(&fx, "peer").await;
        let mut old = Box::pin(run(&fx, WAIT, json!({"threadId":"peer"})));
        wait_registered(&fx.state, &mut old, 1).await;
        settle(&fx, &peer.message.id, "completed").await;
        let message = queue(&fx, "peer", "next work").await;
        fx.state
            .store
            .update_message_delivery(&message.id, "streaming", Some("peer"), Some("next-turn"))
            .await
            .unwrap();
        let next = caller_of(&fx.state, &message.id).await;
        let mut reverse = Box::pin(execute(
            &fx.state,
            &next,
            WAIT,
            json!({"threadId":"project-chat"}),
        ));
        wait_registered(&fx.state, &mut reverse, 1).await;
        assert_eq!(old.await.unwrap()["status"], "finished");
        let completed = run(
            &fx,
            WAIT,
            json!({"threadId":"peer","messageId":peer.message.id}),
        )
        .await
        .unwrap();
        assert_eq!(completed["status"], "finished");
        assert_eq!(wait_count(&fx.state), 1);
        drop(reverse);
        assert_eq!(wait_count(&fx.state), 0);
    }

    #[tokio::test]
    async fn wait_graph_is_shared_by_state_clones_and_isolated_between_hosts() {
        let one = fx().await;
        let two = fx().await;
        let peer_one = running_peer(&one, "peer").await;
        let peer_two = running_peer(&two, "peer").await;
        let mut forward = Box::pin(run(&one, WAIT, json!({"threadId":"peer"})));
        wait_registered(&one.state, &mut forward, 1).await;
        let cloned = one.state.clone();
        assert_eq!(
            execute(&cloned, &peer_one, WAIT, json!({"threadId":"project-chat"}))
                .await
                .unwrap_err()
                .code,
            "wait_cycle"
        );
        let mut independent = Box::pin(execute(
            &two.state,
            &peer_two,
            WAIT,
            json!({"threadId":"project-chat"}),
        ));
        wait_registered(&two.state, &mut independent, 1).await;
        drop(forward);
        assert_eq!(wait_count(&one.state), 0);
        assert_eq!(wait_count(&two.state), 1);
        drop(independent);
        assert_eq!(wait_count(&two.state), 0);
    }

    #[tokio::test]
    async fn delegate_wait_dependency_rejects_a_child_waiting_on_its_parent() {
        let fx = fx().await;
        let args = json!({"clientRequestId":fresh(),"prompt":"Fixture task","provider":"codex","model":"fake","mode":"wait"});
        let mut delegated = Box::pin(run(&fx, DELEGATE, args));
        wait_registered(&fx.state, &mut delegated, 1).await;
        let child = fx
            .state
            .store
            .project_conversations("project")
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.conversation_id != "project-chat")
            .unwrap();
        let message = fx
            .state
            .store
            .open_message_ids(&child.conversation_id)
            .await
            .unwrap()
            .remove(0);
        fx.state
            .store
            .update_message_delivery(&message, "streaming", Some("child"), Some("child-turn"))
            .await
            .unwrap();
        let caller = caller_of(&fx.state, &message).await;
        let error = execute(&fx.state, &caller, WAIT, json!({"threadId":"project-chat"}))
            .await
            .unwrap_err();
        let wire = response(Err(error));
        assert_eq!(wire["success"], false);
        let body: Value =
            serde_json::from_str(wire["contentItems"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "wait_cycle");
        assert_eq!(wait_count(&fx.state), 1);
        settle(&fx, &message, "completed").await;
        assert_eq!(delegated.await.unwrap()["status"], "finished");
        assert_eq!(wait_count(&fx.state), 0);
        // A retry of an async delegation can itself close the cycle. It must
        // use the same admission path as the explicit wait tool.
        let args = json!({"clientRequestId":fresh(),"prompt":"Second fixture task","provider":"codex","model":"fake","mode":"async"});
        let child = run(&fx, DELEGATE, args.clone()).await.unwrap();
        let message = child["messageId"].as_str().unwrap();
        fx.state
            .store
            .update_message_delivery(
                message,
                "streaming",
                Some("second-child"),
                Some("second-turn"),
            )
            .await
            .unwrap();
        let caller = caller_of(&fx.state, message).await;
        let mut reverse = Box::pin(execute(
            &fx.state,
            &caller,
            WAIT,
            json!({"threadId":"project-chat"}),
        ));
        wait_registered(&fx.state, &mut reverse, 1).await;
        let mut retry = args;
        retry["mode"] = json!("wait");
        let error = tokio::time::timeout(Duration::from_secs(10), run(&fx, DELEGATE, retry))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error, "wait_cycle");
        assert_eq!(wait_count(&fx.state), 1);
        drop(reverse);
        assert_eq!(wait_count(&fx.state), 0);
        let requests = std::fs::read_to_string(fx.dir.path().join("requests-jsonl")).unwrap();
        assert!(!requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|r| matches!(r["method"].as_str(), Some("turn/start" | "turn/steer"))));
        println!("delegate wait rejected child's reverse dependency; completion left 0 registrations; 0 provider turns started");
    }

    #[tokio::test]
    async fn wait_times_out_as_still_running_without_stopping_the_thread() {
        let fx = fx().await;
        add_thread(
            &fx,
            "peer",
            "Peer",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let running = queue(&fx, "peer", "long work").await;
        fx.state
            .store
            .update_message_delivery(
                &running.id,
                "streaming",
                Some("peer-thread"),
                Some("peer-turn"),
            )
            .await
            .unwrap();
        let timed_out = run(&fx, WAIT, json!({"threadId":"peer","timeoutSeconds":1}))
            .await
            .unwrap();
        assert_eq!(timed_out["status"], "still_running");
        assert_eq!(timed_out["threadStatus"], "working");
        assert_eq!(wait_count(&fx.state), 0);
        let after = fx
            .state
            .store
            .message_by_id(&running.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, "streaming");
        // The same wait answers as soon as the work ends, and idle threads do not wait.
        let store = fx.state.store.clone();
        let finishing = running.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            store
                .update_message_delivery(&finishing, "completed", None, None)
                .await
                .unwrap();
        });
        let done = run(&fx, WAIT, json!({"threadId":"peer","timeoutSeconds":20}))
            .await
            .unwrap();
        assert_eq!(
            (done["status"].as_str(), done["outcome"].as_str()),
            (Some("finished"), Some("completed"))
        );
        let idle = run(&fx, WAIT, json!({"threadId":"peer"})).await.unwrap();
        assert_eq!(idle["status"], "idle");
        assert_eq!(wait_count(&fx.state), 0);
    }

    #[tokio::test]
    async fn delegate_creates_a_narrowed_child_with_lineage_and_replays_once() {
        let fx = fx().await;
        let request = fresh();
        let args = json!({"clientRequestId":request,"prompt":"Fix the flaky test","provider":"codex","model":"fake","accessMode":"read_only","title":"Flaky test"});
        let first = run(&fx, DELEGATE, args.clone()).await.unwrap();
        let child_id = first["threadId"].as_str().unwrap();
        assert_eq!(
            (first["created"].as_bool(), first["accessMode"].as_str()),
            (Some(true), Some("read_only"))
        );
        let child = fx
            .state
            .store
            .project_conversation(child_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.project_id, "project");
        assert_eq!(child.cwd, fx.parent.conversation.cwd);
        assert_eq!(
            (child.title.as_str(), child.model.as_deref()),
            ("Flaky test", Some("fake"))
        );
        let link = fx
            .state
            .store
            .thread_delegation(child_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(link.parent_conversation_id, "project-chat");
        // The child receives only the prompt, labelled as delegated by the parent.
        let queued = fx.state.store.pending_queue(child_id).await.unwrap();
        assert_eq!(
            queued.iter().map(|q| q.body.as_str()).collect::<Vec<_>>(),
            ["Fix the flaky test"]
        );
        let source = fx
            .state
            .store
            .message_source(first["messageId"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.kind, "delegation");
        // A retry finds the same thread and message.
        let again = run(&fx, DELEGATE, args).await.unwrap();
        assert_eq!(
            (again["threadId"].as_str(), again["created"].as_bool()),
            (Some(child_id), Some(false))
        );
        assert_eq!(again["messageId"], first["messageId"]);
        assert_eq!(
            fx.state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            fx.state.store.pending_queue(child_id).await.unwrap().len(),
            1
        );
        // Reusing the request for another task is refused.
        let other = json!({"clientRequestId":request,"prompt":"Something else","provider":"codex","model":"fake"});
        assert_eq!(
            run(&fx, DELEGATE, other).await.unwrap_err(),
            "request_reused"
        );
    }

    // Contract: schema-invalid timeout values never create a child or queue
    // work, even on a retry or when the caller does not wait for the task.
    #[tokio::test]
    async fn delegate_rejects_invalid_timeouts_without_persisting_or_poisoning_requests() {
        let mut failures = Vec::new();
        for mode in [None, Some("async"), Some("wait")] {
            for timeout in [
                json!(0),
                json!(1801),
                json!(-1),
                json!("1"),
                json!(1.5),
                json!(true),
                Value::Null,
                json!([]),
                json!({}),
            ] {
                let fx = fx().await;
                let db = sqlx::SqlitePool::connect(&format!(
                    "sqlite://{}",
                    fx.dir.path().join("state.db").display()
                ))
                .await
                .unwrap();
                let before = delegation_rows(&db).await;
                let mut args = json!({"clientRequestId":fresh(),"prompt":"Bad timeout","provider":"codex","model":"fake","timeoutSeconds":timeout});
                if let Some(mode) = mode {
                    args["mode"] = json!(mode);
                }
                let mut rejected = true;
                for attempt in 1..=2 {
                    let result = tokio::time::timeout(
                        Duration::from_secs(2),
                        run(&fx, DELEGATE, args.clone()),
                    )
                    .await;
                    let after = delegation_rows(&db).await;
                    println!(
                        "delegate timeout mode={mode:?} timeout={timeout} attempt={attempt}: result={result:?}; rows (threads, delegations, messages, creation requests, sources) {before:?} -> {after:?}"
                    );
                    if !matches!(&result, Ok(Err(code)) if code == "invalid") || after != before {
                        rejected = false;
                        failures.push(format!(
                            "mode={mode:?} timeout={timeout} attempt={attempt}: {result:?}; {before:?} -> {after:?}"
                        ));
                    }
                }
                if !rejected {
                    continue;
                }
                // Correcting the rejected request, including its task, must
                // create a fresh child rather than replaying a poisoned one.
                args.as_object_mut().unwrap().remove("timeoutSeconds");
                args["mode"] = json!("async");
                args["prompt"] = json!("Corrected job");
                let first = run(&fx, DELEGATE, args.clone()).await.unwrap();
                assert_eq!(first["created"], true);
                let created = (
                    before.0 + 1,
                    before.1 + 1,
                    before.2 + 1,
                    before.3 + 1,
                    before.4 + 1,
                );
                assert_eq!(delegation_rows(&db).await, created);
                // Timeout is a wait option, not part of the task digest.
                args["timeoutSeconds"] = json!(1800);
                let replay = run(&fx, DELEGATE, args.clone()).await.unwrap();
                assert_eq!(replay["created"], false);
                assert_eq!(replay["threadId"], first["threadId"]);
                assert_eq!(replay["messageId"], first["messageId"]);
                assert_eq!(delegation_rows(&db).await, created);
                args["prompt"] = json!("Different job");
                assert_eq!(
                    run(&fx, DELEGATE, args).await.unwrap_err(),
                    "request_reused"
                );
                assert_eq!(delegation_rows(&db).await, created);
                println!("corrected timeout request: {before:?} -> {created:?}; replay and request_reused unchanged");
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[tokio::test]
    async fn delegate_defaults_to_the_parents_access_and_never_exceeds_it() {
        let fx = fx().await;
        let inherited =
            json!({"clientRequestId":fresh(),"prompt":"Do it","provider":"codex","model":"fake"});
        assert_eq!(
            run(&fx, DELEGATE, inherited).await.unwrap()["accessMode"],
            "workspace"
        );
        let wider = json!({"clientRequestId":fresh(),"prompt":"Do it all","provider":"codex","model":"fake","accessMode":"full_access"});
        assert_eq!(
            run(&fx, DELEGATE, wider).await.unwrap_err(),
            "permission_escalation"
        );
        assert_eq!(
            fx.state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            2,
            "the refused delegation created nothing"
        );
        // Approval: a Codex parent always asks, so no child may ask less.
        let parent = &fx.parent.conversation;
        assert_eq!(default_approval(AgentFamily::Claude, parent), "ask");
        assert_eq!(default_approval(AgentFamily::Codex, parent), "ask");
        let mut claude_parent = parent.clone();
        claude_parent.family = AgentFamily::Claude;
        claude_parent.claude_approval = "accept_edits".into();
        assert_eq!(
            default_approval(AgentFamily::Claude, &claude_parent),
            "accept_edits"
        );
        claude_parent.claude_approval = "auto".into();
        assert_eq!(
            default_approval(AgentFamily::Claude, &claude_parent),
            "auto"
        );
        let mut asking = claude_parent.clone();
        asking.claude_approval = "ask".into();
        assert!(within_authority(&asking, &claude_parent).is_err());
        assert!(within_authority(&claude_parent, &asking).is_ok());
    }

    async fn full_access_parent(fx: &Fx) -> Caller {
        fx.state
            .store
            .update_project_conversation(
                "project-chat",
                ProjectConversationPatch {
                    access_mode: Some("full_access"),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        caller_of(&fx.state, &fx.parent.message.id).await
    }

    // Contract: OpenCode and Cursor run under the owner's own settings, which
    // Cupola cannot lower, so only a thread at Full access (or one that is
    // itself under such settings) may start or direct them.
    #[test]
    fn unenforced_providers_need_full_access_or_an_equal_parent() {
        let thread =
            |family: AgentFamily, access: &str, approval: &str| StoredProjectConversation {
                conversation_id: "t".into(),
                project_id: "project".into(),
                family,
                provider_store: String::new(),
                native_session_id: None,
                cwd: String::new(),
                roots_revision: 0,
                title: String::new(),
                title_chosen: true,
                model: None,
                effort: None,
                service_tier: None,
                access_mode: access.into(),
                claude_approval: approval.into(),
                plan_mode: false,
                native_settings_turn: None,
                is_pinned: false,
                has_unread: false,
                wonder_archived: false,
                creation_request_id: None,
                created_at: String::new(),
                updated_at: String::new(),
                last_activity_at: String::new(),
            };
        let asking = thread(AgentFamily::Claude, "workspace", "ask");
        for native in [AgentFamily::OpenCode, AgentFamily::Cursor] {
            let child = thread(native, "workspace", "ask");
            assert_eq!(
                within_authority(&asking, &child).unwrap_err().code,
                "permission_escalation",
                "{native:?} from an Ask thread"
            );
            for narrow in ["read_only", "workspace"] {
                let parent = thread(AgentFamily::Claude, narrow, "accept_edits");
                assert!(within_authority(&parent, &child).is_err(), "{narrow}");
            }
            let full = thread(AgentFamily::Claude, "full_access", "ask");
            assert!(within_authority(&full, &child).is_ok());
            let codex_full = thread(AgentFamily::Codex, "full_access", "ask");
            assert!(within_authority(&codex_full, &child).is_ok());
            // Equal trust: the parent is itself under the owner's settings.
            let peer = thread(AgentFamily::OpenCode, "workspace", "ask");
            assert!(within_authority(&peer, &child).is_ok());
        }
        // Providers Cupola does enforce are unchanged: same or narrower.
        let codex_child = thread(AgentFamily::Codex, "workspace", "ask");
        assert!(within_authority(&asking, &codex_child).is_ok());
        let codex_full_child = thread(AgentFamily::Codex, "full_access", "ask");
        assert!(within_authority(&asking, &codex_full_child).is_err());
        let auto_child = thread(AgentFamily::Claude, "workspace", "auto");
        assert!(within_authority(&asking, &auto_child).is_err());
    }

    #[tokio::test]
    async fn ask_threads_cannot_start_or_direct_threads_under_their_own_settings() {
        let fx = fx().await;
        // The fixture parent is a Workspace Codex thread, which always asks.
        for provider in ["opencode", "cursor"] {
            let args = json!({"clientRequestId":fresh(),"prompt":"x","provider":provider});
            assert_eq!(
                run(&fx, DELEGATE, args).await.unwrap_err(),
                "permission_escalation",
                "delegate to {provider}"
            );
        }
        assert_eq!(
            fx.state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            1,
            "the refused delegations created nothing"
        );
        for (id, family) in [("oc", AgentFamily::OpenCode), ("cu", AgentFamily::Cursor)] {
            add_thread(&fx, id, id, family, "workspace", "now").await;
            let send = json!({"threadId":id,"message":"hi","clientRequestId":fresh()});
            assert_eq!(
                run(&fx, SEND, send).await.unwrap_err(),
                "permission_escalation",
                "send to {id}"
            );
        }
        // At Full access the same sends pass the authority check.
        let full = full_access_parent(&fx).await;
        let send = json!({"threadId":"oc","message":"hi","clientRequestId":fresh()});
        let result = execute(&fx.state, &full, SEND, send).await;
        assert!(
            result
                .as_ref()
                .err()
                .is_none_or(|e| e.code != "permission_escalation"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn delegate_rejects_unavailable_targets_plan_mode_and_runaway_depth() {
        let fx = fx().await;
        let bad_model = json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"not-a-model"});
        assert_eq!(
            run(&fx, DELEGATE, bad_model).await.unwrap_err(),
            "unavailable_model"
        );
        let no_claude = json!({"clientRequestId":fresh(),"prompt":"x","provider":"claude","model":"claude:sonnet"});
        assert_eq!(
            run(&fx, DELEGATE, no_claude).await.unwrap_err(),
            "unavailable_provider"
        );
        let bad_provider = json!({"clientRequestId":fresh(),"prompt":"x","provider":"gemini"});
        assert_eq!(
            run(&fx, DELEGATE, bad_provider).await.unwrap_err(),
            "invalid"
        );
        // OpenCode runs under its own settings on the Mac: no access choice,
        // and it is refused while its runtime is missing.
        let chosen = json!({"clientRequestId":fresh(),"prompt":"x","provider":"opencode","accessMode":"read_only"});
        assert_eq!(run(&fx, DELEGATE, chosen).await.unwrap_err(), "invalid");
        let no_opencode = json!({"clientRequestId":fresh(),"prompt":"x","provider":"opencode"});
        assert_eq!(
            run(&fx, DELEGATE, no_opencode.clone()).await.unwrap_err(),
            "permission_escalation",
            "an Ask thread may not start a thread under OpenCode's own settings"
        );
        // A Full access thread may; the runtime is still missing here.
        let full = full_access_parent(&fx).await;
        assert_eq!(
            execute(&fx.state, &full, DELEGATE, no_opencode)
                .await
                .unwrap_err()
                .code,
            "unavailable_provider"
        );
        assert_eq!(
            fx.state
                .store
                .project_conversations("project")
                .await
                .unwrap()
                .len(),
            1
        );
        // A chain three deep cannot grow another level.
        for (child, parent) in [("a", "project-chat"), ("b", "a"), ("c", "b")] {
            add_thread(
                &fx,
                child,
                child,
                AgentFamily::Codex,
                "workspace",
                "2026-01-01T00:00:00Z",
            )
            .await;
            fx.state
                .store
                .record_thread_delegation(child, parent, &fresh(), "owner", "now")
                .await
                .unwrap();
        }
        let mut deep = caller_of(&fx.state, &fx.parent.message.id).await;
        deep.conversation = fx
            .state
            .store
            .project_conversation("c")
            .await
            .unwrap()
            .unwrap();
        let args =
            json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake"});
        assert_eq!(
            execute(&fx.state, &deep, DELEGATE, args.clone())
                .await
                .unwrap_err()
                .code,
            "depth_limit"
        );
        let mut planning = caller_of(&fx.state, &fx.parent.message.id).await;
        planning.conversation.plan_mode = true;
        assert_eq!(
            execute(&fx.state, &planning, DELEGATE, args)
                .await
                .unwrap_err()
                .code,
            "plan_mode"
        );
        let send = json!({"threadId":"a","message":"x","clientRequestId":fresh()});
        assert_eq!(
            execute(&fx.state, &planning, SEND, send)
                .await
                .unwrap_err()
                .code,
            "plan_mode"
        );
    }

    // Contract: a planning thread must not ask the owner for a secret or run a
    // host command with one, even when its chat has Full access.
    #[tokio::test]
    async fn secret_tools_refuse_a_planning_thread() {
        let fx = fx().await;
        let mut planning = full_access_parent(&fx).await;
        planning.conversation.plan_mode = true;
        let request = json!({"name":"DEPLOY_TOKEN","purpose":"deploy"});
        let run =
            json!({"secretRef":"secret:x","command":"true","delivery":"file","variable":"TOKEN"});
        for (tool, args) in [(secret::NAME, request), (secret::RUN, run)] {
            let error = execute(&fx.state, &planning, tool, args.clone())
                .await
                .unwrap_err();
            assert_eq!(error.code, "plan_mode", "{tool}");
            planning.conversation.plan_mode = false;
            let after = execute(&fx.state, &planning, tool, args).await;
            assert!(
                after.as_ref().err().is_none_or(|e| e.code != "plan_mode"),
                "{tool} outside plan: {after:?}"
            );
            planning.conversation.plan_mode = true;
        }
    }

    #[tokio::test]
    async fn delegate_wait_blocks_until_the_child_ends() {
        for timeout in [None, Some(1), Some(1800)] {
            let fx = fx().await;
            let mut args = json!({"clientRequestId":fresh(),"prompt":"Quick job","provider":"codex","model":"fake","mode":"wait"});
            if let Some(timeout) = timeout {
                args["timeoutSeconds"] = json!(timeout);
            }
            let store = fx.state.store.clone();
            let watcher = tokio::spawn(async move {
                // Stand in for the dispatcher: finish whatever the child was given.
                for _ in 0..200 {
                    let children = store.project_conversations("project").await.unwrap();
                    if let Some(child) = children
                        .into_iter()
                        .find(|c| c.conversation_id != "project-chat")
                    {
                        if let Some(message) = store
                            .open_message_ids(&child.conversation_id)
                            .await
                            .unwrap()
                            .first()
                        {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            store
                                .update_message_delivery(message, "completed", None, None)
                                .await
                                .unwrap();
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
            let done =
                tokio::time::timeout(Duration::from_secs(10), run(&fx, DELEGATE, args.clone()))
                    .await
                    .expect("the fixture must finish without waiting for the full timeout")
                    .unwrap();
            watcher.await.unwrap();
            assert_eq!(
                (done["status"].as_str(), done["outcome"].as_str()),
                (Some("finished"), Some("completed"))
            );
            assert_eq!(done["created"], true);
            // Waiting again with a different valid timeout reuses the same task.
            args["timeoutSeconds"] = json!(1);
            let replay = run(&fx, DELEGATE, args).await.unwrap();
            assert_eq!(replay["created"], false);
            assert_eq!(replay["threadId"], done["threadId"]);
            assert_eq!(replay["messageId"], done["messageId"]);
            assert_eq!(replay["outcome"], "completed");
            println!("delegate wait timeout={timeout:?}: finished/completed; replay reused child and message");
            // The parent has now seen the result itself, so no wake-up follows.
            settle(&fx, &fx.parent.message.id, "completed").await;
            wake_parents(&fx.state).await;
            assert!(fx
                .state
                .store
                .open_message_ids("project-chat")
                .await
                .unwrap()
                .is_empty());
        }
    }

    // Contract (T3 Code's delegation rule): the child reads only the task and
    // a role line, never the parent's conversation.
    #[tokio::test]
    async fn delegate_sends_only_the_task_and_its_role() {
        let fx = fx().await;
        let mut bodies = Vec::new();
        for role in [Some("review"), Some("general"), None] {
            let mut args = json!({"clientRequestId":fresh(),"prompt":"Check the parser","provider":"codex","model":"fake"});
            if let Some(role) = role {
                args["role"] = json!(role);
            }
            let child = run(&fx, DELEGATE, args).await.unwrap();
            let queued = fx
                .state
                .store
                .pending_queue(child["threadId"].as_str().unwrap())
                .await
                .unwrap();
            bodies.push(queued[0].body.clone());
            // The title still comes from the task, not the role line.
            assert_eq!(child["title"], "Check the parser");
        }
        assert_eq!(
            bodies,
            [
                "Act as the review sub-agent for this task.\n\nCheck the parser",
                "Check the parser",
                "Check the parser"
            ]
        );
        let bad = json!({"clientRequestId":fresh(),"prompt":"x","provider":"codex","model":"fake","role":"boss"});
        assert_eq!(run(&fx, DELEGATE, bad).await.unwrap_err(), "invalid");
        // A Read only parent cannot hand work to OpenCode, whose own settings
        // are recorded as Workspace.
        let mut reader = fx.parent.conversation.clone();
        reader.access_mode = "read_only".into();
        let narrow = Caller {
            runtime: None,
            message: fx.parent.message.clone(),
            conversation: reader,
            project: fx.parent.project.clone(),
        };
        let to_opencode = json!({"clientRequestId":fresh(),"prompt":"x","provider":"opencode"});
        assert_eq!(
            execute(&fx.state, &narrow, DELEGATE, to_opencode)
                .await
                .unwrap_err()
                .code,
            "permission_escalation"
        );
    }

    // Contract: a delegated task's outcome and final reply are recorded once,
    // in the store, when its work first ends; a parent reads them after a
    // restart even though the provider's history is gone, and later work in
    // the child does not replace them.
    #[tokio::test]
    async fn a_delegated_result_is_recorded_once_and_survives_a_restart() {
        let fx = fx().await;
        let task = delegated(&fx, "a", "Tests", "project-chat").await;
        assert_eq!(task_result(&fx.state, "a").await, None, "still working");
        settle(&fx, &task.id, "completed").await;
        settle(&fx, &fx.parent.message.id, "completed").await;
        // The wake-up records the result before it tells the parent.
        wake_parents(&fx.state).await;
        let recorded = fx
            .state
            .store
            .delegation_result("a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (recorded.outcome.as_str(), recorded.text.as_deref()),
            ("completed", None)
        );
        // A restart: a new store on the same database. The fixture's fake
        // runtime has no history for the child, as after a lost session.
        let mut restarted = fx.state.clone();
        restarted.store = wonder_store::Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            fx.dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let parent = caller_of(&restarted, &fx.parent.message.id).await;
        let waited = execute(&restarted, &parent, WAIT, json!({"threadId":"a"}))
            .await
            .unwrap();
        assert_eq!(waited["status"], "idle");
        assert_eq!(waited["taskResult"]["outcome"], "completed");
        assert_eq!(waited["taskResult"]["finishedAt"], json!(recorded.at));
        // Later work that fails does not rewrite the task's result.
        let later = queue(&fx, "a", "one more thing").await;
        settle(&fx, &later.id, "failed").await;
        assert_eq!(task_result(&restarted, "a").await, Some(recorded));
        // A thread that was not delegated has no task result.
        assert_eq!(task_result(&restarted, "project-chat").await, None);
    }

    async fn delegated(
        fx: &Fx,
        id: &str,
        title: &str,
        parent: &str,
    ) -> wonder_store::StoredMessage {
        add_thread(
            fx,
            id,
            title,
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        fx.state
            .store
            .record_thread_delegation(id, parent, &fresh(), "owner", "now")
            .await
            .unwrap();
        queue(fx, id, "task").await
    }

    #[tokio::test]
    async fn two_finished_children_wake_an_idle_parent_once() {
        let fx = fx().await;
        let a = delegated(&fx, "a", "Tests", "project-chat").await;
        let b = delegated(&fx, "b", "Docs", "project-chat").await;
        settle(&fx, &a.id, "completed").await;
        settle(&fx, &b.id, "failed").await;
        // The parent is still running its turn: nothing is sent into it.
        wake_parents(&fx.state).await;
        assert_eq!(
            fx.state
                .store
                .open_message_ids("project-chat")
                .await
                .unwrap()
                .len(),
            1
        );
        settle(&fx, &fx.parent.message.id, "completed").await;
        wake_parents(&fx.state).await;
        wake_parents(&fx.state).await;
        let open = fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap();
        assert_eq!(
            open.len(),
            1,
            "one batched wake-up, however often the loop runs"
        );
        let wake = fx
            .state
            .store
            .message_by_id(&open[0])
            .await
            .unwrap()
            .unwrap();
        assert!(
            wake.body.contains("\"Tests\" (completed)") && wake.body.contains("\"Docs\" (failed)"),
            "{}",
            wake.body
        );
        assert!(wake.body.contains("wonder_thread_read"));
        let source = fx
            .state
            .store
            .message_source(&wake.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.kind, "wake");
        // It reaches the timeline but not the owner's queue.
        assert!(fx
            .state
            .store
            .pending_queue("project-chat")
            .await
            .unwrap()
            .is_empty());
        assert!(fx
            .state
            .store
            .queued_message_bodies("project-chat")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(provider_text(Some(&source), &wake.body), wake.body);
    }

    #[tokio::test]
    async fn a_later_completion_waits_for_the_next_idle_moment() {
        let fx = fx().await;
        settle(&fx, &fx.parent.message.id, "completed").await;
        let a = delegated(&fx, "a", "First", "project-chat").await;
        settle(&fx, &a.id, "completed").await;
        wake_parents(&fx.state).await;
        let first = fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        // Another child ends while the wake-up is still queued or running.
        let c = delegated(&fx, "c", "Late", "project-chat").await;
        settle(&fx, &c.id, "completed").await;
        wake_parents(&fx.state).await;
        assert_eq!(
            fx.state
                .store
                .open_message_ids("project-chat")
                .await
                .unwrap(),
            first
        );
        settle(&fx, &first[0], "completed").await;
        wake_parents(&fx.state).await;
        let second = fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap();
        assert_eq!(second.len(), 1);
        let body = fx
            .state
            .store
            .message_by_id(&second[0])
            .await
            .unwrap()
            .unwrap()
            .body;
        assert!(
            body.contains("\"Late\"") && !body.contains("\"First\""),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_nested_child_wakes_its_parent_not_the_root_and_queued_user_work_blocks_a_wake() {
        let fx = fx().await;
        settle(&fx, &fx.parent.message.id, "completed").await;
        let mid = delegated(&fx, "mid", "Middle", "project-chat").await;
        settle(&fx, &mid.id, "completed").await;
        let leaf = delegated(&fx, "leaf", "Leaf", "mid").await;
        settle(&fx, &leaf.id, "completed").await;
        // The user has typed a follow-up for the middle thread: it is not idle,
        // so neither it nor the root is told anything yet.
        let typed = queue(&fx, "mid", "one more thing").await;
        wake_parents(&fx.state).await;
        assert!(fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            fx.state.store.open_message_ids("mid").await.unwrap(),
            std::slice::from_ref(&typed.id)
        );
        // Once it is done the leaf's result goes to Middle, not to the root, and
        // the root waits until Middle has finished acting on it.
        settle(&fx, &typed.id, "completed").await;
        wake_parents(&fx.state).await;
        assert!(fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap()
            .is_empty());
        let mid_open = fx.state.store.open_message_ids("mid").await.unwrap();
        assert_eq!(mid_open.len(), 1);
        let body = fx
            .state
            .store
            .message_by_id(&mid_open[0])
            .await
            .unwrap()
            .unwrap()
            .body;
        assert!(body.contains("\"Leaf\""));
        settle(&fx, &mid_open[0], "completed").await;
        wake_parents(&fx.state).await;
        let root = fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap();
        assert_eq!(root.len(), 1);
        let root_body = fx
            .state
            .store
            .message_by_id(&root[0])
            .await
            .unwrap()
            .unwrap()
            .body;
        assert!(root_body.contains("Middle") && !root_body.contains("Leaf"));
    }

    #[tokio::test]
    async fn new_work_for_a_child_is_reported_again_and_reading_it_cancels_a_wake() {
        let fx = fx().await;
        let a = delegated(&fx, "a", "Worker", "project-chat").await;
        settle(&fx, &a.id, "completed").await;
        settle(&fx, &fx.parent.message.id, "completed").await;
        wake_parents(&fx.state).await;
        let wake = fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap();
        settle(&fx, &wake[0], "completed").await;
        wake_parents(&fx.state).await;
        assert!(
            fx.state
                .store
                .open_message_ids("project-chat")
                .await
                .unwrap()
                .is_empty(),
            "already reported"
        );
        // A new message from the parent rearms the child.
        let mut running_parent = caller_of(&fx.state, &fx.parent.message.id).await;
        running_parent.message.state = "streaming".into();
        let again = json!({"threadId":"a","message":"again","clientRequestId":fresh()});
        execute(&fx.state, &running_parent, SEND, again)
            .await
            .unwrap();
        let open = fx.state.store.open_message_ids("a").await.unwrap();
        settle(&fx, &open[0], "completed").await;
        // Reading the idle child's result makes a wake-up unnecessary.
        let read = execute(&fx.state, &running_parent, READ, json!({"threadId":"a"}))
            .await
            .unwrap();
        assert_eq!(read["status"], "idle");
        wake_parents(&fx.state).await;
        assert!(fx
            .state
            .store
            .open_message_ids("project-chat")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn runtime_calls_are_correlated_scoped_and_replayed() {
        let fx = fx().await;
        let rpc = crate::projects::codex_rpc(&fx.state).await.unwrap();
        let runtime = rpc.health().id().to_owned();
        let params = |call: &str, tool: &str, arguments: Value| json!({"threadId":"thread","turnId":"turn","callId":call,"tool":tool,"arguments":arguments});
        let message = Some(&fx.parent.message);
        let first = handle_call(&fx.state, &runtime, &params("c1", LIST, json!({})), message).await;
        assert_eq!(first["success"], true);
        // The same runtime call replays its saved answer; new arguments are refused.
        assert_eq!(
            handle_call(&fx.state, &runtime, &params("c1", LIST, json!({})), message).await,
            first
        );
        let reused = handle_call(
            &fx.state,
            &runtime,
            &params("c1", LIST, json!({"limit":1})),
            message,
        )
        .await;
        assert_eq!(reused["success"], false);
        assert!(reused["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("call_reused"));
        // Another turn's request is not accepted for this message.
        let wrong =
            json!({"threadId":"thread","turnId":"other","callId":"c2","tool":LIST,"arguments":{}});
        assert_eq!(
            handle_call(&fx.state, &runtime, &wrong, message).await["success"],
            false
        );
        assert_eq!(
            handle_call(&fx.state, &runtime, &params("c3", LIST, json!({})), None).await["success"],
            false
        );
        // A conversation that is not a Project thread has no thread tools.
        let mut elsewhere = fx.parent.message.clone();
        elsewhere.conversation_id = "not-a-project".into();
        let answer = handle_call(
            &fx.state,
            &runtime,
            &params("c4", LIST, json!({})),
            Some(&elsewhere),
        )
        .await;
        assert!(answer["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not_project"));
    }

    // Contract: a Codex thread keeps the tools it started with, so while the
    // owner has them off for its provider every call is declined with the
    // reason, and calls work again once they are back on.
    #[tokio::test]
    async fn calls_are_declined_while_the_owner_has_the_tools_off() {
        let fx = fx().await;
        let rpc = crate::projects::codex_rpc(&fx.state).await.unwrap();
        let runtime = rpc.health().id().to_owned();
        let family = fx
            .state
            .store
            .project_conversation(&fx.parent.message.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .family;
        let params = |call: &str| json!({"threadId":"thread","turnId":"turn","callId":call,"tool":LIST,"arguments":{}});
        let message = Some(&fx.parent.message);
        fx.state
            .store
            .set_agent_thread_tools(family, false, "now")
            .await
            .unwrap();
        let declined = handle_call(&fx.state, &runtime, &params("off"), message).await;
        assert_eq!(declined["success"], false);
        assert!(declined["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("turned off"));
        fx.state
            .store
            .set_agent_thread_tools(family, true, "now")
            .await
            .unwrap();
        let answer = handle_call(&fx.state, &runtime, &params("on"), message).await;
        assert_eq!(answer["success"], true);
    }

    #[tokio::test]
    async fn a_new_codex_project_thread_registers_the_tools_at_start() {
        let fx = fx().await;
        // The fake runtime would hand out the id the parent already owns.
        let script = fx.dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&script).unwrap();
        let original = "elif method == 'thread/start': result = {'thread':{'id':'thread'}}";
        assert!(source.contains(original));
        std::fs::write(
            &script,
            source.replace(
                original,
                "elif method == 'thread/start': result = {'thread':{'id':'fresh-thread'}}",
            ),
        )
        .unwrap();
        fx.state.projects.shutdown().await;
        let id = fresh();
        add_thread(
            &fx,
            &id,
            "Fresh",
            AgentFamily::Codex,
            "workspace",
            "2026-01-01T00:00:00Z",
        )
        .await;
        let message = queue(&fx, &id, "Hello").await;
        assert!(fx
            .state
            .store
            .claim_message_for_dispatch(&message.id)
            .await
            .unwrap());
        let message = fx
            .state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        let mut submitting = false;
        crate::projects::dispatch_inner(&fx.state, &message, &mut submitting)
            .await
            .unwrap();
        let requests = std::fs::read_to_string(fx.dir.path().join("requests-jsonl")).unwrap();
        let start: Value = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|request| request["method"] == "thread/start")
            .unwrap();
        let names: Vec<_> = start["params"]["dynamicTools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let offered: Vec<_> = specs()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, offered);
    }

    // Contract: a visual is kept as a verified file of the caller's own
    // conversation and the result says what shows, what was left out and why;
    // `replace` names only an earlier visual of that same conversation.
    #[tokio::test]
    async fn show_visual_keeps_a_verified_file_and_reports_what_shows() {
        let fx = fx().await;
        let caller = running_peer(&fx, &fresh()).await;
        let other = running_peer(&fx, &fresh()).await;
        let state = &fx.state;
        async fn show(state: &AppState, caller: &Caller, args: Value) -> Result<Value, ToolError> {
            execute(state, caller, SHOW_VISUAL, args).await
        }
        let html = "<h2>Options</h2><p style=\"color:#16a34a\">Riverside wins.</p><table><tr><td>a</td></tr></table><iframe></iframe><script>build()</script>";
        let first = show(state, &caller, json!({"title":"Walk: options","html":html}))
            .await
            .unwrap();
        assert_eq!(first["kind"], "html");
        assert_eq!(first["title"], "Walk options");
        assert_eq!(first["shows"], json!({"heading":1,"paragraph":1,"table":1}));
        assert_eq!(first["omitted"][0]["what"], "embedded page or object");
        let warnings = first["warnings"].to_string();
        assert!(
            warnings.contains("1 script will run once offscreen"),
            "{warnings}"
        );
        assert!(warnings.contains("#16a34a"), "{warnings}");
        let conversation = &caller.conversation.conversation_id;
        let files = fx
            .state
            .store
            .list_conversation_files(conversation)
            .await
            .unwrap();
        let file = files.iter().find(|f| f.id == first["visualId"]).unwrap();
        assert_eq!(file.mime_type.as_deref(), Some("text/html"));
        assert_eq!(file.name, "Walk options.html");
        assert_eq!(
            file.sha256.as_deref(),
            Some(hex::encode(Sha256::digest(html)).as_str())
        );
        assert!(visual::is_visual(file));

        let document = json!({"version":1,"fallback":"Riverside: 45 minutes.","components":[
            {"id":"t","type":"table","title":"Routes","columns":["Route","Time"],"rows":[["Riverside","45"]]}]});
        let second = show(
            state,
            &caller,
            json!({"title":"Routes","components":document,"replace":first["visualId"]}),
        )
        .await
        .unwrap();
        assert_eq!(second["replaces"], first["visualId"]);
        assert_eq!(second["shows"], json!({"table":1}));
        // The Project's Outputs link the revision to the visual it updates.
        let project = fx
            .state
            .store
            .project_conversation(&caller.conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .project_id;
        let outputs = fx
            .state
            .store
            .project_outputs(
                &project,
                &wonder_store::ProjectOutputQuery {
                    origin: wonder_store::ProjectOutputOrigin::Agent,
                    file_type: wonder_store::ProjectOutputType::Pages,
                    name: None,
                    after: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        let revision = outputs
            .iter()
            .find(|output| output.file.id == second["visualId"])
            .unwrap();
        assert_eq!(
            revision.replaces_file_id.as_deref(),
            first["visualId"].as_str()
        );
        let files = fx
            .state
            .store
            .list_conversation_files(conversation)
            .await
            .unwrap();
        let file = files.iter().find(|f| f.id == second["visualId"]).unwrap();
        assert_eq!(file.mime_type.as_deref(), Some(visual::COMPONENTS_MIME));
        assert_eq!(
            file.byte_size,
            Some(serde_json::to_vec(&document).unwrap().len() as i64)
        );

        // A preview posts nothing. This host has no helper to draw it, so the
        // result is the checks alone; a drawn one's pictures become image
        // content beside the text.
        let kept = |files: Vec<wonder_store::StoredConversationFile>| files.len();
        let before = kept(
            state
                .store
                .list_conversation_files(conversation)
                .await
                .unwrap(),
        );
        let preview = show(
            state,
            &caller,
            json!({"title":"Walk: options","html":html,"preview":true}),
        )
        .await
        .unwrap();
        assert_eq!(preview["exact"], false, "{preview}");
        assert_eq!(preview["posted"], false);
        assert!(preview.get("visualId").is_none());
        assert_eq!(preview["shows"], first["shows"]);
        assert!(preview["previewUnavailable"]
            .as_str()
            .unwrap()
            .contains("can't draw"));
        assert_eq!(
            kept(
                state
                    .store
                    .list_conversation_files(conversation)
                    .await
                    .unwrap()
            ),
            before
        );
        let wire = response(Ok(preview));
        assert_eq!(wire["contentItems"].as_array().unwrap().len(), 1);
        let drawn = json!({"preview":true,"previewImage":[{"textSize":"standard","imageUrl":"data:image/png;base64,AAAA"}]});
        let wire = response(Ok(drawn));
        assert_eq!(
            wire["contentItems"][1],
            json!({"type":"inputImage","imageUrl":"data:image/png;base64,AAAA"})
        );
        assert!(!wire["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .contains("AAAA"));

        // Another thread's visual is not this conversation's to replace.
        let code = |result: Result<Value, ToolError>| result.err().map(|e| e.code);
        assert_eq!(
            code(
                show(
                    state,
                    &other,
                    json!({"title":"x","html":"<p>x</p>","replace":first["visualId"]})
                )
                .await
            ),
            Some("not_found")
        );
        assert!(fx
            .state
            .store
            .list_conversation_files(&other.conversation.conversation_id)
            .await
            .unwrap()
            .is_empty());
        for (args, expected) in [
            (json!({"title":"x"}), "invalid"),
            (
                json!({"title":"x","html":"<p>x</p>","components":document}),
                "invalid",
            ),
            (json!({"title":"x","html":"  "}), "invalid"),
            (json!({"title":"x","html":7}), "invalid"),
            (json!({"html":"<p>x</p>"}), "invalid"),
            (
                json!({"title":"x","html":"<p>x</p>","colour":"red"}),
                "invalid",
            ),
            (
                json!({"title":"x","html":"<p>x</p>","replace":3}),
                "invalid",
            ),
            (
                json!({"title":"x","html":"<p>x</p>","preview":"yes"}),
                "invalid",
            ),
            (json!({"title":"x","components":{"version":1}}), "invalid"),
            (
                json!({"title":"x","html":"a".repeat(visual::HTML_BYTES + 1)}),
                "too_large",
            ),
        ] {
            assert_eq!(
                code(show(state, &caller, args.clone()).await),
                Some(expected),
                "{args}"
            );
        }
    }

    // Contract: a page longer than other tools' argument limit reaches the
    // visual tool through the runtime path; other tools keep their limit.
    #[tokio::test]
    async fn a_visual_page_may_exceed_other_tools_argument_limit() {
        let fx = fx().await;
        let rpc = crate::projects::codex_rpc(&fx.state).await.unwrap();
        let runtime = rpc.health().id().to_owned();
        let params = |call: &str, tool: &str, arguments: Value| json!({"threadId":"thread","turnId":"turn","callId":call,"tool":tool,"arguments":arguments});
        let page = format!("<p>{}</p>", "x".repeat(200 * 1024));
        let message = Some(&fx.parent.message);
        let text = |answer: Value| {
            answer["contentItems"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        // The fixture's thread has no media storage, so the call gets past
        // the size check and stops at storage.
        let answer = handle_call(
            &fx.state,
            &runtime,
            &params("v1", SHOW_VISUAL, json!({"title":"Big","html":page})),
            message,
        )
        .await;
        assert!(text(answer).contains("unavailable"));
        let answer = handle_call(
            &fx.state,
            &runtime,
            &params("v2", LIST, json!({"cursor":page})),
            message,
        )
        .await;
        assert!(text(answer).contains("exceed the supported size"));
    }

    // ---------------------------------------------------------------------
    // schedule
    // ---------------------------------------------------------------------

    async fn schedule_fx() -> Fx {
        let fx = fx().await;
        // The parent's own thread is the default destination; it must be
        // a bound Project thread, as it is for any running turn.
        crate::test_support::bind_project_thread(
            &fx.state,
            "project-chat",
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        fx
    }

    async fn bound_peer(fx: &Fx, id: &str, access: &str) {
        add_thread(fx, id, id, AgentFamily::Codex, access, "now").await;
        crate::test_support::bind_project_thread(
            &fx.state,
            id,
            AgentFamily::Codex,
            &format!("native-{id}"),
            None,
            "now",
        )
        .await
        .unwrap();
    }

    fn summary_json(automation: wonder_store::StoredAutomation) -> Value {
        serde_json::to_value(crate::automation_summary(automation)).unwrap()
    }

    // Contract: "check CI in 20 minutes" saves one single-run automation for
    // this chat that the owner's Automations list shows (with the chat that
    // made it), a retry returns that same one, and the reply is one plain
    // sentence. When it runs, its prompt is posted into this same chat and
    // the automation has no next run.
    #[tokio::test]
    async fn schedule_once_lands_in_the_same_chat_and_retries_make_one() {
        let fx = schedule_fx().await;
        let request = fresh();
        let args = json!({"name":"Check CI","prompt":"Check whether CI passed.","when":{"inMinutes":20},
            "timezone":"Europe/London","clientRequestId":request});
        let first = run(&fx, SCHEDULE, args.clone()).await.unwrap();
        assert_eq!(first["outcome"], "created", "{first}");
        assert_eq!(first["threadId"], "project-chat");
        let message = first["message"].as_str().unwrap();
        assert!(
            message.starts_with("Scheduled Check CI for ")
                && message.ends_with(". Pause or delete it in Automations."),
            "{message}"
        );
        assert!(first["note"].as_str().unwrap().contains("computer is on"));
        let next: chrono::DateTime<Utc> = first["nextRunAt"].as_str().unwrap().parse().unwrap();
        let minutes = (next - Utc::now()).num_minutes();
        assert!((18..=20).contains(&minutes), "{next}");

        // A retry seconds later is still "20 minutes from the first try".
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let retry = run(&fx, SCHEDULE, args).await.unwrap();
        assert_eq!(retry["automationId"], first["automationId"]);
        assert_eq!(retry["outcome"], "already_scheduled");
        assert_eq!(retry["nextRunAt"], first["nextRunAt"]);
        let saved = fx.state.store.list_automations().await.unwrap();
        assert_eq!(saved.len(), 1);
        let shown = summary_json(saved[0].clone());
        assert_eq!(shown["createdByConversationId"], "project-chat");
        assert_eq!(shown["scopeId"], "project-chat");
        assert!(shown["rrule"].as_str().unwrap().ends_with(";COUNT=1"));

        // Run it now, as the scheduler does when it is due.
        let automation = saved[0].clone();
        let due = automation.next_run_at.clone().unwrap();
        assert!(fx
            .state
            .store
            .claim_scheduled_automation("once-run", &automation, &due, &now_text(), None, true)
            .await
            .unwrap());
        crate::run_automation(fx.state.clone(), automation.clone(), due, "once-run".into()).await;
        let run_row = fx
            .state
            .store
            .automation_run_by_id("once-run")
            .await
            .unwrap()
            .unwrap();
        let posted = fx
            .state
            .store
            .message_by_id(run_row.message_id.as_deref().expect("posted"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(posted.conversation_id, "project-chat");
        assert_eq!(posted.body, "Check whether CI passed.");
        let listed = run(&fx, SCHEDULES, json!({})).await.unwrap();
        assert_eq!(listed["automations"][0]["status"], "done", "{listed}");
        assert!(listed["automations"][0]["nextRunAt"].is_null());
        // Once its time has passed, it cannot simply be resumed.
        let mut ran = fx
            .state
            .store
            .automation_by_id(&automation.id)
            .await
            .unwrap()
            .unwrap();
        ran.rrule =
            crate::automation_schedule::one_shot_rule(Utc::now() - chrono::Duration::minutes(1));
        assert!(fx.state.store.update_automation(&ran).await.unwrap());
        let resumed = run(
            &fx,
            SCHEDULE_UPDATE,
            json!({"automationId":first["automationId"],"status":"active"}),
        )
        .await;
        assert_eq!(
            resumed.unwrap_err(),
            "invalid",
            "a past single run cannot resume"
        );
    }

    // Contract: "every weekday at 9" becomes the rule the phone's editor
    // writes for weekdays, says so in the owner's words, and can be paused
    // and deleted by the agent; one the owner made is theirs alone.
    #[tokio::test]
    async fn schedule_every_weekday_is_paused_and_deleted_by_its_agent_only() {
        let fx = schedule_fx().await;
        let created = run(
            &fx,
            SCHEDULE,
            json!({"name":"Check CI","prompt":"Summarise new PRs.",
            "when":{"every":{"frequency":"daily","weekdays":["FR","MO","TU","WE","TH"],"hour":9}},
            "timezone":"America/New_York","notify":"failed_runs_only","clientRequestId":fresh()}),
        )
        .await
        .unwrap();
        assert_eq!(
            created["message"],
            "Scheduled Check CI for 9:00 every weekday. Pause or delete it in Automations."
        );
        let id = created["automationId"].as_str().unwrap().to_owned();
        let saved = fx.state.store.automation_by_id(&id).await.unwrap().unwrap();
        assert_eq!(
            saved.rrule,
            "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0"
        );
        assert_eq!(saved.notification_policy, "failed_runs_only");
        assert_eq!(created["nextRunLocal"].as_str().unwrap()[11..16], *"09:00");

        let paused = run(
            &fx,
            SCHEDULE_UPDATE,
            json!({"automationId":id,"status":"paused"}),
        )
        .await
        .unwrap();
        assert_eq!(paused["status"], "paused");
        assert_eq!(
            paused["message"],
            "Paused Check CI. Resume or delete it in Automations."
        );
        let moved = run(
            &fx,
            SCHEDULE_UPDATE,
            json!({"automationId":id,"status":"active",
            "when":{"every":{"frequency":"hourly","minute":15}}}),
        )
        .await
        .unwrap();
        assert_eq!(
            moved["message"],
            "Resumed Check CI for every hour at :15. Pause or delete it in Automations."
        );

        // The owner's own automation is listed but not the agent's to change.
        let (status, owner) = crate::save_automation(
            &fx.state,
            crate::CreateAutomationRequest {
                name: "Owner brief".into(),
                kind: "continuation".into(),
                conversation_id: Some("project-chat".into()),
                prompt: "Brief me.".into(),
                rrule: "FREQ=DAILY;BYHOUR=8;BYMINUTE=0".into(),
                timezone: "UTC".into(),
                status: None,
                notification_policy: None,
                scope_type: None,
                scope_id: Some("project-chat".into()),
                client_request_id: Some("owner-request".into()),
            },
            "automation",
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        let listed = run(&fx, SCHEDULES, json!({})).await.unwrap();
        let listed = listed["automations"].as_array().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed
            .iter()
            .any(|item| item["automationId"] == owner.id && item["createdByAgent"] == false));
        for args in [
            json!({"automationId":owner.id,"status":"paused"}),
            json!({"automationId":owner.id,"status":"deleted"}),
        ] {
            assert_eq!(
                run(&fx, SCHEDULE_UPDATE, args).await.unwrap_err(),
                "owner_automation"
            );
        }
        let deleted = run(
            &fx,
            SCHEDULE_UPDATE,
            json!({"automationId":id,"status":"deleted"}),
        )
        .await
        .unwrap();
        assert_eq!(deleted["message"], "Deleted Check CI.");
        assert!(fx
            .state
            .store
            .automation_by_id(&id)
            .await
            .unwrap()
            .is_none());
        assert!(fx
            .state
            .store
            .automation_by_id(&owner.id)
            .await
            .unwrap()
            .is_some());
    }

    // Contract: an agent schedules work only in its own Project, only for a
    // chat with no wider access than its own, and a scheduled run never
    // starts while its destination has since become wider than the chat
    // that made it: the run fails with the reason and posts nothing.
    #[tokio::test]
    async fn scheduled_runs_never_exceed_the_scheduling_chats_access() {
        let fx = schedule_fx().await;
        add_foreign_thread(&fx).await;
        bound_peer(&fx, "wide", "full_access").await;
        bound_peer(&fx, "peer", "workspace").await;
        let args = |thread: &str| {
            json!({"name":"Later","prompt":"Look again.","when":{"inMinutes":5},
            "timezone":"UTC","threadId":thread,"clientRequestId":fresh()})
        };
        assert_eq!(
            run(&fx, SCHEDULE, args("foreign")).await.unwrap_err(),
            "not_found"
        );
        assert_eq!(
            run(&fx, SCHEDULE, args("wide")).await.unwrap_err(),
            "permission_escalation"
        );
        let past = json!({"name":"Late","prompt":"x","when":{"at":"2020-01-01T00:00:00Z"},
            "timezone":"UTC","clientRequestId":fresh()});
        assert_eq!(run(&fx, SCHEDULE, past).await.unwrap_err(), "invalid");
        assert!(fx.state.store.list_automations().await.unwrap().is_empty());

        let created = run(&fx, SCHEDULE, args("peer")).await.unwrap();
        assert!(
            created["message"].as_str().unwrap().contains(" in peer."),
            "{created}"
        );
        fx.state
            .store
            .update_project_conversation(
                "peer",
                ProjectConversationPatch {
                    access_mode: Some("full_access"),
                    ..Default::default()
                },
                "later",
            )
            .await
            .unwrap();
        let automation = fx.state.store.list_automations().await.unwrap().remove(0);
        assert!(fx
            .state
            .store
            .claim_scheduled_automation("wide-run", &automation, "manual", &now_text(), None, false)
            .await
            .unwrap());
        crate::run_automation(
            fx.state.clone(),
            automation,
            "manual".into(),
            "wide-run".into(),
        )
        .await;
        let failed = fx
            .state
            .store
            .automation_run_by_id("wide-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error.unwrap().contains("wider access"));
        assert!(failed.message_id.is_none());
    }

    // Contract: about 20 agent-made automations per Project; the limit never
    // refuses a retry of one already made.
    #[tokio::test]
    async fn schedule_limits_agent_automations_per_project() {
        let fx = schedule_fx().await;
        let args = |request: &str| {
            json!({"name":"Tick","prompt":"Check.","when":{"every":{"frequency":"minutely","interval":30}},
            "timezone":"UTC","clientRequestId":request})
        };
        let first = fresh();
        run(&fx, SCHEDULE, args(&first)).await.unwrap();
        for _ in 1..20 {
            run(&fx, SCHEDULE, args(&fresh())).await.unwrap();
        }
        assert_eq!(
            run(&fx, SCHEDULE, args(&fresh())).await.unwrap_err(),
            "limit"
        );
        assert_eq!(
            run(&fx, SCHEDULE, args(&first)).await.unwrap()["outcome"],
            "already_scheduled"
        );
        assert_eq!(fx.state.store.list_automations().await.unwrap().len(), 20);
    }

    // ---------------------------------------------------------------------
    // notify
    // ---------------------------------------------------------------------

    async fn notify_pool(fx: &Fx) -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            fx.dir.path().join("state.db").display()
        ))
        .await
        .unwrap()
    }

    async fn notify_phone(fx: &Fx) {
        fx.state
            .store
            .register_push("owner", "phone", "https://push.invalid", "secret")
            .await
            .unwrap();
    }

    fn note(title: &str, key: Option<&str>, request: &str) -> Value {
        let mut args =
            json!({"title":title,"body":"The nightly digest is ready.","clientRequestId":request});
        if let Some(key) = key {
            args["dedupeKey"] = json!(key);
        }
        args
    }

    /// The previews the phone would receive now, in send order.
    async fn notify_previews(fx: &Fx) -> Vec<Value> {
        fx.state.store.distribute_push().await.unwrap();
        let mut previews = Vec::new();
        while let Some(delivery) = fx.state.store.next_push().await.unwrap() {
            let preview = crate::push::delivery_preview(&fx.state, &delivery)
                .await
                .unwrap()
                .unwrap();
            previews.push(serde_json::to_value(preview).unwrap());
            fx.state
                .store
                .finish_push_claim(&delivery, "delivered")
                .await
                .unwrap();
        }
        previews
    }

    // Contract: an agent's notification reaches the owner's phone through the
    // push outbox with its own title and text, opens only this chat or a
    // file of this Project (rechecked when it is sent), and never carries an
    // approval or question action. A retry sends nothing more.
    #[tokio::test]
    async fn notify_sends_one_push_that_only_opens_the_chat_or_a_file() {
        let fx = fx().await;
        notify_phone(&fx).await;
        let request = fresh();
        let first = run(&fx, NOTIFY, note("Digest ready", None, &request))
            .await
            .unwrap();
        assert_eq!(first["state"], "queued");
        assert_eq!(
            run(&fx, NOTIFY, note("Digest ready", None, &request))
                .await
                .unwrap()["state"],
            "queued"
        );
        let previews = notify_previews(&fx).await;
        assert_eq!(previews.len(), 1, "{previews:?}");
        let preview = &previews[0];
        assert_eq!(preview["conversationId"], "project-chat");
        assert!(preview["title"]
            .as_str()
            .unwrap()
            .ends_with(" · Digest ready"));
        assert_eq!(preview["body"], "The nightly digest is ready.");
        assert_eq!(preview["open"], json!({"kind":"thread"}));
        assert!(preview.get("approval").is_none() && preview.get("question").is_none());

        // A file of this Project opens in the chat that has it.
        let other = add_thread(&fx, "peer", "Peer", AgentFamily::Codex, "workspace", "now").await;
        fx.state
            .store
            .upsert_conversation_file(
                "report",
                &other.conversation_id,
                "artifact",
                "report.html",
                Some("text/html"),
                Some(10),
                None,
                Some("report.html"),
                "ready",
                None,
                None,
                Some("tool:1"),
                "now",
            )
            .await
            .unwrap();
        let mut args = note("Report", None, &fresh());
        args["open"] = json!({"kind":"file","fileId":"report"});
        assert_eq!(run(&fx, NOTIFY, args).await.unwrap()["state"], "queued");
        let previews = notify_previews(&fx).await;
        assert_eq!(
            previews[0]["open"],
            json!({"kind":"file","fileId":"report","conversationId":"peer"})
        );

        // A file removed before sending falls back to opening the chat.
        let mut args = note("Report again", None, &fresh());
        args["open"] = json!({"kind":"file","fileId":"report"});
        run(&fx, NOTIFY, args).await.unwrap();
        sqlx::query("DELETE FROM conversation_files WHERE id='report'")
            .execute(&notify_pool(&fx).await)
            .await
            .unwrap();
        assert_eq!(
            notify_previews(&fx).await[0]["open"],
            json!({"kind":"thread"})
        );
    }

    // Contract: a chat sends at most three notifications in ten minutes; a
    // fourth is refused as rate_limited, one with an earlier dedupeKey
    // updates that notification instead of sending another (even at the
    // limit), and once the window has passed the chat may send again.
    #[tokio::test]
    async fn notify_is_rate_limited_per_chat_and_coalesced_by_dedupe_key() {
        let fx = fx().await;
        notify_phone(&fx).await;
        for (index, key) in ["ci", "deploy", "digest"].iter().enumerate() {
            let sent = run(
                &fx,
                NOTIFY,
                note(&format!("Update {index}"), Some(key), &fresh()),
            )
            .await
            .unwrap();
            assert_eq!(sent["state"], "queued");
        }
        let limited = fresh();
        assert_eq!(
            run(&fx, NOTIFY, note("One more", None, &limited))
                .await
                .unwrap()["state"],
            "rate_limited"
        );
        let coalesced = fresh();
        assert_eq!(
            run(&fx, NOTIFY, note("CI green", Some("ci"), &coalesced))
                .await
                .unwrap()["state"],
            "coalesced"
        );
        assert_eq!(
            run(&fx, NOTIFY, note("CI green", Some("ci"), &coalesced))
                .await
                .unwrap()["state"],
            "coalesced"
        );
        let previews = notify_previews(&fx).await;
        let titles: Vec<_> = previews
            .iter()
            .map(|preview| {
                preview["title"]
                    .as_str()
                    .unwrap()
                    .rsplit(" · ")
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(titles, ["CI green", "Update 1", "Update 2"]);

        // Another chat has its own allowance.
        let peer = add_thread(&fx, "peer", "Peer", AgentFamily::Codex, "workspace", "now").await;
        let peer = Caller {
            conversation: peer,
            ..caller_of(&fx.state, &fx.parent.message.id).await
        };
        let sent = execute(&fx.state, &peer, NOTIFY, note("Peer", None, &fresh()))
            .await
            .unwrap();
        assert_eq!(sent["state"], "queued");

        sqlx::query("UPDATE push_intents SET created_at=created_at-601")
            .execute(&notify_pool(&fx).await)
            .await
            .unwrap();
        assert_eq!(
            run(&fx, NOTIFY, note("Later", Some("ci"), &fresh()))
                .await
                .unwrap()["state"],
            "queued"
        );
    }

    // Contract: a dedupeKey update only merges into a notification still
    // waiting to be sent. Once the outbox has delivered it, the update is
    // news: it queues and sends a second push with the new text, and it
    // counts toward the chat's limit like any other.
    #[tokio::test]
    async fn notify_update_after_delivery_sends_a_new_push() {
        let fx = fx().await;
        notify_phone(&fx).await;
        let sent = run(&fx, NOTIFY, note("CI running", Some("ci"), &fresh()))
            .await
            .unwrap();
        assert_eq!(sent["state"], "queued");
        let delivered = notify_previews(&fx).await;
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0]["title"]
            .as_str()
            .unwrap()
            .ends_with(" · CI running"));

        let update = run(&fx, NOTIFY, note("CI green", Some("ci"), &fresh()))
            .await
            .unwrap();
        assert_eq!(update["state"], "queued");
        let delivered = notify_previews(&fx).await;
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert!(delivered[0]["title"]
            .as_str()
            .unwrap()
            .ends_with(" · CI green"));

        // While that new one waits, a further update still merges into it.
        run(&fx, NOTIFY, note("CI red", Some("ci"), &fresh()))
            .await
            .unwrap();
        let third = run(&fx, NOTIFY, note("CI green again", Some("ci"), &fresh()))
            .await
            .unwrap();
        assert_eq!(third["state"], "coalesced");
        let delivered = notify_previews(&fx).await;
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert!(delivered[0]["title"]
            .as_str()
            .unwrap()
            .ends_with(" · CI green again"));

        // Three pushes went out in the window, so a fourth is refused.
        assert_eq!(
            run(&fx, NOTIFY, note("CI flaky", Some("ci"), &fresh()))
                .await
                .unwrap()["state"],
            "rate_limited"
        );
        assert!(notify_previews(&fx).await.is_empty());
    }

    // Contract: nothing is recorded or claimed sent when no phone takes
    // notifications, or when the call is malformed or names a file outside
    // this Project; open never takes anything but a chat or a file.
    #[tokio::test]
    async fn notify_refuses_without_a_phone_and_for_bad_targets() {
        let fx = fx().await;
        assert_eq!(
            run(&fx, NOTIFY, note("Digest", None, &fresh()))
                .await
                .unwrap_err(),
            "no_phone"
        );
        notify_phone(&fx).await;
        add_foreign_thread(&fx).await;
        fx.state
            .store
            .upsert_conversation_file(
                "theirs", "foreign", "artifact", "x.html", None, None, None, None, "ready", None,
                None, None, "now",
            )
            .await
            .unwrap();
        let with = |field: &str, value: Value| {
            let mut args = note("Digest", None, &fresh());
            args[field] = value;
            args
        };
        for (args, expected) in [
            (with("title", json!("x".repeat(61))), "invalid"),
            (with("body", json!("é".repeat(181))), "invalid"),
            (
                with("open", json!({"kind":"preview","previewId":"p"})),
                "invalid",
            ),
            (with("open", json!({"kind":"approve"})), "invalid"),
            (
                with("open", json!({"kind":"thread","command":"rm -rf /"})),
                "invalid",
            ),
            (
                with("open", json!({"kind":"file","fileId":"missing"})),
                "not_found",
            ),
            (
                with("open", json!({"kind":"file","fileId":"theirs"})),
                "not_found",
            ),
            (with("action", json!("approve")), "invalid"),
            (with("clientRequestId", json!("not-a-uuid")), "invalid"),
        ] {
            assert_eq!(
                run(&fx, NOTIFY, args.clone()).await.unwrap_err(),
                expected,
                "{args}"
            );
        }
        let intents: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM push_intents WHERE source_key LIKE 'notify:%'",
        )
        .fetch_one(&notify_pool(&fx).await)
        .await
        .unwrap();
        assert_eq!(intents, 0);
    }

    // Contract: any thread reads the preferences, only a full access thread
    // changes them, and nothing outside the safe list (computer use, tools,
    // access, approvals) can be named.
    #[tokio::test]
    async fn preferences_change_only_from_full_access_and_only_safe_fields() {
        let fx = fx().await;
        let with_access = |access: &str| Caller {
            runtime: None,
            message: fx.parent.message.clone(),
            conversation: StoredProjectConversation {
                access_mode: access.to_owned(),
                ..fx.parent.conversation.clone()
            },
            project: fx.parent.project.clone(),
        };
        let call = |access: &str, args: Value| {
            let caller = with_access(access);
            let state = fx.state.clone();
            async move {
                execute(&state, &caller, PREFERENCES, args)
                    .await
                    .map_err(|e| e.code.to_owned())
            }
        };
        let read = call("read_only", json!({})).await.unwrap();
        assert_eq!(read["newChatDefaults"]["startsInT3Code"], false);
        assert_eq!(read["projectSettings"]["autoArchive"]["idleDays"], 3);
        let change = json!({"update":{"startsInT3Code":true,"includeWorktrees":true,
            "autoArchive":{"idleDays":7,"onMerge":false}}});
        for access in ["read_only", "workspace"] {
            assert_eq!(
                call(access, change.clone()).await.unwrap_err(),
                "needs_full_access"
            );
        }
        assert_eq!(
            call("read_only", json!({})).await.unwrap()["newChatDefaults"]["startsInT3Code"],
            false
        );
        for forbidden in [
            "computerUse",
            "threadTools",
            "accessMode",
            "approval",
            "relay",
        ] {
            let args = json!({"update":{forbidden:true}});
            assert_eq!(call("full_access", args).await.unwrap_err(), "invalid");
        }
        let done = call("full_access", change).await.unwrap();
        assert_eq!(done["state"], "updated");
        assert_eq!(done["before"]["newChatDefaults"]["startsInT3Code"], false);
        assert_eq!(done["after"]["newChatDefaults"]["startsInT3Code"], true);
        // A bad later field still says what the earlier one already saved.
        let caller = with_access("full_access");
        let error = execute(
            &fx.state,
            &caller,
            PREFERENCES,
            json!({"update":{"startsInT3Code":false,"customInstruction":
                {"family":"nope","enabled":true,"text":"x"}}}),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("Already changed: startsInT3Code"));
        let again = call("workspace", json!({})).await.unwrap();
        assert_eq!(again["projectSettings"]["includeWorktrees"], true);
        assert_eq!(again["projectSettings"]["autoArchive"]["idleDays"], 7);
    }

    // ---- takeover --------------------------------------------------------

    /// A live phone view of `source` with an active control lease, as Take
    /// control leaves it. Returns the session id.
    async fn hold_control(fx: &Fx, id: &str, source: &str) -> String {
        let store = &fx.state.store;
        let now = Utc::now();
        let at = |offset: i64| {
            (now + chrono::Duration::seconds(offset)).to_rfc3339_opts(SecondsFormat::Millis, true)
        };
        store
            .insert_computer_session(&wonder_store::ComputerSessionCreate {
                id: format!("session-{id}"),
                client_request_id: format!("session-request-{id}"),
                owner_device_id: "phone".into(),
                host_installation_id: fx.state.host_installation_id.clone(),
                conversation_id: "project-chat".into(),
                generation: 1,
                state: wonder_store::ComputerSessionState::Live,
                video_quality: "standard".into(),
                video_max_fps: 15,
                source_id: Some(source.into()),
                source_name: Some("Google Chrome".into()),
                source_kind: Some(
                    if source.starts_with("simulator:") {
                        "simulator"
                    } else {
                        "window"
                    }
                    .into(),
                ),
                source_width: Some(1280),
                source_height: Some(720),
                source_scale: Some(2.0),
                crop_json: None,
                geometry_revision: 1,
                failure_reason: None,
                now: at(0),
            })
            .await
            .unwrap();
        let granted = store
            .acquire_computer_control_lease(&wonder_store::ComputerControlLeaseCreate {
                id: format!("lease-{id}"),
                client_request_id: format!("lease-request-{id}"),
                session_id: format!("session-{id}"),
                owner_device_id: "phone".into(),
                host_installation_id: fx.state.host_installation_id.clone(),
                conversation_id: "project-chat".into(),
                session_generation: 1,
                source_id: Some(source.into()),
                geometry_revision: 1,
                now: at(0),
                expires_at: at(120),
            })
            .await
            .unwrap();
        assert!(matches!(
            granted,
            wonder_store::ComputerLeaseAcquireResult::Granted(_)
        ));
        format!("session-{id}")
    }

    async fn give_back(fx: &Fx, session: &str) {
        let lease = fx
            .state
            .store
            .active_computer_control_lease_for_session(session, &now_text())
            .await
            .unwrap()
            .unwrap();
        let reply = computer_sessions::release_control(
            axum::extract::State(fx.state.clone()),
            Some(axum::Extension(crate::AuthenticatedDevice {
                device_id: lease.owner_device_id.clone(),
                session_binding: "fixture".into(),
            })),
            axum::extract::Path(session.to_owned()),
            axum::Json(
                serde_json::from_value(json!({"leaseId":lease.id,
                "generation":lease.session_generation,"geometryRevision":lease.geometry_revision,
                "sourceId":lease.source_id}))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(reply.status(), axum::http::StatusCode::OK);
    }

    fn open_takeover_states(state: &AppState) -> Vec<&'static str> {
        state
            .computer_supervisor
            .open_takeovers()
            .into_iter()
            .map(|(_, request)| request.state.as_str())
            .collect()
    }

    async fn until_takeover(state: &AppState, wanted: &[&str]) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while open_takeover_states(state) != wanted {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("takeover never reached {wanted:?}"));
    }

    // Contract: the request waits through active control, then finishes with
    // returned on explicit hand-back or expired on lease loss, never a false return.
    #[tokio::test]
    async fn takeover_wait_finishes_on_handback_or_lost_control() {
        for outcome in ["returned", "lost", "expired"] {
            let lose_control = outcome != "returned";
            let fx = fx().await;
            let state = fx.state.clone();
            let parent = Caller {
                runtime: fx.parent.runtime.clone(),
                message: fx.parent.message.clone(),
                conversation: fx.parent.conversation.clone(),
                project: fx.parent.project.clone(),
            };
            // Keep the request running while the simulated phone hands back
            // control. Pausing this future in select can leave takeover_state
            // holding the session lock across a store await, so give_back then
            // waits for a future that the test will never poll again.
            let mut asking = tokio::spawn(async move {
                execute(&state, &parent, TAKEOVER, json!({"reason":"Sign in to GitHub","app":"Google Chrome","clientRequestId":fresh(),"timeoutSeconds":60})).await
            });
            tokio::select! {
                _ = &mut asking => panic!("returned before the owner acted"),
                _ = until_takeover(&fx.state, &["waiting"]) => {}
            }
            let (id, request) = fx.state.computer_supervisor.open_takeovers().remove(0);
            assert_eq!(request.conversation_id, "project-chat");
            assert_eq!(request.app.as_deref(), Some("Google Chrome"));
            // The phone's alert names the agent and this computer.
            let host = crate::host_name::current(&fx.state).await;
            let key = format!("{}{id}", computer_sessions::TAKEOVER_PUSH_PREFIX);
            assert_eq!(
                computer_sessions::takeover_push_preview(&fx.state, &key).await,
                Some((
                    format!("Codex needs you on {host}"),
                    "Sign in to GitHub. Open Cupola to take control.".into()
                ))
            );
            let session = hold_control(&fx, "chrome", "window:42").await;
            // The acquisition owner reports native activation for this card/lease;
            // merely observing a durable reservation must never accept it.
            let stored = fx
                .state
                .store
                .computer_session(&session)
                .await
                .unwrap()
                .unwrap();
            let lease = fx
                .state
                .store
                .computer_control_lease("lease-chrome")
                .await
                .unwrap()
                .unwrap();
            fx.state
                .computer_supervisor
                .activate_takeover(&id, &stored, &lease, Utc::now());
            tokio::select! {
                _ = &mut asking => panic!("returned while the owner still has control"),
                _ = until_takeover(&fx.state, &["in_control"]) => {}
            }
            if outcome == "expired" {
                let now = Utc::now();
                fx.state
                    .store
                    .heartbeat_computer_control_lease(
                        &lease.id,
                        &lease.owner_device_id,
                        &lease.host_installation_id,
                        &lease.session_id,
                        &lease.conversation_id,
                        lease.session_generation,
                        lease.source_id.as_deref(),
                        lease.geometry_revision,
                        &now.to_rfc3339_opts(SecondsFormat::Millis, true),
                        &(now - chrono::Duration::seconds(1))
                            .to_rfc3339_opts(SecondsFormat::Millis, true),
                    )
                    .await
                    .unwrap()
                    .unwrap();
            } else if lose_control {
                fx.state
                    .store
                    .release_computer_control_lease_for_session(
                        &session,
                        &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    )
                    .await
                    .unwrap();
            } else {
                give_back(&fx, &session).await;
            }
            if lose_control {
                assert!(!computer_sessions::owner_has_control(&fx.state)
                    .await
                    .unwrap());
            }
            let result = tokio::time::timeout(Duration::from_secs(30), asking)
                .await
                .unwrap()
                .expect("takeover request task panicked")
                .map_err(|e| e.code)
                .unwrap();
            assert_eq!(
                result["status"],
                if lose_control { "expired" } else { "returned" }
            );
            // Codex's own computer use is not Cupola's to pause; the result says so.
            assert_eq!(result["computerUseFenced"], false);
            assert!(open_takeover_states(&fx.state).is_empty());
            // An answered request never alerts.
            assert_eq!(
                computer_sessions::takeover_push_preview(&fx.state, &key).await,
                None
            );
        }
    }

    // Contract: Not now answers declined; a retry of the same request gets
    // the same answer instead of asking the owner again.
    #[tokio::test]
    async fn declined_takeover_is_answered_once_and_retries_reuse_it() {
        let fx = fx().await;
        add_thread(
            &fx,
            "claude-peer",
            "Claude peer",
            AgentFamily::Claude,
            "workspace",
            "now",
        )
        .await;
        let message = queue(&fx, "claude-peer", "running fixture").await;
        fx.state
            .store
            .update_message_delivery(
                &message.id,
                "streaming",
                Some("claude-thread"),
                Some("claude-turn"),
            )
            .await
            .unwrap();
        let peer = caller_of(&fx.state, &message.id).await;
        let request = fresh();
        let args =
            json!({"reason":"Approve the passkey","clientRequestId":request,"timeoutSeconds":60});
        let state = fx.state.clone();
        let asking = {
            let args = args.clone();
            let peer = Caller {
                runtime: peer.runtime.clone(),
                message: peer.message.clone(),
                conversation: peer.conversation.clone(),
                project: peer.project.clone(),
            };
            tokio::spawn(async move {
                execute(&state, &peer, TAKEOVER, args)
                    .await
                    .map_err(|e| e.code)
            })
        };
        until_takeover(&fx.state, &["waiting"]).await;
        let (id, request) = fx.state.computer_supervisor.open_takeovers().remove(0);
        assert_eq!(request.agent, "Claude");
        // The phone lists it and declines it through the HTTP routes.
        let phone = || {
            Some(axum::Extension(crate::AuthenticatedDevice {
                device_id: "phone".into(),
                session_binding: "fixture".into(),
            }))
        };
        let body = |response: axum::response::Response| async move {
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };
        let listed = body(
            computer_sessions::takeover_requests(axum::extract::State(fx.state.clone()), phone())
                .await,
        )
        .await;
        assert_eq!(listed["requests"][0]["id"], json!(id));
        assert_eq!(listed["requests"][0]["reason"], "Approve the passkey");
        crate::tests::validate_http_contract("computerTakeoverRequest", &listed["requests"][0]);
        let decline = |who| {
            computer_sessions::decline_takeover(
                axum::extract::State(fx.state.clone()),
                who,
                axum::extract::Path(id.clone()),
            )
        };
        assert_eq!(decline(None).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(decline(phone()).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            decline(phone()).await.status(),
            StatusCode::NO_CONTENT,
            "repeated"
        );
        assert!(!fx
            .state
            .computer_supervisor
            .decline_takeover(&id, Utc::now()));
        let result = asking.await.unwrap().unwrap();
        assert_eq!(result["status"], "declined");
        // Claude's computer use runs through Cupola, which pauses it.
        assert!(result.get("computerUseFenced").is_none());
        assert_eq!(result["requestId"], json!(id));
        let again = execute(&fx.state, &peer, TAKEOVER, args).await.unwrap();
        assert_eq!(again["status"], "declined");
        assert!(open_takeover_states(&fx.state).is_empty());
    }

    // Contract: stopping a reply cancels its pending request. Retrying that
    // id returns the same cancellation and must never describe an expiry.
    #[tokio::test]
    async fn takeover_cancelled_wait_retry_preserves_the_outcome() {
        let fx = fx().await;
        let args = json!({"reason":"Sign in", "clientRequestId":fresh(), "timeoutSeconds":60});
        let asking = execute(&fx.state, &fx.parent, TAKEOVER, args.clone());
        tokio::pin!(asking);
        tokio::select! {
            result = &mut asking => panic!("request returned early: {result:?}"),
            _ = until_takeover(&fx.state, &["waiting"]) => {}
        }
        fx.state
            .store
            .update_message_delivery(&fx.parent.message.id, "interrupted", None, None)
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(30), &mut asking)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "cancelled");
        let again = execute(&fx.state, &fx.parent, TAKEOVER, args)
            .await
            .unwrap();
        assert_eq!(again["status"], "cancelled");
        assert_eq!(
            again["note"],
            "The reply that requested control ended, so this request was cancelled."
        );
        assert!(open_takeover_states(&fx.state).is_empty());
    }

    // Contract: an unanswered request expires at the agent's timeout, and
    // control the owner still holds then is reported.
    #[tokio::test]
    async fn takeover_expires_and_reports_control_still_held() {
        let fx = fx().await;
        let expired = execute(
            &fx.state,
            &fx.parent,
            TAKEOVER,
            json!({"reason":"Sign in","clientRequestId":fresh(),"timeoutSeconds":1}),
        )
        .await
        .unwrap();
        assert_eq!(expired["status"], "expired");
        assert!(expired.get("ownerHasControl").is_none());
        hold_control(&fx, "held", "display:1").await;
        let held = execute(
            &fx.state,
            &fx.parent,
            TAKEOVER,
            json!({"reason":"Sign in","clientRequestId":fresh(),"timeoutSeconds":1}),
        )
        .await
        .unwrap();
        assert_eq!(held["status"], "expired");
        assert_eq!(held["ownerHasControl"], true);
        for args in [
            json!({"clientRequestId":fresh()}),
            json!({"reason":"x","clientRequestId":"not-a-uuid"}),
            json!({"reason":"x","clientRequestId":fresh(),"timeoutSeconds":1801}),
            json!({"reason":"x","clientRequestId":fresh(),"app":""}),
            json!({"reason":"x","clientRequestId":fresh(),"window":"Chrome"}),
        ] {
            assert_eq!(
                run(&fx, TAKEOVER, args.clone()).await.unwrap_err(),
                "invalid",
                "{args}"
            );
        }
    }

    // Contract: the bridges' check answers whether the owner holds control of
    // the screen or a window; simulator control leaves the computer alone.
    // It is not a counted or saved tool call.
    #[tokio::test]
    async fn control_check_reports_phone_control_of_the_computer_only() {
        let fx = fx().await;
        let rpc = crate::projects::codex_rpc(&fx.state).await.unwrap();
        let runtime = rpc.health().id().to_owned();
        let message = Some(&fx.parent.message);
        let check = |call: &str| json!({"threadId":"thread","turnId":"turn","callId":call,"tool":CONTROL_CHECK,"arguments":{}});
        let answer = |value: Value| -> Value {
            assert_eq!(value["success"], true, "{value}");
            serde_json::from_str(value["contentItems"][0]["text"].as_str().unwrap()).unwrap()
        };
        let held = |n: &str| {
            let (state, runtime, check) = (&fx.state, &runtime, check(n));
            async move {
                answer(handle_call(state, runtime, &check, message).await)["ownerHasControl"]
                    .clone()
            }
        };
        assert_eq!(held("c1").await, false);
        let simulator = hold_control(&fx, "sim", "simulator:ABC").await;
        assert_eq!(held("c2").await, false);
        give_back(&fx, &simulator).await;
        let window = hold_control(&fx, "chrome", "window:42").await;
        // The same call id may be asked again; nothing was saved for it.
        assert_eq!(held("c1").await, true);
        give_back(&fx, &window).await;
        assert_eq!(held("c1").await, false);
        assert!(registered(CONTROL_CHECK));
        assert!(!specs().iter().any(|spec| spec["name"] == CONTROL_CHECK));
    }
}
