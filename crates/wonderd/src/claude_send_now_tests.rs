//! Send now (Guide) and plain sends to a Claude Project chat while a reply
//! runs, against the fake Claude bridge of `provider_switch_tests`.
//! Owner (Oct 12): a Guide waited in Claude's queue during a long command,
//! Stop did not reach it, and the next message was dispatched into the still
//! running reply and failed with "Claude is already working".
use crate::provider_switch_tests::{fx, Fx, CLAUDE_THREAD};
use axum::http::StatusCode;
use serde_json::Value;
use wonder_store::{AgentFamily, MessageInsert, StoredMessage};

/// A Claude Project chat whose first reply has finished.
async fn claude_chat() -> Fx {
    let fx = fx().await;
    let first = fx
        .queue(
            "start in Claude",
            Some((AgentFamily::Claude, "claude:sonnet")),
        )
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    fx
}

async fn stored(fx: &Fx, message: &StoredMessage) -> StoredMessage {
    fx.state
        .store
        .message_by_id(&message.id)
        .await
        .unwrap()
        .unwrap()
}

async fn queued_ids(fx: &Fx) -> Vec<String> {
    fx.state
        .store
        .pending_queue(&fx.chat)
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.id)
        .collect()
}

fn touch(fx: &Fx, name: &str) {
    std::fs::write(fx.dir.path().join(name), "").unwrap();
}

fn remove(fx: &Fx, name: &str) {
    std::fs::remove_file(fx.dir.path().join(name)).unwrap();
}

// Contract: while Cupola's Claude runtime still runs a reply, a plain message
// waits in the queue: the dispatcher holds it, its receipt says queued, and a
// send the runtime refuses for the running reply returns to its place instead
// of becoming an uncertain failure.
#[tokio::test]
async fn a_message_while_claudes_reply_still_runs_waits_in_the_queue() {
    let fx = claude_chat().await;
    touch(&fx, "claude-active");
    assert!(
        crate::projects::held(&fx.state, &fx.chat, None).await,
        "the runtime's running reply holds the chat"
    );
    let next = fx.queue("Continue", None).await;
    let receipt = crate::queue::receipt(&fx.state, stored(&fx, &next).await)
        .await
        .unwrap();
    assert_eq!(receipt.queued, Some(true));

    // The store believed the chat idle and dispatched anyway (the race the
    // owner hit): the refusal keeps the message queued, with no error.
    touch(&fx, "claude-running");
    fx.offer_models().await;
    crate::projects::dispatch(fx.state.clone(), next.clone()).await;
    let held = stored(&fx, &next).await;
    assert_eq!(held.state, "accepted_by_wonder");
    assert_eq!(
        fx.state
            .store
            .message_delivery_error(&next.id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(queued_ids(&fx).await, vec![next.id.clone()]);
    assert_eq!(
        fx.claude_requests("turn/start").len(),
        2,
        "one refused attempt, nothing written twice"
    );

    // When the reply ends the message is sent.
    remove(&fx, "claude-running");
    remove(&fx, "claude-active");
    assert!(!crate::projects::held(&fx.state, &fx.chat, None).await);
    crate::projects::dispatch(fx.state.clone(), next.clone()).await;
    assert_eq!(stored(&fx, &next).await.state, "accepted_by_codex");
    assert!(queued_ids(&fx).await.is_empty());
}

// Contract: a Guide Claude refuses because its reply already ended is sent as
// a queued message, not failed.
#[tokio::test]
async fn a_guide_whose_claude_reply_ended_is_queued_and_sent_next() {
    for typed in [false, true] {
        let fx = claude_chat().await;
        let running = fx.queue("Long task", None).await;
        let (_, turn, _) = fx.release(&running).await.unwrap();
        fx.state
            .store
            .update_message_delivery(&running.id, "streaming", Some(CLAUDE_THREAD), Some(&turn))
            .await
            .unwrap();
        let MessageInsert::Inserted(guide) = fx
            .state
            .store
            .insert_guide_message(
                "owner",
                &uuid::Uuid::new_v4().to_string(),
                "Do this instead",
                "hash",
                &fx.chat,
                &[],
                "2026-01-01T00:00:01Z",
                &turn,
            )
            .await
            .unwrap()
        else {
            panic!("new guide")
        };
        touch(
            &fx,
            if typed {
                "claude-steer-finished"
            } else {
                "claude-steer-untyped"
            },
        );
        let response = crate::dispatch_guide(fx.state.clone(), guide.clone(), turn.clone()).await;
        if !typed {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(stored(&fx, &guide).await.state, "uncertain");
            assert!(
                queued_ids(&fx).await.is_empty(),
                "unproven rejection cannot queue a second send"
            );
            continue;
        }
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let receipt: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["deliveryState"], "accepted_by_wonder");
        assert_eq!(receipt["queued"], true);
        assert_eq!(fx.claude_requests("turn/steer").len(), 1);
        let queued = stored(&fx, &guide).await;
        assert_eq!(
            (queued.state.as_str(), queued.codex_turn_id),
            ("accepted_by_wonder", None)
        );
        assert_eq!(queued_ids(&fx).await, vec![guide.id.clone()]);
        assert!(fx.state.store.pending_guides().await.unwrap().is_empty());

        // Once the reply's end is recorded, it is sent as its own turn.
        fx.finish(&running).await;
        crate::projects::dispatch(fx.state.clone(), stored(&fx, &guide).await).await;
        assert_eq!(stored(&fx, &guide).await.state, "accepted_by_codex");
        let starts = fx.claude_requests("turn/start");
        assert_eq!(
            starts.last().unwrap()["params"]["input"][0]["text"],
            "Do this instead"
        );
    }
}

// Contract: Send now that arrives after its reply ended is queued and sent
// next, and a replay of it gets the same queued receipt.
#[tokio::test]
async fn send_now_after_the_reply_ended_is_queued() {
    let fx = claude_chat().await;
    let ended = fx.queue("Short task", None).await;
    let (_, turn, _) = fx.release(&ended).await.unwrap();
    fx.state
        .store
        .update_message_delivery(&ended.id, "completed", Some(CLAUDE_THREAD), Some(&turn))
        .await
        .unwrap();
    let request = || crate::SteerTurnRequest {
        codex_skills: vec![],
        device_id: "owner".into(),
        client_message_id: "55555555-5555-4555-8555-555555555555".into(),
        body: "One more thing".into(),
        expected_turn_id: turn.clone(),
        attachment_ids: vec![],
    };
    let send = || {
        crate::steer_turn(
            axum::extract::State(fx.state.clone()),
            axum::extract::Path((fx.chat.clone(), turn.clone())),
            None,
            axum::Json(request()),
        )
    };
    let receipts = [send().await, send().await];
    for response in receipts {
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let receipt: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["deliveryState"], "accepted_by_wonder");
        assert_eq!(
            receipt["clientMessageId"],
            "55555555-5555-4555-8555-555555555555"
        );
    }
    let queue = fx.state.store.pending_queue(&fx.chat).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].body, "One more thing");
    assert!(fx.state.store.pending_guides().await.unwrap().is_empty());
    assert!(
        fx.claude_requests("turn/steer").is_empty(),
        "nothing was steered"
    );
}

// Contract (owner, Oct 18): a chat another app on the Mac holds (here Claude
// Code in Terminal) waits for it; Send now ends only that app's verified
// process for the chat, then sends the waiting message exactly once. A
// holder Wonder cannot verify is never signalled: the message stays queued
// and the phone says why. Background agents wait for Send now too.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn send_now_takes_the_chat_over_from_the_app_that_holds_it() {
    use crate::desktop_activity::tests::{
        foreign_process, AFTER_CHECK, DESKTOP_PIDS, HOST_APPS, SESSION_DIRS,
    };
    use axum::extract::{Path, State};
    let fx = claude_chat().await;
    // Its own session, so the private records folder below serves only this test.
    let session = uuid::Uuid::new_v4().to_string();
    let rpc = crate::claude::for_thread(&fx.state, CLAUDE_THREAD)
        .await
        .unwrap();
    crate::publish_app_server_notification(
        &fx.state,
        serde_json::json!({"id":"own-session","method":"thread/session/reset","params":{
            "threadId":CLAUDE_THREAD,"oldSessionId":crate::provider_switch_tests::CLAUDE_SESSION,
            "sessionId":session,"retainedTurnIds":[]},"_wonderRuntimeId":rpc.health().id()}),
    )
    .await;
    let stored_session = fx
        .state
        .store
        .project_conversation(&fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored_session.native_session_id.as_deref(),
        Some(session.as_str())
    );
    let records = tempfile::tempdir().unwrap();
    let transcripts = tempfile::tempdir().unwrap();
    SESSION_DIRS.lock().unwrap().insert(
        session.clone(),
        (records.path().to_owned(), transcripts.path().to_owned()),
    );
    let write = |pid: i32, session: &str, entrypoint: &str, status: &str| {
        std::fs::write(
            records.path().join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "procStart": crate::desktop_activity::record_start_label(pid), "sessionId": session, "status": status, "statusUpdatedAt": 1, "cwd": "/work/Held", "entrypoint": entrypoint})
                .to_string(),
        )
        .unwrap()
    };
    let holder_process = |name: &str, verified: bool, app: &str| {
        let pid = foreign_process(&records.path().join(name), None);
        if verified {
            DESKTOP_PIDS.lock().unwrap().insert(pid);
        }
        HOST_APPS.lock().unwrap().insert(pid, app.to_owned());
        pid
    };
    let send = |client: &'static str, take_over: bool| {
        let state = fx.state.clone();
        let chat = fx.chat.clone();
        async move {
            let response = crate::send_message_inner(
                State(state),
                Path(chat),
                None,
                axum::Json(crate::SendMessageRequest {
                    codex_skills: vec![],
                    take_over,
                    project_model: None,
                    device_id: "owner".into(),
                    client_message_id: client.into(),
                    body: "Continue here".into(),
                    attachment_ids: vec![],
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let receipt: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 100_000)
                    .await
                    .unwrap(),
            )
            .unwrap();
            receipt
        }
    };
    let dispatch = |id: String| {
        let state = fx.state.clone();
        async move {
            let message = state.store.message_by_id(&id).await.unwrap().unwrap();
            crate::projects::dispatch(state.clone(), message).await;
            // The dispatch loop releases a claim a held dispatch kept.
            state
                .store
                .recover_direct_dispatch_claims_except(&state.projects.dispatching())
                .await
                .unwrap();
            state.store.message_by_id(&id).await.unwrap().unwrap()
        }
    };
    let alive = crate::platform::process_alive;
    let starts = fx.claude_requests("turn/start").len();

    // Claude Code in Terminal has the chat open; another chat's process runs beside it.
    let terminal = holder_process("terminal", true, "Terminal");
    write(terminal, &session, "cli", "idle");
    let other = holder_process("other", true, "Terminal");
    write(other, "another-chat", "cli", "busy");
    let holder = crate::desktop_activity::claude_holder(&session)
        .await
        .unwrap();
    assert_eq!(
        (holder.app.as_str(), holder.waits, holder.can_take_over),
        ("Claude Code in Terminal", true, true)
    );
    let receipt = send("66666666-6666-4666-8666-666666666666", false).await;
    assert_eq!(receipt["queued"], true);
    let waiting = queued_ids(&fx).await;
    assert_eq!(waiting.len(), 1);
    let id = waiting[0].clone();
    let held = dispatch(id.clone()).await;
    assert_eq!(held.state, "accepted_by_wonder", "a plain send waits");
    assert!(alive(terminal));
    assert_eq!(fx.claude_requests("turn/start").len(), starts);

    // Send now on the same queued message.
    let receipt = send("66666666-6666-4666-8666-666666666666", true).await;
    assert_eq!(
        receipt["queued"], false,
        "Send now does not wait for the Mac"
    );
    let sent = dispatch(id.clone()).await;
    assert_eq!(sent.state, "accepted_by_codex");
    assert!(!alive(terminal), "the holder's process ended");
    assert_eq!(
        std::fs::read_to_string(records.path().join("terminal")).unwrap(),
        "INT"
    );
    assert!(alive(other), "another chat's process kept running");
    assert!(!records.path().join("other").exists());
    assert_eq!(fx.claude_requests("turn/start").len(), starts + 1);
    assert_eq!(dispatch(id).await.state, "accepted_by_codex");
    assert_eq!(
        fx.claude_requests("turn/start").len(),
        starts + 1,
        "sent exactly once"
    );
    fx.finish(&sent).await;

    // T3 Code keeps the chat open, even replying from before the message
    // (owner, Oct 18): a plain send waits like a terminal; only Send now ends it.
    let t3 = holder_process("t3", true, "T3 Code");
    write(t3, &session, "sdk-ts", "busy");
    let receipt = send("77777777-7777-4777-8777-777777777777", false).await;
    assert_eq!(receipt["queued"], true);
    let id = queued_ids(&fx).await[0].clone();
    assert_eq!(dispatch(id.clone()).await.state, "accepted_by_wonder");
    assert!(alive(t3) && !records.path().join("t3").exists());
    // Then it runs background agents there: they wait for Send now too.
    write(t3, &session, "sdk-ts", "shell");
    assert_eq!(dispatch(id.clone()).await.state, "accepted_by_wonder");
    assert!(alive(t3));
    send("77777777-7777-4777-8777-777777777777", true).await;
    let sent = dispatch(id).await;
    assert_eq!(sent.state, "accepted_by_codex");
    assert!(!alive(t3));
    fx.finish(&sent).await;

    // Claude app sync off (the default): the Claude app holds the chat like
    // a terminal. A plain send waits and stops nothing; only Send now ends it.
    let desktop = holder_process("desktop-held", true, "Claude");
    write(desktop, &session, "claude-desktop", "idle");
    let holder = crate::desktop_activity::claude_holder(&session)
        .await
        .unwrap();
    assert!(
        !holder.waits,
        "read with sync on, the app never holds the chat"
    );
    assert!(holder.with_app_sync(false).waits);
    let receipt = send("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", false).await;
    assert_eq!(receipt["queued"], true);
    let id = queued_ids(&fx).await[0].clone();
    assert_eq!(dispatch(id.clone()).await.state, "accepted_by_wonder");
    assert!(alive(desktop) && !records.path().join("desktop-held").exists());
    send("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", true).await;
    let sent = dispatch(id).await;
    assert_eq!(sent.state, "accepted_by_codex");
    assert!(!alive(desktop));
    fx.finish(&sent).await;

    // With Claude app sync on, a plain send stops the Claude app's copy (the
    // last sender wins). A Send now made while an earlier message is being
    // sent past it is left for the next dispatch.
    fx.state
        .store
        .set_provider_setting(
            AgentFamily::Claude,
            wonder_store::ProviderSetting::ClaudeAppSync,
            true,
            "now",
        )
        .await
        .unwrap();
    let desktop = holder_process("desktop", true, "Claude");
    write(desktop, &session, "claude-desktop", "idle");
    let receipt = send("99999999-9999-4999-8999-999999999999", false).await;
    assert_eq!(receipt["queued"], false);
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let (state, chat) = (fx.state.clone(), fx.chat.clone());
    AFTER_CHECK.lock().unwrap().insert(
        session.clone(),
        Box::new(move || crate::projects::request_take_over(&state, &chat, "the next message")),
    );
    let sent = dispatch(id).await;
    assert_eq!(sent.state, "accepted_by_codex");
    assert!(!alive(desktop));
    fx.finish(&sent).await;
    let later = holder_process("later", true, "T3 Code");
    write(later, &session, "sdk-ts", "idle");
    assert!(
        !crate::projects::held(&fx.state, &fx.chat, None).await,
        "the later Send now still applies"
    );

    // A process Wonder cannot prove is Claude Code is never signalled: the
    // message stays queued and says why.
    let starts = fx.claude_requests("turn/start").len();
    let unverified = holder_process("unverified", false, "T3 Code");
    write(unverified, &session, "sdk-ts", "idle");
    send("88888888-8888-4888-8888-888888888888", true).await;
    let id = queued_ids(&fx).await[0].clone();
    assert_eq!(dispatch(id.clone()).await.state, "accepted_by_wonder");
    assert!(alive(unverified) && !records.path().join("unverified").exists());
    assert_eq!(fx.claude_requests("turn/start").len(), starts);
    let notice = crate::projects::notice(&fx.state, &fx.chat).unwrap();
    assert!(
        notice.contains("T3 Code") && notice.ends_with("Your message is still queued."),
        "{notice}"
    );
    // The Send now was used up: the message waits as before, unsignalled.
    assert!(crate::projects::held(&fx.state, &fx.chat, None).await);
    for pid in [other, unverified, later] {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    SESSION_DIRS.lock().unwrap().remove(&session);
}

/// A Claude chat T3 Code has open (a verified `sdk-ts` process under
/// T3 Code), whose session is the fake T3 Code's thread `t3-thread`.
struct T3Scene {
    fx: Fx,
    /// The Codex scene's other chat, which nothing on the Mac holds.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // Codex scene is macOS-only.
    other_chat: Option<String>,
    session: String,
    records: tempfile::TempDir,
    _transcripts: tempfile::TempDir,
    holder: i32,
}

/// T3 Code's id for the thread's own model, as the catalog lists it, so a
/// message with the thread's own model finds T3 Code's thread already so.
async fn t3_model_names(fx: &Fx) {
    if let Some(sonnet) = fx
        .state
        .runtime_catalog
        .write()
        .await
        .models
        .iter_mut()
        .find(|option| option.id == "claude:sonnet")
    {
        sonnet.native_ids = vec!["claude-sonnet-5-5".into()];
    }
}

async fn t3_scene(fake: &std::sync::Arc<crate::t3_code::tests::Fake>) -> T3Scene {
    use crate::desktop_activity::tests::{foreign_process, DESKTOP_PIDS, HOST_APPS, SESSION_DIRS};
    let mut fx = claude_chat().await;
    let session = uuid::Uuid::new_v4().to_string();
    let rpc = crate::claude::for_thread(&fx.state, CLAUDE_THREAD)
        .await
        .unwrap();
    crate::publish_app_server_notification(
        &fx.state,
        serde_json::json!({"id":"own-session","method":"thread/session/reset","params":{
            "threadId":CLAUDE_THREAD,"oldSessionId":crate::provider_switch_tests::CLAUDE_SESSION,
            "sessionId":session,"retainedTurnIds":[]},"_wonderRuntimeId":rpc.health().id()}),
    )
    .await;
    let cwd = fx
        .state
        .store
        .project_conversation(&fx.chat)
        .await
        .unwrap()
        .unwrap()
        .cwd;
    let records = tempfile::tempdir().unwrap();
    let transcripts = tempfile::tempdir().unwrap();
    SESSION_DIRS.lock().unwrap().insert(
        session.clone(),
        (records.path().to_owned(), transcripts.path().to_owned()),
    );
    let transcript = crate::desktop_activity::claude_session_transcript(&session, &cwd).unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &transcript,
        [
            serde_json::json!({"type":"user","message":{"content":"Plan the release notes"}}),
            serde_json::json!({"type":"user","message":{"content":"Keep it short"}}),
        ]
        .map(|line| line.to_string())
        .join("\n"),
    )
    .unwrap();
    fake.project("p", &cwd);
    fake.thread(
        "t3-thread",
        "p",
        "claudeAgent",
        None,
        &["Plan the release notes", "Keep it short"],
    );
    let (t3, _) = crate::t3_code::tests::connected(fake).await;
    fx.state.t3_code = t3;
    t3_model_names(&fx).await;
    let holder = foreign_process(&records.path().join("t3"), None);
    DESKTOP_PIDS.lock().unwrap().insert(holder);
    HOST_APPS.lock().unwrap().insert(holder, "T3 Code".into());
    std::fs::write(
        records.path().join(format!("{holder}.json")),
        serde_json::json!({"pid": holder, "procStart": crate::desktop_activity::record_start_label(holder),
            "sessionId": session, "status": "busy", "statusUpdatedAt": 1, "cwd": cwd, "entrypoint": "sdk-ts"})
            .to_string(),
    )
    .unwrap();
    T3Scene {
        fx,
        other_chat: None,
        session,
        records,
        _transcripts: transcripts,
        holder,
    }
}

impl T3Scene {
    async fn send(&self, client: &str, take_over: bool) -> Value {
        self.send_with(client, take_over, vec![]).await
    }

    async fn send_with(&self, client: &str, take_over: bool, attachments: Vec<String>) -> Value {
        self.send_full(client, take_over, attachments, None).await
    }

    /// A send with another model than the thread's, which T3 Code's send
    /// can't carry.
    async fn send_model(&self, client: &str, take_over: bool, model: Value) -> Value {
        self.fx.offer_models().await;
        t3_model_names(&self.fx).await;
        let id = model["model"].as_str().unwrap();
        let family = serde_json::from_value(model["family"].clone()).unwrap();
        let mut catalog = self.fx.state.runtime_catalog.write().await;
        if !catalog.models.iter().any(|option| option.id == id) {
            catalog
                .models
                .push(crate::provider_switch_tests::model(id, family));
        }
        drop(catalog);
        let model = serde_json::from_value(model).unwrap();
        self.send_full(client, take_over, vec![], Some(model)).await
    }

