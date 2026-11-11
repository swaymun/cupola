//! Cupola RPC contracts: the checked-in schema, the method table against the
//! routes, and both authorities over a real socket into the real router.
use super::client::{CallError, Client};
use super::*;
use crate::{now_ms, router, AppState};
use base64::Engine;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use sha2::Digest;

const THREAD: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";

/// Run with `WONDER_UPDATE_RPC_SCHEMA=1` to rewrite the file after a change.
#[test]
fn schema_matches_the_checked_in_file() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(SCHEMA_PATH);
    let generated = schema_text();
    if std::env::var_os("WONDER_UPDATE_RPC_SCHEMA").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "{SCHEMA_PATH} is out of date with crates/wonderd/src/rpc. Regenerate it: \
         WONDER_UPDATE_RPC_SCHEMA=1 cargo test -p wonderd rpc::tests::schema, \
         or `wonder rpc --schema > {SCHEMA_PATH}`."
    );
}

/// Example params for every method; each must satisfy the schema.
fn example(name: &str) -> Value {
    match name {
        "initialize" => json!({"protocolVersion": 1, "clientInfo": {"name": "test"}}),
        "host.rename" => json!({"name": "Studio"}),
        "media.orphans" | "media.prune" => json!({"olderThanDays": 14}),
        "devices.revoke" => json!({"deviceId": "d 1"}),
        "devices.forget" | "pairing.reject" => json!({"deviceId": "d1"}),
        "devices.rename" => json!({"deviceId": "d1", "label": "Work iPhone"}),
        "pairing.confirm" => json!({"deviceId": "d1", "code": "B36D1B62BB5F"}),
        "pairing.cancel" => json!({"offerId": "o1"}),
        "relay.set" => json!({"allowed": true}),
        "relay.enable" => json!({
            "grant": "g".repeat(43),
            "label": "k7xq2abcdefghijklmnopqrstu",
        }),
        "providers.list" => json!({"refresh": true}),
        "providers.reconnect" => json!({"provider": "codex"}),
        "update.prepare" | "update.cancel" => json!({
            "requestId": "00000000-0000-4000-8000-000000000002",
            "consentedRunning": 1,
        }),
        "voice.setKey" => json!({"apiKey": "sk-0123456789abcdefghij"}),
        "threads.list" => json!({"projectId": "p1", "cursor": "c 1", "limit": 10}),
        "threads.attach" => json!({"projectId": "p1", "reference": "r1"}),
        "threads.read" => json!({"threadId": THREAD, "before": "b", "limit": 20}),
        "threads.send" => json!({
            "threadId": THREAD,
            "idempotencyKey": "00000000-0000-4000-8000-000000000001",
            "body": "hello",
        }),
        "threads.create" => json!({
            "projectId": "p 1",
            "idempotencyKey": "00000000-0000-4000-8000-000000000003",
            "body": "hello",
            "family": "claude",
            "model": "m",
            "accountId": "default",
        }),
        "threads.delegate" => json!({
            "threadId": THREAD,
            "idempotencyKey": "00000000-0000-4000-8000-000000000004",
            "prompt": "task",
            "provider": "claude",
            "folder": "/work/tree",
            "maxChildren": 4,
        }),
        "threads.notifications" => json!({"threadId": THREAD}),
        "threads.setNotifications" => json!({"threadId": THREAD, "notificationLevel": "follow"}),
        "threads.moveToWorktree" => json!({"threadId": THREAD}),
        "threads.queue" => json!({"threadId": THREAD}),
        "queue.edit" => json!({
            "threadId": THREAD, "messageId": "m 1", "expectedRevision": 2,
            "expectedTurnId": "turn",
        }),
        "approvals.resolve" => json!({
            "approvalId": "a1", "decision": "accept", "actionNonce": "n",
            "expectedState": "pending", "idempotencyKey": "k", "issuedAtMs": 1,
            "signature": "s",
        }),
        "events.subscribe" => json!({"after": {"hostEpoch": "epoch", "sequence": 3}}),
        "events.unsubscribe" => json!({"subscriptionId": "s1"}),
        "http.request" => json!({"method": "GET", "path": "/api/v1/host/status"}),
        _ => json!({}),
    }
}

fn validator(schema: &Value, part: Value) -> jsonschema::Validator {
    jsonschema::validator_for(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": schema["$defs"],
        "allOf": [part],
    }))
    .unwrap()
}

/// Checks a real result against the method's `result` in the published schema,
/// which is what a schema-aware client validates.
fn validate_result(method: &str, result: &Value) {
    let schema = schema();
    let check = validator(&schema, schema["methods"][method]["result"].clone());
    let errors: Vec<_> = check.iter_errors(result).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{method} result {result}: {errors:?}");
}

