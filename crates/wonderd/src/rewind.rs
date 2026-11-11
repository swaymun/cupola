//! Edit from here: rewind a Project conversation to before one of the
//! owner's messages, first in the provider's own history and then in
//! Wonder's history index, so the phone can put that message back in the
//! composer.
//!
//! What each provider offers (`ProviderCapabilities::rewind`/`rewind_files`):
//!
//! - Codex: `thread/revert {threadId, beforeTurnId}` replaces the thread's
//!   durable history with the turns before one. It never reverts files.
//! - Claude: Wonder's bridge (`thread/rewind`) resumes the session at the
//!   entry before the message (`resumeSessionAt`) and, only when the owner
//!   turned on file backups before those replies ran, restores files with the
//!   SDK's `rewindFiles`.
//! - OpenCode: ACP cannot remove turns; refused.
//!
//! Wonder never reports restored files unless the provider restored them.
use crate::{projects, AppState, OwnerAuthority};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wonder_app_server::RpcClient;
use wonder_store::{AgentFamily, NativeSessionKey, ProviderSetting, RewindOperation};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RewindRequest {
    /// The turn the owner's message opened. It and every later turn go.
    pub(crate) turn_id: String,
    /// Also put back the files the agent edited from that message on.
    #[serde(default)]
    pub(crate) restore_files: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RewindResponse {
    history_revision: i64,
    /// Turns removed from the conversation, the chosen one included.
    removed_turns: usize,
    /// True only when the provider restored files.
    files_restored: bool,
    /// How many files the provider restored, when it said.
    #[serde(skip_serializing_if = "Option::is_none")]
    files_changed: Option<u64>,
    /// Files the provider left as they are because a link was in the way.
    #[serde(skip_serializing_if = "Option::is_none")]
    files_skipped: Option<u64>,
}

/// What the provider removed.
struct NativeRewind {
    removed: Vec<String>,
    files_restored: bool,
    files_changed: Option<u64>,
    files_skipped: Option<u64>,
    /// The session the conversation continues in, when it changed.
    session: Option<String>,
}

/// Serializes active rewinds and recovery passes in this process. The
/// journal owns the durable fence; dropping this guard never releases it.
struct Rewinding(AppState, String);
impl Drop for Rewinding {
    fn drop(&mut self) {
        self.0.projects.end_rewind(&self.1);
        self.0.dispatch_wake.notify_one();
    }
}

/// How long recovery waits after this process's own provider call ended
/// without a reply before it trusts the provider's history: a call that timed
/// out may still apply late.
const LATE_APPLY_GRACE_MS: i64 = 15_000;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    projects::error(status, message)
}

pub(crate) async fn rewind(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Json(request): Json<RewindRequest>,
) -> Response {
    if request.turn_id.is_empty() || request.turn_id.len() > 128 {
        return error(StatusCode::BAD_REQUEST, "Choose one of your messages.");
    }
    let Ok(Some(source)) = state.store.project_conversation(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let provider = source.family.provider();
    let name = provider.display_name;
    if !provider.capabilities.rewind {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{name} chats can’t be edited from an earlier message."),
        );
    }
    if request.restore_files {
        if !provider.capabilities.rewind_files {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{name} can’t restore files. Your files stay as they are."),
            );
        }
        if !state
            .store
            .provider_setting(source.family, ProviderSetting::FileCheckpointing)
            .await
            .unwrap_or(false)
        {
            return error(
                StatusCode::CONFLICT,
                format!("Turn on “Let {name} restore files when you rewind” in Settings first."),
            );
        }
    }
    let Some(native) = source.native_session_id.clone() else {
        return error(
            StatusCode::CONFLICT,
            "Send a message first. There is nothing to edit yet.",
        );
    };
    match projects::is_archived(&state, &source).await {
        Ok(false) => {}
        Ok(true) => {
            return error(
                StatusCode::CONFLICT,
                "This chat is archived. Restore it before editing it.",
            )
        }
        Err(message) => return error(StatusCode::SERVICE_UNAVAILABLE, message),
    }
    let Ok(Some(binding)) = state.store.runtime_binding(&id).await else {
        return error(
            StatusCode::CONFLICT,
            "Send a message first. There is nothing to edit yet.",
        );
    };
    // The journal, the provider call and the settlement belong to this
    // process, not to the HTTP request: a dropped connection cannot cancel
    // them half way and leave the chat fenced.
    let job = Job {
        state: state.clone(),
        id,
        family: source.family,
        native,
        thread: binding.thread_id.clone(),
        turn: request.turn_id,
        restore_files: request.restore_files,
    };
    match tokio::spawn(job.run()).await {
        Ok(response) => response,
        Err(_) => recovery_error(),
    }
}

