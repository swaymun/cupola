//! Persisted continuations for an explicit host update. Ordinary shutdown and
//! user Stop never create a continuation, and uncertain submissions never retry.
use crate::AppState;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use wonder_app_server::RpcClient;
use wonder_store::UpdateHandoff;

const CONTINUE: &str = "Continue the work paused for a Cupola update in this conversation. Read the previous tool results and check current state before proceeding. Do not repeat completed actions or replay the original request.";
const CONTINUE_AFTER_RESTART: &str = "Continue the work cut off when Cupola or the computer restarted in this conversation. Read the previous tool results and check current state before proceeding. Do not repeat completed actions or replay the original request.";
/// The `request_id` of a continuation for a reply a crash or restart cut off
/// (`continue_after_restart`), rather than one an update paused.
pub(crate) const RESTART_REQUEST: &str = "restart";
/// Continuations one reply gets after restarts, so a reply that keeps
/// crashing its runtime settles as failed.
const MAX_RESTART_CONTINUATIONS: i64 = 2;

fn after_restart(handoff: &UpdateHandoff) -> bool {
    handoff.request_id == RESTART_REQUEST
}

/// "Continue threads after restarts": a reply whose runtime ended mid-turn
/// (a crash, reboot or forced restart, not the owner's Stop) gets the same
/// continuation an update gives it, when the owner has the setting on and
/// the turn's settings were saved when it started. Returns whether the
/// continuation was recorded; `recover` then sends it, with every check an
/// update continuation gets.
pub(crate) async fn continue_after_restart(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    thread: &str,
    turn: &str,
) -> bool {
    if !matches!(message.state.as_str(), "accepted_by_codex" | "streaming")
        || !state
            .store
            .continuation_settings()
            .await
            .is_ok_and(|settings| settings.continue_after_restart)
        || !state
            .store
            .message_continuations(&message.id, RESTART_REQUEST)
            .await
            .is_ok_and(|count| count < MAX_RESTART_CONTINUATIONS)
    {
        return false;
    }
    let Ok(Some((resume_params, turn_params))) = state.store.update_turn_settings(thread).await
    else {
        return false;
    };
    let handoff = UpdateHandoff {
        thread_id: thread.into(),
        stopped_turn_id: turn.into(),
        request_id: RESTART_REQUEST.into(),
        conversation_id: message.conversation_id.clone(),
        message_id: Some(message.id.clone()),
        resume_client_id: uuid::Uuid::new_v4().to_string(),
        resume_params,
        turn_params,
        state: "paused".into(),
        resumed_turn_id: None,
    };
    if state.store.save_update_handoff(&handoff).await.is_err() {
        return false;
    }
    // A handoff for this turn that already existed (cancelled or finished)
    // is never revived.
    let recorded = state
        .store
        .pending_update_handoffs()
        .await
        .is_ok_and(|pending| {
            pending
                .iter()
                .any(|h| h.thread_id == thread && h.stopped_turn_id == turn && after_restart(h))
        });
    if recorded {
        let _ = state.logger.record(
            "info",
            "restart_continuation_scheduled",
            json!({"threadId": thread, "turnId": turn}),
        );
    }
    recorded
}

/// Whether a paused Claude turn's frozen policy grants the same access as the
/// policy Wonder builds now from the conversation's current choices. Only what
/// the owner chose is compared; fields a newer Wonder adds or drops must not
/// strand paused work, and the continuation then runs with the fresh policy.
/// A protected folder added by the update narrows access and is accepted.
pub(crate) fn same_claude_access(frozen: &Value, fresh: &Value) -> bool {
    fn roots<'a>(policy: &'a Value, key: &str) -> Option<BTreeSet<&'a str>> {
        policy[key].as_array()?.iter().map(Value::as_str).collect()
    }
    let plan = |policy: &Value| policy["planMode"].as_bool().unwrap_or(false);
    ["mode", "approvalMode", "workspace"]
        .iter()
        .all(|key| frozen[*key].is_string() && frozen[*key] == fresh[*key])
        && plan(frozen) == plan(fresh)
        && ["readRoots", "writeRoots"]
            .iter()
            .all(|key| roots(frozen, key).is_some() && roots(frozen, key) == roots(fresh, key))
        && matches!((roots(frozen, "deniedRoots"), roots(fresh, "deniedRoots")),
            (Some(frozen), Some(fresh)) if frozen.is_subset(&fresh))
}

/// Why a paused turn was not continued. A blocked continuation cannot succeed
/// without the owner, so its response is settled instead of left running.
enum Failure {
    Blocked(String),
    Retry(String),
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Self::Retry(error)
    }
}

impl From<&str> for Failure {
    fn from(error: &str) -> Self {
        Self::Retry(error.to_owned())
    }
}

pub(crate) async fn remember_settings(
    state: &AppState,
    thread: &str,
    resume: &Value,
    turn: &Value,
) -> Result<(), String> {
    let mut policy = turn.clone();
    let object = policy.as_object_mut().ok_or("Invalid turn settings")?;
    // The update adds a continuation; storing/replaying the original input
    // would duplicate actions and attachments.
    object.remove("input");
    object.remove("clientUserMessageId");
    state
        .store
        .save_update_turn_settings(thread, &resume.to_string(), &policy.to_string())
        .await
        .map_err(|e| e.to_string())
}

async fn result(rpc: &RpcClient, method: &str, params: Value) -> Result<Value, String> {
    let reply = rpc
        .request(method, params)
        .await
        .map_err(|e| e.to_string())?;
    if reply.error.is_some() {
        return Err(format!("Update handoff: {method} was rejected"));
    }
    reply
        .result
        .ok_or_else(|| format!("Update handoff: {method} returned no result"))
}

