//! The recovery endpoint a host supervisor serves while its daemon is down.
//!
//! The supervisor (`headless/rescue.rs`) owns the process state and the
//! listener; this module owns what a request must prove and what it may do,
//! so it is testable without a daemon or a supervisor:
//!
//! - The caller is a paired device. The request carries the same
//!   `wonder-action-v1` signature the daemon checks for other consequential
//!   actions, verified by the same [`PairingState`] code against the device
//!   store, which is opened read-only for each request, so a device revoked a
//!   moment ago is refused and the daemon need not run.
//! - The request is fresh: `issuedAtMs` is within a minute of now, and its
//!   nonce was not used before (remembered for as long as the window lets it
//!   verify). The nonce is claimed only after the signature verified, so an
//!   unauthenticated caller cannot fill the cache.
//! - The command is one of three ([`RescueCommand`]); nothing in a request
//!   names a path, a program or a file.

use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::triage_redaction::{mentions_device, Redactor};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
    ConnectOptions, Row,
};
use wonder_api::{
    pairing_protocol::{DevicePublicKeyJwk, PairingState},
    rescue::{
        RescueCommand, RescueDiagnostics, RescueError, RescueErrorCode, RescueRequest,
        RescueRestartResponse, RescueState, RescueStatus, RESCUE_WINDOW_MS,
    },
};

/// Restarts a phone may ask for within [`RESTART_WINDOW_MS`], all devices
/// together: three tries is enough to recover from a transient fault and few
/// enough that a stolen phone cannot keep a broken host in a restart loop.
pub const RESTART_LIMIT: usize = 3;
pub const RESTART_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Status and diagnostics reads, all devices together, per minute.
const READ_LIMIT: usize = 60;
const READ_WINDOW_MS: u64 = 60 * 1000;
/// Remembered nonces. Past this, requests wait for old ones to age out.
const MAX_NONCES: usize = 4096;
/// A request is a few hundred bytes of JSON.
const MAX_BODY_BYTES: usize = 2048;
/// Diagnostics: the newest lines of the daemon's log, read from its tail.
pub const DIAGNOSTIC_LINES: usize = 200;
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// What the supervisor tells the endpoint, and asks of it.
pub trait RescueHost: Send + Sync {
    fn status(&self) -> RescueStatus;
    /// Asks the supervisor to (re)start the daemon now and, when it had
    /// given up, to forget the crashes that made it. Returns the state the
    /// host is in once the request is queued.
    fn request_restart(&self) -> RescueState;
}

/// A sliding window of event times.
#[derive(Debug)]
struct RateLimit {
    limit: usize,
    window_ms: u64,
    events: VecDeque<u64>,
}

impl RateLimit {
    fn new(limit: usize, window_ms: u64) -> Self {
        Self {
            limit,
            window_ms,
            events: VecDeque::new(),
        }
    }

    /// Records an event at `now_ms`, or says in how many ms one more fits.
    fn acquire(&mut self, now_ms: u64) -> Result<(), u64> {
        while self
            .events
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= self.window_ms)
        {
            self.events.pop_front();
        }
        if self.events.len() >= self.limit {
            let oldest = self.events.front().copied().unwrap_or(now_ms);
            return Err((oldest + self.window_ms).saturating_sub(now_ms).max(1));
        }
        self.events.push_back(now_ms);
        Ok(())
    }
}

pub struct Rescue {
    data_dir: PathBuf,
    log_dir: PathBuf,
    host: Arc<dyn RescueHost>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    nonces: Mutex<HashMap<String, u64>>,
    restarts: Mutex<RateLimit>,
    reads: Mutex<RateLimit>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

type Refusal = (StatusCode, RescueError);

fn refusal(status: StatusCode, error: RescueErrorCode, detail: &str) -> Refusal {
    (
        status,
        RescueError {
            error,
            detail: detail.into(),
            retry_after_seconds: None,
        },
    )
}

fn unauthorized() -> Refusal {
    refusal(
        StatusCode::UNAUTHORIZED,
        RescueErrorCode::Unauthorized,
        "This request was not accepted. Check that this phone is still paired with this computer.",
    )
}

impl Rescue {
    /// `data_dir` holds `wonder.sqlite3` and `host-installation-id`; the
    /// daemon's log is `log_dir/wonderd.jsonl`.
    pub fn new(data_dir: &Path, log_dir: &Path, host: Arc<dyn RescueHost>) -> Arc<Self> {
        Self::with_clock(data_dir, log_dir, host, Box::new(now_ms))
    }

