//! Edit from here against the fake Codex runtime and the fake Claude bridge:
//! what each provider is asked to do, what Cupola refuses, and what Cupola's
//! own history keeps afterwards.
use crate::provider_switch_tests::{fx, CLAUDE_SESSION, CLAUDE_THREAD};
use crate::rewind::{rewind, RewindRequest};
use crate::OwnerAuthority;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    Extension, Json,
};
use serde_json::{json, Value};
use wonder_store::{AgentFamily, ProviderSetting};

async fn body(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

fn requests(dir: &std::path::Path, log: &str, method: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(log))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|request| request["method"] == method)
        .collect()
}

async fn call(
    state: &crate::AppState,
    chat: &str,
    turn: &str,
    restore: bool,
) -> (StatusCode, Value) {
    body(
        rewind(
            State(state.clone()),
            Extension(OwnerAuthority),
            Path(chat.to_owned()),
            Json(
                serde_json::from_value::<RewindRequest>(
                    json!({"turnId": turn, "restoreFiles": restore}),
                )
                .unwrap(),
            ),
        )
        .await,
    )
    .await
}

// Contract: Codex is asked to revert exactly before the owner's turn, only
// when the chat is idle and the turn is in its history; the removed turns
// leave Cupola's history, files are never claimed restored, and dispatch
// holds new messages while a rewind runs.
#[tokio::test]
async fn codex_rewind_reverts_native_history_and_hides_the_removed_turns() {
    let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
    let reverts = || requests(dir.path(), "requests-jsonl", "thread/revert");

    let (status, reply) = call(&state, "project-chat", "turn", false).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(reply.to_string().contains("still working"), "{reply}");
    state
        .store
        .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
        .await
        .unwrap();

    let (status, reply) = call(&state, "project-chat", "turn", true).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert!(reply.to_string().contains("stay as they are"), "{reply}");

    std::fs::write(
        dir.path().join("turn-pages.json"),
        json!([[{"id":"later","status":"completed"}],
            [{"id":"turn","status":"completed"},{"id":"earlier","status":"completed"}]])
        .to_string(),
    )
    .unwrap();
    let (status, reply) = call(&state, "project-chat", "missing", false).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(reply.to_string().contains("no longer in this chat"));
    assert!(reverts().is_empty());

    std::fs::write(dir.path().join("revert-error"), "").unwrap();
    let (status, _) = call(&state, "project-chat", "turn", false).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    std::fs::remove_file(dir.path().join("revert-error")).unwrap();
    let unchanged = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, "project-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (unchanged.history_revision, unchanged.messages.len()),
        (2, 1)
    );

    // While a rewind runs, the chat's messages wait and a second rewind is refused.
    assert!(state.projects.begin_rewind("project-chat"));
    assert!(crate::projects::held(&state, "project-chat", None).await);
    let (status, _) = call(&state, "project-chat", "turn", false).await;
    assert_eq!(status, StatusCode::CONFLICT);
    state.projects.end_rewind("project-chat");

    let (status, reply) = call(&state, "project-chat", "turn", false).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    crate::tests::validate_http_contract("rewindConversationResponse", &reply);
    assert_eq!(
        reply,
        json!({"historyRevision": 4, "removedTurns": 2, "filesRestored": false})
    );
    let sent = reverts();
    assert_eq!(sent.len(), 2, "the failed attempt and this one");
    assert_eq!(
        sent[1]["params"],
        json!({"threadId": "thread", "beforeTurnId": "turn"})
    );
    let after = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, "project-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.history_revision, 4);
    assert!(
        after.messages.is_empty(),
        "the rewound message left history"
    );
    assert!(!crate::projects::held(&state, "project-chat", None).await);
}