struct Job {
    state: AppState,
    id: String,
    family: AgentFamily,
    native: String,
    thread: String,
    turn: String,
    restore_files: bool,
}

impl Job {
    async fn run(self) -> Response {
        let Job {
            state,
            id,
            family,
            native,
            thread,
            turn,
            restore_files,
        } = self;
        let provider = family.provider();
        let name = provider.display_name;
        let busy = || {
            error(
                StatusCode::CONFLICT,
                format!("{name} is still working in this chat. Edit it when the reply finishes."),
            )
        };
        let Some(_admission) = state.update_admission.claim_guard().await else {
            return error(
                StatusCode::CONFLICT,
                "Cupola is preparing to update. Try again shortly.",
            );
        };
        // Work delivered, queued or uncertain, or a reply running in the
        // provider's own app on the Mac, belongs to the history being cut.
        if state
            .store
            .conversation_has_archive_blocking_work(&id)
            .await
            .unwrap_or(true)
            || projects::held_on_mac(&state, &id, None).await
        {
            return busy();
        }
        let _rewinding = {
            let _dispatch = state
                .update_admission
                .dispatch_guard(&state.dispatch_lock)
                .await;
            if state
                .store
                .conversation_has_archive_blocking_work(&id)
                .await
                .unwrap_or(true)
                || !state.projects.begin_rewind(&id)
            {
                return busy();
            }
            Rewinding(state.clone(), id.clone())
        };
        let rpc = match crate::claude::for_thread(&state, &thread).await {
            Ok(rpc) => rpc,
            Err(message) => return error(StatusCode::SERVICE_UNAVAILABLE, message),
        };
        let turns = match native_turns(&rpc, &thread).await {
            Ok(turns) => turns,
            Err(message) => return error(StatusCode::BAD_GATEWAY, message),
        };
        if !turns.contains(&turn) {
            return error(
                StatusCode::CONFLICT,
                "That message is no longer in this chat. Reopen it and try again.",
            );
        }
        let operation = RewindOperation {
            id: uuid::Uuid::new_v4().to_string(),
            conversation: id.clone(),
            thread: thread.clone(),
            native_session: native,
            runtime_id: rpc.health().id().to_owned(),
            turn: turn.clone(),
            turns,
            ..Default::default()
        };
        if state
            .store
            .begin_project_rewind(&operation, &crate::now_ms().to_string())
            .await
            .is_err()
        {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Cupola couldn’t save the edit. Wait a moment and try again.",
            );
        }
        // From this point a lost provider response and every store error leave
        // the durable fence in place. Recovery only reads the provider; it
        // never repeats a rewind (especially one that restores files).
        let response = match provider.native_session {
            NativeSessionKey::ThreadId => {
                rpc.request(
                    "thread/revert",
                    json!({"threadId": thread, "beforeTurnId": turn}),
                )
                .await
            }
            NativeSessionKey::BridgeSessionId => {
                rpc.request(
                    "thread/rewind",
                    json!({"threadId": thread, "turnId": turn, "restoreFiles": restore_files}),
                )
                .await
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(_) => {
                // A timeout does not prove the provider stopped; recovery
                // inspects it once the grace has passed.
                let _ = state
                    .store
                    .record_rewind_call_end(&operation, &crate::now_ms().to_string())
                    .await;
                return recovery_error();
            }
        };
        // If this small receipt write also fails, the intent remains uncertain
        // and recovery proves the outcome after the grace. A successful local
        // settlement below still removes the whole journal atomically.
        let _ = state.store.record_rewind_response(&operation).await;
        if let Some(failed) = response.error {
            // A completed rejection can be rolled back only after reading the
            // unchanged before-image.
            if let Ok(verdict) = inspect(&state, &operation, &rpc).await {
                let _ = apply(&state, &operation, verdict).await;
            }
            return if provider.native_session == NativeSessionKey::ThreadId {
                error(
                    StatusCode::BAD_GATEWAY,
                    format!("{name} couldn’t rewind this chat. Try again."),
                )
            } else {
                error(StatusCode::CONFLICT, failed.message)
            };
        }
        if response.result.is_none() {
            return recovery_error();
        }
        let native_rewind = match provider.native_session {
            NativeSessionKey::ThreadId => NativeRewind {
                removed: operation.removed_turns().unwrap().to_vec(),
                files_restored: false,
                files_changed: None,
                files_skipped: None,
                session: None,
            },
            NativeSessionKey::BridgeSessionId => match response
                .result
                .and_then(|result| bridge_result(result, &turn, restore_files).ok())
            {
                Some(result) => result,
                None => {
                    if let Ok(verdict) = inspect(&state, &operation, &rpc).await {
                        let _ = apply(&state, &operation, verdict).await;
                    }
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "The rewind could not be confirmed. History is being checked.",
                    );
                }
            },
        };
        let mut reported = native_rewind.removed.clone();
        let mut planned = operation.removed_turns().unwrap().to_vec();
        reported.sort();
        planned.sort();
        if reported != planned {
            return recovery_error();
        }
        let revision = state
            .store
            .settle_project_rewind(
                &operation,
                native_rewind.session.as_deref(),
                &crate::now_ms().to_string(),
            )
            .await;
        let Ok(history_revision) = revision else {
            let _ = state.logger.record(
                "error",
                "project_rewind_not_saved",
                json!({"conversationId": id}),
            );
            return recovery_error();
        };
        Json(RewindResponse {
            history_revision,
            removed_turns: native_rewind.removed.len(),
            files_restored: native_rewind.files_restored,
            files_changed: native_rewind.files_changed,
            files_skipped: native_rewind.files_skipped,
        })
        .into_response()
    }
}

