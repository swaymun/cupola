//! The host side of Wonder RPC on `/api/v1/rpc`. `wonder rpc` bridges stdio to
//! this same socket with the loopback capability, so both transports share it.
//!
//! A routed method becomes one internal request to the daemon's own router,
//! carrying the connection's credentials: the authentication middleware and
//! the route decide exactly as they do for the phone's HTTP request, on every
//! call (so a revoked or expired session stops working mid-connection).
//!
//! `/api/v1/rpc/sealed` serves the same protocol inside a [`sealed`] channel,
//! for routes whose TLS ends before this host (the relay): the upgrade
//! carries no credential, and the session arrives sealed.
use super::*;
use crate::{
    history::{compact_event, COMPACT_VIEW_HEADER},
    now_ms, AppState, AuthenticatedDevice, LocalOwnerAuthority, MatchedOrigin,
};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response as HttpResponse},
    Extension, Router,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Semaphore};
use tower::ServiceExt;

/// The largest request the host reads. A send's body is at most 64 KiB.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
/// The largest route body passed back as a result, as the route wrote it
/// (before base64 or history compression).
pub const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
/// The largest reply frame the host sends, sealing tag included: room for a
/// [`MAX_RESULT_BYTES`] body as base64 with its envelope. The phone's sealed
/// socket reads frames up to this size, so a larger reply is refused with a
/// small 413 inside the channel rather than closing the phone's connection.
/// Event frames have their own, smaller bound
/// ([`wonder_api::EVENT_FRAME_MAX_BYTES`]).
pub const MAX_RESPONSE_WIRE_BYTES: usize = 24 * 1024 * 1024;
/// Why a reply was refused for its size, in the phone's words.
const TOO_LARGE: &str = "This is too large to send to your device.";
/// The same for `http.request`, which only the relay's sealed channel
/// serves; large files still open on the routes that don't go through it.
const TOO_LARGE_FOR_RELAY: &str = "This is too large to open over Reach from anywhere. Connect on \
     the home network or Tailscale, then try again.";
/// The largest frame either socket reads. The frame cap is set with the
/// message cap: tungstenite's own frame cap is 16 MiB, and it buffers a whole
/// frame before it checks the message cap, so a sealed handshake (no
/// credential yet) could otherwise make the host hold 16 MiB per socket.
const MAX_PLAIN_FRAME_BYTES: usize = MAX_REQUEST_BYTES + 1024;
const MAX_SEALED_FRAME_BYTES: usize = MAX_PLAIN_FRAME_BYTES + sealed::TAG_BYTES;
pub const MAX_IN_FLIGHT: usize = 32;
/// Reply bytes that may wait in a connection's queue for a peer that reads
/// slowly or not at all. The queue holds 256 replies and one can be
/// [`MAX_RESPONSE_WIRE_BYTES`], so without this a device that asks for big
/// answers and never reads them pins gigabytes. A reply that does not fit is
/// replaced by a small `BUSY` error and the caller asks again once it reads.
pub const MAX_QUEUED_REPLY_BYTES: usize = 128 * 1024 * 1024;
/// Replies at most this long are never counted, so an error always has room.
const UNCOUNTED_REPLY_BYTES: usize = 16 * 1024;
const QUEUE_UNIT: usize = 1024;
const MAX_SUBSCRIPTIONS: usize = 8;
const PING_AFTER: Duration = Duration::from_secs(15);
/// A peer that sends nothing (not even a pong) for this long is gone.
const IDLE_LIMIT: Duration = Duration::from_secs(45);
/// A subscription rereads the journal at least this often, in case a wakeup
/// was missed while the broadcast lagged.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// A sealed client has this long to finish its handshake and send its session.
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);
/// Sealed handshakes in progress at once. They are unauthenticated (anyone who
/// reaches the relay address can start one), so they are capped rather than
/// left to hold sockets and host-key reads without limit.
pub const MAX_OPEN_HANDSHAKES: usize = 32;
/// The share of those one client address may hold; see [`HandshakeSlots`].
pub const MAX_HANDSHAKES_PER_SOURCE: usize = 4;

/// Room for sealed handshakes still in progress; see [`MAX_OPEN_HANDSHAKES`].
/// A request through the relay names its real client in `CF-Connecting-IP`
/// (Cloudflare sets it; only the relay address is believed on it),
/// and one client (an IPv6 /64) may hold at most
/// [`MAX_HANDSHAKES_PER_SOURCE`] places, so a single address cannot lock every
/// phone out. A request without it (home network, Tailscale) shares the pool.
#[derive(Clone)]
pub struct HandshakeSlots {
    room: Arc<Semaphore>,
    sources: Arc<std::sync::Mutex<HashMap<std::net::IpAddr, usize>>>,
}

impl Default for HandshakeSlots {
    fn default() -> Self {
        Self {
            room: Arc::new(Semaphore::new(MAX_OPEN_HANDSHAKES)),
            sources: Arc::default(),
        }
    }
}

/// One place held by a handshake; the place returns when it is dropped.
pub struct HandshakeSlot {
    _room: tokio::sync::OwnedSemaphorePermit,
    source: Option<(std::net::IpAddr, HandshakeSlots)>,
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let Some((source, slots)) = self.source.take() else {
            return;
        };
        let mut sources = slots.sources.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(held) = sources.get_mut(&source) {
            *held -= 1;
            if *held == 0 {
                sources.remove(&source);
            }
        }
    }
}

impl HandshakeSlots {
    /// A place for a handshake from `client` (the `CF-Connecting-IP` value),
    /// or `None` when there is none to give.
    fn try_acquire(&self, client: Option<&str>) -> Option<HandshakeSlot> {
        let source = client
            .and_then(|text| text.trim().parse::<std::net::IpAddr>().ok())
            .map(|address| match address {
                std::net::IpAddr::V6(v6) => {
                    let [a, b, c, d, ..] = v6.segments();
                    std::net::IpAddr::V6(std::net::Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
                }
                v4 => v4,
            });
        if let Some(source) = source {
            let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
            if sources.get(&source).copied().unwrap_or(0) >= MAX_HANDSHAKES_PER_SOURCE {
                return None;
            }
            // Counted before the pool is tried, so the check and the count
            // are one step; undone below if the pool is full.
            *sources.entry(source).or_default() += 1;
        }
        let slot = |room| HandshakeSlot {
            _room: room,
            source: source.map(|source| (source, self.clone())),
        };
        match self.room.clone().try_acquire_owned() {
            Ok(room) => Some(slot(room)),
            Err(_) => {
                if let Some(source) = source {
                    let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(held) = sources.get_mut(&source) {
                        *held -= 1;
                        if *held == 0 {
                            sources.remove(&source);
                        }
                    }
                }
                None
            }
        }
    }
}

/// The credentials the upgrade request proved, replayed on each routed call.
#[derive(Clone)]
enum Credentials {
    Owner {
        capability: String,
    },
    Device {
        device_id: String,
        session: String,
        origin: String,
        /// Set by `initialize`; non-GET routes require it.
        csrf: Option<String>,
        /// The home network listener the socket arrived on, if any. Its
        /// routed calls carry it too, so they are checked against that
        /// listener's origin like the upgrade was.
        home_route: Option<crate::lan::HomeRoute>,
        /// Arrived over `/api/v1/rpc/sealed`: its routed calls are already
        /// decrypted, so the relay address's path refusal does not apply.
        sealed: bool,
    },
}

/// Marks a call routed from inside a sealed channel. Only this module adds
/// it, as a request extension the network cannot set.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SealedCall;

pub(crate) async fn socket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    local: Option<Extension<LocalOwnerAuthority>>,
    device: Option<Extension<AuthenticatedDevice>>,
    matched: Option<Extension<MatchedOrigin>>,
    home_route: Option<Extension<crate::lan::HomeRoute>>,
) -> HttpResponse {
    let credentials = if local.is_some() {
        Credentials::Owner {
            capability: state.loopback_capability.clone(),
        }
    } else if let Some(Extension(device)) = device {
        // The middleware already matched the origin and verified the
        // session; keep exactly what it accepted.
        let Some(session) = crate::cookie_value(&headers, "__Host-wonder_session") else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        // Replay the trusted origin this socket arrived on, so a challenge
        // names the route the phone used even when it sent no Origin.
        let Some(Extension(MatchedOrigin(origin))) = matched else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        Credentials::Device {
            device_id: device.device_id,
            session,
            origin,
            csrf: None,
            home_route: home_route.map(|Extension(route)| route),
            sealed: false,
        }
    } else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.max_message_size(MAX_PLAIN_FRAME_BYTES)
        .max_frame_size(MAX_PLAIN_FRAME_BYTES)
        .on_upgrade(move |socket| serve(socket, state, credentials, None))
}

