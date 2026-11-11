//! Push transport. Preview plaintext, pairing and durable intent remain local.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use std::{sync::OnceLock, time::Duration};
const OFFICIAL_ENDPOINT: &str = "https://wonder-push.saimun-shahee.workers.dev";
pub(crate) fn endpoint() -> Option<String> {
    let value = std::env::var("WONDER_PUSH_ENDPOINT").unwrap_or_else(|_| OFFICIAL_ENDPOINT.into());
    let url = reqwest::Url::parse(&value).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/")
        .then(|| value.trim_end_matches('/').to_owned())
}
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("push HTTP client configuration")
    })
}
pub(super) async fn config() -> Response {
    Json(serde_json::json!({"endpoint":endpoint(),"topic":std::env::var("WONDER_PUSH_TOPIC").unwrap_or_else(|_|"com.swaymun.wonder".into()),"environment":std::env::var("WONDER_PUSH_ENVIRONMENT").unwrap_or_else(|_|"production".into()),"previewVersion":1})).into_response()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Registration {
    id: String,
    sender_secret: String,
    preview_key: Option<String>,
    #[serde(flatten)]
    action: SignedActionFields,
}
pub(super) async fn register(
    State(state): State<AppState>,
    Extension(device): Extension<AuthenticatedDevice>,
    Json(request): Json<Registration>,
) -> Response {
    let Some(endpoint) = endpoint() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if uuid::Uuid::parse_str(&request.id).is_err()
        || request.preview_key.as_ref().is_some_and(|key| {
            URL_SAFE_NO_PAD
                .decode(key)
                .map_or(true, |bytes| bytes.len() != 32)
        })
        || request.sender_secret.len() != 43
        || !request
            .sender_secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(error) = verify_signed_action(
        &state,
        &device,
        "push.register",
        "/api/v1/push/registration",
        &action_body_sha256(&match &request.preview_key {
            Some(key) => serde_json::json!([request.id, request.sender_secret, key]),
            None => serde_json::json!([request.id, request.sender_secret]),
        }),
        &request.action,
        "active",
    )
    .await
    {
        return (StatusCode::UNAUTHORIZED, error).into_response();
    }
    store_verified_registration(
        &state,
        &device.device_id,
        &endpoint,
        &request.id,
        &request.sender_secret,
        request.preview_key.as_deref(),
    )
    .await
}

/// Push belongs to a Wonder account: the service must confirm that this
/// registration exists and was enrolled by a signed-in account session
/// (`{"id": id, "account": true}`) before the Mac stores its capability.
/// An older or self-hosted service without accounts is refused.
async fn verify_account_registration(endpoint: &str, id: &str, sender_secret: &str) -> bool {
    // The Mac never accepts an unproven or caller-selected service endpoint.
    let Ok(response) = client()
        .post(format!("{endpoint}/v1/registrations/{id}/status"))
        .bearer_auth(sender_secret)
        .json(&serde_json::json!({}))
        .send()
        .await
    else {
        return false;
    };
    if response.status() != reqwest::StatusCode::OK {
        return false;
    }
    response
        .json::<serde_json::Value>()
        .await
        .is_ok_and(|body| body["id"] == id && body["account"] == true)
}

async fn store_verified_registration(
    state: &AppState,
    device_id: &str,
    endpoint: &str,
    id: &str,
    sender_secret: &str,
    preview_key: Option<&str>,
) -> Response {
    if !verify_account_registration(endpoint, id, sender_secret).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Push registration could not be verified with a Cupola account.",
        )
            .into_response();
    }
    match state
        .store
        .register_push(device_id, id, endpoint, sender_secret)
        .await
    {
        Ok(()) => {
            if let Some(key) = preview_key {
                if state
                    .store
                    .register_push_preview(device_id, id, key)
                    .await
                    .is_err()
                {
                    return StatusCode::CONFLICT.into_response();
                }
            }
            Json(serde_json::json!({"registered":true})).into_response()
        }
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Presence {
    state: String,
    /// The conversation open on screen while in the foreground ("" for none).
    /// Only that conversation stays quiet. Older phones omit it and keep the
    /// whole Mac quiet while in the foreground.
    conversation_id: Option<String>,
    #[serde(flatten)]
    action: SignedActionFields,
}
pub(super) async fn presence(
    State(state): State<AppState>,
    Extension(device): Extension<AuthenticatedDevice>,
    Json(request): Json<Presence>,
) -> Response {
    if !matches!(request.state.as_str(), "foreground" | "background")
        || request
            .conversation_id
            .as_ref()
            .is_some_and(|id| id.len() > 256 || id.chars().any(char::is_control))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(error) = verify_signed_action(
        &state,
        &device,
        "push.presence",
        "/api/v1/push/presence",
        &action_body_sha256(&match &request.conversation_id {
            Some(conversation) => serde_json::json!([request.state, conversation]),
            None => serde_json::json!([request.state]),
        }),
        &request.action,
        "active",
    )
    .await
    {
        return (StatusCode::UNAUTHORIZED, error).into_response();
    }
    match state
        .store
        .push_presence(
            &device.device_id,
            request.state == "foreground",
            request.conversation_id.as_deref(),
            request.action.issued_at_ms.unwrap_or(0) as i64,
            now_ms() as i64,
        )
        .await
    {
        Ok(()) => Json(serde_json::json!({"updated":true})).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn encrypt_preview(
    key: &str,
    registration: &str,
    route: &str,
    event: &str,
    preview: &wonder_store::PushPreview,
    host: &str,
) -> Option<String> {
    let key = URL_SAFE_NO_PAD.decode(key).ok()?;
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_256_GCM, &key).ok()?);
    let mut nonce = [0u8; 12];
    SystemRandom::new().fill(&mut nonce).ok()?;
    let mut value = serde_json::to_value(preview).ok()?;
    value["hostInstallationId"] = host.into();
    let mut bytes = serde_json::to_vec(&value).ok()?;
    if bytes.len() > 2400 {
        return None;
    }
    let aad = format!("wonder-push-v1\n{registration}\n{route}\n{event}");
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(aad.as_bytes()),
        &mut bytes,
    )
    .ok()?;
    let mut combined = nonce.to_vec();
    combined.extend(bytes);
    Some(URL_SAFE_NO_PAD.encode(combined))
}
pub(super) async fn revoke(
    State(state): State<AppState>,
    Extension(device): Extension<AuthenticatedDevice>,
    Json(request): Json<SignedActionFields>,
) -> Response {
    if let Err(error) = verify_signed_action(
        &state,
        &device,
        "push.revoke",
        "/api/v1/push/revoke",
        &action_body_sha256(&serde_json::json!([])),
        &request,
        "active",
    )
    .await
    {
        return (StatusCode::UNAUTHORIZED, error).into_response();
    }
    match state.store.revoke_push(&device.device_id).await {
        Ok(()) => Json(serde_json::json!({"revoked":true})).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
pub(super) async fn route(
    State(state): State<AppState>,
    Extension(device): Extension<AuthenticatedDevice>,
    Path(route): Path<String>,
) -> Response {
    if uuid::Uuid::parse_str(&route).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match state.store.push_route(&device.device_id, &route).await {
        Ok(Some(id)) => Json(serde_json::json!({"conversationId":id})).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            // Revocation remains queued across restart and takes priority.
            if let Ok(Some(record)) = state.store.next_push_revocation().await {
                if let Ok(response) = client()
                    .post(format!(
                        "{}/v1/registrations/{}/revoke",
                        record.endpoint, record.id
                    ))
                    .bearer_auth(record.sender_secret)
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                {
                    if response.status().is_success()
                        || response.status() == reqwest::StatusCode::GONE
                    {
                        let _ = state.store.finish_push_revocation(&record.id).await;
                    }
                }
            }
            if state.store.distribute_push().await.is_err() {
                continue;
            }
            for _ in 0..10 {
                let Ok(Some(delivery)) = state.store.next_push().await else {
                    break;
                };
                deliver_push(&state, delivery).await;
            }
        }
    })
}