    pub fn with_clock(
        data_dir: &Path,
        log_dir: &Path,
        host: Arc<dyn RescueHost>,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            data_dir: data_dir.to_owned(),
            log_dir: log_dir.to_owned(),
            host,
            clock,
            nonces: Mutex::default(),
            restarts: Mutex::new(RateLimit::new(RESTART_LIMIT, RESTART_WINDOW_MS)),
            reads: Mutex::new(RateLimit::new(READ_LIMIT, READ_WINDOW_MS)),
        })
    }

    /// The routes to serve: nothing else is reachable through them.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/v1/rescue/{command}", post(handle))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(Arc::clone(self))
    }

    /// Checks a request in the order that keeps unauthenticated callers away
    /// from every shared resource: shape, freshness, the device, its
    /// signature, and only then the replay cache.
    async fn authenticate(
        &self,
        command: RescueCommand,
        request: &RescueRequest,
    ) -> Result<(), Refusal> {
        let nonce = &request.action_nonce;
        if !(16..=128).contains(&nonce.len())
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                RescueErrorCode::BadRequest,
                "The request nonce must be 16 to 128 letters, digits, - or _.",
            ));
        }
        let now = (self.clock)();
        if request.issued_at_ms.abs_diff(now) > RESCUE_WINDOW_MS {
            return Err(refusal(
                StatusCode::UNAUTHORIZED,
                RescueErrorCode::Stale,
                "The request is too old or this phone's clock is wrong.",
            ));
        }
        let host_installation_id =
            std::fs::read_to_string(self.data_dir.join("host-installation-id"))
                .map_err(|_| unauthorized())?;
        let device = self
            .paired_device(&request.device_id)
            .await
            .ok_or_else(unauthorized)?;
        if device.expires_at_ms.is_some_and(|at| now >= at) {
            return Err(unauthorized());
        }
        let mut pairing = PairingState::default();
        pairing.restore_device(
            request.device_id.clone(),
            device.public_key,
            device.revoked,
            device.expires_at_ms,
        );
        let target = command.path();
        let transcript = command.transcript(
            &target,
            &request.device_id,
            host_installation_id.trim(),
            nonce,
            request.issued_at_ms,
        );
        pairing
            .verify_action_signature(&request.device_id, &transcript, &request.signature)
            .map_err(|_| unauthorized())?;
        let mut nonces = self.nonces.lock().unwrap_or_else(|e| e.into_inner());
        // A nonce can be replayed only while its request still verifies.
        nonces.retain(|_, issued| issued.saturating_add(2 * RESCUE_WINDOW_MS) > now);
        if nonces.contains_key(nonce.as_str()) {
            return Err(unauthorized());
        }
        if nonces.len() >= MAX_NONCES {
            let mut refused = refusal(
                StatusCode::TOO_MANY_REQUESTS,
                RescueErrorCode::RateLimited,
                "Too many recent requests. Try again in a minute.",
            );
            refused.1.retry_after_seconds = Some(60);
            return Err(refused);
        }
        nonces.insert(nonce.clone(), request.issued_at_ms);
        Ok(())
    }

    /// What the diagnostics must not reveal beyond the logger's key-based
    /// redaction: home folder, this computer's names and the phones' names
    /// (the triage file's rules, so both hand out the same thing). The flag says
    /// whether the phones' names could be read.
    async fn redactor(&self) -> (Redactor, bool) {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| self.data_dir.parent().map(|p| p.display().to_string()));
        let mut hostnames: Vec<String> = ["HOSTNAME", "COMPUTERNAME", "HOST"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .collect();
        hostnames.extend(crate::platform::host_name());
        let short: Vec<String> = hostnames
            .iter()
            .filter_map(|name| name.split('.').next().map(str::to_owned))
            .collect();
        hostnames.extend(short);
        let devices = self.device_labels().await;
        let names_known = devices.is_some();
        (
            Redactor::new(home.as_deref(), hostnames, devices.unwrap_or_default()),
            names_known,
        )
    }

    /// Every phone's name in the store, read-only (`None` when it cannot be read).
    async fn device_labels(&self) -> Option<Vec<String>> {
        let mut connection = self.store_options().connect().await.ok()?;
        sqlx::query("SELECT label FROM devices")
            .fetch_all(&mut connection)
            .await
            .ok()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.try_get::<String, _>("label").ok())
                    .collect()
            })
    }

    fn store_options(&self) -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            .filename(self.data_dir.join("wonder.sqlite3"))
            .read_only(true)
            .create_if_missing(false)
            // Leave the journal mode as the daemon set it: asking SQLite to
            // set it again is a write.
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(2))
    }

    /// The device as the store has it right now, read-only; `None` when it is
    /// unknown, forgotten, or the store cannot be read (no device can be
    /// proven then).
    async fn paired_device(&self, device_id: &str) -> Option<PairedDevice> {
        let mut connection = self.store_options().connect().await.ok()?;
        let row = sqlx::query(
            "SELECT public_key_jwk, revoked_at, session_expires_at_ms FROM devices \
             WHERE id = ? AND role = 'owner' AND is_local = 0 AND forgotten = 0",
        )
        .bind(device_id)
        .fetch_optional(&mut connection)
        .await
        .ok()??;
        let public_key: DevicePublicKeyJwk =
            serde_json::from_str(row.try_get::<&str, _>("public_key_jwk").ok()?).ok()?;
        let expires_at_ms = row
            .try_get::<Option<i64>, _>("session_expires_at_ms")
            .ok()?
            .map(u64::try_from)
            .transpose()
            .ok()?;
        Some(PairedDevice {
            public_key,
            revoked: row.try_get::<Option<&str>, _>("revoked_at").ok()?.is_some(),
            expires_at_ms,
        })
    }

    async fn run(&self, command: RescueCommand, body: &[u8]) -> Result<Response, Refusal> {
        let request: RescueRequest = serde_json::from_slice(body).map_err(|_| {
            refusal(
                StatusCode::BAD_REQUEST,
                RescueErrorCode::BadRequest,
                "The request body is not a rescue request.",
            )
        })?;
        self.authenticate(command, &request).await?;
        let now = (self.clock)();
        let limited = |wait_ms: u64, what: &str| {
            let mut refused = refusal(
                StatusCode::TOO_MANY_REQUESTS,
                RescueErrorCode::RateLimited,
                what,
            );
            refused.1.retry_after_seconds = Some(wait_ms.div_ceil(1000));
            refused
        };
        match command {
            RescueCommand::Status | RescueCommand::Diagnostics => {
                self.reads
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .acquire(now)
                    .map_err(|wait| limited(wait, "Too many recent requests."))?;
            }
            RescueCommand::Restart => {
                self.restarts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .acquire(now)
                    .map_err(|wait| {
                        limited(
                            wait,
                            "This computer was asked to restart too often. Wait a few minutes.",
                        )
                    })?;
            }
        }
        Ok(match command {
            RescueCommand::Status => Json(self.host.status()).into_response(),
            RescueCommand::Restart => Json(RescueRestartResponse {
                accepted: true,
                state: self.host.request_restart(),
            })
            .into_response(),
            RescueCommand::Diagnostics => {
                let log_dir = self.log_dir.clone();
                let (redactor, names_known) = self.redactor().await;
                let lines = tokio::task::spawn_blocking(move || {
                    redacted_log_tail(
                        &log_dir.join("wonderd.jsonl"),
                        DIAGNOSTIC_LINES,
                        &redactor,
                        names_known,
                    )
                })
                .await
                .unwrap_or_default();
                Json(RescueDiagnostics {
                    status: self.host.status(),
                    generated_at_ms: now,
                    log_lines: lines,
                })
                .into_response()
            }
        })
    }
}