/// `/api/v1/rpc/sealed`: the middleware matched the Origin to a trusted
/// route and nothing else; the device proves itself inside the channel.
pub(crate) async fn sealed_socket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    matched: Option<Extension<MatchedOrigin>>,
    home_route: Option<Extension<crate::lan::HomeRoute>>,
) -> HttpResponse {
    let Some(Extension(MatchedOrigin(origin))) = matched else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // Believed only on the relay address, where Cloudflare sets it; on the
    // home network or Tailscale a peer could write anyone's address there.
    let client = (crate::relay::relay_origin(&state.data_dir).as_deref() == Some(origin.as_str()))
        .then(|| headers.get("cf-connecting-ip"))
        .flatten()
        .and_then(|value| value.to_str().ok());
    let Some(slot) = state.sealed_handshakes.try_acquire(client) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "Too many connections are starting. Try again shortly.",
        )
            .into_response();
    };
    ws.max_message_size(MAX_SEALED_FRAME_BYTES)
        .max_frame_size(MAX_SEALED_FRAME_BYTES)
        .on_upgrade(move |mut socket| async move {
            let opened = tokio::time::timeout(
                HANDSHAKE_LIMIT,
                open_sealed(
                    &mut socket,
                    &state,
                    origin,
                    home_route.map(|Extension(route)| route),
                ),
            )
            .await;
            drop(slot);
            match opened {
                Ok(Some((credentials, channel))) => {
                    serve(socket, state, credentials, Some(channel)).await
                }
                _ => {
                    let _ = socket.close().await;
                }
            }
        })
}

/// Runs the sealed handshake, then reads the session from the first sealed
/// frame, or renews it ([`sealed::Renewal`]). `None` ends the connection; a
/// client whose session is not valid is told so, sealed, first.
async fn open_sealed(
    socket: &mut WebSocket,
    state: &AppState,
    origin: String,
    home_route: Option<crate::lan::HomeRoute>,
) -> Option<(Credentials, (sealed::Sealer, sealed::Opener))> {
    let Message::Text(hello) = next_message(socket).await? else {
        return None;
    };
    let host_key = crate::lan::host_key(&state.data_dir).await.ok()?;
    let (welcome, mut sealer, mut opener) =
        sealed::accept(hello.as_str(), &host_key, &origin).ok()?;
    socket.send(Message::Text(welcome.into())).await.ok()?;
    let opening: sealed::Opening = read_sealed(socket, &mut opener).await?;
    let verified = match opening {
        sealed::Opening::Session(session) => state
            .pairing
            .lock()
            .await
            .verify_session(&session, None, now_ms())
            .map(|device_id| (device_id, session))
            .map_err(|_| None),
        sealed::Opening::Renew(request) => {
            renew_sealed(socket, state, &origin, request, &mut sealer, &mut opener).await
        }
    };
    let (device_id, session) = match verified {
        Ok(verified) => verified,
        Err(refusal) => {
            let refused = encode(
                None,
                Err(refusal.unwrap_or_else(|| {
                    RpcError::new(
                        codes::for_http_status(401),
                        "This device is not signed in to this computer. Pair it again.",
                    )
                })),
            );
            if let Ok(frame) = sealer.seal(refused.as_bytes()) {
                let _ = socket.send(Message::Binary(frame.into())).await;
            }
            return None;
        }
    };
    Some((
        Credentials::Device {
            device_id,
            session,
            origin,
            csrf: None,
            home_route,
            sealed: true,
        },
        (sealer, opener),
    ))
}

async fn next_message(socket: &mut WebSocket) -> Option<Message> {
    loop {
        match socket.recv().await? {
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(message) => return Some(message),
            Err(_) => return None,
        }
    }
}

/// The next handshake frame, which must open and parse as `T`.
async fn read_sealed<T: serde::de::DeserializeOwned>(
    socket: &mut WebSocket,
    opener: &mut sealed::Opener,
) -> Option<T> {
    let Message::Binary(frame) = next_message(socket).await? else {
        return None;
    };
    serde_json::from_slice(&opener.open(&frame).ok()?).ok()
}

