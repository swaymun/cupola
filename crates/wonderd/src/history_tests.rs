use super::*;
use axum::body::to_bytes;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use std::time::{Duration, Instant};

// The host must not rescan unchanged native history, or mistake a partial
// read/new lifecycle/older-turn edit for a complete saved provider revision.
// This extends the real RPC + SQLite hydration owner, not a cache mock.
#[tokio::test]
async fn native_refresh_checkpoint_preserves_repairs_and_older_changes() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    // Native rollout hints are process-wide; never lend a temporary path to
    // another fixture using the ordinary thread ID.
    let thread = uuid::Uuid::new_v4().to_string();
    crate::test_support::bind_project_thread(
        &state,
        chat,
        wonder_store::AgentFamily::Codex,
        &thread,
        None,
        "now",
    )
    .await
    .unwrap();
    let source = dir.path().join("refresh-source.jsonl");
    std::fs::write(&source, "original\n").unwrap();
    let configure = |value: serde_json::Value| {
        std::fs::write(dir.path().join("refresh-fixture.json"), value.to_string()).unwrap()
    };
    async fn run(state: &AppState, dir: &std::path::Path, repair: bool) -> (usize, bool) {
        let pages = || {
            std::fs::read_to_string(dir.join("requests"))
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "thread/items/list")
                .count()
        };
        let before = pages();
        let token = uuid::Uuid::new_v4().to_string();
        let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        assert!(state
            .store
            .claim_history_refresh(chat, &token, now_ms() as i64)
            .await
            .unwrap());
        let thread = state
            .store
            .conversation_thread(chat)
            .await
            .unwrap()
            .unwrap();
        let result = refresh_runtime_history(state, chat, &thread, &token, repair).await;
        state
            .store
            .update_history_refresh(
                chat,
                &token,
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
                now_ms() as i64,
                None,
            )
            .await
            .unwrap();
        (pages() - before, result.is_ok())
    }
    configure(serde_json::json!({"count":301,"failAt":100}));
    assert_eq!(run(&state, dir.path(), false).await, (2, false));
    configure(serde_json::json!({"count":301}));
    assert_eq!(
        run(&state, dir.path(), false).await,
        (4, true),
        "partial bootstrap must retry all pages"
    );
    let sequence = state
        .store
        .committed_sequence(&state.host_epoch)
        .await
        .unwrap();
    assert_eq!(run(&state, dir.path(), false).await, (0, true));
    assert_eq!(
        state
            .store
            .committed_sequence(&state.host_epoch)
            .await
            .unwrap(),
        sequence
    );

    // A changed item is older than three unchanged newest pages.
    configure(serde_json::json!({"count":301,"changed":0}));
    std::fs::write(&source, "original plus a late older reply\n").unwrap();
    assert_eq!(run(&state, dir.path(), false).await, (4, true));
    let db = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    let detail: String =
        sqlx::query_scalar("SELECT payload_json FROM history_entries WHERE source_id=?")
            .bind(format!("{chat}:{thread}:turn:item-0"))
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(
        detail.contains("changedchanged"),
        "late edit reached saved history"
    );
    let last = state
        .store
        .conversation_history_page(&state.host_epoch, chat, None, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        last.events[0].item_id.as_deref(),
        Some("item-300"),
        "chronology is unchanged"
    );
    assert_eq!(run(&state, dir.path(), false).await, (0, true));

    // A process/lifecycle change can occur without a transcript append.
    configure(serde_json::json!({"count":301,"changed":0,"status":"inProgress"}));
    assert_eq!(run(&state, dir.path(), false).await, (4, true));
    assert_eq!(run(&state, dir.path(), false).await, (0, true));
    assert_eq!(
        run(&state, dir.path(), true).await,
        (4, true),
        "explicit repair ignores the checkpoint"
    );

    // Reopening the durable store retains the checkpoint; reset invalidates it
    // even if the provider has not yet rewritten its transcript.
    let reopened = wonder_store::Store::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    let mut restarted = state.clone();
    restarted.store = reopened;
    assert_eq!(run(&restarted, dir.path(), false).await, (0, true));
    // A new runtime process (a provider or Cupola update, a daemon restart)
    // may project the same transcript differently: re-read once.
    let config = state
        .projects
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    state
        .projects
        .codex
        .lock()
        .await
        .restart(config)
        .await
        .unwrap();
    assert_eq!(run(&state, dir.path(), false).await, (4, true));
    assert_eq!(run(&state, dir.path(), false).await, (0, true));
    sqlx::query("UPDATE history_hydration SET file_change_version=0 WHERE conversation_id=?")
        .bind(chat)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(run(&state, dir.path(), false).await, (4, true));

    // A turn list the host cannot read completely (past its page bound, or a
    // page without data) still refreshes the items, as before the checkpoint
    // existed; it only never vouches for an unchanged source.
    for incomplete in [
        serde_json::json!({"count":301,"turnPages":101}),
        serde_json::json!({"count":301,"turnsWithoutData":true}),
    ] {
        configure(incomplete);
        for _ in 0..2 {
            assert_eq!(
                run(&state, dir.path(), false).await,
                (4, true),
                "an incomplete turn list falls back to a full scan"
            );
        }
    }

    // A preview the host could not keep this time (no preview storage, a
    // failed save) is retried by the next refresh, never frozen behind an
    // unchanged checkpoint; once kept, the checkpoint applies again.
    let blocked = dir.path().join("project-media").join(chat);
    let _ = std::fs::remove_dir_all(&blocked);
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, "not a folder").unwrap();
    configure(serde_json::json!({"count":301,"media":5}));
    let preview = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT payload_json FROM history_entries WHERE source_id=?",
        )
        .bind(format!("{chat}:{thread}:turn:item-5"))
        .fetch_one(&db)
        .await
        .unwrap()
    };
    for _ in 0..2 {
        assert_eq!(
            run(&state, dir.path(), false).await,
            (4, true),
            "a degraded preview is retried"
        );
        assert!(preview().await.contains("Preview unavailable"));
    }
    std::fs::remove_file(&blocked).unwrap();
    assert_eq!(run(&state, dir.path(), false).await, (4, true));
    assert!(preview().await.contains("wonderArtifact"));
    assert_eq!(run(&state, dir.path(), false).await, (0, true));

    std::fs::write(&source, "original plus a newer reply\n").unwrap();
    configure(serde_json::json!({"count":301,"mutateDuringRead":true}));
    assert_eq!(run(&state, dir.path(), false).await, (4, true));
    configure(serde_json::json!({"count":301}));
    assert_eq!(
        run(&state, dir.path(), false).await,
        (4, true),
        "a changing read never establishes a checkpoint"
    );
    assert_eq!(run(&state, dir.path(), false).await, (0, true));
    // The Claude bridge's items also follow its task list and turn receipts,
    // which change without a transcript write or a turn status change. Its
    // own projection revision re-reads them.
    configure(serde_json::json!({"count":301,"projection":"a"}));
    assert_eq!(
        run(&state, dir.path(), false).await,
        (4, true),
        "a projection change without a transcript write re-reads"
    );
    assert_eq!(run(&state, dir.path(), false).await, (0, true));
    configure(serde_json::json!({"count":301}));
    std::fs::remove_file(&source).unwrap();
    for _ in 0..2 {
        assert_eq!(
            run(&state, dir.path(), false).await,
            (4, true),
            "unknown revisions retain full recovery"
        );
    }
    // A runtime's projection revision vouches on its own when the host has
    // no transcript file to check (the bridge finds Claude sessions the
    // host's default path does not), and a revision that moves during the
    // read never establishes a checkpoint.
    for (projection, pages) in [("b", 4), ("b", 0), ("c", 4), ("c", 0)] {
        configure(serde_json::json!({"count":301,"projection":projection}));
        assert_eq!(run(&state, dir.path(), false).await, (pages, true));
    }
    configure(serde_json::json!({"count":301,"projection":"d","mutateProjectionDuringRead":true}));
    assert_eq!(run(&state, dir.path(), false).await, (4, true));
    configure(serde_json::json!({"count":301,"projection":"d"}));
    assert_eq!(
        run(&state, dir.path(), false).await,
        (4, true),
        "a changing projection never establishes a checkpoint"
    );
    assert_eq!(run(&state, dir.path(), false).await, (0, true));
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// The revision gates both the phone's reload and the host's skipped re-read,
// so a same-size rewrite that keeps its mtime must still change it.
#[cfg(unix)]
#[tokio::test]
async fn history_file_revision_sees_same_size_rewrites_that_keep_their_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    std::fs::write(&path, "first\n").unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let keep_mtime = |path: &std::path::Path| {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap()
    };
    let original = history_file_revision(&path).await.unwrap();
    // Replaced by a rename (an editor's or tool's atomic save).
    let staged = dir.path().join("transcript.tmp");
    std::fs::write(&staged, "other\n").unwrap();
    keep_mtime(&staged);
    std::fs::rename(&staged, &path).unwrap();
    let replaced = history_file_revision(&path).await.unwrap();
    assert_ne!(replaced, original, "a new file at the same path");
    // Rewritten in place.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, "third\n").unwrap();
    keep_mtime(&path);
    assert_ne!(history_file_revision(&path).await.unwrap(), replaced);
}

