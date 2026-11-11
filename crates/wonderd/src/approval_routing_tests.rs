use super::*;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn message(
    state: &AppState,
    client: &str,
    conversation: &str,
    thread: &str,
    turn: &str,
) -> wonder_store::StoredMessage {
    crate::test_support::bind_project_thread(
        state,
        conversation,
        AgentFamily::Codex,
        thread,
        None,
        "now",
    )
    .await
    .unwrap();
    let MessageInsert::Inserted(message) = state
        .store
        .insert_message("owner", client, "Work", "hash", conversation, "1")
        .await
        .unwrap()
    else {
        panic!()
    };
    state
        .store
        .update_message_delivery(&message.id, "accepted_by_codex", Some(thread), Some(turn))
        .await
        .unwrap();
    message
}
async fn approval(state: &AppState, id: &str, thread: &str, turn: &str) {
    state.store.insert_pending_approval(id,"item/commandExecution/requestApproval",&json!({"threadId":thread,"turnId":turn,"itemId":"command","conversationId":"forged-chat","cwd":"/forged/project/path","availableDecisions":["accept","decline"]}).to_string(),"1").await.unwrap();
}
async fn list(state: &AppState) -> Value {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/approvals")
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn project_approval_routes_from_exact_turn_and_preserves_nonce() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    message(
        &state,
        "direct",
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        "direct-thread",
        "direct-turn",
    )
    .await;
    approval(&state, "direct-approval", "direct-thread", "direct-turn").await;
    let before = state
        .store
        .list_pending_approvals()
        .await
        .unwrap()
        .remove(0);
    let result = list(&state).await;
    assert_eq!(
        result[0]["conversationId"],
        "d19b0f7e-97bb-45d7-8206-324a62386dd9"
    );
    assert!(result[0]["params"].get("conversationId").is_none());
    assert_eq!(result[0]["params"]["cwd"], "/forged/project/path");
    assert_eq!(result[0]["actionNonce"], before.action_nonce);
    assert_eq!(
        state
            .store
            .list_pending_approvals()
            .await
            .unwrap()
            .remove(0),
        before
    );
    state
        .store
        .register_subagent_ownership(
            "child",
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            "child-thread",
            "direct-thread",
            Some("Review"),
            None,
            None,
            "{}",
            None,
            Some(false),
            "active",
            Some(false),
            "now",
        )
        .await
        .unwrap();
    approval(&state, "child-approval", "child-thread", "child-turn").await;
    let child = list(&state)
        .await
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["approvalId"] == "child-approval")
        .unwrap()
        .clone();
    assert_eq!(
        child["conversationId"],
        "d19b0f7e-97bb-45d7-8206-324a62386dd9"
    );
    let persisted = state
        .store
        .approval("child-approval")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.thread_id, "child-thread");
    assert_eq!(persisted.turn_id, "child-turn");
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn phone_question_preserves_multiple_choice_semantics_through_redaction() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    message(
        &state,
        "question",
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        "thread",
        "turn",
    )
    .await;
    let params = json!({"threadId":"thread","turnId":"turn","questions":[
        {"id":"q","question":"Which fixtures?","multiSelect":true,"privateField":"hidden",
         "options":[{"label":"A, B"},{"label":"C"}]}]});
    state
        .store
        .insert_pending_approval(
            "question",
            "item/tool/requestUserInput",
            &params.to_string(),
            "1",
        )
        .await
        .unwrap();
    let visible = list(&state).await;
    let question = &visible[0]["params"]["questions"][0];
    assert_eq!(question["multiSelect"], true);
    assert_eq!(question["options"][0]["label"], "A, B");
    assert!(question.get("privateField").is_none());
    let reply = json!({"answers":{"q":{"answers":["A, B","C"]}}});
    assert!(validate_user_input_response(&params.to_string(), &reply).is_ok());
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_thread_turn_or_missing_mapping_never_uses_forged_route() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    message(
        &state,
        "direct",
        "actual-chat",
        "actual-thread",
        "actual-turn",
    )
    .await;
    for (id, thread, turn) in [
        ("wrong-thread", "wrong-thread", "actual-turn"),
        ("wrong-turn", "actual-thread", "wrong-turn"),
        ("unmapped", "missing-thread", "missing-turn"),
        ("empty-thread", "", "actual-turn"),
    ] {
        approval(&state, id, thread, turn).await;
    }
    let result = list(&state).await;
    assert_eq!(result, json!([]));
    assert_eq!(state.store.list_pending_approvals().await.unwrap().len(), 4);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