// Exact native history proves both interruption and an already accepted
// continuation. A missing echo after an ambiguous submit is never a retry.
async fn history(
    rpc: &RpcClient,
    thread: &str,
    turn: &str,
    client: &str,
) -> Result<(Option<String>, Option<String>), String> {
    let mut status = None;
    let mut cursor = None;
    let mut seen = HashSet::new();
    for _ in 0..200 {
        let page = result(
            rpc,
            "thread/turns/list",
            json!({"threadId":thread,"limit":100,
            "sortDirection":"desc","itemsView":"notLoaded","cursor":cursor}),
        )
        .await?;
        for entry in page["data"]
            .as_array()
            .ok_or("Native turn history is unavailable")?
        {
            if entry["id"].as_str() == Some(turn) {
                status = entry["status"].as_str().map(str::to_owned);
            }
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        match &cursor {
            None => {
                if client.is_empty() {
                    return Ok((status, None));
                }
                return Ok((status, accepted_continuation(rpc, thread, client).await?));
            }
            Some(next) if !seen.insert(next.clone()) => break,
            _ => {}
        }
    }
    Err("Native turn history is incomplete".into())
}

async fn accepted_continuation(
    rpc: &RpcClient,
    thread: &str,
    client: &str,
) -> Result<Option<String>, String> {
    let mut cursor = None;
    let mut seen = HashSet::new();
    for _ in 0..200 {
        let page = result(
            rpc,
            "thread/items/list",
            json!({"threadId":thread,"limit":100,"sortDirection":"desc","cursor":cursor}),
        )
        .await?;
        if let Some(turn) = crate::find_client_message(&page, client) {
            return Ok(Some(turn));
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        match &cursor {
            None => return Ok(None),
            Some(next) if !seen.insert(next.clone()) => break,
            _ => {}
        }
    }
    Err("Native item history is incomplete".into())
}

pub(crate) async fn pause(state: &AppState, request: &str) -> Result<(), String> {
    if state
        .store
        .has_unsafe_update_work(&state.host_installation_id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err(
            "Finish computer sharing or wait for work to be accepted before updating.".into(),
        );
    }
    let mut handoffs = Vec::new();
    let mut roots = Vec::new();
    let mut tracked = HashSet::new();
    for message in state
        .store
        .active_runtime_messages()
        .await
        .map_err(|e| e.to_string())?
    {
        if !matches!(message.state.as_str(), "accepted_by_codex" | "streaming") {
            continue;
        }
        let (Some(thread), Some(turn)) = (&message.codex_thread_id, &message.codex_turn_id) else {
            return Err("A running turn has no native receipt yet".into());
        };
        let rpc = crate::claude::for_thread(state, thread).await?;
        let (status, _) = history(&rpc, thread, turn, "").await?;
        if status.as_deref() != Some("inProgress") {
            continue;
        }
        let (resume, params) = state.store.update_turn_settings(thread).await.map_err(|e| e.to_string())?
            .ok_or("This turn started before update recovery was installed. Let it finish before updating.")?;
        tracked.insert(thread.clone());
        roots.push((message.conversation_id.clone(), thread.clone()));
        handoffs.push((
            UpdateHandoff {
                thread_id: thread.clone(),
                stopped_turn_id: turn.clone(),
                request_id: request.into(),
                conversation_id: message.conversation_id,
                message_id: Some(message.id),
                resume_client_id: uuid::Uuid::new_v4().to_string(),
                resume_params: resume,
                turn_params: params,
                state: "paused".into(),
                resumed_turn_id: None,
            },
            rpc,
        ));
    }
    // Discover native children through the same verified ownership path as the
    // conversation UI. No unrelated threads from the owner's home are stopped.
    let mut index = 0;
    while index < roots.len() {
        if roots.len() > 256 {
            return Err("Too many active agent tasks to safely pause this update".into());
        }
        let (conversation, parent) = roots[index].clone();
        index += 1;
        for child in crate::subagents::list_runtime_threads(state, &conversation, &parent).await? {
            let thread = &child.ownership.thread_id;
            if !tracked.insert(thread.clone()) {
                continue;
            }
            roots.push((child.ownership.conversation_id.clone(), thread.clone()));
            let page = result(
                &child.rpc,
                "thread/turns/list",
                json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
            )
            .await?;
            let Some(turn) = page.pointer("/data/0") else {
                continue;
            };
            if turn["status"].as_str() != Some("inProgress") {
                continue;
            }
            if child.thread["canAcceptDirectInput"].as_bool() != Some(true) {
                return Err("An agent task cannot safely continue after an update yet. Let it finish first.".into());
            }
            handoffs.push((
                UpdateHandoff {
                    thread_id: thread.clone(),
                    stopped_turn_id: turn["id"].as_str().ok_or("Missing agent turn")?.into(),
                    request_id: request.into(),
                    conversation_id: child.ownership.conversation_id,
                    message_id: None,
                    resume_client_id: uuid::Uuid::new_v4().to_string(),
                    resume_params: crate::inherited_child_resume_params(thread).to_string(),
                    turn_params: json!({"threadId":thread}).to_string(),
                    state: "paused".into(),
                    resumed_turn_id: None,
                },
                child.rpc,
            ));
        }
    }
    // Include a previous, partially prepared roster when preparation is retried.
    for handoff in state
        .store
        .pending_update_handoffs()
        .await
        .map_err(|e| e.to_string())?
    {
        if tracked.insert(handoff.thread_id.clone()) {
            let rpc = crate::claude::for_thread(state, &handoff.thread_id).await?;
            handoffs.push((handoff, rpc));
        }
    }
    for route in state.ingestion.runtime_routes() {
        let rpc = route.rpc().await?;
        // Every runtime reports only the threads it has loaded. Claude and
        // ACP runtimes retain hundreds of idle journals that are not work.
        let loaded = result(&rpc, "thread/loaded/list", json!({})).await?;
        if !loaded["nextCursor"].is_null() {
            return Err("The loaded thread roster is incomplete".into());
        }
        let threads = loaded["data"]
            .as_array()
            .ok_or("Missing loaded threads")?
            .iter()
            .map(|v| {
                v.as_str()
                    .or_else(|| v["id"].as_str())
                    .map(str::to_owned)
                    .ok_or("Missing loaded thread identity")
            })
            .collect::<Result<Vec<_>, _>>()?;
        for thread in threads {
            // Native goal schedulers and detached terminals can keep running
            // outside Wonder's turn-admission fence. Wait for them rather
            // than stopping work that has no safe continuation contract.
            if route.family == wonder_store::AgentFamily::Codex {
                let goal = result(&rpc, "thread/goal/get", json!({"threadId":thread})).await?;
                if !goal.get("goal").is_some_and(|g| {
                    g.is_null()
                        || matches!(
                            g["status"].as_str(),
                            Some(
                                "complete"
                                    | "blocked"
                                    | "paused"
                                    | "budgetLimited"
                                    | "usageLimited"
                            )
                        )
                }) {
                    return Err("Let the active goal finish or pause it before updating.".into());
                }
                let background = result(
                    &rpc,
                    "thread/backgroundTerminals/list",
                    json!({"threadId":thread,"limit":1}),
                )
                .await?;
                if !background["data"].as_array().is_some_and(Vec::is_empty)
                    || !background["nextCursor"].is_null()
                {
                    return Err("Finish background commands before updating.".into());
                }
            }
            if tracked.contains(&thread) {
                continue;
            }
            let native = result(
                &rpc,
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            )
            .await?;
            match native
                .pointer("/thread/status/type")
                .and_then(Value::as_str)
            {
                Some("idle" | "notLoaded") => {}
                _ => {
                    return Err(
                        "Other native work is still running. Let it finish before updating.".into(),
                    )
                }
            }
        }
    }
    // Save the entire roster before interrupting anything. A crash or failed
    // installation can then recover every stopped turn on the old host too.
    for (handoff, _) in &handoffs {
        state
            .store
            .save_update_handoff(handoff)
            .await
            .map_err(|e| e.to_string())?;
    }
    for (handoff, rpc) in handoffs.iter().rev() {
        if history(rpc, &handoff.thread_id, &handoff.stopped_turn_id, "")
            .await?
            .0
            .as_deref()
            == Some("inProgress")
        {
            result(
                rpc,
                "turn/interrupt",
                json!({"threadId":handoff.thread_id,"turnId":handoff.stopped_turn_id}),
            )
            .await?;
        }
        loop {
            let (status, _) =
                history(rpc, &handoff.thread_id, &handoff.stopped_turn_id, "").await?;
            match status.as_deref() {
                Some("interrupted") => break,
                Some("completed" | "failed") => {
                    state
                        .store
                        .cancel_update_handoff(&handoff.thread_id, &handoff.stopped_turn_id)
                        .await
                        .map_err(|e| e.to_string())?;
                    break;
                }
                Some("inProgress") => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await
                }
                _ => return Err("The turn's interruption could not be confirmed".into()),
            }
        }
    }
    Ok(())
}

/// Suppress only the exact update-induced terminal event, including a late
/// event after the new turn is bound. A natural completion wins the race.
pub(crate) async fn terminal(
    state: &AppState,
    thread: &str,
    turn: &str,
    status: Option<&str>,
) -> Result<bool, String> {
    let Some(handoff) = state
        .store
        .update_handoff(thread, turn)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(false);
    };
    if matches!(handoff.state.as_str(), "cancelled" | "finished") {
        return Ok(false);
    }
    if status == Some("interrupted") {
        settle_partial_assistant(state, &handoff).await?;
        return Ok(true);
    }
    if status.is_none() {
        let rpc = crate::claude::for_thread(state, thread).await?;
        if history(&rpc, thread, turn, "").await?.0.as_deref() == Some("interrupted") {
            settle_partial_assistant(state, &handoff).await?;
            return Ok(true);
        }
    }
    state
        .store
        .cancel_update_handoff(thread, turn)
        .await
        .map_err(|e| e.to_string())?;
    Ok(false)
}

