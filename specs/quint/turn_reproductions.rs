// Included in update_handoff::tests to reuse its isolated SQLite + subprocess
// fixture. No live provider. These CHARACTERIZE defects; the desired contracts
// fail separately in counterexamples.qnt. Remove/flip the defect assertions when
// the owning implementation is fixed, rather than blessing them as product rules.

async fn quint_stop(state: &AppState) -> axum::response::Response {
    crate::interrupt_turn(
        axum::extract::State(state.clone()),
        axum::extract::Path((
            "d19b0f7e-97bb-45d7-8206-324a62386dd9".to_owned(),
            "old".to_owned(),
        )),
    )
    .await
}

// QT-1: a SUCCESSFUL Stop can disappear before the stopped turn settles.
#[tokio::test]
async fn quint_repro_expired_stop_continues_after_restart() {
    let (dir, state, message) = fixture().await;
    state
        .store
        .set_continuation_settings(None, Some(true))
        .await
        .unwrap();
    let response = quint_stop(&state).await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    assert!(state
        .store
        .turn_stop_recorded("thread", "old")
        .await
        .unwrap());
    // No terminal notification was ingested; the receipt is still active.
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
    // Virtual wall time: >7 days later another thread saves Stop before this
    // turn's recovery. Exercise the real retention SQL, not manual DELETE.
    let later = chrono::Utc::now().timestamp() + 7 * 24 * 3600 + 1;
    state
        .store
        .record_turn_stop("other-thread", "other-turn", later)
        .await
        .unwrap();
    state.projects.forget_cached_stops();
    restart(&state).await;
    assert!(!state
        .store
        .turn_stop_recorded("thread", "old")
        .await
        .unwrap());
    crate::projects::recover(&state).await;
    let handoffs = state.store.pending_update_handoffs().await.unwrap();
    recover(&state).await.unwrap();
    let submitted = starts(&dir);
    state.projects.codex.lock().await.shutdown().await.unwrap();
    assert_eq!(
        handoffs.len(),
        1,
        "QT-1: the expired Stop is treated as a crash"
    );
    assert_eq!(
        submitted.len(),
        1,
        "QT-1: reproduce the unwanted continuation"
    );
    assert_eq!(submitted[0]["input"][0]["text"], CONTINUE_AFTER_RESTART);
    assert_ne!(submitted[0]["clientUserMessageId"], "original");
    eprintln!("QT-1 measured: Stop HTTP 202; retention removed unresolved Stop; restart sent 1 continuation");
}

// QT-2: transport failure is not evidence that the provider refused Stop.
#[tokio::test]
async fn quint_repro_lost_stop_response_continues_after_restart() {
    let (dir, state, _) = fixture().await;
    state
        .store
        .set_continuation_settings(None, Some(true))
        .await
        .unwrap();
    fs::write(dir.path().join("lose-stop-response"), "").unwrap();
    let response = quint_stop(&state).await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let native: Value =
        serde_json::from_slice(&fs::read(dir.path().join("update-fixture.json")).unwrap()).unwrap();
    assert_eq!(
        native["thread"][0]["status"], "interrupted",
        "the provider took Stop before exiting without its response"
    );
    assert!(!state
        .store
        .turn_stop_recorded("thread", "old")
        .await
        .unwrap());
    fs::remove_file(dir.path().join("lose-stop-response")).unwrap();
    state.projects.forget_cached_stops();
    restart(&state).await;
    crate::projects::recover(&state).await;
    let handoffs = state.store.pending_update_handoffs().await.unwrap();
    recover(&state).await.unwrap();
    let submitted = starts(&dir);
    state.projects.codex.lock().await.shutdown().await.unwrap();
    assert_eq!(
        handoffs.len(),
        1,
        "QT-2: the lost acknowledgement erased Stop"
    );
    assert_eq!(
        submitted.len(),
        1,
        "QT-2: reproduce the unwanted continuation"
    );
    assert_eq!(submitted[0]["input"][0]["text"], CONTINUE_AFTER_RESTART);
    eprintln!("QT-2 measured: provider interrupted; Stop HTTP 503 erased persistence; restart sent 1 continuation");
}