#[test]
fn every_method_routes_to_its_declared_route_with_params_the_schema_accepts() {
    let schema = schema();
    for method in METHODS {
        let params = example(method.name);
        let check = validator(&schema, schema["methods"][method.name]["params"].clone());
        let errors: Vec<_> = check.iter_errors(&params).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{}: {errors:?}", method.name);
        let Some(declared) = method.route else {
            continue;
        };
        let call = route(method.name, Some(params), &Caller::Owner).unwrap();
        let (verb, template) = declared.split_once(' ').unwrap();
        assert_eq!(call.method.as_str(), verb, "{}", method.name);
        // Each `{param}` is exactly one percent-encoded segment.
        let path = call.uri.split('?').next().unwrap();
        let (want, got): (Vec<_>, Vec<_>) =
            (template.split('/').collect(), path.split('/').collect());
        assert_eq!(want.len(), got.len(), "{}: {path}", method.name);
        for (want, got) in want.iter().zip(&got) {
            if !want.starts_with('{') {
                assert_eq!(want, got, "{}: {path}", method.name);
            }
        }
    }
    // An unknown param is refused, not ignored.
    let error = route(
        "threads.send",
        Some(json!({"threadId": THREAD, "idempotencyKey": "k", "body": "x", "deviceId": "other"})),
        &Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(error.code, codes::INVALID_PARAMS);
}

/// The guide's method table names every method this host serves.
#[test]
fn the_guide_has_a_row_for_every_method() {
    let guide = include_str!("../../RPC.md");
    for method in METHODS {
        let authority = match method.authority {
            Authority::Any => "any",
            Authority::Owner => "owner",
            Authority::PairedSigned => "signed",
            Authority::OwnerOrSigned => "owner or signed",
            Authority::SealedDevice => "sealed device",
        };
        assert!(
            guide.contains(&format!("| `{}` | {authority} |", method.name)),
            "crates/wonderd/RPC.md has no method-table row for `{}` with authority {authority}",
            method.name
        );
    }
    for (notification, _) in NOTIFICATIONS {
        assert!(
            guide.contains(&format!("`{notification}`")),
            "crates/wonderd/RPC.md does not describe the `{notification}` notification"
        );
    }
}

#[test]
fn ids_stay_one_path_segment_and_queries_are_encoded() {
    assert_eq!(segment("6f1c-AB_9.~"), "6f1c-AB_9.~");
    assert_eq!(segment("../x y"), "..%2Fx%20y");
    let call = route(
        "devices.revoke",
        Some(example("devices.revoke")),
        &Caller::Owner,
    )
    .unwrap();
    assert_eq!(call.uri, "/api/v1/devices/d%201/revoke");
    // The local owner sends no signed fields; the route accepts `null`.
    assert_eq!(call.body, Some(Value::Null));
    let call = route(
        "threads.list",
        Some(example("threads.list")),
        &Caller::Owner,
    )
    .unwrap();
    assert_eq!(
        call.uri,
        "/api/v1/projects/p1/threads?cursor=c%201&limit=10"
    );
}

#[test]
fn the_sender_comes_from_the_session_never_from_params() {
    for (method, key) in [
        ("threads.send", "00000000-0000-4000-8000-000000000001"),
        ("threads.create", "00000000-0000-4000-8000-000000000003"),
    ] {
        let params = Some(example(method));
        let owner = route(method, params.clone(), &Caller::Owner).unwrap();
        assert_eq!(owner.body.unwrap()["deviceId"], LOCAL_SENDER, "{method}");
        let device = route(method, params, &Caller::Device("phone".into())).unwrap();
        let body = device.body.unwrap();
        assert_eq!(body["deviceId"], "phone", "{method}");
        assert_eq!(body["clientMessageId"], key, "{method}");
    }
}

#[test]
fn route_refusals_keep_their_status_and_the_daemons_words() {
    assert_eq!(codes::for_http_status(401), -32001);
    assert_eq!(codes::for_http_status(404), -32004);
    assert_eq!(codes::for_http_status(409), -32009);
    assert_eq!(codes::for_http_status(503), -32103);
    let error = RpcError::from_http(409, br#"{"detail":"Pairing is paused."}"#);
    assert_eq!(error.message, "Pairing is paused.");
    assert_eq!(error.http_status(), Some(409));
    let error = RpcError::from_http(503, b"This computer's connection address isn't ready yet.");
    assert_eq!(
        error.message,
        "This computer's connection address isn't ready yet."
    );
    let error = RpcError::from_http(409, br#"{"error":"idempotency_conflict"}"#);
    assert_eq!(
        error.data.unwrap().body,
        Some(json!({"error": "idempotency_conflict"}))
    );
}

// ----- Over a real socket -----

async fn serve(state: &AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("ws://{addr}/api/v1/rpc")
}

async fn owner(state: &AppState) -> Client {
    let url = serve(state).await;
    let capability = state.loopback_capability.clone();
    Client::connect(&url, &[("x-wonder-loopback-capability", &capability)])
        .await
        .unwrap()
}

fn code(result: Result<Value, CallError>) -> i64 {
    match result {
        Err(CallError::Rpc(error)) => error.code,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .expect("no answer in time")
}

#[tokio::test]
async fn the_local_owner_negotiates_then_calls_the_existing_routes() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    // A client that is not the owner or a paired device never gets a socket.
    let url = serve(&state).await;
    assert!(Client::connect(&url, &[]).await.is_err());
    assert!(
        Client::connect(&url, &[("x-wonder-loopback-capability", "wrong")])
            .await
            .is_err()
    );

    let client = owner(&state).await;
    assert_eq!(
        code(client.call("host.status", Value::Null).await),
        codes::NOT_INITIALIZED
    );
    let refused = client
        .call(
            "initialize",
            json!({"protocolVersion": 99, "clientInfo": {"name": "test"}}),
        )
        .await;
    let Err(CallError::Rpc(error)) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(error.code, codes::UNSUPPORTED_VERSION);
    assert_eq!(error.data.unwrap().supported, Some(vec![PROTOCOL_VERSION]));

    let init = client.initialize("test").await.unwrap();
    assert_eq!(init.authority, "owner");
    // Apps tell hosts apart by the release they run, as `cupola version` says.
    assert_eq!(init.server_info.version, crate::RELEASE_VERSION);
    assert_eq!(init.host_epoch, state.host_epoch);
    let names: Vec<_> = METHODS.iter().map(|m| m.name.to_owned()).collect();
    assert_eq!(init.methods, names);
    assert_eq!(
        code(client.initialize("again").await.map(|_| Value::Null)),
        codes::ALREADY_INITIALIZED
    );

    // The owner never signs a challenge; it hears why instead of an internal error.
    let challenge = client.call("auth.challenge", Value::Null).await;
    let Err(CallError::Rpc(error)) = challenge else {
        panic!("{challenge:?}")
    };
    assert_eq!(error.code, codes::for_http_status(403));
    assert!(
        error.message.starts_with("Only a paired device signs in"),
        "{}",
        error.message
    );

    let status = client.call("host.status", Value::Null).await.unwrap();
    crate::tests::validate_http_contract("hostStatus", &status);
    let devices = client.call("devices.list", json!({})).await.unwrap();
    assert!(devices
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["id"] == "owner"));
    let projects = client.call("projects.list", Value::Null).await.unwrap();
    crate::tests::validate_http_contract("projectsResponse", &projects);
    let snapshot = client
        .call("threads.read", json!({"threadId": THREAD, "limit": 5}))
        .await
        .unwrap();
    crate::tests::validate_http_contract("conversationSnapshot", &snapshot);

    for level in ["default", "follow", "mute"] {
        let saved = client
            .call(
                "threads.setNotifications",
                json!({"threadId": THREAD, "notificationLevel": level}),
            )
            .await
            .unwrap();
        validate_result("threads.setNotifications", &saved);
        assert_eq!(saved, json!({"notificationLevel": level}));
        assert_eq!(
            client
                .call("threads.notifications", json!({"threadId": THREAD}))
                .await
                .unwrap(),
            saved
        );
    }
    assert_eq!(
        code(
            client
                .call(
                    "threads.setNotifications",
                    json!({"threadId": THREAD, "notificationLevel": "invalid"})
                )
                .await
        ),
        codes::INVALID_PARAMS
    );
    assert_eq!(
        code(
            client
                .call(
                    "threads.setNotifications",
                    json!({"threadId": "missing", "notificationLevel": "follow"})
                )
                .await
        ),
        codes::for_http_status(404)
    );

    // The routes' own rules still decide: approvals need a paired device's
    // signature, and an unknown device is the route's 404.
    assert_eq!(
        code(
            client
                .call("approvals.resolve", example("approvals.resolve"))
                .await
        ),
        codes::for_http_status(401)
    );
    assert_eq!(
        code(
            client
                .call("devices.forget", json!({"deviceId": "nobody"}))
                .await
        ),
        codes::for_http_status(404)
    );
    assert_eq!(
        code(
            client
                .call(
                    "devices.rename",
                    json!({"deviceId": "nobody", "label": "Work"})
                )
                .await
        ),
        codes::for_http_status(404)
    );
    assert_eq!(
        code(client.call("threads.read", json!({"thread": THREAD})).await),
        codes::INVALID_PARAMS
    );
    assert_eq!(
        code(client.call("threads.delete", Value::Null).await),
        codes::METHOD_NOT_FOUND
    );
}

#[tokio::test]
async fn malformed_frames_get_json_rpc_errors_and_the_connection_survives() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let mut request = serve(&state).await.into_client_request().unwrap();
    request.headers_mut().insert(
        "x-wonder-loopback-capability",
        state.loopback_capability.parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut ask = async |text: &str| -> Value {
        socket.send(Message::Text(text.into())).await.unwrap();
        loop {
            if let Message::Text(reply) = within(socket.next()).await.unwrap().unwrap() {
                return serde_json::from_str(reply.as_str()).unwrap();
            }
        }
    };
    assert_eq!(ask("{not json").await["error"]["code"], codes::PARSE_ERROR);
    let batch = ask(r#"[{"jsonrpc":"2.0","id":1,"method":"host.status"}]"#).await;
    assert_eq!(batch["error"]["code"], codes::INVALID_REQUEST);
    let old = ask(r#"{"jsonrpc":"1.0","id":2,"method":"host.status"}"#).await;
    assert_eq!(
        (old["id"].clone(), old["error"]["code"].clone()),
        (json!(2), json!(codes::INVALID_REQUEST))
    );
    let init = ask(r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"t"}}}"#).await;
    assert_eq!(init["id"], "a");
    assert_eq!(init["result"]["protocolVersion"], 1);
}

#[tokio::test]
async fn a_send_is_idempotent_per_key_and_conflicts_on_a_different_body() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let _ingestion = crate::ingestion::spawn(state.clone()).await;
    for _ in 0..200 {
        if state.ingestion.readiness(&state.store).await.ready {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let client = owner(&state).await;
    client.initialize("test").await.unwrap();
    let send = |body: &str| {
        json!({
            "threadId": THREAD,
            "idempotencyKey": "00000000-0000-4000-8000-0000000000aa",
            "body": body,
        })
    };
    let first = client.call("threads.send", send("hello")).await.unwrap();
    validate_result("threads.send", &first);
    assert_eq!(first["queued"], false);
    let again = client.call("threads.send", send("hello")).await.unwrap();
    assert_eq!(again["wonderMessageId"], first["wonderMessageId"]);
    let conflict = client.call("threads.send", send("something else")).await;
    let Err(CallError::Rpc(error)) = conflict else {
        panic!("{conflict:?}")
    };
    assert_eq!(error.code, codes::for_http_status(409));
    assert_eq!(
        error.data.unwrap().body,
        Some(json!({"error": "idempotency_conflict"}))
    );
    let stored = state
        .store
        .message_by_device_and_client_message_id(
            LOCAL_SENDER,
            "00000000-0000-4000-8000-0000000000aa",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["wonderMessageId"], stored.id);

    // An empty message is a mistake in the request, not one that is too large.
    let empty = client
        .call(
            "threads.send",
            json!({
                "threadId": THREAD,
                "idempotencyKey": "00000000-0000-4000-8000-0000000000ab",
                "body": "  ",
            }),
        )
        .await;
    let Err(CallError::Rpc(error)) = empty else {
        panic!("{empty:?}")
    };
    assert_eq!(error.http_status(), Some(400), "{error:?}");
    let oversized = client
        .call(
            "threads.send",
            json!({
                "threadId": THREAD,
                "idempotencyKey": "00000000-0000-4000-8000-0000000000ac",
                "body": "x".repeat(64 * 1024 + 1),
            }),
        )
        .await;
    let Err(CallError::Rpc(error)) = oversized else {
        panic!("{oversized:?}")
    };
    assert_eq!(error.http_status(), Some(413), "{error:?}");
}

/// What `wonder chat` drives: a new thread with its first message (sent
/// once per key), the models a new thread may use, and the thread's queue.
#[tokio::test]
async fn the_owner_starts_a_thread_and_reads_and_edits_its_queue() {
    let (_dir, state) = crate::test_support::fixture().await;
    let _ingestion = crate::ingestion::spawn(state.clone()).await;
    for _ in 0..200 {
        if state.ingestion.readiness(&state.store).await.ready {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let client = owner(&state).await;
    client.initialize("test").await.unwrap();
    let models = client.call("models.list", Value::Null).await.unwrap();
    validate_result("models.list", &models);
    let model = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["agentFamily"] == "codex")
        .expect("a Codex model")["id"]
        .clone();

    let projects = client.call("projects.list", Value::Null).await.unwrap();
    let project = projects["projects"][0]["id"].as_str().unwrap().to_owned();
    let create = |body: &str| {
        json!({
            "projectId": project,
            "idempotencyKey": "00000000-0000-4000-8000-0000000000bb",
            "body": body,
            "family": "codex",
            "model": model,
        })
    };
    let created = within(client.call("threads.create", create("hello"))).await;
    let created = created.unwrap();
    validate_result("threads.create", &created);
    let thread = created["conversation"]["conversationId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(created["receipt"]["conversationId"], thread.as_str());
    // A retry of the same key is the same thread and message, never a second.
    let again = within(client.call("threads.create", create("hello")))
        .await
        .unwrap();
    assert_eq!(again["conversation"]["conversationId"], thread.as_str());
    assert_eq!(
        again["receipt"]["wonderMessageId"],
        created["receipt"]["wonderMessageId"]
    );

    let queue = client
        .call("threads.queue", json!({"threadId": thread}))
        .await
        .unwrap();
    validate_result("threads.queue", &queue);
    // Sending a message that is not queued now is the route's conflict.
    assert_eq!(
        code(
            client
                .call(
                    "queue.edit",
                    json!({"threadId": thread, "messageId": "gone",
                           "expectedRevision": 1, "expectedTurnId": "turn"}),
                )
                .await
        ),
        codes::for_http_status(409)
    );
    assert_eq!(
        code(
            client
                .call(
                    "queue.edit",
                    json!({"threadId": thread, "messageId": "gone", "expectedRevision": 1,
                           "cancel": true, "expectedTurnId": "turn"}),
                )
                .await
        ),
        codes::for_http_status(400)
    );
}

#[tokio::test]
async fn events_replay_from_a_cursor_then_follow_and_a_stale_epoch_ends() {
    for batch in [false, true] {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let client = owner(&state).await;
        assert!(
            client
                .initialize("test")
                .await
                .unwrap()
                .capabilities
                .event_batches
        );
        let first = crate::publish_event(
            &state,
            wonder_api::WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
        // From before the first event: it is replayed.
        let subscribed = client
        .call(
            "events.subscribe",
            json!({"after": {"hostEpoch": state.host_epoch, "sequence": first.sequence - 1}, "batch": batch}),
        )
        .await
        .unwrap();
        let id = subscribed["subscriptionId"].as_str().unwrap().to_owned();
        let event = within(client.notification()).await.unwrap();
        assert_eq!(event.method, if batch { "events.batch" } else { "event" });
        assert_eq!(event.params["subscriptionId"], id.as_str());
        assert_eq!(
            if batch {
                &event.params["events"][0]["sequence"]
            } else {
                &event.params["event"]["sequence"]
            },
            &json!(first.sequence)
        );
        // Then live events follow.
        let second = crate::publish_event(
            &state,
            wonder_api::WonderEvent::HostStatus {
                state: "degraded".into(),
            },
        )
        .await
        .unwrap();
        let event = within(client.notification()).await.unwrap();
        assert_eq!(
            if batch {
                &event.params["events"][0]["sequence"]
            } else {
                &event.params["event"]["sequence"]
            },
            &json!(second.sequence)
        );
        assert_eq!(
            client
                .call("events.unsubscribe", json!({"subscriptionId": id}))
                .await
                .unwrap(),
            json!({})
        );
        assert_eq!(
            code(
                client
                    .call("events.unsubscribe", json!({"subscriptionId": id}))
                    .await
            ),
            codes::for_http_status(404)
        );
        // A cursor from another host epoch cannot be replayed: read state again.
        client
            .call(
                "events.subscribe",
                json!({"after": {"hostEpoch": "old-epoch", "sequence": 1}}),
            )
            .await
            .unwrap();
        let ended = within(client.notification()).await.unwrap();
        assert_eq!(ended.method, "events.ended");
        assert_eq!(ended.params["reason"], "resync");
    }
}

#[tokio::test]
async fn batched_replay_coalesces_a_committed_burst_without_losing_sequences() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let client = owner(&state).await;
    client.initialize("batch-test").await.unwrap();
    let before = state
        .store
        .committed_sequence(&state.host_epoch)
        .await
        .unwrap();
    for _ in 0..100 {
        crate::publish_event(
            &state,
            wonder_api::WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
    }
    client
        .call(
            "events.subscribe",
            json!({"batch": true, "after": {"hostEpoch": state.host_epoch, "sequence": before}}),
        )
        .await
        .unwrap();
    let frame = within(client.notification()).await.unwrap();
    assert_eq!(frame.method, "events.batch");
    let events = frame.params["events"].as_array().unwrap();
    assert_eq!(events.len(), 100);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence"], before + index as u64 + 1);
    }
    eprintln!("RELAY_REPLAY_EVENTS=100; LEGACY_FRAMES=100; BATCHED_FRAMES=1");
}

#[tokio::test]
async fn ended_subscriptions_free_their_places_for_a_fresh_one() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let client = owner(&state).await;
    client.initialize("test").await.unwrap();
    // More resyncs than a connection may hold subscriptions at once.
    for _ in 0..12 {
        client
            .call(
                "events.subscribe",
                json!({"after": {"hostEpoch": "old-epoch", "sequence": 1}}),
            )
            .await
            .unwrap();
        let ended = within(client.notification()).await.unwrap();
        assert_eq!(ended.method, "events.ended");
    }
    // The recovery subscription is accepted and follows new events.
    client.call("events.subscribe", json!({})).await.unwrap();
    let published = crate::publish_event(
        &state,
        wonder_api::WonderEvent::HostStatus {
            state: "ready".into(),
        },
    )
    .await
    .unwrap();
    let event = within(client.notification()).await.unwrap();
    assert_eq!(event.method, "event");
    assert_eq!(event.params["event"]["sequence"], published.sequence);
}

/// A paired phone as the event socket tests set one up: a device key, a
/// session with its CSRF token, and the public origin.
pub(crate) async fn paired_phone(state: &AppState) -> SigningKey {
    let key = SigningKey::from_slice(&[21_u8; 32]).unwrap();
    let point = key.verifying_key().to_sec1_point(false);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut pairing = state.pairing.lock().await;
    pairing.restore_device(
        "phone".into(),
        wonder_api::pairing_protocol::DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: b64.encode(point.x().unwrap()),
            y: b64.encode(point.y().unwrap()),
        },
        false,
        None,
    );
    pairing.restore_session(
        wonder_api::pairing_protocol::session_token_hash("session-token"),
        "phone".into(),
        sha2::Sha256::digest(b"csrf-token").into(),
        now_ms() + 600_000,
    );
    drop(pairing);
    state
        .store
        .upsert_owner_device("phone", "Phone", "{}", "now")
        .await
        .unwrap();
    key
}

async fn phone_socket(state: &AppState) -> Client {
    let url = serve(state).await;
    Client::connect(
        &url,
        &[
            ("cookie", "__Host-wonder_session=session-token"),
            ("origin", &state.public_origin),
        ],
    )
    .await
    .unwrap()
}

pub(crate) fn signed_challenge(key: &SigningKey, challenge: &Value) -> Value {
    let challenge: wonder_api::pairing_protocol::Challenge =
        serde_json::from_value(challenge.clone()).unwrap();
    let transcript = wonder_api::pairing::ChallengeTranscript {
        device_id: "phone",
        challenge_id: &challenge.challenge_id,
        nonce: &challenge.nonce,
        origin: &challenge.origin,
        host_installation_id: &challenge.host_installation_id,
        issued_at_ms: challenge.issued_at_ms,
        expires_at_ms: challenge.expires_at_ms,
    };
    let signature: Signature = key.sign(&transcript.to_bytes());
    json!({
        "deviceId": "phone",
        "csrfToken": "csrf-token",
        "challengeId": challenge.challenge_id,
        "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    })
}

#[tokio::test]
async fn a_paired_device_proves_its_session_and_keeps_todays_limits() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let key = paired_phone(&state).await;
    // Without the session cookie, or from another origin, no socket.
    let url = serve(&state).await;
    assert!(Client::connect(&url, &[("origin", &state.public_origin)])
        .await
        .is_err());
    assert!(Client::connect(
        &url,
        &[
            ("cookie", "__Host-wonder_session=session-token"),
            ("origin", "https://evil.example")
        ]
    )
    .await
    .is_err());

    let phone = phone_socket(&state).await;
    device_contract(&state, &key, phone).await;
}

/// What a paired device may and may not do over one socket, the same over
/// `/api/v1/rpc` and sealed over `/api/v1/rpc/sealed`.
async fn device_contract(state: &AppState, key: &SigningKey, phone: Client) {
    let init =
        |auth: Value| json!({"protocolVersion": 1, "clientInfo": {"name": "phone"}, "auth": auth});
    // The session alone is not enough: initialize needs the signed challenge.
    assert_eq!(
        code(
            phone
                .call(
                    "initialize",
                    json!({"protocolVersion": 1, "clientInfo": {"name": "phone"}})
                )
                .await
        ),
        codes::INVALID_PARAMS
    );
    let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
    let mut forged = signed_challenge(key, &challenge);
    forged["csrfToken"] = "wrong-csrf".into();
    assert_eq!(
        code(phone.call("initialize", init(forged)).await),
        codes::for_http_status(401)
    );
    let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
    let result = phone
        .call("initialize", init(signed_challenge(key, &challenge)))
        .await
        .unwrap();
    assert_eq!(result["authority"], "device");
    assert_eq!(result["deviceId"], "phone");

    // Reads work; owner-only and signed actions are refused as over HTTP.
    let status = phone.call("host.status", Value::Null).await.unwrap();
    crate::tests::validate_http_contract("hostStatus", &status);
    assert_eq!(
        code(phone.call("pairing.offer", Value::Null).await),
        codes::for_http_status(403)
    );
    assert_eq!(
        code(phone.call("host.providers", Value::Null).await),
        codes::for_http_status(403)
    );
    assert_eq!(
        code(
            phone
                .call("devices.revoke", json!({"deviceId": "owner"}))
                .await
        ),
        codes::for_http_status(401)
    );
    // Only the owner, on the computer, allows the relay; a phone hands over
    // a grant only with its signature.
    assert_eq!(
        code(phone.call("relay.set", json!({"allowed": true})).await),
        codes::for_http_status(403)
    );
    assert_eq!(
        code(phone.call("relay.enable", example("relay.enable")).await),
        codes::for_http_status(401)
    );
    assert!(!crate::relay::read_settings(&state.data_dir).allowed);

    // Revoking the device ends its socket at once.
    state.revocations.send("phone".into()).unwrap();
    let closed = within(async {
        loop {
            match phone.call("host.status", Value::Null).await {
                Err(CallError::Closed) => break,
                _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    let () = closed;
}

/// A socket opened over a second trusted route without an Origin header (a
/// native client's upgrade) keeps that route: its challenges name it, so the
/// phone's signature verifies, and its calls keep passing the origin check.
/// (Home network origins are trusted only on their own listener; see
/// `lan::tests::a_phone_signs_in_over_the_home_route_and_its_socket_ends_with_the_listener`.)
#[tokio::test]
async fn a_socket_over_a_secondary_route_without_origin_gets_that_routes_challenge() {
    const SECOND: &str = "https://wonder.example.com";
    let (dir, mut state) = crate::ingestion::tests::fixture().await;
    let status_file = dir.path().join("origin-status.jsonl");
    std::fs::write(
        &status_file,
        format!(
            r#"{{"state":"ready","origin":"{}","routes":[{{"kind":"custom","origin":"{SECOND}"}}]}}"#,
            state.public_origin
        ),
    )
    .unwrap();
    state.public_origin_file = Some(status_file);
    let key = paired_phone(&state).await;
    let url = serve(&state).await;
    let cookie = ("cookie", "__Host-wonder_session=session-token");
    // A host outside the trusted set gets no socket.
    assert!(
        Client::connect(&url, &[cookie, ("host", "wonder.example.org")])
            .await
            .is_err()
    );

    let init =
        |auth: Value| json!({"protocolVersion": 1, "clientInfo": {"name": "phone"}, "auth": auth});
    for (headers, route) in [
        (vec![cookie, ("host", "wonder.example.com")], SECOND),
        (
            vec![cookie, ("origin", state.public_origin.as_str())],
            state.public_origin.as_str(),
        ),
    ] {
        let phone = Client::connect(&url, &headers).await.unwrap();
        let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
        assert_eq!(challenge["origin"], route);
        let result = phone
            .call("initialize", init(signed_challenge(&key, &challenge)))
            .await
            .unwrap();
        assert_eq!(result["authority"], "device");
        let status = phone.call("host.status", Value::Null).await.unwrap();
        crate::tests::validate_http_contract("hostStatus", &status);
    }
}

#[tokio::test]
async fn revocation_closes_the_socket_of_a_device_that_stopped_reading() {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let key = paired_phone(&state).await;
    let mut request = serve(&state).await.into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "cookie",
        "__Host-wonder_session=session-token".parse().unwrap(),
    );
    headers.insert("origin", state.public_origin.parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (mut sink, mut stream) = socket.split();
    let mut answer = async |frame: Value| -> Value {
        sink.send(Message::text(frame.to_string())).await.unwrap();
        loop {
            if let Some(Ok(Message::Text(text))) = stream.next().await {
                return serde_json::from_str::<Value>(&text).unwrap()["result"].clone();
            }
        }
    };
    let challenge = answer(json!({"jsonrpc": "2.0", "id": 1, "method": "auth.challenge"})).await;
    let init = answer(
        json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {
            "protocolVersion": 1, "clientInfo": {"name": "phone"},
            "auth": signed_challenge(&key, &challenge),
        }}),
    )
    .await;
    assert_eq!(init["authority"], "device");

    // Requests with large answers, never read, fill the reply queue and the
    // socket buffers until the host has nowhere to put the next answer.
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = sent.clone();
    let flood = tokio::spawn(async move {
        let method = "x".repeat(100 * 1024);
        for id in 0..2_000 {
            let frame = json!({"jsonrpc": "2.0", "id": id, "method": method});
            if sink.send(Message::text(frame.to_string())).await.is_err() {
                break;
            }
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });
    let mut last = usize::MAX;
    while sent.load(Ordering::Relaxed) != last {
        last = sent.load(Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        last < 2_000,
        "the host read every request without backpressure"
    );

    state.revocations.send("phone".into()).unwrap();
    // The host drops the connection. Answers already in this client's receive
    // buffer may still arrive first (macOS delivers them ahead of the reset;
    // Linux usually does not), so the contract is that the stream ends soon,
    // not that the very next frame is the close.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => continue,
                Some(Ok(Message::Ping(_)))
                | Some(Ok(Message::Pong(_)))
                | Some(Ok(Message::Frame(_))) => continue,
                _ => break,
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "the revoked socket was still open 10 s after revocation"
    );
    flood.abort();
}

// ----- Sealed frames over the relay -----

const RELAY: &str = "https://r-k7x2q.getcupola.app";

/// What the edge saw on one proxied connection.
#[derive(Clone, Debug)]
enum Seen {
    /// The upgrade request's headers, as the edge received them.
    Upgrade(Vec<(String, String)>),
    Text {
        from_client: bool,
        text: String,
    },
    Binary {
        from_client: bool,
        bytes: Vec<u8>,
    },
}

/// A WebSocket proxy in place of the relay's edge: it ends the client's
/// connection, opens its own to the host on the same path with the same
/// headers, and logs every message it forwards. With `tamper: Some(n)` it
/// flips one bit of the `n`th binary message from the client (0-based).
struct Edge {
    base: String,
    log: std::sync::Arc<std::sync::Mutex<Vec<Seen>>>,
}

impl Edge {
    async fn start(host: &str, tamper: Option<usize>) -> Self {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{
            client::IntoClientRequest,
            handshake::server::{Request, Response},
            Message,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", listener.local_addr().unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let host = host.to_owned();
        let seen = log.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (host, seen) = (host.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut upgrade = None;
                    let client = tokio_tungstenite::accept_hdr_async(
                        tcp,
                        |request: &Request, response: Response| {
                            upgrade = Some((
                                request.uri().path().to_owned(),
                                request
                                    .headers()
                                    .iter()
                                    .map(|(name, value)| {
                                        (
                                            name.as_str().to_owned(),
                                            value.to_str().unwrap_or_default().to_owned(),
                                        )
                                    })
                                    .collect::<Vec<_>>(),
                            ));
                            Ok(response)
                        },
                    )
                    .await
                    .unwrap();
                    let (path, headers) = upgrade.unwrap();
                    seen.lock().unwrap().push(Seen::Upgrade(headers.clone()));
                    let mut request = format!("{host}{path}").into_client_request().unwrap();
                    for (name, value) in &headers {
                        // Like cloudflared: the relay hostname stays the Host.
                        if matches!(name.as_str(), "origin" | "cookie" | "host") {
                            request.headers_mut().insert(
                                name.parse::<axum::http::HeaderName>().unwrap(),
                                value.parse().unwrap(),
                            );
                        }
                    }
                    let Ok((upstream, _)) = tokio_tungstenite::connect_async(request).await else {
                        return;
                    };
                    let (mut client_sink, mut client_stream) = client.split();
                    let (mut host_sink, mut host_stream) = upstream.split();
                    let up_seen = seen.clone();
                    let up = async move {
                        let mut binary = 0;
                        while let Some(Ok(message)) = client_stream.next().await {
                            let message = match message {
                                Message::Text(text) => {
                                    up_seen.lock().unwrap().push(Seen::Text {
                                        from_client: true,
                                        text: text.as_str().to_owned(),
                                    });
                                    Message::Text(text)
                                }
                                Message::Binary(bytes) => {
                                    let mut bytes = bytes.to_vec();
                                    up_seen.lock().unwrap().push(Seen::Binary {
                                        from_client: true,
                                        bytes: bytes.clone(),
                                    });
                                    if tamper == Some(binary) {
                                        bytes[0] ^= 1;
                                    }
                                    binary += 1;
                                    Message::binary(bytes)
                                }
                                Message::Close(_) => break,
                                other => other,
                            };
                            if host_sink.send(message).await.is_err() {
                                break;
                            }
                        }
                        let _ = host_sink.close().await;
                    };
                    let down = async move {
                        while let Some(Ok(message)) = host_stream.next().await {
                            match &message {
                                Message::Text(text) => seen.lock().unwrap().push(Seen::Text {
                                    from_client: false,
                                    text: text.as_str().to_owned(),
                                }),
                                Message::Binary(bytes) => seen.lock().unwrap().push(Seen::Binary {
                                    from_client: false,
                                    bytes: bytes.to_vec(),
                                }),
                                Message::Close(_) => break,
                                _ => {}
                            }
                            if client_sink.send(message).await.is_err() {
                                break;
                            }
                        }
                        let _ = client_sink.close().await;
                    };
                    // Either side ending ends the proxied connection.
                    tokio::select! { _ = up => {}, _ = down => {} }
                });
            }
        });
        Self { base, log }
    }

    fn seen(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }
}

/// A host whose trusted routes include the relay origin, with the relay on.
async fn relay_host() -> (tempfile::TempDir, AppState) {
    let (dir, mut state) = crate::ingestion::tests::fixture().await;
    let status_file = dir.path().join("origin-status.jsonl");
    std::fs::write(
        &status_file,
        format!(
            r#"{{"state":"ready","origin":"{}","routes":[{{"kind":"relay","origin":"{RELAY}"}}]}}"#,
            state.public_origin
        ),
    )
    .unwrap();
    state.public_origin_file = Some(status_file);
    std::fs::write(
        state.data_dir.join(crate::relay::STATUS_FILE),
        format!(r#"{{"enabled":true,"state":"ready","label":"k7x2q","origin":"{RELAY}"}}"#),
    )
    .unwrap();
    (dir, state)
}

fn host_pin(state: &AppState) -> String {
    crate::lan::load_or_create_host_key(&state.data_dir)
        .unwrap()
        .pin()
}

async fn sealed_phone(base: &str, state: &AppState, session: &str) -> Client {
    Client::connect_sealed(
        &format!("{base}/api/v1/rpc/sealed"),
        RELAY,
        &host_pin(state),
        session,
    )
    .await
    .unwrap()
}

/// The R3 check: the edge logs only ciphertext while the device contract
/// holds sealed, end to end through the proxy.
#[tokio::test]
async fn the_relay_edge_sees_only_ciphertext_while_the_device_contract_holds_sealed() {
    let (_dir, state) = relay_host().await;
    let key = paired_phone(&state).await;
    let host = serve(&state).await;
    let edge = Edge::start(host.trim_end_matches("/api/v1/rpc"), None).await;
    let phone = sealed_phone(&edge.base, &state, "session-token").await;

    // Events flow sealed too.
    let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
    assert_eq!(
        challenge["origin"], RELAY,
        "the challenge names the relay route"
    );
    let result = phone
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"},
                   "auth": signed_challenge(&key, &challenge)}),
        )
        .await
        .unwrap();
    assert_eq!(result["authority"], "device");
    assert_eq!(result["capabilities"]["eventBatches"], true);
    phone
        .call("events.subscribe", json!({"batch": true}))
        .await
        .unwrap();
    let published = crate::publish_event(
        &state,
        wonder_api::WonderEvent::HostStatus {
            state: "ready".into(),
        },
    )
    .await
    .unwrap();
    let event = within(phone.notification()).await.unwrap();
    assert_eq!(event.method, "events.batch");
    assert_eq!(event.params["events"][0]["sequence"], published.sequence);
    let status = phone.call("host.status", Value::Null).await.unwrap();
    crate::tests::validate_http_contract("hostStatus", &status);
    drop(phone);

    // The rest of the device contract, on a fresh sealed socket.
    device_contract(
        &state,
        &key,
        sealed_phone(&edge.base, &state, "session-token").await,
    )
    .await;

    let seen = edge.seen();
    let secrets = [
        "session-token",
        "csrf-token",
        "jsonrpc",
        "auth.challenge",
        "initialize",
        "host.status",
        "events.subscribe",
        "hostStatus",
        "authority",
        "phone",
        &state.host_epoch,
        &state.host_installation_id,
    ];
    let mut binary = 0;
    let mut texts = Vec::new();
    for entry in &seen {
        match entry {
            Seen::Upgrade(headers) => {
                assert!(
                    headers
                        .iter()
                        .all(|(name, value)| name != "cookie" && !value.contains("session-token")),
                    "the upgrade carried a credential: {headers:?}"
                );
            }
            Seen::Text { from_client, text } => texts.push((*from_client, text.clone())),
            Seen::Binary { bytes, .. } => {
                binary += 1;
                for secret in secrets {
                    assert!(
                        !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                        "the edge read {secret:?}"
                    );
                }
            }
        }
    }
    // Per connection: the hello and the welcome, nothing else in the clear.
    assert_eq!(texts.len(), 4, "{texts:?}");
    for (from_client, text) in &texts {
        let value: Value = serde_json::from_str(text).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        if *from_client {
            assert_eq!(keys, ["clientKey", "v"]);
        } else {
            assert_eq!(keys, ["proof", "serverKey"]);
        }
        for secret in secrets {
            assert!(!text.contains(secret), "the handshake showed {secret:?}");
        }
    }
    assert!(binary > 20, "only {binary} sealed frames crossed the edge");
    let log: Vec<String> = seen
        .iter()
        .map(|entry| match entry {
            Seen::Upgrade(headers) => format!("upgrade {headers:?}"),
            Seen::Text { from_client, text } => format!("text from_client={from_client} {text}"),
            Seen::Binary { from_client, bytes } => format!(
                "binary from_client={from_client} {} bytes {}",
                bytes.len(),
                hex::encode(&bytes[..bytes.len().min(24)])
            ),
        })
        .collect();
    if let Some(path) = std::env::var_os("WONDER_R3_EDGE_LOG") {
        std::fs::write(path, log.join("\n")).unwrap();
    }
}

/// The phone's own HTTP API calls run inside the sealed channel with its
/// session and CSRF token, answer with the route's status and body (a
/// refusal is an answer, not an RPC error), and reach only device API paths.
/// Only a sealed device connection can use it.
#[tokio::test]
async fn a_sealed_phone_makes_its_http_calls_inside_the_channel() {
    use base64::Engine as _;
    let (_dir, state) = relay_host().await;
    let key = paired_phone(&state).await;
    let host = serve(&state).await;
    let edge = Edge::start(host.trim_end_matches("/api/v1/rpc"), None).await;
    let phone = sealed_phone(&edge.base, &state, "session-token").await;
    let request = |method: &str, path: &str, body: Option<&str>| json!({"method": method, "path": path, "body": body});
    // Not before initialize.
    assert_eq!(
        code(
            phone
                .call("http.request", request("GET", "/api/v1/host/status", None))
                .await
        ),
        codes::NOT_INITIALIZED
    );
    let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
    phone
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"},
                   "auth": signed_challenge(&key, &challenge)}),
        )
        .await
        .unwrap();
    let body = |answer: &Value| {
        base64::engine::general_purpose::STANDARD
            .decode(answer["body"].as_str().unwrap())
            .unwrap()
    };

    let answer = phone
        .call("http.request", request("GET", "/api/v1/host/status", None))
        .await
        .unwrap();
    validate_result("http.request", &answer);
    assert_eq!(answer["status"], 200);
    let status: Value = serde_json::from_slice(&body(&answer)).unwrap();
    crate::tests::validate_http_contract("hostStatus", &status);

    // A POST carries the session and CSRF token: the device is signed in, so
    // the route asks for the action signature it needs.
    let answer = phone
        .call(
            "http.request",
            request(
                "POST",
                "/api/v1/relay/enable",
                Some(r#"{"grant":"g","label":"k7xq2abcdefghijklmnopqrstu"}"#),
            ),
        )
        .await
        .unwrap();
    assert_eq!(answer["status"], 401);
    assert_eq!(body(&answer), b"signed action fields are required");

    // A refusal keeps its status for the phone to read.
    let answer = phone
        .call(
            "http.request",
            request("GET", &format!("/api/v1/conversations/{THREAD}/nope"), None),
        )
        .await
        .unwrap();
    assert_eq!(answer["status"], 404);

    for path in [
        "/api/v1/events",
        "/api/v1/rpc",
        "/api/v1/rpc/sealed",
        "/api/v1/pairing/claim",
        "/api/v1/pairing/code",
        "/api/v1/pairing/offers",
        "/api/v1/pairing/session/other",
        "/api/v1/mcp",
        "/api/v1/terminals/term_1/socket",
        "/api/v1/../readyz",
        "/api/v1//host/status",
        "/readyz",
        "/pair",
        "/api/v1/host/status#x",
    ] {
        assert_eq!(
            code(phone.call("http.request", request("GET", path, None)).await),
            codes::for_http_status(404),
            "{path}"
        );
    }
    // The phone renews its session through the channel, as it does at home:
    // the challenge names the relay route the phone used.
    let answer = phone
        .call(
            "http.request",
            request(
                "POST",
                "/api/v1/pairing/session/refresh-challenge",
                Some(r#"{"deviceId":"phone"}"#),
            ),
        )
        .await
        .unwrap();
    assert_eq!(answer["status"], 200, "{answer}");
    let challenge: Value = serde_json::from_slice(&body(&answer)).unwrap();
    assert_eq!(challenge["origin"], RELAY);
    let signed = signed_challenge(&key, &challenge);
    let renewal = json!({"challengeId": signed["challengeId"], "signature": signed["signature"]});
    let answer = phone
        .call(
            "http.request",
            request(
                "POST",
                "/api/v1/pairing/session",
                Some(&renewal.to_string()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(answer["status"], 200, "{answer}");
    let credential: Value = serde_json::from_slice(&body(&answer)).unwrap();
    assert_eq!(credential["deviceId"], "phone");

    assert_eq!(
        code(
            phone
                .call(
                    "http.request",
                    request("TRACE", "/api/v1/host/status", None)
                )
                .await
        ),
        codes::INVALID_PARAMS
    );
    drop(phone);

    // The plain socket and the owner have their own routes; not this.
    let plain = phone_socket(&state).await;
    let challenge = plain.call("auth.challenge", Value::Null).await.unwrap();
    plain
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"},
                   "auth": signed_challenge(&key, &challenge)}),
        )
        .await
        .unwrap();
    assert_eq!(
        code(plain.call("http.request", example("http.request")).await),
        codes::for_http_status(403)
    );
    let owner = owner(&state).await;
    owner.initialize("relay-test").await.unwrap();
    assert_eq!(
        code(owner.call("http.request", example("http.request")).await),
        codes::for_http_status(403)
    );
}

/// The table the phone's relay connection test serves from: the session
/// renewal and device calls it makes on the relay are served, while claims,
/// offers, sockets and MCP stay refused.
#[test]
fn sealed_http_paths_match_the_phones_table() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/contracts/native/sealed-http-paths.json");
    let table: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for (key, served) in [("served", true), ("refused", false)] {
        let paths = table[key].as_array().unwrap();
        assert!(!paths.is_empty(), "{key}");
        for path in paths {
            let path = path.as_str().unwrap();
            assert_eq!(super::sealed_http_path(path), served, "{path}");
        }
    }
}

