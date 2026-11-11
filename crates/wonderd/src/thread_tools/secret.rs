//! Secret requests use Cupola's signed phone approvals for both providers.
//! Only opaque references enter the tool result; the vault owns all values.
use super::*;

pub(in crate::thread_tools) const NAME: &str = "wonder_request_secret";
pub(in crate::thread_tools) const RUN: &str = "wonder_run_with_secret";
pub(crate) const METHOD: &str = "wonder/requestSecret";
const PREFIX: &str = "secret:";
const VALUE_BYTES: usize = 16 * 1024;
const SAVED: &str = "saved";
const CANCELLED: &str = "cancelled";
const EXPIRED: &str = "expired";

pub(in crate::thread_tools) fn spec() -> Value {
    json!({"type":"function","name":NAME,"description":"Ask the owner for a secret by name and explain why you need it. Their phone collects the value privately after device authentication and saves it in this Project's Keychain vault on the host computer. Returns an opaque secretRef, never the value. Reuse the ref with wonder_run_with_secret in later turns of this chat. An existing grant to this chat returns immediately. Never ask for values in chat or read the Keychain directly. Waits up to timeoutSeconds (default 600, maximum 1800). Reuse clientRequestId on retries.","inputSchema":{
        "type":"object","additionalProperties":false,"required":["name","purpose"],"properties":{
            "name":{"type":"string","maxLength":64,"description":"Stable secret name, such as DEPLOY_TOKEN. Letters, digits, dots, dashes and underscores only."},
            "purpose":{"type":"string","maxLength":500},
            "timeoutSeconds":{"type":"integer","minimum":1,"maximum":MAX_WAIT},
            "clientRequestId":{"type":"string","format":"uuid"}
        }
    }})
}
pub(in crate::thread_tools) fn run_spec() -> Value {
    json!({"type":"function","name":RUN,"description":"Run a bounded shell command in this chat's folder using a saved secretRef. Requires the chat's existing Full access grant; unavailable in read-only/workspace modes. delivery file sets the named environment variable to a temporary 0600 file path; delivery env sets it to the value. The host removes the file and ends the process group on completion, error, timeout or cancellation, and redacts output before returning it. Do not echo, copy, encode or persist secrets. Use file delivery where possible. The ref is valid only for this Project and an owner-approved chat.","inputSchema":{
        "type":"object","additionalProperties":false,"required":["secretRef","command","delivery","variable"],"properties":{
            "secretRef":{"type":"string"},"command":{"type":"string","maxLength":16384},
            "delivery":{"type":"string","enum":["file","env"]},"variable":{"type":"string","maxLength":64},
            "timeoutSeconds":{"type":"integer","minimum":1,"maximum":120}
        }
    }})
}
pub(in crate::thread_tools) async fn run(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    forbid_plan_mode_with(
        caller,
        "This thread is planning. Finish the plan before running commands with a secret.",
    )?;
    only_known(
        &args,
        &[
            "secretRef",
            "command",
            "delivery",
            "variable",
            "timeoutSeconds",
        ],
    )?;
    if caller.conversation.access_mode != "full_access" {
        return Err(ToolError::new(
            "permission_required",
            "Running a command with a secret requires this chat's Full access permission.",
        ));
    }
    let reference = text_arg(&args, "secretRef", 100)?;
    let delivery = text_arg(&args, "delivery", 10)?;
    let variable = text_arg(&args, "variable", 64)?;
    let command = text_arg(&args, "command", 16384)?;
    let seconds = int_arg(&args, "timeoutSeconds", 30, 1, 120)?;
    tokio::select! {
        result = crate::secrets::run(state, &caller.project.id, &caller.conversation.conversation_id,
            crate::secrets::CommandRequest { reference: &reference, delivery: &delivery, variable: &variable, command: &command, seconds }) => result.map_err(|why| ToolError::new("secret_unavailable", why)),
        _ = async {
            loop {
                tokio::time::sleep(POLL).await;
                if !state.store.message_by_id(&caller.message.id).await.ok().flatten()
                    .is_some_and(|m| matches!(m.state.as_str(), "accepted_by_codex" | "streaming")) { break; }
            }
        } => Err(ToolError::new("cancelled", "The secret command stopped with this turn.")),
    }
}

pub(crate) fn is_request(server_request_id: &str) -> bool {
    server_request_id.starts_with(PREFIX)
}

/// What the phone shows for the request: no field can carry the value.
pub(crate) fn summary(params: &Value) -> Option<Value> {
    Some(json!({
        "label": params["label"].as_str()?,
        "purpose": params["purpose"].as_str()?,
        "destination": "in this Project’s Keychain vault on your computer",
        "expiresAtMs": params["expiresAtMs"].as_u64()?,
    }))
}