// Contract: Claude's rewind goes through the bridge with the owner's file
// choice; restoring files needs the owner's setting, which every later
// Claude turn then carries; the bridge's refusal reaches the owner as is; a
// rewind of the first message moves the chat to the bridge's fresh session.
#[tokio::test]
async fn claude_rewind_goes_through_the_bridge_and_needs_the_backup_setting() {
    let fx = fx().await;
    let first = fx
        .queue("first", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    let (thread, first_turn, _) = fx.release(&first).await.unwrap();
    assert_eq!(thread, CLAUDE_THREAD);
    let finish = |message: wonder_store::StoredMessage, turn: String| {
        let store = fx.state.store.clone();
        async move {
            store
                .update_message_delivery(&message.id, "completed", Some(CLAUDE_THREAD), Some(&turn))
                .await
                .unwrap();
        }
    };
    finish(first.clone(), first_turn.clone()).await;
    fx.write(
        "claude-turns.json",
        &json!([{ "id":first_turn, "status":"completed" }]),
    );
    let starts = || fx.claude_requests("turn/start");
    assert!(
        starts()[0]["params"].get("fileCheckpointing").is_none(),
        "off by default"
    );

    let (status, reply) = call(&fx.state, &fx.chat, &first_turn, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply.to_string().contains("restore files when you rewind"),
        "{reply}"
    );
    assert!(fx.claude_requests("thread/rewind").is_empty());

    fx.state
        .store
        .set_provider_setting(
            AgentFamily::Claude,
            ProviderSetting::FileCheckpointing,
            true,
            "now",
        )
        .await
        .unwrap();
    let second = fx.queue("second", None).await;
    let (_, second_turn, _) = fx.release(&second).await.unwrap();
    finish(second.clone(), second_turn.clone()).await;
    fx.write("claude-turns.json", &json!([{ "id":second_turn, "status":"completed" }, { "id":first_turn, "status":"completed" }]));
    assert_eq!(starts()[1]["params"]["fileCheckpointing"], true);

    std::fs::write(
        fx.dir.path().join("claude-rewind-refused"),
        "Wait for Claude to finish before editing this conversation.",
    )
    .unwrap();
    let (status, reply) = call(&fx.state, &fx.chat, &second_turn, true).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        reply.to_string().contains("Wait for Claude to finish"),
        "{reply}"
    );
    std::fs::remove_file(fx.dir.path().join("claude-rewind-refused")).unwrap();

    // A bridge cannot report restored files the owner did not ask for.
    fx.write(
        "claude-rewind.json",
        &json!({"removedTurnIds": [second_turn], "filesRestored": true}),
    );
    let (status, _) = call(&fx.state, &fx.chat, &second_turn, false).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    fx.write(
        "claude-rewind.json",
        &json!({"removedTurnIds": [second_turn], "filesRestored": true, "filesChanged": 2}),
    );
    let (status, reply) = call(&fx.state, &fx.chat, &second_turn, true).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    crate::tests::validate_http_contract("rewindConversationResponse", &reply);
    assert_eq!(reply["filesRestored"], true);
    assert_eq!(reply["filesChanged"], 2);
    assert_eq!(
        fx.claude_requests("thread/rewind").last().unwrap()["params"],
        json!({"threadId": CLAUDE_THREAD, "turnId": second_turn, "restoreFiles": true})
    );
    let page = fx
        .state
        .store
        .committed_conversation_snapshot(&fx.state.host_epoch, &fx.chat)
        .await
        .unwrap()
        .unwrap();
    let bodies: Vec<_> = page.messages.iter().map(|m| m.body.as_str()).collect();
    assert!(
        bodies.contains(&"first") && !bodies.contains(&"second"),
        "{bodies:?}"
    );

    fx.write(
        "claude-turns.json",
        &json!([{ "id":first_turn, "status":"completed" }]),
    );
    let fresh = "99999999-8888-4777-8666-555555555555";
    fx.write(
        "claude-rewind.json",
        &json!({"removedTurnIds": [first_turn], "filesRestored": false, "sessionId": fresh}),
    );
    let (status, reply) = call(&fx.state, &fx.chat, &first_turn, false).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let stored = fx
        .state
        .store
        .project_conversation(&fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.native_session_id.as_deref(), Some(fresh));
    assert_ne!(stored.native_session_id.as_deref(), Some(CLAUDE_SESSION));
    assert_eq!(reply["historyRevision"], 8);
}

