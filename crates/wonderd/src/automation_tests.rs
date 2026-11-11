use super::*;
use axum::body::to_bytes;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(state: &AppState, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"error":String::from_utf8_lossy(&bytes)})),
    )
}
fn create_body() -> Value {
    json!({"name":"Morning brief","kind":"continuation","scopeType":"project_thread","scopeId":"d19b0f7e-97bb-45d7-8206-324a62386dd9","conversationId":"d19b0f7e-97bb-45d7-8206-324a62386dd9","prompt":"Prepare a brief.","rrule":"FREQ=DAILY;BYHOUR=9;BYMINUTE=0","timezone":"America/New_York","clientRequestId":"create-once"})
}
#[tokio::test]
async fn project_automation_http_uses_bound_thread() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
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
    state
        .store
        .create_project(
            "project",
            "project-request",
            "hash",
            "Project",
            &[wonder_store::ProjectRootInput {
                path: "/tmp/project".into(),
                canonical_path: "/tmp/project".into(),
            }],
            0,
            "now",
        )
        .await
        .unwrap();
    state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: "project-thread",
            project_id: "project",
            family: wonder_store::AgentFamily::Codex,
            provider_store: "codex:home",
            native_session_id: Some("native-project-thread"),
            cwd: "/tmp/project",
            roots_revision: 1,
            title: "Project work",
            model: None,
            effort: None,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now: "now",
        })
        .await
        .unwrap();
    state
        .store
        .bind_project_runtime(
            "project-thread",
            wonder_store::AgentFamily::Codex,
            "codex:home",
            "native-project-thread",
            None,
            "now",
        )
        .await
        .unwrap();
    let body = json!({
        "name":"Daily project review", "kind":"continuation", "scopeType":"project_thread",
        "scopeId":"project-thread", "conversationId":"project-thread", "prompt":"Review the project.",
        "rrule":"FREQ=DAILY;BYHOUR=9", "timezone":"UTC", "clientRequestId":"project-create"
    });
    let (status, created) = call(&state, "POST", "/api/v1/automations", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["scopeType"], "project_thread");
    assert_eq!(created["scopeId"], "project-thread");
    let path = format!("/api/v1/automations/{}", created["id"].as_str().unwrap());
    let (status, paused) = call(&state, "PATCH", &path, json!({"status":"paused"})).await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["status"], "paused");
    let (status, resumed) = call(&state, "PATCH", &path, json!({"status":"active"})).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    state
        .store
        .update_project_metadata("project", None, Some(false), None, "later")
        .await
        .unwrap();
    let (status, replayed) = call(&state, "POST", "/api/v1/automations", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(
        replayed["id"], created["id"],
        "an exact lost-response retry remains recoverable"
    );
    let mut changed_retry = body.clone();
    changed_retry["prompt"] = json!("A different draft after the Project was hidden.");
    let (status, conflict) = call(&state, "POST", "/api/v1/automations", changed_retry).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    let mut new_request = body;
    new_request["clientRequestId"] = json!("project-create-after-hidden");
    let (status, unavailable) = call(&state, "POST", "/api/v1/automations", new_request).await;
    assert_eq!(status, StatusCode::CONFLICT, "{unavailable}");
    let (status, denied) = call(&state, "PATCH", &path, json!({"status":"active"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{denied}");
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
#[tokio::test]
async fn automation_http_create_edit_preview_and_archive_fences() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
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
    let (status, preview) = call(
        &state,
        "POST",
        "/api/v1/automations/preview",
        json!({"rrule":"FREQ=MONTHLY;BYMONTHDAY=15;BYHOUR=9","timezone":"America/New_York"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert!(preview["nextRunAt"].is_string());
    let (status, created) = call(&state, "POST", "/api/v1/automations", create_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    crate::tests::validate_http_contract("automationSummary", &created);
    let (status, duplicate) = call(&state, "POST", "/api/v1/automations", create_body()).await;
    assert_eq!(status, StatusCode::OK, "{duplicate}");
    assert_eq!(created["id"], duplicate["id"]);
    let mut changed_retry = create_body();
    changed_retry["prompt"] = json!("A revised draft after an uncertain first response.");
    let (status, conflict) = call(&state, "POST", "/api/v1/automations", changed_retry).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        created["prompt"],
        state.store.list_automations().await.unwrap()[0].prompt
    );
    assert_eq!(state.store.list_automations().await.unwrap().len(), 1);
    let path = format!("/api/v1/automations/{}", created["id"].as_str().unwrap());
    let (status,edited)=call(&state,"PATCH",&path,json!({"name":"Weekend review","prompt":"Review the week.","rrule":"FREQ=WEEKLY;BYDAY=SA;BYHOUR=10","timezone":"Europe/London","status":"paused"})).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    crate::tests::validate_http_contract("automationSummary", &edited);
    assert_eq!(edited["name"], "Weekend review");
    assert_eq!(edited["prompt"], "Review the week.");
    assert!(edited["nextRunAt"].is_null());
    assert_eq!(created["revision"], 0);
    assert_eq!(edited["revision"], 1);
    let (status, stale) = call(
        &state,
        "PATCH",
        &path,
        json!({"status":"active","expectedRevision":created["revision"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(
        state
            .store
            .automation_by_id(created["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .status,
        "paused"
    );
    let (status, invalid) = call(
        &state,
        "PATCH",
        &path,
        json!({"rrule":"FREQ=YEARLY","status":"paused"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    assert_eq!(
        state.store.list_automations().await.unwrap()[0].rrule,
        "FREQ=WEEKLY;BYDAY=SA;BYHOUR=10"
    );
    let (status, resumed) = call(&state, "PATCH", &path, json!({"status":"active"})).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert!(resumed["nextRunAt"].is_string());
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
    let (status, error) = call(&state, "PATCH", &path, json!({"status":"active"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let (status, _) = call(&state, "DELETE", &path, Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_create_reuse_cannot_confirm_different_automation_details() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
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
    let first = create_body();
    let mut changed = create_body();
    changed["prompt"] = json!("A different task with the same request identifier.");
    let (left, right) = tokio::join!(
        call(&state, "POST", "/api/v1/automations", first),
        call(&state, "POST", "/api/v1/automations", changed),
    );
    assert!(
        (left.0 == StatusCode::CREATED && right.0 == StatusCode::CONFLICT)
            || (right.0 == StatusCode::CREATED && left.0 == StatusCode::CONFLICT),
        "the losing request must report a conflict: {left:?} {right:?}"
    );
    assert_eq!(state.store.list_automations().await.unwrap().len(), 1);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
#[tokio::test]
async fn automation_run_now_retries_return_one_durable_run() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
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
    let _service = crate::ingestion::spawn(state.clone()).await;
    crate::ingestion::tests::ready(&state).await;
    let request: CreateAutomationRequest = serde_json::from_value(create_body()).unwrap();
    let response = create_automation(State(state.clone()), Json(request)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let automation = state.store.list_automations().await.unwrap().remove(0);
    // Hold runtime admission while checking independent concurrent requests.
    let guard = state.dispatch_lock.lock().await;
    let run = || {
        Some(Json(RunAutomationRequest {
            client_request_id: Some("run-once".into()),
        }))
    };
    let first = run_automation_now(State(state.clone()), Path(automation.id.clone()), run()).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let duplicate =
        run_automation_now(State(state.clone()), Path(automation.id.clone()), run()).await;
    assert_eq!(duplicate.status(), StatusCode::OK);
    let different = run_automation_now(
        State(state.clone()),
        Path(automation.id.clone()),
        Some(Json(RunAutomationRequest {
            client_request_id: Some("run-twice".into()),
        })),
    )
    .await;
    assert_eq!(different.status(), StatusCode::CONFLICT);
    let runs = state
        .store
        .list_automation_runs(&automation.id)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    // A manual run is the automation's last run, without moving its schedule
    // or the revision an open editor saves against.
    let after = state.store.list_automations().await.unwrap().remove(0);
    assert_eq!(
        after.last_run_at.as_deref(),
        Some(runs[0].started_at.as_str())
    );
    assert_eq!(after.next_run_at, automation.next_run_at);
    assert_eq!(after.revision, automation.revision);
    let deletion = delete_automation(State(state.clone()), Path(automation.id.clone())).await;
    assert_eq!(deletion.status(), StatusCode::CONFLICT);
    let history = state
        .store
        .list_automation_runs(&automation.id)
        .await
        .unwrap();
    assert_eq!(
        history[0].conversation_id.as_deref(),
        Some("d19b0f7e-97bb-45d7-8206-324a62386dd9")
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
    drop(guard);
}

#[tokio::test]
async fn automation_recovers_materialization_failure_after_initial_scan() {
    async fn run(state: &AppState) -> wonder_store::StoredAutomationRun {
        state
            .store
            .automation_run_by_id("recover-run")
            .await
            .unwrap()
            .unwrap()
    }
    async fn wait_for_message(state: &AppState, run_id: &str) -> wonder_store::StoredMessage {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let run = state
                    .store
                    .automation_run_by_id(run_id)
                    .await
                    .unwrap()
                    .unwrap();
                if let Some(id) = run.message_id {
                    break state.store.message_by_id(&id).await.unwrap().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the same daemon must materialize accepted work once storage recovers")
    }
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    crate::test_support::bind_project_thread(
        &state,
        conversation,
        AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let _service = crate::ingestion::spawn(state.clone()).await;
    crate::ingestion::tests::ready(&state).await;
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    // Finish the initial recovery scan before this run is accepted.
    automation_scheduler_tick(&state).await;
    let (status, created) = call(&state, "POST", "/api/v1/automations", create_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let automation = state.store.list_automations().await.unwrap().remove(0);
    assert!(state
        .store
        .claim_scheduled_automation("recover-run", &automation, "manual", "now", None, false)
        .await
        .unwrap());
    sqlx::query("CREATE TRIGGER fail_automation_materialization BEFORE INSERT ON dispatch_work BEGIN SELECT RAISE(ABORT, 'fixture materialization failure'); END")
        .execute(&pool).await.unwrap();
    run_automation(
        state.clone(),
        automation.clone(),
        "manual".into(),
        "recover-run".into(),
    )
    .await;
    let failed_attempt = run(&state).await;
    assert_eq!(failed_attempt.status, "running");
    assert!(failed_attempt.message_id.is_none());
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(messages, 0, "the failed transaction left no message");
    assert!(!state
        .store
        .claim_scheduled_automation("overlap", &automation, "other", "now", None, false)
        .await
        .unwrap());
    println!("Before recovery: accepted run is running, message_id=None, messages=0; overlapping occurrence excluded");
    sqlx::query("DROP TRIGGER fail_automation_materialization")
        .execute(&pool)
        .await
        .unwrap();

    state
        .update_admission
        .prepare_update("recover-update", &state)
        .await
        .unwrap();
    automation_scheduler_tick(&state).await;
    tokio::time::timeout(
        Duration::from_secs(60),
        run_automation(
            state.clone(),
            automation.clone(),
            "manual".into(),
            "recover-run".into(),
        ),
    )
    .await
    .expect("update admission defers the executor before materialization or dispatch");
    assert!(
        run(&state).await.message_id.is_none(),
        "update admission also fences retry materialization"
    );
    assert!(state.update_admission.cancel_lease("recover-update").await);

    // Hold native execution; this regression needs no provider turn. Recovery
    // must still materialize, and a duplicate executor must exit before dispatch.
    let dispatch = state.dispatch_lock.lock().await;
    automation_scheduler_tick(&state).await;
    let message = wait_for_message(&state, "recover-run").await;
    assert_eq!(message.state, "accepted_by_wonder");
    assert_eq!(message.body, automation.prompt);
    tokio::time::timeout(
        Duration::from_secs(60),
        run_automation(
            state.clone(),
            automation.clone(),
            "manual".into(),
            "recover-run".into(),
        ),
    )
    .await
    .expect("a duplicate executor must exit while the original waits for dispatch");
    // Target validation by a stale executor cannot fail an already linked run.
    let project_id = format!("project-{conversation}");
    state
        .store
        .update_project_metadata(&project_id, None, Some(false), None, "later")
        .await
        .unwrap();
    run_automation(
        state.clone(),
        automation.clone(),
        "manual".into(),
        "recover-run".into(),
    )
    .await;
    assert_eq!(run(&state).await.status, "running");
    state
        .store
        .update_project_metadata(&project_id, None, Some(true), None, "later")
        .await
        .unwrap();
    for _ in 0..3 {
        automation_scheduler_tick(&state).await;
    }
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        messages, 1,
        "repeated scans cannot duplicate the recovered message"
    );

    // Supply a terminal fixture receipt, then let the real recurrence claim
    // coalesce overdue occurrences and admit exactly one next run.
    state
        .store
        .update_message_delivery(
            &message.id,
            "accepted_by_codex",
            Some("thread"),
            Some("fixture-turn"),
        )
        .await
        .unwrap();
    assert!(state
        .store
        .complete_message_if_active(&message.id)
        .await
        .unwrap());
    let mut next = state
        .store
        .automation_by_id(&automation.id)
        .await
        .unwrap()
        .unwrap();
    next.next_run_at = Some("2026-01-01T00:00:00.000Z".into());
    assert!(state.store.update_automation(&next).await.unwrap());
    automation_scheduler_tick(&state).await;
    let runs = state
        .store
        .list_automation_runs(&automation.id)
        .await
        .unwrap();
    assert_eq!(
        runs.len(),
        2,
        "the next occurrence is admitted without restart"
    );
    assert_eq!(run(&state).await.status, "completed");
    let next_run = runs.iter().find(|run| run.status == "running").unwrap();
    assert_eq!(runs.iter().filter(|run| run.status == "running").count(), 1);
    let next_message = wait_for_message(&state, &next_run.id).await;
    assert_ne!(next_message.id, message.id);
    let current = state
        .store
        .automation_by_id(&automation.id)
        .await
        .unwrap()
        .unwrap();
    assert!(current.next_run_at.unwrap() > Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
    // Terminalize only this synthetic run before releasing blocked executors.
    state
        .store
        .update_message_delivery(&next_message.id, "failed", None, None)
        .await
        .unwrap();
    drop(dispatch);
    let _settled = state.dispatch_lock.lock().await;
    let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
    assert_eq!(
        requests
            .lines()
            .filter(|line| *line == "turn/start")
            .count(),
        0
    );
    println!("After recovery: one recovered message across repeated scans; update and duplicate executor fenced; next occurrence materialized (2 runs, 1 active); native turn/start=0");
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

async fn held_automation_message(trigger: &str, terminal_status: &str) {
    fn requests(dir: &std::path::Path, method: &str) -> usize {
        std::fs::read_to_string(dir.join("requests"))
            .unwrap()
            .lines()
            .filter(|line| *line == method)
            .count()
    }
    async fn stored_run(state: &AppState, run_id: &str) -> wonder_store::StoredAutomationRun {
        state
            .store
            .automation_run_by_id(run_id)
            .await
            .unwrap()
            .unwrap()
    }
    fn write_fixture(dir: &std::path::Path, name: &str, value: Value) {
        let staged = dir.join(format!("{name}.staged"));
        std::fs::write(&staged, value.to_string()).unwrap();
        std::fs::rename(staged, dir.join(name)).unwrap();
    }
    async fn wait_for_delivery(state: &AppState, id: &str, expected: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = state.store.message_by_id(id).await.unwrap().unwrap();
                if message.state == expected {
                    break;
                }
                assert!(
                    !matches!(
                        message.state.as_str(),
                        "safe_to_retry" | "failed" | "uncertain"
                    ),
                    "unexpected delivery: {message:?}; error={:?}; logs={:?}",
                    state.store.message_delivery_error(id).await,
                    std::fs::read_to_string(state.data_dir.join("logs/test.jsonl"))
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shared dispatcher must deliver after the hold releases");
    }
    let (dir, mut state) = crate::ingestion::tests::fixture().await;
    state.runtime_catalog.write().await.apply_models_page(
        AgentFamily::Codex,
        &json!({"data":[{"id":"test-model","isDefault":true}]}),
    );
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    let thread = format!("automation-held-{trigger}");
    crate::test_support::bind_project_thread(
        &state,
        conversation,
        AgentFamily::Codex,
        &thread,
        None,
        "now",
    )
    .await
    .unwrap();
    // Keep the existing scripted transport, giving resume the same native
    // thread identity as read. No real Codex process or model work is used.
    let runtime_file = dir.path().join("runtime.py");
    let source = std::fs::read_to_string(&runtime_file).unwrap().replace(
        "else {'thread':{'status':{'type':'idle'}}}",
        "else {'thread':idle[r['params']['threadId']]}",
    );
    std::fs::write(runtime_file, source).unwrap();
    let config = state.projects.config.lock().unwrap().clone();
    state
        .projects
        .codex
        .lock()
        .await
        .restart(config.clone())
        .await
        .unwrap();
    let cwd = state
        .store
        .project_conversation(conversation)
        .await
        .unwrap()
        .unwrap()
        .cwd;
    let mut native = json!({&thread:{"id":&thread, "projectId":"fixture-project",
        "cwd":cwd, "status":{"type":"active"}}});
    std::fs::write(dir.path().join("idle-fixture.json"), native.to_string()).unwrap();
    std::fs::write(
        dir.path().join("turn-pages.json"),
        json!([[{"id":"desktop-turn","status":"inProgress"}]]).to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("resume-active-writer"), "").unwrap();
    let service = crate::ingestion::spawn(state.clone()).await;
    crate::ingestion::tests::ready(&state).await;
    assert!(projects::held(&state, conversation, None).await);
    let (status, created) = call(&state, "POST", "/api/v1/automations", create_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let automation = state.store.list_automations().await.unwrap().remove(0);
    let run_id = match trigger {
        "executor" => {
            assert!(state
                .store
                .claim_scheduled_automation("held-run", &automation, "manual", "now", None, false)
                .await
                .unwrap());
            // Await this executor with NO dispatcher running. A queue timer
            // cannot mask a direct dispatch or make this regression pass.
            run_automation(
                state.clone(),
                automation.clone(),
                "manual".into(),
                "held-run".into(),
            )
            .await;
            "held-run".to_owned()
        }
        "run-now" => {
            let path = format!("/api/v1/automations/{}/run", automation.id);
            let body = json!({"clientRequestId":"held-run-now"});
            let (status, accepted) = call(&state, "POST", &path, body.clone()).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
            let (status, replayed) = call(&state, "POST", &path, body).await;
            assert_eq!(status, StatusCode::OK, "{replayed}");
            assert_eq!(accepted["id"], replayed["id"]);
            accepted["id"].as_str().unwrap().to_owned()
        }
        "due" => {
            let mut due = automation.clone();
            due.next_run_at = Some("2026-01-01T00:00:00.000Z".into());
            assert!(state.store.update_automation(&due).await.unwrap());
            automation_scheduler_tick(&state).await;
            state
                .store
                .list_automation_runs(&automation.id)
                .await
                .unwrap()
                .remove(0)
                .id
        }
        _ => panic!("unknown fixture trigger"),
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        while stored_run(&state, &run_id).await.message_id.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("accepted automation must materialize without a dispatcher");
    let run = stored_run(&state, &run_id).await;
    let message_id = run.message_id.clone().unwrap();
    let message = state
        .store
        .message_by_id(&message_id)
        .await
        .unwrap()
        .unwrap();
    automation_scheduler_tick(&state).await;
    let run = stored_run(&state, &run_id).await;
    println!("{trigger}: Held before reload: run={run:?}; message={message:?}; error={:?}; turn/start={}", state.store.message_delivery_error(&message_id).await.unwrap(), requests(dir.path(), "turn/start"));
    assert_eq!(message.state, "accepted_by_wonder");
    assert_eq!(run.status, "running");
    assert!(run.error.is_none());
    assert!(state.store.is_durable_dispatch(&message_id).await.unwrap());
    assert_eq!(
        state.store.pending_dispatch_messages().await.unwrap().len(),
        1
    );
    assert_eq!(requests(dir.path(), "turn/start"), 0);
    assert_eq!(requests(dir.path(), "thread/resume"), 0);

    // Reload SQLite and restart the scripted native runtime while the desktop
    // writer still holds the thread. The durable message and link must survive.
    drop(service);
    state
        .projects
        .codex
        .lock()
        .await
        .restart(config)
        .await
        .unwrap();
    state.store = Store::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    let _service = crate::ingestion::spawn(state.clone()).await;
    crate::ingestion::tests::ready(&state).await;
    assert!(projects::held(&state, conversation, None).await);
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM messages), (SELECT COUNT(*) FROM dispatch_work), (SELECT COUNT(*) FROM dispatch_attempts)").fetch_one(&pool).await.unwrap();
    assert_eq!(
        counts,
        (1, 1, 0),
        "reload preserves one unclaimed durable message"
    );
    println!("{trigger}: Reloaded SQLite (messages, dispatch_work, dispatch_attempts)={counts:?}");
    let dispatcher = crate::dispatch::spawn(state.clone()).await.unwrap();
    // Observe a read by the explicitly started queue before checking the held
    // state; scheduler retries below are explicit, not a background scheduler.
    let reads = requests(dir.path(), "thread/read");
    state.dispatch_wake.notify_one();
    tokio::time::timeout(Duration::from_secs(60), async {
        while requests(dir.path(), "thread/read") <= reads {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        automation_scheduler_tick(&state).await;
        run_automation(
            state.clone(),
            automation.clone(),
            "manual".into(),
            run_id.clone(),
        )
        .await;
    }
    let reloaded = stored_run(&state, &run_id).await;
    assert_eq!(reloaded.message_id.as_deref(), Some(message_id.as_str()));
    assert_eq!(reloaded.status, "running");
    assert!(reloaded.error.is_none());
    assert_eq!(
        state
            .store
            .message_by_id(&message_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "accepted_by_wonder"
    );
    assert_eq!(
        state
            .store
            .list_automation_runs(&automation.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        state.store.pending_dispatch_messages().await.unwrap().len(),
        1
    );
    assert_eq!(requests(dir.path(), "turn/start"), 0);
    println!("{trigger}: Held after reload + 3 explicit ticks: run={reloaded:?}; same message={message_id}; pending=1; turn/start=0");

    // The dispatcher polls throughout, so release the hold as one change it
    // can observe: the desktop turn ends and the writer exits first, and the
    // thread reports idle last. Each file is replaced whole, never read torn.
    // A pass between "idle" and a still-running turn would rightly refuse.
    write_fixture(dir.path(), "turn-pages.json", json!([[]]));
    std::fs::remove_file(dir.path().join("resume-active-writer")).unwrap();
    native[&thread]["status"]["type"] = json!("idle");
    write_fixture(dir.path(), "idle-fixture.json", native.clone());
    assert!(!projects::held(&state, conversation, None).await);
    state.dispatch_wake.notify_one();
    wait_for_delivery(&state, &message_id, "accepted_by_codex").await;
    let delivered = state
        .store
        .message_by_id(&message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered.codex_thread_id.as_deref(), Some(thread.as_str()));
    assert_eq!(delivered.codex_turn_id.as_deref(), Some("turn"));
    let payloads = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
    let prompts: Vec<Value> = payloads
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|value| value["method"] == "turn/start")
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["params"]["input"][0]["text"], automation.prompt);
    assert_eq!(stored_run(&state, &run_id).await.status, "running");
    // Recover the result from the scripted provider's native history rather
    // than manufacturing a terminal message or automation-run storage update.
    write_fixture(
        dir.path(),
        "turn-pages.json",
        json!([[{"id":"turn","status":terminal_status}]]),
    );
    projects::recover(&state).await;
    for _ in 0..3 {
        automation_scheduler_tick(&state).await;
        run_automation(
            state.clone(),
            automation.clone(),
            "manual".into(),
            run_id.clone(),
        )
        .await;
        state.dispatch_wake.notify_one();
    }
    let finished = stored_run(&state, &run_id).await;
    assert_eq!(finished.status, terminal_status);
    assert_eq!(finished.message_id.as_deref(), Some(message_id.as_str()));
    assert_eq!(finished.error.is_none(), terminal_status == "completed");
    assert!(state
        .store
        .pending_dispatch_messages()
        .await
        .unwrap()
        .is_empty());
    assert_eq!(requests(dir.path(), "turn/start"), 1);
    println!("{trigger}: Released + 3 explicit ticks: run={finished:?}; message={:?}; turn/start=1; prompt={:?}", state.store.message_by_id(&message_id).await.unwrap().unwrap(), prompts[0]);
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM messages), (SELECT COUNT(*) FROM dispatch_work), (SELECT COUNT(*) FROM dispatch_attempts)").fetch_one(&pool).await.unwrap();
    assert_eq!(
        counts,
        (1, 1, 1),
        "one durable message and one submission attempt"
    );
    println!("{trigger}: Finished SQLite (messages, dispatch_work, dispatch_attempts)={counts:?}");
    dispatcher.abort();
    let _ = dispatcher.await;
    state.projects.shutdown().await;
}

#[tokio::test]
async fn automation_held_message_survives_reload_and_dispatches_once() {
    held_automation_message("executor", "completed").await;
}

#[tokio::test]
async fn automation_run_now_held_message_dispatches_once_after_reload() {
    held_automation_message("run-now", "completed").await;
}

#[tokio::test]
async fn automation_due_held_message_reconciles_real_failure_after_reload() {
    held_automation_message("due", "failed").await;
}

#[tokio::test]
async fn automation_failed_turn_does_not_report_success() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
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
    let automation = state
        .store
        .insert_scoped_automation(
            "auto",
            "Daily",
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
        .claim_scheduled_automation("run", &automation, "now", "now", None, false)
        .await
        .unwrap();
    let message = state
        .store
        .materialize_automation_message(
            "run",
            "owner",
            &automation,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            "now",
        )
        .await
        .unwrap();
    state
        .store
        .update_message_delivery(
            &message.id,
            "accepted_by_codex",
            Some("thread"),
            Some("turn"),
        )
        .await
        .unwrap();
    publish_app_server_notification(&state,json!({"method":"turn/completed","params":{"threadId":"thread","turnId":"turn","turn":{"id":"turn","status":"failed"}}})).await;
    state
        .store
        .reconcile_automation_runs("2026-09-01T00:00:00.000Z")
        .await
        .unwrap();
    let run = state
        .store
        .automation_run_by_id("run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "failed");
    assert!(run.error.is_some());
    assert!(state
        .store
        .automation_by_id("auto")
        .await
        .unwrap()
        .unwrap()
        .last_success_at
        .is_none());
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// Contract: a prompt an automation posted reads as that automation's in the
// thread's history (the phone's clock label), never as the owner's own
// message, and keeps a generic label after the automation is deleted.
#[tokio::test]
async fn automation_prompts_are_labelled_in_thread_history() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    crate::test_support::bind_project_thread(
        &state,
        conversation,
        AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let (status, created) = call(&state, "POST", "/api/v1/automations", create_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let automation = state.store.list_automations().await.unwrap().remove(0);
    let device = state
        .store
        .automation_message_device_id()
        .await
        .unwrap()
        .expect("fixture owner device");
    assert!(state
        .store
        .claim_scheduled_automation("label-run", &automation, "manual", "now", None, false)
        .await
        .unwrap());
    let posted = state
        .store
        .materialize_automation_message("label-run", &device, &automation, conversation, "now")
        .await
        .unwrap();
    let owner = state
        .store
        .insert_message(
            &device,
            "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "My own words",
            "owner-hash",
            conversation,
            "later",
        )
        .await
        .unwrap();
    let owner_id = match owner {
        wonder_store::MessageInsert::Inserted(message) => message.id,
        other => panic!("owner message: {other:?}"),
    };
    let history = |state: AppState| async move {
        let (status, snapshot) = call(
            &state,
            "GET",
            &format!("/api/v1/conversations/{conversation}/history"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{snapshot}");
        snapshot["messages"].as_array().unwrap().clone()
    };
    let messages = history(state.clone()).await;
    let find = |messages: &[Value], id: &str| {
        messages
            .iter()
            .find(|message| message["messageId"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} missing from {messages:?}"))
    };
    assert_eq!(
        find(&messages, &posted.id)["source"],
        json!({"kind":"automation","sourceConversationId":null,"sourceTitle":"Morning brief"})
    );
    assert!(find(&messages, &owner_id).get("source").is_none());

    state
        .store
        .finish_automation_run("label-run", "failed", "later", Some("Stopped."), None)
        .await
        .unwrap();
    let (status, _) = call(
        &state,
        "DELETE",
        &format!("/api/v1/automations/{}", automation.id),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let messages = history(state.clone()).await;
    assert_eq!(
        find(&messages, &posted.id)["source"]["sourceTitle"],
        "Automation"
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// Contract: a run the Mac cannot post (no paired phone left) fails with a
// reason and recovery step the phone shows from /runs, and leaves no message.
#[tokio::test]
async fn automation_without_a_paired_phone_fails_with_its_reason_in_runs() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
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
    let (status, created) = call(&state, "POST", "/api/v1/automations", create_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let automation = state.store.list_automations().await.unwrap().remove(0);
    while let Some(device) = state.store.automation_message_device_id().await.unwrap() {
        assert!(state
            .store
            .revoke_owner_device(&device, "later")
            .await
            .unwrap());
    }
    assert!(state
        .store
        .claim_scheduled_automation("orphan-run", &automation, "manual", "now", None, false)
        .await
        .unwrap());
    run_automation(
        state.clone(),
        automation.clone(),
        "manual".into(),
        "orphan-run".into(),
    )
    .await;
    let (status, runs) = call(
        &state,
        "GET",
        &format!("/api/v1/automations/{}/runs", automation.id),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{runs}");
    assert_eq!(runs[0]["status"], "failed");
    assert_eq!(
        runs[0]["error"],
        "This automation needs a paired phone or iPad. Pair one again with cupola pair on your computer."
    );
    assert!(runs[0]["messageId"].is_null());
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