/// Renews an ended session inside the handshake, so a device that reaches
/// this host only through the relay can sign in again. It is the same proof
/// as `POST /api/v1/pairing/session`: a confirmed, unrevoked device signs a
/// one-time challenge for the origin this socket arrived on with the key it
/// paired with. Nothing else is served until it succeeds, and it runs within
/// the handshake's slot and time limit. `Err(None)` is the ordinary refusal;
/// `Err(Some(_))` a more specific one.
async fn renew_sealed(
    socket: &mut WebSocket,
    state: &AppState,
    origin: &str,
    request: sealed::RenewRequest,
    sealer: &mut sealed::Sealer,
    opener: &mut sealed::Opener,
) -> Result<(String, String), Option<RpcError>> {
    let issued = state.pairing.lock().await.issue_fresh_challenge(
        &request.device_id,
        origin,
        &state.host_installation_id,
        now_ms(),
    );
    let challenge = match issued {
        Ok(challenge) => challenge,
        Err(error) => {
            log_renewal_refusal(
                &format!("device {}", crate::id_prefix(&request.device_id)),
                &error,
            );
            return Err(None);
        }
    };
    let sent = serde_json::to_vec(&sealed::Renewal::Challenge(challenge.clone()))
        .ok()
        .and_then(|frame| sealer.seal(&frame).ok());
    let Some(frame) = sent else {
        return Err(None);
    };
    if socket.send(Message::Binary(frame.into())).await.is_err() {
        return Err(None);
    }
    let Some(answer) = read_sealed::<sealed::RenewAnswer>(socket, opener).await else {
        return Err(None);
    };
    // Only the challenge issued on this connection, never one from elsewhere.
    if answer.challenge_id != challenge.challenge_id {
        return Err(None);
    }
    let mut pairing = state.pairing.lock().await;
    let verified = pairing.verify_challenge(&answer.challenge_id, &answer.signature, now_ms());
    let session = match verified {
        Ok(session) => session,
        Err(error) => {
            log_renewal_refusal(
                &format!("device {}", crate::id_prefix(&request.device_id)),
                &error,
            );
            return Err(None);
        }
    };
    if state
        .store
        .insert_session(
            &wonder_api::pairing_protocol::session_token_hash(&session.session_token),
            &session.device_id,
            &wonder_api::pairing_protocol::csrf_token_hash(&session.csrf_token),
            session.expires_at_ms,
            &now_ms().to_string(),
        )
        .await
        .is_err()
    {
        pairing.rollback_session(&answer.challenge_id, &session);
        return Err(Some(RpcError::new(
            codes::for_http_status(503),
            "This computer could not sign this device in just now. Try again.",
        )));
    }
    drop(pairing);
    let renewed = sealed::Renewal::Renewed(sealed::RenewedSession {
        session_token: session.session_token.clone(),
        csrf_token: session.csrf_token,
        device_id: session.device_id.clone(),
        host_installation_id: session.host_installation_id,
        expires_at_ms: (session.expires_at_ms != wonder_api::pairing_protocol::NEVER_EXPIRES_AT_MS)
            .then_some(session.expires_at_ms),
    });
    let frame = serde_json::to_vec(&renewed)
        .ok()
        .and_then(|frame| sealer.seal(&frame).ok())
        .ok_or(None)?;
    socket
        .send(Message::Binary(frame.into()))
        .await
        .map_err(|_| None)?;
    Ok((session.device_id, session.session_token))
}

fn log_renewal_refusal(subject: &str, error: &wonder_api::pairing_protocol::PairingError) {
    if let Some(reason) = crate::pairing_refusal_reason(error) {
        crate::log_device_auth_refusal("relay session renewal", subject, reason);
    }
}

struct Session {
    state: AppState,
    app: Router,
    credentials: Credentials,
    initialized: bool,
    compact: bool,
    in_flight: Arc<Semaphore>,
    subscriptions: HashMap<String, Subscription>,
    out: mpsc::Sender<Outgoing>,
    /// See [`MAX_QUEUED_REPLY_BYTES`].
    queued: Arc<Semaphore>,
}