async fn settle_partial_assistant(state: &AppState, handoff: &UpdateHandoff) -> Result<(), String> {
    let now = chrono::Utc::now().to_rfc3339();
    for message in state
        .store
        .complete_assistant_messages_for_codex_thread_and_turn(
            &handoff.thread_id,
            &handoff.stopped_turn_id,
            &now,
        )
        .await
        .map_err(|e| e.to_string())?
    {
        crate::publish_event_with_context(
            state,
            wonder_api::WonderEvent::AssistantCompleted { text: message.text },
            crate::EventContext {
                conversation_id: Some(handoff.conversation_id.clone()),
                message_id: handoff.message_id.clone(),
                thread_id: Some(handoff.thread_id.clone()),
                turn_id: Some(handoff.stopped_turn_id.clone()),
                item_id: Some(message.item_id),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) async fn recover(state: &AppState) -> Result<(), String> {
    let mut dispatch = Some(state.dispatch_lock.lock().await);
    for handoff in state
        .store
        .pending_update_handoffs()
        .await
        .map_err(|e| e.to_string())?
    {
        let error = match recover_one(state, &handoff, &mut dispatch).await {
            Ok(()) => continue,
            Err(Failure::Blocked(reason)) => {
                block(state, &handoff, &reason).await?;
                continue;
            }
            Err(Failure::Retry(error)) => error,
        };
        if state
            .store
            .note_update_handoff_error(&handoff, &error)
            .await
            .map_err(|e| e.to_string())?
        {
            let _ = state.logger.record(
                "warn",
                "update_continuation_pending",
                json!({"threadId":handoff.thread_id,"error":error}),
            );
            crate::publish_event_with_context(state, wonder_api::WonderEvent::TerminalError {
            message: if after_restart(&handoff) {
                "Cupola could not safely continue work cut off by a restart. Open this conversation on your computer to check its state before continuing."
            } else {
                "Cupola could not safely continue work paused for the update. Open this conversation on your computer to check its state before continuing."
            }.into()
        }, crate::EventContext { conversation_id:Some(handoff.conversation_id.clone()),message_id:handoff.message_id.clone(),
            thread_id:Some(handoff.thread_id.clone()),turn_id:Some(handoff.stopped_turn_id.clone()), ..Default::default() }).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Ends a paused response whose continuation needs the owner: keep its partial
/// reply, mark it stopped, and say what to do. Sending a message continues.
async fn block(state: &AppState, handoff: &UpdateHandoff, reason: &str) -> Result<(), String> {
    let _ = state.logger.record(
        "warn",
        "update_continuation_blocked",
        json!({"threadId":handoff.thread_id,"error":reason}),
    );
    state
        .store
        .cancel_update_handoff(&handoff.thread_id, &handoff.stopped_turn_id)
        .await
        .map_err(|e| e.to_string())?;
    settle_partial_assistant(state, handoff).await?;
    let message = match &handoff.message_id {
        Some(id) => state
            .store
            .message_by_id(id)
            .await
            .map_err(|e| e.to_string())?,
        None => None,
    };
    if let Some(message) = message {
        if state
            .store
            .interrupt_message_if_active(&message.id)
            .await
            .map_err(|e| e.to_string())?
        {
            crate::publish_message_state(
                state,
                &message,
                crate::DeliveryState::Interrupted,
                Some(&handoff.thread_id),
                Some(&handoff.stopped_turn_id),
            )
            .await;
        }
    }
    crate::publish_event_with_context(
        state,
        wonder_api::WonderEvent::TerminalError {
            message: format!("{} {reason} Check this conversation's access, then send a message to continue.", if after_restart(handoff) {
                "Cupola restarted while this response was running and stopped it instead of continuing."
            } else {
                "Cupola updated while this response was running and stopped it instead of continuing."
            }),
        },
        crate::EventContext {
            conversation_id: Some(handoff.conversation_id.clone()),
            message_id: handoff.message_id.clone(),
            thread_id: Some(handoff.thread_id.clone()),
            turn_id: Some(handoff.stopped_turn_id.clone()),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Returns the current Claude policy the continuation must run with.
async fn validate_execution(
    state: &AppState,
    handoff: &UpdateHandoff,
) -> Result<Option<Value>, String> {
    let message_id = handoff
        .message_id
        .as_deref()
        .ok_or("Missing paused execution")?;
    let message = state
        .store
        .message_by_id(message_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Missing paused message")?;
    let conversation = state
        .store
        .owning_project_conversation(&handoff.conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("The paused Project is unavailable")?;
    if message.conversation_id != handoff.conversation_id
        || conversation.conversation_id != handoff.conversation_id
        || !state
            .store
            .verified_project_runtime_owner(&handoff.conversation_id, &handoff.thread_id)
            .await
            .map_err(|e| e.to_string())?
        || message.codex_thread_id.as_deref() != Some(&handoff.thread_id)
    {
        return Err("The paused execution no longer belongs to this Project thread".into());
    }
    crate::projects::validate_update_policy(
        state,
        &conversation,
        &serde_json::from_str(&handoff.resume_params).map_err(|e| e.to_string())?,
        &serde_json::from_str(&handoff.turn_params).map_err(|e| e.to_string())?,
    )
    .await
}

async fn validate_parent_execution(state: &AppState, conversation: &str) -> Result<(), String> {
    let owner = state
        .store
        .owning_project_conversation(conversation)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("The paused agent's Project is unavailable")?;
    let handoff = state
        .store
        .conversation_update_handoffs(&owner.conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .rev()
        .find(|h| h.message_id.is_some())
        .ok_or("The parent's paused execution is unavailable")?;
    validate_execution(state, &handoff).await.map(|_| ())
}

/// Saves the continuation choices. Turning "Continue threads after
/// restarts" off settles every restart continuation not yet sent, so its
/// reply fails as it would have with the setting off.
pub(crate) async fn set_continuation_settings(
    state: &AppState,
    resume_after_limit: Option<bool>,
    continue_after_restart: Option<bool>,
) -> Result<wonder_store::ContinuationSettings, String> {
    let settings = state
        .store
        .set_continuation_settings(resume_after_limit, continue_after_restart)
        .await
        .map_err(|e| e.to_string())?;
    if continue_after_restart == Some(false) {
        let _dispatch = state.dispatch_lock.lock().await;
        for handoff in state
            .store
            .pending_update_handoffs()
            .await
            .map_err(|e| e.to_string())?
        {
            if after_restart(&handoff) && handoff.state == "paused" {
                drop_restart_continuation(state, &handoff).await?;
            }
        }
    }
    Ok(settings)
}

/// Ends a restart continuation the owner turned off: its reply fails with
/// what it wrote so far, as when the setting was off at the restart.
async fn drop_restart_continuation(
    state: &AppState,
    handoff: &UpdateHandoff,
) -> Result<(), String> {
    state
        .store
        .cancel_update_handoff(&handoff.thread_id, &handoff.stopped_turn_id)
        .await
        .map_err(|e| e.to_string())?;
    settle_partial_assistant(state, handoff).await?;
    crate::process_app_server_notification(
        state,
        json!({"method": "turn/completed",
        "params": {"threadId": handoff.thread_id, "turnId": handoff.stopped_turn_id,
            "turn": {"id": handoff.stopped_turn_id, "status": "failed"}}}),
        false,
    )
    .await;
    Ok(())
}

async fn recover_one<'a>(
    state: &'a AppState,
    handoff: &UpdateHandoff,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) -> Result<(), Failure> {
    // Only a continuation not yet sent is dropped; a `submitting` one may
    // already be accepted, so it keeps its receipt reconciliation below.
    if after_restart(handoff)
        && handoff.state == "paused"
        && !state
            .store
            .continuation_settings()
            .await
            .map_err(|e| e.to_string())?
            .continue_after_restart
    {
        return Ok(drop_restart_continuation(state, handoff).await?);
    }
    if state
        .store
        .owning_project_conversation(&handoff.conversation_id)
        .await
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err(Failure::Blocked("The paused Project is unavailable".into()));
    }
    let mut policy = None;
    let rpc = if let Some(child) =
        crate::subagents::runtime_for_conversation(state, &handoff.conversation_id).await?
    {
        if child.ownership.thread_id != handoff.thread_id {
            return Err("Agent task ownership changed".into());
        }
        validate_parent_execution(state, &handoff.conversation_id)
            .await
            .map_err(Failure::Blocked)?;
        child.rpc
    } else {
        let binding = state
            .store
            .runtime_binding(&handoff.conversation_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Conversation ownership is unavailable")?;
        if binding.thread_id != handoff.thread_id {
            return Err("Conversation ownership changed".into());
        }
        if let Some(message_id) = &handoff.message_id {
            let message = state
                .store
                .message_by_id(message_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("Missing paused message")?;
            if !matches!(message.state.as_str(), "accepted_by_codex" | "streaming")
                || message.codex_turn_id.as_deref() != Some(&handoff.stopped_turn_id)
            {
                state
                    .store
                    .cancel_update_handoff(&handoff.thread_id, &handoff.stopped_turn_id)
                    .await
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
            policy = validate_execution(state, handoff)
                .await
                .map_err(Failure::Blocked)?;
        }
        crate::claude::for_thread(state, &handoff.thread_id).await?
    };
    let mut resume: Value =
        serde_json::from_str(&handoff.resume_params).map_err(|e| e.to_string())?;
    if let Some(policy) = &policy {
        resume["wonderPolicy"] = policy.clone();
    }
    let resumed = result(&rpc, "thread/resume", resume).await?;
    if resumed.pointer("/thread/id").and_then(Value::as_str) != Some(&handoff.thread_id) {
        return Err("The runtime reopened a different conversation".into());
    }
    let (status, accepted) = history(
        &rpc,
        &handoff.thread_id,
        &handoff.stopped_turn_id,
        &handoff.resume_client_id,
    )
    .await?;
    if status.as_deref() == Some("interrupted") {
        settle_partial_assistant(state, handoff).await?;
    }
    let new_turn = if let Some(turn) = accepted {
        turn
    } else {
        if resumed
            .pointer("/thread/status/type")
            .and_then(Value::as_str)
            == Some("active")
        {
            return Err("Other native work is still running in this conversation".into());
        }
        // A submit intent is written before bytes leave Wonder. Never blindly
        // resend if its result was lost during another restart.
        if handoff.state == "submitting" {
            return Err("The continuation's acceptance is uncertain; no duplicate was sent".into());
        }
        match status.as_deref() {
            Some("inProgress") => return Ok(()), // preparation failed before Stop
            Some("interrupted") => {}
            Some("completed" | "failed") => {
                state
                    .store
                    .cancel_update_handoff(&handoff.thread_id, &handoff.stopped_turn_id)
                    .await
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
            _ => return Err("The paused turn's native history is unavailable".into()),
        }
        let mut params: Value =
            serde_json::from_str(&handoff.turn_params).map_err(|e| e.to_string())?;
        params["clientUserMessageId"] = json!(handoff.resume_client_id);
        if let (Some(policy), Some(frozen)) = (policy, params.get_mut("wonderPolicy")) {
            *frozen = policy;
        }
        let text = if after_restart(handoff) {
            CONTINUE_AFTER_RESTART
        } else {
            CONTINUE
        };
        params["input"] = json!([{"type":"text","text":text}]);
        if !state
            .store
            .claim_update_continuation(handoff)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(());
        }
        // An error response is the provider saying no, not a lost answer: no
        // turn exists, so settle the reply instead of stranding it as
        // `submitting`. A timeout or transport error stays uncertain.
        let reply = rpc
            .request("turn/start", params)
            .await
            .map_err(|e| e.to_string())?;
        if reply.error.is_some() {
            return Err(Failure::Blocked(
                "The runtime refused to continue this conversation".into(),
            ));
        }
        let response = reply
            .result
            .ok_or("Update handoff: turn/start returned no result")?;
        response
            .pointer("/turn/id")
            .or_else(|| response.get("turnId"))
            .and_then(Value::as_str)
            .ok_or("The continuation returned no native receipt")?
            .to_owned()
    };
    // An accepted continuation must be committed even if its response arrived
    // after Stop; the user-facing Stop path is serialized with this dispatcher.
    if state
        .store
        .finish_update_continuation(handoff, &new_turn)
        .await
        .map_err(|e| e.to_string())?
    {
        state.ingestion.request_reconciliation();
        if let Some(id) = &handoff.message_id {
            if let Some(message) = state
                .store
                .message_by_id(id)
                .await
                .map_err(|e| e.to_string())?
            {
                crate::drain_pending_with_dispatch(state, &handoff.thread_id, &new_turn, dispatch)
                    .await;
                crate::publish_message_state(
                    state,
                    &message,
                    crate::DeliveryState::AcceptedByCodex,
                    Some(&handoff.thread_id),
                    Some(&new_turn),
                )
                .await;
            }
        }
        let _ = state.logger.record("info", "update_turn_resumed", json!({"threadId":handoff.thread_id,"oldTurnId":handoff.stopped_turn_id,"turnId":new_turn}));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use wonder_store::{AgentFamily, MessageInsert};

    // Counterexamples use this owner's real recovery path and offline runtime.
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../specs/quint/turn_reproductions.rs"
    ));

    async fn fixture() -> (tempfile::TempDir, AppState, wonder_store::StoredMessage) {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let workspace = state
            .store
            .project_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .unwrap()
            .cwd;
        fs::write(
            dir.path().join("update-fixture.json"),
            json!({"thread":[{"id":"old","status":"inProgress"}]}).to_string(),
        )
        .unwrap();
        crate::test_support::bind_project_thread(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();

        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                "original",
                "perform the task",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("message");
        };
        state
            .store
            .update_message_delivery(
                &message.id,
                "accepted_by_codex",
                Some("thread"),
                Some("old"),
            )
            .await
            .unwrap();
        let rpc = state.projects.codex.lock().await.rpc();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, rpc.health());
        remember_settings(&state, "thread", &json!({"threadId":"thread","excludeTurns":true,"cwd":workspace,"runtimeWorkspaceRoots":[workspace]}),
            &json!({"threadId":"thread","input":[{"type":"text","text":"perform the task"}],"clientUserMessageId":"original","approvalPolicy":"on-request","sandboxPolicy":{"type":"workspaceWrite","writableRoots":[workspace],"networkAccess":false},"model":"saved-model","effort":"high"})).await.unwrap();
        (dir, state, message)
    }

    async fn restart(state: &AppState) {
        let config = state
            .projects
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut runtime = state.projects.codex.lock().await;
        runtime.shutdown().await.unwrap();
        runtime.restart(config).await.unwrap();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, runtime.health());
    }

    fn starts(dir: &tempfile::TempDir) -> Vec<Value> {
        fs::read_to_string(dir.path().join("requests-jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|r| r["method"] == "turn/start")
            .map(|r| r["params"].clone())
            .collect()
    }

    #[tokio::test]
    async fn update_refuses_work_that_started_after_the_owner_was_asked() {
        let (_dir, state, message) = fixture().await;
        // The owner saw an idle host; a reply started before the prepare.
        let refused = state
            .update_admission
            .prepare_consented_update("asked-idle", Some(0), &state)
            .await
            .unwrap_err();
        assert_eq!(refused.0, axum::http::StatusCode::CONFLICT);
        assert!(refused.1.starts_with(crate::update_admission::WORK_STARTED));
        // Nothing was paused or fenced, and the reply keeps running.
        assert!(!state.update_admission.work_paused());
        assert!(state.update_admission.claim_guard().await.is_some());
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "accepted_by_codex");
        // Consent that covers the running reply pauses it as before.
        state
            .update_admission
            .prepare_consented_update("asked-busy", Some(1), &state)
            .await
            .unwrap();
        assert!(state.update_admission.claim_guard().await.is_none());
        assert!(state.update_admission.cancel_lease("asked-busy").await);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restart_continues_same_receipt_with_frozen_policy_and_no_original_replay() {
        let (dir, mut state, message) = fixture().await;
        state
            .update_admission
            .prepare_update("update", &state)
            .await
            .unwrap();
        assert!(state.update_admission.claim_guard().await.is_none());
        assert!(terminal(&state, "thread", "old", Some("interrupted"))
            .await
            .unwrap());
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_codex"
        );
        // Reopen SQLite and restart the real JSON-RPC child process. The new
        // admission instance represents a replacement daemon's empty lease.
        state.store = wonder_store::Store::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        state.update_admission = Default::default();
        restart(&state).await;
        recover(&state).await.unwrap();
        recover(&state).await.unwrap();
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.codex_turn_id.as_deref(),
            Some("resumed-1"),
            "{}",
            fs::read_to_string(dir.path().join("logs/test.jsonl")).unwrap_or_default()
        );
        let started = starts(&dir);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["model"], "saved-model");
        assert_eq!(started[0]["approvalPolicy"], "on-request");
        assert_eq!(started[0]["input"][0]["text"], CONTINUE);
        assert_ne!(started[0]["clientUserMessageId"], "original");
        assert!(terminal(&state, "thread", "old", Some("interrupted"))
            .await
            .unwrap());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_reply_a_crash_cut_off_continues_only_with_the_setting_on() {
        for setting in [true, false] {
            let (dir, state, message) = fixture().await;
            state
                .store
                .set_continuation_settings(None, Some(setting))
                .await
                .unwrap();
            // The runtime ended mid-turn (a crash, not Stop) and came back.
            fs::write(
                dir.path().join("update-fixture.json"),
                json!({"thread":[{"id":"old","status":"interrupted"}]}).to_string(),
            )
            .unwrap();
            restart(&state).await;
            crate::projects::recover(&state).await;
            let pending = state.store.pending_update_handoffs().await.unwrap();
            assert_eq!(pending.len(), usize::from(setting));
            recover(&state).await.unwrap();
            let saved = state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap();
            if setting {
                assert_eq!(pending[0].request_id, RESTART_REQUEST);
                let started = starts(&dir);
                assert_eq!(started.len(), 1);
                assert_eq!(started[0]["input"][0]["text"], CONTINUE_AFTER_RESTART);
                assert_eq!(started[0]["model"], "saved-model");
                assert_eq!(saved.codex_turn_id.as_deref(), Some("resumed-1"));
                assert_eq!(saved.state, "accepted_by_codex");
            } else {
                assert!(starts(&dir).is_empty());
                assert_ne!(saved.state, "accepted_by_codex");
            }
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    // Contract: a Stop the host restarted before the provider confirmed is the
    // owner's, so the interrupted turn ends stopped and is never continued.
    #[tokio::test]
    async fn a_stop_survives_a_host_restart_and_is_never_continued() {
        let (dir, state, message) = fixture().await;
        state
            .store
            .set_continuation_settings(None, Some(true))
            .await
            .unwrap();
        crate::projects::request_stop(&state, "thread", "old")
            .await
            .unwrap();
        // The host restarts before turn/completed: memory is gone, the store stays.
        state.projects.forget_cached_stops();
        fs::write(
            dir.path().join("update-fixture.json"),
            json!({"thread":[{"id":"old","status":"interrupted"}]}).to_string(),
        )
        .unwrap();
        restart(&state).await;
        let mut events = state.events.subscribe();
        crate::projects::recover(&state).await;
        assert!(state
            .store
            .pending_update_handoffs()
            .await
            .unwrap()
            .is_empty());
        recover(&state).await.unwrap();
        assert!(starts(&dir).is_empty());
        let mut published = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let wonder_api::WonderEvent::MessageState { state } = event.event {
                published.push(state);
            }
        }
        assert_eq!(published, [wonder_api::DeliveryState::Interrupted]);
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(saved.state, "accepted_by_codex");
        // Settled: the Stop is gone with the turn.
        assert!(!state
            .store
            .turn_stop_recorded("thread", "old")
            .await
            .unwrap());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn turning_continue_after_restarts_off_settles_continuations_not_yet_sent() {
        // Off through the owner's setting (which settles at once), or off
        // in the store after the continuation was recorded (recover rechecks).
        for through_setting in [true, false] {
            let (dir, state, message) = fixture().await;
            fs::write(
                dir.path().join("update-fixture.json"),
                json!({"thread":[{"id":"old","status":"interrupted"}]}).to_string(),
            )
            .unwrap();
            restart(&state).await;
            crate::projects::recover(&state).await;
            let pending = state.store.pending_update_handoffs().await.unwrap();
            assert_eq!(pending.len(), 1);
            if through_setting {
                set_continuation_settings(&state, None, Some(false))
                    .await
                    .unwrap();
                assert!(state
                    .store
                    .pending_update_handoffs()
                    .await
                    .unwrap()
                    .is_empty());
            } else {
                state
                    .store
                    .set_continuation_settings(None, Some(false))
                    .await
                    .unwrap();
                assert!(!state
                    .store
                    .claim_update_continuation(&pending[0])
                    .await
                    .unwrap());
            }
            recover(&state).await.unwrap();
            assert!(starts(&dir).is_empty());
            assert!(state
                .store
                .pending_update_handoffs()
                .await
                .unwrap()
                .is_empty());
            let saved = state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap();
            // Settled like a turn the setting never continued: no longer running.
            assert!(!matches!(
                saved.state.as_str(),
                "accepted_by_codex" | "streaming"
            ));
            assert_eq!(saved.codex_turn_id.as_deref(), Some("old"));
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn turning_continue_after_restarts_off_keeps_a_continuation_already_being_sent() {
        // A `submitting` continuation may already be accepted: turning the
        // setting off must not drop it or its receipt reconciliation.
        let (dir, state, _message) = fixture().await;
        fs::write(
            dir.path().join("update-fixture.json"),
            json!({"thread":[{"id":"old","status":"interrupted"}]}).to_string(),
        )
        .unwrap();
        restart(&state).await;
        crate::projects::recover(&state).await;
        let pending = state.store.pending_update_handoffs().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(state
            .store
            .claim_update_continuation(&pending[0])
            .await
            .unwrap());
        state
            .store
            .set_continuation_settings(None, Some(false))
            .await
            .unwrap();
        let _ = recover(&state).await;
        let left = state.store.pending_update_handoffs().await.unwrap();
        assert_eq!(left.len(), 1, "a continuation being sent is not dropped");
        assert_eq!(left[0].state, "submitting");
        assert!(starts(&dir).is_empty(), "nothing is sent again");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_reply_that_keeps_crashing_is_continued_at_most_twice() {
        let (_dir, state, message) = fixture().await;
        for turn in ["old", "second", "third"] {
            state
                .store
                .update_message_delivery(
                    &message.id,
                    "accepted_by_codex",
                    Some("thread"),
                    Some(turn),
                )
                .await
                .unwrap();
            let current = state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap();
            let recorded = continue_after_restart(&state, &current, "thread", turn).await;
            assert_eq!(recorded, turn != "third", "{turn}");
            state
                .store
                .cancel_update_handoff("thread", turn)
                .await
                .unwrap();
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manual_stop_and_natural_completion_never_continue() {
        for manual in [true, false] {
            let (dir, state, message) = fixture().await;
            if !manual {
                fs::write(dir.path().join("complete-during-update"), "").unwrap();
            }
            pause(&state, "update").await.unwrap();
            if manual {
                state
                    .store
                    .cancel_update_handoff("thread", "old")
                    .await
                    .unwrap();
                state
                    .store
                    .interrupt_message_if_active(&message.id)
                    .await
                    .unwrap();
            }
            recover(&state).await.unwrap();
            assert!(starts(&dir).is_empty());
            assert!(!terminal(
                &state,
                "thread",
                "old",
                Some(if manual { "interrupted" } else { "completed" })
            )
            .await
            .unwrap());
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_install_resumes_on_existing_host_and_unmarked_stop_does_not() {
        let (dir, state, _) = fixture().await;
        state
            .update_admission
            .prepare_update("update", &state)
            .await
            .unwrap();
        assert!(state.update_admission.cancel_lease("update").await);
        recover(&state).await.unwrap();
        assert_eq!(starts(&dir).len(), 1);
        assert!(
            !terminal(&state, "thread", "other-user-stop", Some("interrupted"))
                .await
                .unwrap()
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lost_acceptance_response_reconciles_once_without_duplicate() {
        let (dir, state, message) = fixture().await;
        pause(&state, "update").await.unwrap();
        fs::write(dir.path().join("lose-update-response"), "").unwrap();
        recover(&state).await.unwrap();
        assert_eq!(
            state.store.pending_update_handoffs().await.unwrap()[0].state,
            "submitting"
        );
        fs::remove_file(dir.path().join("lose-update-response")).unwrap();
        restart(&state).await;
        recover(&state).await.unwrap();
        assert_eq!(starts(&dir).len(), 1);
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .codex_turn_id
                .as_deref(),
            Some("resumed-1")
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_children_continue_with_inherited_settings_before_the_parent() {
        let (dir, state, _) = fixture().await;
        fs::write(dir.path().join("update-fixture.json"),json!({"thread":[{"id":"old","status":"inProgress"}],"child-thread":[{"id":"child-old","status":"inProgress"}]}).to_string()).unwrap();
        pause(&state, "update").await.unwrap();
        assert_eq!(
            state.store.pending_update_handoffs().await.unwrap().len(),
            2
        );
        restart(&state).await;
        recover(&state).await.unwrap();
        let started = starts(&dir);
        assert_eq!(started.len(), 2);
        assert_eq!(started[0]["threadId"], "child-thread");
        assert!(started[0].get("permissions").is_none());
        assert!(started[0].get("model").is_none());
        assert_eq!(started[1]["threadId"], "thread");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn archived_parent_never_resumes_its_native_children() {
        let (dir, state, _) = fixture().await;
        fs::write(dir.path().join("update-fixture.json"), json!({"thread":[{"id":"old","status":"inProgress"}],"child-thread":[{"id":"child-old","status":"inProgress"}]}).to_string()).unwrap();
        pause(&state, "update").await.unwrap();
        state
            .store
            .update_project_metadata(
                "project-d19b0f7e-97bb-45d7-8206-324a62386dd9",
                None,
                Some(false),
                None,
                "now",
            )
            .await
            .unwrap();
        restart(&state).await;
        recover(&state).await.unwrap();
        assert!(starts(&dir).is_empty());
        // Neither waits to retry: both need the owner.
        assert!(state
            .store
            .pending_update_handoffs()
            .await
            .unwrap()
            .is_empty());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn revoked_write_root_is_not_restored_by_update_continuation() {
        let (dir, state, message) = fixture().await;
        let extra = dir.path().join("shared");
        fs::create_dir(&extra).unwrap();
        let extra = crate::canonical_path::canonicalize(extra)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let workspace = state
            .store
            .project_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .unwrap()
            .cwd;
        let roots = vec![workspace.clone(), extra.clone()];
        state
            .store
            .update_project_roots(
                "project-d19b0f7e-97bb-45d7-8206-324a62386dd9",
                1,
                &roots
                    .iter()
                    .map(|p| wonder_store::ProjectRootInput {
                        path: p.clone(),
                        canonical_path: p.clone(),
                    })
                    .collect::<Vec<_>>(),
                0,
                "now",
            )
            .await
            .unwrap();
        remember_settings(
            &state,
            "thread",
            &json!({"threadId":"thread","excludeTurns":true,
            "cwd":workspace,"runtimeWorkspaceRoots":roots}),
            &json!({"threadId":"thread","approvalPolicy":"on-request","sandboxPolicy":{"type":"workspaceWrite","writableRoots":roots,"networkAccess":false}}),
        )
        .await
        .unwrap();
        pause(&state, "update").await.unwrap();
        state
            .store
            .update_project_roots(
                "project-d19b0f7e-97bb-45d7-8206-324a62386dd9",
                2,
                &[wonder_store::ProjectRootInput {
                    path: workspace.clone(),
                    canonical_path: workspace,
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        restart(&state).await;
        recover(&state).await.unwrap();
        assert!(starts(&dir).is_empty());
        // A continuation that needs the owner is settled, not left running.
        assert!(state
            .store
            .pending_update_handoffs()
            .await
            .unwrap()
            .is_empty());
        let message = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.state, "interrupted");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn claude_access_compares_owner_choices_not_policy_format() {
        let frozen = json!({"mode":"full_access","approvalMode":"full_access","planMode":false,
            "workspace":"/media","readRoots":["/"],"writeRoots":["/app"],"deniedRoots":["/secret"],
            "unsandboxedCommands":true});
        let fresh = json!({"mode":"full_access","approvalMode":"full_access","planMode":false,
            "workspace":"/media","readRoots":["/"],"writeRoots":["/app"],"deniedRoots":["/secret"]});
        // A field a newer Wonder dropped, or a protected folder it added, does not strand work.
        assert!(same_claude_access(&frozen, &fresh));
        let mut narrower = fresh.clone();
        narrower["deniedRoots"] = json!(["/secret", "/new"]);
        assert!(same_claude_access(&frozen, &narrower));
        // Any change to what the owner chose blocks the continuation.
        for (key, value) in [
            ("mode", json!("workspace")),
            ("approvalMode", json!("auto")),
            ("planMode", json!(true)),
            ("workspace", json!("/other")),
            ("writeRoots", json!(["/app", "/b"])),
            ("readRoots", json!(["/app"])),
            ("deniedRoots", json!([])),
        ] {
            let mut changed = fresh.clone();
            changed[key] = value;
            assert!(!same_claude_access(&frozen, &changed), "{key}");
        }
        assert!(!same_claude_access(&json!(null), &fresh));
    }

    #[tokio::test]
    async fn stopping_parent_cancels_its_paused_native_children() {
        let (dir, state, message) = fixture().await;
        fs::write(dir.path().join("update-fixture.json"),json!({"thread":[{"id":"old","status":"inProgress"}],"child-thread":[{"id":"child-old","status":"inProgress"}]}).to_string()).unwrap();
        pause(&state, "update").await.unwrap();
        state
            .store
            .cancel_update_handoff_tree("d19b0f7e-97bb-45d7-8206-324a62386dd9", "thread", "old")
            .await
            .unwrap();
        state
            .store
            .interrupt_message_if_active(&message.id)
            .await
            .unwrap();
        restart(&state).await;
        recover(&state).await.unwrap();
        assert!(starts(&dir).is_empty());
        assert!(state
            .store
            .pending_update_handoffs()
            .await
            .unwrap()
            .is_empty());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn automation_stays_running_through_pause_and_finishes_on_the_continuation() {
        let (_dir, state, message) = fixture().await;
        state
            .store
            .insert_scoped_automation(
                "auto",
                "Task",
                "continuation",
                "project_thread",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                Some("d19b0f7e-97bb-45d7-8206-324a62386dd9"),
                "Task",
                "FREQ=DAILY",
                "UTC",
                "active",
                "all_runs",
                None,
                "now",
            )
            .await
            .unwrap();
        state
            .store
            .claim_automation_run("run", "auto", "now", "now")
            .await
            .unwrap();
        state
            .store
            .set_automation_run_message_id("run", &message.id)
            .await
            .unwrap();
        pause(&state, "update").await.unwrap();
        let mut events = state.events.subscribe();
        assert!(crate::process_app_server_notification(&state,json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"old","status":"interrupted"}}}),false).await);
        assert_eq!(
            state
                .store
                .automation_run_for_message(&message.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        assert!(events.try_recv().is_err());
        recover(&state).await.unwrap();
        assert!(crate::process_app_server_notification(&state,json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"resumed-1","status":"completed"}}}),false).await);
        assert_eq!(
            state
                .store
                .automation_run_for_message(&message.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuation_is_hidden_and_original_bubble_keeps_its_turn() {
        let (_dir, state, message) = fixture().await;

        pause(&state, "update").await.unwrap();
        recover(&state).await.unwrap();
        let handoff = state
            .store
            .update_handoff("thread", "old")
            .await
            .unwrap()
            .unwrap();
        assert!(crate::process_app_server_notification(&state,json!({"method":"item/completed","params":{"threadId":"thread","turnId":"resumed-1","item":{"id":"continuation","type":"userMessage","clientId":handoff.resume_client_id,"content":[{"type":"text","text":CONTINUE}]}}}),false).await);
        let response = crate::history::conversation_snapshot(
            axum::extract::State(state.clone()),
            axum::extract::Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
            axum::extract::Query(crate::history::HistoryQuery::default()),
            axum::http::HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snapshot: Value = serde_json::from_slice(&body).unwrap();
        let user_items: Vec<_> = snapshot["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| {
                t["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|i| i["type"] == "userMessage")
                    .map(move |i| (t, i))
            })
            .collect();
        assert_eq!(user_items.len(), 1);
        assert_eq!(user_items[0].0["id"], "old");
        assert_eq!(user_items[0].1["text"], "perform the task");
        assert_eq!(snapshot["messages"][0]["codexTurnId"], "resumed-1");
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .codex_turn_id
                .as_deref(),
            Some("resumed-1")
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_goal_or_background_command_defers_before_interrupting() {
        for goal in [true, false] {
            let (dir, state, _) = fixture().await;
            if goal {
                fs::write(
                    dir.path().join("goal.json"),
                    json!({"status":"active"}).to_string(),
                )
                .unwrap();
            } else {
                fs::write(
                    dir.path().join("idle-fixture.json"),
                    json!({"thread":{"background":[{"id":"terminal"}]}}).to_string(),
                )
                .unwrap();
            }
            assert!(pause(&state, "update").await.is_err());
            assert!(state
                .store
                .pending_update_handoffs()
                .await
                .unwrap()
                .is_empty());
            let checkpoint: Value = serde_json::from_str(
                &fs::read_to_string(dir.path().join("update-fixture.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(checkpoint["thread"][0]["status"], "inProgress");
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn retained_journals_are_not_loaded_work_but_a_loaded_turn_still_waits() {
        let (dir, state, _) = fixture().await;
        // Hundreds of retained chats, none loaded and running.
        fs::write(dir.path().join("retained-journals"), "").unwrap();
        fs::write(
            dir.path().join("update-fixture.json"),
            json!({}).to_string(),
        )
        .unwrap();
        pause(&state, "update").await.unwrap();
        // A loaded thread with a running turn still refuses.
        fs::write(
            dir.path().join("update-fixture.json"),
            json!({"unowned":[{"id":"external","status":"inProgress"}]}).to_string(),
        )
        .unwrap();
        assert!(pause(&state, "update").await.is_err());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ambiguous_submission_and_unknown_work_fail_closed() {
        let (dir, state, _) = fixture().await;
        fs::write(dir.path().join("update-fixture.json"),json!({"thread":[{"id":"old","status":"inProgress"}],"unowned":[{"id":"external","status":"inProgress"}]}).to_string()).unwrap();
        assert!(pause(&state, "update").await.is_err());
        assert!(state
            .store
            .pending_update_handoffs()
            .await
            .unwrap()
            .is_empty());
        fs::write(
            dir.path().join("update-fixture.json"),
            json!({"thread":[{"id":"old","status":"inProgress"}]}).to_string(),
        )
        .unwrap();
        pause(&state, "update").await.unwrap();
        fs::write(dir.path().join("reject-update-continuation"), "").unwrap();
        recover(&state).await.unwrap();
        recover(&state).await.unwrap();
        // The provider answered with an error: one start, and the handoff is
        // settled rather than left `submitting` with its chat Working forever.
        assert_eq!(starts(&dir).len(), 1);
        assert!(state
            .store
            .pending_update_handoffs()
            .await
            .unwrap()
            .is_empty());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}
