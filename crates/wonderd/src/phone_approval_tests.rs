//! Synthetic paired-phone requests; no live Project or model work is started.

use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::{json, Value};
use tower::ServiceExt;

struct PhoneFixture {
    dir: tempfile::TempDir,
    state: AppState,
    key: SigningKey,
    thread: &'static str,
}

pub(crate) fn hold_provider_send(dir: &std::path::Path) {
    let script = dir.join("runtime.py");
    let original = std::fs::read_to_string(&script).unwrap();
    let marker = "    elif method == 'turn/start':\n        with open(root + '/accepted-client'";
    assert!(original.contains(marker));
    std::fs::write(
        &script,
        original.replacen(
            marker,
            r#"    elif method == 'turn/start':
        open(root + '/send-waiting', 'w').close()
        def pending_send(request_id):
            while not os.path.exists(root + '/release-send'): time.sleep(0.01)
            print(json.dumps({'id':request_id,'result':{'turn':{'id':'sending-turn'}}}),flush=True)
        threading.Thread(target=pending_send,args=(r['id'],),daemon=True).start()
        continue
        with open(root + '/accepted-client'"#,
            1,
        ),
    )
    .unwrap();
}

// Reconnect must not replace a runtime whose only work is an unresolved
// submission. The native roster can still say idle before acceptance.
#[tokio::test]
async fn reconnect_preserves_a_send_without_a_turn_receipt() {
    let (dir, state, sending) = projects::tests::dispatch_failure_fixture().await;
    hold_provider_send(dir.path());
    let send = tokio::spawn(projects::dispatch(state.clone(), sending.clone()));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !dir.path().join("send-waiting").exists() {
            assert!(!send.is_finished());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let generation = state.projects.codex.lock().await.health().id().to_owned();
    let response = crate::provider_runtime::reconnect(
        State(state.clone()),
        Some(Extension(LocalOwnerAuthority)),
        Path("codex".into()),
    )
    .await;
    let after = state.projects.codex.lock().await.health().id().to_owned();
    let pending = !send.is_finished();
    std::fs::write(dir.path().join("release-send"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(10), send)
        .await
        .unwrap()
        .unwrap();
    state.projects.shutdown().await;
    eprintln!(
        "Reconnect during pending submit: status={}, same_generation={}, send_pending={pending}",
        response.status(),
        generation == after
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(after, generation);
    assert!(pending);
    assert_eq!(
        state
            .store
            .message_by_id(&sending.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "accepted_by_codex"
    );
}

// Contract: a provider awaiting A's turn/start receipt cannot hold B's Stop.
// The real daemon dispatch/Stop owners use the existing scripted transport;
// its stdin stays responsive while A's response is explicitly held.
#[tokio::test]
async fn stop_on_a_finished_turn_is_a_quiet_success_that_changes_nothing() {
    for finished in ["completed", "failed"] {
        let (dir, state, sending) = projects::tests::dispatch_failure_fixture().await;
        state
            .store
            .update_message_delivery(
                &sending.id,
                "streaming",
                Some("done-thread"),
                Some("done-turn"),
            )
            .await
            .unwrap();
        state
            .store
            .force_message_state(&sending.id, finished)
            .await
            .unwrap();
        let response = interrupt_turn(
            State(state.clone()),
            Path((sending.conversation_id.clone(), "done-turn".into())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{finished}");
        let after = state
            .store
            .message_by_id(&sending.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.state, finished,
            "Stop must not change a finished turn"
        );
        let requests =
            std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap_or_default();
        assert!(!requests.contains("turn/interrupt"), "{requests}");
    }
}

#[tokio::test]
async fn stop_reaches_its_provider_while_another_conversation_send_waits() {
    for terminal_before_receipt in [false, true] {
        let (dir, state, sending) = projects::tests::dispatch_failure_fixture().await;
        let source = state
            .store
            .project_conversation(&sending.conversation_id)
            .await
            .unwrap()
            .unwrap();
        state
            .store
            .create_project_conversation(wonder_store::ProjectConversationInsert {
                conversation_id: "stop-chat",
                project_id: &source.project_id,
                family: AgentFamily::Codex,
                provider_store: &source.provider_store,
                native_session_id: Some("stop-thread"),
                cwd: &source.cwd,
                roots_revision: source.roots_revision,
                title: "Stop target",
                model: source.model.as_deref(),
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
        test_support::bind_project_thread(
            &state,
            "stop-chat",
            AgentFamily::Codex,
            "stop-thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let MessageInsert::Inserted(active) = state
            .store
            .insert_message(
                "owner",
                "stop-target",
                "Keep working",
                "hash",
                "stop-chat",
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        state
            .store
            .update_message_delivery(
                &active.id,
                "streaming",
                Some("stop-thread"),
                Some("stop-turn"),
            )
            .await
            .unwrap();
        hold_provider_send(dir.path());
        let send = tokio::spawn(projects::dispatch(state.clone(), sending.clone()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !dir.path().join("send-waiting").exists() {
                if send.is_finished() {
                    panic!(
                        "send must reach the provider: {:?}; {:?}",
                        state.store.message_by_id(&sending.id).await.unwrap(),
                        state
                            .store
                            .message_delivery_error(&sending.id)
                            .await
                            .unwrap()
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scripted provider holds A's receipt");
        let started = std::time::Instant::now();
        let stopped = tokio::time::timeout(
            Duration::from_secs(60),
            interrupt_turn(
                State(state.clone()),
                Path(("stop-chat".into(), "stop-turn".into())),
            ),
        )
        .await;
        let elapsed = started.elapsed();
        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        let reached = requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|r| {
                r["method"] == "turn/interrupt"
                    && r["params"]["threadId"] == "stop-thread"
                    && r["params"]["turnId"] == "stop-turn"
            });
        let held = !send.is_finished() && !dir.path().join("release-send").exists();
        eprintln!("Stop while A held: elapsed={elapsed:?}, bound=500ms, reached_provider={reached}, A_held={held}");
        let same_chat_stop = if reached {
            // This claim still belongs to A even though other chats can enter.
            projects::dispatch(state.clone(), sending.clone()).await;
            {
                let _dispatch = state.dispatch_lock.lock().await;
                state
                    .store
                    .recover_direct_dispatch_claims_except(&state.projects.dispatching())
                    .await
                    .unwrap();
            }
            assert_eq!(
                state
                    .store
                    .message_by_id(&sending.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "dispatching_to_codex"
            );
            let update = tokio::time::timeout(
                Duration::from_secs(60),
                state
                    .update_admission
                    .prepare_update("pending-send-update", &state),
            )
            .await
            .unwrap();
            assert_eq!(
                update.unwrap_err().0,
                StatusCode::CONFLICT,
                "an update must not overtake an unresolved submission"
            );
            assert!(!state.update_admission.work_paused());
            let archive = tokio::time::timeout(
                Duration::from_secs(60),
                projects::update_conversation(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path(sending.conversation_id.clone()),
                    Json(serde_json::from_value(json!({"isArchived":true})).unwrap()),
                ),
            )
            .await
            .expect("archive settles around the unresolved send without awaiting its provider");
            // Archive is never refused; the send keeps its receipt's authority.
            assert_eq!(archive.status(), StatusCode::OK);
            assert!(
                state
                    .store
                    .project_conversation(&sending.conversation_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .wonder_archived
            );
            let rewind = tokio::time::timeout(
                Duration::from_secs(60),
                crate::rewind::rewind(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path(sending.conversation_id.clone()),
                    Json(crate::rewind::RewindRequest {
                        turn_id: "earlier-turn".into(),
                        restore_files: false,
                    }),
                ),
            )
            .await
            .expect("rewind cannot cut history under the unresolved send");
            assert_eq!(rewind.status(), StatusCode::CONFLICT);
            if terminal_before_receipt {
                tokio::time::timeout(Duration::from_secs(60), publish_app_server_notification(&state, json!({
                "method":"turn/completed", "params":{"threadId":"thread","turn":{"id":"sending-turn","status":"completed"}}
            }))).await.expect("early completion can be buffered while A waits");
            }
            // Stop for the very same send must await its receipt. With the old
            // global barrier removed wholesale this would return an early 404.
            let stop = tokio::spawn(interrupt_turn(
                State(state.clone()),
                Path((sending.conversation_id.clone(), "sending-turn".into())),
            ));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                !stop.is_finished(),
                "same-chat Stop must wait for the send receipt"
            );
            Some(stop)
        } else {
            None
        };
        // Release even on the pre-fix failure, so the captured reproduction leaves
        // neither a blocked daemon task nor a scripted provider behind.
        std::fs::write(dir.path().join("release-send"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(10), send)
            .await
            .unwrap()
            .unwrap();
        if let Some(stop) = same_chat_stop {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(60), stop)
                    .await
                    .unwrap()
                    .unwrap()
                    .status(),
                if terminal_before_receipt {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::ACCEPTED
                }
            );
            let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
            assert_eq!(
                requests
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|r| r["method"] == "turn/start")
                    .count(),
                1,
                "the pending send cannot be submitted twice"
            );
            assert_eq!(
                state
                    .store
                    .message_by_id(&sending.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                if terminal_before_receipt {
                    "completed"
                } else {
                    "accepted_by_codex"
                },
                "native completion keeps authority over the later dispatch receipt and Stop"
            );
        }
        state.projects.shutdown().await;
        assert!(held, "A must still be waiting when Stop is measured");
        assert!(
            reached,
            "B's Stop must reach the provider before A is released"
        );
        assert_eq!(
            stopped
                .expect("B's Stop must not wait for A's held send")
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            state
                .store
                .message_by_id(&active.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "streaming",
            "an interrupt acknowledgement is not a terminal event"
        );
    }
}

// Contract: Stop only reaches the exact active receipt. Neither a provider
// rejection nor a stale/foreign target can finish a message or stop other work.
#[tokio::test]
async fn stop_rejects_stale_foreign_turns_and_preserves_work_on_provider_error() {
    let fixture = PhoneFixture::new().await;
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    for (chat, turn) in [("foreign-chat", "turn"), (conversation, "foreign-turn")] {
        assert_eq!(
            interrupt_turn(
                State(fixture.state.clone()),
                Path((chat.into(), turn.into()))
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    let message = fixture
        .state
        .store
        .message_for_codex_turn("turn")
        .await
        .unwrap()
        .unwrap();
    fixture
        .state
        .store
        .update_message_delivery(&message.id, "completed", None, None)
        .await
        .unwrap();
    assert_eq!(
        interrupt_turn(
            State(fixture.state.clone()),
            Path((conversation.into(), "turn".into()))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    fixture
        .state
        .store
        .force_message_state(&message.id, "streaming")
        .await
        .unwrap();
    std::fs::write(
        fixture.dir.path().join("interrupt-response.json"),
        json!({"error":{"code":-32000,"message":"scripted stop rejection"}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        interrupt_turn(
            State(fixture.state.clone()),
            Path((conversation.into(), "turn".into()))
        )
        .await
        .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        fixture
            .state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "streaming"
    );
    let requests = std::fs::read_to_string(fixture.dir.path().join("requests-jsonl")).unwrap();
    assert_eq!(
        requests
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|r| r["method"] == "turn/interrupt")
            .count(),
        1,
        "only the active owned turn reaches the provider"
    );
    fixture.shutdown().await;
}

// Contract: owner action identity survives HTTP decoding and provider routing.
// A repeated request is the same action; internal and legacy Stops mint no ID.
#[tokio::test]
async fn stop_action_contract_reaches_only_the_claude_runtime_unchanged() {
    let contract: Value = serde_json::from_str(include_str!(
        "../../../tests/contracts/native/stop-actions.json"
    ))
    .unwrap();
    let authority: Value = serde_json::from_str(include_str!(
        "../../../packages/protocol/schemas/wonder-http-v1.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&json!({
        "$ref": "#/$defs/interruptTurnRequest", "$defs": authority["$defs"]
    }))
    .unwrap();
    for family in [AgentFamily::Claude, AgentFamily::Codex] {
        let mut fixture = PhoneFixture::with_family(family).await;
        // Claude Stops go to the Claude bridge, which logs what it receives.
        let log = if family == AgentFamily::Claude {
            fixture.install_claude_bridge().await;
            "claude-requests-jsonl"
        } else {
            "requests-jsonl"
        };
        let chat = if family == AgentFamily::Claude {
            "phone-provider"
        } else {
            "d19b0f7e-97bb-45d7-8206-324a62386dd9"
        };
        let path = format!("/api/v1/conversations/{chat}/turns/turn/interrupt");
        let app = Router::new()
            .route(
                "/api/v1/conversations/{conversation_id}/turns/{turn_id}/interrupt",
                post(interrupt_turn_http),
            )
            .with_state(fixture.state.clone());
        for body in contract["invalid"].as_array().unwrap() {
            assert!(!validator.is_valid(body), "invalid contract: {body}");
            let response = app
                .clone()
                .oneshot(
                    Request::post(&path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        }
        let actions = [
            contract["first"].clone(),
            contract["first"].clone(),
            contract["legacy"].clone(),
            json!({"ownerActionId": null}),
            contract["second"].clone(),
        ];
        for body in &actions {
            crate::test_support::assert_http_contract("interruptTurnRequest", body);
            let response = app
                .clone()
                .oneshot(
                    Request::post(&path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED, "{body}");
        }
        assert_eq!(
            app.oneshot(Request::post(&path).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            interrupt_turn(
                State(fixture.state.clone()),
                Path((chat.into(), "turn".into()))
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        let requests = std::fs::read_to_string(fixture.dir.path().join(log)).unwrap();
        let stops: Vec<Value> = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|r| r["method"] == "turn/interrupt")
            .map(|r| r["params"].clone())
            .collect();
        assert_eq!(
            stops.len(),
            actions.len() + 2,
            "invalid actions never reach the runtime"
        );
        for (i, params) in stops.iter().enumerate() {
            assert_eq!(params["threadId"], fixture.thread);
            assert_eq!(params["turnId"], "turn");
            let expected = if family == AgentFamily::Claude {
                actions
                    .get(i)
                    .and_then(|body| body["ownerActionId"].as_str())
            } else {
                None
            };
            assert_eq!(params["ownerActionId"].as_str(), expected);
            if expected.is_none() {
                assert!(params.get("ownerActionId").is_none());
            }
        }
        fixture.shutdown().await;
    }
}

impl PhoneFixture {
    /// A Claude bridge that acknowledges every request and logs it.
    async fn install_claude_bridge(&mut self) {
        let entrypoint = self.dir.path().join("claude-bridge.py");
        std::fs::write(
            &entrypoint,
            r#"import json, sys, os
root = os.path.dirname(__file__)
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); result = {}
    with open(root + '/claude-requests-jsonl', 'a') as log: log.write(json.dumps(r) + '\n')
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#,
        )
        .unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: self.dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(tokio::sync::Mutex::new(
            wonder_app_server::AppServerClient::unavailable(
                "test".into(),
                crate::ingestion::notification_sink(self.state.store.clone()),
            ),
        ));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        self.state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
    }

    async fn new() -> Self {
        Self::with_family(AgentFamily::Codex).await
    }

    async fn with_family(family: AgentFamily) -> Self {
        let (dir, state) = ingestion::tests::fixture().await;
        let thread = match family {
            AgentFamily::Codex => "thread",
            AgentFamily::Claude => "claude-thread",
            AgentFamily::OpenCode => "opencode-thread",
            AgentFamily::Cursor => "cursor-thread",
        };
        let key = SigningKey::from_slice(&[11_u8; 32]).unwrap();
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
        let conversation = if family == AgentFamily::Codex {
            "d19b0f7e-97bb-45d7-8206-324a62386dd9"
        } else {
            let original = state
                .store
                .project_conversation("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()
                .unwrap();
            state
                .store
                .create_project_conversation(wonder_store::ProjectConversationInsert {
                    conversation_id: "phone-provider",
                    project_id: &original.project_id,
                    family,
                    provider_store: crate::providers::provider_store(&state, family),
                    native_session_id: None,
                    cwd: &original.cwd,
                    roots_revision: original.roots_revision,
                    title: "Phone approval",
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
            "phone-provider"
        };
        crate::test_support::bind_project_thread(&state, conversation, family, thread, None, "now")
            .await
            .unwrap();
        let MessageInsert::Inserted(message) = state
            .store
            .insert_message(
                "owner",
                "approval-owner",
                "Work",
                "hash",
                conversation,
                "now",
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        state
            .store
            .update_message_delivery(&message.id, "accepted_by_codex", Some(thread), Some("turn"))
            .await
            .unwrap();
        let health = state.projects.codex.lock().await.health();
        state
            .ingestion
            .register_provider(family, &state.projects.codex, health);
        Self {
            dir,
            state,
            key,
            thread,
        }
    }

    async fn seed(&self, id: &str, method: &str, mut params: Value) {
        params["threadId"] = json!(self.thread);
        params["turnId"] = json!("turn");
        params["itemId"] = json!(id);
        params["_wonderRuntimeId"] = json!(self.state.projects.codex.lock().await.health().id());
        params["_wonderRequestId"] = json!(id);
        self.state
            .store
            .insert_pending_approval(id, method, &params.to_string(), "now")
            .await
            .unwrap();
    }

    async fn body(&self, id: &str, decision: Value, response: Option<Value>) -> Value {
        let approval = self.state.store.approval(id).await.unwrap().unwrap();
        let mut body = json!({
            "decision": decision, "actionNonce": approval.action_nonce,
            "expectedState": "pending", "idempotencyKey": format!("resolve-{id}"),
            "issuedAtMs": now_ms(), "signature": ""
        });
        if let Some(response) = response {
            body["responseJson"] = json!(response.to_string());
        }
        self.sign(id, &mut body);
        body
    }

    fn sign(&self, id: &str, body: &mut Value) {
        let request: ApprovalResolution = serde_json::from_value(body.clone()).unwrap();
        let hash = hex::encode(Sha256::digest(canonical_approval_body(&request).as_bytes()));
        let path = format!("/api/v1/approvals/{id}/resolve");
        let transcript = ActionTranscript {
            action: "approval.resolve",
            target: &path,
            body_sha256: &hash,
            action_nonce: &request.action_nonce,
            session_binding: "phone-session",
            device_id: "owner",
            host_installation_id: &self.state.host_installation_id,
            issued_at_ms: request.issued_at_ms,
            expected_state: &request.expected_state,
        };
        let signature: Signature = self.key.sign(&transcript.to_bytes());
        body["signature"] =
            json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    }

    async fn post(&self, id: &str, body: Value) -> (StatusCode, String) {
        let app = Router::new()
            .route(
                "/api/v1/approvals/{approval_id}/resolve",
                post(resolve_approval),
            )
            .layer(Extension(AuthenticatedDevice {
                device_id: "owner".into(),
                session_binding: "phone-session".into(),
            }))
            .with_state(self.state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/approvals/{id}/resolve"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// A signed answer for the secret route (`save_requested_secret`).
    #[cfg(target_os = "macos")]
    async fn secret_body(&self, id: &str, decision: &str, value: Option<&str>) -> Value {
        let approval = self.state.store.approval(id).await.unwrap().unwrap();
        let mut body = json!({
            "decision": decision, "actionNonce": approval.action_nonce,
            "expectedState": "pending", "idempotencyKey": format!("secret-{id}"),
            "issuedAtMs": now_ms(), "signature": ""
        });
        if let Some(value) = value {
            body["value"] = json!(value);
        }
        self.sign_secret(id, &mut body);
        body
    }

    #[cfg(target_os = "macos")]
    fn sign_secret(&self, id: &str, body: &mut Value) {
        let answer: SecretAnswer = serde_json::from_value(body.clone()).unwrap();
        let hash = hex::encode(Sha256::digest(canonical_secret_body(&answer).as_bytes()));
        let path = format!("/api/v1/secret-requests/{id}");
        let transcript = ActionTranscript {
            action: "secret.save",
            target: &path,
            body_sha256: &hash,
            action_nonce: &answer.action_nonce,
            session_binding: "phone-session",
            device_id: "owner",
            host_installation_id: &self.state.host_installation_id,
            issued_at_ms: answer.issued_at_ms,
            expected_state: &answer.expected_state,
        };
        let signature: Signature = self.key.sign(&transcript.to_bytes());
        body["signature"] =
            json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    }

    #[cfg(target_os = "macos")]
    async fn expect_secret_status(&self, id: &str, body: Value, status: StatusCode) {
        let app = Router::new()
            .route(
                "/api/v1/secret-requests/{approval_id}",
                post(save_requested_secret),
            )
            .layer(Extension(AuthenticatedDevice {
                device_id: "owner".into(),
                session_binding: "phone-session".into(),
            }))
            .with_state(self.state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/secret-requests/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let actual = response.status();
        let text = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            actual,
            status,
            "secret {id}: {}",
            String::from_utf8_lossy(&text)
        );
    }

    async fn expect_status(&self, id: &str, body: Value, status: StatusCode) {
        let (actual, error) = self.post(id, body).await;
        assert_eq!(actual, status, "approval {id}: {error}");
    }

    async fn assert_forwarded(&self, id: &str, expected: Value) {
        self.assert_forwarded_request(id, id, expected).await;
    }

    async fn assert_forwarded_request(&self, id: &str, request_id: &str, expected: Value) {
        let stored = self.state.store.approval(id).await.unwrap().unwrap();
        assert_eq!(stored.state, "resolved");
        // The answer is forwarded, then forgotten: it may be a secret.
        assert_eq!(stored.resolution_json, None);
        assert!(stored.resolution_idempotency_key.is_some());
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let responses =
                    std::fs::read_to_string(self.dir.path().join("responses")).unwrap_or_default();
                let matches = responses
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|response| response["id"] == json!(request_id))
                    .collect::<Vec<_>>();
                if !matches.is_empty() {
                    assert_eq!(matches.len(), 1, "must forward each approval only once");
                    assert_eq!(matches[0]["result"], expected);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn assert_pending(&self, id: &str) {
        let stored = self.state.store.approval(id).await.unwrap().unwrap();
        assert_eq!(stored.state, "pending");
        assert!(stored.resolution_json.is_none());
    }

    async fn shutdown(self) {
        self.state
            .projects
            .codex
            .lock()
            .await
            .shutdown()
            .await
            .unwrap();
    }
}

// Contract: a terminal provider turn closes its own requests, including a
// response whose durable intent survived a failed write. Old phone actions
// cannot be forwarded, while other turns/transports remain actionable.
#[tokio::test]
async fn terminal_turn_retires_only_its_requests_and_refuses_stale_phone_actions() {
    for (status, handoff) in [
        ("completed", false),
        ("failed", false),
        ("interrupted", false),
        ("interrupted", true),
    ] {
        let fixture = PhoneFixture::new().await;
        let runtime = fixture
            .state
            .projects
            .codex
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        let methods = [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/permissions/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
            "item/tool/call",
        ];
        for (i, method) in methods.iter().enumerate() {
            fixture
                .seed(
                    &format!("request-{i}"),
                    method,
                    json!({"availableDecisions":["accept","decline"]}),
                )
                .await;
        }
        let stale = fixture.body("request-0", json!("accept"), None).await;
        fixture
            .state
            .store
            .begin_approval_resolution("request-1")
            .await
            .unwrap()
            .unwrap();
        fixture
            .state
            .store
            .persist_approval_resolution_intent(
                "request-1",
                "saved-key",
                "saved-hash",
                r#"{"decision":"accept"}"#,
            )
            .await
            .unwrap();
        for (id, rt, thread, turn) in [
            ("new-turn", runtime.as_str(), "thread", "new-turn"),
            ("other-thread", runtime.as_str(), "other-thread", "turn"),
            ("other-runtime", "other-runtime", "thread", "turn"),
        ] {
            fixture
                .state
                .store
                .insert_pending_approval(
                    id,
                    methods[0],
                    &json!({
                        "_wonderRuntimeId":rt, "threadId":thread, "turnId":turn, "itemId":id
                    })
                    .to_string(),
                    "now",
                )
                .await
                .unwrap();
        }
        if handoff {
            fixture
                .state
                .store
                .save_update_handoff(&wonder_store::UpdateHandoff {
                    thread_id: "thread".into(),
                    stopped_turn_id: "turn".into(),
                    request_id: "update".into(),
                    conversation_id: "d19b0f7e-97bb-45d7-8206-324a62386dd9".into(),
                    message_id: None,
                    resume_client_id: "continue".into(),
                    resume_params: "{}".into(),
                    turn_params: "{}".into(),
                    state: "paused".into(),
                    resumed_turn_id: None,
                })
                .await
                .unwrap();
        }
        let mut events = fixture.state.events.subscribe();
        let terminal = json!({"_wonderRuntimeId":runtime, "method":"turn/completed",
            "params":{"threadId":"thread", "turn":{"id":"turn","status":status}}});
        publish_app_server_notification(&fixture.state, terminal.clone()).await;
        if handoff {
            // The update path returns early and deliberately keeps its message
            // active for continuation; approval cleanup cannot depend on that.
            assert_eq!(
                fixture
                    .state
                    .store
                    .message_for_codex_thread_and_turn("thread", "turn")
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "accepted_by_codex"
            );
            assert_eq!(
                fixture
                    .state
                    .store
                    .update_handoff("thread", "turn")
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "paused"
            );
        }

        for i in 0..methods.len() {
            let approval = fixture
                .state
                .store
                .approval(&format!("request-{i}"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                approval.state, "resolved",
                "{status}: {}",
                approval.approval_id
            );
            assert_eq!(approval.decision.as_deref(), Some("turn_ended"));
        }
        let saved = fixture
            .state
            .store
            .approval("request-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.resolution_idempotency_key.as_deref(),
            Some("saved-key")
        );
        assert_eq!(saved.resolution_body_sha256.as_deref(), Some("saved-hash"));
        assert_eq!(saved.resolution_json, None);
        assert_eq!(
            fixture
                .state
                .store
                .list_pending_approvals()
                .await
                .unwrap()
                .len(),
            3
        );
        let mut resolved = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let WonderEvent::ApprovalResolved {
                request_id,
                decision,
            } = event.event
            {
                assert_eq!(decision, "turn_ended");
                assert_eq!(event.thread_id.as_deref(), Some("thread"));
                assert_eq!(event.turn_id.as_deref(), Some("turn"));
                assert_eq!(event.approval_id.as_deref(), Some(request_id.as_str()));
                resolved.push(request_id);
            }
        }
        assert_eq!(resolved.len(), methods.len());
        publish_app_server_notification(&fixture.state, terminal).await;
        assert!(
            events.try_recv().is_err(),
            "replaying completion must not repeat retirement"
        );
        fixture
            .expect_status("request-0", stale, StatusCode::CONFLICT)
            .await;
        assert!(
            !fixture.dir.path().join("responses").exists(),
            "stale acceptance reached the runtime"
        );
        fixture.shutdown().await;
    }
}

// An answer already admitted under the approval lock finishes before terminal
// cleanup. Its accepted decision must not be overwritten with turn_ended.
#[tokio::test]
async fn terminal_turn_waits_for_an_admitted_phone_answer() {
    let fixture = PhoneFixture::new().await;
    fixture
        .seed(
            "admitted",
            "item/commandExecution/requestApproval",
            json!({"availableDecisions":["accept","decline"]}),
        )
        .await;
    let body = fixture.body("admitted", json!("accept"), None).await;
    let runtime = fixture
        .state
        .projects
        .codex
        .lock()
        .await
        .health()
        .id()
        .to_owned();
    let pairing = fixture.state.pairing.lock().await;
    let mut resolving = Box::pin(fixture.post("admitted", body));
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            tokio::select! {
                response = &mut resolving => panic!("answer did not wait at signature verification: {response:?}"),
                approval = fixture.state.store.approval("admitted") => {
                    if approval.unwrap().unwrap().state == "resolving" { break; }
                }
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let mut terminal = Box::pin(publish_app_server_notification(
        &fixture.state,
        json!({
            "_wonderRuntimeId":runtime, "method":"turn/completed",
            "params":{"threadId":"thread", "turn":{"id":"turn","status":"interrupted"}}
        }),
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut terminal)
            .await
            .is_err()
    );
    drop(pairing);
    let (status, error) = resolving.await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{error}");
    tokio::time::timeout(Duration::from_secs(60), &mut terminal)
        .await
        .unwrap();
    drop(terminal);
    fixture
        .assert_forwarded("admitted", json!({"decision":"accept"}))
        .await;
    let approval = fixture
        .state
        .store
        .approval("admitted")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approval.decision.as_deref(), Some("accept"));
    fixture.shutdown().await;
}

#[test]
fn phone_summary_preserves_consent_constraints_without_runtime_secrets() {
    let schema = json!({
        "type": "object", "required": ["format", "amount"], "additionalProperties": false,
        "properties": {
            "format": {"type": "string", "oneOf": [{"const": "png", "title": "PNG"}, {"const": "jpeg", "title": "JPEG"}]},
            "amount": {"type": "integer", "minimum": 1, "maximum": 4, "default": 2},
            "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "minItems": 1, "maxItems": 2},
            "name": {"type": "string", "minLength": 2, "maxLength": 40, "pattern": "^[a-z]+$"}
        }
    });
    let permissions = json!({"fileSystem": {"read": ["/approved"], "globScanMaxDepth": 3}, "network": {"enabled": true}});
    let decision =
        json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git", "status"]}});
    let params = json!({
        "requestedSchema": schema, "mode": "form", "message": "Choose an export",
        "url": "https://service.example/approve", "grantRoot": "/approved",
        "permissions": permissions, "additionalPermissions": permissions,
        "availableDecisions": ["acceptForSession", decision],
        "networkApprovalContext": {"host": "service.example", "protocol": "https", "secret": "omit"},
        "_wonderRuntimeId": "private-runtime", "_wonderRequestId": 42, "token": "omit"
    });
    let summary = redact_approval_params("mcpServer/elicitation/request", &params);
    for key in [
        "requestedSchema",
        "mode",
        "message",
        "url",
        "grantRoot",
        "permissions",
        "additionalPermissions",
        "availableDecisions",
    ] {
        assert_eq!(
            summary[key], params[key],
            "consent field {key} must survive"
        );
    }
    assert_eq!(
        summary["networkApprovalContext"],
        json!({"host": "service.example", "protocol": "https"})
    );
    for key in ["_wonderRuntimeId", "_wonderRequestId", "token"] {
        assert!(summary.get(key).is_none());
    }
    test_support::assert_http_contract(
        "approvalSummary",
        &json!({
            "approvalId": "phone-form", "conversationId": "d19b0f7e-97bb-45d7-8206-324a62386dd9", "method": "mcpServer/elicitation/request",
            "params": summary, "actionNonce": "nonce", "resolutionIdempotencyKey": null
        }),
    );
    let oversized = json!({"type": "object", "description": "x".repeat(64 * 1024)});
    assert!(redact_approval_params(
        "mcpServer/elicitation/request",
        &json!({"requestedSchema": oversized})
    )
    .get("requestedSchema")
    .is_none());

    let native = json!({"serverName":"cua_repl", "mode":"form", "requestedSchema":{"type":"object","properties":{}},
        "_meta":{"codex_approval_kind":"mcp_tool_call", "riskLevel":"low", "subtitle":"Read the selected app.",
            "persist":["session","always"], "tool_params_display":[{"display_name":"App","value":"Calculator"}],
            "tool_params":{"secret":"omit"}, "x-codex-turn-metadata":{"session_id":"private"}}});
    let summary = redact_approval_params("mcpServer/elicitation/request", &native);
    assert_eq!(
        summary["elicitationContext"],
        json!({"isComputerUse":true, "riskLevel":"low",
        "subtitle":"Read the selected app.", "details":["App: Calculator"]})
    );
    assert!(summary.get("_meta").is_none());
    assert!(!summary.to_string().contains("private"));
    assert!(!summary.to_string().contains("secret"));
    test_support::assert_http_contract(
        "approvalSummary",
        &json!({"approvalId":"native", "conversationId":"d19b0f7e-97bb-45d7-8206-324a62386dd9",
        "method":"mcpServer/elicitation/request", "params":summary, "actionNonce":"nonce", "resolutionIdempotencyKey":null}),
    );
}

#[tokio::test]
async fn phone_forwards_advertised_session_and_structured_command_decisions() {
    let fixture = PhoneFixture::new().await;
    let decisions = [
        json!("acceptForSession"),
        json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git", "status"]}}),
        json!({"applyNetworkPolicyAmendment": {"network_policy_amendment": {"host": "service.example", "action": "allow"}}}),
    ];
    for (method_index, method) in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ]
    .iter()
    .enumerate()
    {
        for (decision_index, decision) in decisions.iter().enumerate() {
            let id = format!("command-{method_index}-{decision_index}");
            fixture
                .seed(
                    &id,
                    method,
                    json!({"availableDecisions": [decision, "decline"]}),
                )
                .await;
            let body = fixture.body(&id, decision.clone(), None).await;
            fixture
                .expect_status(&id, body, StatusCode::NO_CONTENT)
                .await;
            fixture
                .assert_forwarded(&id, json!({"decision": decision}))
                .await;
        }
    }
    fixture
        .seed(
            "not-advertised",
            "item/commandExecution/requestApproval",
            json!({"availableDecisions": ["accept", "decline"]}),
        )
        .await;
    let body = fixture
        .body("not-advertised", json!("acceptForSession"), None)
        .await;
    fixture
        .expect_status("not-advertised", body, StatusCode::BAD_REQUEST)
        .await;
    fixture.assert_pending("not-advertised").await;
    fixture.shutdown().await;
}

// Contract: signed remembered choices retain the exact provider option, with
// the existing nonce, signature, replay and concurrent-resolution protections.
#[tokio::test]
async fn phone_opencode_remembered_choices_are_exact_signed_and_provider_bound() {
    for (permission_key, owner) in [
        ("opencodePermission", AgentFamily::OpenCode),
        ("acpPermission", AgentFamily::Cursor),
    ] {
        for family in AgentFamily::ALL {
            let fixture = PhoneFixture::with_family(family).await;
            for (index, method) in [
                "item/commandExecution/requestApproval",
                "item/fileChange/requestApproval",
            ]
            .iter()
            .enumerate()
            {
                for kind in ["allow_always", "reject_always"] {
                    let id = format!("remember-{index}-{kind}");
                    let decision =
                        json!({(permission_key):{"optionId":"opaque/option:α", "kind":kind}});
                    fixture
                    .seed(
                        &id,
                        method,
                        json!({"agentFamily":owner.as_str(), "availableDecisions":[decision, "cancel"]}),
                    )
                    .await;
                    let body = fixture.body(&id, decision.clone(), None).await;
                    if family != owner {
                        fixture
                            .expect_status(&id, body, StatusCode::BAD_REQUEST)
                            .await;
                        fixture.assert_pending(&id).await;
                        continue;
                    }
                    let mut tampered = body.clone();
                    tampered["decision"][permission_key]["kind"] =
                        json!(if kind == "allow_always" {
                            "reject_always"
                        } else {
                            "allow_always"
                        });
                    fixture
                        .expect_status(&id, tampered, StatusCode::BAD_REQUEST)
                        .await;
                    fixture.assert_pending(&id).await;
                    let mut bad_signature = body.clone();
                    bad_signature["signature"] = json!("invalid");
                    fixture
                        .expect_status(&id, bad_signature, StatusCode::UNAUTHORIZED)
                        .await;
                    let mut stale_nonce = body.clone();
                    stale_nonce["actionNonce"] = json!("other-request");
                    fixture.sign(&id, &mut stale_nonce);
                    fixture
                        .expect_status(&id, stale_nonce, StatusCode::PRECONDITION_FAILED)
                        .await;
                    let (a, b) = tokio::join!(
                        fixture.post(&id, body.clone()),
                        fixture.post(&id, body.clone())
                    );
                    assert!(
                        a.0 == StatusCode::NO_CONTENT || b.0 == StatusCode::NO_CONTENT,
                        "{a:?} {b:?}"
                    );
                    assert!([StatusCode::NO_CONTENT, StatusCode::CONFLICT].contains(&a.0));
                    assert!([StatusCode::NO_CONTENT, StatusCode::CONFLICT].contains(&b.0));
                    fixture.expect_status(&id, body, StatusCode::CONFLICT).await;
                    fixture
                        .assert_forwarded(&id, json!({"decision":decision}))
                        .await;
                }
            }
            if family == owner {
                let allow = json!({(permission_key):{"optionId":"allow", "kind":"allow_always"}});
                let deny = json!({(permission_key):{"optionId":"deny", "kind":"reject_always"}});
                fixture
                    .seed(
                        "signed-exact-option",
                        "item/commandExecution/requestApproval",
                        json!({"availableDecisions":[allow, deny]}),
                    )
                    .await;
                let mut body = fixture.body("signed-exact-option", deny, None).await;
                body["decision"] = allow;
                fixture
                    .expect_status("signed-exact-option", body, StatusCode::UNAUTHORIZED)
                    .await;
                fixture.assert_pending("signed-exact-option").await;
            }
            fixture.shutdown().await;
        }
    }
}

#[tokio::test]
async fn phone_rejects_unadvertised_and_malformed_opencode_rules() {
    for (permission_key, owner) in [
        ("opencodePermission", AgentFamily::OpenCode),
        ("acpPermission", AgentFamily::Cursor),
    ] {
        let fixture = PhoneFixture::with_family(owner).await;
        let advertised = json!({(permission_key):{"optionId":"exact", "kind":"reject_always"}});
        let invalid = [
            json!({(permission_key):{"optionId":"forged", "kind":"allow_always"}}),
            json!({(permission_key):{"optionId":"exact", "kind":"allow_always"}}),
            json!({(permission_key):{"optionId":"exact"}}),
            json!({(permission_key):{"optionId":"", "kind":"allow_always"}}),
            json!({(permission_key):{"optionId":"exact", "kind":"unknown"}}),
            json!({(permission_key):{"optionId":"exact", "kind":"reject_always", "permissions":true}}),
        ];
        for (index, decision) in invalid.iter().enumerate() {
            let id = format!("forged-rule-{index}");
            fixture
                .seed(
                    &id,
                    "item/commandExecution/requestApproval",
                    json!({"availableDecisions":[advertised]}),
                )
                .await;
            let body = fixture.body(&id, decision.clone(), None).await;
            fixture
                .expect_status(&id, body, StatusCode::BAD_REQUEST)
                .await;
            fixture.assert_pending(&id).await;
            // Even malformed shapes explicitly advertised by a peer fail closed.
            if index >= 2 {
                let id = format!("malformed-rule-{index}");
                fixture
                    .seed(
                        &id,
                        "item/fileChange/requestApproval",
                        json!({"availableDecisions":[decision]}),
                    )
                    .await;
                let body = fixture.body(&id, decision.clone(), None).await;
                fixture
                    .expect_status(&id, body, StatusCode::BAD_REQUEST)
                    .await;
                fixture.assert_pending(&id).await;
            }
        }
        fixture
            .seed(
                "missing-advertisement",
                "item/commandExecution/requestApproval",
                json!({}),
            )
            .await;
        let body = fixture
            .body("missing-advertisement", advertised, None)
            .await;
        fixture
            .expect_status("missing-advertisement", body, StatusCode::BAD_REQUEST)
            .await;
        fixture.assert_pending("missing-advertisement").await;
        fixture.shutdown().await;
    }
}

#[test]
fn phone_canonical_signature_body_keeps_structured_decisions_and_utf8() {
    let request = ApprovalResolution {
        decision: json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["tool", "a/b", "café", "line\nbreak"]}}),
        action_nonce: "nonce".into(),
        expected_state: "pending".into(),
        idempotency_key: "key".into(),
        issued_at_ms: 123,
        signature: "not part of body".into(),
        response_json: Some(r#"{"city":"Montréal","count":2}"#.into()),
    };
    assert_eq!(
        canonical_approval_body(&request),
        r#"[{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["tool","a/b","café","line\nbreak"]}},"nonce","pending","key","{\"city\":\"Montréal\",\"count\":2}"]"#
    );
}

// Contract: removing a Project or its folder cannot overtake an admitted
// approval, then leave that approval forwarding work after removal returned.
#[tokio::test]
async fn project_removal_and_phone_approval_have_one_admission_order() {
    for change in ["exclude", "folders"] {
        let fixture = PhoneFixture::new().await;
        fixture
            .seed(
                "admitted",
                "item/commandExecution/requestApproval",
                json!({"availableDecisions":["accept","decline"]}),
            )
            .await;
        let body = fixture.body("admitted", json!("accept"), None).await;
        // Pause the real handler at signature verification, after it claims
        // the approval. No provider or filesystem delay is needed.
        let pairing = fixture.state.pairing.lock().await;
        let mut resolving = Box::pin(fixture.post("admitted", body));
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                tokio::select! {
                    response = &mut resolving => panic!("approval did not wait for signature verification: {response:?}"),
                    approval = fixture.state.store.approval("admitted") => {
                        if approval.unwrap().unwrap().state == "resolving" {
                            break;
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        let project = "project-d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let replacement = fixture.dir.path().join("replacement");
        std::fs::create_dir(&replacement).unwrap();
        let patch = if change == "exclude" {
            json!({"isIncluded":false})
        } else {
            json!({"folders":[replacement],"rootsRevision":1})
        };
        let path = format!("/api/v1/projects/{project}");
        let mut updating = Box::pin(test_support::call(&fixture.state, "PATCH", &path, patch));
        let overtook = tokio::time::timeout(Duration::from_millis(50), &mut updating)
            .await
            .ok();
        drop(pairing);
        let (status, error) = resolving.await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{error}");
        fixture
            .assert_forwarded("admitted", json!({"decision":"accept"}))
            .await;
        let did_overtake = overtook.is_some();
        let (status, error) = match overtook {
            Some(response) => response,
            None => (&mut updating).await,
        };
        drop(updating);
        assert_eq!(status, StatusCode::OK, "{error}");
        fixture
            .seed(
                "after-removal",
                "item/commandExecution/requestApproval",
                json!({"availableDecisions":["accept","decline"]}),
            )
            .await;
        let body = fixture.body("after-removal", json!("accept"), None).await;
        fixture
            .expect_status("after-removal", body, StatusCode::CONFLICT)
            .await;
        fixture.assert_pending("after-removal").await;
        fixture.shutdown().await;
        assert!(!did_overtake, "{change} overtook an admitted approval");
    }
}

#[tokio::test]
async fn phone_permission_grants_stay_within_requested_paths_network_and_depth() {
    let fixture = PhoneFixture::new().await;
    let requested = json!({"fileSystem": {
        "read": ["/approved"], "write": ["/approved/output"], "globScanMaxDepth": 3,
        "entries": [{"access": "read", "path": {"type": "glob_pattern", "pattern": "/approved/*.txt"}}]
    }, "network": {"enabled": false}});
    fixture
        .seed(
            "permissions",
            "item/permissions/requestApproval",
            json!({"permissions": requested}),
        )
        .await;
    for permissions in [
        json!({"fileSystem": {"read": ["/other"]}}),
        json!({"fileSystem": {"write": ["/approved"]}}),
        json!({"fileSystem": {"globScanMaxDepth": 4}}),
        json!({"fileSystem": {"entries": [{"access": "write", "path": {"type": "glob_pattern", "pattern": "/approved/*.txt"}}]}}),
        json!({"network": {"enabled": true}}),
    ] {
        let body = fixture
            .body(
                "permissions",
                json!("accept"),
                Some(json!({"permissions": permissions, "scope": "turn"})),
            )
            .await;
        fixture
            .expect_status("permissions", body, StatusCode::BAD_REQUEST)
            .await;
        fixture.assert_pending("permissions").await;
    }
    let granted = json!({"permissions": {"fileSystem": {"read": ["/approved"], "globScanMaxDepth": 2}}, "scope": "session"});
    let body = fixture
        .body(
            "permissions",
            json!("acceptForSession"),
            Some(granted.clone()),
        )
        .await;
    fixture
        .expect_status("permissions", body, StatusCode::NO_CONTENT)
        .await;
    fixture.assert_forwarded("permissions", granted).await;
    fixture
        .seed(
            "network-narrow",
            "item/permissions/requestApproval",
            json!({"permissions": {"network": {"enabled": true}}}),
        )
        .await;
    let narrowed = json!({"permissions": {"network": {"enabled": false}}, "scope": "turn"});
    let body = fixture
        .body("network-narrow", json!("accept"), Some(narrowed.clone()))
        .await;
    fixture
        .expect_status("network-narrow", body, StatusCode::NO_CONTENT)
        .await;
    fixture.assert_forwarded("network-narrow", narrowed).await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn phone_permission_decline_and_cancel_never_grant_access() {
    let fixture = PhoneFixture::new().await;
    for decision in ["decline", "cancel"] {
        fixture
            .seed(
                decision,
                "item/permissions/requestApproval",
                json!({"permissions": {"network": {"enabled": true}}}),
            )
            .await;
        for response in [
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"}),
            json!({"permissions": {}, "scope": "session"}),
            json!({"permissions": {}, "scope": "turn", "strictAutoReview": true}),
        ] {
            let body = fixture
                .body(decision, json!(decision), Some(response))
                .await;
            fixture
                .expect_status(decision, body, StatusCode::BAD_REQUEST)
                .await;
            fixture.assert_pending(decision).await;
        }
        let response = json!({"permissions": {}, "scope": "turn"});
        let body = fixture
            .body(decision, json!(decision), Some(response.clone()))
            .await;
        fixture
            .expect_status(decision, body, StatusCode::NO_CONTENT)
            .await;
        fixture.assert_forwarded(decision, response).await;
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn phone_mcp_form_and_url_requests_accept_decline_and_cancel() {
    let fixture = PhoneFixture::new().await;
    for mode in ["form", "url"] {
        for action in ["accept", "decline", "cancel"] {
            let id = format!("mcp-{mode}-{action}");
            fixture.seed(&id, "mcpServer/elicitation/request", json!({
                "serverName": "synthetic", "mode": mode, "url": "https://service.example/approve",
                "requestedSchema": {"type": "object", "required": ["count"], "properties": {"count": {"type": "integer", "minimum": 1}}}
            })).await;
            let mut response = json!({"action": action});
            if mode == "form" && action == "accept" {
                response["content"] = json!({"count": 2});
            }
            if action != "accept" {
                let body = fixture
                    .body(
                        &id,
                        json!(action),
                        Some(json!({"action": action, "content": {"count": 2}})),
                    )
                    .await;
                fixture
                    .expect_status(&id, body, StatusCode::BAD_REQUEST)
                    .await;
                fixture.assert_pending(&id).await;
                // Explicit null and omitted content are both valid non-disclosure responses.
                if mode == "url" {
                    response["content"] = Value::Null;
                }
            }
            let body = fixture
                .body(&id, json!(action), Some(response.clone()))
                .await;
            fixture
                .expect_status(&id, body, StatusCode::NO_CONTENT)
                .await;
            fixture.assert_forwarded(&id, response).await;
        }
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn phone_mcp_rejects_missing_mistyped_and_out_of_bounds_answers() {
    let fixture = PhoneFixture::new().await;
    let schema = json!({
        "type": "object", "required": ["count", "format", "enabled"], "additionalProperties": false,
        "properties": {
            "count": {"type": "integer", "minimum": 1, "maximum": 4},
            "format": {"type": "string", "enum": ["png", "jpeg"]},
            "enabled": {"type": "boolean"}
        }
    });
    for mode in ["form", "openai/form", "openaiForm"] {
        let id = mode.replace('/', "-");
        fixture
            .seed(
                &id,
                "mcpServer/elicitation/request",
                json!({"mode": mode, "serverName": "synthetic", "requestedSchema": schema}),
            )
            .await;
        for content in [
            json!({}),
            json!({"count": "2", "format": "png", "enabled": true}),
            json!({"count": 1.5, "format": "png", "enabled": true}),
            json!({"count": 0, "format": "png", "enabled": true}),
            json!({"count": 5, "format": "png", "enabled": true}),
            json!({"count": 2, "format": "gif", "enabled": true}),
            json!({"count": 2, "format": "png", "enabled": "true"}),
            json!({"count": 2, "format": "png", "enabled": true, "unrequested": "value"}),
        ] {
            let body = fixture
                .body(
                    &id,
                    json!("accept"),
                    Some(json!({"action": "accept", "content": content})),
                )
                .await;
            fixture
                .expect_status(&id, body, StatusCode::BAD_REQUEST)
                .await;
            fixture.assert_pending(&id).await;
        }
        let response =
            json!({"action": "accept", "content": {"count": 2, "format": "png", "enabled": false}});
        let body = fixture
            .body(&id, json!("accept"), Some(response.clone()))
            .await;
        fixture
            .expect_status(&id, body, StatusCode::NO_CONTENT)
            .await;
        fixture.assert_forwarded(&id, response).await;
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn phone_mcp_url_and_unsupported_forms_cannot_smuggle_answers() {
    let fixture = PhoneFixture::new().await;
    fixture.seed("url-content", "mcpServer/elicitation/request", json!({"mode": "url", "serverName": "synthetic", "url": "https://service.example/approve"})).await;
    let body = fixture
        .body(
            "url-content",
            json!("accept"),
            Some(json!({"action": "accept", "content": {"token": "not a form"}})),
        )
        .await;
    fixture
        .expect_status("url-content", body, StatusCode::BAD_REQUEST)
        .await;
    fixture.assert_pending("url-content").await;
    let response = json!({"action": "accept", "content": null});
    let body = fixture
        .body("url-content", json!("accept"), Some(response.clone()))
        .await;
    fixture
        .expect_status("url-content", body, StatusCode::NO_CONTENT)
        .await;
    fixture.assert_forwarded("url-content", response).await;
    for (id, params) in [
        ("missing-schema", json!({"mode": "form"})),
        (
            "external-metaschema",
            json!({"mode": "form", "requestedSchema": {"$schema": "https://untrusted.invalid/meta.json", "type": "object"}}),
        ),
        ("unknown-mode", json!({"mode": "future-verification"})),
        (
            "external-schema",
            json!({"mode": "form", "requestedSchema": {"type": "object", "$ref": "https://untrusted.invalid/schema.json"}}),
        ),
    ] {
        fixture
            .seed(id, "mcpServer/elicitation/request", params)
            .await;
        let body = fixture
            .body(
                id,
                json!("accept"),
                Some(json!({"action": "accept", "content": {}})),
            )
            .await;
        fixture
            .expect_status(id, body, StatusCode::BAD_REQUEST)
            .await;
        fixture.assert_pending(id).await;
        let response = json!({"action": "decline"});
        let body = fixture
            .body(id, json!("decline"), Some(response.clone()))
            .await;
        fixture
            .expect_status(id, body, StatusCode::NO_CONTENT)
            .await;
        fixture.assert_forwarded(id, response).await;
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn phone_can_decline_an_unregistered_dynamic_tool_without_executing_it() {
    let fixture = PhoneFixture::new().await;
    fixture.seed("unregistered", "item/tool/call", json!({"tool": "unknown", "callId": "unknown-call", "arguments": {"secret": "not executed"}})).await;
    for response in [
        json!({"success": true, "contentItems": []}),
        json!({"success": false, "contentItems": [{"type": "inputText", "text": "untrusted"}]}),
    ] {
        let body = fixture
            .body("unregistered", json!("respond"), Some(response))
            .await;
        fixture
            .expect_status("unregistered", body, StatusCode::BAD_REQUEST)
            .await;
        fixture.assert_pending("unregistered").await;
    }
    let response = json!({"success": false, "contentItems": []});
    let body = fixture
        .body("unregistered", json!("respond"), Some(response.clone()))
        .await;
    fixture
        .expect_status("unregistered", body, StatusCode::NO_CONTENT)
        .await;
    fixture.assert_forwarded("unregistered", response).await;
    assert!(!fixture.dir.path().join("executions").exists());
    fixture.shutdown().await;
}

#[tokio::test]
async fn phone_signature_binds_form_content_and_replay_cannot_forward_twice() {
    let fixture = PhoneFixture::new().await;
    fixture
        .seed(
            "signed-form",
            "mcpServer/elicitation/request",
            json!({
                "serverName": "synthetic", "mode": "form",
                "requestedSchema": {"type": "object", "properties": {"choice": {"type": "string"}}}
            }),
        )
        .await;
    let response = json!({"action": "accept", "content": {"choice": "first"}});
    let body = fixture
        .body("signed-form", json!("accept"), Some(response.clone()))
        .await;
    let mut tampered = body.clone();
    tampered["responseJson"] =
        json!(json!({"action": "accept", "content": {"choice": "second"}}).to_string());
    fixture
        .expect_status("signed-form", tampered, StatusCode::UNAUTHORIZED)
        .await;
    fixture.assert_pending("signed-form").await;
    let mut wrong_nonce = body.clone();
    wrong_nonce["actionNonce"] = json!("different-nonce");
    fixture.sign("signed-form", &mut wrong_nonce);
    fixture
        .expect_status("signed-form", wrong_nonce, StatusCode::PRECONDITION_FAILED)
        .await;
    fixture.assert_pending("signed-form").await;
    let mut expired = body.clone();
    expired["issuedAtMs"] = json!(now_ms() - 120_000);
    fixture.sign("signed-form", &mut expired);
    fixture
        .expect_status("signed-form", expired, StatusCode::PRECONDITION_FAILED)
        .await;
    fixture.assert_pending("signed-form").await;
    assert!(!fixture.dir.path().join("responses").exists());
    fixture
        .expect_status("signed-form", body.clone(), StatusCode::NO_CONTENT)
        .await;
    fixture
        .expect_status("signed-form", body, StatusCode::CONFLICT)
        .await;
    fixture.assert_forwarded("signed-form", response).await;
    // The signature cannot authorize work after its Project owner is revoked.
    fixture
        .seed(
            "revoked-owner",
            "item/commandExecution/requestApproval",
            json!({"availableDecisions":["accept","decline"]}),
        )
        .await;
    fixture
        .state
        .store
        .update_project_metadata(
            "project-d19b0f7e-97bb-45d7-8206-324a62386dd9",
            None,
            Some(false),
            None,
            "later",
        )
        .await
        .unwrap();
    let body = fixture.body("revoked-owner", json!("accept"), None).await;
    fixture
        .expect_status("revoked-owner", body, StatusCode::CONFLICT)
        .await;
    fixture.assert_pending("revoked-owner").await;
    let sent = std::fs::read_to_string(fixture.dir.path().join("responses")).unwrap();
    assert!(!sent
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["id"] == "revoked-owner"));
    fixture.shutdown().await;
}

// Contract: only the actual Claude runtime accepts denial feedback. The signed
// bytes and durable receipt bind one exact instruction throughout recovery.
#[tokio::test]
async fn phone_denial_instruction_requires_native_claude_and_exact_signed_bytes() {
    for family in [
        AgentFamily::Claude,
        AgentFamily::Codex,
        AgentFamily::OpenCode,
    ] {
        let fixture = PhoneFixture::with_family(family).await;
        let params = json!({"kind":"command", "supportsDenyMessage":true,
            "agentFamily":"claude", "availableDecisions":["accept","decline","cancel"]});
        let runtime = fixture
            .state
            .projects
            .codex
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        let mut advertised = params.clone();
        advertised["threadId"] = json!(fixture.thread);
        advertised["turnId"] = json!("turn");
        advertised["itemId"] = json!("capability");
        assert!(process_app_server_notification(&fixture.state, json!({"_wonderRuntimeId":runtime,
            "id":"capability", "method":"item/commandExecution/requestApproval", "params":advertised}), true).await);
        let stored = fixture
            .state
            .store
            .approval(&format!("{runtime}:capability"))
            .await
            .unwrap()
            .unwrap();
        let advertised: Value = serde_json::from_str(&stored.params_json).unwrap();
        assert_eq!(
            advertised["supportsDenyMessage"],
            json!(family == AgentFamily::Claude)
        );
        assert_eq!(
            redact_approval_params(&stored.method, &advertised)["supportsDenyMessage"],
            json!(family == AgentFamily::Claude)
        );
        let feedback = "  Read café.txt instead.\nKeep it unchanged.  ";
        let response = json!({"decision":"decline", "message":feedback});
        fixture
            .seed(
                "feedback",
                "item/commandExecution/requestApproval",
                params.clone(),
            )
            .await;
        let body = fixture
            .body("feedback", json!("decline"), Some(response.clone()))
            .await;
        if family == AgentFamily::Claude {
            let mut tampered = body.clone();
            tampered["responseJson"] =
                json!(json!({"decision":"decline", "message":"changed"}).to_string());
            fixture
                .expect_status("feedback", tampered, StatusCode::UNAUTHORIZED)
                .await;
            fixture.assert_pending("feedback").await;
            fixture
                .state
                .store
                .begin_approval_resolution("feedback")
                .await
                .unwrap()
                .unwrap();
            let signed: ApprovalResolution = serde_json::from_value(body.clone()).unwrap();
            let hash = hex::encode(Sha256::digest(canonical_approval_body(&signed).as_bytes()));
            assert!(fixture
                .state
                .store
                .persist_approval_resolution_intent(
                    "feedback",
                    &signed.idempotency_key,
                    &hash,
                    &response.to_string()
                )
                .await
                .unwrap());
            let mut changed = body.clone();
            changed["responseJson"] =
                json!(json!({"decision":"decline", "message":"changed"}).to_string());
            fixture.sign("feedback", &mut changed);
            fixture
                .expect_status("feedback", changed, StatusCode::CONFLICT)
                .await;
            fixture
                .expect_status("feedback", body.clone(), StatusCode::NO_CONTENT)
                .await;
            fixture.assert_forwarded("feedback", response.clone()).await;
            fixture
                .expect_status("feedback", body, StatusCode::CONFLICT)
                .await;
        } else {
            fixture
                .expect_status("feedback", body, StatusCode::BAD_REQUEST)
                .await;
            fixture.assert_pending("feedback").await;
        }
        for (index, (method, decision, changes, response)) in [
            (
                "item/fileChange/requestApproval",
                "decline",
                json!({}),
                response.clone(),
            ),
            (
                "item/commandExecution/requestApproval",
                "accept",
                json!({}),
                json!({"decision":"accept", "message":feedback}),
            ),
            (
                "item/commandExecution/requestApproval",
                "cancel",
                json!({}),
                json!({"decision":"cancel", "message":feedback}),
            ),
            (
                "item/commandExecution/requestApproval",
                "decline",
                json!({"supportsDenyMessage":false}),
                response.clone(),
            ),
            (
                "item/commandExecution/requestApproval",
                "decline",
                json!({"kind":"tool"}),
                response.clone(),
            ),
            (
                "item/commandExecution/requestApproval",
                "decline",
                json!({}),
                json!({"decision":"decline", "message":" \n"}),
            ),
            (
                "item/commandExecution/requestApproval",
                "decline",
                json!({}),
                json!({"decision":"decline", "message":"é".repeat(1001)}),
            ),
            (
                "item/commandExecution/requestApproval",
                "decline",
                json!({}),
                json!({"decision":"decline", "message":false}),
            ),
            (
                "item/commandExecution/requestApproval",
                "decline",
                json!({}),
                json!({"decision":"decline", "updatedInput":{"command":"other"}}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("invalid-{index}");
            let mut params = params.clone();
            params
                .as_object_mut()
                .unwrap()
                .extend(changes.as_object().unwrap().clone());
            fixture.seed(&id, method, params).await;
            let body = fixture.body(&id, json!(decision), Some(response)).await;
            fixture
                .expect_status(&id, body, StatusCode::BAD_REQUEST)
                .await;
            fixture.assert_pending(&id).await;
        }
        fixture.shutdown().await;
    }
}

// A secret typed into a question reaches the provider once and is not kept
// on the Mac after the request resolves.
#[tokio::test]
async fn phone_secret_answer_is_forwarded_and_not_kept() {
    let fixture = PhoneFixture::new().await;
    fixture
        .seed(
            "secret-question",
            "item/tool/requestUserInput",
            json!({"isBlocking":true,"questions":[{"id":"token","header":"Token","question":"Paste the deploy token","isSecret":true}]}),
        )
        .await;
    let response = json!({"answers":{"token":{"answers":["sk-synthetic-5d1e7c"]}}});
    let body = fixture
        .body("secret-question", json!("respond"), Some(response.clone()))
        .await;
    fixture
        .expect_status("secret-question", body, StatusCode::NO_CONTENT)
        .await;
    fixture.assert_forwarded("secret-question", response).await;
    fixture.shutdown().await;
}

// The signed phone route, real fixture-service Keychain, reusable reference,
// delivery and names-only deletion. No dummy value is written to test files.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn requested_secret_reaches_only_its_destination() {
    let secret = uuid::Uuid::new_v4().to_string();
    for family in [AgentFamily::Codex, AgentFamily::Claude] {
        let fixture = PhoneFixture::with_family(family).await;
        let message = fixture
            .state
            .store
            .message_by_device_and_client_message_id("owner", "approval-owner")
            .await
            .unwrap()
            .unwrap();
        let owner = fixture
            .state
            .store
            .project_conversation(&message.conversation_id)
            .await
            .unwrap()
            .unwrap();
        let _cleanup =
            crate::secrets::TestItem::new(&fixture.state, &owner.project_id, "TEST_TOKEN");
        std::fs::create_dir_all(&owner.cwd).unwrap();
        let runtime = fixture
            .state
            .projects
            .codex
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        let args = json!({"name":"TEST_TOKEN","purpose":"Count a dummy token without showing it.","timeoutSeconds":60,"clientRequestId":uuid::Uuid::new_v4().to_string()});
        let call = {
            let (state, runtime, message, args) = (
                fixture.state.clone(),
                runtime.clone(),
                message.id.clone(),
                args.clone(),
            );
            tokio::spawn(async move {
                thread_tools::call_for_test(
                    &state,
                    &runtime,
                    &message,
                    thread_tools::REQUEST_SECRET,
                    args,
                )
                .await
            })
        };
        let request = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(request) = fixture
                    .state
                    .store
                    .list_pending_approvals()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|a| a.method == thread_tools::secret::METHOD)
                {
                    break request;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let id = &request.approval_id;
        let kept = fixture
            .body(id, json!("save"), Some(json!({"value":secret})))
            .await;
        fixture
            .expect_status(id, kept, StatusCode::BAD_REQUEST)
            .await;
        let body = fixture.secret_body(id, "save", Some(&secret)).await;
        let mut forged = body.clone();
        forged["value"] = json!("tampered");
        fixture
            .expect_secret_status(id, forged, StatusCode::UNAUTHORIZED)
            .await;
        fixture
            .expect_secret_status(id, body.clone(), StatusCode::NO_CONTENT)
            .await;
        let result = tokio::time::timeout(Duration::from_secs(10), call)
            .await
            .unwrap()
            .unwrap();
        assert!(!result.to_string().contains(&secret));
        let info = crate::secrets::existing(
            &fixture.state,
            &owner.project_id,
            "TEST_TOKEN",
            &owner.conversation_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.to_string().contains(&info.secret_ref));
        assert!(info.secret_ref.starts_with("secretRef:"));
        fixture
            .expect_secret_status(id, body, StatusCode::CONFLICT)
            .await;
        let reuse = thread_tools::call_for_test(
            &fixture.state,
            &runtime,
            &message.id,
            thread_tools::REQUEST_SECRET,
            args,
        )
        .await;
        assert!(reuse.to_string().contains(&info.secret_ref));
        assert!(fixture
            .state
            .store
            .approval(id)
            .await
            .unwrap()
            .unwrap()
            .resolution_json
            .is_none());
        assert!(crate::secrets::existing(
            &fixture.state,
            &owner.project_id,
            "TEST_TOKEN",
            "another-chat"
        )
        .await
        .unwrap()
        .is_none());
        assert!(crate::secrets::run(
            &fixture.state,
            "another-project",
            &owner.conversation_id,
            crate::secrets::CommandRequest {
                reference: &info.secret_ref,
                delivery: "file",
                variable: "TOKEN_FILE",
                command: "wc -c < \"$TOKEN_FILE\"",
                seconds: 5
            }
        )
        .await
        .is_err());
        fixture
            .state
            .store
            .update_project_conversation(
                &owner.conversation_id,
                wonder_store::ProjectConversationPatch {
                    access_mode: Some("full_access"),
                    ..Default::default()
                },
                "now",
            )
            .await
            .unwrap();
        let output = crate::secrets::run(
            &fixture.state,
            &owner.project_id,
            &owner.conversation_id,
            crate::secrets::CommandRequest {
                reference: &info.secret_ref,
                delivery: "file",
                variable: "TOKEN_FILE",
                command: "wc -c < \"$TOKEN_FILE\"",
                seconds: 5,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            output["stdout"].as_str().unwrap().trim(),
            secret.len().to_string()
        );
        let echoed = crate::secrets::run(
            &fixture.state,
            &owner.project_id,
            &owner.conversation_id,
            crate::secrets::CommandRequest {
                reference: &info.secret_ref,
                delivery: "env",
                variable: "TOKEN",
                command: "printf '%s' \"$TOKEN\"",
                seconds: 5,
            },
        )
        .await
        .unwrap();
        assert_eq!(echoed["stdout"], "[secret:TEST_TOKEN]");
        let response =
            crate::secrets::list(State(fixture.state.clone()), Extension(OwnerAuthority)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let listed = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(!listed.windows(secret.len()).any(|w| w == secret.as_bytes()));
        let list: Value = serde_json::from_slice(&listed).unwrap();
        assert_eq!(list["secrets"][0]["name"], "TEST_TOKEN");
        let delete = serde_json::from_value(
            json!({"projectId":owner.project_id,"secretRef":info.secret_ref}),
        )
        .unwrap();
        assert_eq!(
            crate::secrets::delete(
                State(fixture.state.clone()),
                Extension(OwnerAuthority),
                Json(delete)
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        assert!(crate::secrets::existing(
            &fixture.state,
            &owner.project_id,
            "TEST_TOKEN",
            &owner.conversation_id
        )
        .await
        .unwrap()
        .is_none());
        assert!(crate::secrets::run(
            &fixture.state,
            &owner.project_id,
            &owner.conversation_id,
            crate::secrets::CommandRequest {
                reference: &info.secret_ref,
                delivery: "env",
                variable: "TOKEN",
                command: "true",
                seconds: 5
            }
        )
        .await
        .is_err());
        for suffix in ["", "-wal"] {
            let bytes = std::fs::read(fixture.dir.path().join(format!("state.db{suffix}")))
                .unwrap_or_default();
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "no plaintext may enter SQLite or its WAL"
            );
        }
        fixture.shutdown().await;
    }
}

// Native question answers keep the exact signed text; runtime labels cannot
// advertise a notification reply on a provider that has no question protocol.
#[tokio::test]
async fn phone_question_reply_uses_native_route_and_immutable_signed_answer() {
    for family in [
        AgentFamily::Codex,
        AgentFamily::Claude,
        AgentFamily::OpenCode,
    ] {
        let fixture = PhoneFixture::with_family(family).await;
        let runtime = fixture
            .state
            .projects
            .codex
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        let params = json!({"agentFamily":"claude", "_wonderSupportsQuestionReply":true,
            "threadId":fixture.thread,"turnId":"turn","itemId":"question","isBlocking":true,
            "questions":[{"id":"q:0","header":"Tone","question":"Which tone?","isOther":true,"isSecret":false}]});
        assert!(process_app_server_notification(&fixture.state,
            json!({"_wonderRuntimeId":runtime,"id":"question","method":"item/tool/requestUserInput","params":params}), true).await);
        let id = format!("{runtime}:question");
        let stored = fixture.state.store.approval(&id).await.unwrap().unwrap();
        let params: Value = serde_json::from_str(&stored.params_json).unwrap();
        let summary = redact_approval_params(&stored.method, &params);
        test_support::assert_http_contract(
            "approvalSummary",
            &json!({"approvalId":id,
            "conversationId":"phone-question", "method":stored.method, "params":summary,
            "actionNonce":stored.action_nonce, "resolutionIdempotencyKey":null}),
        );
        assert_eq!(
            summary["notificationQuestion"].is_object(),
            family != AgentFamily::OpenCode
        );
        assert!(summary.get("_wonderSupportsQuestionReply").is_none());
        if family == AgentFamily::OpenCode {
            fixture.shutdown().await;
            continue;
        }
        assert_eq!(
            summary["notificationQuestion"],
            json!({"id":"q:0","body":"Tone\nWhich tone?"})
        );
        let response = json!({"answers":{"q:0":{"answers":["  Café, please.\nKeep the accents — thanks.  "]}}});
        let body = fixture
            .body(&id, json!("respond"), Some(response.clone()))
            .await;
        let mut tampered = body.clone();
        tampered["responseJson"] =
            json!(json!({"answers":{"q:0":{"answers":["Changed"]}}}).to_string());
        fixture
            .expect_status(&id, tampered.clone(), StatusCode::UNAUTHORIZED)
            .await;
        fixture.assert_pending(&id).await;
        fixture
            .state
            .store
            .begin_approval_resolution(&id)
            .await
            .unwrap()
            .unwrap();
        let signed: ApprovalResolution = serde_json::from_value(body.clone()).unwrap();
        let hash = hex::encode(Sha256::digest(canonical_approval_body(&signed).as_bytes()));
        assert!(fixture
            .state
            .store
            .persist_approval_resolution_intent(
                &id,
                &signed.idempotency_key,
                &hash,
                &response.to_string()
            )
            .await
            .unwrap());
        fixture.sign(&id, &mut tampered);
        fixture
            .expect_status(&id, tampered, StatusCode::CONFLICT)
            .await;
        fixture
            .expect_status(&id, body, StatusCode::NO_CONTENT)
            .await;
        fixture
            .assert_forwarded_request(&id, "question", response)
            .await;
        fixture.shutdown().await;
    }
}

// ACP cancellation is a notification. Its bridge must confirm the targeted
// prompt's outcome before the phone can treat Stop as successful.
#[tokio::test]
async fn acp_stop_requires_native_confirmation_and_keeps_terminal_event_authority() {
    for (reply, expected) in [
        (json!({"result":{}}), None),
        (
            json!({"result":{"turn":{"id":"another-turn","status":"interrupted"}}}),
            None,
        ),
        (
            json!({"result":{"turn":{"id":"turn","status":"inProgress"}}}),
            None,
        ),
        (
            json!({"error":{"code":-32000,"message":"OpenCode has not confirmed that this response stopped."}}),
            None,
        ),
        (
            json!({"result":{"turn":{"id":"turn","status":"interrupted"}}}),
            Some(DeliveryState::Interrupted),
        ),
        (
            json!({"result":{"turn":{"id":"turn","status":"completed"}}}),
            Some(DeliveryState::Completed),
        ),
        (
            json!({"result":{"turn":{"id":"turn","status":"failed"}}}),
            Some(DeliveryState::Failed),
        ),
    ] {
        let mut fixture = PhoneFixture::with_family(AgentFamily::OpenCode).await;
        fixture.state.opencode = Some(Arc::new(crate::claude::Runtime {
            client: fixture.state.projects.codex.clone(),
            config: wonder_app_server::BridgeLaunchConfig {
                node_bin: "/usr/bin/python3".into(),
                entrypoint: fixture.dir.path().join("runtime.py"),
                state_dir: fixture.dir.path().into(),
                npm_cli: None,
                claude_config_dir: None,
                wonder_version: "test".into(),
                family: "opencode",
            },
        }));
        let message = fixture
            .state
            .store
            .message_for_codex_turn("turn")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(
            fixture.dir.path().join("interrupt-response.json"),
            reply.to_string(),
        )
        .unwrap();
        let response = interrupt_turn(
            State(fixture.state.clone()),
            Path((message.conversation_id.clone(), "turn".into())),
        )
        .await;
        assert_eq!(
            response.status(),
            if expected.is_some() {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::BAD_GATEWAY
            },
            "{reply}"
        );
        if reply.get("error").is_some() {
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("OpenCode has not confirmed"));
        }
        assert_eq!(
            fixture
                .state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "accepted_by_codex",
            "Stop must not invent an interruption before native terminal ingestion"
        );
        if let Some(expected) = expected {
            let runtime = fixture
                .state
                .projects
                .codex
                .lock()
                .await
                .health()
                .id()
                .to_owned();
            publish_app_server_notification(&fixture.state, json!({
                "_wonderRuntimeId": runtime, "method":"turn/completed",
                "params":{"threadId":fixture.thread,"turn":{"id":"turn","status":reply["result"]["turn"]["status"]}}
            })).await;
            // Delivery receipts settle as completed; the durable timeline
            // event carries the native response outcome shown to the phone.
            let events = fixture
                .state
                .store
                .events_for_conversation(&message.conversation_id)
                .await
                .unwrap();
            let terminal = events.iter().rev().find_map(|event| match &event.event {
                WonderEvent::MessageState { state } if event.turn_id.as_deref() == Some("turn") => {
                    Some(state)
                }
                _ => None,
            });
            assert_eq!(terminal, Some(&expected));
        }
        fixture.shutdown().await;
    }
}

// Contract: OpenCode has no Send now (`steer: false`). A Guide for its running
// reply never reaches the runtime as a steer; it waits in the queue and is
// sent when the reply ends, instead of becoming an uncertain delivery.
#[tokio::test]
async fn opencode_guide_waits_in_the_queue_without_steering() {
    let mut fixture = PhoneFixture::with_family(AgentFamily::OpenCode).await;
    fixture.state.opencode = Some(Arc::new(crate::claude::Runtime {
        client: fixture.state.projects.codex.clone(),
        config: wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint: fixture.dir.path().join("runtime.py"),
            state_dir: fixture.dir.path().into(),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "opencode",
        },
    }));
    let MessageInsert::Inserted(guide) = fixture
        .state
        .store
        .insert_guide_message(
            "owner",
            "guide",
            "change direction",
            "hash-guide",
            "phone-provider",
            &[],
            "now",
            "turn",
        )
        .await
        .unwrap()
    else {
        panic!("guide")
    };
    let response = dispatch_guide(fixture.state.clone(), guide.clone(), "turn".into()).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let receipt: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(receipt["queued"], true, "it waits behind the running reply");
    let requests = std::fs::read_to_string(fixture.dir.path().join("requests")).unwrap_or_default();
    assert!(!requests.lines().any(|method| method == "turn/steer"));
    let saved = fixture
        .state
        .store
        .message_by_id(&guide.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.body, "change direction");
    fixture.shutdown().await;
}

/// What happens next to a question T3 Code's agent asks (`t3_code::questions`).
#[derive(Clone, Copy, Debug)]
enum T3Step {
    /// The agent asks; T3 Code lists the question.
    Asks,
    /// Cupola reads the T3 thread; the turn still runs.
    Reads,
    /// The owner answers on the phone, and Cupola says this.
    Phone(StatusCode),
    /// The same, but T3 Code's reply to the answer is lost.
    PhoneLost(StatusCode),
    /// The owner answers in T3 Code on the computer.
    Computer,
    /// T3 Code's turn ends; Cupola's next read stops watching.
    TurnEnds,
    /// Cupola's sign-in ends: a read fails, then the sign-in renews.
    SignInRenews,
    /// A provider runtime restarts (`ingestion::retire_lost_approvals`).
    RuntimeRestarts,
    /// The watch stops before the turn ends (it gave up, or T3 Code no
    /// longer accepts Cupola), with T3 Code reachable or not.
    WatchStops { reachable: bool },
    /// The host restarts (`ingestion::recheck_desktop_approvals`), with T3
    /// Code reachable or not.
    Restarts { reachable: bool },
}

// Contract (owner, Oct 22: "Approvals, yeah, I feel like we should be able
// to"): a question T3 Code's agent asks shows once on the phone, in its own
// conversation. Answered there, it reaches T3 Code once, as T3 Code takes it;
// answered on the computer, or left when the turn ends, it closes on the
// phone and a late phone answer is refused with why. A lost answer is read
// back, and a sign-in renewal while it waits changes nothing. One closed
// because T3 Code couldn't be read, by a stopped watch or a host restart,
// is offered again once T3 Code is read still asking it.
#[tokio::test]
async fn t3_code_questions_follow_t3_code_in_every_order() {
    use T3Step::*;
    const NO: StatusCode = StatusCode::NO_CONTENT;
    const LATE: StatusCode = StatusCode::CONFLICT;
    let rows: &[(&str, &[T3Step], &str, usize)] = &[
        (
            "answered on the phone",
            &[Asks, Reads, Reads, Phone(NO), Reads],
            "respond",
            1,
        ),
        (
            "answered on the computer",
            &[Asks, Reads, Computer, Reads, Phone(LATE)],
            "t3_code_resolved",
            0,
        ),
        (
            "answered on the computer first",
            &[Asks, Reads, Computer, Phone(LATE), Reads],
            "t3_code_resolved",
            1,
        ),
        (
            "the turn ends",
            &[Asks, Reads, TurnEnds, Phone(LATE)],
            "turn_ended",
            0,
        ),
        (
            "the answer's reply is lost",
            &[Asks, Reads, PhoneLost(NO), Reads],
            "respond",
            1,
        ),
        (
            "the sign-in renews meanwhile",
            &[Asks, Reads, SignInRenews, Reads, Phone(NO)],
            "respond",
            1,
        ),
        (
            "asked while the sign-in renews",
            &[SignInRenews, Asks, Reads, Phone(NO)],
            "respond",
            1,
        ),
        (
            "a provider runtime restarts while the question waits",
            &[Asks, Reads, RuntimeRestarts, Reads, Phone(NO)],
            "respond",
            1,
        ),
        (
            "the watch stops while T3 Code still asks",
            &[Asks, Reads, WatchStops { reachable: true }, Phone(NO)],
            "respond",
            1,
        ),
        (
            "the watch stops after it was answered on the computer",
            &[
                Asks,
                Reads,
                Computer,
                WatchStops { reachable: true },
                Phone(LATE),
            ],
            "t3_code_resolved",
            0,
        ),
        (
            "the watch stops while T3 Code can't be reached",
            &[Asks, Reads, WatchStops { reachable: false }, Phone(LATE)],
            "t3_code_on_computer",
            0,
        ),
        (
            "T3 Code can't be reached, then still asks",
            &[
                Asks,
                Reads,
                WatchStops { reachable: false },
                Reads,
                Phone(NO),
            ],
            "respond",
            1,
        ),
        (
            "the host restarts while T3 Code still asks",
            &[Asks, Reads, Restarts { reachable: true }, Phone(NO)],
            "respond",
            1,
        ),
        (
            "the host restarts after it was answered on the computer",
            &[
                Asks,
                Reads,
                Computer,
                Restarts { reachable: true },
                Phone(LATE),
            ],
            "t3_code_resolved",
            0,
        ),
        (
            "the host restarts while T3 Code can't be reached",
            &[Asks, Reads, Restarts { reachable: false }, Phone(LATE)],
            "t3_code_on_computer",
            0,
        ),
        (
            "the host restarts while T3 Code can't be reached, then it still asks",
            &[Asks, Reads, Restarts { reachable: false }, Reads, Phone(NO)],
            "respond",
            1,
        ),
    ];
    for ((consolidated, variant), (row, steps, decision, answers)) in
        [(false, 0), (true, 0), (true, 1), (true, 2), (true, 3)]
            .into_iter()
            .flat_map(|api| rows.iter().map(move |row| (api, row)))
    {
        let mut fixture = PhoneFixture::new().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let fake = crate::t3_code::tests::Fake::start().await;
        fake.consolidated_requests
            .store(consolidated, std::sync::atomic::Ordering::SeqCst);
        fake.missing_tool_variant
            .store(variant, std::sync::atomic::Ordering::SeqCst);
        fake.thread("t3-thread", "p", "codex", None, &[]);
        fake.queue_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let grants = Arc::new(crate::t3_code::MemoryGrantStore::default());
        let t3 = crate::t3_code::tests::one_tap(&fake, &grants);
        t3.connect().await.unwrap();
        fixture.state.t3_code = t3.clone();
        let key = crate::t3_code::questions::request_key("t3-thread", "req-1");
        let mut renewing = false;
        for step in *steps {
            match step {
                Asks => fake.questions.lock().unwrap().push(json!({
                    "threadId": "t3-thread", "requestId": "req-1", "questions": [
                    {"id": "tone", "header": "Tone", "question": "Which tone?", "options": [
                        {"label": "Brief", "description": "A line or two", "value": "brief"},
                        {"label": "Thorough", "description": "Every step"}]}]})),
                Reads => {
                    if renewing {
                        crate::t3_code::tests::until("renewed", || t3.connected()).await;
                        renewing = false;
                    }
                    let running =
                        crate::t3_code::questions::poll(&fixture.state, conversation, "t3-thread")
                            .await;
                    assert_eq!(running, Ok(true), "{row}: {step:?}");
                    if consolidated {
                        let before = fake.calls.lock().unwrap().len();
                        assert_eq!(
                            crate::t3_code::questions::poll(
                                &fixture.state,
                                conversation,
                                "t3-thread"
                            )
                            .await,
                            Ok(true)
                        );
                        assert!(
                            !fake.calls.lock().unwrap()[before..]
                                .iter()
                                .any(|(name, _)| name.starts_with("t3_pending_request_")),
                            "{row}: cached tool shape must avoid another missing-tool call"
                        );
                    }
                    // The phone lists it once, in its conversation, while T3 Code asks it.
                    let listed = list_approvals(State(fixture.state.clone())).await;
                    let listed: Value = serde_json::from_slice(
                        &to_bytes(listed.into_body(), 1 << 20).await.unwrap(),
                    )
                    .unwrap();
                    let shown: Vec<&Value> = listed
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| item["approvalId"] == key.as_str())
                        .collect();
                    let asked = !fake.questions.lock().unwrap().is_empty();
                    assert_eq!(shown.len(), usize::from(asked), "{row}: {step:?}");
                    if let Some(item) = shown.first() {
                        assert_eq!(item["conversationId"], conversation, "{row}");
                        assert_eq!(item["method"], "item/tool/requestUserInput", "{row}");
                        assert_eq!(item["params"]["questions"][0]["question"], "Which tone?");
                        assert_eq!(
                            item["params"]["questions"][0]["options"][0],
                            json!({"label": "Brief", "description": "A line or two"})
                        );
                    }
                }
                Phone(status) | PhoneLost(status) => {
                    if matches!(step, PhoneLost(_)) {
                        fake.lose_answer
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    let response = json!({"answers": {"tone": {"answers": ["Brief"]}}});
                    let body = fixture.body(&key, json!("respond"), Some(response)).await;
                    let (actual, said) = fixture.post(&key, body).await;
                    assert_eq!(actual, *status, "{row}: {said}");
                    // Closed by T3 Code only as the phone answered: the phone says why.
                    if *row == "answered on the computer first" {
                        assert_eq!(said, crate::t3_code::questions::GONE, "{row}");
                    }
                    if *decision == crate::t3_code::questions::ON_COMPUTER {
                        assert_eq!(said, crate::t3_code::questions::ANSWER_ON_COMPUTER, "{row}");
                    }
                }
                Computer => fake.questions.lock().unwrap().clear(),
                TurnEnds => {
                    fake.queue_sends
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    let running =
                        crate::t3_code::questions::poll(&fixture.state, conversation, "t3-thread")
                            .await;
                    assert_eq!(running, Ok(false), "{row}: the watch stops");
                }
                SignInRenews => {
                    crate::t3_code::tests::end_sign_in(&fake, &t3, &grants);
                    let failed =
                        crate::t3_code::questions::poll(&fixture.state, conversation, "t3-thread")
                            .await;
                    assert!(failed.is_err(), "{row}: {failed:?}");
                    t3.renew_if_due();
                    renewing = true;
                }
                RuntimeRestarts => {
                    crate::ingestion::retire_lost_approvals(&fixture.state)
                        .await
                        .unwrap();
                    let open = fixture.state.store.approval(&key).await.unwrap().unwrap();
                    assert_eq!(open.state, "pending", "{row}: still answerable");
                }
                WatchStops { reachable } | Restarts { reachable } => {
                    let failing = &fake.failing;
                    failing.store(!reachable, std::sync::atomic::Ordering::SeqCst);
                    if matches!(step, Restarts { .. }) {
                        crate::ingestion::recheck_desktop_approvals(&fixture.state).await;
                    } else {
                        crate::t3_code::questions::stopped(
                            &fixture.state,
                            conversation,
                            "t3-thread",
                        )
                        .await;
                    }
                    failing.store(false, std::sync::atomic::Ordering::SeqCst);
                    // What the phone lists is what it can still answer.
                    let listed = list_approvals(State(fixture.state.clone())).await;
                    let listed: Value = serde_json::from_slice(
                        &to_bytes(listed.into_body(), 1 << 20).await.unwrap(),
                    )
                    .unwrap();
                    let shown = listed
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["approvalId"] == key.as_str());
                    let asked = *reachable && !fake.questions.lock().unwrap().is_empty();
                    assert_eq!(shown, asked, "{row}");
                }
            }
        }
        let stored = fixture.state.store.approval(&key).await.unwrap();
        let stored = stored.unwrap_or_else(|| panic!("{row}: never shown"));
        assert_eq!(stored.state, "resolved", "{row}");
        assert_eq!(stored.decision.as_deref(), Some(*decision), "{row}");
        let sent = fake.answers.lock().unwrap().clone();
        assert_eq!(sent.len(), *answers, "{row}: {sent:?}");
        if *decision == "respond" {
            assert_eq!(
                sent[0],
                json!({"threadId": "t3-thread", "requestId": "req-1", "answers": {"tone": "brief"}}),
                "{row}"
            );
        }
        fixture.shutdown().await;
    }
}

// Contract: T3 Code's tools can't approve its permission requests, so while
// its turn waits on one the thread says it waits for a permission on the
// computer (also beside a question the phone can answer), until the turn ends.
#[tokio::test]
async fn a_t3_code_permission_waits_on_the_computer() {
    let fixture = PhoneFixture::new().await;
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    let fake = crate::t3_code::tests::Fake::start().await;
    fake.thread("t3-thread", "p", "codex", None, &[]);
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (t3, _) = crate::t3_code::tests::connected(&fake).await;
    let mut state = fixture.state.clone();
    state.t3_code = t3.clone();
    let poll = || crate::t3_code::questions::poll(&state, conversation, "t3-thread");
    // The phone's Chats row and header say "Waiting for a permission on your Mac".
    let stored = state
        .store
        .project_conversation(conversation)
        .await
        .unwrap()
        .unwrap();
    let listed = || async {
        serde_json::to_value(crate::projects::attached_summary(&state, &stored, 1, None).await)
            .unwrap()
    };
    assert_eq!(poll().await, Ok(true));
    assert!(!t3.approves_on_computer(conversation), "nothing waits");
    assert_eq!(listed().await.get("waitingOnMacPermission"), None);
    fake.permissions
        .store(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(poll().await, Ok(true));
    assert!(t3.approves_on_computer(conversation));
    let row = listed().await;
    assert_eq!(row["waitingOnMacPermission"], true);
    crate::test_support::assert_http_contract("projectThreadSummary", &row);
    fake.questions
        .lock()
        .unwrap()
        .push(json!({"threadId": "t3-thread", "requestId": "q",
        "questions": [{"id": "a", "header": "", "question": "Go on?", "options": []}]}));
    assert_eq!(poll().await, Ok(true));
    assert!(t3.approves_on_computer(conversation), "a question too");
    fake.permissions
        .store(0, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(poll().await, Ok(true));
    assert!(!t3.approves_on_computer(conversation), "only the question");
    fake.permissions
        .store(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(poll().await, Ok(true));
    fake.queue_sends
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(poll().await, Ok(false));
    assert!(!t3.approves_on_computer(conversation), "the turn ended");
    fixture.shutdown().await;
}

// Contract: a watch that stops before T3 Code's turn ends, because T3 Code
// stopped answering or Cupola was disconnected from it, leaves no question
// on the phone looking answerable: what it can't read back waits on the
// computer, and a late answer from the phone says so.
#[tokio::test]
async fn a_stopped_t3_code_watch_leaves_its_questions_to_the_computer() {
    for disconnects in [false, true] {
        let mut fixture = PhoneFixture::new().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let fake = crate::t3_code::tests::Fake::start().await;
        fake.thread("t3-thread", "p", "codex", None, &[]);
        fake.queue_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (t3, _) = crate::t3_code::tests::connected(&fake).await;
        fixture.state.t3_code = t3.clone();
        t3.questions.poll_every(std::time::Duration::from_millis(5));
        fake.questions.lock().unwrap().push(json!({
            "threadId": "t3-thread", "requestId": "req-1", "questions": [
            {"id": "tone", "header": "Tone", "question": "Which tone?", "options": []}]}));
        let key = crate::t3_code::questions::request_key("t3-thread", "req-1");
        assert_eq!(
            crate::t3_code::questions::poll(&fixture.state, conversation, "t3-thread").await,
            Ok(true)
        );
        if disconnects {
            t3.disconnect().await.unwrap();
        } else {
            fake.failing
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        crate::t3_code::questions::watch(&fixture.state, conversation, "t3-thread");
        let watching = || t3.questions.watching("t3-thread");
        crate::t3_code::tests::until("stopped", || !watching()).await;
        let closed = fixture.state.store.approval(&key).await.unwrap().unwrap();
        assert_eq!(closed.state, "resolved", "disconnects: {disconnects}");
        assert_eq!(
            closed.decision.as_deref(),
            Some(crate::t3_code::questions::ON_COMPUTER),
            "disconnects: {disconnects}"
        );
        let response = json!({"answers": {"tone": {"answers": ["Brief"]}}});
        let body = fixture.body(&key, json!("respond"), Some(response)).await;
        let (status, said) = fixture.post(&key, body).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(said, crate::t3_code::questions::ANSWER_ON_COMPUTER);
        assert!(fake.answers.lock().unwrap().is_empty());
        fixture.shutdown().await;
    }
}

// Contract: a question offered again after T3 Code couldn't be read is a
// new offer: an answer signed for the closed one never reaches T3 Code, and
// the phone's answer to the new one does, once.
#[tokio::test]
async fn a_reopened_t3_code_question_takes_only_an_answer_to_the_new_offer() {
    let mut fixture = PhoneFixture::new().await;
    let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    let fake = crate::t3_code::tests::Fake::start().await;
    fake.thread("t3-thread", "p", "codex", None, &[]);
    fake.queue_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (t3, _) = crate::t3_code::tests::connected(&fake).await;
    fixture.state.t3_code = t3.clone();
    fake.questions.lock().unwrap().push(json!({
        "threadId": "t3-thread", "requestId": "req-1", "questions": [
        {"id": "tone", "header": "Tone", "question": "Which tone?", "options": []}]}));
    let key = crate::t3_code::questions::request_key("t3-thread", "req-1");
    let poll = || crate::t3_code::questions::poll(&fixture.state, conversation, "t3-thread");
    assert_eq!(poll().await, Ok(true));
    let response = json!({"answers": {"tone": {"answers": ["Brief"]}}});
    let first = fixture.state.store.approval(&key).await.unwrap().unwrap();
    let stale = fixture
        .body(&key, json!("respond"), Some(response.clone()))
        .await;
    fake.failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    crate::t3_code::questions::stopped(&fixture.state, conversation, "t3-thread").await;
    fake.failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let closed = fixture.state.store.approval(&key).await.unwrap().unwrap();
    assert_eq!(
        closed.decision.as_deref(),
        Some(crate::t3_code::questions::ON_COMPUTER)
    );
    assert_eq!(poll().await, Ok(true));
    let reopened = fixture.state.store.approval(&key).await.unwrap().unwrap();
    assert_eq!(reopened.state, "pending");
    assert_ne!(reopened.action_nonce, first.action_nonce);
    let (status, said) = fixture.post(&key, stale).await;
    assert_ne!(status, StatusCode::NO_CONTENT, "{said}");
    assert!(fake.answers.lock().unwrap().is_empty());
    let mut fresh = fixture.body(&key, json!("respond"), Some(response)).await;
    fresh["idempotencyKey"] = json!("resolve-again");
    fixture.sign(&key, &mut fresh);
    let (status, said) = fixture.post(&key, fresh).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{said}");
    assert_eq!(fake.answers.lock().unwrap().len(), 1);
    fixture.shutdown().await;
}

/// What happens next to a request ChatGPT's desktop app asks in a Codex
/// thread it runs (`chatgpt_approvals`).
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
enum ChatGptStep {
    /// The turn asks for approval (request 6, or the next number).
    Asks,
    /// The external poll sees the turn running: Cupola follows the thread.
    Follows,
    /// Cupola has applied ChatGPT's newest state.
    Settles,
    /// The owner answers on the phone, and Cupola says this.
    Phone(StatusCode),
    /// The owner answers in ChatGPT on the computer.
    Computer,
    /// The turn asks while Cupola misses that change (a revision gap).
    AsksUnseen,
    /// ChatGPT sends every follower a new snapshot.
    Snapshot,
    /// ChatGPT drops Cupola's connection; ChatGPT itself stays.
    Disconnects,
    /// ChatGPT quits: its router is gone.
    Quits,
    /// ChatGPT is back, its turn still waiting.
    Returns,
    /// The turn ends: the external poll stops asking for the watch.
    TurnEnds,
}

// Contract (owner, Oct 22: "How do we not have approvals?"): a request a
// Codex turn running in ChatGPT's desktop app waits on shows once on the
// phone, in its conversation, while ChatGPT asks it. Answered there, it
// reaches ChatGPT once, as ChatGPT's own follower method takes it; answered
// in ChatGPT first, or left when the turn ends or ChatGPT goes away, it
// closes on the phone and a late answer is refused with why. A missed
// change, a new snapshot or a dropped connection never shows it twice or
// loses it, and one closed because ChatGPT went away is offered again once
// ChatGPT is followed again and still asks it.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn chatgpt_requests_follow_chatgpt_in_every_order() {
    use crate::codex_desktop_ipc::tests::FakeRouter;
    use ChatGptStep::*;
    const NO: StatusCode = StatusCode::NO_CONTENT;
    const LATE: StatusCode = StatusCode::CONFLICT;
    // (row, steps, final decision of request 6, answers ChatGPT received)
    let rows: &[(&str, &[ChatGptStep], &str, usize)] = &[
        (
            "answered on the phone",
            &[Asks, Follows, Settles, Phone(NO), Settles],
            "accept",
            1,
        ),
        (
            "asked while followed",
            &[Follows, Settles, Asks, Settles, Phone(NO), Settles],
            "accept",
            1,
        ),
        (
            "answered in ChatGPT first",
            &[Asks, Follows, Settles, Computer, Settles, Phone(LATE)],
            "chatgpt_resolved",
            0,
        ),
        (
            "a change Cupola missed",
            &[Follows, Settles, AsksUnseen, Snapshot, Settles, Phone(NO)],
            "accept",
            1,
        ),
        (
            "a gap is filled by asking again",
            &[
                Follows,
                Settles,
                AsksUnseen,
                Asks,
                Settles,
                Phone(NO),
                Settles,
            ],
            "accept",
            1,
        ),
        (
            "a new snapshot shows it once",
            &[Asks, Follows, Settles, Snapshot, Settles, Phone(NO)],
            "accept",
            1,
        ),
        (
            "the connection drops and Cupola follows again",
            &[Asks, Follows, Settles, Disconnects, Settles, Phone(NO)],
            "accept",
            1,
        ),
        (
            "the turn ends",
            &[Asks, Follows, Settles, TurnEnds, Phone(LATE)],
            "turn_ended",
            0,
        ),
        (
            "ChatGPT quits",
            &[Asks, Follows, Settles, Quits, Phone(LATE)],
            "chatgpt_on_computer",
            0,
        ),
        (
            "ChatGPT comes back and still asks",
            &[
                Asks,
                Follows,
                Settles,
                Quits,
                Returns,
                Follows,
                Settles,
                Phone(NO),
                Settles,
            ],
            "accept",
            1,
        ),
    ];
    for (row, steps, decision, answers) in rows {
        let fixture = PhoneFixture::new().await;
        let conversation = uuid::Uuid::new_v4().to_string();
        let thread = uuid::Uuid::new_v4().to_string();
        crate::test_support::bind_project_thread(
            &fixture.state,
            &conversation,
            AgentFamily::Codex,
            &thread,
            None,
            "now",
        )
        .await
        .unwrap();
        let router = FakeRouter::start(true, HashMap::new());
        router.serve_thread(&thread);
        let next = std::sync::atomic::AtomicU64::new(6);
        let request = |id: u64| {
            json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {
                "kind": "command", "threadId": thread, "turnId": "chatgpt-turn",
                "itemId": format!("exec-{id}"), "command": "/bin/zsh -lc 'touch approved.txt'",
                "reason": "Do you want to allow the command `touch approved.txt` in this folder?",
                "cwd": "/tmp/scratch", "supportsDenyMessage": true,
                "availableDecisions": ["accept", "cancel"]}})
        };
        let key =
            |id: u64| crate::chatgpt_approvals::request_key(&thread, "chatgpt-turn", &json!(id));
        let first = key(6);
        // The external poll, while the turn runs.
        let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let poll = {
            let (state, conversation, thread, running) = (
                fixture.state.clone(),
                conversation.clone(),
                thread.clone(),
                running.clone(),
            );
            tokio::spawn(async move {
                loop {
                    if running.load(std::sync::atomic::Ordering::SeqCst) {
                        crate::chatgpt_approvals::watch(&state, &conversation, &thread).await;
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            })
        };
        let listed = || async {
            let listed = list_approvals(State(fixture.state.clone())).await;
            let listed: Value =
                serde_json::from_slice(&to_bytes(listed.into_body(), 1 << 20).await.unwrap())
                    .unwrap();
            listed
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["conversationId"] == conversation.as_str())
                .cloned()
                .collect::<Vec<_>>()
        };
        for step in *steps {
            match step {
                Asks => {
                    let id = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    router.owner().ask(request(id));
                }
                AsksUnseen => {
                    let id = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    router.owner().missed(request(id));
                    // The next change shows Cupola the gap.
                    router.owner().unrelated();
                }
                Follows => {
                    running.store(true, std::sync::atomic::Ordering::SeqCst);
                    let follows = || router.owner().following();
                    for _ in 0..2_000 {
                        if follows() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(follows(), "{row}: follows");
                }
                Settles => {
                    // The phone lists exactly what ChatGPT asks, each once.
                    let mut shown = Vec::new();
                    let mut asked = Vec::new();
                    for _ in 0..2_000 {
                        asked = router
                            .owner()
                            .requests
                            .iter()
                            .map(|request| key(request["id"].as_u64().unwrap()))
                            .collect::<Vec<_>>();
                        shown = listed()
                            .await
                            .iter()
                            .map(|item| item["approvalId"].as_str().unwrap().to_owned())
                            .collect::<Vec<_>>();
                        if shown == asked {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert_eq!(shown, asked, "{row}: {step:?}");
                    if let Some(item) = listed().await.first() {
                        assert_eq!(item["method"], "item/commandExecution/requestApproval");
                        assert_eq!(
                            item["params"]["reason"],
                            "Do you want to allow the command `touch approved.txt` in this folder?"
                        );
                        assert_eq!(
                            item["params"]["availableDecisions"],
                            json!(["accept", "cancel"])
                        );
                        assert!(item["params"].get("supportsDenyMessage").is_none(), "{row}");
                        assert!(item["params"].get("_wonderChatGpt").is_none(), "{row}");
                    }
                }
                Phone(status) => {
                    let body = fixture.body(&first, json!("accept"), None).await;
                    let (actual, said) = fixture.post(&first, body).await;
                    assert_eq!(actual, *status, "{row}: {said}");
                    match *decision {
                        crate::chatgpt_approvals::RESOLVED_THERE => {
                            assert_eq!(said, crate::chatgpt_approvals::GONE, "{row}")
                        }
                        crate::chatgpt_approvals::ON_COMPUTER => {
                            assert_eq!(said, crate::chatgpt_approvals::ANSWER_ON_COMPUTER, "{row}")
                        }
                        _ => {}
                    }
                }
                Computer => router.owner().remove(&json!(6)),
                Returns => router.serve_thread(&thread),
                Snapshot => router.owner().resend(),
                Disconnects => {
                    let before = router.owner().follows;
                    router.owner().disconnect();
                    for _ in 0..2_000 {
                        if router.owner().follows > before && router.owner().following() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(router.owner().follows > before, "{row}: follows again");
                }
                Quits | TurnEnds => {
                    if matches!(step, Quits) {
                        crate::codex_desktop_ipc::TEST_ROUTERS
                            .lock()
                            .unwrap()
                            .remove(&thread);
                        router.owner().disconnect();
                    } else {
                        running.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    for _ in 0..2_000 {
                        if !crate::chatgpt_approvals::watching(&thread) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(
                        !crate::chatgpt_approvals::watching(&thread),
                        "{row}: stopped"
                    );
                    // Nothing it left looks answerable.
                    assert!(listed().await.is_empty(), "{row}");
                    if matches!(step, TurnEnds) {
                        assert!(!router.owner().following(), "{row}: stops following");
                    }
                }
            }
        }
        poll.abort();
        let stored = fixture.state.store.approval(&first).await.unwrap();
        let stored = stored.unwrap_or_else(|| panic!("{row}: never shown"));
        assert_eq!(stored.state, "resolved", "{row}");
        assert_eq!(stored.decision.as_deref(), Some(*decision), "{row}");
        let sent = router.owner().answers.clone();
        assert_eq!(sent.len(), *answers, "{row}: {sent:?}");
        if *answers == 1 {
            assert_eq!(
                sent[0],
                json!({"conversationId": thread, "requestId": 6, "decision": "accept"}),
                "{row}"
            );
        }
        crate::codex_desktop_ipc::TEST_ROUTERS
            .lock()
            .unwrap()
            .remove(&thread);
        fixture.shutdown().await;
    }
}

#[cfg(target_os = "macos")]
fn chatgpt_request(thread: &str, id: u64) -> Value {
    json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {
        "kind": "command", "threadId": thread, "turnId": "chatgpt-turn",
        "itemId": format!("exec-{id}"), "command": "/bin/zsh -lc 'touch approved.txt'",
        "reason": "Do you want to allow the command `touch approved.txt` in this folder?",
        "cwd": "/tmp/scratch", "availableDecisions": ["accept", "cancel"]}})
}

/// A Codex conversation whose thread ChatGPT runs, served by a fake router,
/// with request 6 open on the phone as the host before a restart left it.
#[cfg(target_os = "macos")]
async fn chatgpt_thread_asking(
    fixture: &PhoneFixture,
) -> (
    String,
    String,
    String,
    crate::codex_desktop_ipc::tests::FakeRouter,
) {
    let conversation = uuid::Uuid::new_v4().to_string();
    let thread = uuid::Uuid::new_v4().to_string();
    crate::test_support::bind_project_thread(
        &fixture.state,
        &conversation,
        AgentFamily::Codex,
        &thread,
        None,
        "now",
    )
    .await
    .unwrap();
    let router = crate::codex_desktop_ipc::tests::FakeRouter::start(true, HashMap::new());
    router.serve_thread(&thread);
    crate::chatgpt_approvals::reconcile(
        &fixture.state,
        &conversation,
        &thread,
        &[chatgpt_request(&thread, 6)],
    )
    .await;
    let key = crate::chatgpt_approvals::request_key(&thread, "chatgpt-turn", &json!(6));
    let open = fixture.state.store.approval(&key).await.unwrap().unwrap();
    assert_eq!(open.state, "pending");
    (conversation, thread, key, router)
}

// Contract: after the host restarts, a request ChatGPT asked that the phone
// still offers is read back from ChatGPT once, whether or not its turn still
// runs on the Mac: still asked, it stays answerable; answered there, it
// closes; ChatGPT unreadable, only the computer can answer it.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_restart_reads_chatgpt_requests_back_once() {
    use crate::chatgpt_approvals::{ON_COMPUTER, RESOLVED_THERE};
    for (row, asks, reachable, decision) in [
        ("still asked", true, true, None),
        (
            "answered there meanwhile",
            false,
            true,
            Some(RESOLVED_THERE),
        ),
        ("ChatGPT can't be reached", true, false, Some(ON_COMPUTER)),
    ] {
        let fixture = PhoneFixture::new().await;
        let (_, thread, key, router) = chatgpt_thread_asking(&fixture).await;
        if asks {
            router.owner().ask(chatgpt_request(&thread, 6));
        }
        if !reachable {
            crate::codex_desktop_ipc::TEST_ROUTERS
                .lock()
                .unwrap()
                .remove(&thread);
        }
        crate::ingestion::recheck_desktop_approvals(&fixture.state).await;
        let stored = fixture.state.store.approval(&key).await.unwrap().unwrap();
        match decision {
            None => assert_eq!(stored.state, "pending", "{row}"),
            Some(decision) => {
                assert_eq!(stored.state, "resolved", "{row}");
                assert_eq!(stored.decision.as_deref(), Some(decision), "{row}");
            }
        }
        assert!(
            !crate::chatgpt_approvals::watching(&thread),
            "{row}: read once"
        );
        for _ in 0..2_000 {
            if !router.owner().following() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!router.owner().following(), "{row}: stops following");
        if decision.is_none() {
            let body = fixture.body(&key, json!("accept"), None).await;
            let (status, said) = fixture.post(&key, body).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{row}: {said}");
            assert_eq!(router.owner().answers.len(), 1, "{row}");
        }
        crate::codex_desktop_ipc::TEST_ROUTERS
            .lock()
            .unwrap()
            .remove(&thread);
        fixture.shutdown().await;
    }
}

// Contract: when ChatGPT keeps accepting Cupola's connection but never lets
// it follow the thread, the watch gives up after a bounded number of
// connections and leaves the request to the computer.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_watch_chatgpt_never_lets_follow_gives_up() {
    let fixture = PhoneFixture::new().await;
    let (conversation, thread, key, router) = chatgpt_thread_asking(&fixture).await;
    router.owner().ask(chatgpt_request(&thread, 6));
    router.owner().refuse_follows = true;
    crate::chatgpt_approvals::watch(&fixture.state, &conversation, &thread).await;
    for _ in 0..2_000 {
        if !crate::chatgpt_approvals::watching(&thread) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!crate::chatgpt_approvals::watching(&thread), "gives up");
    let connections = router
        .methods()
        .iter()
        .filter(|method| *method == "thread-owner-discovery")
        .count();
    assert_eq!(connections, 3);
    let stored = fixture.state.store.approval(&key).await.unwrap().unwrap();
    assert_eq!(
        stored.decision.as_deref(),
        Some(crate::chatgpt_approvals::ON_COMPUTER)
    );
    crate::codex_desktop_ipc::TEST_ROUTERS
        .lock()
        .unwrap()
        .remove(&thread);
    fixture.shutdown().await;
}

/// Live, end to end through Cupola with a temporary data folder: follows the
/// ChatGPT scratch thread `CUPOLA_RELAY_THREAD` (refused unless its folder is
/// inside `CUPOLA_RELAY_FIXTURE_ROOT`), waits up to 4 minutes for a command
/// approval its running turn asks (start one in ChatGPT meanwhile), answers
/// it from the "phone" with a signed Allow, and checks ChatGPT took it.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "live: answers one approval in a ChatGPT scratch thread"]
async fn live_scratch_thread_approval_answered_from_the_phone() {
    let log = |step: &str, detail: Value| {
        println!(
            "{}",
            json!({"t": chrono::Utc::now().to_rfc3339(), "step": step, "detail": detail})
        );
    };
    let (thread, home, rollout) = crate::codex_desktop_ipc::tests::scratch_thread();
    // As the external poll's index records it.
    crate::desktop_activity::remember_codex_rollout(&thread, rollout.to_str().unwrap());
    // Test builds read codex processes from stand-ins: name ChatGPT's own
    // Codex server, which holds the thread.
    let servers = std::process::Command::new("/usr/bin/pgrep")
        .args([
            "-f",
            "ChatGPT.app/Contents/Resources/codex-cli/.*app-server",
        ])
        .output()
        .unwrap();
    for pid in String::from_utf8_lossy(&servers.stdout).lines() {
        let pid: i32 = pid.trim().parse().unwrap();
        crate::desktop_activity::tests::CODEX_PIDS
            .lock()
            .unwrap()
            .insert(pid);
        crate::desktop_activity::tests::CODEX_ARGS
            .lock()
            .unwrap()
            .insert(pid, vec!["codex".into(), "app-server".into()]);
    }
    let mut fixture = PhoneFixture::new().await;
    fixture.state.codex_home = home;
    let conversation = uuid::Uuid::new_v4().to_string();
    crate::test_support::bind_project_thread(
        &fixture.state,
        &conversation,
        AgentFamily::Codex,
        &thread,
        None,
        "now",
    )
    .await
    .unwrap();
    let poll = {
        let (state, conversation, thread) =
            (fixture.state.clone(), conversation.clone(), thread.clone());
        tokio::spawn(async move {
            loop {
                crate::chatgpt_approvals::watch(&state, &conversation, &thread).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let listed = || async {
        let listed = list_approvals(State(fixture.state.clone())).await;
        let listed: Value =
            serde_json::from_slice(&to_bytes(listed.into_body(), 1 << 20).await.unwrap()).unwrap();
        listed
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["conversationId"] == conversation.as_str())
            .cloned()
            .collect::<Vec<_>>()
    };
    log(
        "waiting",
        json!({"watching": crate::chatgpt_approvals::watching(&thread)}),
    );
    let started = std::time::Instant::now();
    let item = loop {
        if let Some(item) = listed().await.into_iter().next() {
            break item;
        }
        assert!(
            started.elapsed() < Duration::from_secs(240),
            "no approval asked"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    log("shown", item.clone());
    let key = item["approvalId"].as_str().unwrap().to_owned();
    let body = fixture.body(&key, json!("accept"), None).await;
    let sent = std::time::Instant::now();
    let (status, said) = fixture.post(&key, body).await;
    log(
        "phone",
        json!({"status": status.as_u16(), "said": said, "ms": sent.elapsed().as_millis()}),
    );
    assert_eq!(status, StatusCode::NO_CONTENT, "{said}");
    // ChatGPT took it: the request leaves its list, and the phone's too.
    let mut gone = false;
    for _ in 0..300 {
        if listed().await.is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stored = fixture.state.store.approval(&key).await.unwrap().unwrap();
    log(
        "closed",
        json!({"gone": gone, "state": stored.state, "decision": stored.decision,
            "ms": sent.elapsed().as_millis()}),
    );
    poll.abort();
    for _ in 0..400 {
        if !crate::chatgpt_approvals::watching(&thread) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    log(
        "stopped",
        json!({"watching": crate::chatgpt_approvals::watching(&thread)}),
    );
    assert!(gone);
    assert_eq!(stored.decision.as_deref(), Some("accept"));
    fixture.shutdown().await;
}

// Contract: an edit approval lists the change the agent proposes, kept with
// that request and its nonce. Codex's request names its file change item, so
// the item's proposal is attached; a completed item (the applied change) is
// never shown as a proposal. Over-cap diffs are marked, not cut short.
#[tokio::test]
async fn edit_approvals_carry_the_proposed_diff_bound_to_their_request() {
    let fixture = PhoneFixture::new().await;
    let runtime = fixture
        .state
        .projects
        .codex
        .lock()
        .await
        .health()
        .id()
        .to_owned();
    let thread = fixture.thread;
    let notify = |value: Value| process_app_server_notification(&fixture.state, value, true);
    let file_item = |id: &str, diff: &str| {
        json!({"type":"fileChange","id":id,"status":"inProgress","changes":[
            {"path":"/work/src/lib.rs","kind":{"type":"update","move_path":null},"diff":diff}]})
    };
    let store = &fixture.state.store;
    let summary = |id: String| async move {
        let stored = store.approval(&id).await.unwrap().unwrap();
        let params: Value = serde_json::from_str(&stored.params_json).unwrap();
        let summary = redact_approval_params(&stored.method, &params);
        test_support::assert_http_contract(
            "approvalSummary",
            &json!({"approvalId":id, "conversationId":"chat", "method":stored.method,
                "params":summary, "actionNonce":stored.action_nonce, "resolutionIdempotencyKey":null}),
        );
        (summary, stored.action_nonce)
    };
    let request = |id: &str, item: &str| {
        json!({"_wonderRuntimeId":runtime,"id":id,"method":"item/fileChange/requestApproval",
            "params":{"threadId":thread,"turnId":"turn","itemId":item,"reason":"Apply the fix?"}})
    };

    assert!(notify(json!({"_wonderRuntimeId":runtime,"method":"item/started",
        "params":{"threadId":thread,"turnId":"turn","item":file_item("edit-1","@@ -1 +1 @@\n-old\n+draft")}})).await);
    assert!(notify(json!({"_wonderRuntimeId":runtime,"method":"item/fileChange/patchUpdated",
        "params":{"threadId":thread,"turnId":"turn","itemId":"edit-1","changes":[
            {"path":"/work/src/lib.rs","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-old\n+new"}]}})).await);
    assert!(notify(request("first", "edit-1")).await);
    let (first, first_nonce) = summary(format!("{runtime}:first")).await;
    assert_eq!(
        first["changes"],
        json!([{"path":"/work/src/lib.rs","kind":"update","diff":"@@ -1 +1 @@\n-old\n+new"}])
    );

    // A different proposal is a different request with its own nonce; the
    // first request keeps exactly what it showed.
    assert!(notify(json!({"_wonderRuntimeId":runtime,"method":"item/started",
        "params":{"threadId":thread,"turnId":"turn","item":file_item("edit-2","@@ -1 +1 @@\n-new\n+newer")}})).await);
    assert!(notify(request("second", "edit-2")).await);
    let (second, second_nonce) = summary(format!("{runtime}:second")).await;
    assert_eq!(second["changes"][0]["diff"], "@@ -1 +1 @@\n-new\n+newer");
    assert_ne!(first_nonce, second_nonce);
    assert_eq!(summary(format!("{runtime}:first")).await.0, first);

    // Only an item seen before it was applied is a proposal.
    assert!(
        notify(json!({"_wonderRuntimeId":runtime,"method":"item/completed",
        "params":{"threadId":thread,"turnId":"turn","item":file_item("edit-3","@@ applied @@")}}))
        .await
    );
    assert!(notify(request("third", "edit-3")).await);
    assert!(summary(format!("{runtime}:third"))
        .await
        .0
        .get("changes")
        .is_none());

    // Claude's bridge sends its own bounded changes; each diff over 64 KB, and
    // any past 200 KB in one request, is marked instead of shown.
    let large = "+".repeat(PROPOSED_DIFF_BYTES + 1);
    let fits = "+".repeat(60 * 1024);
    let summary = redact_approval_params(
        "item/fileChange/requestApproval",
        &json!({"changes":[
            {"path":"/a","kind":"update","diff":large},
            {"path":"/b","kind":"add","diff":fits},
            {"path":"/c","kind":"update","diff":fits},
            {"path":"/d","kind":"update","diff":fits},
            {"path":"/e","kind":"update","diff":fits},
            {"path":"/f","kind":"update","truncated":true},
            {"path":"/g","kind":"update","diff":""},
            {"kind":"update","diff":"+no path"},
        ]}),
    );
    let changes = summary["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 7, "a change without a path is not shown");
    assert_eq!(
        changes[0],
        json!({"path":"/a","kind":"update","truncated":true})
    );
    assert!(changes[1..4]
        .iter()
        .all(|change| change["diff"] == fits.as_str()));
    assert_eq!(
        changes[4],
        json!({"path":"/e","kind":"update","truncated":true})
    );
    assert_eq!(
        changes[5],
        json!({"path":"/f","kind":"update","truncated":true})
    );
    assert_eq!(changes[6], json!({"path":"/g","kind":"update"}));
    fixture.shutdown().await;
}

/// The phone builds this by hand in Swift (`testSecretDecisionTranscriptMatchesRust`);
/// both tests pin the same bytes so a drift in either fails before a Save is refused.
#[test]
fn secret_signature_transcript_matches_the_phone() {
    let answer = |decision: &str, value: Option<&str>, key: &str| SecretAnswer {
        decision: decision.into(),
        value: value.map(str::to_owned),
        action_nonce: "nonce".into(),
        expected_state: "pending".into(),
        idempotency_key: key.into(),
        issued_at_ms: 1234,
        signature: String::new(),
    };
    let save = answer("save", Some("p\"a\\b/é📸\n"), "key-1");
    let hash = hex::encode(Sha256::digest(canonical_secret_body(&save).as_bytes()));
    let transcript = ActionTranscript {
        action: "secret.save",
        target: "/api/v1/secret-requests/secret:1",
        body_sha256: &hash,
        action_nonce: "nonce",
        session_binding: "binding",
        device_id: "phone",
        host_installation_id: "host",
        issued_at_ms: 1234,
        expected_state: "pending",
    };
    assert_eq!(
        String::from_utf8(transcript.to_bytes()).unwrap(),
        "wonder-action-v1\nsecret.save\n/api/v1/secret-requests/secret:1\n7ea16be923592b32ce408c96353f405529f2cfbc0a8d63ed61c5f503b826db76\nnonce\nbinding\nphone\nhost\n1234\npending"
    );
    let cancel = answer("cancel", None, "key-2");
    assert_eq!(
        hex::encode(Sha256::digest(canonical_secret_body(&cancel).as_bytes())),
        "b41b260ac8fdd97f0dc5a35ec3ea56d2639f38f0c756095deeaec83be2f01908"
    );
}

// Contract: a ChatGPT request left `resolving` (the phone's answer unknown, or
// the host restarted mid-answer) is closed by the next reconcile that no
// longer lists it, and by the watch ending, so it never lingers.
#[tokio::test]
async fn a_chatgpt_request_stuck_resolving_closes() {
    use crate::chatgpt_approvals::{reconcile, request_key, stopped, ON_COMPUTER, RESOLVED_THERE};
    let ask = |thread: &str, id: u64| {
        json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {
            "kind": "command", "threadId": thread, "turnId": "chatgpt-turn",
            "itemId": format!("exec-{id}"), "command": "touch approved.txt",
            "cwd": "/tmp/scratch", "availableDecisions": ["accept", "cancel"]}})
    };
    for (row, relisted, ends, decision) in [
        (
            "answered on the computer",
            false,
            false,
            Some(RESOLVED_THERE),
        ),
        ("still asked", true, false, None),
        ("watch ends", true, true, Some(ON_COMPUTER)),
    ] {
        let fixture = PhoneFixture::new().await;
        let (conversation, thread) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let key = request_key(&thread, "chatgpt-turn", &json!(6));
        reconcile(&fixture.state, &conversation, &thread, &[ask(&thread, 6)]).await;
        let id = fixture
            .state
            .store
            .approval(&key)
            .await
            .unwrap()
            .unwrap()
            .approval_id;
        let begun = fixture
            .state
            .store
            .begin_approval_resolution(&id)
            .await
            .unwrap();
        assert_eq!(begun.unwrap().state, "resolving", "{row}");
        if ends {
            stopped(&fixture.state, &conversation, &thread, false).await;
        } else {
            let listed = if relisted {
                vec![ask(&thread, 6)]
            } else {
                vec![]
            };
            reconcile(&fixture.state, &conversation, &thread, &listed).await;
        }
        let stored = fixture.state.store.approval(&key).await.unwrap().unwrap();
        match decision {
            None => assert_eq!(stored.state, "resolving", "{row}"),
            Some(decision) => {
                assert_eq!(stored.state, "resolved", "{row}");
                assert_eq!(stored.decision.as_deref(), Some(decision), "{row}");
            }
        }
        fixture.shutdown().await;
    }
}