/// The relay is the owner's choice on this computer: until they allow it a
/// grant is refused, and turning it off clears the choice.
#[tokio::test]
async fn the_owner_allows_the_relay_before_a_grant_can_start_it() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let owner = owner(&state).await;
    owner.initialize("relay-test").await.unwrap();
    let status = owner.call("relay.status", Value::Null).await.unwrap();
    validate_result("relay.status", &status);
    assert_eq!(status["allowed"], false);
    assert_eq!(status["state"], "off");
    assert_eq!(
        code(owner.call("relay.enable", example("relay.enable")).await),
        codes::for_http_status(403)
    );
    let allowed = owner
        .call("relay.set", json!({"allowed": true}))
        .await
        .unwrap();
    validate_result("relay.set", &allowed);
    assert_eq!(allowed["allowed"], true);
    assert_eq!(allowed["enabled"], false);
    assert_eq!(allowed["detail"], crate::relay::WAITING_MESSAGE);
    // A malformed grant is refused before anything starts.
    assert_eq!(
        code(
            owner
                .call(
                    "relay.enable",
                    json!({"grant": "short", "label": "k7xq2abcdefghijklmnopqrstu"})
                )
                .await
        ),
        codes::for_http_status(400)
    );
    let off = owner
        .call("relay.set", json!({"allowed": false}))
        .await
        .unwrap();
    assert_eq!(off["allowed"], false);
    assert!(!crate::relay::read_settings(&state.data_dir).allowed);
}