async fn serve(
    socket: WebSocket,
    state: AppState,
    credentials: Credentials,
    channel: Option<(sealed::Sealer, sealed::Opener)>,
) {
    // Subscribe before anything else so a revocation cannot slip between
    // the upgrade and the first call.
    let mut revocations = state.revocations.subscribe();
    let (mut sealer, mut opener) = channel.unzip();
    let (mut sink, mut stream) = socket.split();
    let (out, mut outgoing) = mpsc::channel::<Outgoing>(256);
    let writer = tokio::spawn(async move {
        let mut ping =
            tokio::time::interval_at(tokio::time::Instant::now() + PING_AFTER, PING_AFTER);
        loop {
            tokio::select! {
                next = outgoing.recv() => match next {
                    Some(Outgoing { text, queued }) => {
                        // One writer seals every frame, so nonces follow send order.
                        let message = match sealer.as_mut() {
                            Some(sealer) => match sealer.seal(text.as_bytes()) {
                                Ok(frame) => Message::Binary(frame.into()),
                                Err(_) => break,
                            },
                            None => Message::Text(text.into()),
                        };
                        let sent = sink.send(message).await;
                        // The reply leaves the queue budget once it is written.
                        drop(queued);
                        if sent.is_err() { break }
                    }
                    None => break,
                },
                _ = ping.tick() => if sink.send(Message::Ping(Default::default())).await.is_err() { break },
            }
        }
        let _ = sink.close().await;
    });
    let mut session = Session {
        app: crate::router(state.clone()),
        state,
        credentials,
        initialized: false,
        compact: false,
        in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        subscriptions: HashMap::new(),
        out,
        queued: Arc::new(Semaphore::new(MAX_QUEUED_REPLY_BYTES / QUEUE_UNIT)),
    };
    let mut session_deadline: Option<tokio::time::Instant> = None;
    let mut last_received = tokio::time::Instant::now();
    // A frame is read only once its reply has a place in the outgoing
    // queue, so a peer that stops reading stalls its own requests and never
    // this loop: revocation, expiry and the idle limit stay live.
    let mut slot: Option<Slot> = None;
    loop {
        let expiry = async {
            match session_deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            reserved = session.out.clone().reserve_owned(), if slot.is_none() => match reserved {
                Ok(place) => slot = Some(Slot { place, budget: session.queued.clone() }),
                Err(_) => break,
            },
            frame = stream.next(), if slot.is_some() => {
                last_received = tokio::time::Instant::now();
                match frame {
                    // On a sealed connection only frames that open are read;
                    // anything else (altered, replayed, plaintext) ends it.
                    Some(Ok(Message::Binary(frame))) if opener.is_some() => {
                        let Some(text) = opener
                            .as_mut()
                            .and_then(|opener| opener.open(&frame).ok())
                            .and_then(|plain| String::from_utf8(plain).ok())
                        else {
                            break;
                        };
                        let slot = slot.take().expect("guarded");
                        if let Some(deadline) = session.handle(&text, slot).await {
                            session_deadline = deadline;
                        }
                    }
                    Some(Ok(Message::Text(_))) if opener.is_some() => break,
                    Some(Ok(Message::Text(text))) => {
                        let slot = slot.take().expect("guarded");
                        if let Some(deadline) = session.handle(text.as_str(), slot).await {
                            session_deadline = deadline;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Binary(_))) => {
                        reply(slot.take().expect("guarded"), None, Err(RpcError::new(
                            codes::INVALID_REQUEST,
                            "Send each request as one JSON text frame.",
                        )));
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
            revoked = revocations.recv() => match revoked {
                Ok(device_id) if session.device_id() == Some(device_id.as_str()) => break,
                Ok(_) => {}
                // A device that may have missed its own revocation stops, as
                // `/api/v1/events` does; the owner has nothing to revoke.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    if session.device_id().is_none() => {}
                Err(_) => break,
            },
            _ = expiry => {
                // Recheck the pairing state rather than trusting the timer.
                match session.session_deadline().await {
                    Ok(deadline) => session_deadline = deadline,
                    Err(()) => break,
                }
            }
            _ = tokio::time::sleep_until(last_received + IDLE_LIMIT) => break,
        }
    }
    for (_, subscription) in session.subscriptions.drain() {
        subscription.task.abort();
    }
    drop(session);
    // Answers already on their way get a moment; calls still running (a
    // provider sign-in) must not hold a closed or revoked socket open.
    let mut writer = writer;
    if tokio::time::timeout(CLOSE_GRACE, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

type Reply = Result<Value, RpcError>;

/// One frame waiting for the writer. A counted reply carries its share of the
/// connection's queue budget until the frame has been written.
pub(crate) struct Outgoing {
    text: String,
    queued: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl From<String> for Outgoing {
    fn from(text: String) -> Self {
        Self { text, queued: None }
    }
}

/// A reserved place in the outgoing queue for one reply.
struct Slot {
    place: mpsc::OwnedPermit<Outgoing>,
    /// [`MAX_QUEUED_REPLY_BYTES`], in [`QUEUE_UNIT`]s, shared by the connection.
    budget: Arc<Semaphore>,
}

impl Slot {
    fn send(self, id: Option<RequestId>, text: String) {
        if text.len() <= UNCOUNTED_REPLY_BYTES {
            self.place.send(text.into());
            return;
        }
        let units = u32::try_from(text.len().div_ceil(QUEUE_UNIT)).unwrap_or(u32::MAX);
        match self.budget.try_acquire_many_owned(units) {
            Ok(queued) => {
                self.place.send(Outgoing {
                    text,
                    queued: Some(queued),
                });
            }
            Err(_) => {
                self.place.send(
                    encode(
                        id,
                        Err(RpcError::new(
                            codes::BUSY,
                            "Too many answers are waiting for this connection to read them. \
                             Read what has arrived, then ask again.",
                        )),
                    )
                    .into(),
                );
            }
        }
    }
}

fn reply(slot: Slot, id: Option<RequestId>, reply: Reply) {
    reply_or(slot, id, reply, too_large);
}

/// Sends `reply`, or `oversized()` in its place when the reply would not fit
/// in one frame the phone reads.
fn reply_or(slot: Slot, id: Option<RequestId>, reply: Reply, oversized: fn() -> Reply) {
    let text = capped(id.clone(), reply, oversized);
    slot.send(id, text);
}

fn capped(id: Option<RequestId>, reply: Reply, oversized: fn() -> Reply) -> String {
    let text = encode(id.clone(), reply);
    if text.len() + sealed::TAG_BYTES <= MAX_RESPONSE_WIRE_BYTES {
        text
    } else {
        encode(id, oversized())
    }
}

/// A routed call's answer that does not fit, refused as the route's own 413
/// would be.
fn too_large() -> Reply {
    Err(RpcError::from_http(
        413,
        json!({ "detail": TOO_LARGE }).to_string().as_bytes(),
    ))
}

/// `http.request`'s answer that does not fit: an HTTP 413, so the phone
/// handles it as it does at home and the connection stays up.
fn http_too_large() -> Reply {
    use base64::Engine as _;
    Ok(serde_json::to_value(super::HttpResponse {
        body_encoding: None,
        uncompressed_bytes: None,
        status: 413,
        content_type: Some("application/json".into()),
        error_code: None,
        preview_sha256: None,
        body: base64::engine::general_purpose::STANDARD
            .encode(json!({ "detail": TOO_LARGE_FOR_RELAY }).to_string()),
    })
    .unwrap_or(Value::Null))
}

enum Collected {
    TooLarge,
    Failed,
}

/// Reads a route's body up to [`MAX_RESULT_BYTES`].
async fn collect(body: Body) -> Result<Vec<u8>, Collected> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Collected::Failed)?;
        if bytes.len() + chunk.len() > MAX_RESULT_BYTES {
            return Err(Collected::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn encode(id: Option<RequestId>, reply: Reply) -> String {
    let (result, error) = match reply {
        Ok(result) => (Some(result), None),
        Err(error) => (None, Some(error)),
    };
    serde_json::to_string(&Response {
        jsonrpc: JSONRPC.into(),
        id,
        result,
        error,
    })
    .unwrap_or_default()
}

pub(crate) fn notification(method: &str, params: impl Serialize) -> String {
    notification_value(method, serde_json::to_value(params).unwrap_or(Value::Null))
}

/// [`notification`] for params that are already a `Value`, moved in rather
/// than copied through a second `to_value`.
fn notification_value(method: &str, params: Value) -> String {
    serde_json::to_string(&Notification {
        jsonrpc: JSONRPC.into(),
        method: method.into(),
        params,
    })
    .unwrap_or_default()
}

/// The `event` or `events.batch` frame of one subscription. The events are
/// turned into JSON once and moved into the frame; the frame's text equals
/// `notification(method, EventNotification | EventBatchNotification)`.
fn subscription_frame(method: &str, subscription_id: &str, field: &str, events: Value) -> String {
    let mut params = serde_json::Map::with_capacity(2);
    params.insert("subscriptionId".into(), subscription_id.into());
    params.insert(field.into(), events);
    notification_value(method, Value::Object(params))
}

impl Session {
    fn device_id(&self) -> Option<&str> {
        match &self.credentials {
            Credentials::Device { device_id, .. } => Some(device_id),
            Credentials::Owner { .. } => None,
        }
    }

    fn caller(&self) -> Caller {
        match &self.credentials {
            Credentials::Owner { .. } => Caller::Owner,
            Credentials::Device { device_id, .. } => Caller::Device(device_id.clone()),
        }
    }

    /// Handles one frame, answering into `slot` without waiting for queue
    /// space. Returns a new session deadline after `initialize`.
    async fn handle(&mut self, text: &str, slot: Slot) -> Option<Option<tokio::time::Instant>> {
        let (id, request) = match super::read_frame(text) {
            super::Incoming::Call(id, request) => (id, request),
            // No client notifications are defined; ignore them, as JSON-RPC asks.
            super::Incoming::Notification => return None,
            super::Incoming::Invalid(id, error) => {
                reply(slot, id, Err(error));
                return None;
            }
        };
        let Some(method) = super::method(&request.method) else {
            reply(
                slot,
                Some(id),
                Err(RpcError::new(
                    codes::METHOD_NOT_FOUND,
                    format!("Unknown method `{}`.", request.method),
                )),
            );
            return None;
        };
        if method.name == "initialize" {
            let (answer, deadline) = match self.initialize(request.params).await {
                Ok((result, deadline)) => (Ok(result), Some(deadline)),
                Err(error) => (Err(error), None),
            };
            reply(slot, Some(id), answer);
            return deadline;
        }
        if !self.initialized && !method.before_initialize {
            reply(
                slot,
                Some(id),
                Err(RpcError::new(
                    codes::NOT_INITIALIZED,
                    "Call `initialize` first.",
                )),
            );
            return None;
        }
        match method.name {
            "auth.challenge" if self.device_id().is_none() => reply(
                slot,
                Some(id),
                Err(RpcError::new(
                    codes::for_http_status(403),
                    "Only a paired device signs in with a challenge. This computer is already \
                     signed in as its owner.",
                )),
            ),
            "events.subscribe" => match self.subscribe(request.params).await {
                Ok((result, plan)) => {
                    // The subscription's first event follows its result.
                    reply(slot, Some(id), Ok(result));
                    self.start(plan);
                }
                Err(error) => reply(slot, Some(id), Err(error)),
            },
            "events.unsubscribe" => {
                let answer = self.unsubscribe(request.params);
                reply(slot, Some(id), answer);
            }
            "http.request" => {
                if !matches!(self.credentials, Credentials::Device { sealed: true, .. }) {
                    reply(
                        slot,
                        Some(id),
                        Err(RpcError::new(
                            codes::for_http_status(403),
                            "Only a paired phone's sealed connection can use `http.request`.",
                        )),
                    );
                    return None;
                }
                let params: HttpRequestParams = match super::parse(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        reply(slot, Some(id), Err(error));
                        return None;
                    }
                };
                let Ok(permit) = self.in_flight.clone().try_acquire_owned() else {
                    reply(
                        slot,
                        Some(id),
                        Err(RpcError::new(
                            codes::BUSY,
                            "Too many requests at once on this connection. Wait for some to finish.",
                        )),
                    );
                    return None;
                };
                let app = self.app.clone();
                let credentials = self.credentials.clone();
                tokio::spawn(async move {
                    let answer = dispatch_http(app, &credentials, params).await;
                    reply_or(slot, Some(id), answer, http_too_large);
                    drop(permit);
                });
            }
            name => {
                let Ok(permit) = self.in_flight.clone().try_acquire_owned() else {
                    reply(slot,
                        Some(id),
                        Err(RpcError::new(
                            codes::BUSY,
                            "Too many requests at once on this connection. Wait for some to finish.",
                        )),
                    );
                    return None;
                };
                let call = route(name, request.params, &self.caller());
                let app = self.app.clone();
                let credentials = self.credentials.clone();
                let compact = self.compact;
                // The call keeps its reserved slot, so its answer never waits.
                tokio::spawn(async move {
                    let answer = match call {
                        Ok(call) => dispatch(app, &credentials, compact, call).await,
                        Err(error) => Err(error),
                    };
                    reply(slot, Some(id), answer);
                    drop(permit);
                });
            }
        }
        None
    }

    async fn initialize(
        &mut self,
        params: Option<Value>,
    ) -> Result<(Value, Option<tokio::time::Instant>), RpcError> {
        if self.initialized {
            return Err(RpcError::new(
                codes::ALREADY_INITIALIZED,
                "This connection is already initialized.",
            ));
        }
        let params: InitializeParams = super::parse(params)?;
        if params.protocol_version != PROTOCOL_VERSION {
            let mut error = RpcError::new(
                codes::UNSUPPORTED_VERSION,
                format!(
                    "This computer speaks Cupola RPC version {PROTOCOL_VERSION}. Update Cupola \
                     on this computer or the client so they match."
                ),
            );
            error.data = Some(ErrorData {
                supported: Some(vec![PROTOCOL_VERSION]),
                ..Default::default()
            });
            return Err(error);
        }
        let state = self.state.clone();
        let mut deadline = None;
        if let Credentials::Device {
            device_id,
            session,
            csrf,
            ..
        } = &mut self.credentials
        {
            // The same proof `/api/v1/events` asks for: the session with its
            // CSRF token, then a signature over a fresh one-time challenge.
            let refused = || {
                RpcError::new(
                    codes::for_http_status(401),
                    "This device is not signed in to this computer. Pair it again.",
                )
            };
            let Some(auth) = params.auth.as_ref() else {
                return Err(RpcError::invalid_params(
                    "a paired device must send `auth` (see auth.challenge)",
                ));
            };
            let mut pairing = state.pairing.lock().await;
            let now = now_ms();
            let verified = pairing
                .verify_session(session, Some(&auth.csrf_token), now)
                .map_err(|_| refused())?;
            if verified != *device_id || auth.device_id != *device_id {
                return Err(refused());
            }
            pairing
                .verify_fresh_challenge(&auth.challenge_id, device_id, &auth.signature, now_ms())
                .map_err(|_| refused())?;
            deadline = pairing
                .session_expires_at_ms(session)
                .and_then(|expires| deadline_after(expires.saturating_sub(now)));
            *csrf = Some(auth.csrf_token.clone());
        }
        self.initialized = true;
        self.compact = params.capabilities.compact_view;
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            server_info: ServerInfo {
                name: "wonderd".into(),
                version: crate::RELEASE_VERSION.into(),
                api_version: "v1".into(),
            },
            authority: match self.credentials {
                Credentials::Owner { .. } => "owner",
                Credentials::Device { .. } => "device",
            }
            .into(),
            device_id: self.device_id().map(str::to_owned),
            host_epoch: state.host_epoch.clone(),
            host_installation_id: state.host_installation_id.clone(),
            methods: METHODS.iter().map(|m| m.name.to_owned()).collect(),
            notifications: NOTIFICATIONS.iter().map(|(n, _)| (*n).to_owned()).collect(),
            capabilities: ServerCapabilities {
                compact_view: true,
                event_batches: true,
                history_compression: vec![crate::history_compression::RAW_DEFLATE.into()],
                max_in_flight: MAX_IN_FLIGHT as u32,
                max_request_bytes: MAX_REQUEST_BYTES as u32,
            },
        };
        Ok((
            serde_json::to_value(result).unwrap_or(Value::Null),
            deadline,
        ))
    }

    /// The session's next deadline, or `Err` once it has ended.
    async fn session_deadline(&self) -> Result<Option<tokio::time::Instant>, ()> {
        let Credentials::Device { session, .. } = &self.credentials else {
            return Ok(None);
        };
        let pairing = self.state.pairing.lock().await;
        let now = now_ms();
        pairing.verify_session(session, None, now).map_err(|_| ())?;
        Ok(pairing
            .session_expires_at_ms(session)
            .and_then(|expires| deadline_after(expires.saturating_sub(now).max(1))))
    }

    async fn subscribe(&mut self, params: Option<Value>) -> Result<(Value, Plan), RpcError> {
        let params: EventsSubscribeParams = super::parse(params)?;
        // A subscription that ended (`events.ended`) no longer holds a place.
        self.subscriptions
            .retain(|_, subscription| subscription.running());
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return Err(RpcError::new(
                codes::BUSY,
                "Too many event subscriptions on this connection.",
            ));
        }
        let state = &self.state;
        let cursor = match params.after {
            Some(after) => after,
            None => EventCursor {
                host_epoch: state.host_epoch.clone(),
                sequence: state
                    .store
                    .committed_sequence(&state.host_epoch)
                    .await
                    .map_err(|_| {
                        RpcError::new(
                            codes::for_http_status(503),
                            "Chats could not be refreshed. Try again.",
                        )
                    })?,
            },
        };
        let plan = Plan {
            subscription_id: uuid::Uuid::new_v4().to_string(),
            after: cursor,
            batch: params.batch,
        };
        let result = EventsSubscribeResult {
            subscription_id: plan.subscription_id.clone(),
            cursor: plan.after.clone(),
        };
        Ok((serde_json::to_value(result).unwrap_or(Value::Null), plan))
    }

    fn start(&mut self, plan: Plan) {
        let ended = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(stream(
            self.state.clone(),
            EventQueue {
                out: self.out.clone(),
                budget: self.queued.clone(),
            },
            plan.subscription_id.clone(),
            plan.after,
            self.compact,
            plan.batch,
            ended.clone(),
        ));
        self.subscriptions.insert(
            plan.subscription_id,
            Subscription {
                task: task.abort_handle(),
                ended,
            },
        );
    }

    fn unsubscribe(&mut self, params: Option<Value>) -> Reply {
        let params: EventsUnsubscribeParams = super::parse(params)?;
        match self.subscriptions.remove(&params.subscription_id) {
            Some(subscription) => {
                subscription.task.abort();
                Ok(json!({}))
            }
            None => Err(RpcError::new(
                codes::for_http_status(404),
                "No such subscription on this connection.",
            )),
        }
    }
}

struct Subscription {
    task: tokio::task::AbortHandle,
    /// Set before `events.ended` is queued, so a client that reads it and
    /// subscribes again always finds the place free.
    ended: Arc<AtomicBool>,
}

impl Subscription {
    fn running(&self) -> bool {
        !self.ended.load(Ordering::Acquire) && !self.task.is_finished()
    }
}

/// A subscription to start once its result is on the wire.
struct Plan {
    batch: bool,
    subscription_id: String,
    after: EventCursor,
}

fn deadline_after(remaining_ms: u64) -> Option<tokio::time::Instant> {
    tokio::time::Instant::now().checked_add(Duration::from_millis(remaining_ms))
}

/// Runs one route call through the daemon's router with the connection's
/// credentials.
async fn dispatch(app: Router, credentials: &Credentials, compact: bool, call: RouteCall) -> Reply {
    let mut request = builder(credentials, compact, call.method, call.uri);
    let body = match call.body {
        Some(body) => {
            request = request.header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            Body::from(serde_json::to_vec(&body).unwrap_or_default())
        }
        None => Body::empty(),
    };
    let Ok(request) = request.body(body) else {
        return Err(RpcError::invalid_params("an ID is not usable in a request"));
    };
    let response = match app.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    let bytes = match collect(response.into_body()).await {
        Ok(bytes) => bytes,
        Err(Collected::TooLarge) => return too_large(),
        Err(Collected::Failed) => {
            return Err(RpcError::new(
                codes::INTERNAL_ERROR,
                "The answer could not be read. Try again.",
            ))
        }
    };
    if !status.is_success() {
        return Err(RpcError::from_http(status.as_u16(), &bytes));
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        RpcError::new(
            codes::INTERNAL_ERROR,
            "The host answered with something other than JSON.",
        )
    })
}

/// `http.request`: one of the device's HTTP API calls, answered with its
/// status, error code and body whatever the status, so the phone handles a
/// refusal exactly as it does at home.
async fn dispatch_http(app: Router, credentials: &Credentials, params: HttpRequestParams) -> Reply {
    let method = match params.method.as_str() {
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" => {
            axum::http::Method::from_bytes(params.method.as_bytes())
                .map_err(|_| RpcError::invalid_params("not an HTTP method"))?
        }
        _ => return Err(RpcError::invalid_params("not an HTTP method")),
    };
    if !super::sealed_http_path(&params.path) {
        return Err(RpcError::new(
            codes::for_http_status(404),
            "This isn't available through the relay.",
        ));
    }
    if params
        .response_compression
        .as_deref()
        .is_some_and(|codec| codec != crate::history_compression::RAW_DEFLATE)
    {
        return Err(RpcError::invalid_params("unsupported response compression"));
    }
    let compress_history = params.response_compression.is_some()
        && crate::history_compression::history_path(&params.method, &params.path);
    let mut request = builder(credentials, params.compact, method, params.path);
    let body = match params.body {
        Some(text) => {
            request = request.header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            Body::from(text)
        }
        None => Body::empty(),
    };
    let Ok(request) = request.body(body) else {
        return Err(RpcError::invalid_params("not a usable path"));
    };
    let response = match app.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let (parts, body) = response.into_parts();
    let text = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let status = parts.status.as_u16();
    let content_type = text(header::CONTENT_TYPE.as_str());
    let error_code = text("x-wonder-error");
    let preview_sha256 = text(crate::media_refs::PREVIEW_SHA256_HEADER);
    let bytes = match collect(body).await {
        Ok(bytes) => bytes,
        Err(Collected::TooLarge) => return http_too_large(),
        Err(Collected::Failed) => {
            return Err(RpcError::new(
                codes::INTERNAL_ERROR,
                "The answer could not be read. Try again.",
            ))
        }
    };
    // Compress only the body of an authorized stored-history GET, never
    // JSON-RPC authentication fields, cookies or general event notifications.
    let original_bytes = bytes.len();
    let (bytes, compressed) = if compress_history && status == 200 {
        crate::history_compression::compress(bytes, true).await
    } else {
        (bytes, false)
    };
    Ok(serde_json::to_value(super::HttpResponse {
        body_encoding: compressed.then(|| crate::history_compression::RAW_DEFLATE.into()),
        uncompressed_bytes: compressed.then_some(original_bytes),
        status,
        content_type,
        error_code,
        preview_sha256,
        body: {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        },
    })
    .unwrap_or(Value::Null))
}

/// A router request carrying the connection's credentials.
fn builder(
    credentials: &Credentials,
    compact: bool,
    method: axum::http::Method,
    uri: String,
) -> axum::http::request::Builder {
    let mut request = axum::http::Request::builder().method(method).uri(uri);
    match credentials {
        Credentials::Owner { capability } => {
            request = request
                .header(header::HOST, "localhost")
                .header("x-wonder-loopback-capability", capability);
        }
        Credentials::Device {
            session,
            origin,
            csrf,
            home_route,
            sealed,
            ..
        } => {
            // The call names the route the socket was accepted on (a relay
            // origin is trusted only for its own address).
            request = request
                .header(
                    header::HOST,
                    origin.strip_prefix("https://").unwrap_or("localhost"),
                )
                .header(header::COOKIE, format!("__Host-wonder_session={session}"))
                .header(header::ORIGIN, origin);
            if let Some(route) = home_route {
                request = request.extension(route.clone());
            }
            if *sealed {
                request = request.extension(SealedCall);
            }
            if let Some(csrf) = csrf {
                request = request.header("x-wonder-csrf", csrf);
            }
        }
    }
    if compact {
        request = request.header(COMPACT_VIEW_HEADER, "compact");
    }
    request
}

/// Whether an event frame fits the phone's event bound once sealed, tag
/// included.
fn event_frame_fits(text: &str) -> bool {
    text.len() + sealed::TAG_BYTES <= wonder_api::EVENT_FRAME_MAX_BYTES
}

/// Where a subscription's frames go: the connection's queue and its byte
/// budget.
#[derive(Clone)]
struct EventQueue {
    out: mpsc::Sender<Outgoing>,
    budget: Arc<Semaphore>,
}

impl EventQueue {
    /// Queues one event frame against the connection's byte budget. Unlike an
    /// answer, an event is never replaced by `BUSY`: the stream waits for the
    /// budget (the journal keeps what it has not sent), and the permit
    /// returns when the writer has sent the frame. Errs when the connection
    /// is gone.
    async fn send(&self, text: String) -> Result<(), ()> {
        if text.len() <= UNCOUNTED_REPLY_BYTES {
            return self.out.send(text.into()).await.map_err(|_| ());
        }
        let units = u32::try_from(text.len().div_ceil(QUEUE_UNIT)).unwrap_or(u32::MAX);
        let queued = self
            .budget
            .clone()
            .acquire_many_owned(units)
            .await
            .map_err(|_| ())?;
        self.out
            .send(Outgoing {
                text,
                queued: Some(queued),
            })
            .await
            .map_err(|_| ())
    }
}

/// Sends one subscription's events, in journal order, from `after`.
async fn stream(
    state: AppState,
    queue: EventQueue,
    subscription_id: String,
    after: EventCursor,
    compact: bool,
    batch: bool,
    ended_flag: Arc<AtomicBool>,
) {
    // Subscribe before the first read so no commit falls between them.
    let mut wakeups = state.events.subscribe();
    let mut journal = state
        .journal_wake
        .subscribe(state.store.clone(), state.host_epoch.clone());
    let ended = |detail: String| {
        ended_flag.store(true, Ordering::Release);
        notification(
            "events.ended",
            EventsEnded {
                subscription_id: subscription_id.clone(),
                reason: "resync".into(),
                detail: Some(detail),
            },
        )
    };
    if after.host_epoch != state.host_epoch {
        let _ = queue.out.send(ended("epochChanged".into()).into()).await;
        return;
    }
    let mut last = after.sequence;
    loop {
        let mut events = match state.store.replay_batch(&state.host_epoch, last).await {
            Ok(wonder_store::ReplayBatch::Events(events)) => events,
            Ok(wonder_store::ReplayBatch::Resync(reason)) => {
                let detail = serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "resync".into());
                let _ = queue.out.send(ended(detail).into()).await;
                return;
            }
            Err(_) => {
                let _ = queue
                    .out
                    .send(ended("replayWindowExpired".into()).into())
                    .await;
                return;
            }
        };
        if events.is_empty() {
            tokio::select! {
                wakeup = wakeups.recv() => {
                    if matches!(wakeup, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return;
                    }
                }
                changed = journal.changed() => { if changed.is_err() { return; } }
            }
            continue;
        }
        if compact {
            events.iter_mut().for_each(compact_event);
        }
        crate::history::hide_thinking_events(&state, &mut events).await;
        if batch {
            let sequence = events.last().expect("nonempty replay").sequence;
            let text = subscription_frame(
                "events.batch",
                &subscription_id,
                "events",
                Value::Array(
                    events
                        .iter()
                        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
                        .collect(),
                ),
            );
            if !event_frame_fits(&text) {
                let _ = queue
                    .out
                    .send(ended("replayWindowExpired".into()).into())
                    .await;
                return;
            }
            if queue.send(text).await.is_err() {
                return;
            }
            last = sequence;
            // The first frame is immediate; subsequent commits coalesce into
            // at most 20 incremental frames/s, just like the direct socket.
            tokio::time::sleep(crate::sync::FRAME_INTERVAL).await;
            continue;
        }
        for event in events {
            let sequence = event.sequence;
            let text = subscription_frame(
                "event",
                &subscription_id,
                "event",
                serde_json::to_value(&event).unwrap_or(Value::Null),
            );
            // A slow reader holds the stream here; the journal, not memory,
            // keeps what it has not read yet.
            if queue.send(text).await.is_err() {
                return;
            }
            last = sequence;
        }
    }
}

#[cfg(test)]
mod frames {
    use super::*;

    fn event(sequence: u64) -> Value {
        json!({
            "sequence": sequence, "hostEpoch": "epoch",
            "event": {"type": "assistantDelta", "text": "caf\u{e9} \"quoted\"\n", "zeta": [1, {"b": 2, "a": 1}]},
        })
    }

    /// The frames each subscriber is sent are the text the typed
    /// notifications always produced, key order and escaping included.
    #[test]
    fn a_subscription_frame_equals_the_typed_notification() {
        for subscription in ["sub-1", "sub-2", "sub-3"] {
            let single = subscription_frame("event", subscription, "event", event(7));
            let typed = notification(
                "event",
                EventNotification {
                    subscription_id: subscription.into(),
                    event: event(7),
                },
            );
            assert_eq!(single, typed);
            let batch = subscription_frame(
                "events.batch",
                subscription,
                "events",
                Value::Array(vec![event(7), event(8)]),
            );
            let typed = notification(
                "events.batch",
                EventBatchNotification {
                    subscription_id: subscription.into(),
                    events: vec![event(7), event(8)],
                },
            );
            assert_eq!(batch, typed);
        }
    }
}

#[cfg(test)]
mod limits {
    use super::*;
    use base64::Engine as _;

    /// Bytes deflate cannot shrink, like a photo.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn phone() -> Credentials {
        Credentials::Device {
            device_id: "phone".into(),
            session: "session-token".into(),
            origin: "https://relay.example".into(),
            csrf: None,
            home_route: None,
            sealed: true,
        }
    }

    async fn get(app: Router, path: &str, compression: bool) -> String {
        let params = HttpRequestParams {
            response_compression: compression
                .then(|| crate::history_compression::RAW_DEFLATE.into()),
            method: "GET".into(),
            path: path.into(),
            body: None,
            compact: false,
        };
        let answer = dispatch_http(app, &phone(), params).await;
        capped(Some(RequestId::Number(7)), answer, http_too_large)
    }

    fn result(frame: &str) -> Value {
        let frame: Value = serde_json::from_str(frame).unwrap();
        assert_eq!(frame["id"], 7);
        frame["result"].clone()
    }

    fn body(result: &Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(result["body"].as_str().unwrap())
            .unwrap()
    }

    #[test]
    fn a_reply_fills_the_phones_frame_and_one_byte_more_is_refused_inside_it() {
        let id = || Some(RequestId::Number(7));
        let envelope = encode(id(), Ok(json!(""))).len();
        let fits = MAX_RESPONSE_WIRE_BYTES - sealed::TAG_BYTES - envelope;
        let frame = capped(id(), Ok(json!("a".repeat(fits))), too_large);
        assert_eq!(frame.len() + sealed::TAG_BYTES, MAX_RESPONSE_WIRE_BYTES);
        let frame = capped(id(), Ok(json!("a".repeat(fits + 1))), too_large);
        assert!(frame.len() < 1024, "{frame}");
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["id"], 7);
        assert_eq!(frame["error"]["code"], codes::for_http_status(413));
        assert_eq!(frame["error"]["data"]["httpStatus"], 413);
        assert_eq!(frame["error"]["message"], TOO_LARGE);

        // `http.request` refuses as the route would: a 413 the phone reads.
        let frame = capped(id(), Ok(json!("a".repeat(fits + 1))), http_too_large);
        let answer = result(&frame);
        assert_eq!(answer["status"], 413);
        let detail: Value = serde_json::from_slice(&body(&answer)).unwrap();
        assert_eq!(detail["detail"], TOO_LARGE_FOR_RELAY);
    }

    #[test]
    fn an_event_batch_counts_the_sealing_tag_against_the_phones_bound() {
        let fits = wonder_api::EVENT_FRAME_MAX_BYTES - sealed::TAG_BYTES;
        assert!(event_frame_fits(&"a".repeat(fits)));
        assert!(!event_frame_fits(&"a".repeat(fits + 1)));
    }

    #[tokio::test]
    async fn a_full_size_attachment_fits_and_a_larger_body_is_a_413_not_an_internal_error() {
        let app = Router::new()
            .route(
                "/api/v1/full",
                axum::routing::get(|| async { incompressible(MAX_RESULT_BYTES) }),
            )
            .route(
                "/api/v1/over",
                axum::routing::get(|| async { incompressible(MAX_RESULT_BYTES + 1) }),
            );
        let frame = get(app.clone(), "/api/v1/full", false).await;
        assert!(frame.len() + sealed::TAG_BYTES <= MAX_RESPONSE_WIRE_BYTES);
        let answer = result(&frame);
        assert_eq!(answer["status"], 200);
        assert_eq!(body(&answer), incompressible(MAX_RESULT_BYTES));

        let frame = get(app, "/api/v1/over", false).await;
        assert!(frame.len() < 1024, "{frame}");
        let answer = result(&frame);
        assert_eq!(answer["status"], 413);
        assert_eq!(answer["contentType"], "application/json");
    }

    #[tokio::test]
    async fn compressed_history_still_fits_and_reports_its_size() {
        let history = json!({ "items": vec!["a message"; 1_000_000] })
            .to_string()
            .into_bytes();
        assert!(history.len() > MAX_RESULT_BYTES / 2);
        let served = history.clone();
        let app = Router::new().route(
            "/api/v1/conversations/{id}/history",
            axum::routing::get(move || async move { served }),
        );
        let frame = get(app, "/api/v1/conversations/t1/history", true).await;
        let answer = result(&frame);
        assert_eq!(answer["status"], 200);
        assert_eq!(
            answer["bodyEncoding"],
            crate::history_compression::RAW_DEFLATE
        );
        assert_eq!(answer["uncompressedBytes"], history.len());
        let sent = body(&answer).len();
        assert!(sent < history.len() / 10, "{sent} of {}", history.len());
    }

    #[tokio::test]
    async fn a_routed_call_over_the_body_budget_is_a_413() {
        let app = Router::new().route(
            "/api/v1/over",
            axum::routing::get(|| async { incompressible(MAX_RESULT_BYTES + 1) }),
        );
        let call = RouteCall {
            method: axum::http::Method::GET,
            uri: "/api/v1/over".into(),
            body: None,
        };
        let error = dispatch(app, &phone(), false, call).await.unwrap_err();
        assert_eq!(error.code, codes::for_http_status(413));
        assert_eq!(error.message, TOO_LARGE);
    }
}