    async fn send_full(
        &self,
        client: &str,
        take_over: bool,
        attachments: Vec<String>,
        project_model: Option<crate::provider_switch::MessageModel>,
    ) -> Value {
        use axum::extract::{Path, State};
        let response = crate::send_message_inner(
            State(self.fx.state.clone()),
            Path(self.fx.chat.clone()),
            None,
            axum::Json(crate::SendMessageRequest {
                codex_skills: vec![],
                take_over,
                project_model,
                device_id: "owner".into(),
                client_message_id: client.into(),
                body: "Continue here".into(),
                attachment_ids: attachments,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    /// A file uploaded to the chat, for a message to carry.
    async fn attach(&self) -> String {
        use axum::extract::{Path, State};
        use base64::Engine;
        let response = crate::upload_conversation_file(
            State(self.fx.state.clone()),
            Path(self.fx.chat.clone()),
            axum::Json(crate::CreateConversationFileRequest {
                client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                name: "notes.txt".into(),
                mime_type: Some("text/plain".into()),
                content_base64: base64::engine::general_purpose::STANDARD.encode("Release notes"),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let upload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap();
        upload["id"].as_str().unwrap().to_owned()
    }

    async fn dispatch(&self, id: &str) -> StoredMessage {
        let state = &self.fx.state;
        let message = state.store.message_by_id(id).await.unwrap().unwrap();
        crate::projects::dispatch(state.clone(), message).await;
        // The dispatch loop releases a claim a held dispatch kept.
        state
            .store
            .recover_direct_dispatch_claims_except(&state.projects.dispatching())
            .await
            .unwrap();
        state.store.message_by_id(id).await.unwrap().unwrap()
    }

    async fn held_by(&self) -> Value {
        let (status, activity) = crate::test_support::call(
            &self.fx.state,
            "GET",
            &format!("/api/v1/conversations/{}/history/activity", self.fx.chat),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{activity}");
        crate::tests::validate_http_contract("historyActivity", &activity);
        activity
    }

    /// The phone's view once T3 Code's lookup, which looking at the chat
    /// starts in the background, found the thread.
    async fn matched(&self) -> Value {
        for _ in 0..300 {
            let activity = self.held_by().await;
            if activity["heldBy"]["t3Code"] == "sends" {
                return activity;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("T3 Code's thread never found: {}", self.held_by().await);
    }

    /// Until no lookup of this chat's T3 thread runs.
    async fn lookup_done(&self) {
        // A hang guard only (about 120 s), no tighter than the scenario's own.
        for _ in 0..12_000 {
            if !self.fx.state.t3_code.lookup_state(&self.session).0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the lookup never finished");
    }

    fn stopped(&self) -> bool {
        !crate::platform::process_alive(self.holder) || self.records.path().join("t3").exists()
    }

    /// T3 Code's Claude session writes a reply (the scene starts so) or
    /// sits idle.
    fn replying(&self, replying: bool) {
        set_replying(
            &self.records.path().join(format!("{}.json", self.holder)),
            replying,
        );
    }
}

fn set_replying(record: &std::path::Path, replying: bool) {
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(record).unwrap()).unwrap();
    value["status"] = if replying { "busy" } else { "idle" }.into();
    value["statusUpdatedAt"] = (value["statusUpdatedAt"].as_i64().unwrap() + 1).into();
    std::fs::write(record, value.to_string()).unwrap();
}

impl Drop for T3Scene {
    fn drop(&mut self) {
        unsafe { libc::kill(self.holder, libc::SIGKILL) };
        crate::desktop_activity::tests::CODEX_PIDS
            .lock()
            .unwrap()
            .remove(&self.holder);
        crate::desktop_activity::tests::SESSION_DIRS
            .lock()
            .unwrap()
            .remove(&self.session);
    }
}

/// A Codex chat whose turn runs in the `codex app-server` T3 Code runs (a
/// stand-in that keeps the thread's rollout open); the thread is the fake
/// T3 Code's `t3-codex`.
#[cfg(target_os = "macos")]
async fn t3_codex_scene(fake: &std::sync::Arc<crate::t3_code::tests::Fake>) -> T3Scene {
    use crate::desktop_activity::tests::{codex_stand_in, CODEX_ARGS, HOST_APPS};
    let mut fx = fx().await;
    // A thread of its own: rollouts are remembered process-wide by thread.
    let thread = uuid::Uuid::new_v4().to_string();
    let project = fx.state.store.project("project").await.unwrap().unwrap();
    let cwd = project.roots[0].path.clone();
    let chat = uuid::Uuid::new_v4().to_string();
    let store = &fx.state.store;
    store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: &chat,
            project_id: "project",
            family: AgentFamily::Codex,
            provider_store: &fx.state.projects.codex_store,
            native_session_id: Some(&thread),
            cwd: &cwd,
            roots_revision: project.roots_revision,
            title: "In T3 Code",
            model: Some("fake"),
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
    store
        .bind_project_runtime(
            &chat,
            AgentFamily::Codex,
            &fx.state.projects.codex_store,
            &thread,
            None,
            "now",
        )
        .await
        .unwrap();
    let other_chat = std::mem::replace(&mut fx.chat, chat);
    // Cupola's runtime reads the thread idle; its latest turn runs on the Mac.
    let idle = fx.dir.path().join("idle-fixture.json");
    let mut threads: Value =
        serde_json::from_str(&std::fs::read_to_string(&idle).unwrap()).unwrap();
    threads[thread.as_str()] =
        serde_json::json!({"id": thread, "status": {"type": "idle"}, "cwd": cwd});
    std::fs::write(&idle, threads.to_string()).unwrap();
    fx.write(
        "turn-pages.json",
        &serde_json::json!([[{"id": "t3-turn", "status": "inProgress", "runningElsewhere": true}]]),
    );
    let records = tempfile::tempdir().unwrap();
    let rollout = records
        .path()
        .join(format!("rollout-2026-10-19T00-00-00-{thread}.jsonl"));
    let prompts = ["Plan the release notes", "Keep it short"];
    std::fs::write(
        &rollout,
        prompts
            .map(|text| {
                serde_json::json!({"type": "event_msg", "payload": {"type": "user_message", "message": text}})
                    .to_string()
            })
            .join("\n"),
    )
    .unwrap();
    crate::desktop_activity::remember_codex_rollout(&thread, rollout.to_str().unwrap());
    fake.project("p", &cwd);
    fake.thread("t3-codex", "p", "codex", None, &prompts);
    let holder = codex_stand_in(&[&rollout], &records.path().join("t3"));
    let arguments = ["codex", "app-server"].map(str::to_owned).to_vec();
    CODEX_ARGS.lock().unwrap().insert(holder, arguments);
    HOST_APPS.lock().unwrap().insert(holder, "T3 Code".into());
    T3Scene {
        fx,
        other_chat: Some(other_chat),
        session: thread,
        records,
        _transcripts: tempfile::tempdir().unwrap(),
        holder,
    }
}

// Contract (owner, Oct 18): with Cupola connected to T3 Code, a plain send to
// a chat T3 Code has open goes into its T3 thread through t3_thread_send,
// once, keyed by the message, and nothing on the Mac is stopped. When T3
// Code refuses Cupola's sign-in the message waits as before, the phone is
// told to connect again, and Send now still takes the chat over.
#[tokio::test]
async fn a_chat_t3_code_holds_goes_into_its_t3_thread_when_connected() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let starts = scene.fx.claude_requests("turn/start").len();
    let activity = scene.matched().await;
    assert_eq!(activity["heldBy"]["app"], "T3 Code");
    assert_eq!(activity["heldBy"]["t3Code"], "sends");
    assert_eq!(activity["heldBy"]["t3CodeReason"], Value::Null);
    // B-414: delivery and the running turn's route share one scan.
    let scans = crate::desktop_activity::tests::holder_scans(&scene.session);
    assert_eq!(scene.held_by().await["heldBy"]["replying"], true);
    assert_eq!(
        crate::desktop_activity::tests::holder_scans(&scene.session) - scans,
        1
    );
    assert_eq!(activity["heldBy"]["waits"], false);
    assert_eq!(activity["openElsewhere"], false);
    // The thread stays held; only sending goes around it.
    assert!(crate::projects::held(&scene.fx.state, &scene.fx.chat, None).await);
    assert!(!crate::projects::send_waits(&scene.fx.state, &scene.fx.chat, None).await);

    let receipt = scene
        .send("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", false)
        .await;
    assert_eq!(receipt["queued"], false, "it does not wait for T3 Code");
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["threadId"], "t3-thread");
    assert_eq!(sends[0]["message"], "Continue here");
    assert_eq!(sends[0]["mode"], "queue", "after any turn running there");
    assert_eq!(sends[0]["clientRequestId"], id.as_str());
    assert!(!scene.stopped(), "T3 Code keeps running the thread");
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts);
    // Dispatching it again sends nothing more.
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(fake.sends.lock().unwrap().len(), 1, "sent exactly once");
    assert_eq!(fake.delivered(), 1);

    // T3 Code no longer accepts Cupola: the message waits for T3 Code.
    fake.revoked
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let receipt = scene
        .send("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", false)
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_wonder");
    assert_eq!(fake.sends.lock().unwrap().len(), 1);
    assert!(!scene.stopped());
    let notice = crate::projects::notice(&scene.fx.state, &scene.fx.chat).unwrap();
    assert!(notice.contains("Connect T3 Code again"), "{notice}");
    assert!(crate::projects::held(&scene.fx.state, &scene.fx.chat, None).await);
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"]["t3Code"], "connect");
    // T3 Code refused the sign-in before it ended (the owner removed
    // Cupola there): the phone asks whether to connect again.
    assert_eq!(activity["heldBy"]["t3CodeReason"], "revoked");
    assert_eq!(activity["heldBy"]["waits"], true);
    // Only macOS verifies another app's Claude process, so only it offers
    // Send now; Linux never signals it.
    if !cfg!(target_os = "macos") {
        assert_eq!(activity["heldBy"]["canTakeOver"], false);
        return;
    }
    assert_eq!(activity["heldBy"]["canTakeOver"], true);
    // Send now takes it over as without T3 Code's connection.
    scene
        .send("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", true)
        .await;
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_codex");
    assert!(scene.stopped());
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts + 1);
}

// Contract (CONTEXT-PERSIST): a chat T3 Code runs reports its context when the
// phone opens it, from the provider's own file and Cupola's saved reading,
// with no reply from Cupola. T3's Claude transcript carries each request's
// tokens but never the window, so the host fills in the model's typical one; with no
// reading at all the phone is told why in words.
#[tokio::test]
async fn a_chat_t3_code_runs_reports_its_context_when_the_phone_opens_it() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let activity = scene.matched().await;
    assert_eq!(activity["contextUsage"], Value::Null, "{activity}");
    let note = activity["contextUsageNote"].as_str().unwrap();
    assert!(note.starts_with("Claude has not reported"), "{note}");
    let cwd = scene
        .fx
        .state
        .store
        .project_conversation(&scene.fx.chat)
        .await
        .unwrap()
        .unwrap()
        .cwd;
    let transcript =
        crate::desktop_activity::claude_session_transcript(&scene.session, &cwd).unwrap();
    let reply = serde_json::json!({"timestamp": "2026-11-04T10:00:00Z", "type": "assistant",
        "message": {"model": "claude-opus-5-5", "usage": {"input_tokens": 3,
        "cache_read_input_tokens": 61_000, "cache_creation_input_tokens": 500, "output_tokens": 97}}});
    let mut text = std::fs::read_to_string(&transcript).unwrap();
    text.push_str(&format!("\n{reply}\n"));
    std::fs::write(&transcript, text).unwrap();
    let opened = scene.held_by().await;
    assert_eq!(opened["contextUsage"]["usedTokens"], 61_600, "{opened}");
    // The reply names its model, so the window is that model's typical one, labeled so.
    assert_eq!(opened["contextUsage"]["windowTokens"], 1_000_000);
    assert_eq!(opened["contextUsage"]["windowIsTypical"], true);
    assert_eq!(opened["contextUsageNote"], Value::Null);
}

// The same for T3 Code's Codex child: the rollout's token_count carries the
// request's tokens and the window, so the opening phone gets both.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_codex_chat_t3_code_runs_reports_its_context_when_the_phone_opens_it() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_codex_scene(&fake).await;
    let rollout = scene.records.path().join(format!(
        "rollout-2026-10-19T00-00-00-{}.jsonl",
        scene.session
    ));
    let idle = scene.fx.dir.path().join("idle-fixture.json");
    let mut threads: Value =
        serde_json::from_str(&std::fs::read_to_string(&idle).unwrap()).unwrap();
    threads[scene.session.as_str()]["path"] = rollout.to_str().unwrap().into();
    std::fs::write(&idle, threads.to_string()).unwrap();
    let before = scene.held_by().await;
    assert_eq!(before["contextUsage"], Value::Null, "{before}");
    assert!(before["contextUsageNote"]
        .as_str()
        .unwrap()
        .starts_with("Codex has not reported"));
    let count = serde_json::json!({"timestamp": "2026-11-04T10:00:00Z", "type": "event_msg",
        "payload": {"type": "token_count", "info": {"last_token_usage": {"total_tokens": 31_000},
        "total_token_usage": {"total_tokens": 940_000}, "model_context_window": 258_400}}});
    let mut text = std::fs::read_to_string(&rollout).unwrap();
    text.push_str(&format!("\n{count}\n"));
    std::fs::write(&rollout, text).unwrap();
    let opened = scene.held_by().await;
    assert_eq!(
        opened["contextUsage"]["usedTokens"], 31_000,
        "the last request, not the running total: {opened}"
    );
    assert_eq!(opened["contextUsage"]["windowTokens"], 258_400);
    assert_eq!(opened["contextUsageNote"], Value::Null);
}

// A question watcher can learn that the owner revoked Cupola before any
// subsequent send. The queued message must give the same reconnect advice
// whether the watcher or the send is the first request T3 Code refuses.
#[tokio::test]
async fn a_revoked_t3_connection_reports_reconnect_before_the_next_send() {
    for (learned_before_send, queue_entry) in [(true, false), (false, false), (true, true)] {
        let fake = crate::t3_code::tests::Fake::start().await;
        let scene = t3_scene(&fake).await;
        scene.matched().await;
        fake.revoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if learned_before_send {
            assert!(
                crate::t3_code::questions::poll(&scene.fx.state, &scene.fx.chat, "t3-thread")
                    .await
                    .is_err()
            );
            assert!(!scene.fx.state.t3_code.connected());
            let delivery =
                crate::t3_code::delivery(&scene.fx.state, &scene.stored().await, None).await;
            assert_eq!(delivery.reason(), Some("revoked"));
        }
        let receipt = scene
            .send("fafafafa-fafa-4afa-8afa-fafafafafafa", false)
            .await;
        let id = receipt["wonderMessageId"].as_str().unwrap();
        let current = if queue_entry {
            let message = scene
                .fx
                .state
                .store
                .message_by_id(id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                crate::projects::send_waits(&scene.fx.state, &scene.fx.chat, Some(&message)).await
            );
            message
        } else {
            scene.dispatch(id).await
        };
        assert_eq!(current.state, "accepted_by_wonder");
        assert!(fake.sends.lock().unwrap().is_empty());
        assert!(!scene.stopped());
        let notice = crate::projects::notice(&scene.fx.state, &scene.fx.chat);
        assert!(
            notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Connect T3 Code again")),
            "learned before send: {learned_before_send}; queue entry: {queue_entry}; notice: {notice:?}"
        );
    }
    // A revoked global connection must not change advice for a different
    // application's holder. Mark the fresh fixture before its first scan.
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    crate::desktop_activity::tests::HOST_APPS
        .lock()
        .unwrap()
        .insert(scene.holder, "Terminal".into());
    fake.revoked
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        crate::t3_code::questions::poll(&scene.fx.state, &scene.fx.chat, "t3-thread")
            .await
            .is_err()
    );
    assert!(!scene.fx.state.t3_code.connected());
    assert!(crate::t3_code::holder(&scene.stored().await)
        .await
        .is_none());
    let receipt = scene
        .send("ecececec-ecec-4cec-8cec-ecececececec", false)
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap();
    assert_eq!(scene.dispatch(id).await.state, "accepted_by_wonder");
    let notice = crate::projects::notice(&scene.fx.state, &scene.fx.chat);
    assert!(
        !notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Connect T3 Code again")),
        "{notice:?}"
    );
    assert!(fake.sends.lock().unwrap().is_empty());
    assert!(!scene.stopped());
}

// Contract: references are resolved before the T3 transport, and recovery
// compares the exact provider text without replacing the user's stored draft.
#[tokio::test]
async fn composer_references_reach_t3_owned_providers_once() {
    // `t3_codex_scene` is macOS-only, so Linux runs the Claude half alone.
    #[cfg(target_os = "macos")]
    let families = [AgentFamily::Claude, AgentFamily::Codex];
    #[cfg(not(target_os = "macos"))]
    let families = [AgentFamily::Claude];
    for family in families {
        let fake = crate::t3_code::tests::Fake::start().await;
        let scene = match family {
            AgentFamily::Claude => t3_scene(&fake).await,
            #[cfg(target_os = "macos")]
            _ => {
                let mut scene = t3_codex_scene(&fake).await;
                scene.fx.state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
                scene
            }
            #[cfg(not(target_os = "macos"))]
            _ => unreachable!("only the Claude family runs off macOS"),
        };
        scene.matched().await;
        let state = &scene.fx.state;
        let source = state
            .store
            .project_conversation(&scene.fx.chat)
            .await
            .unwrap()
            .unwrap();
        let target = uuid::Uuid::new_v4().to_string();
        state
            .store
            .create_project_conversation(wonder_store::ProjectConversationInsert {
                conversation_id: &target,
                project_id: &source.project_id,
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: Some("reference-thread"),
                cwd: &source.cwd,
                roots_revision: source.roots_revision,
                title: "Release reference",
                model: Some("fake"),
                effort: None,
                service_tier: None,
                access_mode: "read_only",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
        crate::test_support::bind_project_thread(
            state,
            &target,
            AgentFamily::Codex,
            "reference-thread",
            None,
            "now",
        )
        .await
        .unwrap();
        scene.fx.write("codex-items.json", &serde_json::json!({"reference-thread": [
            {"turnId":"reference-turn", "item":{"id":"answer", "type":"agentMessage", "text":"Synthetic marker: COPPER-42."}}
        ]}));
        let file = if family == AgentFamily::Claude {
            "@\"docs/Release notes.md\""
        } else {
            "\"docs/Release notes.md\""
        };
        let body = format!(
            "/cupola-summary Compare {file} with [@Release reference](cupola://thread/{target})"
        );
        let message = scene.fx.queue(&body, None).await;
        assert_eq!(scene.dispatch(&message.id).await.state, "completed");
        let sent = fake.sends.lock().unwrap()[0].clone();
        let text = sent["message"].as_str().unwrap();
        assert!(
            text.starts_with("Summarize this conversation's decisions"),
            "{sent}"
        );
        assert!(!text.contains("/cupola-summary"), "{sent}");
        assert!(
            text.contains(file) && text.contains(&target) && text.contains("COPPER-42"),
            "{sent}"
        );
        assert_eq!(stored(&scene.fx, &message).await.body, body);
        let intent = state
            .store
            .dispatch_context(&message.id)
            .await
            .unwrap()
            .unwrap();
        let intent: Value = serde_json::from_str(&intent).unwrap();
        assert_eq!(intent["providerText"], text);
        eprintln!("COMPOSER-PAYLOAD t3-{} {}", family.as_str(), sent);
        scene.dispatch(&message.id).await;
        assert_eq!(fake.sends.lock().unwrap().len(), 1);
        let command = scene.fx.queue("/compact", None).await;
        assert_eq!(scene.dispatch(&command.id).await.state, "completed");
        let sent_command = fake.sends.lock().unwrap()[1].clone();
        assert_eq!(sent_command["message"], "/compact");
        eprintln!("COMPOSER-PAYLOAD t3-{} {}", family.as_str(), sent_command);
        let (status, commands) = crate::test_support::call(
            state,
            "GET",
            &format!("/api/v1/conversations/{}/commands", scene.fx.chat),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            commands["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "compact"),
            "{commands}"
        );
    }
}

// Contract: the agent's tool reaches T3 Code's MCP tools in-process, so it
// works from a Cupola-run turn (T3 Code holds no session then) and acts on
// the thread `search` named; it is not tied to the chat's own T3 session.
#[tokio::test]
async fn the_agent_tool_renames_and_pins_a_named_t3_thread_from_a_cupola_turn() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let receipt = scene
        .send("dddddddd-dddd-4ddd-8ddd-dddddddddddd", false)
        .await;
    let message = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    fake.thread("owned-test-thread", "p", "claudeAgent", None, &["hi"]);
    fake.project("other", "/somewhere/else");
    fake.thread("foreign-thread", "other", "claudeAgent", None, &["hi"]);
    // Another Project's thread is out of reach: refused, nothing changed.
    let refused = crate::thread_tools::call_for_test(
        &scene.fx.state,
        "test-runtime",
        &message,
        crate::thread_tools::T3_THREAD,
        serde_json::json!({"action":"rename","threadId":"foreign-thread","title":"Hijacked"}),
    )
    .await;
    assert!(refused.to_string().contains("another project"), "{refused}");
    assert!(fake.renamed.lock().unwrap().is_empty());
    for args in [
        serde_json::json!({"action":"rename","threadId":"owned-test-thread","title":"Renamed by the agent"}),
        serde_json::json!({"action":"organize","threadId":"owned-test-thread","organize":"pin"}),
    ] {
        let reply = crate::thread_tools::call_for_test(
            &scene.fx.state,
            "test-runtime",
            &message,
            crate::thread_tools::T3_THREAD,
            args,
        )
        .await;
        assert!(reply.to_string().contains("owned-test-thread"), "{reply}");
        assert!(!reply.to_string().contains("t3_code"), "{reply}");
    }
    let renamed = fake.renamed.lock().unwrap().clone();
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0]["threadId"], "owned-test-thread");
    assert_eq!(renamed[0]["title"], "Renamed by the agent");
    let organized = fake.organized.lock().unwrap().clone();
    assert_eq!(organized.len(), 1);
    assert_eq!(organized[0]["threadId"], "owned-test-thread");
    assert_eq!(organized[0]["action"], "pin");
}

// Contract (owner, Oct 22): a chat T3 Code has open goes into the T3 thread
// T3 Code's own store names by the session's exact ID, even when its words
// don't match the transcript (T3 Code keeps the first message as a context
// link; a compacted session opens with its summary). Only a session the
// store doesn't have is matched by its words.
#[tokio::test]
async fn a_chat_t3_code_holds_is_its_t3_thread_by_session_id_before_words() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    // The store has another session only: the words still find the thread.
    fake.keeps("t3-unrelated", "claudeAgent", "another-session")
        .await;
    scene.matched().await;
    let state = &scene.fx.state;
    let stored = state
        .store
        .project_conversation(&scene.fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::t3_code::route(state, &stored).await.as_deref(),
        Some("t3-thread"),
        "matched by its words"
    );

    // T3 Code's store has this session: its thread, whose words differ.
    fake.thread(
        "t3-linked",
        "p",
        "claudeAgent",
        None,
        &[
            "[Release notes](t3-context:thread/abc)",
            "This session is being continued from a previous conversation",
        ],
    );
    fake.keeps("t3-linked", "claudeAgent", &scene.session).await;
    assert_eq!(
        crate::t3_code::route(state, &stored).await.as_deref(),
        Some("t3-linked")
    );
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"]["t3Code"], "sends");
    assert_eq!(activity["heldBy"]["t3CodeReason"], Value::Null);
    let receipt = scene
        .send("cccccccc-cccc-4ccc-8ccc-cccccccccccc", false)
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["threadId"], "t3-linked");
}

// Contract (owner, Oct 22): while a turn runs in the T3 thread, whether
// the phone is looking at the chat or just sent into it, Cupola reads T3
// Code's questions on its own: one the agent asks reaches the phone in this
// chat (and its notifications) without another read, and the reading stops
// once T3 Code's turn ends, closing what it asked.
#[tokio::test]
async fn a_question_in_t3_codes_turn_reaches_the_phone_while_the_turn_runs() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    state
        .t3_code
        .questions
        .poll_every(std::time::Duration::from_millis(20));
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // The phone looks at the chat while T3 Code replies.
    scene.matched().await;
    let watching = || state.t3_code.questions.watching("t3-thread");
    crate::t3_code::tests::until("watching", watching).await;
    fake.questions.lock().unwrap().push(serde_json::json!({
        "threadId": "t3-thread", "requestId": "ask", "questions": [
        {"id": "a", "header": "Scope", "question": "Include the changelog?", "options": [
            {"label": "Yes", "description": ""}, {"label": "No", "description": ""}]}]}));
    let key = crate::t3_code::questions::request_key("t3-thread", "ask");
    for _ in 0..1_500 {
        if state.store.approval(&key).await.unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let asked = state.store.approval(&key).await.unwrap().expect("shown");
    assert_eq!(asked.state, "pending");
    assert_eq!(
        state
            .store
            .approval_push_conversation(&asked.thread_id, &asked.turn_id)
            .await
            .unwrap()
            .as_deref(),
        Some(scene.fx.chat.as_str())
    );
    // T3 Code's turn ends: nothing more is read, and the question closes.
    fake.queue_sends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    crate::t3_code::tests::until("stopped", || !watching()).await;
    let closed = state.store.approval(&key).await.unwrap().unwrap();
    assert_eq!(closed.decision.as_deref(), Some("turn_ended"));

    // A message sent into the T3 thread starts reading again.
    scene.replying(false);
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let receipt = scene
        .send("cccccccc-cccc-4ccc-8ccc-cccccccccccc", false)
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    scene.dispatch(&id).await;
    assert_eq!(fake.sends.lock().unwrap().len(), 1);
    crate::t3_code::tests::until("watching after the send", watching).await;
}

// Contract (lead, Oct 19): a thread T3 Code holds is held for everything
// but sending, even while Cupola sends through T3 Code: rewinding it is
// refused as for any thread running on the Mac.
#[tokio::test]
async fn a_thread_t3_code_holds_cant_be_rewound() {
    use axum::extract::{Path, State};
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    scene.matched().await;
    let response = crate::rewind::rewind(
        State(state.clone()),
        axum::Extension(crate::OwnerAuthority),
        Path(scene.fx.chat.clone()),
        axum::Json(
            serde_json::from_value(serde_json::json!({"turnId": "turn", "restoreFiles": false}))
                .unwrap(),
        ),
    )
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        String::from_utf8_lossy(&body).contains("still working"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(fake.sends.lock().unwrap().is_empty());
    assert!(!scene.stopped());
}

// Contract (lead, Oct 19): a send through T3 Code is durable before T3 Code
// sees it and is never sent again blindly. When T3 Code's answer is lost
// (an error after it applied the send, or Cupola restarting mid-call), the
// message is uncertain until Cupola reads the T3 thread: T3 Code has it
// (in the timeline by the message's ID, or in its queue) and it is
// completed without a second call, or it never arrived and it goes again
// with the same ID. Each message is in the T3 thread exactly once.
#[tokio::test]
async fn a_send_t3_code_may_have_got_is_read_back_not_sent_again() {
    use crate::t3_code::tests::Lose;
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    scene.matched().await;
    let calls = |id: &str| {
        fake.sends
            .lock()
            .unwrap()
            .iter()
            .filter(|send| send["clientRequestId"] == id)
            .count()
    };
    let notice = || crate::projects::notice(state, &scene.fx.chat);

    // T3 Code applied it and then failed the request.
    *fake.lose.lock().unwrap() = Some(Lose::ErrorAfterApplying);
    let receipt = scene
        .send("a1a1a1a1-a1a1-4a1a-8a1a-a1a1a1a1a1a1", false)
        .await;
    let lost = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&lost).await.state, "uncertain");
    assert!(notice().unwrap().contains("checking whether T3 Code"));
    crate::projects::recover(state).await;
    assert_eq!(scene.dispatch(&lost).await.state, "completed");
    assert_eq!(calls(&lost), 1, "read back, not sent again");
    assert_eq!(notice(), None);