#[tokio::test]
async fn a_tampered_frame_closes_the_sealed_socket() {
    let (_dir, state) = relay_host().await;
    paired_phone(&state).await;
    let host = serve(&state).await;
    // Frame 0 is the session; frame 1 is the first call.
    let edge = Edge::start(host.trim_end_matches("/api/v1/rpc"), Some(1)).await;
    let phone = sealed_phone(&edge.base, &state, "session-token").await;
    assert!(matches!(
        within(phone.call("auth.challenge", Value::Null)).await,
        Err(CallError::Closed)
    ));
    within(async {
        while !phone.is_closed() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;

    // A tampered session frame never reaches a session either.
    let edge = Edge::start(host.trim_end_matches("/api/v1/rpc"), Some(0)).await;
    let phone = sealed_phone(&edge.base, &state, "session-token").await;
    assert!(matches!(
        within(phone.call("auth.challenge", Value::Null)).await,
        Err(CallError::Closed)
    ));
}

/// Unauthenticated sealed handshakes are capped: sockets that open and never
/// say hello take the room, the next one is refused, and room comes back as
/// they end.
#[tokio::test]
async fn sealed_handshakes_in_progress_are_capped() {
    let (_dir, state) = relay_host().await;
    let url = serve(&state).await;
    let sealed = format!("{}/api/v1/rpc/sealed", url.trim_end_matches("/api/v1/rpc"));
    let open = || async {
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                sealed.as_str(),
            )
            .unwrap();
        request
            .headers_mut()
            .insert("origin", RELAY.parse().unwrap());
        request
            .headers_mut()
            .insert("host", "r-k7x2q.getcupola.app".parse().unwrap());
        tokio_tungstenite::connect_async(request).await
    };
    let mut held = Vec::new();
    for _ in 0..super::server::MAX_OPEN_HANDSHAKES {
        held.push(open().await.unwrap().0);
    }
    let refused = open().await.expect_err("a handshake past the cap");
    assert!(
        matches!(
            &refused,
            tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 503
        ),
        "{refused:?}"
    );
    drop(held.pop());
    within(async {
        while open().await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// HUNT-RELAY finding 3: relay hostnames are public (certificate
/// transparency), the handshake needs no credential, and one pool of places
/// served everyone, so one source holding 32 idle sockets locked every phone
/// out of the relay. Cloudflare names the real client in `CF-Connecting-IP`;
/// one client address (an IPv6 /64) may hold only a few of the places.
#[tokio::test]
async fn one_source_cannot_take_every_sealed_handshake_place() {
    let (_dir, state) = relay_host().await;
    let url = serve(&state).await;
    let sealed = format!("{}/api/v1/rpc/sealed", url.trim_end_matches("/api/v1/rpc"));
    let open = |source: Option<&'static str>| {
        let sealed = sealed.clone();
        async move {
            let mut request =
                tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                    sealed.as_str(),
                )
                .unwrap();
            let headers = request.headers_mut();
            headers.insert("origin", RELAY.parse().unwrap());
            headers.insert("host", "r-k7x2q.getcupola.app".parse().unwrap());
            if let Some(source) = source {
                headers.insert("cf-connecting-ip", source.parse().unwrap());
            }
            tokio_tungstenite::connect_async(request).await
        }
    };
    let refused = |result: Result<_, tokio_tungstenite::tungstenite::Error>| match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 503);
            true
        }
        Err(other) => panic!("{other:?}"),
        Ok(_) => false,
    };
    let mut held = Vec::new();
    for source in ["203.0.113.9", "2001:db8:1:2::1"] {
        for _ in 0..super::server::MAX_HANDSHAKES_PER_SOURCE {
            let (socket, _) = open(Some(source)).await.expect("a place for this source");
            held.push(socket);
        }
    }
    assert!(
        refused(open(Some("203.0.113.9")).await),
        "one address, one share"
    );
    // The same /64 is the same source.
    assert!(
        refused(open(Some("2001:db8:1:2:ffff::7")).await),
        "one /64, one share"
    );
    // Other sources, and a request with no source at all, still have room.
    for source in [Some("198.51.100.7"), Some("2001:db8:1:3::1"), None] {
        held.push(open(source).await.expect("another source").0);
    }
    // A place comes back when a socket ends.
    drop(held.remove(0));
    within(async {
        while refused(open(Some("203.0.113.9")).await) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// Upgrades a raw TCP connection to `path` with `headers`, so a test can send
/// WebSocket frames a conforming client never would.
async fn raw_upgrade(url: &str, path: &str, headers: &[(&str, &str)]) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = url
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let mut tcp = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\
         sec-websocket-version: 13\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    );
    if !headers.iter().any(|(name, _)| *name == "host") {
        request += &format!("host: {addr}\r\n");
    }
    for (name, value) in headers {
        request += &format!("{name}: {value}\r\n");
    }
    tcp.write_all(format!("{request}\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(tcp.read_u8().await.unwrap());
    }
    assert!(
        head.starts_with(b"HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    tcp
}

/// A masked binary frame header that promises `length` payload bytes.
fn frame_header(length: u64) -> Vec<u8> {
    let mut header = vec![0x82, 0x80 | 127];
    header.extend_from_slice(&length.to_be_bytes());
    header.extend_from_slice(&[0; 4]);
    header
}

/// HUNT-RELAY finding 1: tungstenite caps a frame at 16 MiB unless told
/// otherwise and only checks the message cap once the whole frame has been
/// buffered, so a sealed handshake (no credential yet, 32 at a time) could be
/// made to hold 16 MiB each. A frame that promises more than the connection
/// may ever carry must end it from the header alone.
#[tokio::test]
async fn a_frame_promising_more_than_a_connection_carries_ends_it_at_the_header() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_dir, state) = relay_host().await;
    let url = serve(&state).await;
    let capability = state.loopback_capability.clone();
    for (path, headers) in [
        (
            "/api/v1/rpc/sealed",
            vec![("origin", RELAY), ("host", "r-k7x2q.getcupola.app")],
        ),
        (
            "/api/v1/rpc",
            vec![("x-wonder-loopback-capability", capability.as_str())],
        ),
    ] {
        let mut tcp = raw_upgrade(&url, path, &headers).await;
        tcp.write_all(&frame_header(8 * 1024 * 1024)).await.unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut sink = [0_u8; 256];
            // A close frame, then EOF; a reset also ends it.
            while matches!(tcp.read(&mut sink).await, Ok(n) if n > 0) {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "{path}: the host waited for an 8 MiB frame it can never accept"
        );
    }
}

#[tokio::test]
async fn rpc_over_the_relay_address_is_sealed_or_refused() {
    let (_dir, state) = relay_host().await;
    let key = paired_phone(&state).await;
    let url = serve(&state).await;
    let base = url.trim_end_matches("/api/v1/rpc").to_owned();
    let cookie = ("cookie", "__Host-wonder_session=session-token");
    // Plaintext RPC is refused on the relay address however it is named,
    // and still served on the other routes.
    for host in ["r-k7x2q.getcupola.app", "R-K7X2Q.getcupola.app.:443"] {
        assert!(
            Client::connect(&url, &[cookie, ("origin", RELAY), ("host", host)])
                .await
                .is_err(),
            "{host}"
        );
    }
    // An Origin naming the relay is enough, whatever Host the edge sent.
    assert!(Client::connect(&url, &[cookie, ("origin", RELAY)])
        .await
        .is_err());
    Client::connect(&url, &[cookie, ("origin", &state.public_origin)])
        .await
        .unwrap();

    // Sealed needs a trusted origin and the pinned key.
    let sealed = format!("{base}/api/v1/rpc/sealed");
    assert!(Client::connect_sealed(
        &sealed,
        "https://evil.example",
        &host_pin(&state),
        "session-token"
    )
    .await
    .is_err());
    let impostor = wonder_api::pairing_protocol::HostKey::generate().pin();
    assert!(
        Client::connect_sealed(&sealed, RELAY, &impostor, "session-token")
            .await
            .is_err()
    );

    // A wrong session, or a revoked device, gets no calls.
    let phone = sealed_phone(&base, &state, "not-a-session").await;
    assert!(matches!(
        within(phone.call("auth.challenge", Value::Null)).await,
        Err(CallError::Closed)
    ));

    // A cookie on the sealed upgrade is not a session: only the sealed one is.
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            sealed.as_str(),
        )
        .unwrap();
    request
        .headers_mut()
        .insert("origin", RELAY.parse().unwrap());
    request
        .headers_mut()
        .insert("host", "r-k7x2q.getcupola.app".parse().unwrap());
    request.headers_mut().insert(
        "cookie",
        "__Host-wonder_session=session-token".parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    use futures_util::{SinkExt, StreamExt};
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(
            json!({"jsonrpc": "2.0", "id": 1, "method": "auth.challenge"}).to_string(),
        ))
        .await
        .unwrap();
    let answer = within(socket.next()).await;
    assert!(
        !matches!(answer, Some(Ok(tokio_tungstenite::tungstenite::Message::Text(ref t))) if t.contains("result")),
        "{answer:?}"
    );

    // The right session works, and the sealed socket also ends on revocation
    // (covered by `device_contract`).
    let phone = sealed_phone(&base, &state, "session-token").await;
    let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
    let result = phone
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"},
                   "auth": signed_challenge(&key, &challenge)}),
        )
        .await
        .unwrap();
    assert_eq!(result["deviceId"], "phone");
}