/// The plain-language answer while a rewind is unresolved: history and sends
/// are paused, not broken.
pub(crate) fn recovering() -> Response {
    recovery_error()
}

fn recovery_error() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Cupola is recovering this edit. History and new messages are paused. If this continues, restart Cupola on this computer.",
    )
}

/// Read a complete, bounded newest-first before-image. Repeated cursors,
/// duplicate IDs, active turns and truncated history are not recovery proof.
async fn native_turns(rpc: &RpcClient, thread: &str) -> Result<Vec<String>, String> {
    let mut turns = Vec::new();
    let mut cursors = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut params = json!({"threadId":thread,"limit":100,"itemsView":"notLoaded"});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let page = projects::result(rpc, "thread/turns/list", params).await?;
        let data = page["data"]
            .as_array()
            .ok_or("Couldn’t read the conversation’s history.")?;
        if data.len() > 100 {
            return Err("History page is too large.".into());
        }
        for turn in data {
            let id = turn["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .ok_or("Couldn’t read a message in this conversation.")?;
            if !ids.insert(id.to_owned())
                || !matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            {
                return Err("The conversation is changing. Try again shortly.".into());
            }
            turns.push(id.to_owned());
        }
        match page.get("nextCursor") {
            None | Some(Value::Null) => return Ok(turns),
            Some(Value::String(next)) if !next.is_empty() && cursors.insert(next.clone()) => {
                cursor = Some(next.clone())
            }
            _ => return Err("Couldn’t finish reading this conversation’s history.".into()),
        }
    }
    Err("This conversation is too long to safely check the edit.".into())
}