// Contract: Settings lists exactly the providers that can restore files,
// each off until the owner turns it on, and refuses the others.
#[tokio::test]
async fn file_restore_setting_lists_capable_providers_and_persists() {
    use crate::rewind::{file_restore, set_file_restore, SetFileRestore};
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let (status, listed) =
        body(file_restore(State(state.clone()), Extension(OwnerAuthority)).await).await;
    assert_eq!(status, StatusCode::OK);
    crate::tests::validate_http_contract("fileRestoreSettings", &listed);
    assert_eq!(
        listed,
        json!({"providers": [{"family": "claude", "enabled": false}]})
    );
    let set = |family: AgentFamily, enabled: bool| {
        let state = state.clone();
        async move {
            body(
                set_file_restore(
                    State(state),
                    Extension(OwnerAuthority),
                    Path(family),
                    Json(
                        serde_json::from_value::<SetFileRestore>(json!({"enabled": enabled}))
                            .unwrap(),
                    ),
                )
                .await,
            )
            .await
        }
    };
    let (status, _) = set(AgentFamily::Codex, true).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, listed) = set(AgentFamily::Claude, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed,
        json!({"providers": [{"family": "claude", "enabled": true}]})
    );
    assert!(crate::rewind::checkpointing(&state, AgentFamily::Claude).await);
    assert!(!crate::rewind::checkpointing(&state, AgentFamily::Codex).await);
}

async fn journal_fixture() -> (tempfile::TempDir, crate::AppState) {
    let (dir, state, message) = crate::projects::tests::handoff_fixture().await;
    state
        .store
        .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
        .await
        .unwrap();
    let wonder_store::MessageInsert::Inserted(earlier) = state
        .store
        .insert_dispatch_message(
            "owner",
            "earlier-receipt",
            "kept history",
            "kept-hash",
            "project-chat",
            &[],
            "2000-01-01T00:00:00.000Z",
            true,
        )
        .await
        .unwrap()
    else {
        panic!("earlier fixture receipt");
    };
    state
        .store
        .update_message_delivery(&earlier.id, "completed", Some("thread"), Some("earlier"))
        .await
        .unwrap();
    std::fs::write(dir.path().join("rewind-journal-fixture"), "").unwrap();
    std::fs::write(
        dir.path().join("turn-pages.json"),
        json!([[{"id":"turn","status":"completed"},{"id":"earlier","status":"completed"}]])
            .to_string(),
    )
    .unwrap();
    (dir, state)
}

async fn reached(dir: &std::path::Path, marker: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !dir.join(marker).exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native app-server fixture reached the crash checkpoint");
}

async fn reopen_journal_fixture(
    dir: &std::path::Path,
    mut state: crate::AppState,
) -> crate::AppState {
    state.projects.codex.lock().await.shutdown().await.unwrap();
    state.store =
        wonder_store::Store::connect(&format!("sqlite://{}", dir.join("state.db").display()))
            .await
            .unwrap();
    state.projects = crate::projects::ProjectRuntime::configured(
        dir.join("codex"),
        "test".into(),
        &dir.join("home"),
        &dir.join("claude"),
        crate::ingestion::notification_sink(state.store.clone()),
    );
    state
}