/// Answers the host's renewal challenge, signed with `key` as the phone's
/// device key.
fn renewal_signer(
    key: &SigningKey,
) -> impl FnOnce(&wonder_api::pairing_protocol::Challenge) -> Value + '_ {
    move |challenge| {
        let signed = signed_challenge(key, &serde_json::to_value(challenge).unwrap());
        json!({"challengeId": signed["challengeId"], "signature": signed["signature"]})
    }
}

/// B-038: a device whose session ended while it could reach this host only
/// through the relay signs in again inside the sealed channel, by proving its
/// paired key against a fresh challenge for the relay origin. Revoked devices,
/// other keys and challenges from elsewhere get nothing, and no call is served
/// before the proof.
#[tokio::test]
async fn an_expired_session_renews_over_the_relay_only_with_the_paired_key() {
    let (_dir, state) = relay_host().await;
    let key = paired_phone(&state).await;
    state.pairing.lock().await.restore_session(
        wonder_api::pairing_protocol::session_token_hash("expired-token"),
        "phone".into(),
        sha2::Sha256::digest(b"csrf-token").into(),
        now_ms() - 1,
    );
    let url = serve(&state).await;
    let sealed = format!("{}/api/v1/rpc/sealed", url.trim_end_matches("/api/v1/rpc"));
    let pin = host_pin(&state);

    // The ended session alone gets no calls.
    let phone = Client::connect_sealed(&sealed, RELAY, &pin, "expired-token")
        .await
        .unwrap();
    assert!(matches!(
        within(phone.call("auth.challenge", Value::Null)).await,
        Err(CallError::Closed)
    ));

    // Another key, or a challenge issued for another route, is refused.
    let impostor = SigningKey::from_slice(&[22_u8; 32]).unwrap();
    let refused = within(Client::renew_sealed(
        &sealed,
        RELAY,
        &pin,
        "phone",
        renewal_signer(&impostor),
    ))
    .await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("Pair it again")),
        "{:?}",
        refused.map(|(_, renewed)| renewed)
    );
    let elsewhere = state
        .pairing
        .lock()
        .await
        .issue_fresh_challenge(
            "phone",
            &state.public_origin,
            &state.host_installation_id,
            now_ms(),
        )
        .unwrap();
    let replayed = within(Client::renew_sealed(&sealed, RELAY, &pin, "phone", |_| {
        renewal_signer(&key)(&elsewhere)
    }))
    .await;
    assert!(replayed.is_err());

    // Nothing but the signed answer is read before the proof: a call, or an
    // answer without a valid signature, ends the connection.
    for answer in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "auth.challenge"}),
        json!({"challengeId": "", "signature": "not-a-signature"}),
    ] {
        let early = within(Client::renew_sealed(&sealed, RELAY, &pin, "phone", |c| {
            let mut answer = answer.clone();
            if answer.get("challengeId").is_some() {
                answer["challengeId"] = c.challenge_id.clone().into();
            }
            answer
        }))
        .await;
        assert!(early.is_err(), "{answer}");
    }

    // The paired key renews: the challenge names the relay origin, and the
    // new session opens the usual device protocol.
    let mut seen = None;
    let (phone, renewed) = within(Client::renew_sealed(&sealed, RELAY, &pin, "phone", |c| {
        seen = Some(c.clone());
        renewal_signer(&key)(c)
    }))
    .await
    .unwrap();
    let seen = seen.unwrap();
    assert_eq!(
        (seen.origin.as_str(), seen.device_id.as_str()),
        (RELAY, "phone")
    );
    assert_eq!(renewed.device_id, "phone");
    assert_ne!(renewed.session_token, "expired-token");
    assert!(renewed.expires_at_ms.unwrap() > now_ms());
    let challenge = phone.call("auth.challenge", Value::Null).await.unwrap();
    let mut auth = signed_challenge(&key, &challenge);
    auth["csrfToken"] = renewed.csrf_token.clone().into();
    let result = phone
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"}, "auth": auth}),
        )
        .await
        .unwrap();
    assert_eq!(result["deviceId"], "phone");
    phone.call("host.status", Value::Null).await.unwrap();
    // It is a real session, kept across a restart like one from pairing.
    let stored = state
        .store
        .list_active_sessions(now_ms())
        .await
        .unwrap()
        .into_iter()
        .any(|s| {
            s.token_hash == wonder_api::pairing_protocol::session_token_hash(&renewed.session_token)
        });
    assert!(stored);
    // The renewed session opens a later sealed connection directly.
    Client::connect_sealed(&sealed, RELAY, &pin, &renewed.session_token)
        .await
        .unwrap()
        .call("auth.challenge", Value::Null)
        .await
        .unwrap();

    // Once revoked, the paired key no longer renews.
    state.pairing.lock().await.revoke_device("phone");
    let revoked = within(Client::renew_sealed(
        &sealed,
        RELAY,
        &pin,
        "phone",
        renewal_signer(&key),
    ))
    .await;
    assert!(
        revoked
            .as_ref()
            .is_err_and(|e| e.to_string().contains("Pair it again")),
        "{:?}",
        revoked.map(|(_, renewed)| renewed)
    );
}