    // Cupola stops mid-call: once T3 Code applied it, once before.
    for (lose, client) in [
        (Lose::AfterApplying, "a2a2a2a2-a2a2-4a2a-8a2a-a2a2a2a2a2a2"),
        (Lose::BeforeApplying, "a3a3a3a3-a3a3-4a3a-8a3a-a3a3a3a3a3a3"),
    ] {
        *fake.lose.lock().unwrap() = Some(lose);
        let receipt = scene.send(client, false).await;
        let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
        let message = state.store.message_by_id(&id).await.unwrap().unwrap();
        let (received, delivered) = (
            fake.received.load(std::sync::atomic::Ordering::SeqCst),
            fake.delivered(),
        );
        let dispatch = tokio::spawn(crate::projects::dispatch(state.clone(), message));
        // T3 Code has the call (and, the first time, applied it).
        while fake.received.load(std::sync::atomic::Ordering::SeqCst) == received
            || (lose == Lose::AfterApplying && fake.delivered() == delivered)
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        dispatch.abort();
        let _ = dispatch.await;
        state.store.recover_dispatch_claims().await.unwrap();
        let restarted = state.store.message_by_id(&id).await.unwrap().unwrap();
        assert_eq!(restarted.state, "uncertain", "{lose:?}");
        crate::projects::recover(state).await;
        if lose == Lose::AfterApplying {
            assert_eq!(scene.dispatch(&id).await.state, "completed");
            assert_eq!(calls(&id), 1, "read back, not sent again");
        } else {
            // Not in the T3 thread: it waits its turn and goes again.
            assert_eq!(
                state.store.message_by_id(&id).await.unwrap().unwrap().state,
                "accepted_by_wonder"
            );
            assert_eq!(scene.dispatch(&id).await.state, "completed");
            assert_eq!(
                calls(&id),
                2,
                "the call T3 Code never applied, then this one"
            );
        }
    }

    // T3 Code queued it behind a running turn, then failed the request.
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *fake.lose.lock().unwrap() = Some(Lose::ErrorAfterApplying);
    let receipt = scene
        .send("a4a4a4a4-a4a4-4a4a-8a4a-a4a4a4a4a4a4", false)
        .await;
    let queued = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&queued).await.state, "uncertain");
    crate::projects::recover(state).await;
    assert_eq!(scene.dispatch(&queued).await.state, "completed");
    assert_eq!(calls(&queued), 1);

    assert_eq!(fake.delivered(), 4, "each message applied once");
    let texts = fake.texts("t3-thread");
    assert_eq!(
        texts.iter().filter(|text| *text == "Continue here").count(),
        4,
        "{texts:?}"
    );
    assert!(!scene.stopped());
}

impl T3Scene {
    /// T3 Code released the session (closed there, or idle long enough):
    /// nothing on the Mac has the chat open any more.
    async fn released(&self) {
        std::fs::remove_file(self.records.path().join(format!("{}.json", self.holder))).unwrap();
        let stored = self.stored().await;
        // A hang guard only (about 12 s).
        for _ in 0..1_200 {
            if crate::t3_code::holder(&stored).await.is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("T3 Code still holds the chat");
    }

    async fn stored(&self) -> wonder_store::StoredProjectConversation {
        self.fx
            .state
            .store
            .project_conversation(&self.fx.chat)
            .await
            .unwrap()
            .unwrap()
    }
}

// Contract (owner, Oct 21; lead, Oct 21): a thread T3 Code owns goes
// through T3 Code whenever T3 Code runs, even when T3 Code no longer has it
// open (T3 Code reopens the session, so the reply has its tools and shows
// there). That holds for a thread started in T3 Code that it let go of
// before Cupola ever saw it open there: T3 Code's own store has the
// session's exact ID, so the first message goes at once, without asking T3
// Code which thread it is, and Cupola starts no turn. When T3 Code isn't
// running, Cupola runs the reply itself, sends nothing to T3 Code twice,
// and the phone says why.
#[tokio::test]
async fn a_thread_t3_code_owns_goes_through_t3_code_whenever_it_runs() {
    use std::sync::atomic::Ordering::SeqCst;
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    fake.keeps("t3-thread", "claudeAgent", &scene.session).await;
    scene.released().await;
    let state = &scene.fx.state;
    let starts = scene.fx.claude_requests("turn/start").len();
    assert!(!crate::projects::send_waits(state, &scene.fx.chat, None).await);
    let receipt = scene
        .send("dddddddd-dddd-4ddd-8ddd-dddddddddddd", false)
        .await;
    assert_eq!(receipt["queued"], false);
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["threadId"], "t3-thread");
    assert_eq!(sends[0]["message"], "Continue here");
    assert_eq!(sends[0]["clientRequestId"], id.as_str());
    assert_eq!(
        scene.fx.claude_requests("turn/start").len(),
        starts,
        "Cupola ran nothing"
    );
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(fake.delivered(), 1, "sent exactly once");
    assert_eq!(fake.lookups.load(SeqCst), 0, "T3 Code wasn't asked which");
    let scans = crate::desktop_activity::tests::holder_scans(&scene.session);
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"], Value::Null);
    assert_eq!(activity["runsWithoutT3Code"], false);
    // B-414: the note for a thread nothing holds reuses that scan.
    assert_eq!(
        crate::desktop_activity::tests::holder_scans(&scene.session) - scans,
        1
    );

    // T3 Code quits: the send never reaches it, so Cupola runs this reply.
    fake.stop();
    let receipt = scene
        .send("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee", false)
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    scene.dispatch(&id).await;
    // No "queued, tries again" note: it doesn't wait for T3 Code.
    assert_eq!(crate::projects::notice(state, &scene.fx.chat), None);
    assert!(!crate::projects::send_waits(state, &scene.fx.chat, None).await);
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_codex");
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts + 1);
    assert_eq!(
        fake.sends.lock().unwrap().len(),
        1,
        "nothing more went to T3 Code"
    );
    let activity = scene.held_by().await;
    assert_eq!(activity["runsWithoutT3Code"], true, "{activity}");
    assert_eq!(activity["heldBy"], Value::Null);
}

// Contract (lead, Oct 21): which T3 thread a Cupola thread is outlives a
// restart. With nothing kept in memory and T3 Code closed, the phone still
// says replies run without T3 Code, and Cupola runs the next one itself.
#[tokio::test]
async fn a_thread_t3_code_owns_stays_its_after_a_restart_while_t3_code_is_closed() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    scene.released().await;
    let state = &scene.fx.state;
    fake.stop();
    state.t3_code.restarted_for_test();
    let starts = scene.fx.claude_requests("turn/start").len();
    let activity = scene.held_by().await;
    assert_eq!(activity["runsWithoutT3Code"], true, "{activity}");
    assert_eq!(activity["heldBy"], Value::Null);
    let receipt = scene
        .send("12121212-1212-4212-8212-121212121212", false)
        .await;
    assert_eq!(receipt["queued"], false);
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_codex");
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts + 1);
    assert!(fake.sends.lock().unwrap().is_empty());
    // Archived in T3 Code, as its store says while it is closed: the thread
    // isn't T3 Code's any more, whatever Cupola's store remembers.
    fake.keeps("t3-thread", "claudeAgent", &scene.session).await;
    fake.archive("t3-thread").await;
    let activity = scene.held_by().await;
    assert_eq!(activity["runsWithoutT3Code"], false, "{activity}");
}

// Contract (lead, Oct 21): a message T3 Code's send can't carry (another
// provider here; also skills, or files past its limits) to a thread T3 Code
// owns but no longer has open waits while T3 Code runs, rather than running
// without T3 Code: nothing goes to T3 Code, Cupola starts no turn, nothing
// fails, a later message doesn't overtake it, and the phone says why.
#[tokio::test]
async fn a_message_t3_code_cant_carry_waits_on_a_thread_it_owns_but_closed() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    scene.released().await;
    let state = &scene.fx.state;
    let starts = scene.fx.claude_requests("turn/start").len();
    let model = serde_json::json!({"family": "codex", "model": "gpt-6.1"});
    let receipt = scene
        .send_model("13131313-1313-4313-8313-131313131313", false, model)
        .await;
    assert_eq!(receipt["queued"], true, "{receipt}");
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_wonder");
    assert_eq!(state.store.message_delivery_error(&id).await.unwrap(), None);
    assert!(crate::projects::send_waits(state, &scene.fx.chat, None).await);
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"], Value::Null);
    assert_eq!(activity["t3CodeCantSend"], "model", "{activity}");
    assert_eq!(activity["runsWithoutT3Code"], false);
    let later = scene
        .send("14141414-1414-4414-8414-141414141414", false)
        .await;
    assert_eq!(later["queued"], true, "{later}");
    assert!(fake.sends.lock().unwrap().is_empty());
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts);
}

// Contract (owner, Oct 21; lead, Oct 21): a thread T3 Code doesn't own is
// sent as without T3 Code at once and never waits on T3 Code, also in a
// folder where Start in T3 Code just started another thread, and also when
// its messages look like a T3 thread's: only T3 Code's own store, by the
// session's exact ID, makes a thread T3 Code's. The phone says nothing about
// T3 Code, connected or not.
#[tokio::test]
async fn a_thread_t3_code_doesnt_own_is_sent_at_once_without_t3_code() {
    use std::sync::atomic::Ordering::SeqCst;
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.released().await;
    let state = &scene.fx.state;
    // Start in T3 Code started a thread in this chat's folder just now; T3
    // Code runs it in a session of its own.
    let (status, launched) = crate::test_support::call(
        state,
        "POST",
        "/api/v1/projects/project/t3-code/threads",
        serde_json::json!({"title": "Release notes", "message": "Draft them"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{launched}");
    let other = uuid::Uuid::new_v4().to_string();
    fake.keeps(
        launched["threadId"].as_str().unwrap(),
        "claudeAgent",
        &other,
    )
    .await;
    let starts = scene.fx.claude_requests("turn/start").len();
    let lookups = fake.lookups.load(SeqCst);
    let activity = scene.held_by().await;
    assert_eq!(activity["runsWithoutT3Code"], false);
    assert_eq!(activity["t3CodeCantSend"], Value::Null);
    assert!(!crate::projects::send_waits(state, &scene.fx.chat, None).await);
    let receipt = scene
        .send("ffffffff-ffff-4fff-8fff-ffffffffffff", false)
        .await;
    assert_eq!(receipt["queued"], false);
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_codex");
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts + 1);
    assert_eq!(fake.lookups.load(SeqCst), lookups, "T3 Code wasn't asked");
    assert!(fake.sends.lock().unwrap().is_empty());
    fake.stop();
    assert_eq!(scene.held_by().await["runsWithoutT3Code"], false);
}

// Contract: when T3 Code stops answering, a message to a thread it has open
// waits (nothing stopped, no failure) and says why; nothing is sent twice.
#[tokio::test]
async fn a_chat_t3_code_holds_waits_while_t3_code_is_offline() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    // The phone looked at the chat, which matched the thread.
    scene.matched().await;
    fake.stop();
    let receipt = scene
        .send("cccccccc-cccc-4ccc-8ccc-cccccccccccc", false)
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let waiting = scene.dispatch(&id).await;
    assert_eq!(waiting.state, "accepted_by_wonder");
    assert_eq!(
        scene
            .fx
            .state
            .store
            .message_delivery_error(&id)
            .await
            .unwrap(),
        None
    );
    let notice = crate::projects::notice(&scene.fx.state, &scene.fx.chat).unwrap();
    assert!(notice.contains("didn't answer"), "{notice}");
    assert!(!scene.stopped());
    assert!(
        crate::projects::held(&scene.fx.state, &scene.fx.chat, None).await,
        "it waits for T3 Code or Send now"
    );
    assert!(fake.sends.lock().unwrap().is_empty());
}

// Contract (lead, Oct 19): the same for a Codex thread whose turn runs in
// T3 Code. Not connected, a message waits for T3 Code (which Send now cannot
// stop) and the phone says to connect it (whether dispatch holds it is
// Codex's busy evidence, covered with the codex CLI). Connected, the phone neither
// queues nor waits: the waiting message and the next one each go into the T3
// thread once, nothing on the Mac is stopped and Cupola starts no turn.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_codex_thread_t3_code_runs_goes_into_its_t3_thread_once_connected() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let mut scene = t3_codex_scene(&fake).await;
    let activity = scene.held_by().await;
    assert_eq!(activity["runningElsewhere"], true, "{activity}");
    assert_eq!(activity["heldBy"]["app"], "T3 Code");
    assert_eq!(activity["heldBy"]["t3Code"], "connect");
    assert_eq!(activity["heldBy"]["t3CodeReason"], "signedOut");
    assert_eq!(activity["heldBy"]["waits"], true);
    assert_eq!(activity["heldBy"]["canTakeOver"], false);
    assert_eq!(activity["openElsewhere"], true);
    let receipt = scene
        .send("dddddddd-dddd-4ddd-8ddd-dddddddddddd", false)
        .await;
    assert_eq!(receipt["queued"], true, "{receipt}");
    let waiting = receipt["wonderMessageId"].as_str().unwrap().to_owned();

    scene.fx.state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
    let activity = scene.matched().await;
    assert_eq!(activity["heldBy"]["waits"], false);
    assert_eq!(activity["openElsewhere"], false);
    assert_eq!(activity["sendStopsMacReply"], false);
    // The thread stays held; only sending goes around it.
    assert!(crate::projects::held(&scene.fx.state, &scene.fx.chat, None).await);
    let turns = || {
        scene.fx.codex_requests("turn/start").len() + scene.fx.codex_requests("turn/steer").len()
    };
    let before = turns();
    assert_eq!(scene.dispatch(&waiting).await.state, "completed");
    let receipt = scene
        .send("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee", false)
        .await;
    assert_eq!(receipt["queued"], false, "it does not wait for T3 Code");
    let next = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&next).await.state, "completed");
    assert_eq!(scene.dispatch(&next).await.state, "completed");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(
        sends
            .iter()
            .map(|send| (send["threadId"].as_str(), send["clientRequestId"].as_str()))
            .collect::<Vec<_>>(),
        [
            (Some("t3-codex"), Some(waiting.as_str())),
            (Some("t3-codex"), Some(next.as_str()))
        ],
        "each sent exactly once"
    );
    assert_eq!(fake.delivered(), 2);
    assert!(!scene.stopped(), "T3 Code keeps running the thread");
    assert_eq!(turns(), before, "Cupola starts no turn of its own");
}

// Contract (lead, Oct 19): a message T3 Code's send can't carry (more files
// than it takes here; also skills, larger files or another provider) goes
// the usual way: it waits for the thread T3 Code has open, the phone is told
// why, a later message does not overtake it, nothing goes to T3 Code and
// nothing fails; Send now still takes the thread over and sends it with
// Claude. (Another provider's Send now is a provider switch, not this
// takeover: `a_message_t3_code_cant_carry_waits_on_a_thread_it_owns_but_closed`
// and the Codex scene below cover its waiting.)
#[tokio::test]
async fn a_message_t3_code_cant_carry_waits_for_the_claude_thread_there() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    scene.matched().await;
    let client = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let mut files = Vec::new();
    for _ in 0..=crate::t3_code::MAX_FILES {
        files.push(scene.attach().await);
    }
    let receipt = scene.send_with(client, false, files.clone()).await;
    let starts = scene.fx.claude_requests("turn/start").len();
    assert_eq!(receipt["queued"], true, "{receipt}");
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let message = state.store.message_by_id(&id).await.unwrap().unwrap();
    assert!(
        crate::projects::held(state, &scene.fx.chat, Some(&message)).await,
        "the dispatch loop leaves it waiting"
    );
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_wonder");
    assert_eq!(state.store.message_delivery_error(&id).await.unwrap(), None);
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"]["t3Code"], "cantSend", "{activity}");
    assert_eq!(activity["heldBy"]["t3CodeLimit"], "attachments");
    assert_eq!(activity["heldBy"]["waits"], true);
    // Only macOS verifies another app's Claude process, so only it offers
    // Send now; Linux never signals it.
    assert_eq!(activity["heldBy"]["canTakeOver"], cfg!(target_os = "macos"));
    assert_eq!(activity["openElsewhere"], true);
    // A plain message after it waits its turn rather than going to T3 Code.
    let later = scene
        .send("abababab-abab-4bab-8bab-abababababab", false)
        .await;
    assert_eq!(later["queued"], true, "{later}");
    assert!(crate::projects::held(state, &scene.fx.chat, Some(&message)).await);
    assert!(fake.sends.lock().unwrap().is_empty());
    assert!(fake.attachment_sends.lock().unwrap().is_empty());
    assert!(!scene.stopped());
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts);
    if !cfg!(target_os = "macos") {
        return; // Linux offers no Send now (above).
    }

    // Send now takes the thread over from T3 Code and sends it.
    scene.send_with(client, true, files).await;
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_codex");
    assert!(scene.stopped());
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts + 1);
    assert!(fake.sends.lock().unwrap().is_empty());
    assert!(fake.attachment_sends.lock().unwrap().is_empty());
}

// Contract (owner, Oct 22): a message with another model of the same
// provider goes into the T3 thread T3 Code has open: T3 Code switches that
// thread first (read, then `t3_thread_configure` only where it differs),
// so the reply runs with the phone's choice and T3 Code's own composer
// shows it. A later message with the same choice switches nothing again.
// T3 Code's thread, not Cupola's record of it, is what a message is compared
// with: a message with the thread's own model switches it back, as it does
// after the owner changes the model in T3 Code's composer.
#[tokio::test]
async fn a_message_with_another_model_switches_the_t3_thread_first() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    let model = serde_json::json!({"family": "claude", "model": "claude-opus-5-5"});
    let receipt = scene
        .send_model("c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1", false, model.clone())
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"]["t3Code"], "sends", "{activity}");
    scene.dispatch(&id).await;
    let configures = fake.configures.lock().unwrap().clone();
    assert_eq!(configures.len(), 1, "{configures:?}");
    assert_eq!(configures[0]["threadId"], "t3-thread");
    assert_eq!(configures[0]["modelSelection"]["instanceId"], "claudeAgent");
    assert_eq!(configures[0]["modelSelection"]["model"], "claude-opus-5-5");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 1, "sent once, after the switch");
    assert_eq!(sends[0]["clientRequestId"], id.as_str());

    let receipt = scene
        .send_model("c2c2c2c2-c2c2-4c2c-8c2c-c2c2c2c2c2c2", false, model)
        .await;
    let next = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    scene.dispatch(&next).await;
    assert_eq!(fake.configures.lock().unwrap().len(), 1, "already switched");
    assert_eq!(fake.sends.lock().unwrap().len(), 2);

    // The thread's own model (T3 Code knows it as `claude-sonnet-5-5`).
    let own = |fake: &crate::t3_code::tests::Fake| {
        let configures = fake.configures.lock().unwrap();
        let selection = &configures.last().unwrap()["modelSelection"];
        (configures.len(), selection["model"].clone())
    };
    let receipt = scene
        .send("c3c3c3c3-c3c3-4c3c-8c3c-c3c3c3c3c3c3", false)
        .await;
    let back = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    scene.dispatch(&back).await;
    assert_eq!(own(&fake), (2, "claude-sonnet-5-5".into()), "switched back");
    assert_eq!(fake.sends.lock().unwrap().len(), 3);

    // The owner picks another model in T3 Code's composer: the next
    // message from the phone runs with the thread's own again.
    fake.configured.lock().unwrap().insert(
        "t3-thread".into(),
        serde_json::json!({"instanceId": "claudeAgent", "model": "claude-opus-5-5",
            "options": {"effort": "medium"}}),
    );
    let receipt = scene
        .send("c4c4c4c4-c4c4-4c4c-8c4c-c4c4c4c4c4c4", false)
        .await;
    let after = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    scene.dispatch(&after).await;
    assert_eq!(own(&fake), (3, "claude-sonnet-5-5".into()));
    assert_eq!(fake.sends.lock().unwrap().len(), 4);
    assert!(!scene.stopped(), "T3 Code keeps running the thread");
}

