//! Runtime question lifecycles. Silence never becomes an answer or permission.
use super::*;
pub(super) const OPTIONAL_QUESTION_TTL_MS: u64 = 300_000;

pub(super) fn optional_deadline(params: &serde_json::Value, now: u64) -> Option<u64> {
    (params
        .get("isBlocking")
        .and_then(serde_json::Value::as_bool)
        == Some(false))
    .then(|| {
        now.saturating_add(
            params
                .get("autoResolutionMs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(OPTIONAL_QUESTION_TTL_MS),
        )
    })
}
pub(super) fn async_questions(item: &serde_json::Value) -> Option<&serde_json::Value> {
    if item.get("type")?.as_str()? != "agentMessage" || item.get("delivery")?.as_str()? != "async" {
        return None;
    }
    let questions = item.get("questions")?;
    let values = questions.as_array()?;
    if values.is_empty()
        || values.len() > 20
        || values
            .iter()
            .any(|q| q.get("title").and_then(serde_json::Value::as_str).is_none())
    {
        return None;
    }
    Some(questions)
}

fn summary(q: wonder_store::AsyncQuestion) -> serde_json::Value {
    serde_json::json!({"id":q.id,"conversationId":q.conversation_id,"turnId":q.turn_id,"itemId":q.item_id,"questions":q.questions,"state":q.state,"response":q.response,"expiresAtMs":q.expires_at_ms})
}
pub(super) async fn list(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
) -> Response {
    match state
        .store
        .async_questions(&conversation, now_ms() as i64)
        .await
    {
        Ok(items) => Json(items.into_iter().map(summary).collect::<Vec<_>>()).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
/// Every question still waiting, across conversations (the host-wide list the
/// phone's Needs you view reads, as it does the approvals).
pub(super) async fn list_pending(State(state): State<AppState>) -> Response {
    match state.store.pending_async_questions(now_ms() as i64).await {
        Ok(items) => Json(items.into_iter().map(summary).collect::<Vec<_>>()).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Answer {
    answers: Vec<String>,
    skip: bool,
}
pub(super) async fn answer(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
    device: Option<Extension<AuthenticatedDevice>>,
    local: Option<Extension<LocalOwnerAuthority>>,
    Json(answer): Json<Answer>,
) -> Response {
    let device_id = match (device, local) {
        (Some(Extension(device)), _) => device.device_id,
        (None, Some(_)) => {
            if state
                .store
                .ensure_local_desktop(&now_ms().to_string())
                .await
                .is_err()
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            "wonder-desktop".to_owned()
        }
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let Ok(items) = state
        .store
        .async_questions(&conversation, now_ms() as i64)
        .await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(question) = items.into_iter().find(|q| q.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if state
        .store
        .owning_project_conversation(&question.runtime_conversation_id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let questions = question
        .questions
        .as_array()
        .expect("stored question array");
    if (answer.skip && !answer.answers.is_empty())
        || (!answer.skip
            && (answer.answers.len() != questions.len()
                || answer
                    .answers
                    .iter()
                    .any(|a| a.trim().is_empty() || a.len() > 8192)))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let response = serde_json::json!({"answers":answer.answers,"skip":answer.skip}).to_string();
    let body = (!answer.skip).then(|| {
        questions
            .iter()
            .zip(&answer.answers)
            .map(|(q, a)| format!("{}\n{}", q["title"].as_str().unwrap_or("Question"), a))
            .collect::<Vec<_>>()
            .join("\n\n")
    });
    if body.as_ref().is_some_and(|b| b.len() > 65536) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    match state
        .store
        .answer_async_question(
            &conversation,
            &id,
            &device_id,
            &response,
            body.as_deref(),
            now_ms() as i64,
        )
        .await
    {
        Ok(true) => {
            let _ = publish_event_with_context(
                &state,
                WonderEvent::Activity {
                    category: "conversation".into(),
                    state: "updated".into(),
                    detail: None,
                },
                EventContext {
                    conversation_id: Some(conversation),
                    ..EventContext::default()
                },
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            "This question was answered, skipped or expired. Your draft is still saved.",
        )
            .into_response(),
        Err(sqlx::Error::Protocol(message))
            if [Store::QUESTION_PARENT_UNAVAILABLE].contains(&message.as_str()) =>
        {
            (StatusCode::CONFLICT, message).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Host-owned timeouts run even while the phone is disconnected. Only an
/// explicitly nonblocking request can receive the protocol's empty answer map.
pub(super) async fn expire_optional(state: &AppState) -> Result<(), sqlx::Error> {
    let _guard = state.approval_lock.lock().await;
    for approval in state.store.list_pending_approvals().await? {
        if approval.method != "item/tool/requestUserInput" || approval.state != "pending" {
            continue;
        }
        let params: serde_json::Value =
            serde_json::from_str(&approval.params_json).unwrap_or_default();
        if params
            .get("isBlocking")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
            || !params
                .get("_wonderQuestionDeadlineMs")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|v| now_ms() >= v)
        {
            continue;
        }
        let Some(client) = state.ingestion.approval_client(&params) else {
            continue;
        };
        let runtime = client.lock().await.rpc();
        if !runtime.health().is_alive() {
            continue;
        }
        if state
            .store
            .begin_approval_resolution(&approval.approval_id)
            .await?
            .is_none()
        {
            continue;
        }
        let response = serde_json::json!({"answers":{}});
        if !state
            .store
            .persist_approval_resolution_intent(
                &approval.approval_id,
                "optional-question-expired",
                "unanswered",
                &response.to_string(),
            )
            .await?
        {
            continue;
        }
        let Some(id) = params.get("_wonderRequestId").cloned() else {
            continue;
        };
        // Ambiguous transport outcomes stay resolving and are not automatically retried.
        if runtime
            .respond_value(id, Some(response), None)
            .await
            .is_ok()
        {
            state
                .store
                .finish_approval_resolution(
                    &approval.approval_id,
                    "expired_unanswered",
                    &now_ms().to_string(),
                )
                .await?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_timeout_returns_empty_answers_once_and_never_resolves_permission_or_required_input(
    ) {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let rpc = state.projects.codex.lock().await.rpc();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, rpc.health());
        for (id, method, blocking) in [
            ("1", "item/tool/requestUserInput", false),
            ("2", "item/tool/requestUserInput", true),
            ("3", "item/commandExecution/requestApproval", false),
        ] {
            state.store.insert_pending_approval(id,method,&serde_json::json!({"threadId":"thread","turnId":"turn","itemId":id,"_wonderRuntimeId":rpc.health().id(),"_wonderRequestId":id,"isBlocking":blocking,"_wonderQuestionDeadlineMs":0,"questions":[{"id":"q","question":"Day?"}]}).to_string(),"0").await.unwrap();
        }
        expire_optional(&state).await.unwrap();
        expire_optional(&state).await.unwrap();
        let pending = state.store.list_pending_approvals().await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|a| a.approval_id != "1"));
        // An ordinary RPC acts as a stdio ordering barrier after the response.
        rpc.request("account/read", serde_json::json!({}))
            .await
            .unwrap();
        let responses = std::fs::read_to_string(dir.path().join("responses")).unwrap();
        let rows: Vec<serde_json::Value> = responses
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["result"], serde_json::json!({"answers":{}}));
    }
    #[tokio::test]
    async fn async_message_ingestion_is_durable_and_not_an_approval() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let MessageInsert::Inserted(message) = state
            .store
            .insert_message(
                "owner",
                "active",
                "work",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "0",
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let event = serde_json::json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"async","type":"agentMessage","delivery":"async","text":"Which day?","questions":[{"title":"Which day?","options":["Friday","Saturday"]}]}}});
        assert!(process_app_server_notification(&state, event.clone(), true).await);
        assert!(process_app_server_notification(&state, event, true).await);
        let items = state
            .store
            .async_questions("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].questions[0]["options"][1], "Saturday");
        assert!(state
            .store
            .list_pending_approvals()
            .await
            .unwrap()
            .is_empty());
    }
    #[tokio::test]
    async fn desktop_answers_require_local_authority_and_replay_once() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        state
            .store
            .save_async_question(
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "thread",
                "turn",
                "question",
                r#"[{"title":"Which day?"}]"#,
                now_ms() as i64 + 60_000,
            )
            .await
            .unwrap();
        let id = state
            .store
            .async_questions("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap()[0]
            .id
            .clone();
        let response = answer(
            State(state.clone()),
            Path(("d19b0f7e-97bb-45d7-8206-324a62386dd9".into(), id.clone())),
            None,
            None,
            Json(Answer {
                answers: vec!["Friday".into()],
                skip: false,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        for _ in 0..2 {
            let response = answer(
                State(state.clone()),
                Path(("d19b0f7e-97bb-45d7-8206-324a62386dd9".into(), id.clone())),
                None,
                Some(Extension(LocalOwnerAuthority)),
                Json(Answer {
                    answers: vec!["Friday".into()],
                    skip: false,
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        let messages = state
            .store
            .messages_for_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].device_id, "wonder-desktop");
        let response = answer(
            State(state.clone()),
            Path(("d19b0f7e-97bb-45d7-8206-324a62386dd9".into(), id)),
            None,
            Some(Extension(LocalOwnerAuthority)),
            Json(Answer {
                answers: vec![],
                skip: true,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
    #[test]
    fn only_explicitly_optional_questions_expire() {
        assert_eq!(
            optional_deadline(&serde_json::json!({"isBlocking":false}), 10),
            Some(300010)
        );
        assert_eq!(
            optional_deadline(
                &serde_json::json!({"isBlocking":true,"autoResolutionMs":1}),
                10
            ),
            None
        );
        assert_eq!(
            optional_deadline(&serde_json::json!({"autoResolutionMs":1}), 10),
            None
        );
        assert!(async_questions(&serde_json::json!({"type":"agentMessage","delivery":"async","questions":[{"title":"Which day?"}]})).is_some());
        assert!(async_questions(
            &serde_json::json!({"type":"agentMessage","questions":[{"title":"Which day?"}]})
        )
        .is_none());
    }
}