/// Contract: an agent learns only which phone is waiting. Neither RPC readers,
/// journal subscribers, local logs nor saved state can recover its comparison;
/// confirmation is checked by the daemon even when the CLI is bypassed.
#[tokio::test]
async fn pairing_keeps_the_phone_comparison_out_of_rpc_events_logs_and_files() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    for exhausted in [false, true] {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let client = owner(&state).await;
        client.initialize("pairing-secrecy-test").await.unwrap();
        client.call("events.subscribe", json!({})).await.unwrap();
        let offer = client.call("pairing.offer", Value::Null).await.unwrap();
        let key = SigningKey::from_slice(&[17; 32]).unwrap();
        let point = key.verifying_key().to_sec1_point(false);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let response = router(state.clone()).oneshot(Request::builder()
            .method("POST").uri("/api/v1/pairing/code")
            .header("origin", &state.public_origin).header("content-type", "application/json")
            .body(Body::from(json!({"humanCode":offer["humanCode"], "label":"Secrecy fixture phone",
                "publicKey":{"kty":"EC","crv":"P-256","x":b64.encode(point.x().unwrap()),"y":b64.encode(point.y().unwrap())}}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), 200);
        let claim: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        let c: wonder_api::pairing_protocol::Challenge =
            serde_json::from_value(claim["challenge"].clone()).unwrap();
        let transcript = wonder_api::pairing::ChallengeTranscript {
            device_id: &c.device_id,
            challenge_id: &c.challenge_id,
            nonce: &c.nonce,
            origin: &c.origin,
            host_installation_id: &c.host_installation_id,
            issued_at_ms: c.issued_at_ms,
            expires_at_ms: c.expires_at_ms,
        };
        // Independently compute what the native phone displays.
        let phone_code =
            hex::encode_upper(sha2::Sha256::digest(transcript.to_bytes()))[..12].to_owned();
        let pending = client.call("pairing.pending", Value::Null).await.unwrap();
        validate_result("pairing.pending", &pending);
        assert_eq!(pending[0]["deviceId"], c.device_id);
        assert_eq!(pending[0]["offerId"], offer["offerId"]);
        let serialized = pending.to_string();
        assert!(!serialized.contains(&phone_code), "{serialized}");
        assert!(!serialized.contains(&c.nonce), "{serialized}");
        for field in [
            "challenge",
            "verificationText",
            "verificationHash",
            "nonce",
            "challengeId",
        ] {
            assert!(pending[0].get(field).is_none(), "{field}: {pending}");
        }
        assert_eq!(
            code(
                client
                    .call("pairing.confirm", json!({"deviceId":c.device_id}))
                    .await
            ),
            codes::INVALID_PARAMS
        );
        for wrong in ["", "wrong", "000000000000", "1234567890123"] {
            let response = client
                .call(
                    "pairing.confirm",
                    json!({"deviceId":c.device_id,"code":wrong}),
                )
                .await;
            assert_eq!(
                code(response),
                codes::for_http_status(if wrong.is_empty() { 400 } else { 403 })
            );
            assert!(!state
                .store
                .list_owner_devices()
                .await
                .unwrap()
                .iter()
                .any(|d| d.id == c.device_id));
        }
        // Calling the underlying HTTP endpoint cannot bypass the RPC schema.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/pairing/pending/{}/confirm", c.device_id))
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        if exhausted {
            // The three nonempty wrong answers above count toward this
            // enrollment's twenty-guess limit. Reading pending cannot reset it.
            for _ in 3..20 {
                assert_eq!(
                    code(
                        client
                            .call(
                                "pairing.confirm",
                                json!({"deviceId":c.device_id,"code":"wrong"})
                            )
                            .await
                    ),
                    codes::for_http_status(403)
                );
                client.call("pairing.pending", Value::Null).await.unwrap();
            }
            assert_eq!(
                code(
                    client
                        .call(
                            "pairing.confirm",
                            json!({"deviceId":c.device_id,"code":phone_code})
                        )
                        .await
                ),
                codes::for_http_status(429)
            );
            assert!(!state
                .store
                .list_owner_devices()
                .await
                .unwrap()
                .iter()
                .any(|d| d.id == c.device_id));
            client
                .call("pairing.reject", json!({"deviceId":c.device_id}))
                .await
                .unwrap();
        } else {
            assert_eq!(
                client
                    .call(
                        "pairing.confirm",
                        json!({"deviceId":c.device_id,"code":phone_code})
                    )
                    .await
                    .unwrap(),
                json!({})
            );
            assert!(state
                .store
                .list_owner_devices()
                .await
                .unwrap()
                .iter()
                .any(|d| d.id == c.device_id));
            assert_eq!(
                client.call("pairing.pending", Value::Null).await.unwrap(),
                json!([])
            );
            assert!(client
                .call(
                    "pairing.confirm",
                    json!({"deviceId":c.device_id,"code":phone_code})
                )
                .await
                .is_err());
            // Confirmation still requires the phone's original signature to get a session.
            let signature: Signature = key.sign(&transcript.to_bytes());
            let response = router(state.clone()).oneshot(Request::builder().method("POST").uri("/api/v1/pairing/session")
            .header("origin", &state.public_origin).header("content-type", "application/json")
            .body(Body::from(json!({"challengeId":c.challenge_id,"signature":b64.encode(signature.to_bytes())}).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), 200);
        }
        // A marker proves that the real subscription is drained through pairing.
        crate::publish_event(
            &state,
            wonder_api::WonderEvent::HostStatus {
                state: "secrecy-check-complete".into(),
            },
        )
        .await
        .unwrap();
        loop {
            let event = within(client.notification()).await.unwrap();
            let serialized = serde_json::to_string(&event).unwrap();
            assert!(!serialized.contains(&phone_code));
            assert!(!serialized.contains(&c.nonce));
            if event.params["event"]["event"]["data"]["state"] == "secrecy-check-complete" {
                break;
            }
        }
        fn inspect_files(dir: &std::path::Path, code: &[u8], nonce: &[u8]) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    inspect_files(&path, code, nonce);
                } else if path.is_file() {
                    let bytes = std::fs::read(&path).unwrap();
                    assert!(
                        !bytes.windows(code.len()).any(|w| w == code),
                        "code in {}",
                        path.display()
                    );
                    assert!(
                        !bytes.windows(nonce.len()).any(|w| w == nonce),
                        "nonce in {}",
                        path.display()
                    );
                }
            }
        }
        inspect_files(dir.path(), phone_code.as_bytes(), c.nonce.as_bytes());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}

