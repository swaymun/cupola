//! Durable Project sends. The database is the queue; HTTP does not own a task.
use crate::AppState;
use serde_json::json;
use std::time::Duration;

pub async fn spawn(state: AppState) -> Result<tokio::task::JoinHandle<()>, sqlx::Error> {
    state.store.recover_dispatch_claims().await?;
    Ok(tokio::spawn(async move {
        let mut goal_checks = tokio::task::JoinSet::new();
        let mut project_recovery = tokio::task::JoinSet::new();
        let mut next_goal_check = tokio::time::Instant::now();
        let mut next_project_recovery = tokio::time::Instant::now();
        let mut next_update_recovery = tokio::time::Instant::now();
        loop {
            while goal_checks.try_join_next().is_some() {}
            while project_recovery.try_join_next().is_some() {}
            if goal_checks.is_empty() && tokio::time::Instant::now() >= next_goal_check {
                let goal_state = state.clone();
                goal_checks
                    .spawn(async move { crate::goals::enforce_time_limits(&goal_state).await });
                next_goal_check = tokio::time::Instant::now() + Duration::from_secs(5);
            }
            let Some(_admission) = state.update_admission.claim_guard().await else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            if tokio::time::Instant::now() >= next_update_recovery {
                if let Err(error) = crate::update_handoff::recover(&state).await {
                    let _ = state.logger.record(
                        "error",
                        "update_handoff_recovery_failed",
                        json!({"error":error}),
                    );
                }
                next_update_recovery = tokio::time::Instant::now() + Duration::from_secs(5);
            }
            let _ = crate::questions::expire_optional(&state).await;
            if crate::projects::ready(&state).await {
                if let Err(error) = tick(&state).await {
                    let _ = state.logger.record(
                        "error",
                        "dispatch_recovery_failed",
                        json!({"error":error}),
                    );
                }
            }
            drop(_admission);
            // Native history can span many pages. Keep these reads outside
            // update admission and dispatch; recheck each receipt under both
            // locks before applying an observation.
            if project_recovery.is_empty() && tokio::time::Instant::now() >= next_project_recovery {
                let recovery_state = state.clone();
                project_recovery
                    .spawn(async move { crate::projects::recover(&recovery_state).await });
                next_project_recovery = tokio::time::Instant::now() + Duration::from_secs(5);
            }
            // An accepted send wakes the loop at once (a wake that arrives
            // during a tick is kept, so it is never lost); otherwise poll.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = state.dispatch_wake.notified() => {}
            }
        }
    }))
}

async fn message_ready(state: &AppState, message: &wonder_store::StoredMessage) -> bool {
    crate::projects::is_project(state, &message.conversation_id).await
        && crate::projects::ready(state).await
}

pub(crate) async fn tick(state: &AppState) -> Result<(), String> {
    {
        let _guard = state.dispatch_lock.lock().await;
        // A cancelled task or failed response commit can leave a claim even
        // without a process restart. No direct dispatch can run under this lock.
        state
            .store
            .recover_direct_dispatch_claims_except(&state.projects.dispatching())
            .await
            .map_err(|e| e.to_string())?;
    }
    for (message, turn) in state
        .store
        .pending_guides()
        .await
        .map_err(|e| e.to_string())?
    {
        if message_ready(state, &message).await {
            let _ = crate::dispatch_guide(state.clone(), message, turn).await;
        }
    }
    // Idle parents learn that delegated threads finished before new work starts.
    crate::thread_tools::wake_parents(state).await;
    for message in state
        .store
        .pending_dispatch_messages()
        .await
        .map_err(|e| e.to_string())?
    {
        if !message_ready(state, &message).await {
            continue;
        }
        if crate::projects::is_project(state, &message.conversation_id).await {
            // Stays queued until the reply ends or the Mac releases the chat,
            // unless T3 Code holding it takes the message into its thread
            // (known without waiting for T3 Code: `t3_code::route`).
            if crate::projects::send_waits(state, &message.conversation_id, Some(&message)).await {
                continue;
            }
            // Each Project chat dispatches on its own task: one slow handover
            // (Claude on the Mac taking seconds to let go) does not hold the
            // sends of other chats. The claim keeps one dispatch per message.
            tokio::spawn(crate::projects::dispatch(state.clone(), message));
        }
    }
    Ok(())
}

