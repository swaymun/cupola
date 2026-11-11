//! `wonder_notify`: an agent pings the owner's phone mid-task ("the digest is
//! ready", "please look at this file") without ending its turn. It goes
//! through the same push intents, outbox and encrypted preview as approvals
//! and finished replies (`Store::enqueue_agent_notification`), so the owner's
//! phone registration and the open-chat quiet rule apply unchanged. A tap
//! only opens this chat or one of the Project's files; it never approves
//! anything or runs a command.
use super::*;
use wonder_store::{
    AgentNotification, AgentNotificationOutcome, PushOpen, AGENT_NOTIFY_LIMIT,
    AGENT_NOTIFY_WINDOW_SECONDS,
};

pub(in crate::thread_tools) const NOTIFY: &str = "wonder_notify";

const TITLE_CHARS: usize = 60;
const BODY_CHARS: usize = 180;
const DEDUPE_CHARS: usize = 80;

pub(in crate::thread_tools) fn spec() -> Value {
    let request_id = json!({"type":"string","format":"uuid","description":"A new UUID for this notification. Reuse it when you retry; never reuse it for a different one."});
    let open = json!({"type":"object","additionalProperties":false,
        "description":"What a tap opens: this chat (default) or a file of this Project by its fileId.",
        "properties":{"kind":{"type":"string","enum":["thread","file"]},"fileId":{"type":"string"}},
        "required":["kind"]});
    json!({"type":"function","name":NOTIFY,
        "description":format!("Send the owner a notification on their phone now, without ending your turn: for example a digest of long work, or that something is ready to look at. Use it sparingly, for things worth interrupting them for; your final reply already notifies them. A tap opens this chat or the file you name. Each chat may send {AGENT_NOTIFY_LIMIT} every {} minutes; a notification with the same dedupeKey as one still waiting to be sent updates it instead of sending another; once that one has been sent, the same key sends a new notification. Returns state queued (on its way, not confirmed delivered), coalesced or rate_limited.", AGENT_NOTIFY_WINDOW_SECONDS / 60),
        "inputSchema":{"type":"object","additionalProperties":false,
            "properties":{
                "title":{"type":"string","maxLength":TITLE_CHARS,"description":"A few words, e.g. Tests pass."},
                "body":{"type":"string","maxLength":BODY_CHARS,"description":"One or two plain sentences."},
                "open":open,
                "dedupeKey":{"type":"string","maxLength":DEDUPE_CHARS,"description":"The same key for updates about the same thing, e.g. ci-status."},
                "clientRequestId":request_id},
            "required":["title","body","clientRequestId"]}})
}

/// Bounded in characters the phone shows, not bytes.
fn short_text(args: &Value, name: &str, max: usize) -> Result<String, ToolError> {
    let value = text_arg(args, name, max * 4)?;
    if value.chars().count() > max {
        return Err(ToolError::new(
            "invalid",
            format!("{name} is longer than {max} characters."),
        ));
    }
    Ok(value)
}

async fn open_target(
    state: &AppState,
    caller: &Caller,
    args: &Value,
) -> Result<PushOpen, ToolError> {
    let invalid = |message: &str| ToolError::new("invalid", message);
    let Some(open) = args.get("open").filter(|open| !open.is_null()) else {
        return Ok(PushOpen::Thread);
    };
    let open = open
        .as_object()
        .ok_or_else(|| invalid("open must be an object such as {\"kind\":\"thread\"}."))?;
    match open.get("kind").and_then(Value::as_str) {
        Some("thread") if open.len() == 1 => Ok(PushOpen::Thread),
        Some("file") if open.len() == 2 => {
            let file_id = open
                .get("fileId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .ok_or_else(|| invalid("open.fileId is required for kind file."))?;
            // Another Project's file is reported exactly like a missing one.
            let conversation_id = state
                .store
                .agent_notification_file(&caller.conversation.conversation_id, file_id)
                .await?
                .ok_or_else(|| ToolError::new("not_found", "No such file in this Project."))?;
            Ok(PushOpen::File {
                file_id: file_id.to_owned(),
                conversation_id,
            })
        }
        Some("thread") => Err(invalid("open kind thread takes no other field.")),
        Some("file") => Err(invalid("open kind file takes only fileId.")),
        _ => Err(invalid("open.kind must be thread or file.")),
    }
}

pub(in crate::thread_tools) async fn notify(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &["title", "body", "open", "dedupeKey", "clientRequestId"],
    )?;
    let request = request_id(&args)?;
    let title = short_text(&args, "title", TITLE_CHARS)?;
    let body = short_text(&args, "body", BODY_CHARS)?;
    // Without a key, each notification stands alone.
    let dedupe_key = match args.get("dedupeKey") {
        None | Some(Value::Null) => request.clone(),
        Some(_) => format!("key:{}", short_text(&args, "dedupeKey", DEDUPE_CHARS)?),
    };
    let open = open_target(state, caller, &args).await?;
    let outcome = state
        .store
        .enqueue_agent_notification(AgentNotification {
            conversation_id: &caller.conversation.conversation_id,
            client_request_id: &request,
            dedupe_key: &dedupe_key,
            title: &title,
            body: &body,
            open,
        })
        .await?;
    let minutes = AGENT_NOTIFY_WINDOW_SECONDS / 60;
    let (state, message) = match outcome {
        AgentNotificationOutcome::Queued => (
            "queued",
            "Cupola is sending the notification to the owner's phone. It is not confirmed as delivered.".to_owned(),
        ),
        AgentNotificationOutcome::Coalesced => (
            "coalesced",
            "A notification with this dedupeKey was still waiting to be sent; it now has this text and no second one was sent.".to_owned(),
        ),
        AgentNotificationOutcome::RateLimited => (
            "rate_limited",
            format!("Not sent: this chat already sent {AGENT_NOTIFY_LIMIT} notifications in the last {minutes} minutes. Put it in your reply, or try again later."),
        ),
        AgentNotificationOutcome::NoPhone => {
            return Err(ToolError::new(
                "no_phone",
                "Not sent: no phone has Cupola notifications turned on. Put it in your reply instead.",
            ))
        }
    };
    Ok(json!({"state": state, "message": message}))
}
