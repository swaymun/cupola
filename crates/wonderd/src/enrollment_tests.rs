use super::*;
use axum::body::to_bytes;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use tower::ServiceExt;

async fn call(
    state: &AppState,
    path: &str,
    body: serde_json::Value,
    local: bool,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("origin", &state.public_origin);
    if local {
        request = request.header("x-wonder-loopback-capability", &state.loopback_capability);
    }
    let response = router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let data = to_bytes(response.into_body(), 65536).await.unwrap();
    (
        status,
        serde_json::from_slice(&data).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn enrollment_requires_local_confirmation_and_revoke_blocks_renewal() {
    let (dir, mut state) = crate::ingestion::tests::fixture().await;
    state.public_origin = "https://wonder.example.ts.net".into();
    state.public_origin_file = None;
    let key = SigningKey::from_slice(&[7_u8; 32]).unwrap();
    let point = key.verifying_key().to_sec1_point(false);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (status, offer) = call(
        &state,
        "/api/v1/pairing/offers",
        serde_json::Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = serde_json::json!({"humanCode": offer["humanCode"], "publicKey": {"kty":"EC","crv":"P-256", "x":b64.encode(point.x().unwrap()),"y":b64.encode(point.y().unwrap())},"label":"Test iPhone","sessionExpiration":"never"});
    let (status, claim) = call(&state, "/api/v1/pairing/code", body.clone(), false).await;
    assert_eq!(status, StatusCode::OK);
    let id = claim["deviceId"].as_str().unwrap();
    assert!(!state
        .store
        .list_owner_devices()
        .await
        .unwrap()
        .iter()
        .any(|d| d.id == id));
    assert_eq!(
        call(&state, "/api/v1/pairing/code", body, false).await.0,
        StatusCode::GONE
    );
    let challenge: wonder_api::pairing_protocol::Challenge =
        serde_json::from_value(claim["challenge"].clone()).unwrap();
    let transcript = wonder_api::pairing::ChallengeTranscript {
        device_id: id,
        challenge_id: &challenge.challenge_id,
        nonce: &challenge.nonce,
        origin: &challenge.origin,
        host_installation_id: &challenge.host_installation_id,
        issued_at_ms: challenge.issued_at_ms,
        expires_at_ms: challenge.expires_at_ms,
    };
    let signature: Signature = key.sign(&transcript.to_bytes());
    let signed = serde_json::json!({"challengeId":challenge.challenge_id,"signature":b64.encode(signature.to_bytes())});
    assert_eq!(
        call(&state, "/api/v1/pairing/session", signed.clone(), false)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &state,
            "/api/v1/pairing/session/refresh-challenge",
            serde_json::json!({"deviceId":id}),
            false
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let confirmation = serde_json::json!({"code": hex::encode_upper(sha2::Sha256::digest(transcript.to_bytes()))[..12]});
    let confirm = format!("/api/v1/pairing/pending/{id}/confirm");
    assert_eq!(
        call(&state, &confirm, serde_json::Value::Null, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        dir.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_confirmation BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT, 'test storage failure'); END").execute(&pool).await.unwrap();
    assert_eq!(
        call(&state, &confirm, confirmation.clone(), true).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        call(&state, "/api/v1/pairing/session", signed.clone(), false)
            .await
            .0,
        StatusCode::CONFLICT
    );
    sqlx::query("DROP TRIGGER fail_confirmation")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(
        call(&state, &confirm, confirmation.clone(), true).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(state
        .store
        .list_owner_devices()
        .await
        .unwrap()
        .iter()
        .any(|d| d.id == id));
    let (status, credential) = call(&state, "/api/v1/pairing/session", signed, false).await;
    assert_eq!(status, StatusCode::OK);
    let token = credential["sessionToken"].as_str().unwrap();
    assert_eq!(
        state
            .pairing
            .lock()
            .await
            .verify_session(token, None, now_ms())
            .unwrap(),
        id
    );
    assert_eq!(
        call(
            &state,
            &format!("/api/v1/devices/{id}/revoke"),
            serde_json::Value::Null,
            true
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(state
        .pairing
        .lock()
        .await
        .verify_session(token, None, now_ms())
        .is_err());
    assert!(state
        .store
        .list_active_sessions(now_ms())
        .await
        .unwrap()
        .is_empty());
    assert_ne!(
        call(
            &state,
            "/api/v1/pairing/session/refresh-challenge",
            serde_json::json!({"deviceId":id}),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
}

async fn send(
    state: &AppState,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let response = router(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let data = to_bytes(response.into_body(), 65536).await.unwrap();
    (
        status,
        serde_json::from_slice(&data).unwrap_or(serde_json::Value::Null),
    )
}

fn sign_challenge(
    key: &SigningKey,
    device_id: &str,
    challenge: &serde_json::Value,
) -> serde_json::Value {
    let challenge: wonder_api::pairing_protocol::Challenge =
        serde_json::from_value(challenge.clone()).unwrap();
    let transcript = wonder_api::pairing::ChallengeTranscript {
        device_id,
        challenge_id: &challenge.challenge_id,
        nonce: &challenge.nonce,
        origin: &challenge.origin,
        host_installation_id: &challenge.host_installation_id,
        issued_at_ms: challenge.issued_at_ms,
        expires_at_ms: challenge.expires_at_ms,
    };
    let signature: Signature = key.sign(&transcript.to_bytes());
    serde_json::json!({
        "challengeId": challenge.challenge_id,
        "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    })
}

/// A phone paired over Tailscale keeps working over a second trusted route,
/// and every challenge names the origin the phone actually used. (A home
/// network route is trusted only on its own listener; `lan::tests` covers it.)
#[tokio::test]
async fn sessions_work_over_every_trusted_origin_and_challenges_echo_the_route() {
    const TAILNET: &str = "https://wonder.example.ts.net";
    const HOME: &str = "https://wonder.example.com";
    let (dir, mut state) = crate::ingestion::tests::fixture().await;
    let status_file = dir.path().join("origin-status.jsonl");
    std::fs::write(
        &status_file,
        format!(
            r#"{{"state":"ready","origin":"{TAILNET}","routes":[{{"kind":"custom","origin":"{HOME}"}}]}}"#
        ),
    )
    .unwrap();
    state.public_origin_file = Some(status_file);
    let capability = state.loopback_capability.clone();
    let local = [("x-wonder-loopback-capability", capability.as_str())];

    // Pair over the primary (Tailscale) origin, as the QR code says.
    let key = SigningKey::from_slice(&[9_u8; 32]).unwrap();
    let point = key.verifying_key().to_sec1_point(false);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (status, offer) = send(
        &state,
        "POST",
        "/api/v1/pairing/offers",
        &local,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let claim_body = serde_json::json!({"humanCode": offer["humanCode"], "publicKey": {"kty":"EC","crv":"P-256","x":b64.encode(point.x().unwrap()),"y":b64.encode(point.y().unwrap())},"label":"Test iPhone","sessionExpiration":"never"});
    let (status, claim) = send(
        &state,
        "POST",
        "/api/v1/pairing/code",
        &[("origin", TAILNET)],
        claim_body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claim["challenge"]["origin"], TAILNET);
    let id = claim["deviceId"].as_str().unwrap().to_owned();
    let confirm = format!("/api/v1/pairing/pending/{id}/confirm");
    // A mistyped text is refused as a mismatch and leaves the phone pending.
    let (status, refused) = send(
        &state,
        "POST",
        &confirm,
        &local,
        serde_json::json!({"code": "000000000000"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"], PAIRING_CODE_MISMATCH);
    // People type the phone's text in lowercase, grouped with spaces or hyphens.
    let code = phone_code(&claim["challenge"]).to_ascii_lowercase();
    let typed = format!("{}-{} {}", &code[..4], &code[4..8], &code[8..]);
    let (status, _) = send(
        &state,
        "POST",
        &confirm,
        &local,
        serde_json::json!({"code": typed}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let signed = sign_challenge(&key, &id, &claim["challenge"]);
    let (status, tailnet_session) = send(
        &state,
        "POST",
        "/api/v1/pairing/session",
        &[("origin", TAILNET)],
        signed,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Refresh the session over the home route: the challenge names that route,
    // and a signature over it yields a working session.
    let (status, refresh) = send(
        &state,
        "POST",
        "/api/v1/pairing/session/refresh-challenge",
        &[("origin", HOME)],
        serde_json::json!({"deviceId": id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(refresh["origin"], HOME);
    let signed = sign_challenge(&key, &id, &refresh);
    let (status, home_session) = send(
        &state,
        "POST",
        "/api/v1/pairing/session",
        &[("origin", HOME)],
        signed,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Both sessions work over both origins; each challenge echoes its origin.
    for session in [&tailnet_session, &home_session] {
        let cookie = format!(
            "__Host-wonder_session={}",
            session["sessionToken"].as_str().unwrap()
        );
        for origin in [TAILNET, HOME] {
            let (status, challenge) = send(
                &state,
                "GET",
                "/api/v1/events/challenge",
                &[("origin", origin), ("cookie", &cookie)],
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{origin}");
            assert_eq!(challenge["origin"], origin);
        }
    }

    // A GET without Origin is accepted only when Host names a member's host;
    // the challenge then names that member.
    let cookie = format!(
        "__Host-wonder_session={}",
        home_session["sessionToken"].as_str().unwrap()
    );
    let (status, challenge) = send(
        &state,
        "GET",
        "/api/v1/events/challenge",
        &[("host", "wonder.example.com"), ("cookie", &cookie)],
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(challenge["origin"], HOME);
    for host in [
        "evil.example",
        "wonder.example.com:443",
        "wonder.example.org",
    ] {
        let (status, _) = send(
            &state,
            "GET",
            "/api/v1/events/challenge",
            &[("host", host), ("cookie", &cookie)],
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
    }

    // An unknown origin gets 403 on authenticated and pairing routes alike,
    // and a non-GET without Origin is refused even with a member's Host.
    for origin in [
        "https://evil.example",
        "http://wonder.example.com",
        "https://wonder.example.com:443",
    ] {
        let (status, _) = send(
            &state,
            "GET",
            "/api/v1/events/challenge",
            &[("origin", origin), ("cookie", &cookie)],
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
        let (status, _) = send(
            &state,
            "POST",
            "/api/v1/pairing/session/refresh-challenge",
            &[("origin", origin)],
            serde_json::json!({"deviceId": id}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
    }
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/pairing/session/refresh-challenge",
        &[("host", "wonder.example.com")],
        serde_json::json!({"deviceId": id}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Pairing offers and MCP keep using the primary (Tailscale) origin only.
    assert_eq!(
        crate::current_public_origin(&state).await.as_deref(),
        Some(TAILNET)
    );
}

fn phone_code(value: &serde_json::Value) -> String {
    let c: wonder_api::pairing_protocol::Challenge = serde_json::from_value(value.clone()).unwrap();
    let transcript = wonder_api::pairing::ChallengeTranscript {
        device_id: &c.device_id,
        challenge_id: &c.challenge_id,
        nonce: &c.nonce,
        origin: &c.origin,
        host_installation_id: &c.host_installation_id,
        issued_at_ms: c.issued_at_ms,
        expires_at_ms: c.expires_at_ms,
    };
    hex::encode_upper(sha2::Sha256::digest(transcript.to_bytes()))[..12].into()
}

struct PairedPhone {
    id: String,
    key: SigningKey,
    cookie: String,
    csrf: String,
}

async fn pair_phone(state: &AppState, seed: u8, label: &str) -> PairedPhone {
    pair_phone_reporting(state, seed, label, None).await
}

/// Pairs a phone that reports `model` as its hardware model, as the iOS app does.
async fn pair_phone_reporting(
    state: &AppState,
    seed: u8,
    label: &str,
    model: Option<&str>,
) -> PairedPhone {
    let origin = state.public_origin.clone();
    let capability = state.loopback_capability.clone();
    let local = [("x-wonder-loopback-capability", capability.as_str())];
    let key = SigningKey::from_slice(&[seed; 32]).unwrap();
    let point = key.verifying_key().to_sec1_point(false);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (status, offer) = send(
        state,
        "POST",
        "/api/v1/pairing/offers",
        &local,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let mut body = serde_json::json!({"humanCode": offer["humanCode"], "publicKey": {"kty":"EC","crv":"P-256","x":b64.encode(point.x().unwrap()),"y":b64.encode(point.y().unwrap())},"label":label,"sessionExpiration":"never"});
    if let Some(model) = model {
        body["model"] = model.into();
    }
    let (status, claim) = send(
        state,
        "POST",
        "/api/v1/pairing/code",
        &[("origin", &origin)],
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = claim["deviceId"].as_str().unwrap().to_owned();
    let (status, _) = send(
        state,
        "POST",
        &format!("/api/v1/pairing/pending/{id}/confirm"),
        &local,
        serde_json::json!({"code": phone_code(&claim["challenge"])}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let signed = sign_challenge(&key, &id, &claim["challenge"]);
    let (status, session) = send(
        state,
        "POST",
        "/api/v1/pairing/session",
        &[("origin", &origin)],
        signed,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    PairedPhone {
        id,
        key,
        cookie: format!(
            "__Host-wonder_session={}",
            session["sessionToken"].as_str().unwrap()
        ),
        csrf: session["csrfToken"].as_str().unwrap().to_owned(),
    }
}

/// The phone's signed `device.revoke` request for `target_id`.
async fn phone_revoke(
    state: &AppState,
    phone: &PairedPhone,
    target_id: &str,
) -> (StatusCode, serde_json::Value) {
    let path = format!("/api/v1/devices/{target_id}/revoke");
    let nonce = format!("revoke-{}-{target_id}", phone.id);
    let issued_at_ms = now_ms();
    let body_sha256 = action_body_sha256(&serde_json::json!([]));
    let transcript = ActionTranscript {
        action: "device.revoke",
        target: &path,
        body_sha256: &body_sha256,
        action_nonce: &nonce,
        session_binding: &phone.csrf,
        device_id: &phone.id,
        host_installation_id: &state.host_installation_id,
        issued_at_ms,
        expected_state: "active",
    };
    let signature: Signature = phone.key.sign(&transcript.to_bytes());
    let origin = state.public_origin.clone();
    send(
        state,
        "POST",
        &path,
        &[
            ("origin", &origin),
            ("cookie", &phone.cookie),
            ("x-wonder-csrf", &phone.csrf),
        ],
        serde_json::json!({
            "actionNonce": nonce,
            "issuedAtMs": issued_at_ms,
            "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        }),
    )
    .await
}

async fn is_revoked(state: &AppState, id: &str) -> bool {
    state
        .store
        .list_owner_devices()
        .await
        .unwrap()
        .iter()
        .find(|device| device.id == id)
        .unwrap()
        .revoked_at
        .is_some()
}

/// A paired phone can disconnect itself but not another paired device; the
/// owner on this computer can still disconnect any device.
#[tokio::test]
async fn paired_device_revokes_only_itself_and_owner_revokes_any() {
    let (_dir, mut state) = crate::ingestion::tests::fixture().await;
    state.public_origin = "https://wonder.example.ts.net".into();
    state.public_origin_file = None;
    let phone_a = pair_phone(&state, 11, "Phone A").await;
    let phone_b = pair_phone(&state, 12, "Phone B").await;
    let phone_c = pair_phone(&state, 13, "Phone C").await;

    let (status, _) = phone_revoke(&state, &phone_a, &phone_b.id).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!is_revoked(&state, &phone_b.id).await);
    // A device that does not exist is refused the same way, so the answer
    // does not reveal which device ids are paired.
    let (status, _) = phone_revoke(&state, &phone_a, "not-a-device").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = phone_revoke(&state, &phone_a, &phone_a.id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(is_revoked(&state, &phone_a.id).await);
    assert!(!is_revoked(&state, &phone_b.id).await);

    let (status, _) = call(
        &state,
        &format!("/api/v1/devices/{}/revoke", phone_c.id),
        serde_json::Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(is_revoked(&state, &phone_c.id).await);
    assert!(!is_revoked(&state, &phone_b.id).await);
}

/// Phones are all named "iPhone", so the device list carries the hardware
/// model each one reported. A malformed model is dropped, never refused.
#[tokio::test]
async fn device_list_names_the_model_each_phone_reported() {
    let (_dir, mut state) = crate::ingestion::tests::fixture().await;
    state.public_origin = "https://wonder.example.ts.net".into();
    state.public_origin_file = None;
    let reported = pair_phone_reporting(&state, 21, "iPhone", Some("iPhone17,2")).await;
    let malformed = pair_phone_reporting(&state, 22, "iPhone", Some("iPhone<script>")).await;
    let older = pair_phone(&state, 23, "iPhone").await;
    let capability = state.loopback_capability.clone();
    let (status, devices) = send(
        &state,
        "GET",
        "/api/v1/devices",
        &[("x-wonder-loopback-capability", capability.as_str())],
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let model = |id: &str| {
        devices
            .as_array()
            .unwrap()
            .iter()
            .find(|device| device["id"] == id)
            .unwrap()["model"]
            .clone()
    };
    assert_eq!(model(&reported.id), "iPhone17,2");
    assert!(model(&malformed.id).is_null());
    assert!(model(&older.id).is_null());
}

#[test]
fn refusal_log_names_an_id_prefix_without_untrusted_characters() {
    assert_eq!(id_prefix("0123456789abcdef"), "01234567…");
    assert_eq!(id_prefix("ab\ncd"), "abcd…");
    assert_eq!(id_prefix(""), "(none)");
    assert_eq!(
        pairing_refusal_reason(&PairingError::BindingMismatch),
        Some("device was revoked")
    );
    assert_eq!(
        pairing_refusal_reason(&PairingError::ConfirmationRequired),
        None
    );
}

/// Pairs `key` through the human-code flow and confirms it on the computer,
/// returning the new device id.
async fn pair_and_confirm(state: &AppState, key: &SigningKey, label: &str) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let point = key.verifying_key().to_sec1_point(false);
    let (status, offer) = call(
        state,
        "/api/v1/pairing/offers",
        serde_json::Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = serde_json::json!({"humanCode": offer["humanCode"], "publicKey": {"kty":"EC","crv":"P-256", "x":b64.encode(point.x().unwrap()),"y":b64.encode(point.y().unwrap())},"label":label,"sessionExpiration":"never"});
    let (status, claim) = call(state, "/api/v1/pairing/code", body, false).await;
    assert_eq!(status, StatusCode::OK);
    let id = claim["deviceId"].as_str().unwrap().to_owned();
    let challenge: wonder_api::pairing_protocol::Challenge =
        serde_json::from_value(claim["challenge"].clone()).unwrap();
    let transcript = wonder_api::pairing::ChallengeTranscript {
        device_id: &id,
        challenge_id: &challenge.challenge_id,
        nonce: &challenge.nonce,
        origin: &challenge.origin,
        host_installation_id: &challenge.host_installation_id,
        issued_at_ms: challenge.issued_at_ms,
        expires_at_ms: challenge.expires_at_ms,
    };
    let confirmation = serde_json::json!({"code": hex::encode_upper(sha2::Sha256::digest(transcript.to_bytes()))[..12]});
    let confirm = format!("/api/v1/pairing/pending/{id}/confirm");
    assert_eq!(
        call(state, &confirm, confirmation, true).await.0,
        StatusCode::NO_CONTENT
    );
    id
}

/// Pairing the same phone again (same device key) replaces its row; a phone
/// with a different key is a different device. The replaced identity stops
/// working.
#[tokio::test]
async fn pairing_the_same_device_key_again_replaces_its_row() {
    let (_dir, mut state) = crate::ingestion::tests::fixture().await;
    state.public_origin = "https://wonder.example.ts.net".into();
    state.public_origin_file = None;
    let phone = SigningKey::from_slice(&[7_u8; 32]).unwrap();
    let ids = |devices: Vec<wonder_store::StoredDevice>| {
        devices.into_iter().map(|d| d.id).collect::<Vec<_>>()
    };
    let baseline = ids(state.store.list_owner_devices().await.unwrap());
    let first = pair_and_confirm(&state, &phone, "iPhone").await;
    let second = pair_and_confirm(&state, &phone, "iPhone").await;
    assert_ne!(first, second);
    let mut expected = baseline.clone();
    expected.push(second.clone());
    expected.sort();
    let mut kept = ids(state.store.list_owner_devices().await.unwrap());
    kept.sort();
    assert_eq!(kept, expected, "re-pairing must leave one device row");
    assert_ne!(
        call(
            &state,
            "/api/v1/pairing/session/refresh-challenge",
            serde_json::json!({"deviceId": first}),
            false
        )
        .await
        .0,
        StatusCode::OK,
        "the replaced identity must stop working"
    );

    let other = SigningKey::from_slice(&[8_u8; 32]).unwrap();
    let third = pair_and_confirm(&state, &other, "iPad").await;
    let mut expected = baseline;
    expected.extend([second, third]);
    let mut kept = ids(state.store.list_owner_devices().await.unwrap());
    kept.sort();
    expected.sort();
    assert_eq!(kept, expected, "a different key is a different device");
}