pub(crate) async fn find_accepted_turn(
    state: &AppState,
    message: &wonder_store::StoredMessage,
) -> Result<Option<String>, String> {
    crate::projects::find_accepted_turn(state, message).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use wonder_store::{MessageInsert, Store};

    const DESKTOP_CHAT: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";

    async fn desktop_fixture(
        thread: &str,
    ) -> (
        tempfile::TempDir,
        AppState,
        crate::codex_activity::tests::Fixture,
    ) {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let index = crate::codex_activity::tests::Fixture::new_for_thread(thread).await;
        state.codex_home = index.dir.path().to_path_buf();
        // Runtime recovery rediscovers this catalog. Selecting it in memory
        // alone would race recovery and test a missing-model failure instead.
        std::fs::write(
            dir.path().join("models.json"),
            json!({"data":[{
                "id":"test-model","model":"test-model","displayName":"Test model","isDefault":true
            }]})
            .to_string(),
        )
        .unwrap();
        crate::test_support::bind_project_thread(
            &state,
            DESKTOP_CHAT,
            wonder_store::AgentFamily::Codex,
            thread,
            None,
            "now",
        )
        .await
        .unwrap();
        let cwd = state
            .store
            .project_conversation(DESKTOP_CHAT)
            .await
            .unwrap()
            .unwrap()
            .cwd;
        std::fs::write(
            dir.path().join("idle-fixture.json"),
            json!({thread:{"id":thread,
            "path":index.path,"cwd":cwd,"projectId":"native-project","source":"vscode","originator":"codex_desktop",
            "cliVersion":"0.163.0-alpha.10.2","status":{"type":"idle"}}})
            .to_string(),
        )
        .unwrap();
        let script = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&script).unwrap();
        let original = "else {'thread':{'status':{'type':'idle'}}}";
        assert!(source.contains(original));
        std::fs::write(
            &script,
            source.replace(
                original,
                "else {'thread':{'id':r['params']['threadId'],'status':{'type':'idle'}}}",
            ),
        )
        .unwrap();
        state.projects.shutdown().await;
        assert!(
            crate::projects::media_workspace(&state, DESKTOP_CHAT)
                .await
                .is_some(),
            "fixture must have usable owned storage"
        );
        (dir, state, index)
    }

    async fn ready_ingestion(state: &AppState) -> crate::ingestion::NotificationService {
        let service = crate::ingestion::spawn(state.clone()).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while !crate::projects::ready(state).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("durable ingestion must be ready before testing admission");
        service
    }

    async fn attempt_count(dir: &std::path::Path, message: &str) -> i64 {
        let pool =
            sqlx::SqlitePool::connect(&format!("sqlite://{}", dir.join("state.db").display()))
                .await
                .unwrap();
        sqlx::query_scalar("SELECT COUNT(*) FROM dispatch_attempts WHERE message_id=?")
            .bind(message)
            .fetch_one(&pool)
            .await
            .unwrap()
    }

    fn start_count(dir: &std::path::Path, thread: Option<&str>) -> usize {
        std::fs::read_to_string(dir.join("requests-jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|request| {
                request["method"] == "turn/start"
                    && thread.is_none_or(|id| request["params"]["threadId"] == id)
            })
            .count()
    }

    async fn detail_notice(state: &AppState) -> serde_json::Value {
        let (status, detail) = crate::test_support::call(
            state,
            "GET",
            &format!("/api/v1/project-conversations/{DESKTOP_CHAT}"),
            json!({}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{detail}");
        crate::test_support::assert_http_contract("projectConversationDetail", &detail);
        detail["notice"].clone()
    }

    // Contract: a desktop-origin completed rollout with the producer's nullable
    // timestamps passes the repeated queue gate and persists one submission.
    #[tokio::test]
    async fn desktop_origin_nullable_completion_dispatches_once() {
        let thread = "desktop-nullable-completion";
        let (dir, state, mut index) = desktop_fixture(thread).await;
        index.producer_completion().await;
        assert!(!state.projects.codex_alive().await);
        crate::projects::codex_rpc(&state).await.unwrap();
        let message = enqueue(&state, "desktop-completed", true).await;
        let _ingestion = ready_ingestion(&state).await;
        eprintln!(
            "desktop fixture: ingestion ready; indexed busy={:?}",
            crate::codex_activity::busy(index.dir.path(), thread, &index.path).await
        );
        for _ in 0..10 {
            tick(&state).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        let starts = start_count(dir.path(), None);
        let attempts = attempt_count(dir.path(), &message.id).await;
        eprintln!("desktop nullable completion: state={} thread={:?} turn={:?} turn/start={starts} attempts={attempts}",
            saved.state, saved.codex_thread_id, saved.codex_turn_id);
        assert_eq!(
            saved.state,
            "accepted_by_codex",
            "delivery error: {:?}",
            state
                .store
                .message_delivery_error(&message.id)
                .await
                .unwrap()
        );
        assert_eq!(saved.codex_thread_id.as_deref(), Some(thread));
        assert_eq!(starts, 1);
        assert_eq!(attempts, 1);
        assert!(detail_notice(&state).await.is_null());
        state.projects.shutdown().await;
    }

    // Contract: a foreign writer wins over this runtime's idle snapshot;
    // another chat progresses, unknown evidence has a recovery notice, and
    // matching terminal proof clears that notice and releases exactly once.
    #[tokio::test]
    async fn desktop_writer_waits_with_public_notice_while_peer_progresses() {
        let thread = "desktop-active-then-completed";
        let (dir, state, mut index) = desktop_fixture(thread).await;
        let message = enqueue(&state, "desktop-working", true).await;
        let _ingestion = ready_ingestion(&state).await;
        crate::test_support::project_thread(
            &state.store,
            "75dd4a87-8a54-401a-bbc5-8f94217dd771",
            "Peer",
            state.data_dir.to_str().unwrap(),
            &state.projects.codex_store,
            None,
            None,
            "now",
        )
        .await
        .unwrap();
        let MessageInsert::Inserted(peer) = state
            .store
            .insert_dispatch_message(
                "owner",
                "peer-receipt",
                "hello",
                "hash",
                "75dd4a87-8a54-401a-bbc5-8f94217dd771",
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("peer receipt")
        };
        for _ in 0..3 {
            tick(&state).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        settle(&state, &peer, "accepted_by_codex").await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder"
        );
        assert_eq!(start_count(dir.path(), Some(thread)), 0);
        assert_eq!(attempt_count(dir.path(), &message.id).await, 0);
        let working = detail_notice(&state).await;
        assert!(
            working.as_str().unwrap().contains("while Codex is working"),
            "{working}"
        );
        let database = index.dir.path().join("thread_history_1.sqlite");
        let hidden = database.with_extension("unavailable");
        std::fs::rename(&database, &hidden).unwrap();
        tick(&state).await.unwrap();
        let unknown = detail_notice(&state).await;
        assert!(
            unknown.as_str().unwrap().contains("can't confirm"),
            "{unknown}"
        );
        assert!(
            unknown.as_str().unwrap().contains("quit and reopen Codex"),
            "{unknown}"
        );
        assert_eq!(start_count(dir.path(), Some(thread)), 0);
        assert_eq!(attempt_count(dir.path(), &message.id).await, 0);
        std::fs::rename(hidden, database).unwrap();
        index.producer_completion().await;
        tick(&state).await.unwrap();
        settle(&state, &message, "accepted_by_codex").await;
        tick(&state).await.unwrap();
        assert_eq!(start_count(dir.path(), Some(thread)), 1);
        assert_eq!(attempt_count(dir.path(), &message.id).await, 1);
        assert_eq!(start_count(dir.path(), None), 2);
        assert!(detail_notice(&state).await.is_null());
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        eprintln!("desktop active -> unknown -> completed: peer turn/start=1; held turn/start=0 attempts=0; released state={} thread={:?} turn={:?} turn/start=1 attempts=1; detail.notice cleared", saved.state, saved.codex_thread_id, saved.codex_turn_id);
        state.projects.shutdown().await;
    }

    #[tokio::test]
    async fn desktop_idle_writer_conflict_has_recovery_and_never_submits_concurrently() {
        let thread = "desktop-idle-held-writer";
        let (dir, state, mut index) = desktop_fixture(thread).await;
        index.producer_completion().await;
        std::fs::write(dir.path().join("resume-active-writer"), "").unwrap();
        let message = enqueue(&state, "desktop-held-idle", true).await;
        let _ingestion = ready_ingestion(&state).await;
        tick(&state).await.unwrap();
        settle(&state, &message, "safe_to_retry").await;
        let notice = detail_notice(&state).await;
        assert!(
            notice.as_str().is_some_and(|notice| notice
                .contains("Let any reply finish, then quit Codex and retry in Cupola.")),
            "{notice}"
        );
        assert_eq!(
            state
                .store
                .message_delivery_error(&message.id)
                .await
                .unwrap()
                .as_deref(),
            notice.as_str()
        );
        assert_eq!(start_count(dir.path(), Some(thread)), 0);
        assert_eq!(attempt_count(dir.path(), &message.id).await, 1);
        std::fs::remove_file(dir.path().join("resume-active-writer")).unwrap();
        assert!(state
            .store
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
        tick(&state).await.unwrap();
        settle(&state, &message, "accepted_by_codex").await;
        tick(&state).await.unwrap();
        assert_eq!(start_count(dir.path(), Some(thread)), 1);
        assert_eq!(attempt_count(dir.path(), &message.id).await, 2);
        assert!(detail_notice(&state).await.is_null());
        eprintln!("idle writer conflict: safe_to_retry, turn/start=0 attempts=1, public notice=stored error; after release + explicit retry: accepted_by_codex, total turn/start=1 attempts=2, notice cleared");
        state.projects.shutdown().await;
    }

    async fn enqueue(state: &AppState, client: &str, direct: bool) -> wonder_store::StoredMessage {
        {
            let mut catalog = state.runtime_catalog.write().await;
            catalog.apply_models_page(
                wonder_store::AgentFamily::Codex,
                &json!({"data":[{"id":"test-model","isDefault":true}]}),
            );
        }
        let MessageInsert::Inserted(message) = state
            .store
            .insert_dispatch_message(
                "owner",
                client,
                "hello",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                &[],
                "now",
                direct,
            )
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        message
    }

    async fn settle(state: &AppState, message: &wonder_store::StoredMessage, expected: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                // A receipt commits before dispatch publishes its error/event
                // and updates the conversation notice. Observe it under the
                // same lifecycle lock so those later effects have finished.
                let dispatch = state.dispatch_lock.lock().await;
                if state
                    .store
                    .message_by_id(&message.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state
                    == expected
                {
                    break;
                }
                drop(dispatch);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            let logs = std::fs::read_to_string(state.data_dir.join("logs/test.jsonl"));
            panic!("{error}: expected {expected}; {logs:?}");
        });
    }

    #[tokio::test]
    async fn restart_recovers_accepted_and_claimed_work_without_another_request() {
        for claimed in [false, true] {
            let (dir, mut state) = crate::ingestion::tests::fixture().await;
            let message = enqueue(&state, "direct", true).await;
            let guide = enqueue(&state, "guide", false).await;
            if claimed {
                assert!(state
                    .store
                    .claim_message_for_dispatch(&message.id)
                    .await
                    .unwrap());
            }
            state.store = Store::connect(&format!(
                "sqlite://{}",
                dir.path().join("state.db").display()
            ))
            .await
            .unwrap();
            let _ingestion = crate::ingestion::spawn(state.clone()).await;
            let dispatcher = spawn(state.clone()).await.unwrap();
            settle(&state, &message, "accepted_by_codex").await;
            assert_eq!(
                state
                    .store
                    .message_by_id(&guide.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "uncertain"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("requests"))
                    .unwrap()
                    .lines()
                    .filter(|l| *l == "turn/start")
                    .count(),
                1
            );
            dispatcher.abort();
            let _ = dispatcher.await;
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn execution_before_lost_receipt_reconciles_without_repeating_action() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let message = enqueue(&state, "direct", true).await;
        std::fs::write(dir.path().join("crash-after-execution"), "").unwrap();
        let _ingestion = crate::ingestion::spawn(state.clone()).await;
        let dispatcher = spawn(state.clone()).await.unwrap();
        settle(&state, &message, "completed").await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("requests"))
                .unwrap()
                .lines()
                .filter(|l| *l == "turn/start")
                .count(),
            1
        );
        dispatcher.abort();
        let _ = dispatcher.await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn response_commit_failure_recovers_from_disk_without_duplicate_submission() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let message = enqueue(&state, "direct", true).await;
        let url = format!("sqlite://{}", dir.path().join("state.db").display());
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        sqlx::query("CREATE TRIGGER fail_receipt BEFORE UPDATE OF state ON messages WHEN NEW.state = 'accepted_by_codex' BEGIN SELECT RAISE(ABORT, 'receipt write fault'); END")
            .execute(&pool).await.unwrap();
        crate::projects::dispatch(state.clone(), message.clone()).await;
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "dispatching_to_codex"
        );
        sqlx::query("DROP TRIGGER fail_receipt")
            .execute(&pool)
            .await
            .unwrap();
        state.projects.codex.lock().await.shutdown().await.unwrap();
        state.store = Store::connect(&url).await.unwrap();
        std::fs::write(dir.path().join("bad-history"), "").unwrap();
        let _ingestion = crate::ingestion::spawn(state.clone()).await;
        let dispatcher = spawn(state.clone()).await.unwrap();
        settle(&state, &message, "accepted_by_codex").await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // An unreadable receipt holds this execution without blocking other Projects.
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
        std::fs::remove_file(dir.path().join("bad-history")).unwrap();
        settle(&state, &message, "completed").await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("requests"))
                .unwrap()
                .lines()
                .filter(|l| *l == "turn/start")
                .count(),
            1
        );
        dispatcher.abort();
        let _ = dispatcher.await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn absent_history_never_releases_a_possibly_submitted_action() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let message = enqueue(&state, "direct", true).await;
        state
            .store
            .claim_message_for_dispatch(&message.id)
            .await
            .unwrap();
        state
            .store
            .begin_dispatch_submission(&message.id, "thread")
            .await
            .unwrap();
        state.store.recover_dispatch_claims().await.unwrap();
        tick(&state).await.unwrap();
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
            .requeue_message_for_retry(&message.id)
            .await
            .unwrap());
        assert!(!std::fs::read_to_string(dir.path().join("requests"))
            .unwrap()
            .lines()
            .any(|l| l == "turn/start"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}