// SB-1: the early abstraction incorrectly let uncertain receipts continue.
#[tokio::test]
async fn quint_unknown_recovery_does_not_continue_an_uncertain_receipt() {
    let (dir, state, message) = fixture().await;
    state
        .store
        .set_continuation_settings(None, Some(true))
        .await
        .unwrap();
    crate::projects::request_stop(&state, "thread", "old")
        .await
        .unwrap();
    state.projects.forget_cached_stops();
    restart(&state).await;
    // Still reads inProgress after runtime loss: recover marks it uncertain.
    crate::projects::recover(&state).await;
    assert_eq!(
        state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    assert!(!state
        .store
        .turn_stop_recorded("thread", "old")
        .await
        .unwrap());
    fs::write(
        dir.path().join("update-fixture.json"),
        json!({"thread":[{"id":"old","status":"interrupted"}]}).to_string(),
    )
    .unwrap();
    crate::projects::recover(&state).await;
    assert!(state
        .store
        .pending_update_handoffs()
        .await
        .unwrap()
        .is_empty());
    recover(&state).await.unwrap();
    let submitted = starts(&dir);
    state.projects.codex.lock().await.shutdown().await.unwrap();
    assert!(
        submitted.is_empty(),
        "SB-1: uncertain must not schedule a continuation"
    );
    eprintln!("SB-1 measured: unknown -> uncertain -> interrupted; 0 continuations");
}

// QT-3: unresolved submission intent is excluded from both queue head and busy
// checks. A later queued message can run without the context of its predecessor.
#[tokio::test]
async fn quint_repro_later_queue_message_overtakes_uncertain_submission() {
    let (dir, state, _) = fixture().await;
    // Handoff recovery uses frozen settings; new dispatch needs discovery too.
    // Recovery may release an idle runtime and its catalog. Serve the model
    // page on every fake restart, rather than injecting an ephemeral catalog.
    fs::write(
        dir.path().join("models.json"),
        json!({"data":[{"id":"fake","model":"fake","displayName":"Fake","isDefault":true}]})
            .to_string(),
    )
    .unwrap();
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    // Fresh dispatch validates native cwd against the owned project roots.
    // The handoff fixture otherwise omits cwd because handoffs freeze it.
    let workspace = state
        .store
        .project_conversation(conversation)
        .await
        .unwrap()
        .unwrap()
        .cwd;
    fs::write(dir.path().join("update-cwd"), workspace).unwrap();
    let mut queued = Vec::new();
    for client in ["first-queued", "second-queued"] {
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                client,
                client,
                client,
                conversation,
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("queued message");
        };
        queued.push(message);
    }
    fs::write(
        dir.path().join("update-fixture.json"),
        json!({"thread":[{"id":"old","status":"completed"}]}).to_string(),
    )
    .unwrap();
    crate::projects::recover(&state).await;
    assert!(state
        .store
        .claim_message_for_dispatch(&queued[0].id)
        .await
        .unwrap());
    state
        .store
        .begin_dispatch_submission_with_context(&queued[0].id, "thread", None)
        .await
        .unwrap();
    // Crash before turn/start bytes. This is exactly the store's startup hook.
    state.store.recover_dispatch_claims().await.unwrap();
    let ready = state.store.pending_dispatch_messages().await.unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, queued[1].id);
    let _runtime = crate::projects::codex_rpc(&state).await.unwrap();
    crate::projects::dispatch(state.clone(), ready[0].clone()).await;
    let submitted = starts(&dir);
    let first = state
        .store
        .message_by_id(&queued[0].id)
        .await
        .unwrap()
        .unwrap();
    let second = state
        .store
        .message_by_id(&queued[1].id)
        .await
        .unwrap()
        .unwrap();
    state.projects.codex.lock().await.shutdown().await.unwrap();
    assert_eq!(first.state, "uncertain");
    assert!(first.codex_turn_id.is_none());
    assert_eq!(
        second.state,
        "accepted_by_codex",
        "QT-3 dispatch failed: {:?}",
        state
            .store
            .message_delivery_error(&second.id)
            .await
            .unwrap()
    );
    assert_eq!(
        submitted.len(),
        1,
        "QT-3: only the later queued input was sent"
    );
    assert_eq!(submitted[0]["clientUserMessageId"], "second-queued");
    eprintln!("QT-3 measured: first queued input uncertain with no receipt; second queued input reached turn/start");
}

