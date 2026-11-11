//! Owner/authentication and wire contract for permanent Cupola chat deletion.
use super::*;
use serde_json::Value;
use tower::ServiceExt;

async fn delete(state: &AppState, id: &str, owner: bool) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/project-conversations/{id}"));
    if owner {
        request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
    }
    let response = router(state.clone())
        .oneshot(request.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned())),
    )
}

#[tokio::test]
async fn delete_requires_owner_refuses_work_and_returns_a_retryable_receipt() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        crate::test_support::project_thread(
            &state.store,
            "delete-chat",
            "Delete fixture",
            state.data_dir.to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        assert_ne!(delete(&state, "delete-chat", false).await.0, StatusCode::OK);
        assert!(state
            .store
            .project_conversation("delete-chat")
            .await
            .unwrap()
            .is_some());
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_message(
                "owner",
                "delete-prompt",
                "Saved message",
                "hash",
                "delete-chat",
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("new fixture prompt")
        };
        assert_eq!(
            delete(&state, "delete-chat", true).await.0,
            StatusCode::CONFLICT
        );
        assert!(state
            .store
            .project_conversation("delete-chat")
            .await
            .unwrap()
            .is_some());
        state
            .store
            .update_message_delivery(&message.id, "completed", None, None)
            .await
            .unwrap();
        let (status, receipt) = delete(&state, "delete-chat", true).await;
        if status != StatusCode::OK {
            let storage = state
                .store
                .delete_project_chat("delete-chat", "retry")
                .await;
            panic!("Delete response {status}: {receipt}; storage: {storage:?}");
        }
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(
            receipt,
            serde_json::json!({"conversationId":"delete-chat","deleted":true})
        );
        assert_eq!(delete(&state, "delete-chat", true).await, (status, receipt));
        assert_eq!(
            delete(&state, "unknown", true).await.0,
            StatusCode::NOT_FOUND
        );
        assert!(state
            .store
            .project_conversation("delete-chat")
            .await
            .unwrap()
            .is_none());
        assert!(state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .is_none());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    })
    .await
    .expect("delete route must finish within its fixture budget");
}