// Contract (lead, Oct 19): the same for a Codex thread whose turn runs in
// T3 Code: a message with another provider waits for that turn (which Send now can't
// stop), says why, and Cupola neither sends it to T3 Code nor starts a turn.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_message_t3_code_cant_carry_waits_for_the_codex_turn_there() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let mut scene = t3_codex_scene(&fake).await;
    scene.fx.state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
    // Codex reports the turn T3 Code runs as running (`codex_busy`).
    let idle = scene.fx.dir.path().join("idle-fixture.json");
    let mut threads: Value =
        serde_json::from_str(&std::fs::read_to_string(&idle).unwrap()).unwrap();
    threads[scene.session.as_str()]["status"]["type"] = "active".into();
    std::fs::write(&idle, threads.to_string()).unwrap();
    let state = &scene.fx.state;
    scene.matched().await;
    let turns = || {
        scene.fx.codex_requests("turn/start").len() + scene.fx.codex_requests("turn/steer").len()
    };
    let before = turns();
    let receipt = scene
        .send_model(
            "acacacac-acac-4cac-8cac-acacacacacac",
            false,
            serde_json::json!({"family": "claude", "model": "claude-opus-5-5"}),
        )
        .await;
    assert_eq!(receipt["queued"], true, "{receipt}");
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let message = state.store.message_by_id(&id).await.unwrap().unwrap();
    assert!(
        crate::projects::held(state, &scene.fx.chat, Some(&message)).await,
        "the dispatch loop leaves it waiting"
    );
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"]["t3Code"], "cantSend", "{activity}");
    assert_eq!(activity["heldBy"]["t3CodeLimit"], "model");
    assert_eq!(activity["heldBy"]["waits"], true);
    assert_eq!(activity["heldBy"]["canTakeOver"], false);
    assert_eq!(activity["openElsewhere"], true);
    let waiting = state.store.message_by_id(&id).await.unwrap().unwrap();
    assert_eq!(waiting.state, "accepted_by_wonder");
    assert_eq!(state.store.message_delivery_error(&id).await.unwrap(), None);
    assert!(fake.sends.lock().unwrap().is_empty());
    assert!(!scene.stopped());
    assert_eq!(turns(), before, "Cupola starts no turn of its own");
}

// Contract (lead, Oct 19): T3 Code's thread is looked up in the background,
// never while a send waits for the answer. A failed lookup is retried only
// after a wait that doubles; meanwhile the chat is held as without T3 Code
// and its message waits (nothing fails, nothing is sent). Once a lookup
// finds the thread, dispatch is woken and the message goes in exactly once.
#[tokio::test]
async fn failed_t3_code_lookups_back_off_and_a_later_one_delivers_once() {
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    // The backoff is real time and the test reads what is left of it, so it
    // is long enough that no stall of the test eats it; the test ends each
    // wait instead of sleeping through it.
    state
        .t3_code
        .set_lookup_timing(Duration::from_secs(5), Duration::from_secs(60));
    fake.failing.store(true, SeqCst);
    let receipt = scene
        .send("b1b1b1b1-b1b1-4b1b-8b1b-b1b1b1b1b1b1", false)
        .await;
    assert_eq!(receipt["queued"], true, "held until the thread is found");
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    scene.lookup_done().await;
    assert_eq!(fake.lookups.load(SeqCst), 1);
    let wait = state.t3_code.lookup_state(&scene.session).1.unwrap();
    assert!(
        wait > Duration::from_secs(30) && wait <= Duration::from_secs(60),
        "{wait:?}"
    );
    // Within the wait nothing asks T3 Code again, and the message waits.
    let activity = scene.held_by().await;
    assert_eq!(activity["heldBy"]["t3Code"], "notFound", "{activity}");
    assert_eq!(activity["heldBy"]["t3CodeReason"], "unreachable");
    assert_eq!(activity["heldBy"]["waits"], true);
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_wonder");
    assert!(crate::projects::send_waits(state, &scene.fx.chat, None).await);
    assert_eq!(fake.lookups.load(SeqCst), 1, "backing off");
    // The next failure waits twice as long: more is left of it than the
    // first wait ever had.
    state.t3_code.end_lookup_backoff_for_test(&scene.session);
    scene.held_by().await;
    scene.lookup_done().await;
    assert_eq!(fake.lookups.load(SeqCst), 2);
    let wait = state.t3_code.lookup_state(&scene.session).1.unwrap();
    assert!(
        wait > Duration::from_secs(60) && wait <= Duration::from_secs(120),
        "{wait:?}"
    );
    assert_eq!(state.store.message_delivery_error(&id).await.unwrap(), None);
    assert!(fake.sends.lock().unwrap().is_empty());

    // T3 Code answers again: the next lookup finds the thread and wakes
    // dispatch (a wake left from the send is dropped first).
    fake.failing.store(false, SeqCst);
    state.t3_code.end_lookup_backoff_for_test(&scene.session);
    let _ = tokio::time::timeout(Duration::from_millis(10), state.dispatch_wake.notified()).await;
    scene.matched().await;
    assert_eq!(fake.lookups.load(SeqCst), 3);
    tokio::time::timeout(Duration::from_secs(1), state.dispatch_wake.notified())
        .await
        .expect("a found thread wakes dispatch");
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 1, "sent exactly once");
    assert_eq!(sends[0]["clientRequestId"], id.as_str());
    assert_eq!(fake.delivered(), 1);
    assert!(!scene.stopped());
}

// Contract (lead, Oct 19): a T3 Code that never answers delays no send. The
// dispatch loop, the send receipt and the phone's view of a thread it has
// open answer at once (held, as without T3 Code), and another chat's
// message is dispatched in the same pass. T3 Code then fails the hung
// lookup, which backs off; once T3 Code answers, the dispatch loop sends
// the waiting message into its thread once.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_t3_code_that_never_answers_delays_no_other_chats_send() {
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;
    // One hang guard over the whole scenario, so a step blocked for good
    // fails (naming the step) instead of hanging the suite. No load delays
    // a step that long, and nothing tighter times any step.
    let step = std::sync::Mutex::new("start");
    let mark = |name: &'static str| *step.lock().unwrap() = name;
    let scenario = async {
        mark("set up T3 Code and the chats");
        let fake = crate::t3_code::tests::Fake::start().await;
        let mut scene = t3_codex_scene(&fake).await;
        // T3 Code's held answer must not race the client's request timeout.
        scene.fx.state.t3_code =
            crate::t3_code::tests::connected_with(&fake, |t3| t3.without_request_timeout())
                .await
                .0;
        let state = scene.fx.state.clone();
        // Codex reports the turn T3 Code runs as running (`codex_busy`).
        let idle = scene.fx.dir.path().join("idle-fixture.json");
        let mut threads: Value =
            serde_json::from_str(&std::fs::read_to_string(&idle).unwrap()).unwrap();
        threads[scene.session.as_str()]["status"]["type"] = "active".into();
        std::fs::write(&idle, threads.to_string()).unwrap();
        // T3 Code answers no call until the test lets it, and the lookup never
        // gives up on its own: every step below must finish while the lookup
        // still waits. One that waited for T3 Code would never finish (the
        // hang guard turns that into a failure), so no step races a clock.
        state
            .t3_code
            .set_lookup_timing(Duration::from_secs(24 * 3600), Duration::from_millis(200));
        let looking = || state.t3_code.lookup_state(&scene.session).0;
        fake.hanging.send_replace(true);

        mark("the phone's view of the chat");
        // Looking at the chat starts the lookup.
        let activity = scene.held_by().await;
        assert!(looking(), "the phone's view waited for T3 Code");
        assert_eq!(activity["heldBy"]["t3Code"], "notFound", "{activity}");
        assert_eq!(activity["heldBy"]["t3CodeReason"], "finding");
        assert_eq!(activity["heldBy"]["waits"], true);
        mark("the send receipt");
        let receipt = scene
            .send("c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1", false)
            .await;
        assert!(looking(), "the send receipt waited for T3 Code");
        assert_eq!(receipt["queued"], true, "{receipt}");
        let held = receipt["wonderMessageId"].as_str().unwrap().to_owned();

        mark("queue the other chat's message");
        // Another chat's message, queued after it, goes in the same pass (the
        // fake runtime's turn pages are every thread's: its latest turn ended).
        std::fs::remove_file(scene.fx.dir.path().join("turn-pages.json")).unwrap();
        scene.fx.offer_models().await;
        let client = uuid::Uuid::new_v4().to_string();
        state
            .store
            .stage_project_message_target(
                "owner",
                &client,
                AgentFamily::Codex,
                "fake",
                None,
                None,
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        let MessageInsert::Inserted(other) = state
            .store
            .insert_dispatch_message(
                "owner",
                &client,
                "Elsewhere",
                "hash",
                scene.other_chat.as_deref().unwrap(),
                &[],
                "2026-01-01T00:00:00Z",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        mark("the first dispatch pass");
        crate::dispatch::tick(&state).await.unwrap();
        assert!(looking(), "the dispatch loop waited for T3 Code");
        let state_of = |id: String| {
            let state = state.clone();
            async move { state.store.message_by_id(&id).await.unwrap().unwrap().state }
        };
        // That pass hands the message to its own task, which claims it, and
        // Codex accepts it while T3 Code still has not answered the lookup.
        mark("Codex accepting the other chat's message");
        let accepted = async {
            loop {
                if state_of(other.id.clone()).await == "accepted_by_codex" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        accepted.await;
        assert!(looking(), "T3 Code's lookup ended before the test let it");
        // The lookup task may reach T3 Code only now; wait for it (the hang
        // guard bounds this), then it must be the only one.
        mark("the lookup reaching T3 Code");
        while fake.lookups.load(SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(fake.lookups.load(SeqCst), 1);
        assert_eq!(state_of(held.clone()).await, "accepted_by_wonder");
        assert!(fake.sends.lock().unwrap().is_empty());

        mark("T3 Code failing the lookup");
        // T3 Code answers the hung lookup with an error: it backs off.
        fake.hanging.send_replace(false);
        scene.lookup_done().await;
        assert_eq!(fake.lookups.load(SeqCst), 1);
        assert!(state.t3_code.lookup_state(&scene.session).1.is_some());

        // T3 Code answers again: the dispatch loop sends the message in once.
        mark("sending the waiting message into T3 Code");
        let completed = async {
            loop {
                crate::dispatch::tick(&state).await.unwrap();
                if state_of(held.clone()).await == "completed" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        completed.await;
        assert_eq!(state_of(held.clone()).await, "completed");
        let sends = fake.sends.lock().unwrap().clone();
        assert_eq!(
            sends
                .iter()
                .map(|send| (send["threadId"].as_str(), send["clientRequestId"].as_str()))
                .collect::<Vec<_>>(),
            [(Some("t3-codex"), Some(held.as_str()))],
            "sent exactly once"
        );
        assert_eq!(fake.delivered(), 1);
        assert!(!scene.stopped());
    };
    if tokio::time::timeout(Duration::from_secs(120), scenario)
        .await
        .is_err()
    {
        panic!("hung: {}", step.lock().unwrap());
    }
}

// Contract (owner, Oct 20, B7): a thread T3 Code runs offers what any
// running thread does. Send queues behind T3 Code's turn, Send now steers it
// and Take over restarts it with the message, all through T3 Code's own
// send with that mode, plain text, and nothing on the Mac is stopped. A
// steer that finds the turn ended starts the next one instead. Stop asks
// T3 Code to interrupt its turn.
#[tokio::test]
async fn a_t3_code_thread_takes_queue_steer_take_over_and_stop() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    let chat = scene.fx.chat.clone();
    scene.matched().await;
    // A turn runs in T3 Code.
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "t3-turn", "status": "inProgress"}]),
    );
    let last = || fake.sends.lock().unwrap().last().cloned().unwrap();

    let queued = scene
        .send("d1d1d1d1-d1d1-4d1d-8d1d-d1d1d1d1d1d1", false)
        .await;
    let id = queued["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(last()["mode"], "queue");
    assert_eq!(last()["message"], "Continue here", "plain, no provenance");

    let steer_path = format!("/api/v1/conversations/{chat}/turns/t3-turn/steer");
    let steer = |client: &'static str| {
        crate::test_support::call(
            state,
            "POST",
            &steer_path,
            serde_json::json!({"deviceId": "owner", "clientMessageId": client,
                "body": "Also check the dates", "expectedTurnId": "t3-turn"}),
        )
    };
    let (status, receipt) = steer("d2d2d2d2-d2d2-4d2d-8d2d-d2d2d2d2d2d2").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{receipt}");
    assert_eq!(receipt["queued"], false, "{receipt}");
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(last()["mode"], "steer");
    assert_eq!(last()["message"], "Also check the dates");

    let take_over = scene
        .send("d3d3d3d3-d3d3-4d3d-8d3d-d3d3d3d3d3d3", true)
        .await;
    assert_eq!(take_over["queued"], false, "{take_over}");
    let id = take_over["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(last()["mode"], "restart");
    assert!(!scene.stopped(), "T3 Code restarts its own turn");

    let (status, reply) = crate::test_support::call(
        state,
        "POST",
        &format!("/api/v1/conversations/{chat}/turns/t3-turn/interrupt"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply["stopConfirmed"], false);
    let interrupts = fake.interrupts.lock().unwrap().clone();
    assert_eq!(interrupts.len(), 1);
    assert_eq!(interrupts[0]["threadId"], "t3-thread");
    assert_eq!(interrupts[0]["runId"], "run-0", "the run that is active");

    // A Stop that arrives after its turn ended and the next one started
    // stops nothing.
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "t3-next", "status": "inProgress"}]),
    );
    let (status, reply) = crate::test_support::call(
        state,
        "POST",
        &format!("/api/v1/conversations/{chat}/turns/t3-turn/interrupt"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply["error"].as_str().unwrap().contains("already ended"),
        "{reply}"
    );
    assert_eq!(fake.interrupts.lock().unwrap().len(), 1, "nothing stopped");
    // Nor one when Cupola can't read which reply runs now.
    scene.fx.write("claude-turns.json", &serde_json::json!([]));
    let (status, reply) = crate::test_support::call(
        state,
        "POST",
        &format!("/api/v1/conversations/{chat}/turns/t3-next/interrupt"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply}");
    assert_eq!(fake.interrupts.lock().unwrap().len(), 1, "nothing stopped");
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "t3-next", "status": "inProgress"}]),
    );
    // Another run replaces the one Stop found before the interrupt goes:
    // the Stop names the run it found, so the new one keeps running.
    fake.replace_after_read
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, reply) = crate::test_support::call(
        state,
        "POST",
        &format!("/api/v1/conversations/{chat}/turns/t3-next/interrupt"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    let interrupts = fake.interrupts.lock().unwrap().clone();
    assert_eq!(interrupts.len(), 2);
    assert_eq!(
        interrupts[1]["runId"], "run-0",
        "not the run-1 that replaced it"
    );

    // Guidance for a reply that another one replaced by the time it goes
    // doesn't steer the new one: it waits behind it.
    let (_, receipt) = steer("d5d5d5d5-d5d5-4d5d-8d5d-d5d5d5d5d5d5").await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(last()["clientRequestId"], id.as_str());
    assert_eq!(last()["mode"], "queue", "not into t3-next");

    // The turn ended before the steer arrived: it starts the next turn.
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "t3-turn", "status": "completed"}]),
    );
    fake.queue_sends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (_, receipt) = steer("d4d4d4d4-d4d4-4d4d-8d4d-d4d4d4d4d4d4").await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    let modes: Vec<Value> = fake
        .sends
        .lock()
        .unwrap()
        .iter()
        .filter(|send| send["clientRequestId"] == id.as_str())
        .map(|send| send["mode"].clone())
        .collect();
    // The idle thread is read first: no steer is attempted into it.
    assert_eq!(modes, ["queue"]);
    let texts = fake.texts("t3-thread");
    assert_eq!(
        texts
            .iter()
            .filter(|text| *text == "Also check the dates")
            .count(),
        3,
        "{texts:?}"
    );
    assert!(!scene.stopped());
}

// Contract (owner, Oct 22): a message T3 Code queues behind its running
// turn shows as queued on the phone, as T3 Code shows it, until T3 Code
// starts it; Send now steers it into that turn, and Edit and Cancel change
// it in T3 Code's queue. The invariant (`t3_code::queue`): the queue lists
// the message exactly while T3 Code's queue has its run; it then shows as
// sent once T3 Code's timeline has it, and as cancelled otherwise. One
// ordering per row, each from a fresh scene.
#[tokio::test]
async fn a_message_t3_code_queues_shows_queued_until_t3_code_takes_it() {
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// T3 Code's turn ends and it starts the next queued message.
        TurnEnds,
        /// Send now from the phone's queue.
        SendNow,
        /// Send now with a turn the phone saw that already ended.
        StaleSendNow,
        /// Edited from the phone; then with the revision before that.
        Edit,
        StaleEdit,
        Cancel,
        /// The owner edits or cancels it in T3 Code.
        EditedThere,
        CancelledThere,
        /// The owner deletes the T3 thread in T3 Code.
        ThreadDeleted,
        /// T3 Code stops answering; then answers again.
        Unreachable,
        Reachable,
        /// Cupola restarts.
        Restart,
        /// The answer to the send was lost after T3 Code queued it.
        LostAnswer,
    }
    /// What the phone's queue and the message show after a step.
    #[derive(Debug, PartialEq)]
    enum Shows {
        Sent,
        Queued,
        Cancelled,
    }
    use Shows::*;
    use Step::*;
    /// A row: its name, whether a turn runs in T3 Code at the send, and
    /// what the phone shows after each step.
    type Row = (&'static str, bool, &'static [(Step, Shows)]);
    let table: &[Row] = &[
        ("send while idle", false, &[]),
        (
            "send while running, delivered at turn end",
            true,
            &[(TurnEnds, Sent)],
        ),
        (
            "steer from the queue",
            true,
            &[(StaleSendNow, Queued), (SendNow, Sent)],
        ),
        (
            "edit, then a stale edit, then delivered",
            true,
            &[(Edit, Queued), (StaleEdit, Queued), (TurnEnds, Sent)],
        ),
        (
            "edited in T3 Code",
            true,
            &[(EditedThere, Queued), (TurnEnds, Sent)],
        ),
        ("cancelled in T3 Code", true, &[(CancelledThere, Cancelled)]),
        ("cancelled from the phone", true, &[(Cancel, Cancelled)]),
        ("its T3 thread deleted", true, &[(ThreadDeleted, Cancelled)]),
        (
            "T3 Code unreachable mid-way",
            true,
            &[(Unreachable, Queued), (Reachable, Queued), (TurnEnds, Sent)],
        ),
        (
            "Cupola restarts while queued",
            true,
            &[(Restart, Queued), (CancelledThere, Cancelled)],
        ),
        (
            "the send's answer was lost",
            true,
            &[(LostAnswer, Queued), (SendNow, Sent)],
        ),
    ];
    for (name, running, steps) in table {
        let fake = crate::t3_code::tests::Fake::start().await;
        let scene = t3_scene(&fake).await;
        let state = &scene.fx.state;
        let chat = scene.fx.chat.clone();
        scene.matched().await;
        fake.queue_sends
            .store(*running, std::sync::atomic::Ordering::SeqCst);
        scene.fx.write(
            "claude-turns.json",
            &serde_json::json!([{"id": "t3-turn", "status": "inProgress"}]),
        );
        let lost = matches!(steps.first(), Some((LostAnswer, _)));
        if lost {
            *fake.lose.lock().unwrap() = Some(crate::t3_code::tests::Lose::ErrorAfterApplying);
        }
        let receipt = scene
            .send("e1e1e1e1-e1e1-4e1e-8e1e-e1e1e1e1e1e1", false)
            .await;
        let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
        let sent = scene.dispatch(&id).await;
        assert_eq!(
            sent.state,
            if lost { "uncertain" } else { "completed" },
            "{name}"
        );
        let queue = || async {
            let (status, queue) = crate::test_support::call(
                state,
                "GET",
                &format!("/api/v1/conversations/{chat}/queue"),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{name}: {queue}");
            crate::tests::validate_http_contract("queueList", &queue);
            queue.as_array().unwrap().clone()
        };
        let change_path = format!("/api/v1/conversations/{chat}/queue/{id}");
        let change = |body: Value| {
            let path = change_path.clone();
            async move { crate::test_support::call(state, "POST", &path, body).await }
        };
        let shows = || async {
            let listed = queue().await;
            let message = state.store.message_by_id(&id).await.unwrap().unwrap();
            match (listed.as_slice(), message.state.as_str()) {
                ([item], "completed") => {
                    assert_eq!(item["id"], id.as_str(), "{name}");
                    assert_eq!(item["waitsInT3Code"], true, "{name}");
                    assert_eq!(
                        item["clientMessageId"],
                        "e1e1e1e1-e1e1-4e1e-8e1e-e1e1e1e1e1e1"
                    );
                    assert_eq!(item["body"], message.body.as_str(), "{name}");
                    Queued
                }
                ([], "completed") => Sent,
                ([], "interrupted") => Cancelled,
                other => panic!("{name}: {other:?}"),
            }
        };
        let revision = || async { queue().await[0]["revision"].as_i64().unwrap() };
        let t3_calls = || fake.queue_changes.lock().unwrap().len();
        if lost {
            // Being checked: neither queued nor sent yet.
            assert!(queue().await.is_empty(), "{name}");
        } else {
            assert_eq!(
                shows().await,
                if *running { Queued } else { Sent },
                "{name}"
            );
        }
        if !*running {
            assert!(fake.queued_runs("t3-thread").is_empty());
        }
        for (step, expected) in steps.iter() {
            match step {
                TurnEnds => {
                    fake.start_next("t3-thread");
                    crate::t3_code::queue::check(state, &chat).await.unwrap();
                }
                SendNow | StaleSendNow => {
                    let turn = if matches!(step, SendNow) {
                        "t3-turn"
                    } else {
                        "t3-old"
                    };
                    let calls = t3_calls();
                    let (status, reply) = change(serde_json::json!({
                        "expectedRevision": revision().await, "expectedTurnId": turn}))
                    .await;
                    if matches!(step, SendNow) {
                        assert_eq!(status, StatusCode::NO_CONTENT, "{name}: {reply}");
                        let (tool, arguments) =
                            fake.queue_changes.lock().unwrap().last().cloned().unwrap();
                        assert_eq!(tool, "t3_queue_promote_to_steer");
                        assert_eq!(arguments["targetRunId"], "run-0", "the run active now");
                        assert!(fake.queued_runs("t3-thread").is_empty());
                    } else {
                        assert_eq!(status, StatusCode::CONFLICT, "{name}: {reply}");
                        assert_eq!(t3_calls(), calls, "{name}: nothing asked of T3 Code");
                    }
                }
                Edit | StaleEdit => {
                    let calls = t3_calls();
                    let current = revision().await;
                    let (status, reply) = change(serde_json::json!({
                        "expectedRevision": if matches!(step, Edit) { current } else { current - 1 },
                        "body": "Continue here, carefully"})).await;
                    if matches!(step, Edit) {
                        assert_eq!(status, StatusCode::NO_CONTENT, "{name}: {reply}");
                        assert_eq!(
                            fake.texts("t3-thread").last().unwrap(),
                            "Continue here, carefully"
                        );
                        assert_eq!(revision().await, current + 1);
                    } else {
                        assert_eq!(status, StatusCode::CONFLICT, "{name}: {reply}");
                        assert_eq!(t3_calls(), calls, "{name}: nothing asked of T3 Code");
                    }
                    assert_eq!(queue().await[0]["body"], "Continue here, carefully");
                }
                Cancel => {
                    let (status, reply) = change(serde_json::json!({
                        "expectedRevision": revision().await, "cancel": true}))
                    .await;
                    assert_eq!(status, StatusCode::NO_CONTENT, "{name}: {reply}");
                    assert_eq!(
                        fake.queue_changes.lock().unwrap().last().unwrap().0,
                        "t3_queue_cancel"
                    );
                    assert!(fake.queued_runs("t3-thread").is_empty());
                }
                EditedThere => {
                    fake.change_queued_here("t3-thread", Some("Continue there"));
                    // A read the phone's queue read started may have got there first.
                    crate::t3_code::queue::check(state, &chat).await.unwrap();
                    assert_eq!(queue().await[0]["body"], "Continue there", "{name}");
                }
                CancelledThere => {
                    fake.change_queued_here("t3-thread", None);
                    crate::t3_code::queue::check(state, &chat).await.unwrap();
                }
                ThreadDeleted => {
                    fake.delete_thread("t3-thread");
                    crate::t3_code::queue::check(state, &chat).await.unwrap();
                }
                Unreachable => {
                    fake.failing
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    assert!(crate::t3_code::queue::check(state, &chat).await.is_err());
                    let (status, reply) = change(serde_json::json!({
                        "expectedRevision": revision().await, "cancel": true}))
                    .await;
                    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{name}: {reply}");
                    assert_eq!(
                        fake.queued_runs("t3-thread").len(),
                        1,
                        "{name}: nothing changed"
                    );
                }
                Reachable => {
                    fake.failing
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    assert!(!crate::t3_code::queue::check(state, &chat).await.unwrap());
                }
                Restart => {
                    state.t3_code.restarted_for_test();
                    scene.matched().await;
                }
                LostAnswer => {
                    crate::projects::recover(state).await;
                    assert_eq!(
                        fake.sends.lock().unwrap().len(),
                        1,
                        "{name}: read back, not sent again"
                    );
                }
            }
            assert_eq!(&shows().await, expected, "{name} after {step:?}");
        }
        // Whatever happened, T3 Code has the message at most once, and a
        // message shown as sent is in its timeline.
        let message = state.store.message_by_id(&id).await.unwrap().unwrap();
        if !steps.iter().any(|(step, _)| matches!(step, ThreadDeleted)) {
            let in_t3 = fake
                .texts("t3-thread")
                .iter()
                .filter(|text| **text == message.body)
                .count();
            assert_eq!(in_t3, usize::from(shows().await != Cancelled), "{name}");
        }
        assert!(!scene.stopped(), "{name}");
    }
}