/// Property: any frame a peer sends is answered (a call, a notification or one
/// error), never a panic, and an ID always survives as a path segment: what
/// `segment` makes holds only unreserved characters and `%XX`, and decodes back
/// to the ID, so no ID can add a path segment, a query or a fragment to a
/// routed call.
#[test]
fn arbitrary_frames_and_ids_cannot_panic_the_reader_or_escape_their_segment() {
    let mut fuzz = crate::test_support::Fuzz::new(0xF4A3E);
    let names: Vec<&str> = METHODS.iter().map(|method| method.name).collect();
    for _ in 0..3_000 {
        let text = fuzz.text(48);
        let frame = match fuzz.below(3) {
            0 => text.clone(),
            1 => json!({"jsonrpc": "2.0", "id": fuzz.next() % 1000, "method": text}).to_string(),
            _ => json!({"jsonrpc": "2.0", "id": text, "method": names[fuzz.below(names.len())],
                        "params": {"threadId": text, "deviceId": text, "id": text}})
            .to_string(),
        };
        let _ = read_frame(&frame);
        let encoded = segment(&text);
        assert!(
            encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.~%".contains(&b)),
            "{encoded}"
        );
        let decoded = percent_decode(&encoded);
        assert_eq!(decoded, text.as_bytes(), "{encoded}");
    }
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}