/// The real transport preparation boundary: cancelled computer snapshots
/// never become alerts, and their content always addresses the computer.
pub(super) async fn delivery_preview(
    state: &AppState,
    delivery: &wonder_store::PushDelivery,
) -> Result<Option<wonder_store::PushPreview>, sqlx::Error> {
    // Recheck after distribution: a mute also applies to retries already queued.
    if state
        .store
        .chat_notification_level(&delivery.conversation_id)
        .await?
        == Some(wonder_store::ChatNotificationLevel::Mute)
    {
        return Ok(None);
    }
    let Some(mut preview) = state.store.push_preview(delivery).await? else {
        return Ok(None);
    };
    if delivery
        .source_key
        .starts_with(crate::computer_sessions::TAKEOVER_PUSH_PREFIX)
    {
        let Some((title, body)) =
            crate::computer_sessions::takeover_push_preview(state, &delivery.source_key).await
        else {
            return Ok(None);
        };
        preview.title = title;
        preview.body = body;
        return Ok(Some(preview));
    }
    if delivery
        .source_key
        .starts_with("external:mac-permission:computer:")
    {
        let count = crate::mac_dialogs::snapshot()["count"]
            .as_u64()
            .unwrap_or(0);
        if count == 0 {
            return Ok(None);
        }
        preview.title = crate::host_name::current(state).await;
        preview.body = format!(
            "{count} {} waiting",
            if count == 1 { "dialog" } else { "dialogs" }
        );
        preview.conversation_id = wonder_store::HOST_VIEW_CONVERSATION_ID.into();
    }
    Ok(Some(preview))
}