// Contract (owner, Oct 20, B7): a message with a file goes into the T3
// thread too: the file is uploaded to T3 Code byte for byte and sent with
// the text. T3 Code takes files only into a running turn, so Send waits in
// Cupola for T3 Code's reply to end (as Send queues after it) while Send now
// goes into it. When T3 Code's answer is lost, recovery looks for this
// send's own uploads, so an earlier message with the same text and file
// never counts as this one.
#[tokio::test]
async fn a_message_with_files_goes_into_the_t3_thread_once() {
    use crate::t3_code::tests::Lose;
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    let chat = scene.fx.chat.clone();
    let starts = scene.fx.claude_requests("turn/start").len();
    scene.matched().await;
    let sent = || fake.attachment_sends.lock().unwrap().clone();

    // T3 Code is writing a reply: Send now goes into it.
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "t3-turn", "status": "inProgress"}]),
    );
    let file = scene.attach().await;
    let (status, receipt) = crate::test_support::call(
        state,
        "POST",
        &format!("/api/v1/conversations/{chat}/turns/t3-turn/steer"),
        serde_json::json!({"deviceId": "owner", "clientMessageId": "e0e0e0e0-e0e0-4e0e-8e0e-e0e0e0e0e0e0",
            "body": "Also this", "expectedTurnId": "t3-turn", "attachmentIds": [file]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{receipt}");
    let steered = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&steered).await.state, "completed");
    assert_eq!(sent().len(), 1);
    assert_eq!(sent()[0]["message"], "Also this");

    // Send waits for it.
    let file = scene.attach().await;
    let receipt = scene
        .send_with("e1e1e1e1-e1e1-4e1e-8e1e-e1e1e1e1e1e1", false, vec![file])
        .await;
    let first = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&first).await.state, "accepted_by_wonder");
    assert_eq!(sent().len(), 1, "nothing steers the running reply");

    // The reply ended: the waiting message goes.
    scene.replying(false);
    assert_eq!(scene.dispatch(&first).await.state, "completed");
    assert_eq!(fake.uploaded("notes.txt").unwrap(), b"Release notes");
    let sent_now = sent();
    assert_eq!(sent_now.len(), 2);
    assert_eq!(sent_now[1]["threadId"], "t3-thread");
    assert_eq!(sent_now[1]["message"], "Continue here");
    assert_eq!(sent_now[1]["attachments"][0]["name"], "notes.txt");
    assert_eq!(sent_now[1]["attachments"][0]["type"], "file");
    assert!(fake.sends.lock().unwrap().is_empty());
    assert!(!scene.stopped());
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts);

    // The same text and file again, and Cupola stops before T3 Code applied
    // it: the earlier message is not this one, so it waits to go again.
    *fake.lose.lock().unwrap() = Some(Lose::BeforeApplying);
    let file = scene.attach().await;
    let receipt = scene
        .send_with("e2e2e2e2-e2e2-4e2e-8e2e-e2e2e2e2e2e2", false, vec![file])
        .await;
    let repeat = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let received = fake.received.load(std::sync::atomic::Ordering::SeqCst);
    let message = state.store.message_by_id(&repeat).await.unwrap().unwrap();
    let dispatch = tokio::spawn(crate::projects::dispatch(state.clone(), message));
    while fake.received.load(std::sync::atomic::Ordering::SeqCst) == received {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    dispatch.abort();
    let _ = dispatch.await;
    state.store.recover_dispatch_claims().await.unwrap();
    assert_eq!(
        state
            .store
            .message_by_id(&repeat)
            .await
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    crate::projects::recover(state).await;
    assert_eq!(
        state
            .store
            .message_by_id(&repeat)
            .await
            .unwrap()
            .unwrap()
            .state,
        "accepted_by_wonder",
        "not marked delivered by the earlier message"
    );
    assert_eq!(scene.dispatch(&repeat).await.state, "completed");
    let texts = fake.texts("t3-thread");
    assert_eq!(
        texts.iter().filter(|text| *text == "Continue here").count(),
        2,
        "{texts:?}"
    );

    // T3 Code applied it and then failed the request: read back, not sent
    // again.
    *fake.lose.lock().unwrap() = Some(Lose::ErrorAfterApplying);
    let file = scene.attach().await;
    let receipt = scene
        .send_with("e3e3e3e3-e3e3-4e3e-8e3e-e3e3e3e3e3e3", false, vec![file])
        .await;
    let lost = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&lost).await.state, "uncertain");
    let before = sent().len();
    crate::projects::recover(state).await;
    assert_eq!(scene.dispatch(&lost).await.state, "completed");
    assert_eq!(sent().len(), before, "read back, not sent again");
}

// Contract (lead, Oct 20, B7): uploading files takes a while, and T3 Code's
// reply may change meanwhile. Send now's guidance never goes into a reply
// that replaced the one it was for, and Send's files never go into a reply
// that started during the upload: both wait for that reply to end.
#[tokio::test]
async fn a_reply_that_changes_during_the_upload_is_not_steered() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    let chat = scene.fx.chat.clone();
    scene.matched().await;
    let sent = || fake.attachment_sends.lock().unwrap().len();
    let turns = scene.fx.dir.path().join("claude-turns.json");
    let record = scene.records.path().join(format!("{}.json", scene.holder));

    // Send now into t3-turn; t3-next replaces it while the file uploads.
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "t3-turn", "status": "inProgress"}]),
    );
    let replaced = turns.clone();
    *fake.during_upload.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(
            &replaced,
            serde_json::json!([{"id": "t3-next", "status": "inProgress"}]).to_string(),
        )
        .unwrap();
    }));
    let file = scene.attach().await;
    let (status, receipt) = crate::test_support::call(
        state,
        "POST",
        &format!("/api/v1/conversations/{chat}/turns/t3-turn/steer"),
        serde_json::json!({"deviceId": "owner", "clientMessageId": "e4e4e4e4-e4e4-4e4e-8e4e-e4e4e4e4e4e4",
            "body": "Also this", "expectedTurnId": "t3-turn", "attachmentIds": [file]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{receipt}");
    let steered = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&steered).await.state, "accepted_by_wonder");
    assert_eq!(sent(), 0, "not into t3-next");
    scene.replying(false);
    assert_eq!(scene.dispatch(&steered).await.state, "completed");
    assert_eq!(sent(), 1);

    // Send with nothing running; a reply starts while the file uploads.
    *fake.during_upload.lock().unwrap() = Some(Box::new(move || set_replying(&record, true)));
    let file = scene.attach().await;
    let receipt = scene
        .send_with("e5e5e5e5-e5e5-4e5e-8e5e-e5e5e5e5e5e5", false, vec![file])
        .await;
    let waiting = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&waiting).await.state, "accepted_by_wonder");
    assert_eq!(sent(), 1, "nothing steers the reply that started");
    scene.replying(false);
    assert_eq!(scene.dispatch(&waiting).await.state, "completed");
    assert_eq!(sent(), 2);
    assert!(!scene.stopped());
}

// Contract (lead, Oct 20, B7): Take over on a thread T3 Code runs restarts
// T3 Code's turn and never quits T3 Code, even when an earlier message with
// a file waits for T3 Code's reply: that one goes first, restarting the
// turn, and the message that asked follows it in order.
#[tokio::test]
async fn take_over_behind_a_waiting_file_never_quits_t3_code() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let starts = scene.fx.claude_requests("turn/start").len();

    let file = scene.attach().await;
    let receipt = scene
        .send_with("f1f1f1f1-f1f1-4f1f-8f1f-f1f1f1f1f1f1", false, vec![file])
        .await;
    let waiting = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&waiting).await.state, "accepted_by_wonder");

    let receipt = scene
        .send("f2f2f2f2-f2f2-4f2f-8f2f-f2f2f2f2f2f2", true)
        .await;
    let taking = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(
        scene.dispatch(&taking).await.state,
        "accepted_by_wonder",
        "in order"
    );
    assert_eq!(scene.dispatch(&waiting).await.state, "completed");
    assert!(!scene.stopped(), "the waiting file doesn't quit T3 Code");
    let interrupts = fake.interrupts.lock().unwrap().clone();
    assert_eq!(interrupts.len(), 1, "T3 Code restarts its turn");
    let waits = fake.waits.lock().unwrap().clone();
    assert_eq!(waits.len(), 1, "the file waits for the reply to stop");
    assert_eq!(waits[0]["runId"], "run-0");
    assert_eq!(fake.attachment_sends.lock().unwrap().len(), 1);
    assert_eq!(scene.dispatch(&taking).await.state, "completed");
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(sends[0]["mode"], "queue", "one restart only");
    assert_eq!(sends[0]["clientRequestId"], taking.as_str());
    assert!(!scene.stopped());
    assert_eq!(scene.fx.claude_requests("turn/start").len(), starts);
}

// Contract (lead, Oct 20, B7): Take over with a file sends the file only
// once T3 Code says its reply stopped, never into the reply that is still
// stopping. A reply that doesn't stop in time sends nothing and says so;
// the message then waits for that reply to end, as Send does.
#[tokio::test]
async fn take_over_with_a_file_waits_for_t3_codes_reply_to_stop() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    scene.matched().await;
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fake.slow_stop
        .store(true, std::sync::atomic::Ordering::SeqCst);
    scene.replying(true);

    let file = scene.attach().await;
    let receipt = scene
        .send_with("f3f3f3f3-f3f3-4f3f-8f3f-f3f3f3f3f3f3", true, vec![file])
        .await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_wonder");
    assert_eq!(fake.interrupts.lock().unwrap().len(), 1);
    assert_eq!(fake.waits.lock().unwrap().len(), 1);
    assert!(
        fake.attachment_sends.lock().unwrap().is_empty(),
        "nothing goes into the reply still stopping"
    );
    let notice = crate::projects::notice(state, &scene.fx.chat).unwrap();
    assert!(notice.contains("didn't stop in time"), "{notice}");
    assert!(!scene.stopped());

    // It waits as Send does (past the backoff) while the reply runs, and
    // goes once it ended.
    state.t3_code.clear_failures_for_test();
    assert_eq!(scene.dispatch(&id).await.state, "accepted_by_wonder");
    assert!(fake.attachment_sends.lock().unwrap().is_empty());
    scene.replying(false);
    fake.queue_sends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(fake.interrupts.lock().unwrap().len(), 1, "stopped once");
    assert_eq!(fake.attachment_sends.lock().unwrap().len(), 1);
    assert!(!scene.stopped());
}