async fn assert_journal_fences(state: &crate::AppState) {
    let response = crate::history::conversation_snapshot(
        State(state.clone()),
        Path("project-chat".into()),
        axum::extract::Query(serde_json::from_value(json!({})).unwrap()),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the native client's history endpoint must not serve stale rows"
    );
    let response = crate::send_message(State(state.clone()), Path("project-chat".into()), None, None,
        Json(serde_json::from_value(json!({"deviceId":"owner", "clientMessageId":"ddeeff00-1234-4567-8888-999999999999", "body":"new work during recovery"})).unwrap())).await;
    let (status, detail) = body(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{detail}");
    assert!(
        detail.to_string().contains("recovering this edit")
            && !detail.to_string().contains("Rewind recovery is pending"),
        "the owner reads a plain state, not store text: {detail}"
    );
    assert!(
        state
            .store
            .committed_conversation_snapshot(&state.host_epoch, "project-chat")
            .await
            .is_err(),
        "unreconciled provider history must never return a stale snapshot"
    );
    assert!(
        state
            .store
            .insert_dispatch_message(
                "owner",
                "after-rewind",
                "new work",
                "new-hash",
                "project-chat",
                &[],
                "later",
                true
            )
            .await
            .is_err(),
        "no new work may be accepted against the stale state"
    );
}

async fn assert_journal_recovered(dir: &std::path::Path, state: &crate::AppState) {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::projects::recover(state),
    )
    .await
    .unwrap();
    let page = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, "project-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.messages
            .iter()
            .map(|m| (m.codex_turn_id.as_deref(), m.body.as_str()))
            .collect::<Vec<_>>(),
        [(Some("earlier"), "kept history")],
        "the local projection exactly matches the provider's retained turn"
    );
    assert!(page.history_revision > 0);
    let native: Value =
        serde_json::from_slice(&std::fs::read(dir.join("turn-pages.json")).unwrap()).unwrap();
    assert_eq!(native, json!([[{"id":"earlier","status":"completed"}]]));
    assert_eq!(
        requests(dir, "requests-jsonl", "thread/revert").len(),
        1,
        "recovery never replays a provider mutation"
    );
    assert!(matches!(
        state
            .store
            .insert_dispatch_message(
                "owner",
                "after-rewind",
                "new work",
                "new-hash",
                "project-chat",
                &[],
                "later",
                true
            )
            .await
            .unwrap(),
        wonder_store::MessageInsert::Inserted(_)
    ));
}