// Rerunnable synthetic host measurement; no provider/model work or personal data.
// Covers the audited 20,982-row / ~105 MiB transcript without retaining its text.
#[tokio::test]
#[ignore = "large native history performance measurement"]
async fn native_refresh_large_history_measurement() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    // Native rollout hints are process-wide; never lend a temporary path to
    // another fixture using the ordinary thread ID.
    let thread = uuid::Uuid::new_v4().to_string();
    crate::test_support::bind_project_thread(
        &state,
        chat,
        wonder_store::AgentFamily::Codex,
        &thread,
        None,
        "now",
    )
    .await
    .unwrap();
    let count = 20_982;
    std::fs::write(
        dir.path().join("refresh-fixture.json"),
        serde_json::json!({"count": count, "bytes": 5248}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("refresh-source.jsonl"),
        "fixture revision\n",
    )
    .unwrap();
    let requests = || {
        std::fs::read_to_string(dir.path().join("requests"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "thread/items/list")
            .count()
    };
    let mut samples = Vec::new();
    for round in 0..4 {
        let token = format!("measurement-{round}");
        let before = requests();
        let start = Instant::now();
        state
            .store
            .claim_history_refresh(chat, &token, now_ms() as i64)
            .await
            .unwrap();
        refresh_runtime_history(&state, chat, &thread, &token, false)
            .await
            .unwrap();
        state
            .store
            .update_history_refresh(chat, &token, "completed", now_ms() as i64, None)
            .await
            .unwrap();
        let sample = serde_json::json!({"round":round, "elapsedMs":start.elapsed().as_secs_f64()*1000.0,
            "itemPages":requests()-before});
        eprintln!("NATIVE_REFRESH_SAMPLE={sample}");
        samples.push(sample);
    }
    let (rows, _) = state.store.conversation_history_mark(chat).await.unwrap();
    assert_eq!(rows, count);
    eprintln!(
        "NATIVE_REFRESH_MEASUREMENT={}",
        serde_json::json!({"rows":rows,
        "textBytes":count*5248, "profile":"debug", "samples":samples})
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn hydrated_compaction_completion_survives_interrupted_turn_and_history_paging() {
    let store = wonder_store::Store::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::test_support::project_thread(
        &store,
        "chat",
        "Project",
        "/selected/workspace",
        "fixture-store",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    store.start_event_epoch("epoch").await.unwrap();
    let mut expected = HashMap::new();
    for (index, (clocks, outcome)) in [
        (
            serde_json::json!({"startedAtMs":10,"completedAtMs":20}),
            "completed",
        ),
        (serde_json::json!({"completedAtMs":20}), "completed"),
        (
            serde_json::json!({"startedAtMs":10,"completedAtMs":null}),
            "interrupted",
        ),
        (serde_json::json!({}), "unknown"),
        (
            serde_json::json!({"startedAtMs":-1,"completedAtMs":20}),
            "unknown",
        ),
        (serde_json::json!({"completedAtMs":"20"}), "unknown"),
        (
            serde_json::json!({"startedAtMs":30,"completedAtMs":20}),
            "unknown",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("compact-{index}");
        expected.insert(id.clone(), outcome);
        let item = serde_json::json!({"id":id,"type":"contextCompaction"});
        let (state, authority) = history_item_lifecycle(&clocks, &item);
        let detail =
            thread_item_upsert_detail_with_authority("turn", &item, state, authority, None)
                .unwrap();
        let mut detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        detail["historyRefresh"] = true.into();
        detail["turnStatus"] = "interrupted".into();
        let mut event = HostEventEnvelope {
            event_id: format!("event-{index}"),
            host_epoch: "epoch".into(),
            sequence: 0,
            occurred_at: format!("2026-10-13T00:00:0{index}Z"),
            request_id: None,
            device_id: None,
            conversation_id: Some("chat".into()),
            message_id: None,
            thread_id: Some("thread".into()),
            turn_id: Some("turn".into()),
            item_id: Some(id),
            approval_id: None,
            event: WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(detail.to_string()),
            },
        };
        store.commit_event(&mut event).await.unwrap();
    }
    let mut cursor = None;
    let mut count = 0;
    loop {
        let page = store
            .conversation_history_page("epoch", "chat", cursor.as_ref(), 1)
            .await
            .unwrap()
            .unwrap();
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &page.messages,
            &page.assistant_messages,
            &page.events,
            &[],
            None,
        );
        for turn in &projection.turns {
            assert_eq!(turn.status, "interrupted");
            for item in &turn.items {
                assert_eq!(item.state, expected[&item.id], "item {}", item.id);
                count += 1;
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, expected.len());
}

#[test]
fn current_activity_omits_terminal_errors_and_large_private_payloads() {
    let item = |status: &str, payload: serde_json::Value| {
        let mut raw = serde_json::json!({"id":"read", "type":"mcpToolCall", "status":status, "server":"Claude", "tool":"Read"});
        raw.as_object_mut()
            .unwrap()
            .extend(payload.as_object().unwrap().clone());
        typed_thread_item_projection(
            &AppServerThreadItem {
                turn_id: "turn".into(),
                item: raw,
            },
            "1000",
            None,
        )
    };
    for status in ["completed", "failed", "interrupted", "unknown"] {
        assert!(compact_current_activity(item(status, serde_json::json!({}))).is_none());
    }
    for error in [
        serde_json::json!({"success":false}),
        serde_json::json!({"error":"private failure"}),
        serde_json::json!({"result":{"isError":true}}),
    ] {
        assert!(compact_current_activity(item("inProgress", error)).is_none());
    }
    let value = compact_current_activity(item("inProgress", serde_json::json!({
        "toolAction":{"kind":"read","paths":["Sources/AppShell.swift"]}, "arguments":{"private":"value"}, "result":{"output":"secret body"}
    }))).unwrap();
    assert_eq!(value["payload"]["toolAction"]["kind"], "read");
    assert!(value["payload"].get("arguments").is_none());
    assert!(value["payload"].get("result").is_none());
    let mut oversized = item("inProgress", serde_json::json!({}));
    oversized.payload =
        serde_json::json!({"toolAction":{"kind":"search","query":"x".repeat(9000)}});
    assert_eq!(
        compact_current_activity(oversized).unwrap()["payload"],
        serde_json::json!({})
    );
}

#[tokio::test]
async fn file_changes_use_selected_workspace_and_repair_legacy_history() {
    for version in [serde_json::Value::Null, serde_json::json!(1)] {
        check_file_change_history_repair(version).await;
    }
}

async fn check_file_change_history_repair(previous_version: serde_json::Value) {
    let store = wonder_store::Store::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::test_support::project_thread(
        &store,
        "chat",
        "Project",
        "/selected/workspace",
        "fixture-store",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    let chat = "chat".to_owned();
    store.start_event_epoch("epoch").await.unwrap();
    let workspace = store
        .conversation_execution_directory(&chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(workspace, "/selected/workspace");
    let raw = serde_json::json!({"id":"edit", "type":"fileChange", "status":"completed", "changes":[
        {"path":"/selected/workspace/Tests/StartupTests.swift", "kind":{"type":"add"}, "diff":"import XCTest\nlet test = true\n"},
        {"path":"/selected/workspace-other/private.swift", "kind":{"type":"update", "move_path":"/selected/workspace/moved.swift"}, "diff":"@@ -1 +1 @@\n-old\n+new"}
    ]});
    let legacy = thread_item_upsert_detail_with_authority("turn", &raw, "failed", 2, None).unwrap();
    let mut legacy: serde_json::Value = serde_json::from_str(&legacy).unwrap();
    legacy["item"]["fileChangeVersion"] = previous_version;
    legacy["item"]["paths"][1] = serde_json::json!("[path outside workspace]");
    legacy["item"]["diffs"][1]["path"] = serde_json::json!("[path outside workspace]");
    legacy["item"]["diffs"][1]
        .as_object_mut()
        .unwrap()
        .remove("originalPath");
    let mut event = HostEventEnvelope {
        event_id: "old".into(),
        host_epoch: "epoch".into(),
        sequence: 0,
        occurred_at: "1000".into(),
        conversation_id: Some(chat.clone()),
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        request_id: None,
        device_id: None,
        message_id: None,
        item_id: Some("edit".into()),
        approval_id: None,
        event: WonderEvent::Activity {
            category: "thread_item_upsert".into(),
            state: "updated".into(),
            detail: Some(legacy.to_string()),
        },
    };
    store.commit_event(&mut event).await.unwrap();
    assert!(store
        .needs_file_change_repair(&chat, 100_000)
        .await
        .unwrap());
    assert!(store
        .claim_history_refresh(&chat, "repair", 100_000)
        .await
        .unwrap());
    assert!(!store
        .needs_file_change_repair(&chat, 100_001)
        .await
        .unwrap());
    let repaired = thread_item_upsert_detail("turn", &raw, "completed", Some(&workspace)).unwrap();
    let mut repaired: serde_json::Value = serde_json::from_str(&repaired).unwrap();
    repaired["historyRefresh"] = serde_json::json!(true);
    event.event_id = "repair".into();
    event.occurred_at = "2000".into();
    event.event = WonderEvent::Activity {
        category: "thread_item_upsert".into(),
        state: "updated".into(),
        detail: Some(repaired.to_string()),
    };
    store.commit_event(&mut event).await.unwrap();
    let page = store
        .conversation_history_page("epoch", &chat, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.workspace_path, workspace);
    let projection = conversation_thread_projection_with_items(
        None,
        &page.messages,
        &page.assistant_messages,
        &page.events,
        &[],
        Some(&page.workspace_path),
    );
    let item = &projection.turns[0].items[0];
    assert_eq!(
        item.state, "failed",
        "repair must not rewrite the original outcome"
    );
    assert_eq!(item.created_at, "1000");
    assert_eq!(item.payload["paths"][0], "Tests/StartupTests.swift");
    assert_eq!(item.payload["paths"][1], "moved.swift");
    assert_eq!(item.payload["diffs"][1]["path"], "moved.swift");
    assert_eq!(
        item.payload["diffs"][1]["originalPath"],
        "[path outside workspace]"
    );
    assert_eq!(
        item.payload["diffs"][0]["diff"],
        "+import XCTest\n+let test = true"
    );
    assert_eq!(item.payload["additions"], 3);
    assert_eq!(item.payload["deletions"], 1);
    assert_eq!(
        item.payload["fileChangeVersion"],
        FILE_CHANGE_PROJECTION_VERSION
    );
    store
        .update_history_refresh(&chat, "repair", "completed", 100_002, None)
        .await
        .unwrap();
    assert!(!store
        .needs_file_change_repair(&chat, 999_999)
        .await
        .unwrap());
}

#[tokio::test]
async fn history_pages_retain_turn_lifecycle_outside_the_item_page() {
    let store = wonder_store::Store::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::test_support::project_thread(
        &store,
        "chat",
        "Project",
        "/tmp",
        "fixture-store",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    store.start_event_epoch("epoch").await.unwrap();
    let event = |id: &str, at: &str, event: WonderEvent| HostEventEnvelope {
        event_id: id.into(),
        host_epoch: "epoch".into(),
        sequence: 0,
        occurred_at: at.into(),
        conversation_id: Some("chat".into()),
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        request_id: None,
        device_id: None,
        message_id: None,
        item_id: None,
        approval_id: None,
        event,
    };
    let item = |id: &str, status: &str| WonderEvent::Activity {
        category: "thread_item_upsert".into(),
        state: "updated".into(),
        detail: Some(
            serde_json::json!({
                "turnId": "turn", "itemId": id, "state": status, "lifecycleAuthority": 2,
                "item": {"id": id, "type": "commandExecution", "command": "test", "status": status}
            })
            .to_string(),
        ),
    };
    store
        .commit_event(&mut event(
            "start",
            "1700000000000",
            WonderEvent::MessageState {
                state: DeliveryState::AcceptedByCodex,
            },
        ))
        .await
        .unwrap();
    store
        .commit_event(&mut event(
            "failed-command",
            "1700000001000",
            item("failed-command", "failed"),
        ))
        .await
        .unwrap();
    store
        .commit_event(&mut event(
            "passed-command",
            "1700000002000",
            item("passed-command", "completed"),
        ))
        .await
        .unwrap();

    // Neither the failed command nor a successful command ends the response.
    let newest = store
        .conversation_history_page("epoch", "chat", None, 1)
        .await
        .unwrap()
        .unwrap();
    let older_cursor = newest.next_cursor.unwrap();
    for before in [None, Some(&older_cursor)] {
        let page = store
            .conversation_history_page("epoch", "chat", before, 1)
            .await
            .unwrap()
            .unwrap();
        let projection = conversation_thread_projection(
            None,
            &page.messages,
            &page.assistant_messages,
            &page.events,
        );
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.turns[0].status, "inProgress");
        assert_eq!(
            projection.turns[0].started_at.as_deref(),
            Some("1700000000000")
        );
        assert_eq!(
            projection.turns[0].items.len(),
            1,
            "lifecycle context must not expand the item page"
        );
    }

    // An older page must use the latest terminal status, even though the end
    // boundary is newer than its cursor. Preserve actual failure and stop too.
    for (state, expected) in [
        (DeliveryState::Completed, "completed"),
        (DeliveryState::Failed, "failed"),
        (DeliveryState::Interrupted, "interrupted"),
    ] {
        store
            .commit_event(&mut event(
                expected,
                "1700000003000",
                WonderEvent::MessageState { state },
            ))
            .await
            .unwrap();
        let page = store
            .conversation_history_page("epoch", "chat", Some(&older_cursor), 1)
            .await
            .unwrap()
            .unwrap();
        let projection = conversation_thread_projection(
            None,
            &page.messages,
            &page.assistant_messages,
            &page.events,
        );
        let turn = &projection.turns[0];
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(turn.status, expected);
        assert_eq!(turn.started_at.as_deref(), Some("1700000000000"));
        assert_eq!(turn.completed_at.as_deref(), Some("1700000003000"));
        assert_eq!(turn.items.len(), 1);
        assert_eq!(
            turn.items[0].state, "failed",
            "command outcome remains visible"
        );
    }
}

#[test]
fn item_outcomes_do_not_invent_a_turn_lifecycle() {
    for status in ["failed", "completed", "interrupted", "running"] {
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[],
            &[AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"id":"command", "type":"commandExecution", "command":"test", "status":status}),
            }],
            None,
        );
        assert_eq!(
            projection.turns[0].status, "unknown",
            "item status: {status}"
        );
    }
}

/// A background task's report wakes Claude into a run of its own: the history
/// projection keeps the owner's turn and, for each wake, its row, its own
/// tool call and its reply apart. The input is the runtime's own `nativeTurns`
/// output for a synthetic transcript (services/claude-runtime/test/wake-turns.test.mjs
/// keeps the file current), not shapes written here.
#[test]
fn each_background_wake_keeps_its_row_work_and_reply_in_its_own_turn() {
    let runtime_turns: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../services/claude-runtime/test/fixtures/wake-turns.json"
    ))
    .unwrap();
    let items: Vec<_> = runtime_turns
        .iter()
        .flat_map(|turn| {
            let id = turn["id"].as_str().unwrap().to_string();
            turn["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |item| AppServerThreadItem {
                    turn_id: id.clone(),
                    item: item.clone(),
                })
        })
        .collect();
    let projection = conversation_thread_projection_with_items(
        Some("thread".into()),
        &[],
        &[],
        &[],
        &items,
        None,
    );
    let turns: Vec<_> = projection
        .turns
        .iter()
        .map(|turn| turn.id.as_str())
        .collect();
    assert_eq!(turns, ["owner-1", "wake-alpha", "wake-beta", "wake-gamma"]);
    for turn in &projection.turns[1..] {
        let kinds: Vec<_> = turn
            .items
            .iter()
            .map(|item| item.item_type.as_str())
            .collect();
        assert_eq!(
            kinds,
            ["taskNotification", "mcpToolCall", "agentMessage"],
            "{}",
            turn.id
        );
        assert!(turn.items[0]
            .text
            .as_deref()
            .is_some_and(|text| text.starts_with("Command \"")));
        assert!(turn.items[0].payload["taskStatus"].as_str().is_some());
        assert_eq!(
            turn.items[2].text.as_deref(),
            Some(format!("Reply for {}", turn.id.trim_start_matches("wake-")).as_str())
        );
    }
}

#[test]
fn cursors_are_versioned_scoped_and_survive_restart() {
    let cursor = wonder_store::HistoryCursor {
        history_revision: 0,
        sort_ms: 1700000000000,
        sequence: 123,
    };
    let token = encode_history_cursor("chat", &cursor);
    assert_eq!(
        decode_history_cursor(Some(&token), "chat")
            .unwrap()
            .unwrap()
            .sequence,
        123
    );
    assert!(decode_history_cursor(Some(&token), "other").is_err());
    assert!(decode_history_cursor(Some("invalid"), "chat").is_err());
    assert!(decode_history_cursor(Some(&"x".repeat(2049)), "chat").is_err());
}