// Contract (owner, Oct 20): New thread in T3 Code starts a thread there, in
// the T3 project for the Project's folder, with its first message after
// Cupola's preamble. A folder
// T3 Code doesn't have, or T3 Code not connected, says what to do instead.
#[tokio::test]
async fn new_thread_in_t3_code_launches_in_the_projects_folder() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    let paths =
        ["project", "missing"].map(|project| format!("/api/v1/projects/{project}/t3-code/threads"));
    let launch = |project: &'static str| {
        crate::test_support::call(
            state,
            "POST",
            &paths[usize::from(project == "missing")],
            serde_json::json!({"title": "Release notes", "message": "Draft them"}),
        )
    };
    let (status, launched) = launch("project").await;
    assert_eq!(status, StatusCode::CREATED, "{launched}");
    assert_eq!(launched["threadId"], "t3-launched-1");
    let launches = fake.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0]["projectId"], "p");
    assert_eq!(launches[0]["title"], "Release notes");
    // T3 Code has no system prompt: Cupola's preamble opens the first
    // message as a briefing, which Cupola's views leave out.
    let first = crate::preamble::t3_first_message("Draft them");
    assert_eq!(launches[0]["message"], first.as_str());
    assert!(first.contains("phone"), "{first}");
    assert_eq!(
        crate::provider_switch::without_handoff(&first).trim_start(),
        "Draft them"
    );
    assert_eq!(fake.texts("t3-launched-1"), [first.as_str()]);
    assert!(
        launches[0].get("modelSelection").is_none(),
        "T3 Code's project default runs it: {:?}",
        launches[0]
    );
    assert!(
        launches[0].get("interactionMode").is_none(),
        "{:?}",
        launches[0]
    );
    // Plan mode starts it in T3 Code's plan mode.
    let (status, launched) = crate::test_support::call(
        state,
        "POST",
        &paths[0],
        serde_json::json!({"title": "Plan it", "message": "Plan the release", "planMode": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{launched}");
    assert_eq!(fake.launches.lock().unwrap()[1]["interactionMode"], "plan");
    let (_, status_body) =
        crate::test_support::call(state, "GET", "/api/v1/t3-code", serde_json::Value::Null).await;
    assert_eq!(
        status_body["launches"], true,
        "the phone offers it only on a Mac that launches"
    );

    fake.projects.lock().unwrap().clear();
    let (status, refused) = launch("project").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("Add it in T3 Code"),
        "{refused}"
    );
    let (status, _) = launch("missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    fake.revoked
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, refused) = launch("project").await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(
        fake.launches.lock().unwrap().len(),
        2,
        "nothing else launched"
    );
}

// Contract (owner, Nov 6): a chat with no Project starts in T3 Code like any
// other, in T3 Code's own scratch folder: the launch asks for `scratch`, names
// no project (T3 Code refuses both together), and the conversation works in
// the folder T3 Code made.
#[tokio::test]
async fn a_no_project_chat_launches_in_t3_codes_scratch_folder() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    let scratch = tempfile::tempdir().unwrap();
    let roots =
        crate::projects::validate_folders(&[scratch.path().to_string_lossy().into_owned()], &[])
            .unwrap();
    state
        .store
        .create_project(
            crate::projects::SCRATCH_PROJECT_ID,
            "scratch-request",
            "hash",
            "No project",
            &roots,
            0,
            "now",
        )
        .await
        .unwrap();
    let (status, launched) = crate::test_support::call(
        state,
        "POST",
        "/api/v1/projects/scratch/t3-code/threads",
        serde_json::json!({"title": "Quick question", "message": "What is a monad?"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{launched}");
    assert_eq!(launched["threadId"], "t3-launched-1");
    let launches = fake.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0]["scratch"], true, "{:?}", launches[0]);
    assert!(
        launches[0].get("projectId").is_none(),
        "T3 Code refuses scratch with a project: {:?}",
        launches[0]
    );
    // The Cupola scratch folder need not be a project in T3 Code.
    assert_eq!(launches[0]["title"], "Quick question");
}

// Contract (live, Oct 21): T3 Code refuses a launch with no model when its
// project has no default model. The launch then runs on a provider T3 Code
// has (the requested one, else the Project's default, else Codex) instead of
// showing that refusal; a requested provider and model T3 Code lists run as
// asked. No other refusal is launched again.
#[tokio::test]
async fn new_thread_in_t3_code_picks_a_model_t3_code_has() {
    use std::sync::atomic::Ordering::SeqCst;
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    let state = &scene.fx.state;
    let launch = |body: serde_json::Value| {
        let mut body = body;
        body["title"] = "Release notes".into();
        crate::test_support::call(
            state,
            "POST",
            "/api/v1/projects/project/t3-code/threads",
            body,
        )
    };
    let selections = || {
        fake.launches
            .lock()
            .unwrap()
            .iter()
            .map(|launch| launch.get("modelSelection").cloned())
            .collect::<Vec<_>>()
    };
    let selection = |instance: &str, model: &str| {
        Some(serde_json::json!({"instanceId": instance, "model": model}))
    };

    // A requested provider and model T3 Code lists run as asked, even where
    // T3 Code's project has a default.
    let request = serde_json::json!({"provider": "codex", "model": "gpt-6.1-mini"});
    let mut contract = request.clone();
    contract["title"] = "Release notes".into();
    crate::test_support::assert_http_contract("launchInT3CodeRequest", &contract);
    let (status, body) = launch(request).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(selections(), [selection("codex", "gpt-6.1-mini")]);

    // Picker catalog is read from the connected T3 instance. Launch consumes
    // those exact advertised options; stale options refuse before creation.
    // A slow T3 must not hold ordinary status reads behind that catalog.
    fake.hanging.send_replace(true);
    let lookups = fake.lookups.load(SeqCst);
    let (status, connection) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::test_support::call(state, "GET", "/api/v1/t3-code", Value::Null),
    )
    .await
    .expect("connection status must not call T3's catalog tool");
    assert_eq!(status, StatusCode::OK, "{connection}");
    assert_eq!(connection["state"], "connected");
    assert!(connection.get("launchCatalog").is_none());
    assert_eq!(fake.lookups.load(SeqCst), lookups);
    fake.hanging.send_replace(false);
    let (status, catalog) =
        crate::test_support::call(state, "GET", "/api/v1/t3-code?catalog=true", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    crate::test_support::assert_http_contract("t3CodeStatus", &catalog);
    let providers = catalog["launchCatalog"]["providers"].as_array().unwrap();
    assert_eq!(
        providers.len(),
        3,
        "providers T3 Code runs that Cupola follows"
    );
    let codex = providers.iter().find(|p| p["family"] == "codex").unwrap();
    assert_eq!(
        codex["runtimeModes"],
        serde_json::json!(["approval-required", "full-access"])
    );
    assert_eq!(codex["models"][0]["id"], "t3:codex:gpt-6.1");
    assert_eq!(codex["models"][0]["serviceTiers"][1]["id"], "fast");
    let (status, searched) = crate::test_support::call(
        state,
        "GET",
        "/api/v1/t3-code?search=gpt-6.1-pro",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{searched}");
    crate::test_support::assert_http_contract("t3CodeStatus", &searched);
    assert_eq!(
        searched["launchCatalog"]["providers"][0]["models"][0]["id"],
        "t3:codex:gpt-6.1-pro"
    );
    fake.launches.lock().unwrap().clear();
    let choices = serde_json::json!({"provider": "codex", "providerInstanceId": "codex", "model": "gpt-6.1-mini",
        "effort": "high", "serviceTier": "fast", "runtimeMode": "approval-required"});
    let mut contract = choices.clone();
    contract["title"] = "Release notes".into();
    crate::test_support::assert_http_contract("launchInT3CodeRequest", &contract);
    let (status, body) = launch(choices.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let launched = fake.launches.lock().unwrap()[0].clone();
    assert_eq!(launched["runtimeMode"], "approval-required");
    assert_eq!(
        launched["modelSelection"]["options"],
        serde_json::json!([
        {"id": "reasoningEffort", "value": "high"}, {"id": "serviceTier", "value": "fast"}])
    );
    for (field, value) in [
        ("effort", "ultra"),
        ("serviceTier", "turbo"),
        ("runtimeMode", "auto"),
        ("providerInstanceId", "removed"),
    ] {
        let mut invalid = choices.clone();
        invalid[field] = value.into();
        let (status, body) = launch(invalid).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {body}");
        assert_eq!(
            fake.launches.lock().unwrap().len(),
            1,
            "refused before launch"
        );
    }
    let (status, body) = launch(
        serde_json::json!({"provider": "claude", "providerInstanceId": "claudeAgent",
        "model": "claude-opus-5-5", "serviceTier": "turbo", "runtimeMode": "approval-required"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        fake.launches.lock().unwrap().len(),
        1,
        "invalid boolean speed never becomes Standard"
    );
    let mut empty_model = choices.clone();
    empty_model["model"] = " ".into();
    let (status, body) = launch(empty_model).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        fake.launches.lock().unwrap().len(),
        1,
        "empty choice never launches a default"
    );

    fake.no_default_model.store(true, SeqCst);
    fake.launches.lock().unwrap().clear();
    // No request and no Project default: Codex's first model, after the
    // refusal (T3 Code created nothing for it).
    let (status, body) = launch(serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(selections(), [None, selection("codex", "gpt-6.1")]);

    // The Project's last provider and the model last chosen for it there.
    state
        .store
        .touch_project("project", AgentFamily::Claude, "later")
        .await
        .unwrap();
    state
        .store
        .record_project_model("project", AgentFamily::Claude, "claude-sonnet-5-5", None)
        .await
        .unwrap();
    fake.launches.lock().unwrap().clear();
    let (status, body) = launch(serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        selections(),
        [None, selection("claudeAgent", "claude-sonnet-5-5")]
    );

    // Contract (owner, Oct 23): T3 Code runs exactly the chosen provider and
    // model, or nothing starts and the reason says why; never the T3
    // project's default (a DeepSeek chat started on Claude).
    fake.launches.lock().unwrap().clear();
    let (status, body) =
        launch(serde_json::json!({"provider": "codex", "model": "gpt-6.1-mini"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(selections(), [selection("codex", "gpt-6.1-mini")]);
    // A model T3 Code lists only when asked for it by name.
    fake.launches.lock().unwrap().clear();
    let (status, body) =
        launch(serde_json::json!({"provider": "codex", "model": "gpt-6.1-pro"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(selections(), [selection("codex", "gpt-6.1-pro")]);
    // Cupola's namespaced id ("claude:…") and a model's provider ids.
    let template = state.runtime_catalog.read().await.models[0].clone();
    state
        .runtime_catalog
        .write()
        .await
        .models
        .push(crate::ModelOption {
            agent_family: AgentFamily::Claude,
            id: "claude:opus".into(),
            display_name: "Opus".into(),
            native_ids: vec!["claude-opus-5-5".into()],
            ..template
        });
    fake.launches.lock().unwrap().clear();
    let (status, body) =
        launch(serde_json::json!({"provider": "claude", "model": "claude:opus"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(selections(), [selection("claudeAgent", "claude-opus-5-5")]);
    fake.launches.lock().unwrap().clear();
    let (status, body) =
        launch(serde_json::json!({"provider": "claude", "model": "claude:claude-sonnet-5-5"}))
            .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        selections(),
        [selection("claudeAgent", "claude-sonnet-5-5")]
    );

    // A model T3 Code doesn't offer: refused by name, nothing launched.
    fake.launches.lock().unwrap().clear();
    let (status, body) = launch(serde_json::json!({"provider": "codex", "model": "gpt-0"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let refused = body["error"].as_str().unwrap();
    assert!(
        refused.contains("doesn't offer gpt-0 for Codex"),
        "{refused}"
    );
    assert_eq!(selections(), [], "nothing launched");

    // A provider T3 Code has turned off: refused, never another provider.
    fake.claude_disabled.store(true, SeqCst);
    let (status, body) =
        launch(serde_json::json!({"provider": "claude", "model": "claude:opus"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let refused = body["error"].as_str().unwrap();
    assert!(refused.contains("doesn't run Claude"), "{refused}");
    assert_eq!(selections(), [], "nothing launched");
    fake.claude_disabled.store(false, SeqCst);

    // OpenCode (the DeepSeek chat) runs in T3 Code and Cupola follows its
    // timeline (`t3_code::feed`); Cursor, which this T3 Code doesn't run, is
    // refused with that reason.
    let (status, body) = launch(serde_json::json!({"provider": "opencode",
        "model": "opencode:opencode/deepseek-v4.1-flash-free", "message": "Start",
        "deviceId": "owner", "clientMessageId": uuid::Uuid::new_v4().to_string()}))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A follow-up in that chat goes into its T3 thread (no native session
    // exists, so Cupola can't run it), once the chat is bound to it.
    let conversation = body["conversationId"].as_str().unwrap().to_owned();
    let bound = state
        .store
        .t3_native_thread(&conversation)
        .await
        .unwrap()
        .expect("runtime binding is durable before launch returns");
    let sent_before = fake.sends.lock().unwrap().len();
    let response = crate::send_message_inner(
        axum::extract::State(state.clone()),
        axum::extract::Path(conversation.clone()),
        None,
        axum::Json(crate::SendMessageRequest {
            codex_skills: vec![],
            take_over: false,
            project_model: None,
            device_id: "owner".into(),
            client_message_id: "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee".into(),
            body: "And again".into(),
            attachment_ids: vec![],
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let receipt: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap(),
    )
    .unwrap();
    let message = state
        .store
        .message_by_id(receipt["wonderMessageId"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    crate::projects::dispatch(state.clone(), message).await;
    let sends = fake.sends.lock().unwrap().clone();
    assert_eq!(sends.len(), sent_before + 1, "{sends:?}");
    assert_eq!(sends.last().unwrap()["threadId"], bound.as_str());
    assert_eq!(sends.last().unwrap()["message"], "And again");
    fake.launches.lock().unwrap().clear();
    let (status, body) = launch(serde_json::json!({"provider": "cursor", "model": null})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("doesn't run Cursor"));
    assert_eq!(selections(), [], "nothing launched");
    let lookups = fake.lookups.load(SeqCst);
    let (_, status_body) = crate::test_support::call(
        state,
        "GET",
        "/api/v1/t3-code?refresh=true",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        status_body["launchProviders"],
        serde_json::json!(["codex", "claude", "opencode", "cursor"])
    );
    assert!(status_body.get("launchCatalog").is_none());
    assert_eq!(
        fake.lookups.load(SeqCst),
        lookups + 1,
        "one bounded reachability probe, no second catalog read"
    );
    assert_eq!(
        selections(),
        [],
        "checking reachability never launches a thread"
    );

    // No provider in T3 Code: says what to do, without T3 Code's wording.
    fake.no_providers.store(true, SeqCst);
    fake.launches.lock().unwrap().clear();
    let (status, body) = launch(serde_json::json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let refused = body["error"].as_str().unwrap();
    assert!(refused.contains("no provider set up"), "{refused}");
    assert!(!refused.contains("modelSelection"), "{refused}");
    assert_eq!(selections(), [None], "launched once");

    // Any other refusal is reported once.
    fake.no_providers.store(false, SeqCst);
    *fake.refuse_launch.lock().unwrap() = Some("The worktree base was not found.".into());
    fake.launches.lock().unwrap().clear();
    let (status, body) = launch(serde_json::json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("worktree base"),
        "{body}"
    );
    assert_eq!(fake.launches.lock().unwrap().len(), 1, "not launched again");
}

// Contract (owner, Oct 23): New chat's Start in T3 Code opens the thread's
// conversation at once, with the typed message in it and "Starting in T3
// Code…"; asking again answers that conversation without a second launch;
// and once T3 Code's store names the agent's session, that session joins
// the same conversation (its ID unchanged) rather than showing as another.
// Contract: T3-only runtime bindings can never be resumed by a host provider,
// including after sign-out, an offline lookup or removal of T3 ownership.
#[tokio::test]
async fn t3_native_followups_stay_in_t3_online_and_offline() {
    async fn dispatch(state: &crate::AppState, message: &StoredMessage) {
        crate::projects::dispatch(state.clone(), message.clone()).await;
        state
            .store
            .recover_direct_dispatch_claims_except(&state.projects.dispatching())
            .await
            .unwrap();
    }
    async fn followup(
        state: &crate::AppState,
        chat: &str,
        attachments: Vec<String>,
    ) -> (StoredMessage, Value) {
        let response = crate::send_message_inner(
            axum::extract::State(state.clone()),
            axum::extract::Path(chat.to_owned()),
            None,
            axum::Json(crate::SendMessageRequest {
                codex_skills: vec![],
                take_over: false,
                project_model: None,
                device_id: "owner".into(),
                client_message_id: uuid::Uuid::new_v4().to_string(),
                body: "Continue in T3".into(),
                attachment_ids: attachments,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let receipt: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap();
        let message = state
            .store
            .message_by_id(receipt["wonderMessageId"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        (message, receipt)
    }
    for (family, model) in [
        ("opencode", "opencode/deepseek-v4.1-flash-free"),
        ("cursor", "grok-4.7"),
    ] {
        let fake = crate::t3_code::tests::Fake::start().await;
        fake.cursor_enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut fx = fx().await;
        let root = fx
            .state
            .store
            .project("project")
            .await
            .unwrap()
            .unwrap()
            .primary_root()
            .path
            .clone();
        fake.project("p", &root);
        let (t3, _) = crate::t3_code::tests::connected(&fake).await;
        fx.state.t3_code = t3.clone();
        let (status, launch) = crate::test_support::call(&fx.state, "POST",
            "/api/v1/projects/project/t3-code/threads", serde_json::json!({
                "title": "T3 native fixture", "provider": family, "model": model,
                "message": "Run ls plans | head -3, then cat /definitely-missing-file.txt; reply DONE.", "deviceId": "owner", "clientMessageId": uuid::Uuid::new_v4().to_string()
            })).await;
        assert_eq!(status, StatusCode::CREATED, "{family}: {launch}");
        let chat = launch["conversationId"].as_str().unwrap();
        let thread = fx
            .state
            .store
            .t3_native_thread(chat)
            .await
            .unwrap()
            .expect("binding is durable before launch returns");
        assert!(fx
            .state
            .store
            .project_conversation(chat)
            .await
            .unwrap()
            .unwrap()
            .native_session_id
            .is_none());
        let recorded: Value =
            serde_json::from_str(include_str!("../tests/fixtures/t3-native-activity.json"))
                .unwrap();
        fake.activity_items
            .lock()
            .unwrap()
            .insert(thread.clone(), recorded[family].as_array().unwrap().clone());
        start_history_refresh(&fx.state, chat).await;
        // Refresh is asynchronous; wait on its durable status, then inspect the actual snapshot.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (_, snapshot) = crate::test_support::call(
                    &fx.state,
                    "GET",
                    &format!("/api/v1/conversations/{chat}"),
                    serde_json::json!({}),
                )
                .await;
                let items = snapshot["thread"]["turns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|turn| turn["items"].as_array().unwrap())
                    .collect::<Vec<_>>();
                let replies = items
                    .iter()
                    .filter(|item| item["type"] == "agentMessage")
                    .collect::<Vec<_>>();
                let ready = replies.iter().any(|item| {
                    item["text"].as_str().is_some_and(|text| {
                        if family == "opencode" {
                            text.contains("maximum number of items is 128")
                        } else {
                            text.ends_with("DONE")
                        }
                    })
                });
                if ready {
                    if family == "cursor" {
                        let positions: Vec<_> = items
                            .iter()
                            .filter_map(|item| item["payload"]["t3Position"].as_i64())
                            .collect();
                        assert_eq!(
                            positions,
                            [0, 1, 2, 3, 4, 5, 6],
                            "T3 order survives history commit and snapshot projection"
                        );

                        assert_eq!(
                            replies.len(),
                            2,
                            "commentary and final reply, with adjacent final chunks merged"
                        );
                        assert!(replies.last().unwrap()["text"]
                            .as_str()
                            .unwrap()
                            .ends_with("DONE"));
                    } else {
                        assert_eq!(replies.len(), 1);
                        assert_eq!(replies[0]["state"], "failed");
                    }
                    if let Ok(directory) = std::env::var("WONDER_T3_PROVIDER_SNAPSHOT_DIR") {
                        std::fs::create_dir_all(&directory).unwrap();
                        std::fs::write(
                            std::path::Path::new(&directory).join(format!("{family}.json")),
                            serde_json::to_vec_pretty(&snapshot).unwrap(),
                        )
                        .unwrap();
                    }
                    break;
                }
                // A refresh already running (started before the timeline was
                // recorded) absorbs this one; ask again until one reads it.
                let _ = crate::test_support::call(
                    &fx.state,
                    "POST",
                    &format!("/api/v1/conversations/{chat}/history/refresh"),
                    serde_json::json!({}),
                )
                .await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("T3 timeline committed to the chat snapshot");
        let (status, activity) = crate::test_support::call(
            &fx.state,
            "GET",
            &format!("/api/v1/conversations/{chat}/history/activity"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{activity}");
        crate::tests::validate_http_contract("historyActivity", &activity);
        assert_eq!(
            activity["latestTurnStatus"],
            if family == "opencode" {
                "failed"
            } else {
                "completed"
            }
        );
        fake.queue_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (status, running) = crate::test_support::call(
            &fx.state,
            "GET",
            &format!("/api/v1/conversations/{chat}/history/activity"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(running["latestTurnStatus"], "inProgress");
        assert_eq!(running["runningElsewhere"], true);
        fake.activity_items.lock().unwrap().get_mut(&thread).unwrap().push(
            serde_json::json!({"position":9,"type":"user_message","itemId":"newer-user","text":"A newer turn"}));
        let stale = crate::projects::interrupt_through_t3_code(&fx.state, chat, "t3-turn-0")
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        assert!(fake.interrupts.lock().unwrap().is_empty());
        fake.activity_items
            .lock()
            .unwrap()
            .get_mut(&thread)
            .unwrap()
            .pop();
        let response = crate::projects::interrupt_through_t3_code(&fx.state, chat, "t3-turn-0")
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            fake.interrupts.lock().unwrap().last().unwrap()["threadId"],
            thread
        );
        fake.queue_sends
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (message, receipt) = followup(&fx.state, chat, vec![]).await;
        assert_eq!(receipt["queued"], false, "{family}: {receipt}");
        dispatch(&fx.state, &message).await;
        assert_eq!(
            fake.sends.lock().unwrap().last().unwrap()["threadId"],
            thread
        );
        assert_eq!(
            fx.state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        // A plain send with files waits for T3's active run; it must never
        // steer the run implicitly or fall through to the synthetic runtime.
        let upload = crate::upload_conversation_file(
            axum::extract::State(fx.state.clone()),
            axum::extract::Path(chat.to_owned()),
            axum::Json(crate::CreateConversationFileRequest {
                client_upload_id: Some(uuid::Uuid::new_v4().to_string()),
                name: "notes.txt".into(),
                mime_type: Some("text/plain".into()),
                content_base64: "aGVsbG8=".into(),
            }),
        )
        .await;
        assert_eq!(upload.status(), StatusCode::OK);
        let uploaded: Value = serde_json::from_slice(
            &axum::body::to_bytes(upload.into_body(), 100_000)
                .await
                .unwrap(),
        )
        .unwrap();
        fake.queue_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (with_file, _) = followup(
            &fx.state,
            chat,
            vec![uploaded["id"].as_str().unwrap().to_owned()],
        )
        .await;
        dispatch(&fx.state, &with_file).await;
        assert!(fake.attachment_sends.lock().unwrap().is_empty());
        assert_eq!(
            fx.state
                .store
                .message_by_id(&with_file.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder"
        );
        fake.queue_sends
            .store(false, std::sync::atomic::Ordering::SeqCst);
        dispatch(&fx.state, &with_file).await;
        assert_eq!(fake.attachment_sends.lock().unwrap().len(), 1);
        assert_eq!(
            fx.state
                .store
                .message_by_id(&with_file.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "completed"
        );
        // Lost ownership must not turn the synthetic runtime into a host session.
        fx.state.store.forget_t3_code_thread(chat).await.unwrap();
        let (waiting, receipt) = followup(&fx.state, chat, vec![]).await;
        assert_eq!(receipt["queued"], true, "forgotten {family}: {receipt}");
        crate::projects::request_take_over(&fx.state, chat, &waiting.id);
        dispatch(&fx.state, &waiting).await;
        assert_eq!(
            fx.state
                .store
                .message_by_id(&waiting.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder"
        );
        fx.state
            .store
            .set_t3_code_thread(chat, &format!("t3:{thread}"), &thread, "now")
            .await
            .unwrap();
        // Sign-out and a known-offline T3 both hold the same pending message.
        fx.state.t3_code = std::sync::Arc::new(crate::t3_code::T3Code::in_memory(
            None,
            std::sync::Arc::new(|_| Box::pin(std::future::ready(Ok(())))),
        ));
        assert!(
            crate::projects::send_waits(&fx.state, chat, Some(&waiting)).await,
            "signed out {family}"
        );
        let offline_stop = crate::interrupt_turn(
            axum::extract::State(fx.state.clone()),
            axum::extract::Path((chat.to_owned(), "t3-turn-0".into())),
        )
        .await;
        assert_eq!(offline_stop.status(), StatusCode::SERVICE_UNAVAILABLE);
        dispatch(&fx.state, &waiting).await;
        assert_eq!(
            fx.state
                .store
                .message_by_id(&waiting.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder"
        );
        fx.state.t3_code = t3;
        fake.stop();
        assert!(fx.state.t3_code.active_run(&thread).await.is_err());
        assert!(
            crate::projects::send_waits(&fx.state, chat, Some(&waiting)).await,
            "offline {family}"
        );
        dispatch(&fx.state, &waiting).await;
        assert_eq!(
            fx.state
                .store
                .message_by_id(&waiting.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_wonder"
        );
        assert_eq!(
            fake.sends.lock().unwrap().len(),
            1,
            "only the online follow-up sent"
        );
    }
}

#[tokio::test]
async fn a_t3_code_launch_opens_its_conversation_and_its_session_joins_it() {
    let (dir, mut state, _) = crate::projects::tests::handoff_fixture().await;
    let entrypoint = dir.path().join("claude-sessions.py");
    std::fs::write(&entrypoint, r#"import json, sys
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'thread/list': result = {'data': [], 'nextCursor': None}
    elif m == 'project/sessions/list' and p.get('sessionId'): result = {'data': [{'sessionId': p['sessionId'], 'cwd': p['dirs'][0], 'title': 'From T3 Code', 'updatedAt': 1}], 'nextCursor': None, 'partial': False}
    elif m == 'project/sessions/list': result = {'data': [], 'nextCursor': None, 'partial': False}
    elif m == 'project/session/attach': result = {'thread': {'id': 'claude-' + p['sessionId']}}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#).unwrap();
    let config = wonder_app_server::BridgeLaunchConfig {
        node_bin: "/usr/bin/python3".into(),
        entrypoint,
        state_dir: dir.path().join("claude-state"),
        npm_cli: None,
        claude_config_dir: None,
        wonder_version: "test".into(),
        family: "claude",
    };
    let client = std::sync::Arc::new(tokio::sync::Mutex::new(
        wonder_app_server::AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        ),
    ));
    client
        .lock()
        .await
        .restart_bridge(config.clone())
        .await
        .unwrap();
    state.claude = Some(std::sync::Arc::new(crate::claude::Runtime {
        client,
        config,
    }));
    {
        let mut catalog = state.runtime_catalog.write().await;
        let mut opus =
            crate::agent_defaults::tests::model("claude:opus", AgentFamily::Claude, &["low"], None);
        opus.native_ids = vec!["claude-opus-5-5".into()];
        catalog.models.push(opus);
        catalog.resolve_defaults();
    }
    let root = state
        .store
        .project("project")
        .await
        .unwrap()
        .unwrap()
        .primary_root()
        .path
        .clone();
    let fake = crate::t3_code::tests::Fake::start().await;
    fake.project("p", &root);
    let (t3, _) = crate::t3_code::tests::connected(&fake).await;
    state.t3_code = t3;

    let request_id = uuid::Uuid::new_v4().to_string();
    let request = serde_json::json!({"title": "Release notes", "message": "Draft them",
        "provider": "claude", "model": "claude:opus",
        "providerInstanceId": "claudeAgent", "effort": "high", "serviceTier": "fast", "runtimeMode": "approval-required",
        "deviceId": "owner", "clientMessageId": request_id});
    crate::test_support::assert_http_contract("launchInT3CodeRequest", &request);
    let path = "/api/v1/projects/project/t3-code/threads";
    let (status, launched) = crate::test_support::call(&state, "POST", path, request.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{launched}");
    crate::tests::validate_http_contract("launchInT3CodeResponse", &launched);
    let id = launched["conversationId"].as_str().unwrap().to_owned();
    assert_eq!(launched["threadId"], "t3-launched-1");
    assert_eq!(launched["conversation"]["conversationId"], id.as_str());
    assert_eq!(launched["conversation"]["family"], "claude");
    let stored = state
        .store
        .project_conversation(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.effort.as_deref(),
        Some("high"),
        "T3's choice wins over native low-only catalog"
    );
    assert_eq!(stored.service_tier.as_deref(), Some("fast"));
    assert_eq!(stored.claude_approval, "ask");
    // The conversation's detail says so until the session joins.
    let starting = crate::projects::notice(&state, &id).unwrap_or_default();
    assert!(starting.contains("Starting in T3 Code"), "{starting}");
    let launches = fake.launches.lock().unwrap().clone();
    assert_eq!(
        launches[0]["modelSelection"],
        serde_json::json!({"instanceId": "claudeAgent", "model": "claude-opus-5-5", "options": [
            {"id": "effort", "value": "high"}, {"id": "fastMode", "value": true}]})
    );
    // The typed message, as delivered to T3 Code, never sent by Cupola.
    let message = state
        .store
        .message_by_device_and_client_message_id("owner", &request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.conversation_id, id);
    assert_eq!(message.state, "completed");

    // Asked again (a lost answer, Send again): the same conversation, and
    // T3 Code isn't asked to start another thread.
    let (status, again) = crate::test_support::call(&state, "POST", path, request).await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_eq!(again["conversationId"], id.as_str());
    assert_eq!(fake.launches.lock().unwrap().len(), 1, "launched once");

    // T3 Code's agent started: its session joins this conversation.
    let session = "00000000-0000-4000-8000-00000000a001";
    fake.keeps("t3-launched-1", "claudeAgent", session).await;
    // The join and the T3 thread's link are two writes
    // (`bind_t3_conversation`): wait for the link, written last.
    let mut linked = None;
    for _ in 0..200 {
        linked = state.store.t3_code_thread(&id, session).await.unwrap();
        if linked.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        linked.as_deref(),
        Some("t3-launched-1"),
        "the session joined"
    );
    let joined = state
        .store
        .project_conversation(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(joined.native_session_id.as_deref(), Some(session));
    assert_eq!(
        joined.title, "Release notes",
        "its own title, not a new row's"
    );
    for _ in 0..200 {
        if crate::projects::notice(&state, &id).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(crate::projects::notice(&state, &id), None, "started");
    assert_eq!(
        state
            .store
            .t3_code_conversation("t3-launched-1")
            .await
            .unwrap()
            .as_deref(),
        Some(id.as_str()),
        "one conversation is that T3 thread"
    );
    // The agent's record of the first message (Cupola's briefing, then the
    // typed words) shows as those words, which pair it with the message
    // saved here: one bubble (`t3_code::echoes`).
    let record = crate::preamble::t3_first_message("Draft them");
    assert_eq!(
        crate::history::shown_text(
            "turn",
            serde_json::json!({"type": "userMessage", "content": [{"type": "text", "text": record}]})
        )
        .as_deref(),
        Some("Draft them")
    );
    let sends = state.store.t3_code_sent_messages(&id).await.unwrap();
    assert_eq!(sends.len(), 1);
    let intent: Value = serde_json::from_str(&sends[0].intent).unwrap();
    assert_eq!(intent["t3ThreadId"], "t3-launched-1");
    assert_eq!(intent["t3Started"], true);
    state.projects.shutdown().await;
}

// Contract: a T3 launch whose first message is a native command goes without
// Cupola's briefing, and exactly the first later message that is not a
// command carries it (a retry of that message sends the same text).
#[tokio::test]
async fn a_command_launch_in_t3_code_defers_the_briefing_to_the_next_message() {
    let (_dir, mut state, _) = crate::projects::tests::handoff_fixture().await;
    {
        let mut catalog = state.runtime_catalog.write().await;
        let mut opus =
            crate::agent_defaults::tests::model("claude:opus", AgentFamily::Claude, &["low"], None);
        opus.native_ids = vec!["claude-opus-5-5".into()];
        catalog.models.push(opus);
        catalog.resolve_defaults();
    }
    let root = state
        .store
        .project("project")
        .await
        .unwrap()
        .unwrap()
        .primary_root()
        .path
        .clone();
    let fake = crate::t3_code::tests::Fake::start().await;
    fake.project("p", &root);
    state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
    let request = serde_json::json!({"title": "Compact", "message": "/compact",
        "provider": "claude", "model": "claude:opus", "providerInstanceId": "claudeAgent", "runtimeMode": "approval-required",
        "deviceId": "owner", "clientMessageId": uuid::Uuid::new_v4().to_string()});
    let path = "/api/v1/projects/project/t3-code/threads";
    let (status, launched) = crate::test_support::call(&state, "POST", path, request).await;
    assert_eq!(status, StatusCode::CREATED, "{launched}");
    let id = launched["conversationId"].as_str().unwrap().to_owned();
    assert_eq!(fake.launches.lock().unwrap()[0]["message"], "/compact");

    // Follow-ups saved the way the launch is: only their intents matter here.
    let follow = |client: &str, flag: Option<&str>| {
        let state = state.clone();
        let (id, client) = (id.clone(), client.to_owned());
        let flag = flag.map(str::to_owned);
        async move {
            let mut intent = serde_json::json!({"deliveryMethod": "t3_thread_send"});
            if let Some(flag) = flag {
                intent[flag] = true.into();
            }
            let message = state
                .store
                .record_t3_code_launch_message(
                    "owner",
                    &client,
                    "next",
                    "sha",
                    &id,
                    "t3-launched-1",
                    &intent,
                    "now",
                )
                .await
                .unwrap();
            state.store.message_by_id(&message).await.unwrap().unwrap()
        }
    };
    let first = follow("first", None).await;
    assert!(crate::projects::t3_briefing_owed(&state, &first).await);
    let second = follow("second", Some("t3Briefed")).await;
    assert!(!crate::projects::t3_briefing_owed(&state, &first).await);
    assert!(
        crate::projects::t3_briefing_owed(&state, &second).await,
        "the message that carried it still carries it on a retry"
    );
    state.projects.shutdown().await;
}

// Contract (owner, Oct 21): T3 Code's tools belong to threads T3 Code runs,
// which T3 Code gives them itself. A thread Cupola runs (Codex or Claude, in
// every access mode, in plan mode or not) is loaded with no
// `mcp_servers.t3-code`, and Cupola's sign-in to T3 Code never reaches it,
// even while Cupola is connected to T3 Code.
#[tokio::test]
async fn cupola_run_agents_get_no_t3_code_mcp_in_any_mode() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let mut fx = crate::provider_switch_tests::fx().await;
    fx.state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
    assert!(fx.state.t3_code.connected());
    let modes = [
        ("full_access", false),
        ("full_access", true),
        ("workspace", false),
        ("read_only", false),
    ];
    let set_mode = |access: &'static str, plan: bool| {
        let fx = &fx;
        async move {
            fx.state
                .store
                .update_project_conversation(
                    &fx.chat,
                    wonder_store::ProjectConversationPatch {
                        access_mode: Some(access),
                        plan_mode: Some(plan),
                        ..Default::default()
                    },
                    "now",
                )
                .await
                .unwrap();
        }
    };
    // The chat starts on Codex, then moves to Claude.
    for switch in [None, Some((AgentFamily::Claude, "claude:sonnet"))] {
        for (index, (access, plan)) in modes.into_iter().enumerate() {
            let with = switch.filter(|_| index == 0);
            let message = fx.queue(&format!("{access} plan={plan}"), with).await;
            set_mode(access, plan).await;
            fx.release(&message).await.unwrap();
            // Ends the reply: one left running stalled tests running alongside.
            fx.finish(&message).await;
        }
    }
    let loads = [
        fx.codex_requests("thread/start"),
        fx.codex_requests("thread/resume"),
        fx.claude_requests("turn/start"),
    ]
    .concat();
    assert!(
        fx.codex_requests("thread/start").len() + fx.codex_requests("thread/resume").len() > 0
            && fx.claude_requests("turn/start").len() == modes.len(),
        "{loads:?}"
    );
    let tokens = fake.tokens.lock().unwrap().clone();
    assert!(!tokens.is_empty());
    for load in &loads {
        assert_eq!(
            load["params"]["config"]["mcp_servers.t3-code"],
            Value::Null,
            "{load}"
        );
        let text = load.to_string();
        assert!(
            tokens.iter().all(|token| !text.contains(token.as_str())),
            "Cupola's T3 Code sign-in reached an agent: {load}"
        );
    }
}

/// Asks for a history refresh of `chat`, waiting for a free refresh slot:
/// the slots are shared by every test running at once, and a busy one
/// answers 429 until another refresh ends.
async fn start_history_refresh(state: &crate::AppState, chat: &str) {
    let path = format!("/api/v1/conversations/{chat}/history/refresh");
    // A hang guard only (about 60 s): other tests may hold both refresh slots.
    for _ in 0..3000 {
        let (status, body) = crate::test_support::call(state, "POST", &path, Value::Null).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            continue;
        }
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        return;
    }
    panic!("no history refresh slot");
}

/// Reads the thread's native history again, as the phone asks when the
/// Mac's history moved, and waits for it to finish.
async fn refresh_history(fx: &Fx) {
    let path = format!("/api/v1/conversations/{}/history/refresh", fx.chat);
    start_history_refresh(&fx.state, &fx.chat).await;
    for _ in 0..3000 {
        let (_, refresh) = crate::test_support::call(&fx.state, "GET", &path, Value::Null).await;
        match refresh["state"].as_str() {
            Some("completed") => return,
            Some("failed") => panic!("history refresh failed: {refresh}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    panic!("history refresh never finished");
}

/// The history the phone reads, and its user bubbles starting with `text`,
/// by the turns each shows in, as the phone draws them (`ReadCache.rows`):
/// an item is the bubble of the message whose client ID it carries,
/// otherwise one of its own; a message no item carries is a bubble of its
/// own.
async fn user_bubbles(
    fx: &Fx,
    text: &str,
) -> (
    Value,
    std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) {
    let (status, history) = crate::test_support::call(
        &fx.state,
        "GET",
        &format!("/api/v1/conversations/{}/history", fx.chat),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    crate::test_support::assert_http_contract("conversationSnapshot", &history);
    let bubbles = bubbles_in(&history, text);
    (history, bubbles)
}

/// The user bubbles starting with `text` in a history the phone read, as
/// `user_bubbles`.
fn bubbles_in(
    history: &Value,
    text: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut bubbles = std::collections::BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for turn in history["thread"]["turns"].as_array().unwrap() {
        for item in turn["items"].as_array().unwrap() {
            if item["type"] == "userMessage"
                && item["text"]
                    .as_str()
                    .is_some_and(|shown| shown.starts_with(text))
            {
                let bubble = item["payload"]["clientId"].as_str().or(item["id"].as_str());
                bubbles
                    .entry(bubble.unwrap().to_owned())
                    .or_default()
                    .insert(turn["id"].as_str().unwrap().to_owned());
            }
        }
    }
    for message in history["messages"].as_array().unwrap() {
        let client = message["clientMessageId"].as_str().unwrap();
        if message["body"].as_str().unwrap().starts_with(text) && !bubbles.contains_key(client) {
            bubbles.insert(client.to_owned(), ["(no turn)".to_owned()].into());
        }
    }
    bubbles
}

/// The records the phone was told are now sent messages' since `events`
/// was last read, one list per event. None of them names a turn: no
/// turn's history changes.
fn told_records(
    events: &mut tokio::sync::broadcast::Receiver<crate::HostEventEnvelope>,
    chat: &str,
) -> Vec<Value> {
    let mut told = Vec::new();
    while let Ok(event) = events.try_recv() {
        let crate::WonderEvent::Activity {
            category,
            detail: Some(detail),
            ..
        } = &event.event
        else {
            continue;
        };
        if category == crate::t3_code::MESSAGE_RECORDS
            && event.conversation_id.as_deref() == Some(chat)
        {
            assert_eq!(event.turn_id, None, "{event:?}");
            told.push(serde_json::from_str::<Value>(detail).unwrap()["records"].clone());
        }
    }
    told
}

/// A history the phone saved, with each record it was told of labelled
/// as its message's, as the phone labels it.
fn labelled(mut history: Value, records: &Value) -> Value {
    for record in records.as_array().unwrap() {
        for turn in history["thread"]["turns"].as_array_mut().unwrap() {
            if turn["id"] != record["turnId"] {
                continue;
            }
            for item in turn["items"].as_array_mut().unwrap() {
                if item["id"] == record["itemId"] {
                    item["payload"]["clientId"] = record["clientId"].clone();
                }
            }
        }
    }
    history
}

/// The phone's message `client` in `history`, which is listed once.
fn listed<'a>(history: &'a Value, client: &str) -> &'a Value {
    let listed: Vec<_> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["clientMessageId"] == client)
        .collect();
    assert_eq!(listed.len(), 1, "{history}");
    listed[0]
}

/// A bubble that shows in these turns.
fn bubble(key: &str, turns: &[&str]) -> (String, std::collections::BTreeSet<String>) {
    (
        key.to_owned(),
        turns.iter().map(|turn| (*turn).to_owned()).collect(),
    )
}

/// The bytes the phone gets when it opens the conversation's file `file`.
async fn opened(fx: &Fx, file: &str) -> Vec<u8> {
    use tower::ServiceExt;
    let response = crate::router(fx.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/v1/conversations/{}/files/{file}", fx.chat))
                .header(
                    "x-wonder-loopback-capability",
                    &fx.state.loopback_capability,
                )
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec()
}

/// The intent Cupola last sent `message` to T3 Code with.
async fn intent(fx: &Fx, message: &str) -> Value {
    let sends = fx
        .state
        .store
        .t3_code_sent_messages(&fx.chat)
        .await
        .unwrap();
    let send = sends.iter().find(|send| send.id == message).unwrap();
    serde_json::from_str(&send.intent).unwrap()
}

/// The transcript the fake Claude bridge reads for the chat's thread.
fn recorded(fx: &Fx, items: &[Value]) {
    fx.write(
        "claude-items.json",
        &serde_json::json!({ CLAUDE_THREAD: items }),
    );
}

/// A user message Claude's transcript recorded, in a turn of its own.
fn user_record(id: &str, text: &str) -> Value {
    user_record_in(id, id, text)
}

/// A user message Claude's transcript recorded in turn `turn`.
fn user_record_in(turn: &str, id: &str, text: &str) -> Value {
    serde_json::json!({"turnId": turn, "item": {"type": "userMessage", "id": id,
        "content": [{"type": "text", "text": text}]}})
}

/// What Claude's transcript records for "Continue here" with the file
/// `notes.txt`: T3 Code adds where it saved the file.
const WITH_FILE: &str =
    "Continue here\n\n[Attached file \"notes.txt\" is saved at: /t3/attachments/notes.txt]";

// Contract (owner, Oct 21; lead, final design): a message sent through T3
// Code shows once, as the owner's, with its delivery state. Once T3 Code
// started it (its timeline has T3 Code's ID for it), its agent's record of
// it, with the same words after the send, is that same bubble: live (the
// phone is told which record it is) and after a reload. A message with a file
// keeps its file, which still opens; its record has T3 Code's note of
// where it saved the file. The same words typed in T3 Code before the send
// stay a bubble of their own. T3 Code gets the words plain.
#[tokio::test]
async fn a_message_sent_through_t3_code_is_one_bubble() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    let mut items = vec![
        user_record("typed", "Continue here"),
        serde_json::json!({"turnId": "typed", "item": {"type": "agentMessage", "id": "typed:reply", "text": "On it"}}),
    ];
    recorded(&scene.fx, &items);
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "typed", "status": "completed"}]),
    );
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let receipt = scene.send(client, false).await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let own = format!("local:{id}");
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    assert_eq!(fake.sends.lock().unwrap()[0]["message"], "Continue here");

    // Queued in T3 Code: Cupola's message alone.
    refresh_history(&scene.fx).await;
    let (_, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
    assert_eq!(
        bubbles,
        [bubble("typed", &["typed"]), bubble(client, &[&own])].into()
    );

    // T3 Code starts it and Claude's transcript records it.
    fake.start_next("t3-thread");
    items.extend([
        user_record("echo", "Continue here\n"),
        serde_json::json!({"turnId": "echo", "item": {"type": "agentMessage", "id": "echo:reply", "text": "Done"}}),
    ]);
    recorded(&scene.fx, &items);
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "echo", "status": "inProgress", "runningElsewhere": true},
            {"id": "typed", "status": "completed"}]),
    );
    let mut events = scene.fx.state.events.subscribe();
    refresh_history(&scene.fx).await;
    assert_eq!(
        told_records(&mut events, &scene.fx.chat),
        [serde_json::json!([{"turnId": "echo", "itemId": "echo", "clientId": client}])],
        "the phone is told the record is the message's"
    );
    let once = [bubble("typed", &["typed"]), bubble(client, &[&own, "echo"])].into();
    for reload in [false, true] {
        if reload {
            refresh_history(&scene.fx).await;
        }
        let (history, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
        assert_eq!(bubbles, once, "reloaded: {reload}");
        assert_eq!(listed(&history, client)["state"], "completed");
        // The turn still says it runs in T3 Code.
        let echo = history["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|turn| turn["id"] == "echo")
            .unwrap();
        assert_eq!(echo["status"], "inProgress", "{echo}");
        assert_eq!(echo["runningElsewhere"], true, "{echo}");
    }

    // The next message carries a file: it starts a turn once T3 Code's
    // reply ended.
    fake.queue_sends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    scene.replying(false);
    let file = scene.attach().await;
    let with_file = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let receipt = scene.send_with(with_file, false, vec![file.clone()]).await;
    let file_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&file_id).await.state, "completed");
    assert_eq!(fake.attachment_sends.lock().unwrap().len(), 1);
    items.push(user_record("files", WITH_FILE));
    recorded(&scene.fx, &items);
    scene.fx.write(
        "claude-turns.json",
        &serde_json::json!([{"id": "files", "status": "completed"},
            {"id": "echo", "status": "completed"}, {"id": "typed", "status": "completed"}]),
    );
    let mut told =
        vec![serde_json::json!([{"turnId": "files", "itemId": "files", "clientId": with_file}])];
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        // Told of the new record only, once.
        assert_eq!(
            told_records(&mut events, &scene.fx.chat),
            std::mem::take(&mut told)
        );
        let (history, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
        assert_eq!(
            bubbles,
            [
                bubble("typed", &["typed"]),
                bubble(client, &[&own, "echo"]),
                bubble(with_file, &[&format!("local:{file_id}"), "files"]),
            ]
            .into()
        );
        assert_eq!(
            listed(&history, with_file)["attachmentIds"],
            serde_json::json!([file])
        );
        assert_eq!(opened(&scene.fx, &file).await, b"Release notes");
    }
}

// Contract (lead, final design): T3 Code starting a message doesn't make
// any record its own. Until its agent records it (one that crashed never
// does), Cupola's message shows alone with its delivery state, also after
// a reload. Two started messages never share a record: each is paired, in
// the order sent, with a record of its own.
#[tokio::test]
async fn a_message_t3_code_started_shows_alone_until_its_agent_records_it() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let first = "a1a1a1a1-a1a1-4a1a-8a1a-a1a1a1a1a1a1";
    let receipt = scene.send(first, false).await;
    let first_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let first_own = format!("local:{first_id}");
    assert_eq!(scene.dispatch(&first_id).await.state, "completed");
    fake.start_next("t3-thread");
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        let (history, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
        assert_eq!(bubbles, [bubble(first, &[&first_own])].into());
        assert_eq!(listed(&history, first)["state"], "completed");
    }

    // A second message T3 Code starts too; one record arrives.
    let second = "b2b2b2b2-b2b2-4b2b-8b2b-b2b2b2b2b2b2";
    let receipt = scene.send(second, false).await;
    let second_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let second_own = format!("local:{second_id}");
    assert_eq!(scene.dispatch(&second_id).await.state, "completed");
    fake.start_next("t3-thread");
    let one = user_record("one", "Continue here");
    recorded(&scene.fx, std::slice::from_ref(&one));
    refresh_history(&scene.fx).await;
    let (_, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
    assert_eq!(
        bubbles,
        [
            bubble(first, &[&first_own, "one"]),
            bubble(second, &[&second_own])
        ]
        .into()
    );

    // The second record.
    recorded(&scene.fx, &[one, user_record("two", "Continue here")]);
    let two = [
        bubble(first, &[&first_own, "one"]),
        bubble(second, &[&second_own, "two"]),
    ]
    .into();
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        assert_eq!(user_bubbles(&scene.fx, "Continue here").await.1, two);
    }
}

// Contract (lead, final design): a message whose agent never records it
// (it crashed) keeps its bubble, delivery state and file, and other words
// the owner then types in T3 Code show too, also after a reload. Accepted
// limit: if the owner types exactly a message's words then, the two show
// as one.
#[tokio::test]
async fn a_message_its_agent_never_recorded_keeps_its_bubble_and_file() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    scene.replying(false);
    let file = scene.attach().await;
    let with_file = "e5e5e5e5-e5e5-4e5e-8e5e-e5e5e5e5e5e5";
    let receipt = scene.send_with(with_file, false, vec![file.clone()]).await;
    let file_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let file_own = format!("local:{file_id}");
    assert_eq!(scene.dispatch(&file_id).await.state, "completed");
    fake.typed("t3-thread", "Something else", false);
    let mut items = vec![user_record("typed", "Something else")];
    recorded(&scene.fx, &items);
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        let (history, bubbles) = user_bubbles(&scene.fx, "").await;
        assert_eq!(
            bubbles,
            [bubble(with_file, &[&file_own]), bubble("typed", &["typed"])].into()
        );
        let message = listed(&history, with_file);
        assert_eq!(message["state"], "completed");
        assert_eq!(message["attachmentIds"], serde_json::json!([file]));
        assert_eq!(opened(&scene.fx, &file).await, b"Release notes");
    }

    // The accepted limit: a text message whose agent never records it, and
    // the owner types exactly its words.
    let text = "f6f6f6f6-f6f6-4f6f-8f6f-f6f6f6f6f6f6";
    let receipt = scene.send(text, false).await;
    let text_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&text_id).await.state, "completed");
    fake.typed("t3-thread", "Continue here", false);
    items.push(user_record("same", "Continue here"));
    recorded(&scene.fx, &items);
    refresh_history(&scene.fx).await;
    assert_eq!(
        user_bubbles(&scene.fx, "").await.1,
        [
            bubble(with_file, &[&file_own]),
            bubble("typed", &["typed"]),
            bubble(text, &[&format!("local:{text_id}"), "same"]),
        ]
        .into()
    );
}

// Contract (lead, fix round 4 and final design): a record saved before a
// message was sent is never its record, even with its words. The phone's
// first message is started and recorded, and the owner types the same
// words in T3 Code, while T3 Code can't be read; then the phone sends a
// second one, also started and recorded. Once T3 Code can be read, each
// message is paired with its own record, and the owner's shows.
#[tokio::test]
async fn a_record_saved_before_a_message_was_sent_is_never_its_record() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let store = &scene.fx.state.store;
    let first = "c3c3c3c3-c3c3-4c3c-8c3c-c3c3c3c3c3c3";
    let receipt = scene.send(first, false).await;
    let first_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&first_id).await.state, "completed");
    fake.start_next("t3-thread");
    fake.typed("t3-thread", "Continue here", false);
    let mut items = vec![
        user_record("one", "Continue here"),
        user_record("typed", "Continue here"),
    ];
    recorded(&scene.fx, &items);
    fake.failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    refresh_history(&scene.fx).await;
    fake.failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(intent(&scene.fx, &first_id).await["t3RetryAtMs"].is_i64());

    let second = "d4d4d4d4-d4d4-4d4d-8d4d-d4d4d4d4d4d4";
    let receipt = scene.send(second, false).await;
    let second_id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    assert_eq!(scene.dispatch(&second_id).await.state, "completed");
    fake.start_next("t3-thread");
    items.push(user_record("two", "Continue here"));
    recorded(&scene.fx, &items);
    store
        .update_t3_code_intent(&first_id, &serde_json::json!({"t3RetryAtMs": 0}))
        .await
        .unwrap();
    refresh_history(&scene.fx).await;
    assert_eq!(
        user_bubbles(&scene.fx, "").await.1,
        [
            bubble(first, &[&format!("local:{first_id}"), "one"]),
            bubble("typed", &["typed"]),
            bubble(second, &[&format!("local:{second_id}"), "two"]),
        ]
        .into()
    );
}

// Contract (lead, fix round 4 and final design): Cupola never stops
// looking for T3 Code to start a message, however long ago it was sent,
// and it reads T3 Code's timeline only when a new record has the message's
// words. The same words typed in T3 Code while the message waited are
// asked about once, then never again, and other words never; the
// message's own record is its bubble days later. A lookup that fails waits
// longer each time before the next.
#[tokio::test]
async fn a_message_t3_code_starts_days_later_is_still_one_bubble() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = "edededed-eded-4ded-8ded-edededededed";
    let receipt = scene.send(client, false).await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let own = format!("local:{id}");
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    let store = &scene.fx.state.store;
    let sent_at = chrono::Utc::now() - chrono::Duration::days(3);
    store
        .update_t3_code_intent(&id, &serde_json::json!({"sentAt": sent_at.to_rfc3339()}))
        .await
        .unwrap();
    let reads = || fake.reads.load(std::sync::atomic::Ordering::SeqCst);

    // The owner types the same words in T3 Code while the message waits:
    // asked once.
    fake.typed("t3-thread", "Continue here", false);
    let mut items = vec![user_record("typed", "Continue here")];
    recorded(&scene.fx, &items);
    let before = reads();
    refresh_history(&scene.fx).await;
    assert!(reads() > before, "T3 Code is asked about the new record");
    let alone = |typed: &[&str]| {
        let mut bubbles: std::collections::BTreeMap<_, _> =
            [bubble(client, &[own.as_str()])].into();
        bubbles.extend(typed.iter().map(|id| bubble(id, &[id])));
        bubbles
    };
    assert_eq!(
        user_bubbles(&scene.fx, "Continue here").await.1,
        alone(&["typed"])
    );
    let before = reads();
    refresh_history(&scene.fx).await;
    assert_eq!(reads(), before, "nothing new was recorded");
    // Other words are never asked about.
    items.push(user_record("other", "Something else"));
    recorded(&scene.fx, &items);
    refresh_history(&scene.fx).await;
    assert_eq!(reads(), before, "not the message's words");

    // T3 Code can't be read: a new record is asked about once, and the
    // next only after a wait.
    fake.failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    items.push(user_record("typed-2", "Continue here"));
    recorded(&scene.fx, &items);
    refresh_history(&scene.fx).await;
    let waited = intent(&scene.fx, &id).await["t3RetryAtMs"]
        .as_i64()
        .unwrap()
        - crate::now_ms() as i64;
    assert!((20_000..=30_000).contains(&waited), "{waited}");
    items.push(user_record("typed-3", "Continue here"));
    recorded(&scene.fx, &items);
    let before = reads();
    refresh_history(&scene.fx).await;
    assert_eq!(reads(), before, "waiting after a failed lookup");
    // Not known to be started, the message is no record: its words typed
    // in T3 Code show on their own.
    assert_eq!(
        user_bubbles(&scene.fx, "Continue here").await.1,
        alone(&["typed", "typed-2", "typed-3"])
    );
    // The wait passes, and the lookup fails again: it doubles.
    store
        .update_t3_code_intent(&id, &serde_json::json!({"t3RetryAtMs": 0}))
        .await
        .unwrap();
    refresh_history(&scene.fx).await;
    let waited = intent(&scene.fx, &id).await["t3RetryAtMs"]
        .as_i64()
        .unwrap()
        - crate::now_ms() as i64;
    assert!((50_000..=60_000).contains(&waited), "{waited}");

    // T3 Code is back; the message still waits there.
    fake.failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    store
        .update_t3_code_intent(&id, &serde_json::json!({"t3RetryAtMs": 0}))
        .await
        .unwrap();
    refresh_history(&scene.fx).await;
    let typed = ["typed", "typed-2", "typed-3"];
    assert_eq!(
        user_bubbles(&scene.fx, "Continue here").await.1,
        alone(&typed)
    );
    // Then T3 Code starts it, and its agent records it.
    fake.start_next("t3-thread");
    items.push(user_record("echo", "Continue here"));
    recorded(&scene.fx, &items);
    let mut once = alone(&typed);
    once.insert(client.to_owned(), [own.clone(), "echo".to_owned()].into());
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        assert_eq!(user_bubbles(&scene.fx, "Continue here").await.1, once);
    }
}

// Contract (lead, live fix and refresh fix): the phone can save an agent's
// record of a message before T3 Code is known to have started that message
// (T3 Code couldn't be read), and shows two bubbles; a newer read that
// leaves the record out doesn't change the one it saved. Once T3 Code is
// read and started the message, the phone is told, once, that the record it
// saved is that message's: labelled as told, its saved read is one bubble,
// without reading again. A phone that missed that news labels its saved
// read from its next read, which names the record also when its page
// leaves the record out. A reload agrees.
#[tokio::test]
async fn a_record_the_phone_saved_before_its_message_started_joins_its_bubble() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = "f0f0f0f0-f0f0-4f0f-8f0f-f0f0f0f0f0f0";
    let receipt = scene.send(client, false).await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let own = format!("local:{id}");
    assert_eq!(scene.dispatch(&id).await.state, "completed");

    // T3 Code starts it and its agent records it, but T3 Code can't be read.
    fake.failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fake.start_next("t3-thread");
    recorded(
        &scene.fx,
        &[
            user_record("echo", "Continue here"),
            serde_json::json!({"turnId": "echo", "item": {"type": "agentMessage", "id": "echo:reply", "text": "Done"}}),
        ],
    );
    let mut events = scene.fx.state.events.subscribe();
    refresh_history(&scene.fx).await;
    assert_eq!(
        told_records(&mut events, &scene.fx.chat),
        Vec::<Value>::new()
    );
    let (saved, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
    assert_eq!(
        bubbles,
        [bubble(client, &[&own]), bubble("echo", &["echo"])].into()
    );

    // T3 Code is back, and the owner goes on in T3 Code.
    fake.failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    scene
        .fx
        .state
        .store
        .update_t3_code_intent(&id, &serde_json::json!({"t3RetryAtMs": 0}))
        .await
        .unwrap();
    recorded(
        &scene.fx,
        &[
            user_record("echo", "Continue here"),
            serde_json::json!({"turnId": "echo", "item": {"type": "agentMessage", "id": "echo:reply", "text": "Done"}}),
            user_record("later", "Thanks"),
            serde_json::json!({"turnId": "later", "item": {"type": "agentMessage", "id": "later:reply", "text": "Sure"}}),
        ],
    );
    refresh_history(&scene.fx).await;
    let told = told_records(&mut events, &scene.fx.chat);
    let records = serde_json::json!([{"turnId": "echo", "itemId": "echo", "clientId": client}]);
    assert_eq!(told, std::slice::from_ref(&records));
    let once = [bubble(client, &[&own, "echo"])].into();
    assert_eq!(
        bubbles_in(&labelled(saved.clone(), &told[0]), "Continue here"),
        once
    );

    // The phone missed that news: its next read of the latest page, which
    // leaves the record out, names it.
    let (status, page) = crate::test_support::call(
        &scene.fx.state,
        "GET",
        &format!("/api/v1/conversations/{}/history?limit=2", scene.fx.chat),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    crate::test_support::assert_http_contract("conversationSnapshot", &page);
    let turns: Vec<_> = page["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["id"].clone())
        .collect();
    assert_eq!(turns, ["later"], "{page}");
    assert_eq!(page["messageRecords"], records);
    assert_eq!(
        bubbles_in(&labelled(saved, &page["messageRecords"]), "Continue here"),
        once
    );
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        assert_eq!(
            told_records(&mut events, &scene.fx.chat),
            Vec::<Value>::new()
        );
        let (history, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
        assert_eq!(bubbles, once);
        assert_eq!(history["messageRecords"], records);
    }
}

// Contract (lead, fix round 3 and final design): the phone sends words
// while T3 Code's reply runs, and the owner types the same words in T3
// Code after them. T3 Code starts both; its agent records the phone's at
// once and the owner's late. The phone's is one bubble, and the owner's
// shows the moment it is recorded: two bubbles, live and after a reload.
#[tokio::test]
async fn the_same_words_typed_in_t3_code_and_recorded_late_are_two_bubbles() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    let mut items = vec![
        user_record("earlier", "Earlier"),
        serde_json::json!({"turnId": "earlier", "item": {"type": "agentMessage", "id": "earlier:reply", "text": "On it"}}),
    ];
    recorded(&scene.fx, &items);
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = "abababab-abab-4bab-8bab-abababababab";
    let receipt = scene.send(client, false).await;
    let id = receipt["wonderMessageId"].as_str().unwrap().to_owned();
    let own = format!("local:{id}");
    assert_eq!(scene.dispatch(&id).await.state, "completed");
    fake.typed("t3-thread", "Continue here", true);
    assert_eq!(
        fake.texts("t3-thread")[2..],
        ["Continue here", "Continue here"]
    );
    refresh_history(&scene.fx).await;
    let (_, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
    assert_eq!(bubbles, [bubble(client, &[&own])].into());

    // T3 Code starts both; only the phone's is recorded yet.
    fake.start_next("t3-thread");
    fake.start_next("t3-thread");
    items.push(user_record("echo", "Continue here"));
    recorded(&scene.fx, &items);
    refresh_history(&scene.fx).await;
    let (_, bubbles) = user_bubbles(&scene.fx, "Continue here").await;
    assert_eq!(bubbles, [bubble(client, &[&own, "echo"])].into());

    // The owner's is recorded late, in the same turn: a bubble of its own.
    items.push(user_record_in("echo", "typed", "Continue here"));
    recorded(&scene.fx, &items);
    let two = [bubble(client, &[&own, "echo"]), bubble("typed", &["echo"])].into();
    for _ in 0..2 {
        refresh_history(&scene.fx).await;
        assert_eq!(user_bubbles(&scene.fx, "Continue here").await.1, two);
    }
}

// Provider item pages still own history while T3 Code holds the native
// session. Completion clocks (Codex) and explicit status (Claude) must survive
// refresh, durable storage and repeated cached reads without another marker.
#[tokio::test]
async fn history_keeps_one_completed_compaction_in_a_t3_code_held_thread() {
    // `t3_codex_scene` is macOS-only, so Linux runs the Claude half alone.
    #[cfg(target_os = "macos")]
    let families = [AgentFamily::Claude, AgentFamily::Codex];
    #[cfg(not(target_os = "macos"))]
    let families = [AgentFamily::Claude];
    for family in families {
        let fake = crate::t3_code::tests::Fake::start().await;
        let mut scene = match family {
            AgentFamily::Claude => t3_scene(&fake).await,
            #[cfg(target_os = "macos")]
            _ => t3_codex_scene(&fake).await,
            #[cfg(not(target_os = "macos"))]
            _ => unreachable!("only the Claude family runs off macOS"),
        };
        scene.fx.state.t3_code = crate::t3_code::tests::connected(&fake).await.0;
        let held = scene.matched().await;
        assert_eq!(held["heldBy"]["app"], "T3 Code", "{held}");
        let mut boundary = serde_json::json!({"turnId":"t3-turn", "item":{
            "id":"native-boundary", "type":"contextCompaction"}});
        if family == AgentFamily::Claude {
            boundary["item"]["status"] = "completed".into();
        } else {
            boundary["startedAtMs"] = 2000.into();
            boundary["completedAtMs"] = 3000.into();
        }
        let mut items = vec![
            serde_json::json!({"turnId":"t3-turn", "item":{"id":"before", "type":"agentMessage", "text":"Before boundary"}}),
            boundary,
            serde_json::json!({"turnId":"t3-turn", "item":{"id":"after", "type":"agentMessage", "text":"After boundary"}}),
        ];
        if family == AgentFamily::Claude {
            // Read a real synthetic native JSONL through the production reader
            // and projector, rather than handing the fake bridge a marker.
            let cwd = scene
                .fx
                .state
                .store
                .project_conversation(&scene.fx.chat)
                .await
                .unwrap()
                .unwrap()
                .cwd;
            let path =
                crate::desktop_activity::claude_session_transcript(&scene.session, &cwd).unwrap();
            let rows = [
                serde_json::json!({"type":"user", "uuid":"t3-turn", "parentUuid":null, "message":{"content":"Fixture question"}}),
                serde_json::json!({"type":"assistant", "uuid":"before", "parentUuid":"t3-turn", "message":{"id":"before", "content":[{"type":"text", "text":"Before boundary"}]}}),
                serde_json::json!({"type":"system", "subtype":"compact_boundary", "uuid":"native-boundary", "parentUuid":null, "logicalParentUuid":"before", "timestamp":"2026-10-27T00:00:03Z", "content":"PRIVATE SYNTHETIC BOUNDARY"}),
                serde_json::json!({"type":"user", "uuid":"summary", "parentUuid":"native-boundary", "isCompactSummary":true, "message":{"content":"PRIVATE SYNTHETIC SUMMARY"}}),
                serde_json::json!({"type":"assistant", "uuid":"after", "parentUuid":"summary", "message":{"id":"after", "content":[{"type":"text", "text":"After boundary"}]}}),
            ];
            std::fs::write(&path, rows.map(|row| row.to_string()).join("\n") + "\n").unwrap();
            let source = r#"
import { readFile } from 'node:fs/promises';
import { ClaudeSessionFiles } from './services/claude-runtime/session-file.mjs';
import { nativeTurns } from './services/claude-runtime/project-history.mjs';
const [path, root, session, cwd] = process.argv.slice(1);
const files = new ClaudeSessionFiles(root);
try {
  const rows = (await readFile(path, 'utf8')).trim().split('\n').map(line => JSON.parse(line));
  const earlier = await files.earlierMessages(session, cwd, 'summary');
  const turns = nativeTurns([...earlier, ...rows.slice(3)]);
  console.log(JSON.stringify(turns.flatMap(turn => turn.items.map(item => ({turnId:turn.id, item})))));
} finally { await files.close(); }
"#;
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new("node")
                    .args(["--input-type=module", "--eval", source])
                    .arg(&path)
                    .arg(path.parent().unwrap().parent().unwrap())
                    .arg(&scene.session)
                    .arg(&cwd)
                    .current_dir(
                        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                            .parent()
                            .unwrap()
                            .parent()
                            .unwrap(),
                    )
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("native reader timed out")
            .expect("Node is required for the native Claude history fixture");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            items = serde_json::from_slice(&output.stdout).unwrap();
            assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE"));
        }
        let thread = scene
            .fx
            .state
            .store
            .conversation_thread(&scene.fx.chat)
            .await
            .unwrap()
            .unwrap();
        scene.fx.write(
            if family == AgentFamily::Claude {
                "claude-items.json"
            } else {
                "codex-items.json"
            },
            &serde_json::json!({ thread: items }),
        );
        for _ in 0..2 {
            refresh_history(&scene.fx).await;
            let (snapshot, _) = user_bubbles(&scene.fx, "unused").await;
            let turn = snapshot["thread"]["turns"]
                .as_array()
                .unwrap()
                .iter()
                .find(|turn| turn["id"] == "t3-turn")
                .expect("native turn");
            let items = turn["items"].as_array().unwrap();
            assert_eq!(
                items
                    .iter()
                    .map(|item| item["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                if family == AgentFamily::Claude {
                    vec![
                        "t3-turn",
                        "t3-turn:before:0",
                        "native-boundary",
                        "t3-turn:after:0",
                    ]
                } else {
                    vec!["before", "native-boundary", "after"]
                }
            );
            let markers = items
                .iter()
                .filter(|item| item["type"] == "contextCompaction")
                .collect::<Vec<_>>();
            assert_eq!(markers.len(), 1, "{snapshot}");
            assert_eq!(markers[0]["state"], "completed");
            assert!(markers[0]["text"].is_null());
        }
    }
}

// Contract (owner, Nov 2): a T3 Code `delegate_task` child in the agent-task
// sheet of a chat T3 Code holds opens read-only from T3 Code's own timeline of
// it, running or completed; one T3 Code can't read (not answering, the chat
// no longer open there, no longer a child of this chat) answers the plain
// reason, never a bare 404 the phone words as a generic failure.
#[tokio::test]
async fn a_t3_code_delegate_child_opens_or_says_why() {
    use axum::extract::{Path, Query, State};
    use axum::{response::IntoResponse, Extension};
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    fake.subtask(
        "t3-done",
        "t3-thread",
        "p",
        "completed",
        &["Transcribe the Duo videos"],
    );
    fake.subtask(
        "t3-running",
        "t3-thread",
        "p",
        "running",
        &["Find the issues"],
    );
    fake.thread("t3-stranger", "p", "claudeAgent", None, &["Not a child"]);
    let read = |thread: &'static str, kind: &'static str| {
        let state = scene.fx.state.clone();
        let chat = scene.fx.chat.clone();
        async move {
            let response = crate::project_subagents::transcript(
                State(state),
                Extension(crate::OwnerAuthority),
                Path((chat, thread.into())),
                Query(serde_json::from_value(serde_json::json!({"kind": kind})).unwrap()),
            )
            .await
            .into_response();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let (status, body) = read("t3-done", "t3Task").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done: Value = serde_json::from_str(&body).unwrap();
    crate::tests::validate_http_contract("projectSubagentTranscript", &done);
    assert_eq!(done["subagent"]["kind"], "t3Task");
    assert_eq!(done["subagent"]["status"], "completed");
    let texts: Vec<String> = done["snapshot"]["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap().clone())
        .filter_map(|item| item["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(texts, ["Transcribe the Duo videos", "ok"], "{body}");

    let (status, body) = read("t3-running", "t3Task").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let running: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(running["subagent"]["status"], "active");

    // Not a child of this chat: says so, whatever else T3 Code holds.
    let (status, body) = read("t3-stranger", "t3Task").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("no longer lists this task"), "{body}");

    // T3 Code failing: the reason, not a 404.
    fake.failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = read("t3-done", "t3Task").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("T3 Code did not answer"), "{body}");
    fake.failing
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // T3 Code let go of the chat: the reason, not Codex's thread lookup.
    scene.released().await;
    let (status, body) = read("t3-done", "t3Task").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("no longer has this chat open"), "{body}");
}

// Contract (owner, Nov 4): a chat T3 Code holds lists its `delegate_task`
// children in its agent tasks, each with the status T3 Code reports, and only
// its own: another chat's children and plain T3 threads are not part of it.
// T3 Code lists a project 100 threads a page, so the children are found on
// later pages too.
#[tokio::test]
async fn a_t3_code_chat_lists_its_delegate_children_as_agent_tasks() {
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    for n in 0..105 {
        fake.thread(&format!("t3-filler-{n}"), "p", "claudeAgent", None, &["x"]);
    }
    fake.subtask("t3-done", "t3-thread", "p", "completed", &["Transcribe"]);
    fake.subtask("t3-running", "t3-thread", "p", "running", &["Find issues"]);
    fake.subtask("t3-elsewhere", "t3-other", "p", "completed", &["Not ours"]);
    let conversation = scene
        .fx
        .state
        .store
        .project_conversation(&scene.fx.chat)
        .await
        .unwrap()
        .unwrap();
    let rows = crate::project_subagents::t3_rows(&scene.fx.state, &conversation).await;
    let list = serde_json::to_value(&rows).unwrap();
    let mut tasks: Vec<(String, String, String)> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "t3Task")
        .map(|row| {
            (
                row["threadId"].as_str().unwrap().to_owned(),
                row["status"].as_str().unwrap().to_owned(),
                row["parentConversationId"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    tasks.sort();
    let chat = scene.fx.chat.clone();
    assert_eq!(
        tasks,
        [
            ("t3-done".to_owned(), "completed".to_owned(), chat.clone()),
            ("t3-running".to_owned(), "active".to_owned(), chat)
        ],
        "{list}"
    );
}

// Contract (lead review, Nov 4): T3 Code that stops answering costs the agent
// tasks list and a task's sheet their budget (1.5 s, 3 s), not reqwest's 30 s:
// at the project list and at later listing pages alike. A task the budget
// ran out before finding is "did not answer in time", not "no longer lists".
#[tokio::test]
async fn a_t3_code_that_stops_answering_costs_the_task_list_only_its_budget() {
    use axum::extract::{Path, Query, State};
    use axum::{response::IntoResponse, Extension};
    use std::time::{Duration, Instant};
    let fake = crate::t3_code::tests::Fake::start().await;
    let scene = t3_scene(&fake).await;
    scene.matched().await;
    for n in 0..105 {
        fake.thread(&format!("t3-filler-{n}"), "p", "claudeAgent", None, &["x"]);
    }
    fake.subtask("t3-done", "t3-thread", "p", "completed", &["Transcribe"]);
    let conversation = scene
        .fx
        .state
        .store
        .project_conversation(&scene.fx.chat)
        .await
        .unwrap()
        .unwrap();
    let read = || {
        let state = scene.fx.state.clone();
        let chat = scene.fx.chat.clone();
        async move {
            let response = crate::project_subagents::transcript(
                State(state),
                Extension(crate::OwnerAuthority),
                Path((chat, "t3-done".into())),
                Query(serde_json::from_value(serde_json::json!({"kind": "t3Task"})).unwrap()),
            )
            .await
            .into_response();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    // The project list never answers.
    fake.hanging.send_replace(true);
    let started = Instant::now();
    let rows = crate::project_subagents::t3_rows(&scene.fx.state, &conversation).await;
    assert!(rows.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let started = Instant::now();
    let (status, body) = read().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("T3 Code did not answer"), "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    fake.hanging.send_replace(false);

    // Page two of the thread list, where the child is, never answers.
    fake.hanging_pages.send_replace(true);
    let started = Instant::now();
    let (status, body) = read().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("did not answer in time"), "{body}");
    assert!(!body.contains("no longer lists"), "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    fake.hanging_pages.send_replace(false);
}