#[cfg(test)]
mod queue_budget {
    use super::*;

    /// HUNT-RELAY finding 4: the reply queue holds 256 replies and each may
    /// be [`MAX_RESPONSE_WIRE_BYTES`], so a device that asked for big answers
    /// and never read them could pin gigabytes of the host's memory. Answers
    /// past the connection's byte budget become a small `BUSY` error, and the
    /// budget comes back as the writer drops what it has sent.
    #[tokio::test]
    async fn answers_a_peer_has_not_read_stop_at_the_queue_budget() {
        let (out, mut queue) = mpsc::channel::<Outgoing>(256);
        let budget = Arc::new(Semaphore::new(4 * 1024)); // 4 MiB, in units
        let ask = |id: i64| {
            let out = out.clone();
            let budget = budget.clone();
            async move {
                let place = out.reserve_owned().await.unwrap();
                reply(
                    Slot { place, budget },
                    Some(RequestId::Number(id)),
                    Ok(json!({ "data": "x".repeat(1_000_000) })),
                );
            }
        };
        for id in 0..16 {
            ask(id).await;
        }
        let mut waiting = Vec::new();
        while let Ok(item) = queue.try_recv() {
            waiting.push(item);
        }
        assert_eq!(
            waiting.len(),
            16,
            "every request is answered, one way or the other"
        );
        let is_busy = |item: &Outgoing| {
            serde_json::from_str::<Value>(&item.text).unwrap()["error"]["code"] == codes::BUSY
        };
        let held: usize = waiting
            .iter()
            .filter(|item| !is_busy(item))
            .map(|item| item.text.len())
            .sum();
        assert!(
            held <= 4 * 1024 * 1024,
            "{held} bytes of answers queued past the budget"
        );
        let busy: Vec<&Outgoing> = waiting.iter().filter(|item| is_busy(item)).collect();
        assert!(!busy.is_empty(), "nothing was held back");
        for item in &busy {
            assert!(item.text.len() < UNCOUNTED_REPLY_BYTES);
        }
        // Once the writer has sent them, the budget is whole again.
        drop(waiting);
        ask(99).await;
        let next = queue.recv().await.unwrap();
        assert!(
            next.queued.is_some(),
            "the budget returned with the sent replies"
        );
    }