// QT-4: terminal settlement clears Stop while an unbound Guide receipt can
// still resolve to that stopped Codex turn and become active again.
#[tokio::test]
async fn quint_repro_late_guide_receipt_continues_a_settled_stopped_turn() {
    let (dir, state, original) = fixture().await;
    state
        .store
        .set_continuation_settings(None, Some(true))
        .await
        .unwrap();
    let MessageInsert::Inserted(guide) = state
        .store
        .insert_guide_message(
            "owner",
            "late-guide",
            "additional instruction",
            "guide-hash",
            &original.conversation_id,
            &[],
            "now",
            "old",
        )
        .await
        .unwrap()
    else {
        panic!("Guide");
    };
    assert!(state.store.claim_guide(&guide.id, "thread").await.unwrap());
    // The Guide was taken into Codex's existing turn, but its reply and native
    // client-ID echo are initially unavailable. Turn status remains readable.
    assert_eq!(
        quint_stop(&state).await.status(),
        axum::http::StatusCode::ACCEPTED
    );
    state.projects.forget_cached_stops();
    restart(&state).await;
    crate::projects::recover(&state).await;
    assert_eq!(
        state
            .store
            .message_by_id(&original.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "completed"
    );
    assert!(!state
        .store
        .turn_stop_recorded("thread", "old")
        .await
        .unwrap());
    let pending = state.store.message_by_id(&guide.id).await.unwrap().unwrap();
    assert_eq!(pending.state, "uncertain");
    assert!(pending.codex_turn_id.is_none());
    // The late native echo identifies the SAME stopped turn. Execute the real
    // Guide reconciliation owner after obtaining that proof; no 60s scan-backoff
    // sleep is needed to exercise its binding/recovery contract.
    fs::write(
        dir.path().join("update-fixture.json"),
        json!({"thread":[{
            "id":"old", "status":"interrupted", "clientId":"late-guide"
        }]})
        .to_string(),
    )
    .unwrap();
    // Settlement can release the now-idle runtime. Use the real lazy-reopen
    // owner for the later native-history read, not a handle to its old process.
    let rpc = crate::projects::codex_rpc(&state).await.unwrap();
    let page = rpc
        .request("thread/items/list", json!({"threadId":"thread"}))
        .await
        .unwrap()
        .result
        .unwrap();
    let found = crate::find_client_message(&page, "late-guide").unwrap();
    assert_eq!(found, "old");
    assert!(crate::accept_reconciled_guide(&state, &pending, "thread", &found).await);
    crate::projects::recover(&state).await;
    let handoffs = state.store.pending_update_handoffs().await.unwrap();
    recover(&state).await.unwrap();
    let submitted = starts(&dir);
    state.projects.codex.lock().await.shutdown().await.unwrap();
    assert_eq!(
        handoffs.len(),
        1,
        "QT-4: late receipt revives the settled turn"
    );
    assert_eq!(handoffs[0].message_id.as_deref(), Some(guide.id.as_str()));
    assert_eq!(
        submitted.len(),
        1,
        "QT-4: stopped work was continued through its Guide receipt"
    );
    assert_eq!(submitted[0]["input"][0]["text"], CONTINUE_AFTER_RESTART);
    eprintln!("QT-4 measured: acknowledged Stop settled; late Guide receipt rebound stopped turn; sent 1 continuation");
}