async fn deliver_push(state: &AppState, delivery: wonder_store::PushDelivery) {
    let preview = match delivery_preview(state, &delivery).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            let _ = state.store.finish_push_claim(&delivery, "cancelled").await;
            return;
        }
        Err(_) => {
            let _ = state.store.finish_push_claim(&delivery, "pending").await;
            return;
        }
    };
    let mut body =
        serde_json::json!({"eventId":delivery.id,"routeId":delivery.route_id,"kind":delivery.kind});
    if let Some(encrypted) = delivery.preview_key.as_deref().and_then(|key| {
        encrypt_preview(
            key,
            &delivery.registration_id,
            &delivery.route_id,
            &delivery.id,
            &preview,
            &state.host_installation_id,
        )
    }) {
        body["preview"] = encrypted.into();
    }
    let request = client()
        .post(format!(
            "{}/v1/registrations/{}/send",
            delivery.endpoint, delivery.registration_id
        ))
        .bearer_auth(&delivery.sender_secret)
        .json(&body)
        .build();
    let Ok(request) = request else {
        let _ = state.store.finish_push_claim(&delivery, "pending").await;
        return;
    };
    let Ok(Some(attempt)) = state.store.begin_push_attempt(delivery).await else {
        return;
    };
    let result = client().execute(request).await;
    let outcome = match result {
        Ok(response) if response.status().is_success() => "delivered",
        // /send reports 410 registration_gone for a missing registration or
        // an invalid APNs token. No delivery can use that capability again.
        Ok(response) if response.status() == reqwest::StatusCode::GONE => {
            let _ = state.store.retire_push_registration(&attempt).await;
            return;
        }
        Ok(response) if matches!(response.status().as_u16(), 400 | 401 | 403 | 404 | 409) => {
            "failed"
        }
        _ if attempt.attempts >= 5 => "failed",
        _ => "pending",
    };
    let _ = state.store.finish_push_attempt(&attempt, outcome).await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    async fn queued_push(endpoint: &str) -> (tempfile::TempDir, AppState, sqlx::SqlitePool) {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        state
            .store
            .register_push("owner", "local", endpoint, "local-secret")
            .await
            .unwrap();
        state
            .store
            .enqueue_external_push(
                "codex:retry:completed",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "completed",
                Some("Private reply"),
            )
            .await
            .unwrap();
        state.store.distribute_push().await.unwrap();
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        (dir, state, pool)
    }

    #[tokio::test]
    async fn mute_rechecks_a_prepared_push_before_transport() {
        let (_dir, state, _pool) = queued_push("https://push.invalid").await;
        let delivery = state.store.next_push().await.unwrap().unwrap();
        assert!(delivery_preview(&state, &delivery).await.unwrap().is_some());
        state
            .store
            .set_chat_notification_level(
                &delivery.conversation_id,
                wonder_store::ChatNotificationLevel::Mute,
            )
            .await
            .unwrap();
        assert!(delivery_preview(&state, &delivery).await.unwrap().is_none());
        assert!(state
            .store
            .begin_push_attempt(delivery)
            .await
            .unwrap()
            .is_none());
    }

    async fn fake_send_service(
        status: StatusCode,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let app = Router::new().route(
            "/v1/registrations/{id}/send",
            post(
                move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                    let observed = observed.clone();
                    async move {
                        assert_eq!(headers["authorization"], "Bearer local-secret");
                        assert_eq!(body.as_object().unwrap().len(), 3);
                        assert!(!body.to_string().contains("Private reply"));
                        assert!(body["eventId"].is_string() && body["routeId"].is_string());
                        assert_eq!(body["kind"], "completed");
                        observed.fetch_add(1, Ordering::SeqCst);
                        status
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (endpoint, requests, server)
    }

    // A missing/invalid Worker registration ends the capability, including
    // other queued notifications, rather than failing just this delivery.
    #[tokio::test]
    async fn gone_push_registration_retires_all_queued_deliveries() {
        use std::sync::atomic::Ordering;
        let (endpoint, requests, server) = fake_send_service(StatusCode::GONE).await;
        let (_dir, state, pool) = queued_push(&endpoint).await;
        state
            .store
            .enqueue_external_push(
                "codex:another:completed",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "completed",
                Some("Another private reply"),
            )
            .await
            .unwrap();
        state.store.distribute_push().await.unwrap();
        let delivery = state.store.next_push().await.unwrap().unwrap();
        deliver_push(&state, delivery).await;
        let retired: bool =
            sqlx::query_scalar("SELECT revoked FROM push_registrations WHERE id='local'")
                .fetch_one(&pool)
                .await
                .unwrap();
        eprintln!(
            "HTTP 410 requests={} registration retired={retired}",
            requests.load(Ordering::SeqCst),
        );
        assert!(
            retired,
            "HTTP 410 must retire the registration, not just the delivery"
        );
        assert!(!state.store.push_enabled().await.unwrap());
        assert!(state.store.next_push().await.unwrap().is_none());
        assert_eq!(
            state
                .store
                .next_push_revocation()
                .await
                .unwrap()
                .unwrap()
                .id,
            "local"
        );
        let outcomes: Vec<(i64, String)> =
            sqlx::query_as("SELECT attempts,state FROM push_outbox ORDER BY attempts DESC")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            outcomes,
            vec![(1, "failed".into()), (0, "cancelled".into())]
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.abort();
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn other_push_errors_preserve_the_registration_and_retry_policy() {
        use std::sync::atomic::Ordering;
        for (status, expected) in [
            (StatusCode::BAD_REQUEST, "failed"),
            (StatusCode::UNAUTHORIZED, "failed"),
            (StatusCode::FORBIDDEN, "failed"),
            (StatusCode::NOT_FOUND, "failed"),
            (StatusCode::CONFLICT, "failed"),
            (StatusCode::TOO_MANY_REQUESTS, "pending"),
            (StatusCode::SERVICE_UNAVAILABLE, "pending"),
        ] {
            let (endpoint, requests, server) = fake_send_service(status).await;
            let (_dir, state, pool) = queued_push(&endpoint).await;
            let delivery = state.store.next_push().await.unwrap().unwrap();
            deliver_push(&state, delivery).await;
            let stored: (i64, String) = sqlx::query_as("SELECT attempts,state FROM push_outbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(stored, (1, expected.into()), "HTTP {status}");
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            assert!(state.store.push_enabled().await.unwrap(), "HTTP {status}");
            assert!(state.store.next_push().await.unwrap().is_none());
            assert!(state.store.next_push_revocation().await.unwrap().is_none());
            server.abort();
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    // Contract: local preparation failures must leave the five HTTP attempts
    // available. This exercises the real preview lookup and local HTTP boundary.
    #[tokio::test]
    async fn pre_send_storage_failures_preserve_transport_budget() {
        use std::sync::atomic::Ordering;
        let (endpoint, requests, server) = fake_send_service(StatusCode::SERVICE_UNAVAILABLE).await;
        let (_dir, state, pool) = queued_push(&endpoint).await;
        sqlx::query("ALTER TABLE project_conversations RENAME TO unavailable_conversations")
            .execute(&pool)
            .await
            .unwrap();
        for _ in 0..5 {
            let delivery = state.store.next_push().await.unwrap().unwrap();
            deliver_push(&state, delivery).await;
            assert!(
                state.store.next_push().await.unwrap().is_none(),
                "preparation failures must back off"
            );
            sqlx::query("UPDATE push_outbox SET next_attempt=0")
                .execute(&pool)
                .await
                .unwrap();
        }
        let stored: (i64, String) = sqlx::query_as("SELECT attempts,state FROM push_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        eprintln!(
            "pre-send storage failures=5 HTTP requests={} stored attempts={} state={}",
            requests.load(Ordering::SeqCst),
            stored.0,
            stored.1
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(stored, (0, "pending".into()));
        sqlx::query("ALTER TABLE unavailable_conversations RENAME TO project_conversations")
            .execute(&pool)
            .await
            .unwrap();
        for expected in 1..=5 {
            let (first, second) = tokio::join!(state.store.next_push(), state.store.next_push());
            let deliveries: Vec<_> = [first.unwrap(), second.unwrap()]
                .into_iter()
                .flatten()
                .collect();
            assert_eq!(
                deliveries.len(),
                1,
                "concurrent workers must not duplicate a send"
            );
            let delivery = deliveries.into_iter().next().unwrap();
            assert_eq!(delivery.attempts, expected - 1);
            deliver_push(&state, delivery).await;
            let stored: (i64, String, i64) =
                sqlx::query_as("SELECT attempts,state,next_attempt-unixepoch() FROM push_outbox")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), expected as usize);
            assert_eq!(stored.0, expected);
            assert_eq!(stored.1, if expected == 5 { "failed" } else { "pending" });
            assert!((60 * (1 << expected) - 1..=60 * (1 << expected)).contains(&stored.2));
            assert!(state.store.next_push().await.unwrap().is_none());
            eprintln!(
                "HTTP 503 requests={} stored attempts={} state={} backoff={}s",
                requests.load(Ordering::SeqCst),
                stored.0,
                stored.1,
                stored.2
            );
            sqlx::query("UPDATE push_outbox SET next_attempt=0")
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(state.store.next_push().await.unwrap().is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 5);
        server.abort();
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn disconnected_http_requests_spend_exactly_five_attempts() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert!(line.starts_with("POST /v1/registrations/local/send "));
                let mut length = 0;
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).await.unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                // Close after receiving the request, without an HTTP response.
            }
        });
        let (_dir, state, pool) = queued_push(&endpoint).await;
        for expected in 1..=5 {
            let delivery = state.store.next_push().await.unwrap().unwrap();
            deliver_push(&state, delivery).await;
            let stored: (i64, String) = sqlx::query_as("SELECT attempts,state FROM push_outbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), expected as usize);
            assert_eq!(stored.0, expected);
            assert_eq!(stored.1, if expected == 5 { "failed" } else { "pending" });
            assert!(state.store.next_push().await.unwrap().is_none());
            eprintln!(
                "HTTP disconnect requests={} attempts={} state={}",
                requests.load(Ordering::SeqCst),
                stored.0,
                stored.1
            );
            sqlx::query("UPDATE push_outbox SET next_attempt=0")
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(state.store.next_push().await.unwrap().is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 5);
        server.abort();
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn post_claim_eligibility_controls_http_and_terminal_state() {
        use std::sync::atomic::Ordering;
        for case in [
            "foreground",
            "registration",
            "device",
            "withdrawn",
            "success",
        ] {
            let (endpoint, requests, server) = fake_send_service(if case == "success" {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            })
            .await;
            let (_dir, state, pool) = queued_push(&endpoint).await;
            let delivery = state.store.next_push().await.unwrap().unwrap();
            assert!(state.store.push_preview(&delivery).await.unwrap().is_some());
            match case {
                "foreground" => state
                    .store
                    .push_presence("owner", true, None, now_ms() as i64, now_ms() as i64)
                    .await
                    .unwrap(),
                "registration" => state.store.revoke_push("owner").await.unwrap(),
                "device" => {
                    assert!(state
                        .store
                        .revoke_owner_device("owner", "later")
                        .await
                        .unwrap());
                }
                "withdrawn" => state
                    .store
                    .withdraw_external_push("codex:retry:completed")
                    .await
                    .unwrap(),
                "success" => (),
                _ => unreachable!(),
            }
            deliver_push(&state, delivery).await;
            let stored: (i64, String) = sqlx::query_as("SELECT attempts,state FROM push_outbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            let expected_attempts = if case == "success" { 1 } else { 0 };
            assert_eq!(requests.load(Ordering::SeqCst), expected_attempts, "{case}");
            assert_eq!(stored.0, expected_attempts as i64, "{case}");
            assert_eq!(
                stored.1,
                match case {
                    "foreground" => "suppressed",
                    "success" => "delivered",
                    _ => "cancelled",
                },
                "{case}"
            );
            assert!(state.store.next_push().await.unwrap().is_none());
            eprintln!(
                "HTTP post-claim {case}: requests={} attempts={} state={}",
                requests.load(Ordering::SeqCst),
                stored.0,
                stored.1
            );
            server.abort();
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn presence_requires_a_device_signature_and_rejects_replay_and_tampering() {
        use axum::{body::Body, http::Request};
        use p256::ecdsa::{signature::Signer, Signature, SigningKey};
        use tower::ServiceExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let key = SigningKey::from_slice(&[17u8; 32]).unwrap();
        let point = key.verifying_key().to_sec1_point(false);
        state.pairing.lock().await.restore_device(
            "owner".into(),
            wonder_api::pairing_protocol::DevicePublicKeyJwk {
                kty: "EC".into(),
                crv: "P-256".into(),
                x: URL_SAFE_NO_PAD.encode(point.x().unwrap()),
                y: URL_SAFE_NO_PAD.encode(point.y().unwrap()),
            },
            false,
            None,
        );
        let path = "/api/v1/push/presence";
        let app = Router::new()
            .route(path, post(presence))
            .layer(Extension(AuthenticatedDevice {
                device_id: "owner".into(),
                session_binding: "phone-session".into(),
            }))
            .with_state(state.clone());
        let issued = now_ms();
        let transcript = ActionTranscript {
            action: "push.presence",
            target: path,
            body_sha256: &action_body_sha256(&serde_json::json!(["foreground"])),
            action_nonce: "presence-proof",
            session_binding: "phone-session",
            device_id: "owner",
            host_installation_id: &state.host_installation_id,
            issued_at_ms: issued,
            expected_state: "active",
        };
        let signature: Signature = key.sign(&transcript.to_bytes());
        let valid = serde_json::json!({"state":"foreground","actionNonce":"presence-proof","issuedAtMs":issued,"signature":URL_SAFE_NO_PAD.encode(signature.to_bytes())});
        let mut tampered = valid.clone();
        tampered["state"] = "background".into();
        // The open conversation is signed too, so it cannot be swapped.
        let issued_open = issued + 1;
        let open_hash = action_body_sha256(&serde_json::json!(["foreground", "chat"]));
        let open_transcript = ActionTranscript {
            body_sha256: &open_hash,
            action_nonce: "presence-open",
            issued_at_ms: issued_open,
            ..transcript
        };
        let signature: Signature = key.sign(&open_transcript.to_bytes());
        let open = serde_json::json!({"state":"foreground","conversationId":"chat","actionNonce":"presence-open","issuedAtMs":issued_open,"signature":URL_SAFE_NO_PAD.encode(signature.to_bytes())});
        let mut swapped = open.clone();
        swapped["conversationId"] = "other".into();
        let mut dropped = open.clone();
        dropped.as_object_mut().unwrap().remove("conversationId");
        for (body, expected) in [
            (
                serde_json::json!({"state":"foreground"}),
                StatusCode::UNAUTHORIZED,
            ),
            (tampered, StatusCode::UNAUTHORIZED),
            (valid.clone(), StatusCode::OK),
            (valid, StatusCode::UNAUTHORIZED),
            (swapped, StatusCode::UNAUTHORIZED),
            (dropped, StatusCode::UNAUTHORIZED),
            (open, StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    /// A local push service whose `/status` reply is set per case.
    async fn fake_status_service(
        reply: std::sync::Arc<std::sync::Mutex<(u16, serde_json::Value)>>,
    ) -> String {
        let app = Router::new().route(
            "/v1/registrations/{id}/status",
            post(move |headers: axum::http::HeaderMap| {
                let reply = reply.clone();
                async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        format!("Bearer {}", "s".repeat(43))
                    );
                    let (status, body) = reply.lock().unwrap().clone();
                    (StatusCode::from_u16(status).unwrap(), Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        endpoint
    }

    #[tokio::test]
    async fn registers_pushes_only_under_an_account_bound_registration() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let id = "11111111-1111-4111-8111-111111111111";
        let reply = std::sync::Arc::new(std::sync::Mutex::new((
            200u16,
            serde_json::json!({ "id": id }),
        )));
        let endpoint = fake_status_service(reply.clone()).await;
        let secret = "s".repeat(43);
        let register =
            || store_verified_registration(&state, "owner", &endpoint, id, &secret, None);
        // Refused: a service without accounts, another registration's reply,
        // an explicit non-account answer, and a gone (signed-out) registration.
        for (status, body) in [
            (200, serde_json::json!({ "id": id })),
            (
                200,
                serde_json::json!({ "id": "22222222-2222-4222-8222-222222222222", "account": true }),
            ),
            (200, serde_json::json!({ "id": id, "account": false })),
            (410, serde_json::json!({ "error": "registration_gone" })),
            (401, serde_json::json!({ "error": "sign_in_required" })),
        ] {
            *reply.lock().unwrap() = (status, body);
            assert_eq!(register().await.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(!state.store.push_enabled().await.unwrap());
        }
        *reply.lock().unwrap() = (200, serde_json::json!({ "id": id, "account": true }));
        assert_eq!(register().await.status(), StatusCode::OK);
        assert!(state.store.push_enabled().await.unwrap());
    }

    #[test]
    fn previews_interoperate_with_cryptokit_and_bind_the_recipient_and_route() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/push-preview-v1.json"
        ))
        .unwrap();
        let raw_key = URL_SAFE_NO_PAD
            .decode(fixture["key"].as_str().unwrap())
            .unwrap();
        let key =
            aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_256_GCM, &raw_key).unwrap());
        let payload = &fixture["payload"];
        let combined = URL_SAFE_NO_PAD
            .decode(payload["preview"].as_str().unwrap())
            .unwrap();
        let aad = format!(
            "wonder-push-v1\n{}\n{}\n{}",
            payload["registrationId"].as_str().unwrap(),
            payload["routeId"].as_str().unwrap(),
            payload["eventId"].as_str().unwrap()
        );
        let nonce: [u8; 12] = combined[..12].try_into().unwrap();
        let mut bytes = combined[12..].to_vec();
        let plaintext = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad.as_bytes()),
                &mut bytes,
            )
            .unwrap();
        assert_eq!(plaintext, fixture["plaintext"].as_str().unwrap().as_bytes());
        let mut tampered = combined[12..].to_vec();
        assert!(key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(format!("{aad}other").as_bytes()),
                &mut tampered
            )
            .is_err());
        let preview = wonder_store::PushPreview {
            title: "Chat".into(),
            body: "Question?".into(),
            conversation_id: "thread".into(),
            approval: None,
            question: None,
            read_through: None,
            open: None,
        };
        let first = encrypt_preview(
            fixture["key"].as_str().unwrap(),
            "registration",
            "route",
            "event",
            &preview,
            "host",
        )
        .unwrap();
        let second = encrypt_preview(
            fixture["key"].as_str().unwrap(),
            "registration",
            "route",
            "event",
            &preview,
            "host",
        )
        .unwrap();
        assert_ne!(first, second, "Each delivery attempt needs a fresh nonce");
        assert!(!first.contains("Question"));
        assert!(encrypt_preview(
            "bad-key",
            "registration",
            "route",
            "event",
            &preview,
            "host"
        )
        .is_none());
    }

    #[test]
    fn push_fixture_matches_http_contract() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/push-v1.json"
        ))
        .unwrap();
        for definition in [
            "pushConfiguration",
            "pushRoute",
            "pushRegistration",
            "pushPresence",
            "pushLiveActivity",
            "pushLiveActivityReply",
        ] {
            crate::tests::validate_http_contract(definition, &fixture[definition]);
        }
    }
}