    /// HUNT-RELAY finding 1 (review): event frames (up to 1 MiB, 256 queued)
    /// must count against the same budget. They wait instead of failing, and
    /// resume when the writer has sent what is queued.
    #[tokio::test]
    async fn event_frames_wait_for_the_queue_budget() {
        let (out, mut queue) = mpsc::channel::<Outgoing>(256);
        let budget = Arc::new(Semaphore::new(2 * 1024)); // 2 MiB, in units
        let events = EventQueue {
            out: out.clone(),
            budget: budget.clone(),
        };
        let frame = || "e".repeat(1024 * 1024);
        events.send(frame()).await.unwrap();
        events.send(frame()).await.unwrap();
        let third = tokio::spawn({
            let events = events.clone();
            async move { events.send(frame()).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !third.is_finished(),
            "a third 1 MiB frame fit a 2 MiB budget"
        );
        let mut queued = 0;
        while queue.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, 2, "only the frames inside the budget were queued");
        // The two sent frames were dropped above, so the budget is whole again.
        tokio::time::timeout(Duration::from_secs(10), third)
            .await
            .expect("the stream resumes once the writer has sent")
            .unwrap()
            .unwrap();
        // Small frames (the `events.ended` notice) are never held back.
        let rest = u32::try_from(budget.available_permits()).unwrap();
        let _all_taken = budget.clone().try_acquire_many_owned(rest).unwrap();
        tokio::time::timeout(Duration::from_secs(10), events.send("{}".into()))
            .await
            .expect("a small frame is not held back")
            .unwrap();
    }
}

#[cfg(all(test, unix))]
mod authority_tests {
    use super::*;

