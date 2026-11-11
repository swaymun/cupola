//! Close requests at the provider's terminal notification, before message
//! mapping or an update handoff can defer the rest of that notification.
use crate::{AppState, EventContext};
use serde_json::Value;
use wonder_api::WonderEvent;

pub(crate) fn is_request(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
            | "item/tool/call"
    )
}

/// False means a delayed request belongs to a turn already known to be over.
/// Missing correlation cannot authorize retiring another turn's requests.
pub(crate) async fn observe(
    state: &AppState,
    method: &str,
    params: &Value,
) -> Result<bool, sqlx::Error> {
    if method != "turn/completed" && !is_request(method) {
        return Ok(true);
    }
    let (Some(runtime), Some(thread), Some(turn)) = (
        params.get("_wonderRuntimeId").and_then(Value::as_str),
        crate::notification_thread_id(params),
        crate::notification_turn_id(params),
    ) else {
        return Ok(true);
    };
    if runtime.is_empty() || thread.is_empty() || turn.is_empty() {
        return Ok(true);
    }
    if method != "turn/completed" {
        return state
            .store
            .approval_turn_ended(runtime, &thread, &turn)
            .await
            .map(|ended| !ended);
    }
    // Signed answers and question expiry hold the same lock through forwarding.
    // They never acquire dispatch_lock, which the notification owner holds.
    let _guard = state.approval_lock.lock().await;
    let template = crate::event_envelope(
        state,
        WonderEvent::ApprovalResolved {
            request_id: String::new(),
            decision: "turn_ended".into(),
        },
        EventContext {
            conversation_id: state
                .store
                .approval_push_conversation(&thread, &turn)
                .await?,
            thread_id: Some(thread.clone()),
            turn_id: Some(turn.clone()),
            ..Default::default()
        },
    );
    loop {
        let events = state
            .store
            .retire_turn_approvals(runtime, &thread, &turn, &template)
            .await?;
        if events.is_empty() {
            break;
        }
        for event in events {
            let _ = state.events.send(event);
        }
    }
    Ok(true)
}

/// The provider's answer that declines a request without owner input.
fn decline_response(method: &str) -> Value {
    match method {
        "item/tool/requestUserInput" => serde_json::json!({"answers":{}}),
        "mcpServer/elicitation/request" => serde_json::json!({"action":"decline"}),
        "item/permissions/requestApproval" => serde_json::json!({"permissions":{},"scope":"turn"}),
        "item/tool/call" => serde_json::json!({"success":false,"contentItems":[
            {"type":"inputText","text":"This request arrived after its reply ended and was not run."}]}),
        _ => serde_json::json!({"decision":"decline"}),
    }
}

/// A request that names an already-ended turn (a background task waking
/// Claude, a spared agent) has no card to wait on. Decline it so the runtime
/// moves on instead of waiting forever for an answer nobody can give.
pub(crate) async fn decline_late_request(state: &AppState, method: &str, params: &Value) {
    let Some(id) = params.get("_wonderRequestId").cloned() else {
        return;
    };
    let Some(client) = state.ingestion.approval_client(params) else {
        return;
    };
    let runtime = client.lock().await.rpc();
    if runtime.health().is_alive() {
        let _ = runtime
            .respond_value(id, Some(decline_response(method)), None)
            .await;
    }
}