async fn publish(state: &AppState, conversation: &str, key: &str, event: WonderEvent) {
    let _ = publish_event_with_context(
        state,
        event,
        EventContext {
            request_id: Some(key.to_owned()),
            conversation_id: Some(conversation.to_owned()),
            approval_id: Some(key.to_owned()),
            ..EventContext::default()
        },
    )
    .await;
}

fn outcome(decision: Option<&str>) -> &'static str {
    match decision {
        Some(SAVED) => SAVED,
        Some(CANCELLED) => CANCELLED,
        _ => EXPIRED,
    }
}

pub(in crate::thread_tools) async fn request(
    state: &AppState,
    caller: &Caller,
    call_id: &str,
    args: Value,
) -> Result<Value, ToolError> {
    forbid_plan_mode_with(
        caller,
        "This thread is planning. Finish the plan before asking for a secret.",
    )?;
    only_known(
        &args,
        &["name", "purpose", "timeoutSeconds", "clientRequestId"],
    )?;
    if !cfg!(target_os = "macos") {
        return Err(ToolError::new(
            "unavailable",
            "Secrets need a computer with a login Keychain.",
        ));
    }
    let client_request = if args.get("clientRequestId").is_some() {
        request_id(&args)?
    } else {
        derived_uuid(&["secret-call", call_id])
    };
    let name = text_arg(&args, "name", 64)?;
    if !crate::secrets::valid_name(&name) {
        return Err(ToolError::new(
            "invalid",
            "Use a secret name with letters, digits, dots, dashes or underscores.",
        ));
    }
    let label = &name;
    let purpose = text_arg(&args, "purpose", 500)?;
    let timeout = int_arg(&args, "timeoutSeconds", DEFAULT_WAIT, 1, MAX_WAIT)?;
    let (Some(runtime), Some(thread), Some(turn)) = (
        caller.runtime.as_deref(),
        caller.message.codex_thread_id.as_deref(),
        caller.message.codex_turn_id.as_deref(),
    ) else {
        return Err(ToolError::new(
            "wrong_turn",
            "The request targets different work.",
        ));
    };
    let id = derived_uuid(&["secret", &caller.message.id, &client_request]);
    let key = format!("{PREFIX}{id}");
    let fingerprint = hex::encode(Sha256::digest(json!([name, purpose]).to_string()));
    let conversation = caller.conversation.conversation_id.clone();
    if let Some(info) = crate::secrets::existing(state, &caller.project.id, &name, &conversation)
        .await
        .map_err(|why| ToolError::new("unavailable", why))?
    {
        return Ok(json!({"state":SAVED,"secretRef":info.secret_ref,"name":name}));
    }
    match state.store.approval(&key).await? {
        Some(existing) => {
            let params: Value = serde_json::from_str(&existing.params_json).unwrap_or_default();
            if params["_wonderSecret"]["fingerprint"] != json!(fingerprint) {
                return Err(ToolError::new(
                    "call_reused",
                    "This clientRequestId was already used for a different request.",
                ));
            }
        }
        None => {
            let expires = now_ms().saturating_add(timeout * 1000);
            let params = json!({
                // The asking runtime and turn: the request closes with them
                // (`retire_lost_approvals`, `retire_turn_approvals`).
                "_wonderRuntimeId": runtime,
                "threadId": thread, "turnId": turn, "itemId": call_id,
                "label": label, "purpose": purpose, "reason": purpose,
                "name": name, "expiresAtMs": expires,
                "_wonderSecret": {"conversationId": conversation, "projectId": caller.project.id, "fingerprint": fingerprint},
            });
            state
                .store
                .insert_pending_approval(&key, METHOD, &params.to_string(), &now_ms().to_string())
                .await?;
            publish(
                state,
                &conversation,
                &key,
                WonderEvent::ApprovalOpened {
                    request_id: key.clone(),
                },
            )
            .await;
        }
    }
    let state_name = wait_for_answer(state, caller, &key).await?;
    let reference = if state_name == SAVED {
        crate::secrets::existing(state, &caller.project.id, &name, &conversation)
            .await
            .map_err(|why| ToolError::new("unavailable", why))?
            .map(|info| info.secret_ref)
    } else {
        None
    };
    Ok(json!({
        "state": state_name,
        "secretRef": reference,
        "name": name,
        "note": match state_name {
            SAVED => "Saved. Use the ref with wonder_run_with_secret; never read or print the value yourself.",
            CANCELLED => "The person chose not to provide it.",
            _ => "No answer arrived in time. Ask again only if you still need it.",
        },
    }))
}