/// Waits for a fixture's marker file. The bound is only a hang guard: the
/// fake runtime is a Python process, which a busy Mac can hold for seconds.
async fn wait_for(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn delayed_history_does_not_block_controls_or_local_pages() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(
        &state,
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO messages(id,device_id,client_message_id,body,body_sha256,conversation_id,state,created_at) SELECT printf('history-%05d',x),'owner',printf('history-client-%05d',x),printf('%01024d',x),'hash','d19b0f7e-97bb-45d7-8206-324a62386dd9','completed',CAST(1600000000000+x AS TEXT) FROM n").execute(&pool).await.unwrap();
    pool.close().await;
    let wonder_store::MessageInsert::Inserted(message) = state
        .store
        .insert_message(
            "owner",
            "active",
            "hello",
            "hash",
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            "1700000000000",
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
            Some("turn"),
        )
        .await
        .unwrap();
    let health = state.projects.codex.lock().await.health();
    let runtime_id = health.id().to_owned();
    state
        .ingestion
        .register_project_runtime(&state.projects.codex, health);
    let mut key_bytes = [0_u8; 32];
    key_bytes[31] = 1;
    let key = SigningKey::from_slice(&key_bytes).unwrap();
    let point = key.verifying_key().to_sec1_point(false);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    state.pairing.lock().await.restore_device(
        "owner".into(),
        wonder_api::pairing_protocol::DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: b64.encode(point.x().unwrap()),
            y: b64.encode(point.y().unwrap()),
        },
        false,
        None,
    );
    // Synthetic authenticated-device fixture; the production approval handler
    // still validates the P-256 signature, nonce, state and durable receipt.
    let app = Router::new()
        .route(
            "/api/v1/conversations/{conversation_id}/turns/{turn_id}/interrupt",
            post(interrupt_turn_http),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/turns/{turn_id}/steer",
            post(steer_turn),
        )
        .route(
            "/api/v1/approvals/{approval_id}/resolve",
            post(resolve_approval),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/history",
            get(conversation_snapshot),
        )
        .layer(Extension(AuthenticatedDevice {
            device_id: "owner".into(),
            session_binding: "test-session".into(),
        }))
        .layer(middleware::from_fn(
            |request: Request<Body>, next: Next| async move {
                let shaped = request.headers().contains_key("x-test-shaped-network");
                let bytes = request
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                if shaped {
                    tokio::time::sleep(Duration::from_micros(50_000 + bytes * 8 / 20)).await;
                }
                let response = next.run(request).await;
                if !shaped {
                    return response;
                }
                let (parts, body) = response.into_parts();
                let body = to_bytes(body, 2_000_000).await.unwrap();
                tokio::time::sleep(Duration::from_micros(50_000 + body.len() as u64 * 8 / 20))
                    .await;
                Response::from_parts(parts, Body::from(body))
            },
        ))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Keep restarting the stalled read before the RPC timeout. Each control
    // sample is taken only after the synthetic runtime confirms history arrived.
    // Retain real TCP/stdio latency measurements (100 samples reproduce gate 05).
    // Shared-host scheduling cannot prove an absolute latency SLA: the regression
    // contract is that each control and local page finish while history is gated.
    let sample_count: usize = std::env::var("WONDER_GATE05_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let p95_index = (sample_count * 95).div_ceil(100) - 1;
    let mut measurements = serde_json::Map::new();
    for shaped in [false, true] {
        for control in ["stop", "steer", "approval"] {
            std::fs::write(dir.path().join("delay-history"), "").unwrap();
            let _ = std::fs::remove_file(dir.path().join("history-waiting"));
            let read_state = state.clone();
            let history = tokio::spawn(async move {
                hydrate_app_server_turn_items(&read_state, "thread", "turn").await
            });
            wait_for(&dir.path().join("history-waiting")).await;
            let mut samples = vec![];
            for i in 0..sample_count + 5 {
                assert!(!history.is_finished());
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
                let (path, body, expected) = match control {
                    "stop" => {
                        let turn = uuid::Uuid::new_v4().to_string();
                        let wonder_store::MessageInsert::Inserted(stop_message) = state
                            .store
                            .insert_message(
                                "owner",
                                &turn,
                                "stop me",
                                "hash",
                                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                                "1700000000000",
                            )
                            .await
                            .unwrap()
                        else {
                            panic!("stop message");
                        };
                        state
                            .store
                            .update_message_delivery(
                                &stop_message.id,
                                "accepted_by_codex",
                                Some("thread"),
                                Some(&turn),
                            )
                            .await
                            .unwrap();
                        (
                            format!("/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/turns/{turn}/interrupt"),
                            serde_json::json!({}),
                            // Requested; the turn ends when Codex reports it.
                            StatusCode::ACCEPTED,
                        )
                    }
                    "steer" => (
                        "/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/turns/turn/steer".into(),
                        serde_json::json!({"deviceId":"owner","clientMessageId":uuid::Uuid::new_v4().to_string(),"body":"Please continue","expectedTurnId":"turn"}),
                        StatusCode::ACCEPTED,
                    ),
                    _ => {
                        let id = uuid::Uuid::new_v4().to_string();
                        state.store.insert_pending_approval(&id,"item/commandExecution/requestApproval",&serde_json::json!({"threadId":"thread","turnId":"turn","itemId":"command","availableDecisions":["accept","decline"],"_wonderRuntimeId":runtime_id,"_wonderRequestId":id}).to_string(),"now").await.unwrap();
                        let approval = state
                            .store
                            .list_pending_approvals()
                            .await
                            .unwrap()
                            .into_iter()
                            .find(|a| a.approval_id == id)
                            .unwrap();
                        let mut request = ApprovalResolution {
                            decision: serde_json::json!("accept"),
                            action_nonce: approval.action_nonce.clone(),
                            expected_state: "pending".into(),
                            idempotency_key: uuid::Uuid::new_v4().to_string(),
                            issued_at_ms: now_ms(),
                            signature: String::new(),
                            response_json: None,
                        };
                        let path = format!("/api/v1/approvals/{id}/resolve");
                        let hash = hex::encode(Sha256::digest(
                            canonical_approval_body(&request).as_bytes(),
                        ));
                        let transcript = ActionTranscript {
                            action: "approval.resolve",
                            target: &path,
                            body_sha256: &hash,
                            action_nonce: &approval.action_nonce,
                            session_binding: "test-session",
                            device_id: "owner",
                            host_installation_id: &state.host_installation_id,
                            issued_at_ms: request.issued_at_ms,
                            expected_state: "pending",
                        };
                        let signature: Signature = key.sign(&transcript.to_bytes());
                        request.signature = b64.encode(signature.to_bytes());
                        (
                            path,
                            serde_json::json!({"decision":request.decision,"actionNonce":request.action_nonce,"expectedState":request.expected_state,"idempotencyKey":request.idempotency_key,"issuedAtMs":request.issued_at_ms,"signature":request.signature}),
                            StatusCode::NO_CONTENT,
                        )
                    }
                };

                let start = Instant::now();
                // A page read is concurrent with every control, on the same pool.
                let (response, page) = tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(
                    http_post(&base, &path, &body, shaped),
                    state.store.conversation_history_page(
                        "epoch",
                        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                        None,
                        100
                    )
                ) }).await.expect("control and local page must finish while the runtime history response stays gated");
                assert!(
                    !history.is_finished(),
                    "the control/page completed only after history stopped waiting"
                );
                assert!(dir.path().join("delay-history").exists());
                let (status, text) = response;
                assert_eq!(status, expected, "{control}: {text}");
                assert!(page.unwrap().is_some());
                if i >= 5 {
                    samples.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            std::fs::remove_file(dir.path().join("delay-history")).unwrap();
            assert!(history.await.unwrap().is_some());
            let mut sorted = samples.clone();
            sorted.sort_by(f64::total_cmp);
            eprintln!(
                "CONTROL_{control}_SHAPED_{shaped}_P95_MS={}; SAMPLES_MS={samples:?}",
                sorted[p95_index]
            );
            measurements.insert(
                format!("{control}_shaped_{shaped}"),
                serde_json::json!({"p95Ms":sorted[p95_index],"samplesMs":samples}),
            );
        }
    }
    eprintln!(
        "CONTROL_MEASUREMENTS={}",
        serde_json::Value::Object(measurements)
    );
    server.abort();
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn refresh_receipt_is_prompt_and_transport_restart_fails_old_reads() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(
        &state,
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    // delay-history holds the runtime's read until this test removes the file,
    // so a receipt that waited for the read would never arrive.
    std::fs::write(dir.path().join("delay-history"), "").unwrap();
    let receipt = tokio::time::timeout(
        Duration::from_secs(60),
        refresh_history_request(
            State(state.clone()),
            Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
            Query(Default::default()),
        ),
    )
    .await
    .expect("the refresh receipt waited for the held runtime read");
    assert_eq!(receipt.status(), StatusCode::ACCEPTED);
    wait_for(&dir.path().join("history-waiting")).await;
    assert_eq!(
        state
            .store
            .history_refresh_status("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap()["state"],
        "refreshing",
        "the receipt came before the held read finished"
    );
    let old = state.projects.codex.lock().await.rpc();
    let config = state
        .projects
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    state
        .projects
        .codex
        .lock()
        .await
        .restart(config)
        .await
        .unwrap();
    assert!(old
        .request("turn/interrupt", serde_json::json!({}))
        .await
        .is_err());
    tokio::time::timeout(Duration::from_secs(60), async {
        while state
            .store
            .history_refresh_status("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap()["state"]
            == "refreshing"
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state
            .store
            .history_refresh_status("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap()["state"],
        "failed"
    );
    std::fs::remove_file(dir.path().join("delay-history")).unwrap();
    std::fs::write(dir.path().join("completed"), "").unwrap();
    assert_eq!(
        refresh_history_request(
            State(state.clone()),
            Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
            Query(Default::default()),
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        while state
            .store
            .history_refresh_status("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap()["state"]
            == "refreshing"
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state
            .store
            .history_refresh_status("d19b0f7e-97bb-45d7-8206-324a62386dd9", now_ms() as i64)
            .await
            .unwrap()["state"],
        "completed"
    );
    let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    let before = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, chat)
        .await
        .unwrap()
        .unwrap();
    assert!(!before.events.is_empty());
    let token = "unchanged-native-read";
    assert!(state
        .store
        .claim_history_refresh(chat, token, now_ms() as i64)
        .await
        .unwrap());
    refresh_runtime_history(&state, chat, "thread", token, false)
        .await
        .unwrap();
    let after = state
        .store
        .committed_conversation_snapshot(&state.host_epoch, chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.last_sequence, before.last_sequence,
        "an unchanged native read must not force another phone replay"
    );
    state
        .store
        .update_history_refresh(chat, token, "completed", now_ms() as i64, None)
        .await
        .unwrap();
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

async fn http_post(
    base: &str,
    path: &str,
    body: &serde_json::Value,
    shaped: bool,
) -> (StatusCode, String) {
    let mut socket = tokio::net::TcpStream::connect(base.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    let payload = body.to_string();
    let shape = if shaped {
        "x-test-shaped-network: 1\r\n"
    } else {
        ""
    };
    let request=format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{shape}\r\n{payload}",payload.len());
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(response).unwrap();
    let status =
        StatusCode::from_u16(text.split_whitespace().nth(1).unwrap().parse().unwrap()).unwrap();
    (status, text)
}

#[tokio::test]
async fn recovery_reads_beyond_thirty_two_pages_without_collecting_other_turns() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(
        &state,
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    std::fs::write(dir.path().join("long-history"), "").unwrap();
    let items = hydrate_app_server_turn_items(&state, "thread", "turn")
        .await
        .unwrap();
    assert_eq!(items.len(), 100);
    assert!(items.iter().all(|item| item.turn_id == "turn"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("requests"))
            .unwrap()
            .lines()
            .filter(|line| *line == "thread/items/list")
            .count(),
        100
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn compact_history_view_omits_unrendered_screenshot_data_only_when_requested() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(
        &state,
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let wonder_store::MessageInsert::Inserted(message) = state
        .store
        .insert_message(
            "owner",
            "shot-client",
            "look",
            "hash",
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            "1700000000000",
        )
        .await
        .unwrap()
    else {
        panic!("message");
    };
    state
        .store
        .update_message_delivery(&message.id, "completed", Some("thread"), Some("turn"))
        .await
        .unwrap();
    let image_url = format!("data:image/png;base64,{}", "A".repeat(200_000));
    let mut event = HostEventEnvelope {
        event_id: "shot".into(),
        host_epoch: state.host_epoch.clone(),
        sequence: 0,
        occurred_at: "1700000001000".into(),
        conversation_id: Some("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        request_id: None,
        device_id: None,
        message_id: Some(message.id.clone()),
        item_id: Some("screenshot-call".into()),
        approval_id: None,
        event: WonderEvent::ComputerUseScreenshot {
            image_url: image_url.clone(),
        },
    };
    state.store.commit_event(&mut event).await.unwrap();
    async fn page(state: &AppState, compact: bool, gzip: bool) -> (usize, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if compact {
            headers.insert(
                history::COMPACT_VIEW_HEADER,
                HeaderValue::from_static("compact"),
            );
        }
        if gzip {
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        }
        let response = conversation_snapshot(
            State(state.clone()),
            Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
            Query(Default::default()),
            headers,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        if gzip {
            assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        }
        let body = to_bytes(response.into_body(), 10_000_000).await.unwrap();
        let len = body.len();
        let decoded = if gzip {
            let mut decoded = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::GzDecoder::new(body.as_ref()),
                &mut decoded,
            )
            .unwrap();
            decoded
        } else {
            body.to_vec()
        };
        (len, serde_json::from_slice(&decoded).unwrap())
    }
    // The same compact contract removes redundant command text, while the
    // full detail remains available to existing native clients on expansion.
    let output = "owned command output\n".repeat(50);
    for (id, text) in [
        ("duplicate", output.as_str()),
        ("distinct", "useful summary"),
    ] {
        let mut command = event.clone();
        command.event_id = id.into();
        command.item_id = Some(id.into());
        command.event = WonderEvent::Activity {
            category: "thread_item_upsert".into(),
            state: "completed".into(),
            detail: thread_item_upsert_detail(
                "turn",
                &serde_json::json!({
                    "id": id, "type": "commandExecution", "command": "printf output",
                    "output": output, "text": text, "status": "completed"
                }),
                "completed",
                None,
            ),
        };
        state.store.commit_event(&mut command).await.unwrap();
    }
    let (full_bytes, full) = page(&state, false, false).await;
    let (compact_bytes, compact) = page(&state, true, false).await;
    let (gzip_bytes, gzipped) = page(&state, true, true).await;
    assert_eq!(gzipped, compact);
    assert!(gzip_bytes < compact_bytes / 2);
    let screenshot = |page: &serde_json::Value| -> serde_json::Value {
        page["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap().iter())
            .find(|item| item["type"] == "imageView")
            .cloned()
            .expect("screenshot item")
    };
    // The web client still receives both copies it renders from.
    assert_eq!(screenshot(&full)["payload"]["imageUrl"], image_url);
    assert!(!full["events"].as_array().unwrap().is_empty());
    // Native clients keep the item (and its turn) but not the unrendered data.
    let item = screenshot(&compact);
    assert!(item["payload"].get("imageUrl").is_none());
    assert_eq!(item["state"], "completed");
    assert_eq!(compact["events"], serde_json::json!([]));
    assert_eq!(compact["lastSequence"], full["lastSequence"]);
    let command = |page: &serde_json::Value| {
        page["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap())
            .find(|item| item["id"] == "duplicate")
            .unwrap()
            .clone()
    };
    assert_eq!(command(&full)["text"], output);
    assert!(command(&compact)["text"].is_null());
    assert_eq!(command(&compact)["payload"]["output"], output);
    assert!(
        compact_bytes < 10_000 && full_bytes > 400_000,
        "{compact_bytes} vs {full_bytes}"
    );
    let mut replay = event.clone();
    history::compact_event(&mut replay);
    assert_eq!(replay.sequence, event.sequence);
    assert!(
        matches!(replay.event, WonderEvent::ComputerUseScreenshot { ref image_url } if image_url.is_empty())
    );
}

fn refresh_envelope(id: &str, detail: serde_json::Value) -> HostEventEnvelope {
    HostEventEnvelope {
        event_id: id.into(),
        host_epoch: "epoch".into(),
        sequence: 0,
        occurred_at: "1000".into(),
        conversation_id: Some("chat".into()),
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        request_id: None,
        device_id: None,
        message_id: None,
        item_id: Some("reply".into()),
        approval_id: None,
        event: WonderEvent::Activity {
            category: "thread_item_upsert".into(),
            state: "updated".into(),
            detail: Some(detail.to_string()),
        },
    }
}

// Codex reports turn start/end in whole seconds plus an exact duration. A
// 1.5s turn must not round to 0s or 1s, and bad clocks invent no time.
#[test]
fn native_turn_timing_reads_provider_clocks() {
    let timing = |turn: serde_json::Value| {
        let timing = native_turn_timing(&turn);
        (timing.started_ms, timing.completed_ms)
    };
    let cases = [
        (
            serde_json::json!({"startedAt":100,"completedAt":101,"durationMs":1500}),
            (Some(100_000), Some(101_500)),
        ),
        (
            serde_json::json!({"startedAt":100,"completedAt":160}),
            (Some(100_000), Some(160_000)),
        ),
        (
            serde_json::json!({"startedAt":100,"completedAt":null,"durationMs":null}),
            (Some(100_000), None),
        ),
        (
            serde_json::json!({"startedAt":100,"durationMs":1500}),
            (Some(100_000), None),
        ),
        (
            serde_json::json!({"completedAt":160}),
            (None, Some(160_000)),
        ),
        (
            serde_json::json!({"startedAt":160,"completedAt":100}),
            (Some(160_000), None),
        ),
        (
            serde_json::json!({"startedAt":-1,"completedAt":"160"}),
            (None, None),
        ),
        (serde_json::json!({"id":"turn"}), (None, None)),
    ];
    for (turn, expected) in cases {
        assert_eq!(timing(turn.clone()), expected, "{turn}");
    }
}

// A turn run on the Mac reaches Cupola as one batch, so the Mac's start and
// finish receipts share a timestamp ("Worked for 0s"). Refreshed provider
// clocks give the turn's real length.
#[test]
fn refreshed_provider_turn_clocks_replace_batched_mac_receipts() {
    let item = serde_json::json!({"id":"reply", "type":"agentMessage", "text":"Done"});
    let mut detail: serde_json::Value =
        serde_json::from_str(&thread_item_upsert_detail("turn", &item, "completed", None).unwrap())
            .unwrap();
    detail["historyRefresh"] = true.into();
    detail["turnStatus"] = "completed".into();
    let receipt = |id: &str, state: DeliveryState| {
        let mut event = refresh_envelope(id, serde_json::json!({}));
        event.occurred_at = "2026-10-21T08:00:30.000000001Z".into();
        event.event = WonderEvent::MessageState { state };
        event
    };
    let batch = [
        receipt("accepted", DeliveryState::AcceptedByCodex),
        receipt("completed", DeliveryState::Completed),
    ];
    let project = |detail: &serde_json::Value| {
        let mut events = batch.to_vec();
        events.push(refresh_envelope("refresh", detail.clone()));
        let turn = conversation_thread_projection_with_items(None, &[], &[], &events, &[], None)
            .turns
            .remove(0);
        (turn.started_at, turn.completed_at)
    };
    assert_eq!(
        project(&detail),
        (
            Some("2026-10-21T08:00:30.000000001Z".into()),
            Some("2026-10-21T08:00:30.000000001Z".into())
        ),
        "without provider clocks the Mac's receipts stand"
    );
    // 2026-10-21T08:00:00Z, finished 75.25s later.
    detail["turnStartedAtMs"] = 1_792_569_600_000_i64.into();
    detail["turnCompletedAtMs"] = 1_792_569_675_250_i64.into();
    assert_eq!(
        project(&detail),
        (
            Some("2026-10-21T08:00:00.000Z".into()),
            Some("2026-10-21T08:01:15.250Z".into())
        )
    );
}

// Contract: Codex's turn clocks read by a history refresh are stored with the
// refreshed items and reach the phone's history page, over batched Mac
// receipts, and survive a later unchanged refresh.
#[tokio::test]
async fn refreshed_provider_turn_clocks_reach_the_phone_history_page() {
    let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    let (dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(
        &state,
        chat,
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    // The Mac caught the turn in one batch: start and finish share a receipt.
    for (id, delivery) in [
        ("accepted", DeliveryState::AcceptedByCodex),
        ("finished", DeliveryState::Completed),
    ] {
        let mut event = refresh_envelope(id, serde_json::json!({}));
        event.host_epoch = state.host_epoch.clone();
        event.conversation_id = Some(chat.into());
        event.item_id = None;
        event.occurred_at = "2026-10-21T08:05:00.000Z".into();
        event.event = WonderEvent::MessageState { state: delivery };
        state.store.commit_event(&mut event).await.unwrap();
    }
    std::fs::write(dir.path().join("completed"), "").unwrap();
    // 2026-10-21T08:00:00Z, 75.25s long (completedAt is whole seconds).
    std::fs::write(
        dir.path().join("turn-pages.json"),
        serde_json::json!([[{"id":"turn", "status":"completed",
            "startedAt":1_792_569_600, "completedAt":1_792_569_675, "durationMs":75_250}]])
        .to_string(),
    )
    .unwrap();
    async fn phone_turn(state: &AppState, chat: &str) -> (serde_json::Value, serde_json::Value) {
        let response = conversation_snapshot(
            State(state.clone()),
            Path(chat.into()),
            Query(Default::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 10_000_000).await.unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let turn = page["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|turn| turn["id"] == "turn")
            .cloned()
            .expect("refreshed turn");
        (turn["startedAt"].clone(), turn["completedAt"].clone())
    }
    assert_eq!(
        phone_turn(&state, chat).await,
        (
            serde_json::json!("2026-10-21T08:05:00.000Z"),
            serde_json::json!("2026-10-21T08:05:00.000Z")
        ),
        "before refresh the batched receipts say 0s"
    );
    let provider = (
        serde_json::json!("2026-10-21T08:00:00.000Z"),
        serde_json::json!("2026-10-21T08:01:15.250Z"),
    );
    for token in ["first-read", "unchanged-read"] {
        assert!(state
            .store
            .claim_history_refresh(chat, token, now_ms() as i64)
            .await
            .unwrap());
        refresh_runtime_history(&state, chat, "thread", token, false)
            .await
            .unwrap();
        state
            .store
            .update_history_refresh(chat, token, "completed", now_ms() as i64, None)
            .await
            .unwrap();
        assert_eq!(phone_turn(&state, chat).await, provider, "{token}");
    }
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// A turn still running in a desktop or terminal app is shown as running there,
// and only that explicit marker lets refreshed history claim "inProgress".
#[test]
fn refreshed_desktop_turn_runs_elsewhere_until_it_finishes() {
    let item = serde_json::json!({"id":"reply", "type":"agentMessage", "text":"Working", "status":"inProgress"});
    let detail = |status: &str, elsewhere: bool| {
        let mut value: serde_json::Value = serde_json::from_str(
            &thread_item_upsert_detail("turn", &item, "updated", None).unwrap(),
        )
        .unwrap();
        value["historyRefresh"] = serde_json::json!(true);
        value["turnStatus"] = serde_json::json!(status);
        if elsewhere {
            value["turnRunningElsewhere"] = serde_json::json!(true);
        }
        value
    };
    let project = |detail: serde_json::Value| {
        conversation_thread_projection_with_items(
            None,
            &[],
            &[],
            &[refresh_envelope("e", detail)],
            &[],
            None,
        )
    };
    let running = project(detail("inProgress", true));
    assert_eq!(running.turns[0].status, "inProgress");
    assert!(running.turns[0].running_elsewhere);
    assert_eq!(
        serde_json::to_value(&running.turns[0]).unwrap()["runningElsewhere"],
        true
    );
    let unmarked = project(detail("inProgress", false));
    assert_ne!(unmarked.turns[0].status, "inProgress");
    // A partially refreshed page may contain a newer running observation
    // alongside the old foreign-process interruption fallback. Native clocks
    // win even across daemon epochs, where journal sequence numbers restart.
    let mut old = refresh_envelope("old", detail("interrupted", false));
    old.sequence = 9999;
    let mut current = detail("inProgress", true);
    current["turnStatusObservedAtMs"] = 1000.into();
    let fresh = refresh_envelope("new", current);
    for events in [
        vec![old.clone(), fresh.clone()],
        vec![fresh.clone(), old.clone()],
    ] {
        let projection =
            conversation_thread_projection_with_items(None, &[], &[], &events, &[], None);
        assert_eq!(projection.turns[0].status, "inProgress");
        assert!(projection.turns[0].running_elsewhere);
    }
    let mut actual = old.clone();
    actual.event = WonderEvent::MessageState {
        state: DeliveryState::Interrupted,
    };
    let projection =
        conversation_thread_projection_with_items(None, &[], &[], &[fresh, actual], &[], None);
    assert_eq!(
        projection.turns[0].status, "interrupted",
        "a real local receipt remains authoritative"
    );
    let finished = project(detail("completed", false));
    assert_eq!(finished.turns[0].status, "completed");
    assert!(serde_json::to_value(&finished.turns[0])
        .unwrap()
        .get("runningElsewhere")
        .is_none());
}

// Live completion and refreshed history must preserve the same long message,
// including safe text beyond the former tool-detail ceiling.
#[tokio::test]
async fn long_messages_survive_typed_live_and_durable_history() {
    let store = wonder_store::Store::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::test_support::project_thread(
        &store,
        "chat",
        "Project",
        "/workspace",
        "fixture-store",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    store.start_event_epoch("epoch").await.unwrap();
    let prefix = "é🙂".repeat(8192);
    let raw =
        format!("{prefix}\u{1b}[31m end sk-secret ghp_secret Bearer secret token=secret\u{1b}[0m");
    let expected = format!("{prefix} end [redacted] [redacted] [redacted] [redacted]");
    let cases = [
        serde_json::json!({"id":"agent", "type":"agentMessage", "text":raw}),
        serde_json::json!({"id":"user", "type":"userMessage", "content":[{"type":"text", "text":raw}]}),
        serde_json::json!({"id":"parts", "type":"agentMessage", "parts":[{"text":prefix}, {"text":"\u{1b}[31m end sk-secret ghp_secret Bearer secret token=secret\u{1b}[0m"}]}),
    ];
    for (index, mut raw) in cases.into_iter().enumerate() {
        raw["status"] = "completed".into();
        raw["savedPath"] = "/workspace/report.md".into();
        raw["nested"] = serde_json::json!({"filePath":"/private/report.md", "password":"private", "text":"m".repeat(20000)});
        let typed = typed_thread_item_projection(
            &AppServerThreadItem {
                turn_id: "turn".into(),
                item: raw.clone(),
            },
            "1000",
            Some("/workspace"),
        );
        let check = |item: &ConversationThreadItem| {
            assert_eq!(
                item.text.as_ref().map(String::len),
                Some(expected.len()),
                "{} display length",
                item.id
            );
            assert!(
                item.text.as_deref() == Some(expected.as_str()),
                "{} display content",
                item.id
            );
            assert!(
                extract_text(&item.payload).as_deref() == Some(expected.as_str()),
                "{} payload content",
                item.id
            );
            assert_eq!(item.payload["savedPath"], "report.md");
            assert_eq!(
                item.payload["nested"]["filePath"],
                "[path outside workspace]"
            );
            assert_eq!(item.payload["nested"]["password"], "[redacted]");
            assert_eq!(
                item.payload["nested"]["text"].as_str().unwrap().len(),
                16 * 1024
            );
        };
        check(&typed);
        for refresh in [false, true] {
            let mut detail: serde_json::Value = serde_json::from_str(
                &thread_item_upsert_detail_with_authority(
                    "turn",
                    &raw,
                    "completed",
                    2,
                    Some("/workspace"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                detail["text"].as_str(),
                Some(expected.as_str()),
                "live display text"
            );
            detail["historyRefresh"] = refresh.into();
            let mut event = refresh_envelope(&format!("{index}-{refresh}"), detail);
            store.commit_event(&mut event).await.unwrap();
            let page = store
                .conversation_history_page("epoch", "chat", None, 100)
                .await
                .unwrap()
                .unwrap();
            let projected = conversation_thread_projection_with_items(
                None,
                &page.messages,
                &page.assistant_messages,
                &page.events,
                &[],
                Some(&page.workspace_path),
            );
            let item = projected
                .turns
                .iter()
                .flat_map(|turn| &turn.items)
                .find(|item| item.id == typed.id)
                .unwrap();
            check(item);
        }
    }
}

#[test]
fn message_text_ceiling_is_visible_utf8_safe_and_keeps_tool_limits() {
    let large = format!("{}🙂tail", "é".repeat(MESSAGE_TEXT_BYTES / 2 - 1));
    for item in [
        serde_json::json!({"type":"agentMessage", "text":large}),
        serde_json::json!({"type":"userMessage", "content":[{"type":"text", "text":large}]}),
        serde_json::json!({"type":"agentMessage", "output":large}),
    ] {
        let safe = sanitize_typed_item(&item, None);
        let text = typed_item_display_text(&safe).unwrap();
        assert!(text.len() <= MESSAGE_TEXT_BYTES);
        assert!(text.ends_with(MESSAGE_TRUNCATED));
        assert!(text.starts_with("éé"));
        assert_eq!(sanitize_typed_item(&safe, None), safe);
    }
    let exact = "é".repeat(MESSAGE_TEXT_BYTES / 2);
    let safe = sanitize_typed_item(
        &serde_json::json!({"type":"agentMessage", "text":exact}),
        None,
    );
    assert_eq!(
        typed_item_display_text(&safe).as_deref(),
        Some(exact.as_str())
    );
    let split =
        serde_json::json!({"type":"userMessage", "content":[{"text":exact}, {"text":"more"}]});
    let display = typed_item_display_text(&sanitize_typed_item(&split, None)).unwrap();
    assert!(display.len() <= MESSAGE_TEXT_BYTES && display.ends_with(MESSAGE_TRUNCATED));
    let command = sanitize_typed_item(
        &serde_json::json!({"type":"commandExecution", "aggregatedOutput":"x".repeat(70000)}),
        None,
    );
    assert_eq!(command["output"].as_str().unwrap().len(), 16 * 1024);
    let tool = sanitize_typed_item(
        &serde_json::json!({"type":"mcpToolCall", "result":{"text":"x".repeat(70000)}}),
        None,
    );
    assert_eq!(tool["result"], "[tool result truncated]");
}

#[test]
fn large_computer_results_keep_only_bounded_native_app_identity() {
    let surface = serde_json::json!({"kind":"computerUse", "app":{"kind":"appId", "appId":"com.apple.TextEdit"}});
    for (server, metadata, retained) in [
        ("cua_repl", surface.clone(), true),
        ("github", surface.clone(), false),
        (
            "cua_repl",
            serde_json::json!({"kind":"computerUse", "app":{"kind":"appId", "appId":"bad#id"}}),
            false,
        ),
        (
            "cua_repl",
            serde_json::json!({"kind":"browser", "app":null}),
            false,
        ),
    ] {
        let safe = sanitize_typed_item(
            &serde_json::json!({
                "type":"mcpToolCall", "server":server,
                "result":{"_meta":{"codex/toolSurface":metadata}, "content":[{"text":"x".repeat(70000)}]}
            }),
            None,
        );
        if retained {
            assert_eq!(safe["result"]["_meta"]["codex/toolSurface"], surface);
            assert_eq!(
                safe["result"]["content"][0]["text"],
                "[tool result truncated]"
            );
            assert!(serde_json::to_vec(&safe).unwrap().len() < 1024);
            assert_eq!(sanitize_typed_item(&safe, None), safe);
        } else {
            assert_eq!(safe["result"], "[tool result truncated]");
        }
    }
}

#[test]
fn key_redaction_ignores_prefixes_inside_words() {
    let clean = sanitize_ansi_and_secrets(
        "<task-id>b1</task-id> disk-usage sk-live123 Bearer abc token=xyz",
        1024,
    );
    assert_eq!(
        clean,
        "<task-id>b1</task-id> disk-usage [redacted] [redacted] [redacted]"
    );
    let prefix = format!("{} ", "x".repeat(16 * 1024 - 3));
    let item = serde_json::json!({"type":"agentMessage", "text":format!("{prefix}sk-secret\u{1b}[0m end")});
    let safe = sanitize_typed_item(&item, None);
    assert_eq!(
        typed_item_display_text(&safe),
        Some(format!("{prefix}[redacted] end"))
    );
}

// Contract: one display projection for live items and history. Terminal
// controls and Codex's attribution envelope render as readable content, while
// user pastes, code examples, JSON, IDs and repeated prose retain their words.
#[test]
fn rendering_fixtures_preserve_content_and_remove_transport_markup() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/protocol/render-sanitize/host-cases.json"
    ))
    .unwrap();
    for case in cases {
        let input = case["input"].as_str().unwrap();
        let expected = case["expected"].as_str().unwrap();
        let item = serde_json::json!({"id":"synthetic-item", "type":case["type"], "text":input});
        let safe = sanitize_typed_item(&item, None);
        assert_eq!(
            typed_item_display_text(&safe).as_deref(),
            Some(expected),
            "{}",
            case["name"]
        );
        assert_eq!(
            sanitize_typed_item(&safe, None),
            safe,
            "idempotent {}",
            case["name"]
        );
        assert_eq!(
            typed_item_display_text(&item).as_deref(),
            Some(expected),
            "direct {}",
            case["name"]
        );
    }
    let raw = "\u{1b}]8;;https://example.invalid\u{7}log\u{1b}]8;;\u{7}";
    let command = sanitize_typed_item(
        &serde_json::json!({"type":"commandExecution", "command":"check", "output":raw}),
        None,
    );
    assert_eq!(command["output"], "log");
    assert_eq!(
        sanitize_ansi_and_secrets("\u{1b}]0;title\u{7}雪abc", 4),
        "雪a"
    );
}

#[tokio::test]
async fn claude_post_result_suggestion_is_a_completed_item_not_an_assistant_reply() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let conversation = "519b0f7e-97bb-45d7-8206-324a62386dd9";
    crate::test_support::bind_project_thread(
        &state,
        conversation,
        AgentFamily::Claude,
        "claude-thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let wonder_store::MessageInsert::Inserted(message) = state
        .store
        .insert_dispatch_message(
            "owner",
            "suggestion-source",
            "Review the layout",
            "hash",
            conversation,
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
            Some("claude-thread"),
            Some("turn"),
        )
        .await
        .unwrap();
    for (method, params) in [
        (
            "turn/completed",
            serde_json::json!({"threadId":"claude-thread", "turnId":"turn", "agentFamily":"claude",
            "turn":{"id":"turn","status":"completed"}}),
        ),
        (
            "item/completed",
            serde_json::json!({"threadId":"claude-thread", "turnId":"turn", "agentFamily":"claude",
            "item":{"id":"hint", "type":"promptSuggestion", "status":"completed", "agentFamily":"claude", "text":"Check the keyboard layout"}}),
        ),
    ] {
        assert!(
            crate::process_app_server_notification(
                &state,
                serde_json::json!({"method":method,"params":params}),
                false
            )
            .await
        );
    }
    let page = state
        .store
        .conversation_history_page(&state.host_epoch, conversation, None, 50)
        .await
        .unwrap()
        .unwrap();
    assert!(
        page.assistant_messages.is_empty(),
        "a hint must never become an assistant bubble"
    );
    let projection = conversation_thread_projection(
        None,
        &page.messages,
        &page.assistant_messages,
        &page.events,
    );
    let turn = projection
        .turns
        .iter()
        .find(|turn| turn.id == "turn")
        .unwrap();
    assert_eq!(turn.status, "completed");
    let hint = turn.items.iter().find(|item| item.id == "hint").unwrap();
    assert_eq!(hint.item_type, "promptSuggestion");
    assert_eq!(hint.state, "completed");
    assert_eq!(hint.text.as_deref(), Some("Check the keyboard layout"));
    assert_eq!(hint.payload["agentFamily"], "claude");
}

#[tokio::test]
async fn codex_turn_diff_becomes_the_turns_edited_files() {
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
    let wonder_store::MessageInsert::Inserted(message) = state
        .store
        .insert_dispatch_message(
            "owner",
            "edit",
            "edit the app",
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
            Some("turn"),
        )
        .await
        .unwrap();
    let notify = |method: &str, params: serde_json::Value| {
        crate::process_app_server_notification(
            &state,
            serde_json::json!({"method": method, "params": params}),
            false,
        )
    };
    // One row-level edit, while the turn also wrote a file through the shell.
    assert!(notify("item/completed", serde_json::json!({"threadId": "thread", "turnId": "turn",
        "item": {"id": "edit", "type": "fileChange", "status": "completed",
                 "changes": [{"path": "app.js", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-a\n+b"}]}})).await);
    assert!(
        notify(
            "turn/diff/updated",
            serde_json::json!({"threadId": "thread", "turnId": "turn",
        "diff": "diff --git a/app.js b/app.js\n--- a/app.js\n+++ b/app.js\n@@ -1 +1 @@\n-a\n+b\n"})
        )
        .await
    );
    assert!(notify("turn/diff/updated", serde_json::json!({"threadId": "thread", "turnId": "turn",
        "diff": "diff --git a/app.js b/app.js\n--- a/app.js\n+++ b/app.js\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/comic.html b/comic.html\nnew file mode 100644\n--- /dev/null\n+++ b/comic.html\n@@ -0,0 +1,3 @@\n+<html>\n+<body>\n+</html>\n"})).await);
    assert!(
        notify(
            "turn/completed",
            serde_json::json!({"threadId": "thread",
        "turn": {"id": "turn", "status": "completed"}})
        )
        .await
    );

    let page = state
        .store
        .conversation_history_page(
            &state.host_epoch,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            None,
            50,
        )
        .await
        .unwrap()
        .unwrap();
    let projection = conversation_thread_projection(
        None,
        &page.messages,
        &page.assistant_messages,
        &page.events,
    );
    let turn = projection
        .turns
        .iter()
        .find(|turn| turn.id == "turn")
        .unwrap();
    let edited = turn
        .edited_files
        .as_ref()
        .expect("turn diff saved on the turn");
    assert_eq!(edited["paths"], serde_json::json!(["app.js", "comic.html"]));
    assert_eq!(
        (edited["additions"].as_u64(), edited["deletions"].as_u64()),
        (Some(4), Some(1))
    );
    assert!(
        turn.items.iter().all(|item| item.item_type != "turnDiff"),
        "the diff is not a row"
    );
    assert_eq!(
        turn.items
            .iter()
            .filter(|item| item.item_type == "fileChange")
            .count(),
        1
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// Owner (Oct 12): four messages the Mac could not deliver looked sent on the
// phone. History carries each undelivered message's reason for its bubble.
#[tokio::test]
async fn an_undelivered_message_carries_its_reason_in_history() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(
        &state,
        "d19b0f7e-97bb-45d7-8206-324a62386dd9",
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let mut ids = Vec::new();
    for client in ["sent-client", "failed-client"] {
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_message(
                "owner",
                client,
                client,
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "1700000000000",
            )
            .await
            .unwrap()
        else {
            panic!("message");
        };
        ids.push(message.id);
    }
    let reason = "This image's format can't be read on your Mac. Attach it as a PNG or JPEG.";
    state
        .store
        .update_message_delivery(&ids[1], "safe_to_retry", None, None)
        .await
        .unwrap();
    state
        .store
        .set_message_delivery_error(&ids[1], reason)
        .await
        .unwrap();
    // A reason never sticks to a message that is not undelivered.
    state
        .store
        .set_message_delivery_error(&ids[0], "ignored")
        .await
        .unwrap();
    let response = conversation_snapshot(
        State(state.clone()),
        Path("d19b0f7e-97bb-45d7-8206-324a62386dd9".into()),
        Query(Default::default()),
        HeaderMap::new(),
    )
    .await;
    let body = to_bytes(response.into_body(), 10_000_000).await.unwrap();
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let by_id = |id: &str| {
        page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["messageId"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(by_id(&ids[1])["deliveryError"], reason);
    assert!(by_id(&ids[0]).get("deliveryError").is_none());
    // Sending it again clears the reason.
    assert!(state
        .store
        .requeue_message_for_retry(&ids[1])
        .await
        .unwrap());
    assert_eq!(
        state.store.message_delivery_error(&ids[1]).await.unwrap(),
        None
    );
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

#[test]
fn native_tool_targets_survive_sanitization_without_secrets_or_unbounded_data() {
    let command = serde_json::json!({"type":"commandExecution", "command":"cat a.swift", "processId":"private",
        "commandActions":[{"type":"read","name":"a.swift","path":"/p/a.swift","command":"cat a.swift"},
            {"type":"search","query":"token=abcdef1234567890", "path":"/outside/private"},
            {"type":"unknown","command":"opaque"}]});
    let safe = sanitize_typed_item(&command, Some("/p"));
    assert_eq!(safe["commandActions"][0]["path"], "a.swift");
    assert_eq!(
        safe["commandActions"][1]["path"],
        "[path outside workspace]"
    );
    assert!(!safe.to_string().contains("abcdef1234567890"));
    assert!(safe.get("processId").is_none());
    assert!(safe["commandActions"][0].get("command").is_none());
    assert_eq!(safe["commandActions"][2]["type"], "unknown");
    let tool = serde_json::json!({"type":"mcpToolCall", "toolAction":{"kind":"read", "paths": vec!["/p/a.swift"; 99],
        "query":"x".repeat(9_000), "description":"token=abcdef1234567890", "unexpected":"private"}});
    let safe = sanitize_typed_item(&tool, Some("/p"));
    assert_eq!(safe["toolAction"]["paths"].as_array().unwrap().len(), 32);
    assert_eq!(safe["toolAction"]["paths"][0], "a.swift");
    assert_eq!(safe["toolAction"]["pathsTruncated"], true);
    assert!(safe["toolAction"]["query"].as_str().unwrap().len() <= 512);
    assert!(safe["toolAction"].get("unexpected").is_none());
    assert!(!safe.to_string().contains("abcdef1234567890"));
    assert_eq!(sanitize_typed_item(&safe, Some("/p")), safe);
}

// An agent's code errors after an edit reach the phone for changed files only,
// with workspace-relative paths, bounded and without marking the edit failed.
#[test]
fn edit_diagnostics_keep_changed_paths_bounded_and_the_edit_completed() {
    let error = |path: &str, line: u64, message: String| serde_json::json!({"path": path, "line": line, "column": 5, "message": message});
    let mut diagnostics = vec![
        error(
            "/p/src/main.ts",
            1,
            "Type 'string' is not assignable. token=abcdef1234567890".into(),
        ),
        error("/p/src/other.ts", 1, "Not changed by this edit".into()),
        error("/elsewhere/x.ts", 1, "Outside".into()),
        serde_json::json!({"path": "/p/src/main.ts", "line": 0, "column": 1, "message": "Bad line"}),
        serde_json::json!({"path": "/p/src/main.ts", "message": "No position"}),
    ];
    diagnostics.extend((0..20).map(|line| error("/p/src/main.ts", line + 2, "m".repeat(5000))));
    let edit = serde_json::json!({"type":"fileChange", "id":"e", "status":"completed",
        "changes":[{"path":"/p/src/main.ts", "kind":{"type":"update"}, "diff":"@@ @@\n-a\n+b"}],
        "diagnostics": diagnostics, "diagnosticsReporter": "OpenCode"});
    let safe = sanitize_typed_item(&edit, Some("/p"));
    let kept = safe["diagnostics"].as_array().unwrap();
    assert_eq!(kept.len(), 10);
    assert_eq!(kept[0]["path"], "src/main.ts");
    assert_eq!(kept[0]["line"], 1);
    assert_eq!(kept[0]["column"], 5);
    assert!(kept[0]["message"]
        .as_str()
        .unwrap()
        .starts_with("Type 'string' is not assignable."));
    assert!(kept.iter().all(|entry| entry["path"] == "src/main.ts"
        && entry["message"].as_str().unwrap().len() <= 512 + 3));
    assert_eq!(safe["diagnosticsReporter"], "OpenCode");
    assert!(safe.get("error").is_none());
    assert!(!safe.to_string().contains("abcdef1234567890"));
    assert_eq!(sanitize_typed_item(&safe, Some("/p")), safe);
    let mut clean = edit.clone();
    clean["diagnostics"] = serde_json::json!("malformed");
    let safe = sanitize_typed_item(&clean, Some("/p"));
    assert!(safe.get("diagnostics").is_none());
    assert!(safe.get("diagnosticsReporter").is_none());
    let agent = serde_json::json!({"type":"mcpToolCall", "toolAction":{"kind":"spawnAgent", "description":"Audit the parser"}});
    let safe = sanitize_typed_item(&agent, Some("/p"));
    assert_eq!(
        safe["toolAction"],
        serde_json::json!({"kind":"spawnAgent", "description":"Audit the parser"})
    );
}

#[tokio::test]
async fn thinking_history_keeps_both_clocks_after_restart_pruning_and_single_item_paging() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("thinking.db").display()
    );
    let store = wonder_store::Store::connect(&url).await.unwrap();
    crate::test_support::project_thread(
        &store,
        "chat",
        "Project",
        "/tmp",
        "fixture-store",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    store.start_event_epoch("epoch").await.unwrap();
    for (id, at, state, refresh) in [
        ("start", "1700000000000", "started", false),
        ("duplicate-start", "1700000005000", "started", false),
        ("end", "1700000012900", "completed", false),
        ("duplicate-end", "1700000015000", "completed", false),
        ("hydrate", "1700000900000", "completed", true),
    ] {
        let mut detail: serde_json::Value = serde_json::from_str(&thread_item_upsert_detail_with_authority("turn",
            &serde_json::json!({"id":"thinking", "type":"reasoning", "summary":["private"], "createdAt":"100"}),
            state, if refresh {0} else {2}, None).unwrap()).unwrap();
        if refresh {
            detail["historyRefresh"] = true.into();
        }
        let mut event = HostEventEnvelope {
            event_id: id.into(),
            host_epoch: "epoch".into(),
            sequence: 0,
            occurred_at: at.into(),
            conversation_id: Some("chat".into()),
            thread_id: Some("thread".into()),
            turn_id: Some("turn".into()),
            item_id: Some("thinking".into()),
            request_id: None,
            device_id: None,
            message_id: None,
            approval_id: None,
            event: WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(detail.to_string()),
            },
        };
        store.commit_event(&mut event).await.unwrap();
    }
    drop(store);
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::query("UPDATE replay_retention SET retained_at=0")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM history_entries WHERE source='event'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
    pool.close().await;
    let store = wonder_store::Store::connect(&url).await.unwrap();
    store.start_event_epoch("restarted").await.unwrap();
    store.prune_replay().await.unwrap();
    let mut cursor = None;
    let mut count = 0;
    loop {
        let page = store
            .conversation_history_page("restarted", "chat", cursor.as_ref(), 1)
            .await
            .unwrap()
            .unwrap();
        if page.events.is_empty() {
            break;
        }
        let mut projected = conversation_thread_projection(
            None,
            &page.messages,
            &page.assistant_messages,
            &page.events,
        );
        assert_eq!(
            projected.turns[0].items[0].payload["summary"],
            serde_json::json!(["private"])
        );
        hide_thinking(&mut projected);
        let item = &projected.turns[0].items[0];
        assert_eq!(item.state, "completed");
        assert_eq!(
            item.payload["thinkingTiming"]["startedAtMs"],
            1700000000000i64
        );
        assert_eq!(
            item.payload["thinkingTiming"]["completedAtMs"],
            1700000012900i64
        );
        assert!(!serde_json::to_string(&projected)
            .unwrap()
            .contains("private"));
        count += 1;
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, 3);
}

#[tokio::test]
async fn a_native_history_page_with_a_reset_token_cannot_repopulate_history() {
    let fx = crate::provider_switch_tests::fx().await;
    let first = fx
        .queue(
            "/clear",
            Some((wonder_store::AgentFamily::Claude, "claude:sonnet")),
        )
        .await;
    fx.release(&first).await.unwrap();
    let thread = crate::provider_switch_tests::CLAUDE_THREAD;
    fx.write("claude-items.json", &serde_json::json!({thread: [{"turnId":"unseen-old-turn", "item":{"id":"old-reply","type":"agentMessage","text":"Old native page arriving after reset"}}]}));
    fx.state
        .store
        .claim_history_refresh(&fx.chat, "old-reader", now_ms() as i64)
        .await
        .unwrap();
    fx.state
        .store
        .reset_project_native_session(
            thread,
            crate::provider_switch_tests::CLAUDE_SESSION,
            "11111111-2222-4333-8444-555555555555",
            &[],
            "reset",
        )
        .await
        .unwrap();
    let result = refresh_runtime_history(&fx.state, &fx.chat, thread, "old-reader", false).await;
    assert!(result.is_err());
    let after = fx
        .state
        .store
        .committed_conversation_snapshot(&fx.state.host_epoch, &fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.history_revision, 1);
    assert!(after.messages.is_empty());
    assert!(after.assistant_messages.is_empty());
    assert!(
        after.events.is_empty(),
        "the superseded page must be rejected before its first item is published"
    );
}

// Contract (reasoning-summary-v1 fixture): a Codex reasoning item keeps only
// its lifecycle and its redacted, bounded summary, never raw content; the
// summary becomes the row's text; hiding removes both and nothing else.
#[test]
fn codex_thinking_summary_is_kept_bounded_and_hidden_by_default() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/reasoning-summary-v1.json"
    ))
    .unwrap();
    let safe = sanitize_typed_item(&fixture["codexItem"], None);
    assert_eq!(safe, fixture["thinking"]);
    assert_eq!(sanitize_typed_item(&safe, None), safe);
    crate::tests::validate_http_contract("reasoningItemPayload", &safe);
    crate::tests::validate_http_contract("reasoningItemPayload", &fixture["hidden"]);
    assert_eq!(
        typed_item_display_text(&safe).as_deref(),
        fixture["thinkingText"].as_str()
    );

    let bounded = sanitize_typed_item(
        &serde_json::json!({"type":"reasoning", "status":"completed", "summary":
            (0..40).map(|_| "x".repeat(10_000)).chain([String::new()]).collect::<Vec<_>>()}),
        None,
    );
    let parts = bounded["summary"].as_array().unwrap();
    assert_eq!(parts.len(), 16);
    assert!(parts
        .iter()
        .all(|part| part.as_str().unwrap().len() <= 4096));
    assert_eq!(sanitize_typed_item(&bounded, None), bounded);
    crate::tests::validate_http_contract("reasoningItemPayload", &bounded);
    // Claude's thinking (no summary) is unchanged: lifecycle only, no text.
    let claude = sanitize_typed_item(
        &serde_json::json!({"id":"t", "type":"reasoning", "status":"inProgress"}),
        None,
    );
    assert_eq!(
        claude,
        serde_json::json!({"id":"t", "type":"reasoning", "status":"inProgress"})
    );
    assert_eq!(typed_item_display_text(&claude), None);

    let detail = thread_item_upsert_detail_with_authority(
        "turn",
        &fixture["codexItem"],
        "completed",
        2,
        None,
    )
    .unwrap();
    let mut event = HostEventEnvelope {
        event_id: "e".into(),
        host_epoch: "epoch".into(),
        sequence: 1,
        occurred_at: "1".into(),
        conversation_id: Some("chat".into()),
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        item_id: Some("rs_1".into()),
        request_id: None,
        device_id: None,
        message_id: None,
        approval_id: None,
        event: WonderEvent::Activity {
            category: "thread_item_upsert".into(),
            state: "updated".into(),
            detail: Some(detail),
        },
    };
    let mut thread = conversation_thread_projection(None, &[], &[], &[event.clone()]);
    let item = &thread.turns[0].items[0];
    assert_eq!(item.text.as_deref(), fixture["thinkingText"].as_str());
    assert_eq!(item.payload["summary"], fixture["thinking"]["summary"]);
    assert!(!serde_json::to_string(&thread)
        .unwrap()
        .contains("raw chain"));
    hide_thinking(&mut thread);
    let item = &thread.turns[0].items[0];
    assert_eq!(item.text, None);
    assert!(item.payload.get("summary").is_none());
    assert_eq!(item.state, "completed");

    hide_thinking_event(&mut event);
    let WonderEvent::Activity {
        detail: Some(detail),
        ..
    } = &event.event
    else {
        panic!("activity")
    };
    let detail: serde_json::Value = serde_json::from_str(detail).unwrap();
    assert_eq!(detail["item"], fixture["hidden"]);
    assert!(detail.get("text").is_none());
    assert_eq!(detail["itemType"], "reasoning");
    assert_eq!(detail["state"], "completed");
}

// Contract: the history snapshot sends Codex thinking text, in its thread and
// its raw events, only while Codex's chat detail is Thinking.
#[tokio::test]
async fn history_shows_codex_thinking_text_only_at_the_thinking_level() {
    use crate::test_support::call;
    let (_dir, state) = crate::test_support::fixture().await;
    crate::test_support::project_thread(
        &state.store,
        "chat",
        "Project",
        "/tmp",
        "fixture-store",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    let detail = thread_item_upsert_detail_with_authority(
        "turn",
        &serde_json::json!({"id":"rs_1", "type":"reasoning", "status":"completed",
            "summary":["Reading the parser"], "content":["raw chain"]}),
        "completed",
        2,
        None,
    )
    .unwrap();
    let mut event = HostEventEnvelope {
        event_id: "thinking".into(),
        host_epoch: state.host_epoch.clone(),
        sequence: 0,
        occurred_at: "1700000000000".into(),
        conversation_id: Some("chat".into()),
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        item_id: Some("rs_1".into()),
        request_id: None,
        device_id: None,
        message_id: None,
        approval_id: None,
        event: WonderEvent::Activity {
            category: "thread_item_upsert".into(),
            state: "updated".into(),
            detail: Some(detail),
        },
    };
    state.store.commit_event(&mut event).await.unwrap();
    let history = || async {
        let (status, body) = call(
            &state,
            "GET",
            "/api/v1/conversations/chat/history",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let item = body["thread"]["turns"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "reasoning")
            .cloned()
            .expect("thinking row");
        assert!(!body.to_string().contains("raw chain"));
        (body, item)
    };
    for (detail, shown) in [
        ("everything", false),
        ("conversation", false),
        ("thinking", true),
        ("everything", false),
    ] {
        let (status, body) = call(
            &state,
            "PUT",
            "/api/v1/settings/chat-detail/codex",
            serde_json::json!({ "detail": detail }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Streaming/replay must apply the same preference even when an upsert
        // carries display text without a summary (including older host events).
        for (summary, lifecycle, status) in [
            (false, "started", "inProgress"),
            (false, "completed", "completed"),
            (true, "completed", "completed"),
        ] {
            let mut live = event.clone();
            let mut payload = serde_json::json!({
                "turnId": "turn", "itemId": "rs_1", "itemType": "reasoning",
                "state": lifecycle, "lifecycleAuthority": 2, "text": "Reading the parser",
                "item": {"id": "rs_1", "type": "reasoning", "status": status}
            });
            if summary {
                payload["item"]["summary"] = serde_json::json!(["Reading the parser"]);
            }
            if let WonderEvent::Activity { detail, .. } = &mut live.event {
                *detail = Some(payload.to_string());
            }
            // A replay batch can mix known and detached conversations. Resolve
            // each independently: an unknown owner must fail closed even when
            // the known conversation in the same batch allows thinking.
            let mut detached = live.clone();
            detached.conversation_id = None;
            let mut unknown = live.clone();
            unknown.conversation_id = Some("missing-chat".into());
            let mut batch = [detached, live, unknown];
            hide_thinking_events(&state, &mut batch).await;
            for index in [0, 2] {
                let WonderEvent::Activity {
                    detail: Some(hidden),
                    ..
                } = &batch[index].event
                else {
                    panic!("activity");
                };
                let hidden: serde_json::Value = serde_json::from_str(hidden).unwrap();
                assert!(hidden.get("text").is_none(), "{detail}: unknown ownership");
                assert!(hidden["item"].get("summary").is_none());
                assert_eq!(hidden["state"], lifecycle);
                assert_eq!(hidden["item"]["status"], status);
                assert_eq!(hidden["lifecycleAuthority"], 2);
            }
            let live = &batch[1];
            let WonderEvent::Activity {
                detail: Some(filtered),
                ..
            } = &live.event
            else {
                panic!("activity");
            };
            let filtered: serde_json::Value = serde_json::from_str(filtered).unwrap();
            assert_eq!(
                filtered.get("text").is_some(),
                shown,
                "{detail}, summary={summary}, lifecycle={lifecycle}: {filtered}"
            );
            assert_eq!(filtered["item"].get("summary").is_some(), shown && summary);
            assert_eq!(filtered["state"], lifecycle);
            assert_eq!(filtered["item"]["status"], status);
            assert_eq!(filtered["lifecycleAuthority"], 2);
        }
        // Permission/question notifications and ordinary messages are never
        // hidden by the thinking-text gate.
        for body in [
            WonderEvent::ApprovalOpened {
                request_id: "question-or-approval".into(),
            },
            WonderEvent::AssistantDelta {
                text: "Visible reply".into(),
            },
            WonderEvent::Activity {
                category: "thread_item_upsert".into(),
                state: "updated".into(),
                detail: Some(
                    serde_json::json!({"text": "Visible question", "item": {
                    "id": "question", "type": "agentMessage", "text": "Visible question"}})
                    .to_string(),
                ),
            },
        ] {
            let mut visible = event.clone();
            visible.event = body;
            let before = serde_json::to_value(&visible).unwrap();
            hide_thinking_events(&state, std::slice::from_mut(&mut visible)).await;
            assert_eq!(serde_json::to_value(&visible).unwrap(), before, "{detail}");
        }
        let (body, item) = history().await;
        assert_eq!(item["state"], "completed", "{detail}");
        assert_eq!(
            body.to_string().contains("Reading the parser"),
            shown,
            "{detail}: {body}"
        );
        if shown {
            assert_eq!(item["text"], "Reading the parser");
        } else {
            assert!(item["text"].is_null());
        }
        crate::tests::validate_http_contract("reasoningItemPayload", &item["payload"]);
    }

    // A chat's own choice beats its provider's, both ways, and clearing it
    // follows the provider again. The API answers with the chats overriding.
    let set_provider = |detail: &'static str| {
        let state = &state;
        async move {
            call(
                state,
                "PUT",
                "/api/v1/settings/chat-detail/codex",
                serde_json::json!({ "detail": detail }),
            )
            .await
        }
    };
    let set_chat = |detail: serde_json::Value| {
        let state = &state;
        async move {
            call(
                state,
                "PUT",
                "/api/v1/conversations/chat/chat-detail",
                serde_json::json!({ "detail": detail }),
            )
            .await
        }
    };
    set_provider("thinking").await;
    let (status, body) = set_chat(serde_json::json!("conversation")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["chats"]["chat"], "conversation", "{body}");
    let (body, item) = history().await;
    assert!(!body.to_string().contains("Reading the parser"), "{body}");
    assert!(item["text"].is_null());
    // Older history pages gate on the same function: Conversation override
    // under a Thinking provider hides thinking.
    assert!(!shows_thinking(&state, "chat").await);
    let mut live = event.clone();
    live.event = WonderEvent::Activity {
        category: "thread_item_upsert".into(),
        state: "updated".into(),
        detail: Some(
            serde_json::json!({"turnId": "turn", "itemId": "rs_1", "itemType": "reasoning",
                "state": "completed", "lifecycleAuthority": 2, "text": "Reading the parser",
                "item": {"id": "rs_1", "type": "reasoning", "status": "completed",
                    "summary": ["Reading the parser"]}})
            .to_string(),
        ),
    };
    hide_thinking_events(&state, std::slice::from_mut(&mut live)).await;
    assert!(!format!("{:?}", live.event).contains("Reading the parser"));
    set_provider("everything").await;
    set_chat(serde_json::json!("thinking")).await;
    let (body, _) = history().await;
    assert!(body.to_string().contains("Reading the parser"), "{body}");
    // A Thinking override under a lower provider level shows it on older pages.
    assert!(shows_thinking(&state, "chat").await);
    let (status, body) = set_chat(serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["chats"].as_object().unwrap().is_empty(), "{body}");
    let (body, _) = history().await;
    assert!(!body.to_string().contains("Reading the parser"), "{body}");
    assert_eq!(
        set_chat(serde_json::json!("everything")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        state.store.chat_detail_override("chat").await.unwrap(),
        Some(wonder_store::ChatDetail::Everything)
    );
    assert_eq!(
        call(
            &state,
            "PUT",
            "/api/v1/conversations/missing/chat-detail",
            serde_json::json!({ "detail": "conversation" })
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        set_chat(serde_json::json!("nonsense")).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

// Contract: summary deltas never publish or store thinking text; the running
// row shows only that the agent is thinking. On completion the whole summary
// is redacted, then cut to size, and shown only at the provider's Thinking
// level, live and after a reload. Covers a long bearer token (Bearer + 4096 Q
// + SECRET_TAIL) and a key split across deltas. Claude's runtime sends its
// finished thinking block's text the same way (one part per paragraph), with
// no deltas (services/claude-runtime projection.mjs).
#[tokio::test]
async fn codex_thinking_summary_is_published_only_on_completion_and_redacted_whole() {
    thinking_summary_is_published_only_on_completion_and_redacted_whole(AgentFamily::Codex).await;
}

#[tokio::test]
async fn claude_thinking_text_is_published_only_on_completion_and_redacted_whole() {
    thinking_summary_is_published_only_on_completion_and_redacted_whole(AgentFamily::Claude).await;
}

async fn thinking_summary_is_published_only_on_completion_and_redacted_whole(family: AgentFamily) {
    // The fixture already holds the Codex conversation.
    let (thread, chat) = if family == AgentFamily::Claude {
        ("claude-thread", "519b0f7e-97bb-45d7-8206-324a62386dd9")
    } else {
        ("thread", "d19b0f7e-97bb-45d7-8206-324a62386dd9")
    };
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    crate::test_support::bind_project_thread(&state, chat, family, thread, None, "now")
        .await
        .unwrap();
    let wonder_store::MessageInsert::Inserted(message) = state
        .store
        .insert_dispatch_message("owner", "think", "think", "hash", chat, &[], "now", true)
        .await
        .unwrap()
    else {
        panic!("message");
    };
    let notify = |method: &str, params: serde_json::Value| {
        crate::process_app_server_notification(
            &state,
            serde_json::json!({"method": method, "params": params}),
            false,
        )
    };
    let long_token = format!("Bearer {}SECRET_TAIL", "Q".repeat(4096));
    let summary = [
        format!("Checking auth {long_token} done"),
        "Using ghp_SPLITKEY123 now".to_owned(),
    ];
    let deltas = [
        (0, "Checking auth Bearer ".to_owned()),
        (0, "Q".repeat(4096)),
        (0, "SECRET_TAIL done".to_owned()),
        (1, "Using g".to_owned()),
        (1, "hp_SPLIT".to_owned()),
        (1, "KEY123 now".to_owned()),
    ];
    let leaks = |text: &str| {
        ["SECRET_TAIL", "QQQQ", "SPLIT", "KEY123"]
            .iter()
            .find(|secret| text.contains(**secret))
            .map(|secret| secret.to_string())
    };
    let run = |turn: &'static str, item: &'static str| {
        let notify = &notify;
        let deltas = &deltas;
        async move {
            assert!(
                notify(
                    "item/started",
                    serde_json::json!({"threadId": thread, "turnId": turn,
                        "item": {"id": item, "type": "reasoning", "summary": [], "content": []}})
                )
                .await
            );
            let deltas = if family == AgentFamily::Codex {
                &deltas[..]
            } else {
                &[]
            };
            for (index, delta) in deltas {
                assert!(
                    notify(
                        "item/reasoning/summaryPartAdded",
                        serde_json::json!({"threadId": thread, "turnId": turn,
                            "itemId": item, "summaryIndex": index})
                    )
                    .await
                );
                assert!(
                    notify(
                        "item/reasoning/summaryTextDelta",
                        serde_json::json!({"threadId": thread, "turnId": turn,
                            "itemId": item, "summaryIndex": index, "delta": delta})
                    )
                    .await
                );
            }
        }
    };
    let complete = |turn: &'static str, item: &'static str| {
        let notify = &notify;
        let summary = &summary;
        async move {
            assert!(
                notify(
                    "item/completed",
                    serde_json::json!({"threadId": thread, "turnId": turn,
                        "item": {"id": item, "type": "reasoning", "summary": summary,
                                 "content": ["raw chain"]}})
                )
                .await
            );
        }
    };
    let rows = |events: &[HostEventEnvelope], item: &str| -> Vec<serde_json::Value> {
        events
            .iter()
            .filter_map(|event| match &event.event {
                WonderEvent::Activity {
                    category,
                    detail: Some(detail),
                    ..
                } if category == "thread_item_upsert" => {
                    serde_json::from_str::<serde_json::Value>(detail).ok()
                }
                _ => None,
            })
            .filter(|detail| detail["itemId"] == item)
            .collect()
    };
    let live = std::cell::RefCell::new(state.events.subscribe());
    let received = std::cell::RefCell::new(Vec::new());
    let events = || {
        while let Ok(event) = live.borrow_mut().try_recv() {
            received.borrow_mut().push(event);
        }
        received.borrow().clone()
    };
    let state = &state;
    let reload = |item: &'static str| async move {
        let (status, body) = crate::test_support::call(
            state,
            "GET",
            &format!("/api/v1/conversations/{chat}/history"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = body["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap().clone())
            .filter(|row| row["type"] == "reasoning" && row["payload"]["id"] == item)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "{body}");
        (body, rows[0].clone())
    };
    // Nothing published, served or stored anywhere holds a secret.
    let no_leak = || async {
        let published = format!("{:?}", events());
        assert_eq!(leaks(&published), None, "published");
        let stored = format!(
            "{:?}",
            state.store.events_for_conversation(chat).await.unwrap()
        );
        assert_eq!(leaks(&stored), None, "stored");
        assert!(!stored.contains("raw chain"));
    };

    for (turn, item, detail, shown) in [
        ("hidden", "rs_hidden", None, false),
        (
            "shown",
            "rs_shown",
            Some(wonder_store::ChatDetail::Thinking),
            true,
        ),
    ] {
        if let Some(detail) = detail {
            state
                .store
                .set_agent_chat_detail(family, detail, "now")
                .await
                .unwrap();
        }
        state
            .store
            .update_message_delivery(&message.id, "accepted_by_codex", Some(thread), Some(turn))
            .await
            .unwrap();
        run(turn, item).await;
        // While running: no summary or text on any live row, and a reload shows
        // only that Codex is thinking.
        let running = rows(&events(), item);
        assert!(!running.is_empty(), "{turn}");
        assert!(
            running
                .iter()
                .all(|row| row["item"].get("summary").is_none() && row.get("text").is_none()),
            "{turn}: {running:?}"
        );
        let (_, row) = reload(item).await;
        assert_eq!(row["state"], "started", "{row}");
        assert!(row["text"].is_null(), "{row}");
        assert!(row["payload"].get("summary").is_none(), "{row}");
        no_leak().await;

        complete(turn, item).await;
        // What the phone's event socket sends (`sync` applies this gate).
        let mut sent = events();
        hide_thinking_events(state, &mut sent).await;
        let done = rows(&sent, item).pop().unwrap();
        assert_eq!(done["state"], "completed");
        let (body, row) = reload(item).await;
        assert_eq!(row["state"], "completed", "{row}");
        assert_eq!(leaks(&body.to_string()), None, "served: {body}");
        no_leak().await;
        if shown {
            let expected = "Checking auth [redacted] done\n\nUsing [redacted] now";
            assert_eq!(done["text"], expected, "{done}");
            assert_eq!(
                done["item"]["summary"],
                serde_json::json!(["Checking auth [redacted] done", "Using [redacted] now"])
            );
            assert_eq!(row["text"], expected, "{row}");
        } else {
            assert!(done.get("text").is_none() && done["item"].get("summary").is_none());
            assert!(row["text"].is_null(), "{row}");
            assert!(!body.to_string().contains("Checking auth"));
        }
    }
}

// Bound: a part is redacted whole, then cut on a character boundary, so a
// credential crossing the cut never survives in part.
#[test]
fn thinking_summary_is_redacted_before_it_is_cut() {
    let token = format!("Bearer {}SECRET_TAIL", "Q".repeat(4096));
    for lead in [0, 1, 2, 3, 4000, 4090] {
        let part = format!("{}é {token} after", "a".repeat(lead));
        let safe = sanitize_typed_item(
            &serde_json::json!({"id": "rs", "type": "reasoning", "status": "completed",
                "summary": [part]}),
            None,
        );
        let text = safe["summary"][0].as_str().unwrap();
        assert!(text.len() <= THINKING_SUMMARY_PART_BYTES, "{lead}");
        assert!(
            !text.contains("SECRET_TAIL") && !text.contains("QQ"),
            "{lead}: {text}"
        );
        // Redacted first, the token shrinks and the text after it still fits.
        assert_eq!(text.ends_with("[redacted] after"), lead < 4090, "{lead}");
        assert_eq!(sanitize_typed_item(&safe, None), safe);
    }
    // A running row never carries a summary, live or in a snapshot, even when
    // the provider's running item already holds one.
    let running = serde_json::json!({"id": "rs", "type": "reasoning", "summary": ["partial"]});
    for state in ["started", "inProgress", "waiting"] {
        let detail: serde_json::Value = serde_json::from_str(
            &thread_item_upsert_detail_with_authority("turn", &running, state, 2, None).unwrap(),
        )
        .unwrap();
        assert!(detail["item"].get("summary").is_none() && detail.get("text").is_none());
        let row = typed_thread_item_projection_with_lifecycle(
            &AppServerThreadItem {
                turn_id: "turn".into(),
                item: running.clone(),
            },
            "now",
            None,
            Some(state),
            2,
        );
        assert!(
            row.text.is_none() && row.payload.get("summary").is_none(),
            "{state}"
        );
    }
    // Only completion shows it: a status-less item without other evidence, or
    // one that failed or was interrupted, carries none.
    for status in [
        None,
        Some("failed"),
        Some("interrupted"),
        Some("inProgress"),
    ] {
        let mut item = running.clone();
        if let Some(status) = status {
            item["status"] = status.into();
        }
        let safe = sanitize_typed_item(&item, None);
        assert!(safe.get("summary").is_none(), "{status:?}");
        assert_eq!(typed_item_display_text(&safe), None, "{status:?}");
    }
    // A cut inside a multibyte character backs off to its boundary.
    let wide = "é".repeat(THINKING_SUMMARY_PART_BYTES);
    let safe = sanitize_typed_item(
        &serde_json::json!({"type": "reasoning", "status": "completed", "summary": [wide]}),
        None,
    );
    assert_eq!(
        safe["summary"][0].as_str().unwrap().len(),
        THINKING_SUMMARY_PART_BYTES
    );
}

#[tokio::test]
async fn worktree_thread_keeps_previews_and_an_unowned_one_still_hydrates() {
    // A thread in a linked worktree of the Project's repository keeps its
    // screenshots like any Project thread. A folder that is not a Project
    // folder or its worktree (a plain subfolder) has no preview storage; its
    // multi-MB images once failed the whole refresh and left it blank.
    for linked in [true, false] {
        images_in_a_project_thread(linked).await;
    }
}

async fn images_in_a_project_thread(linked: bool) {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let chat = "4f1c2d3e-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    let root = crate::canonical_path::canonicalize(&state.data_dir).unwrap();
    let worktree = root.join(".local/wt-feature");
    if linked {
        for args in [
            &["init", "-q"][..],
            &["commit", "--allow-empty", "-qm", "fixture"],
            &["worktree", "add", "-qd", worktree.to_str().unwrap()],
        ] {
            let output = crate::test_git::git()
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=F", "-c", "user.email=f@example.invalid"])
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_DIR")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    } else {
        std::fs::create_dir_all(&worktree).unwrap();
    }
    let (root, worktree) = (root.to_str().unwrap(), worktree.to_str().unwrap());
    state
        .store
        .create_project(
            "worktree-project",
            "worktree-project",
            "fixture",
            "Project",
            &[wonder_store::ProjectRootInput {
                path: root.into(),
                canonical_path: root.into(),
            }],
            0,
            "now",
        )
        .await
        .unwrap();
    let store = crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex);
    state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: chat,
            project_id: "worktree-project",
            family: wonder_store::AgentFamily::Codex,
            provider_store: store,
            native_session_id: None,
            cwd: worktree,
            roots_revision: 1,
            title: "Worktree",
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
    crate::test_support::bind_project_thread(
        &state,
        chat,
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    // The Chats list discovers the Project's worktrees before a thread opens.
    crate::projects::visible_conversations(&state)
        .await
        .unwrap();
    assert_eq!(
        crate::projects::media_workspace(&state, chat)
            .await
            .is_some(),
        linked
    );
    let screenshot = std::path::Path::new(worktree).join("compare.png");
    image::DynamicImage::new_rgb8(64, 64)
        .save(&screenshot)
        .unwrap();
    let inline = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 3 * 1024 * 1024]);
    std::fs::write(
        dir.path().join("history-items.json"),
        serde_json::json!([
            {"turnId":"turn","item":{"id":"ask","type":"userMessage","content":[{"type":"text","text":"Compare the landing"}]}},
            {"turnId":"turn","item":{"id":"look","type":"imageView","path":screenshot}},
            {"turnId":"turn","item":{"id":"tool","type":"mcpToolCall","server":"browser","tool":"screenshot","status":"completed",
                "result":{"content":[{"type":"image","mimeType":"image/png","data":inline}]}}},
            {"turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"The landing matches."}}
        ])
        .to_string(),
    )
    .unwrap();
    let token = "worktree-refresh";
    assert!(state
        .store
        .claim_history_refresh(chat, token, now_ms() as i64)
        .await
        .unwrap());
    refresh_runtime_history(&state, chat, "thread", token, false)
        .await
        .unwrap();
    let response = conversation_snapshot(
        State(state.clone()),
        Path(chat.into()),
        Query(Default::default()),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 50_000_000).await.unwrap();
    assert!(
        body.len() < 200_000,
        "no inline image bytes: {}",
        body.len()
    );
    let snapshot: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items: Vec<&serde_json::Value> = snapshot["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .collect();
    let text = serde_json::to_string(&items).unwrap();
    for id in ["ask", "look", "tool", "answer"] {
        assert!(items.iter().any(|item| item["id"] == id), "{id}: {text}");
    }
    assert!(text.contains("The landing matches."));
    let look = items.iter().find(|item| item["id"] == "look").unwrap();
    assert_eq!(
        look["payload"]["result"]["content"][0]["type"] == "wonderArtifact",
        linked,
        "{look}"
    );
    assert_eq!(
        text.contains("Preview unavailable: Cupola can't keep previews for this thread."),
        !linked,
        "{text}"
    );
    assert!(!text.contains(screenshot.to_str().unwrap()), "no Mac path");
    state.projects.codex.lock().await.shutdown().await.unwrap();
}

// One durable-item contract owns the compact page, expansion route, permission
// boundary and reset visibility. No provider or model request is needed.
#[tokio::test]
async fn compact_history_tool_output_route_preserves_text_and_visibility() {
    use tower::ServiceExt;
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    crate::test_support::bind_project_thread(
        &state,
        chat,
        AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let raw = format!(
        "{}\u{1b}[31mred\u{1b}[0m sk-secret012345678901234567890\nFINAL OUTPUT 🌞",
        "line🌞\n".repeat(1000)
    );
    let safe = sanitize_typed_item(
        &serde_json::json!({"type":"commandExecution", "output":raw}),
        None,
    )["output"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(safe.ends_with("FINAL OUTPUT 🌞"));
    assert!(!safe.contains("sk-secret"));
    for (turn, id, kind, output, thread) in [
        (
            "turn",
            "tool",
            "commandExecution",
            serde_json::json!(raw),
            "thread",
        ),
        (
            "other-turn",
            "tool",
            "commandExecution",
            serde_json::json!("other turn"),
            "thread",
        ),
        (
            "old-turn",
            "old-tool",
            "functionCallOutput",
            serde_json::json!(raw),
            "old-thread",
        ),
        (
            "turn",
            "structured",
            "functionCallOutput",
            serde_json::json!({"value": "structured result"}),
            "thread",
        ),
        (
            "turn",
            "private",
            "reasoning",
            serde_json::json!("private raw text"),
            "thread",
        ),
    ] {
        let mut event = refresh_envelope(
            id,
            serde_json::json!({
                "turnId":turn, "itemId":id, "state":"completed",
                "item":{"id":id, "type":kind, "status":"completed", "output":output},
            }),
        );
        event.event_id = format!("{turn}/{id}");
        event.host_epoch = state.host_epoch.clone();
        event.conversation_id = Some(chat.into());
        event.thread_id = Some(thread.into());
        event.turn_id = Some(turn.into());
        event.item_id = Some(id.into());
        state.store.commit_event(&mut event).await.unwrap();
    }
    let db = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("INSERT INTO project_runtime_history(conversation_id,agent_family,provider_store,runtime_thread_id,switched_at) VALUES(?,'codex','app_server','old-thread','now')")
        .bind(chat).execute(&db).await.unwrap();
    let path = format!("/api/v1/conversations/{chat}/items/tool/output?turnId=turn");
    let (status, full) =
        crate::test_support::call(&state, "GET", &path, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{full}");
    assert_eq!(full["output"], safe);
    assert_eq!(full["outputBytes"], safe.len());
    crate::tests::validate_http_contract("toolOutput", &full);
    let response = router(state.clone())
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    // No trusted origin or loopback capability: the shared middleware rejects it.
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    for (suffix, expected) in [
        ("tool/output?turnId=other-turn", Some("other turn")),
        ("old-tool/output?turnId=old-turn", Some(safe.as_str())),
        ("tool/output?turnId=wrong", None),
        ("wrong/output?turnId=turn", None),
        ("structured/output?turnId=turn", None),
        ("private/output?turnId=turn", None),
    ] {
        let (status, value) = crate::test_support::call(
            &state,
            "GET",
            &format!("/api/v1/conversations/{chat}/items/{suffix}"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(
            status,
            if expected.is_some() {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
            "{suffix}: {value}"
        );
        if let Some(text) = expected {
            assert_eq!(value["output"], text);
        }
    }
    let (status, _) = crate::test_support::call(
        &state,
        "GET",
        &path.replace(chat, "unknown-chat"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = crate::test_support::call(
        &state,
        "GET",
        path.split('?').next().unwrap(),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/conversations/{chat}/history"))
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .header(history::COMPACT_VIEW_HEADER, "compact")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    let item = page["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "turn")
        .unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "tool")
        .unwrap();
    let preview = item["payload"]["output"].as_str().unwrap();
    assert!(preview.len() <= 2048 && safe.starts_with(preview));
    assert_eq!(item["payload"]["outputBytes"], safe.len());
    assert_eq!(item["payload"]["outputTruncated"], true);
    assert!(
        item["text"].is_null(),
        "bounded envelope text must not bypass the preview"
    );
    assert!(item["payload"].get("id").is_none());
    crate::tests::validate_http_contract("compactToolOutput", &item["payload"]);

    // The reader must respect the reset visibility bit even though the saved
    // payload still exists. Deleting its owning Project also revokes access.
    sqlx::query(
        "UPDATE history_entries SET hidden_by_reset=1 WHERE conversation_id=? AND source_id=?",
    )
    .bind(chat)
    .bind(format!("{chat}:thread:turn:tool"))
    .execute(&db)
    .await
    .unwrap();
    let (status, _) =
        crate::test_support::call(&state, "GET", &path, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    sqlx::query("DELETE FROM project_conversations WHERE conversation_id=?")
        .bind(chat)
        .execute(&db)
        .await
        .unwrap();
    let (status, _) = crate::test_support::call(
        &state,
        "GET",
        &path.replace("turnId=turn", "turnId=other-turn"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    state.projects.codex.lock().await.shutdown().await.unwrap();
}