/// Entry point for runtime recovery after a restart or a failed store write.
/// Provider reads hold neither update admission nor dispatch; only the small
/// store write that settles or rolls back takes them, and the store rechecks
/// the exact journal entry there.
pub(crate) async fn recover(state: &AppState) {
    let Ok(operations) = state.store.pending_rewinds().await else {
        return;
    };
    for operation in operations {
        if state.update_admission.work_paused() {
            return;
        }
        // A live rewind in this process owns its operation.
        if !state.projects.begin_rewind(&operation.conversation) {
            continue;
        }
        let _rewinding = Rewinding(state.clone(), operation.conversation.clone());
        let Ok(rpc) = crate::claude::for_thread(state, &operation.thread).await else {
            continue;
        };
        if !provider_quiet(&operation, rpc.health().id(), crate::now_ms() as i64) {
            continue;
        }
        let Ok(verdict) = inspect(state, &operation, &rpc).await else {
            continue;
        };
        let Some(_admission) = state.update_admission.claim_guard().await else {
            return;
        };
        let _dispatch = state
            .update_admission
            .dispatch_guard(&state.dispatch_lock)
            .await;
        let _ = apply(state, &operation, verdict).await;
    }
}

/// Whether nothing can still be changing the provider's history for
/// `operation`: its reply (or a replacement runtime) is in hand, or this
/// process's own call ended long enough ago. An in-flight call holds
/// `Rewinding`, so recovery never reaches it here.
fn provider_quiet(operation: &RewindOperation, runtime_id: &str, now_ms: i64) -> bool {
    if operation.response_received || operation.runtime_id != runtime_id {
        return true;
    }
    let since = operation
        .call_ended_at
        .as_deref()
        .unwrap_or(&operation.created_at)
        .parse::<i64>()
        .unwrap_or(0);
    now_ms.saturating_sub(since) >= LATE_APPLY_GRACE_MS
}

/// What the provider's own history says happened to a journaled rewind.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// The cut is in effect (or the provider moved on): hide these before-image
    /// turns, which the provider no longer has.
    Settle {
        hidden: Vec<String>,
        session: Option<String>,
    },
    /// The provider still has every before-image turn: roll the fence back.
    Cancel,
}

/// Newest-first `provider` turns against the journal's before-image. Turns the
/// owner added in the provider's own app meanwhile never block a decision, and
/// a history that is neither the cut nor the original still resolves to what
/// the provider holds, so a fenced chat cannot stay stuck.
fn verdict(operation: &RewindOperation, provider: &[String], session: Option<String>) -> Verdict {
    let present: std::collections::HashSet<&str> = provider.iter().map(String::as_str).collect();
    let has = |turn: &String| present.contains(turn.as_str());
    let removed = operation.removed_turns().unwrap_or_default();
    let retained = operation.retained_turns().unwrap_or_default();
    if !removed.iter().any(has) && retained.iter().all(has) {
        return Verdict::Settle {
            hidden: removed.to_vec(),
            session,
        };
    }
    let same_session = session
        .as_deref()
        .is_none_or(|session| session == operation.native_session);
    if operation.turns.iter().all(has) && same_session {
        return Verdict::Cancel;
    }
    Verdict::Settle {
        hidden: operation
            .turns
            .iter()
            .filter(|turn| !has(turn))
            .cloned()
            .collect(),
        session,
    }
}