/// Waits until the phone answers, the request expires or the turn ends; an
/// open request left behind is closed as expired so the phone stops offering it.
async fn wait_for_answer(
    state: &AppState,
    caller: &Caller,
    key: &str,
) -> Result<&'static str, ToolError> {
    loop {
        let Some(approval) = state.store.approval(key).await? else {
            return Ok(EXPIRED);
        };
        if approval.state == "resolved" {
            return Ok(outcome(approval.decision.as_deref()));
        }
        let params: Value = serde_json::from_str(&approval.params_json).unwrap_or_default();
        let expired = params["expiresAtMs"]
            .as_u64()
            .is_none_or(|at| now_ms() >= at);
        let turn_active = state
            .store
            .message_by_id(&caller.message.id)
            .await?
            .is_some_and(|m| matches!(m.state.as_str(), "accepted_by_codex" | "streaming"));
        if expired || !turn_active {
            let _guard = state.approval_lock.lock().await;
            if state
                .store
                .resolve_approval_by_server_request_id(key, EXPIRED, &now_ms().to_string())
                .await?
            {
                publish(
                    state,
                    &caller.conversation.conversation_id,
                    key,
                    WonderEvent::ApprovalResolved {
                        request_id: key.to_owned(),
                        decision: EXPIRED.into(),
                    },
                )
                .await;
                return Ok(EXPIRED);
            }
            continue;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The phone's signed answer to a secret request (`save_requested_secret`,
/// or a Cancel through `resolve_approval`, has verified its signature and
/// freshness and holds the approval lock). The value goes only to the
/// destination; the request keeps no saved answer.
pub(crate) async fn answer(
    state: &AppState,
    approval: &wonder_store::StoredApproval,
    decision: &str,
    value: Option<&str>,
) -> Response {
    let id = &approval.approval_id;
    let params: Value = serde_json::from_str(&approval.params_json).unwrap_or_default();
    let conversation = params["_wonderSecret"]["conversationId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let close = |decision: &'static str, status: StatusCode, message: &'static str| {
        let conversation = conversation.clone();
        async move {
            if state
                .store
                .finish_approval_resolution(id, decision, &now_ms().to_string())
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "approval persistence failed",
                )
                    .into_response();
            }
            publish(
                state,
                &conversation,
                id,
                WonderEvent::ApprovalResolved {
                    request_id: id.clone(),
                    decision: decision.into(),
                },
            )
            .await;
            if status == StatusCode::NO_CONTENT {
                status.into_response()
            } else {
                (status, message).into_response()
            }
        }
    };
    let reopen = |status: StatusCode, message: String| async move {
        let _ = state.store.abandon_approval_resolution(id).await;
        (status, message).into_response()
    };
    if params["expiresAtMs"]
        .as_u64()
        .is_none_or(|at| now_ms() >= at)
    {
        return close(
            EXPIRED,
            StatusCode::CONFLICT,
            "This request expired. The agent can ask again.",
        )
        .await;
    }
    // The request belongs to one chat of one Project; a chat moved, archived
    // or removed since can no longer receive it.
    let owner = state
        .store
        .project_conversation(&conversation)
        .await
        .ok()
        .flatten();
    let project = match &owner {
        Some(owner)
            if !owner.wonder_archived
                && params["_wonderSecret"]["projectId"].as_str()
                    == Some(owner.project_id.as_str()) =>
        {
            state
                .store
                .project(&owner.project_id)
                .await
                .ok()
                .flatten()
                .filter(|p| p.is_included)
        }
        _ => None,
    };
    let (Some(owner), Some(_)) = (owner, project) else {
        return close(
            EXPIRED,
            StatusCode::CONFLICT,
            "This request no longer belongs to an available Project. Refresh the chat.",
        )
        .await;
    };
    match decision {
        "cancel" if value.is_none() => return close(CANCELLED, StatusCode::NO_CONTENT, "").await,
        "save" => {}
        _ => return reopen(StatusCode::BAD_REQUEST, "Choose Save or Cancel.".into()).await,
    }
    let Some(value) = value.filter(|v| !v.trim().is_empty() && v.len() <= VALUE_BYTES) else {
        return reopen(
            StatusCode::BAD_REQUEST,
            "Enter a value of at most 16 KB.".into(),
        )
        .await;
    };
    let Some(name) = params["name"]
        .as_str()
        .filter(|name| crate::secrets::valid_name(name))
    else {
        return reopen(
            StatusCode::CONFLICT,
            "This request predates the vault. Ask the agent to request the secret again.".into(),
        )
        .await;
    };
    let saved = crate::secrets::save(
        state,
        &owner.project_id,
        name,
        &conversation,
        owner.family.as_str(),
        value,
    )
    .await;
    match saved {
        Ok(_) => close(SAVED, StatusCode::NO_CONTENT, "").await,
        Err(why) => reopen(StatusCode::CONFLICT, why).await,
    }
}