    // The dispatcher owns the refusal, both before and after initialization.
    // A local owner must hear a useful explanation without framework or wire terms.
    #[cfg(unix)]
    #[tokio::test]
    async fn owner_challenges_explain_that_this_computer_is_already_signed_in() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let (out, mut replies) = mpsc::channel(4);
        let mut session = Session {
            app: crate::router(state.clone()),
            credentials: Credentials::Owner {
                capability: state.loopback_capability.clone(),
            },
            state,
            initialized: false,
            compact: false,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            subscriptions: HashMap::new(),
            out,
            queued: Arc::new(Semaphore::new(MAX_QUEUED_REPLY_BYTES / QUEUE_UNIT)),
        };
        for method in ["auth.challenge", "initialize", "auth.challenge"] {
            let params = if method == "initialize" {
                json!({"protocolVersion": PROTOCOL_VERSION, "clientInfo": {"name": "test"}})
            } else {
                Value::Null
            };
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
            let place = session.out.clone().reserve_owned().await.unwrap();
            let slot = Slot {
                place,
                budget: session.queued.clone(),
            };
            session.handle(&request.to_string(), slot).await;
            let response: Value =
                serde_json::from_str(&replies.recv().await.unwrap().text).unwrap();
            if method == "initialize" {
                assert_eq!(response["result"]["authority"], "owner");
            } else {
                assert_eq!(response["error"]["code"], codes::for_http_status(403));
                assert_eq!(response["error"]["message"],
                    "Only a paired device signs in with a challenge. This computer is already signed in as its owner.");
            }
        }
    }
}