/// Read the provider and decide; never writes.
async fn inspect(
    state: &AppState,
    operation: &RewindOperation,
    rpc: &RpcClient,
) -> Result<Verdict, String> {
    let source = state
        .store
        .project_conversation(&operation.conversation)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("The conversation was removed.")?;
    let session = if source.family.provider().native_session == NativeSessionKey::BridgeSessionId {
        let read =
            projects::result(rpc, "thread/read", json!({"threadId":operation.thread})).await?;
        Some(
            read["thread"]["sessionId"]
                .as_str()
                .ok_or("The native session could not be confirmed.")?
                .to_owned(),
        )
    } else {
        None
    };
    let turns = native_turns(rpc, &operation.thread).await?;
    if let Some(session) = &session {
        let after =
            projects::result(rpc, "thread/read", json!({"threadId":operation.thread})).await?;
        if after["thread"]["sessionId"].as_str() != Some(session.as_str()) {
            return Err("The native session changed while checking the edit.".into());
        }
    }
    Ok(verdict(operation, &turns, session))
}

/// The store write for a verdict; the caller holds update admission.
async fn apply(
    state: &AppState,
    operation: &RewindOperation,
    verdict: Verdict,
) -> Result<(), String> {
    match verdict {
        Verdict::Cancel => state.store.cancel_project_rewind(operation).await,
        Verdict::Settle { hidden, session } => state
            .store
            .settle_project_rewind_cut(
                operation,
                &hidden,
                session.as_deref(),
                &crate::now_ms().to_string(),
            )
            .await
            .map(|_| ()),
    }
    .map_err(|error| error.to_string())
}

fn bridge_result(
    result: Value,
    turn: &str,
    restore_files: bool,
) -> Result<NativeRewind, (StatusCode, String)> {
    let invalid = || {
        (
            StatusCode::BAD_GATEWAY,
            "The rewind could not be confirmed. Reopen the chat.".to_owned(),
        )
    };
    let removed: Vec<String> = result
        .get("removedTurnIds")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .filter(|turns: &Vec<String>| turns.first().map(String::as_str) == Some(turn))
        .ok_or_else(invalid)?;
    let files_restored = result.get("filesRestored").and_then(Value::as_bool) == Some(true);
    if files_restored && !restore_files {
        return Err(invalid());
    }
    Ok(NativeRewind {
        removed,
        files_restored,
        files_changed: result.get("filesChanged").and_then(Value::as_u64),
        files_skipped: result
            .get("filesSkipped")
            .and_then(Value::as_u64)
            .filter(|count| *count > 0),
        session: result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Settings > Providers: whether each provider that can restore files on
/// rewind keeps the backups it needs. Off by default.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileRestoreSettings {
    providers: Vec<FileRestoreSetting>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileRestoreSetting {
    family: AgentFamily,
    enabled: bool,
}

async fn file_restore_listing(state: &AppState) -> Response {
    let mut providers = Vec::new();
    for family in AgentFamily::ALL {
        if !family.provider().capabilities.rewind_files {
            continue;
        }
        let Ok(enabled) = state
            .store
            .provider_setting(family, ProviderSetting::FileCheckpointing)
            .await
        else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Cupola couldn’t read this setting. Try again.",
            );
        };
        providers.push(FileRestoreSetting { family, enabled });
    }
    Json(FileRestoreSettings { providers }).into_response()
}

pub(crate) async fn file_restore(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    file_restore_listing(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetFileRestore {
    enabled: bool,
}

/// Takes effect from the provider's next reply: only replies that ran with
/// backups on can have their files restored.
pub(crate) async fn set_file_restore(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<AgentFamily>,
    Json(request): Json<SetFileRestore>,
) -> Response {
    if !family.provider().capabilities.rewind_files {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{} can’t restore files when you rewind.",
                family.provider().display_name
            ),
        );
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if state
        .store
        .set_provider_setting(
            family,
            ProviderSetting::FileCheckpointing,
            request.enabled,
            &now,
        )
        .await
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The setting could not be saved. Try again.",
        );
    }
    file_restore_listing(&state).await
}

/// Whether a provider's turns should keep file backups. Sent with each turn
/// of a provider that can restore files (`fileCheckpointing`).
pub(crate) async fn checkpointing(state: &AppState, family: AgentFamily) -> bool {
    family.provider().capabilities.rewind_files
        && state
            .store
            .provider_setting(family, ProviderSetting::FileCheckpointing)
            .await
            .unwrap_or(false)
}