struct PairedDevice {
    public_key: DevicePublicKeyJwk,
    revoked: bool,
    expires_at_ms: Option<u64>,
}

async fn handle(
    State(rescue): State<Arc<Rescue>>,
    AxumPath(name): AxumPath<String>,
    body: Bytes,
) -> Response {
    let Some(command) = RescueCommand::from_name(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match rescue.run(command, &body).await {
        Ok(response) => response,
        Err((status, error)) => (status, Json(error)).into_response(),
    }
}

const OMITTED_DEVICE_LINE: &str =
    "<line omitted: it mentions a device and the host could not say which names to hide>";

/// The newest `max_lines` lines of the log at `path`, redacted as the logger
/// redacts and then by `redactor`. Only the last [`LOG_TAIL_BYTES`] are read.
/// Without the phones' names (`names_known` false) a line that mentions a device
/// may hold a name no pattern finds, so it is dropped whole, as the triage file does.
pub fn redacted_log_tail(
    path: &Path,
    max_lines: usize,
    redactor: &Redactor,
    names_known: bool,
) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let length = file.metadata().map_or(0, |metadata| metadata.len());
    let start = length.saturating_sub(LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        // The first line is probably cut short.
        lines.next();
    }
    let lines: Vec<_> = lines
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            if !names_known && mentions_device(line) {
                OMITTED_DEVICE_LINE.to_owned()
            } else {
                redactor.scrub(&crate::logging::redact_log_line(line))
            }
        })
        .collect();
    lines[lines.len().saturating_sub(max_lines)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tower::ServiceExt;

    const HOST_ID: &str = "0b9a3a30-4e0c-4c6a-9d19-1f6f3f5c2a11";

    struct FakeHost {
        restarts: AtomicUsize,
    }

    impl RescueHost for FakeHost {
        fn status(&self) -> RescueStatus {
            RescueStatus {
                state: RescueState::GaveUp,
                last_exit: Some("exit status: 1".into()),
                crash_count: 6,
                version: "test".into(),
            }
        }
        fn request_restart(&self) -> RescueState {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            RescueState::Starting
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        rescue: Arc<Rescue>,
        host: Arc<FakeHost>,
        clock: Arc<AtomicU64>,
        store: Option<wonder_store::Store>,
        key: SigningKey,
        nonce: AtomicU64,
    }

    fn jwk(key: &SigningKey) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let point = key.verifying_key().to_sec1_point(false);
        json!({
            "kty": "EC",
            "crv": "P-256",
            "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
            "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
        })
        .to_string()
    }

    impl Fixture {
        /// A paired device "phone" whose store is closed, as it is when the
        /// daemon is down.
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let url = format!(
                "sqlite://{}?mode=rwc",
                dir.path().join("wonder.sqlite3").display()
            );
            let store = wonder_store::Store::connect(&url).await.unwrap();
            let key = SigningKey::from_slice(&[11_u8; 32]).unwrap();
            store
                .upsert_owner_device("phone", "Phone", &jwk(&key), "now")
                .await
                .unwrap();
            std::fs::write(
                dir.path().join("host-installation-id"),
                format!("{HOST_ID}\n"),
            )
            .unwrap();
            let logs = dir.path().join("logs");
            std::fs::create_dir_all(&logs).unwrap();
            let host = Arc::new(FakeHost {
                restarts: AtomicUsize::new(0),
            });
            let clock = Arc::new(AtomicU64::new(1_800_000_000_000));
            let clock_read = Arc::clone(&clock);
            let rescue = Rescue::with_clock(
                dir.path(),
                &logs,
                host.clone(),
                Box::new(move || clock_read.load(Ordering::SeqCst)),
            );
            Self {
                _dir: dir,
                rescue,
                host,
                clock,
                store: Some(store),
                key,
                nonce: AtomicU64::new(0),
            }
        }

        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }

        fn body(
            &self,
            command: RescueCommand,
            device: &str,
            key: &SigningKey,
            nonce: &str,
            issued: u64,
        ) -> Value {
            use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
            let signature: Signature =
                key.sign(&command.transcript_bytes(device, HOST_ID, nonce, issued));
            json!({
                "deviceId": device,
                "actionNonce": nonce,
                "issuedAtMs": issued,
                "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            })
        }

        fn fresh_nonce(&self) -> String {
            format!("nonce-{:016}", self.nonce.fetch_add(1, Ordering::SeqCst))
        }

        fn signed(&self, command: RescueCommand) -> Value {
            self.body(command, "phone", &self.key, &self.fresh_nonce(), self.now())
        }

        async fn post(&self, command: RescueCommand, body: &Value) -> (StatusCode, Value) {
            let response = self
                .rescue
                .router()
                .oneshot(
                    Request::post(command.path())
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }
    }

    use serde_json::Value;

    #[tokio::test]
    async fn rescue_status_answers_a_paired_device_while_the_store_is_closed() {
        let mut fixture = Fixture::new().await;
        fixture.store = None;
        let (status, body) = fixture
            .post(
                RescueCommand::Status,
                &fixture.signed(RescueCommand::Status),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "gaveUp");
        assert_eq!(body["crashCount"], 6);
        assert_eq!(body["lastExit"], "exit status: 1");
    }

    #[tokio::test]
    async fn rescue_rejects_an_unpaired_device() {
        let fixture = Fixture::new().await;
        let stranger = SigningKey::from_slice(&[12_u8; 32]).unwrap();
        let body = fixture.body(
            RescueCommand::Restart,
            "stranger",
            &stranger,
            &fixture.fresh_nonce(),
            fixture.now(),
        );
        let (status, error) = fixture.post(RescueCommand::Restart, &body).await;
        assert_eq!(
            (status, &error["error"]),
            (StatusCode::UNAUTHORIZED, &json!("unauthorized"))
        );
        assert_eq!(fixture.host.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rescue_rejects_a_revoked_device() {
        let fixture = Fixture::new().await;
        let body = fixture.signed(RescueCommand::Restart);
        assert!(fixture
            .store
            .as_ref()
            .unwrap()
            .revoke_owner_device("phone", "now")
            .await
            .unwrap());
        let (status, error) = fixture.post(RescueCommand::Restart, &body).await;
        assert_eq!(
            (status, &error["error"]),
            (StatusCode::UNAUTHORIZED, &json!("unauthorized"))
        );
        assert_eq!(fixture.host.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rescue_rejects_an_expired_device_session() {
        let fixture = Fixture::new().await;
        fixture
            .store
            .as_ref()
            .unwrap()
            .upsert_owner_device_with_expiration(
                "phone",
                "Phone",
                &jwk(&fixture.key),
                Some(fixture.now() - 1),
                "now",
            )
            .await
            .unwrap();
        let (status, _) = fixture
            .post(
                RescueCommand::Status,
                &fixture.signed(RescueCommand::Status),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // SQLite's INTEGER affinity also permits TEXT/REAL. Corrupt expirations
    // must neither panic this independent recovery endpoint nor become an
    // effectively permanent authorization through a signed-to-unsigned cast.
    #[tokio::test]
    async fn rescue_rejects_corrupt_device_expirations() {
        let fixture = Fixture::new().await;
        let mut connection = SqliteConnectOptions::new()
            .filename(fixture._dir.path().join("wonder.sqlite3"))
            .connect()
            .await
            .unwrap();
        for invalid in ["-1", "not an integer", "1.5"] {
            sqlx::query("UPDATE devices SET session_expires_at_ms=? WHERE id='phone'")
                .bind(invalid)
                .execute(&mut connection)
                .await
                .unwrap();
            let (status, _) = fixture
                .post(
                    RescueCommand::Restart,
                    &fixture.signed(RescueCommand::Restart),
                )
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "expiration {invalid}");
            assert_eq!(fixture.host.restarts.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn rescue_rejects_a_bad_signature() {
        let fixture = Fixture::new().await;
        // Signed by another key, signed for another command, and altered.
        let other = SigningKey::from_slice(&[13_u8; 32]).unwrap();
        let wrong_key = fixture.body(
            RescueCommand::Restart,
            "phone",
            &other,
            &fixture.fresh_nonce(),
            fixture.now(),
        );
        let wrong_command = fixture.signed(RescueCommand::Status);
        let mut altered = fixture.signed(RescueCommand::Restart);
        altered["issuedAtMs"] = json!(fixture.now() + 1);
        for body in [wrong_key, wrong_command, altered] {
            let (status, error) = fixture.post(RescueCommand::Restart, &body).await;
            assert_eq!(
                (status, &error["error"]),
                (StatusCode::UNAUTHORIZED, &json!("unauthorized"))
            );
        }
        assert_eq!(fixture.host.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rescue_rejects_a_replayed_request_and_a_stale_one() {
        let fixture = Fixture::new().await;
        let body = fixture.signed(RescueCommand::Restart);
        let (first, _) = fixture.post(RescueCommand::Restart, &body).await;
        let (replay, error) = fixture.post(RescueCommand::Restart, &body).await;
        assert_eq!(
            (first, replay),
            (StatusCode::OK, StatusCode::UNAUTHORIZED),
            "{error}"
        );
        assert_eq!(fixture.host.restarts.load(Ordering::SeqCst), 1);

        let old = fixture.body(
            RescueCommand::Status,
            "phone",
            &fixture.key,
            &fixture.fresh_nonce(),
            fixture.now() - RESCUE_WINDOW_MS - 1,
        );
        let (status, error) = fixture.post(RescueCommand::Status, &old).await;
        assert_eq!(
            (status, &error["error"]),
            (StatusCode::UNAUTHORIZED, &json!("stale"))
        );
        let future = fixture.body(
            RescueCommand::Status,
            "phone",
            &fixture.key,
            &fixture.fresh_nonce(),
            fixture.now() + RESCUE_WINDOW_MS + 1,
        );
        assert_eq!(
            fixture.post(RescueCommand::Status, &future).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn rescue_nonces_are_remembered_only_for_requests_that_verified() {
        let fixture = Fixture::new().await;
        let nonce = fixture.fresh_nonce();
        let stranger = SigningKey::from_slice(&[14_u8; 32]).unwrap();
        // A forged request must not burn the nonce of the real one.
        let forged = fixture.body(
            RescueCommand::Status,
            "phone",
            &stranger,
            &nonce,
            fixture.now(),
        );
        assert_eq!(
            fixture.post(RescueCommand::Status, &forged).await.0,
            StatusCode::UNAUTHORIZED
        );
        let real = fixture.body(
            RescueCommand::Status,
            "phone",
            &fixture.key,
            &nonce,
            fixture.now(),
        );
        assert_eq!(
            fixture.post(RescueCommand::Status, &real).await.0,
            StatusCode::OK
        );
        // Replayed after the window has passed, it is stale instead.
        fixture
            .clock
            .fetch_add(RESCUE_WINDOW_MS + 1, Ordering::SeqCst);
        assert_eq!(
            fixture.post(RescueCommand::Status, &real).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn rescue_restart_is_rate_limited_and_recovers() {
        let fixture = Fixture::new().await;
        for _ in 0..RESTART_LIMIT {
            let (status, body) = fixture
                .post(
                    RescueCommand::Restart,
                    &fixture.signed(RescueCommand::Restart),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body, json!({"accepted": true, "state": "starting"}));
            fixture.clock.fetch_add(1000, Ordering::SeqCst);
        }
        let (status, body) = fixture
            .post(
                RescueCommand::Restart,
                &fixture.signed(RescueCommand::Restart),
            )
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"], "rate_limited");
        // The first of the three was 3 s ago.
        assert_eq!(body["retryAfterSeconds"], (RESTART_WINDOW_MS - 3000) / 1000);
        assert_eq!(fixture.host.restarts.load(Ordering::SeqCst), RESTART_LIMIT);
        // Reads are not held back by it.
        assert_eq!(
            fixture
                .post(
                    RescueCommand::Status,
                    &fixture.signed(RescueCommand::Status)
                )
                .await
                .0,
            StatusCode::OK
        );
        fixture.clock.fetch_add(RESTART_WINDOW_MS, Ordering::SeqCst);
        assert_eq!(
            fixture
                .post(
                    RescueCommand::Restart,
                    &fixture.signed(RescueCommand::Restart)
                )
                .await
                .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn rescue_serves_only_its_three_commands() {
        let fixture = Fixture::new().await;
        let body = fixture.signed(RescueCommand::Status);
        for path in [
            "/api/v1/rescue/shell",
            "/api/v1/rescue/../status",
            "/api/v1/rescue",
            "/readyz",
        ] {
            let response = fixture
                .rescue
                .router()
                .oneshot(
                    Request::post(path)
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let response = fixture
            .rescue
            .router()
            .oneshot(
                Request::get("/api/v1/rescue/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        // An unknown field is refused rather than ignored, and so is a huge body.
        let mut extra = fixture.signed(RescueCommand::Status);
        extra["path"] = json!("/etc/passwd");
        assert_eq!(
            fixture.post(RescueCommand::Status, &extra).await.0,
            StatusCode::BAD_REQUEST
        );
        let mut huge = fixture.signed(RescueCommand::Status);
        huge["signature"] = json!("x".repeat(MAX_BODY_BYTES));
        assert_eq!(
            fixture.post(RescueCommand::Status, &huge).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let mut short = fixture.signed(RescueCommand::Status);
        short["actionNonce"] = json!("short");
        assert_eq!(
            fixture.post(RescueCommand::Status, &short).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn rescue_diagnostics_returns_status_and_redacted_log_lines() {
        let fixture = Fixture::new().await;
        std::fs::write(
            fixture._dir.path().join("logs/wonderd.jsonl"),
            [
                r#"{"event":"started","sessionToken":"hunter2"}"#,
                "plain line token=abc123 and more",
                "pair https://x.example/#secret=zzz",
                r#"{"event":"home_route_started","dataDirectory":"/Users/someone/.cupola","address":"192.168.1.37:4778","hostKeyPin":"eVElSlnYFU0FW3yplln9VSohSzzUCozRr30qraB6qlE"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let (status, body) = fixture
            .post(
                RescueCommand::Diagnostics,
                &fixture.signed(RescueCommand::Diagnostics),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"]["state"], "gaveUp");
        let text = body["logLines"].to_string();
        for secret in ["hunter2", "abc123", "zzz"] {
            assert!(!text.contains(secret), "{text}");
        }
        // The triage file's rules also apply: no home folder, address or key pin.
        for private in ["someone", "192.168.1.37", "eVElSlnYFU0FW3"] {
            assert!(!text.contains(private), "{text}");
        }
        assert_eq!(body["logLines"].as_array().unwrap().len(), 4);
        assert!(text.contains("started") && text.contains("and more"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[cfg(unix)] // The HTTP contract helper belongs to the Unix host fixture.
    async fn rescue_answers_match_the_protocol_schemas() {
        use crate::test_support::assert_http_contract;
        let fixture = Fixture::new().await;
        let (_, status) = fixture
            .post(
                RescueCommand::Status,
                &fixture.signed(RescueCommand::Status),
            )
            .await;
        assert_http_contract("rescueStatus", &status);
        let (_, restart) = fixture
            .post(
                RescueCommand::Restart,
                &fixture.signed(RescueCommand::Restart),
            )
            .await;
        assert_http_contract("rescueRestartResponse", &restart);
        let (_, diagnostics) = fixture
            .post(
                RescueCommand::Diagnostics,
                &fixture.signed(RescueCommand::Diagnostics),
            )
            .await;
        assert_http_contract("rescueDiagnostics", &diagnostics);
        let (_, refused) = fixture
            .post(RescueCommand::Status, &json!({"deviceId": "x"}))
            .await;
        assert_http_contract("rescueError", &refused);
        assert_http_contract("rescueRequest", &fixture.signed(RescueCommand::Status));
    }

    /// The daemon crashed with committed rows only in the write-ahead log:
    /// the files as a killed process leaves them (database, -wal, -shm,
    /// copied while the store is still open) answer a paired phone.
    #[tokio::test]
    async fn rescue_reads_the_device_store_a_crashed_daemon_left_behind() {
        let fixture = Fixture::new().await;
        let source = fixture._dir.path();
        let copy = tempfile::tempdir().unwrap();
        for name in [
            "wonder.sqlite3",
            "wonder.sqlite3-wal",
            "wonder.sqlite3-shm",
            "host-installation-id",
        ] {
            if let Err(error) = std::fs::copy(source.join(name), copy.path().join(name)) {
                assert!(name.ends_with("-shm"), "{name}: {error}");
            }
        }
        let rescue = Rescue::with_clock(
            copy.path(),
            &copy.path().join("logs"),
            fixture.host.clone(),
            Box::new(|| 1_800_000_000_000),
        );
        let body = fixture.signed(RescueCommand::Status);
        let response = rescue
            .router()
            .oneshot(
                Request::post(RescueCommand::Status.path())
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn rescue_log_tail_keeps_the_newest_lines_of_a_large_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wonderd.jsonl");
        let mut text = String::new();
        for index in 0..20_000 {
            text.push_str(&format!(
                "{{\"index\":{index},\"pad\":\"{}\"}}\n",
                "x".repeat(40)
            ));
        }
        std::fs::write(&path, text).unwrap();
        let lines = redacted_log_tail(&path, 5, &Redactor::default(), true);
        assert_eq!(lines.len(), 5);
        assert!(lines[4].contains("\"index\":19999"), "{lines:?}");
        assert!(
            redacted_log_tail(&dir.path().join("missing"), 5, &Redactor::default(), true)
                .is_empty()
        );
        let unnamed = dir.path().join("devices.jsonl");
        std::fs::write(
            &unnamed,
            "{\"msg\":\"Device Maria's iPhone 15 connected\"}\n{\"msg\":\"tick\"}\n",
        )
        .unwrap();
        let lines = redacted_log_tail(&unnamed, 5, &Redactor::default(), false);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("<line omitted"), "{lines:?}");
        assert!(lines[1].contains("tick"), "{lines:?}");
    }
}