// Native provider success followed by an actual failing SQLite history write.
// Both the current service and a newly opened store must refuse stale reads
// and sends, then the normal recovery loop settles without a second revert.
#[tokio::test]
async fn rewind_store_failure_survives_restart() {
    let (dir, state) = journal_fixture().await;
    std::fs::write(dir.path().join("rewind-fail-store"), "").unwrap();
    let (status, _) = call(&state, "project-chat", "turn", false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_journal_fences(&state).await;
    let state = reopen_journal_fixture(dir.path(), state).await;
    assert_journal_fences(&state).await;
    // Recovery while the write still fails must retain the journal.
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::projects::recover(&state),
    )
    .await
    .unwrap();
    assert_journal_fences(&state).await;
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER rewind_write_fault")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_journal_recovered(dir.path(), &state).await;
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// Crash after native durable mutation but before the reply reaches wonderd:
// no response data can help recovery. The before-image has to survive alone.
#[tokio::test]
async fn rewind_crash_after_provider_write_survives_restart() {
    let (dir, state) = journal_fixture().await;
    std::fs::write(dir.path().join("rewind-wait-after"), "").unwrap();
    let request_state = state.clone();
    let task =
        tokio::spawn(async move { call(&request_state, "project-chat", "turn", false).await });
    reached(dir.path(), "rewind-applied").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let state = reopen_journal_fixture(dir.path(), state).await;
    assert_journal_fences(&state).await;
    assert_journal_recovered(dir.path(), &state).await;
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn rewind_crash_before_provider_write_rolls_back_after_restart() {
    let (dir, state) = journal_fixture().await;
    std::fs::write(dir.path().join("rewind-wait-before"), "").unwrap();
    let request_state = state.clone();
    let task =
        tokio::spawn(async move { call(&request_state, "project-chat", "turn", false).await });
    reached(dir.path(), "rewind-entered").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let state = reopen_journal_fixture(dir.path(), state).await;
    assert_journal_fences(&state).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::projects::recover(&state),
    )
    .await
    .unwrap();
    let page = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, "project-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        page.messages.len(),
        2,
        "the provider never changed its history"
    );
    assert!(matches!(
        state
            .store
            .insert_dispatch_message(
                "owner",
                "after-rewind",
                "new work",
                "new-hash",
                "project-chat",
                &[],
                "later",
                true
            )
            .await
            .unwrap(),
        wonder_store::MessageInsert::Inserted(_)
    ));
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// The recovery owner must distinguish an unexecuted intent from a native
// durable cut. The separate restart cases exercise its production caller.
#[tokio::test]
async fn rewind_recovery_owner_handles_interrupted_native_operations() {
    for applied in [false, true] {
        let (dir, state) = journal_fixture().await;
        let checkpoint = if applied {
            "rewind-wait-after"
        } else {
            "rewind-wait-before"
        };
        std::fs::write(dir.path().join(checkpoint), "").unwrap();
        let request_state = state.clone();
        let task =
            tokio::spawn(async move { call(&request_state, "project-chat", "turn", false).await });
        reached(
            dir.path(),
            if applied {
                "rewind-applied"
            } else {
                "rewind-entered"
            },
        )
        .await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            crate::rewind::recover(&state),
        )
        .await
        .unwrap();
        assert_journal_fences(&state).await; // The abandoned RPC may still be writing.
        let state = reopen_journal_fixture(dir.path(), state).await;
        assert_journal_fences(&state).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            crate::rewind::recover(&state),
        )
        .await
        .unwrap();
        let page = state
            .store
            .committed_conversation_snapshot(&state.host_epoch, "project-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.messages.len(), 1 + usize::from(!applied));
        if applied {
            assert_eq!(page.messages[0].codex_turn_id.as_deref(), Some("earlier"));
        }
        assert!(state.store.pending_rewinds().await.unwrap().is_empty());
        assert_eq!(
            requests(dir.path(), "requests-jsonl", "thread/revert").len(),
            1
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}

/// A pending journal for the fixture chat whose own provider call is over:
/// the runtime id is the live one and the call ended long ago.
async fn quiet_operation(state: &crate::AppState) -> wonder_store::RewindOperation {
    let rpc = crate::claude::for_thread(state, "thread").await.unwrap();
    let operation = wonder_store::RewindOperation {
        id: "operation".into(),
        conversation: "project-chat".into(),
        thread: "thread".into(),
        native_session: "thread".into(),
        runtime_id: rpc.health().id().to_owned(),
        turn: "turn".into(),
        turns: vec!["turn".into(), "earlier".into()],
        ..Default::default()
    };
    state
        .store
        .begin_project_rewind(&operation, &crate::now_ms().to_string())
        .await
        .unwrap();
    operation
}

async fn recover_now(state: &crate::AppState) {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::rewind::recover(state),
    )
    .await
    .unwrap();
}

async fn pending(state: &crate::AppState) -> usize {
    state.store.pending_rewinds().await.unwrap().len()
}

// Client disconnect mid-rewind (the provider call is in flight) must not
// strand the chat: the detached job finishes the call and settles by itself,
// with no restart and no second revert.
#[tokio::test]
async fn rewind_survives_client_disconnect_without_restart() {
    let (dir, state) = journal_fixture().await;
    std::fs::write(dir.path().join("rewind-wait-after"), "").unwrap();
    let request_state = state.clone();
    let task =
        tokio::spawn(async move { call(&request_state, "project-chat", "turn", false).await });
    reached(dir.path(), "rewind-applied").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_journal_fences(&state).await;
    std::fs::remove_file(dir.path().join("rewind-wait-after")).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while pending(&state).await != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the job settles after its handler is gone");
    assert_journal_recovered(dir.path(), &state).await;
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// A provider call that ended without a reply (timeout/transport loss) on a
// runtime that is still the same: recovery waits out the late-apply grace,
// then reads the provider instead of staying fenced until a restart.
#[tokio::test]
async fn rewind_call_that_ended_without_a_reply_recovers_after_the_grace() {
    let (dir, state) = journal_fixture().await;
    let operation = quiet_operation(&state).await;
    std::fs::write(
        dir.path().join("turn-pages.json"),
        json!([[{"id":"earlier","status":"completed"}]]).to_string(),
    )
    .unwrap();
    state
        .store
        .record_rewind_call_end(&operation, &crate::now_ms().to_string())
        .await
        .unwrap();
    recover_now(&state).await;
    assert_eq!(pending(&state).await, 1, "a fresh call end may still apply");
    state
        .store
        .record_rewind_call_end(&operation, "1")
        .await
        .unwrap();
    recover_now(&state).await;
    assert_eq!(pending(&state).await, 0);
    let page = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, "project-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// The owner kept using the thread in the provider's own app while Cupola was
// fenced. Every shape of native history resolves to the provider's truth.
#[tokio::test]
async fn rewind_fence_resolves_when_the_owner_changed_native_history() {
    // (provider turns newest first, visible Cupola turns afterwards)
    let cases: [(&[&str], &[&str]); 4] = [
        (&["new", "turn", "earlier"], &["turn", "earlier"]), // never applied, then continued
        (&["new", "earlier"], &["earlier"]),                 // applied, then continued
        (&["other"], &[]),                                   // replaced entirely
        (&["new", "turn"], &["turn"]),                       // partly different cut
    ];
    for (provider, visible) in cases {
        let (dir, state) = journal_fixture().await;
        let operation = quiet_operation(&state).await;
        state
            .store
            .record_rewind_call_end(&operation, "1")
            .await
            .unwrap();
        let turns: Vec<Value> = provider
            .iter()
            .map(|id| json!({"id":id,"status":"completed"}))
            .collect();
        std::fs::write(
            dir.path().join("turn-pages.json"),
            json!([turns]).to_string(),
        )
        .unwrap();
        recover_now(&state).await;
        assert_eq!(pending(&state).await, 0, "{provider:?}");
        let page = state
            .store
            .committed_conversation_snapshot(&state.host_epoch, "project-chat")
            .await
            .unwrap()
            .unwrap();
        let mut shown: Vec<_> = page
            .messages
            .iter()
            .filter_map(|m| m.codex_turn_id.as_deref())
            .collect();
        shown.sort();
        let mut expected = visible.to_vec();
        expected.sort();
        assert_eq!(shown, expected, "{provider:?}");
        assert_eq!(
            requests(dir.path(), "requests-jsonl", "thread/revert").len(),
            0,
            "recovery never repeats the rewind"
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}

// Reading many native pages must not stall updates or message dispatch.
#[tokio::test]
async fn rewind_recovery_reads_without_update_admission_or_dispatch() {
    let (dir, state) = journal_fixture().await;
    let operation = quiet_operation(&state).await;
    state
        .store
        .record_rewind_call_end(&operation, "1")
        .await
        .unwrap();
    std::fs::write(dir.path().join("turns-list-wait"), "").unwrap();
    let recovery_state = state.clone();
    let recovery = tokio::spawn(async move { crate::rewind::recover(&recovery_state).await });
    reached(dir.path(), "turns-list-entered").await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let _admission = state.update_admission.claim_guard().await.unwrap();
        let _dispatch = state
            .update_admission
            .dispatch_guard(&state.dispatch_lock)
            .await;
    })
    .await
    .expect("a slow native read must not hold update admission or dispatch");
    std::fs::remove_file(dir.path().join("turns-list-wait")).unwrap();
    recovery.await.unwrap();
    assert_eq!(pending(&state).await, 0);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// Codex's rejection reaches the owner in Cupola's words, never raw runtime text.
#[tokio::test]
async fn rewind_rejection_uses_a_friendly_message_and_rolls_back() {
    let (dir, state) = journal_fixture().await;
    std::fs::write(dir.path().join("revert-error"), "").unwrap();
    let (status, detail) = call(&state, "project-chat", "turn", false).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!detail.to_string().contains("revert failed"), "{detail}");
    assert!(detail.to_string().contains("couldn’t rewind"), "{detail}");
    assert_eq!(
        pending(&state).await,
        0,
        "a rejected rewind releases the fence"
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
