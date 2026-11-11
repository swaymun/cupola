//! Snapshot/replay transport reads committed storage; broadcast is only a wakeup.
use super::*;
use axum::extract::ws::{Message, WebSocket};

/// One fallback journal poll per host, shared by every socket/subscription.
/// SQL-triggered invalidations do not pass through publish_event. Keep that
/// safety net, but wake clients only when its committed cursor changes. It
/// polls every 100 ms for a second after a change, then every 500 ms until
/// the next one, so a quiet host is not woken ten times a second.
#[derive(Clone, Default)]
pub struct JournalWake {
    inner: Arc<std::sync::Mutex<std::sync::Weak<tokio::sync::watch::Sender<()>>>>,
    #[cfg(test)]
    polls: Arc<std::sync::atomic::AtomicUsize>,
}

const JOURNAL_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const JOURNAL_QUIET_POLL: std::time::Duration = std::time::Duration::from_millis(500);
const JOURNAL_ACTIVE: std::time::Duration = std::time::Duration::from_secs(1);

impl JournalWake {
    pub(crate) fn subscribe(
        &self,
        store: Store,
        epoch: String,
    ) -> tokio::sync::watch::Receiver<()> {
        let mut owner = self.inner.lock().unwrap();
        if let Some(sender) = owner.upgrade() {
            let mut receiver = sender.subscribe();
            receiver.mark_changed(); // initial replay, including an empty journal
            return receiver;
        }
        let (sender, mut receiver) = tokio::sync::watch::channel(());
        receiver.mark_changed();
        let sender = Arc::new(sender);
        *owner = Arc::downgrade(&sender);
        let owner = self.inner.clone();
        #[cfg(test)]
        let polls = self.polls.clone();
        tokio::spawn(async move {
            let mut previous = None;
            let mut changed_at = tokio::time::Instant::now();
            loop {
                let period = if changed_at.elapsed() < JOURNAL_ACTIVE {
                    JOURNAL_POLL
                } else {
                    JOURNAL_QUIET_POLL
                };
                tokio::select! {
                    _ = sender.closed() => {
                        // A subscriber and final-unsubscribe can race. Make
                        // retirement atomic with choosing/creating a watcher.
                        let mut owner = owner.lock().unwrap();
                        if sender.receiver_count() == 0 { *owner = Default::default(); return; }
                    }
                    _ = tokio::time::sleep(period) => {
                        #[cfg(test)]
                        polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let current = store.committed_sequence(&epoch).await.ok();
                        if current != previous || current.is_none() {
                            if current != previous {
                                changed_at = tokio::time::Instant::now();
                            }
                            previous = current;
                            sender.send_replace(());
                        }
                    }
                }
            }
        });
        receiver
    }
}

/// Read before fetching independent projections. The client invalidates every
/// old scope before installing this checkpoint, then replays strictly after it.
pub(super) async fn checkpoint(State(state): State<AppState>) -> Response {
    match state.store.committed_sequence(&state.host_epoch).await {
        Ok(sequence) => Json(serde_json::json!({
            "hostEpoch": state.host_epoch, "lastSequence": sequence,
            "hostInstallationId": state.host_installation_id
        }))
        .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Chats could not be refreshed. Try again.",
        )
            .into_response(),
    }
}

pub(super) async fn stream_events(
    socket: WebSocket,
    state: AppState,
    session_token: Option<String>,
    local_capability: bool,
    compact: bool,
) {
    stream_events_with_keepalive(
        socket,
        state,
        session_token,
        local_capability,
        compact,
        EVENT_KEEPALIVE,
    )
    .await;
}

#[derive(Clone, Copy)]
struct Keepalive {
    probe_after: std::time::Duration,
    response_timeout: std::time::Duration,
}

const EVENT_KEEPALIVE: Keepalive = Keepalive {
    probe_after: std::time::Duration::from_secs(15),
    response_timeout: std::time::Duration::from_secs(15),
};

async fn stream_events_with_keepalive(
    mut socket: WebSocket,
    state: AppState,
    session_token: Option<String>,
    local_capability: bool,
    compact: bool,
    keepalive: Keepalive,
) {
    // Subscribe before authentication/replay so a revocation cannot race the
    // setup window and leave a live socket authorized after its device is
    // revoked.
    let mut revocations = state.revocations.subscribe();
    let subscription = match socket.recv().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<EventSubscription>(&text).ok(),
        _ => None,
    };
    let Some(subscription) = subscription else {
        return;
    };
    // A paired phone's stream ends when its session expires, as HTTP does.
    let mut session_deadline: Option<(String, tokio::time::Instant)> = None;
    if !local_capability {
        let Some(session_token) = session_token else {
            return;
        };
        let Some(csrf_token) = subscription.csrf_token.as_deref() else {
            return;
        };
        let device_id = {
            let pairing = state.pairing.lock().await;
            let now = now_ms();
            let Ok(device_id) = pairing.verify_session(&session_token, Some(csrf_token), now)
            else {
                return;
            };
            if let Some(at) = pairing
                .session_expires_at_ms(&session_token)
                .and_then(|expires| deadline_after(expires.saturating_sub(now)))
            {
                session_deadline = Some((session_token.clone(), at));
            }
            device_id
        };
        if device_id != subscription.device_id {
            return;
        }
        let Some(challenge_id) = subscription.challenge_id.as_deref() else {
            return;
        };
        let Some(signature) = subscription.signature.as_deref() else {
            return;
        };
        let mut pairing = state.pairing.lock().await;
        if pairing
            .verify_fresh_challenge(challenge_id, &device_id, signature, now_ms())
            .is_err()
        {
            return;
        }
    }
    let mut receiver = state.events.subscribe();
    if subscription.host_epoch != state.host_epoch {
        send_resync(&mut socket, &state, ResyncReason::EpochChanged).await;
        return;
    }
    let mut last_sent = subscription.last_sequence;
    let mut last_acknowledged = subscription.last_sequence;
    let mut journal = state
        .journal_wake
        .subscribe(state.store.clone(), state.host_epoch.clone());
    // A batching client gets each wakeup's events as one JSON array. The first
    // events after a quiet period go out at once; later ones are held until
    // FRAME_INTERVAL has passed since the previous frame, so a streamed reply
    // reaches the phone in a few frames a second, not one frame per token.
    let batch = subscription.batch;
    let mut last_frame: Option<tokio::time::Instant> = None;
    let mut flush_at: Option<tokio::time::Instant> = None;
    let mut last_received = tokio::time::Instant::now();
    let mut probe_pending = true;
    loop {
        // Bound the entire step, including storage reads and socket writes: a
        // half-open peer must not keep the task stuck in a blocked event send.
        let deadline = last_received + keepalive.probe_after + keepalive.response_timeout;
        // timeout_at polls its inner future first. Ready wakeups or invalid
        // frames must not repeatedly win over an already elapsed deadline.
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        let step = async {
            let flush = async {
                match flush_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let expiry = async {
                match &session_deadline {
                    Some((_, at)) => tokio::time::sleep_until(*at).await,
                    None => std::future::pending().await,
                }
            };
            let ready = || !batch || last_frame.is_none_or(|sent| sent.elapsed() >= FRAME_INTERVAL);
            tokio::select! {
                _ = tokio::time::sleep_until(last_received + keepalive.probe_after), if probe_pending => {
                    probe_pending = false;
                    // No user data or durable state changes are needed for liveness.
                    if socket.send(Message::Ping(Default::default())).await.is_err() {
                        return false;
                    }
                },
                _ = expiry => {
                    // Recheck against the pairing state rather than trusting the timer alone.
                    let Some((token, _)) = session_deadline.take() else { return true };
                    let pairing = state.pairing.lock().await;
                    let now = now_ms();
                    if pairing.verify_session(&token, None, now).is_err() {
                        return false;
                    }
                    session_deadline = pairing
                        .session_expires_at_ms(&token)
                        .and_then(|expires| deadline_after(expires.saturating_sub(now).max(1)))
                        .map(|at| (token, at));
                },
                _ = flush => {
                    flush_at = None;
                    match send_committed_events(&mut socket, &state, &mut last_sent, compact, batch).await {
                        Some(true) => (last_frame, flush_at) = sent_frame(batch),
                        Some(false) => {}
                        None => return false,
                    }
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ack) = serde_json::from_str::<EventAcknowledgement>(&text) {
                            if ack.message_type == "ack"
                                && ack.host_epoch == state.host_epoch
                                && ack.sequence >= last_acknowledged
                                && ack.sequence <= last_sent
                            {
                                last_acknowledged = ack.sequence;
                                last_received = tokio::time::Instant::now();
                                probe_pending = true;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        // Axum/tungstenite queues the Pong and flushes it on the
                        // next read/write. Keep polling rather than replying twice.
                        last_received = tokio::time::Instant::now();
                        probe_pending = true;
                    }
                    Some(Ok(Message::Close(_))) | None => return false,
                    Some(Ok(_)) | Some(Err(_)) => return false,
                },
                changed = journal.changed() => {
                    if changed.is_err() { return false; }
                    if !ready() {
                        flush_at.get_or_insert(last_frame.map_or_else(tokio::time::Instant::now, |sent| sent + FRAME_INTERVAL));
                        return true;
                    }
                    match send_committed_events(&mut socket, &state, &mut last_sent, compact, batch).await {
                        Some(true) => (last_frame, flush_at) = sent_frame(batch),
                        Some(false) => {}
                        None => return false,
                    }
                },
                wakeup = receiver.recv() => {
                    if matches!(wakeup, Err(tokio::sync::broadcast::error::RecvError::Closed)) { return false; }
                    // Broadcast order and lag do not determine delivery order.
                    if !ready() {
                        flush_at.get_or_insert(last_frame.map_or_else(tokio::time::Instant::now, |sent| sent + FRAME_INTERVAL));
                        return true;
                    }
                    match send_committed_events(&mut socket, &state, &mut last_sent, compact, batch).await {
                        Some(true) => (last_frame, flush_at) = sent_frame(batch),
                        Some(false) => {}
                        None => return false,
                    }
                },
                revoked = revocations.recv() => match revoked {
                    Ok(device_id) if device_id == subscription.device_id => return false,
                    Ok(_) => {},
                    Err(_) => return false,
                },
            }
            true
        };
        if !tokio::time::timeout_at(deadline, step)
            .await
            .unwrap_or(false)
        {
            return;
        }
    }
}

/// `None` for a deadline too far away to represent ("never" expires).
fn deadline_after(remaining_ms: u64) -> Option<tokio::time::Instant> {
    tokio::time::Instant::now().checked_add(std::time::Duration::from_millis(remaining_ms))
}

/// Minimum spacing between frames for a batching client (see `stream_events`).
pub(crate) const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// `(last_frame, flush_at)` after a send that delivered events. One read
/// returns at most one replay batch, and the shared journal watcher wakes only
/// on new commits, so keep reading until a read comes back empty: at once for a
/// per-event client, at the next frame slot for a batching one.
fn sent_frame(batch: bool) -> (Option<tokio::time::Instant>, Option<tokio::time::Instant>) {
    let now = tokio::time::Instant::now();
    let next = if batch { now + FRAME_INTERVAL } else { now };
    (Some(now), Some(next))
}

/// Sends the committed events after `last_sent`: one frame per event, or one
/// JSON array frame for a batching client. `Some(sent anything)`, or `None`
/// once the stream must end.
async fn send_committed_events(
    socket: &mut WebSocket,
    state: &AppState,
    last_sent: &mut u64,
    compact: bool,
    batch: bool,
) -> Option<bool> {
    let mut events = match state
        .store
        .replay_batch(&state.host_epoch, *last_sent)
        .await
    {
        Ok(wonder_store::ReplayBatch::Events(events)) => events,
        Ok(wonder_store::ReplayBatch::Resync(reason)) => {
            send_resync(socket, state, reason).await;
            return None;
        }
        Err(_) => {
            send_resync(socket, state, ResyncReason::ReplayWindowExpired).await;
            return None;
        }
    };
    if events.is_empty() {
        return Some(false);
    }
    if compact {
        events.iter_mut().for_each(history::compact_event);
    }
    history::hide_thinking_events(state, &mut events).await;
    if batch {
        let payload = serde_json::to_string(&events).ok()?;
        // Include array framing and generated projection notices in the actual
        // wire budget too; their bytes are not all stored as journal payloads.
        if payload.len() > wonder_api::EVENT_FRAME_MAX_BYTES {
            send_resync(socket, state, ResyncReason::ReplayWindowExpired).await;
            return None;
        }
        socket.send(Message::Text(payload.into())).await.ok()?;
        *last_sent = events.last()?.sequence;
        return Some(true);
    }
    for event in events {
        let payload = serde_json::to_string(&event).ok()?;
        if payload.len() > wonder_api::EVENT_FRAME_MAX_BYTES {
            send_resync(socket, state, ResyncReason::ReplayWindowExpired).await;
            return None;
        }
        socket.send(Message::Text(payload.into())).await.ok()?;
        *last_sent = event.sequence;
    }
    Some(true)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventSubscription {
    device_id: String,
    host_epoch: String,
    last_sequence: u64,
    /// Receive events as JSON array frames, coalesced (see `stream_events`).
    #[serde(default)]
    batch: bool,
    csrf_token: Option<String>,
    challenge_id: Option<String>,
    signature: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventAcknowledgement {
    #[serde(rename = "type")]
    message_type: String,
    host_epoch: String,
    sequence: u64,
}

async fn send_resync(socket: &mut WebSocket, state: &AppState, reason: ResyncReason) {
    let envelope = HostEventEnvelope {
        event_id: uuid::Uuid::new_v4().to_string(),
        host_epoch: state.host_epoch.clone(),
        sequence: match state.store.committed_sequence(&state.host_epoch).await {
            Ok(sequence) => sequence,
            Err(_) => return,
        },
        occurred_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| state.started_at.clone()),
        request_id: None,
        device_id: None,
        conversation_id: None,
        message_id: None,
        thread_id: None,
        turn_id: None,
        item_id: None,
        approval_id: None,
        event: WonderEvent::ResyncRequired { reason },
    };
    let _ = socket
        .send(Message::Text(
            serde_json::to_string(&envelope).unwrap_or_default().into(),
        ))
        .await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        tungstenite::{client::IntoClientRequest, Message as ClientMessage},
        MaybeTlsStream, WebSocketStream,
    };
    type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    #[tokio::test]
    async fn journal_wakeup_is_shared_and_retires_with_its_last_reader() {
        use std::sync::atomic::Ordering;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let began = std::time::Instant::now();
        let readers: Vec<_> = (0..20)
            .map(|_| {
                state
                    .journal_wake
                    .subscribe(state.store.clone(), state.host_epoch.clone())
            })
            .collect();
        tokio::time::sleep(std::time::Duration::from_millis(510)).await;
        let elapsed = began.elapsed();
        let polls = state.journal_wake.polls.load(Ordering::Relaxed);
        assert!(polls > 0);
        assert!(
            polls <= elapsed.as_millis() as usize / 100 + 3,
            "twenty readers must share one polling cadence: {polls} in {elapsed:?}"
        );
        eprintln!("JOURNAL_WATCH readers=20 elapsed_ms={} shared_queries={polls} old_idle_queries_per_second=200",elapsed.as_millis());
        drop(readers);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while state.journal_wake.inner.lock().unwrap().upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let polls = state.journal_wake.polls.load(Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(
            state.journal_wake.polls.load(Ordering::Relaxed),
            polls,
            "no watcher remains without clients"
        );
        // Reopening after retirement starts a fresh live receiver.
        let mut next = state
            .journal_wake
            .subscribe(state.store.clone(), state.host_epoch.clone());
        assert!(next.changed().await.is_ok());
    }

    // B-425: a host whose journal stopped changing polls it twice a second,
    // not ten times; a change brings the quick cadence back at once.
    #[tokio::test]
    async fn a_quiet_journal_is_polled_less_often_until_it_changes() {
        use std::sync::atomic::Ordering;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let mut reader = state
            .journal_wake
            .subscribe(state.store.clone(), state.host_epoch.clone());
        tokio::time::sleep(JOURNAL_ACTIVE + std::time::Duration::from_millis(300)).await;
        let polls = || state.journal_wake.polls.load(Ordering::Relaxed);
        let before = polls();
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let quiet = polls() - before;
        assert!(quiet <= 6, "{quiet} polls in 2 s while quiet");
        reader.borrow_and_update();
        publish_ready(&state).await;
        // A hang guard only.
        tokio::time::timeout(std::time::Duration::from_secs(10), reader.changed())
            .await
            .unwrap()
            .unwrap();
        // Over the next second: about ten looks, against two while quiet.
        let before = polls();
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        let active = polls() - before;
        assert!(
            active >= 5,
            "quick again after a change: {active} polls in 1 s"
        );
    }

    #[tokio::test]
    async fn checkpoint_requires_auth_and_reads_committed_head() {
        use tower::ServiceExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let unauthenticated = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/checkpoint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::FORBIDDEN);
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
        let _runtime_busy = state.projects.codex.lock().await;
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/checkpoint")
                    .header("x-wonder-loopback-capability", "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 10000)
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        crate::tests::validate_http_contract("readCheckpoint", &result);
        assert_eq!(result["hostEpoch"], "epoch");
        assert_eq!(
            result["lastSequence"],
            state.store.committed_sequence("epoch").await.unwrap()
        );
        assert_eq!(result["hostInstallationId"], state.host_installation_id);
    }

    async fn connect(
        state: &AppState,
        epoch: &str,
        cursor: u64,
    ) -> (Client, tokio::task::JoinHandle<()>) {
        connect_with(state, epoch, cursor, false).await
    }

    async fn connect_with(
        state: &AppState,
        epoch: &str,
        cursor: u64,
        batch: bool,
    ) -> (Client, tokio::task::JoinHandle<()>) {
        connect_app(router(state.clone()), epoch, cursor, batch).await
    }

    async fn connect_app(
        app: Router,
        epoch: &str,
        cursor: u64,
        batch: bool,
    ) -> (Client, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut request = format!("ws://{addr}/api/v1/events")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("x-wonder-loopback-capability", "test".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket.send(ClientMessage::Text(serde_json::json!({"deviceId":"test-device", "hostEpoch":epoch, "lastSequence":cursor, "batch":batch}).to_string().into())).await.unwrap();
        (socket, server)
    }

    // Exercise the same stream over TCP with a bounded configuration, without
    // changing production timing or the other tests' socket configuration.
    // Probes come quickly, but the grace period is long enough that a test
    // process stalled on a loaded machine does not miss it: while other test
    // binaries compile and link, steps of 0.7 s were measured and the stress
    // failures needed stalls over 2 s. Every assertion below allows for that.
    const TEST_KEEPALIVE: Keepalive = Keepalive {
        probe_after: std::time::Duration::from_millis(100),
        response_timeout: std::time::Duration::from_secs(5),
    };

    async fn connect_keepalive(state: &AppState) -> (Client, tokio::task::JoinHandle<()>) {
        let state = state.clone();
        let app = Router::new().route(
            "/api/v1/events",
            get(move |upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(move |socket| {
                    stream_events_with_keepalive(socket, state, None, true, false, TEST_KEEPALIVE)
                })
            }),
        );
        connect_app(app, "epoch", 0, false).await
    }

    async fn receive_message(socket: &mut Client) -> ClientMessage {
        tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("socket did not respond")
            .expect("socket closed")
            .expect("socket failed")
    }

    async fn publish_ready(state: &AppState) -> HostEventEnvelope {
        publish_event(
            state,
            WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap()
    }

    /// The next event. A keepalive probe may arrive first whenever the test is
    /// slower than `probe_after`; reading it queues tungstenite's automatic
    /// Pong, as a real client would. One deadline covers the whole wait, so a
    /// missing event fails even while probes keep arriving; it is generous
    /// because a loaded machine can stall the test task for seconds.
    async fn receive(socket: &mut Client) -> HostEventEnvelope {
        let event = async {
            loop {
                match socket
                    .next()
                    .await
                    .expect("socket closed")
                    .expect("socket failed")
                {
                    ClientMessage::Ping(_) | ClientMessage::Pong(_) => {}
                    ClientMessage::Text(text) => return serde_json::from_str(&text).unwrap(),
                    message => panic!("expected an event, got {message:?}"),
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), event)
            .await
            .expect("no event arrived")
    }

    // The event socket owns subscription lifetime; tungstenite owns the Pong
    // reply. A control frame must not prevent the next committed event arriving.
    #[tokio::test]
    async fn event_socket_answers_ping_and_keeps_streaming() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (mut client, server) = connect(&state, "epoch", 0).await;
        let first = publish_event(
            &state,
            WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receive(&mut client).await, first);
        let payload = b"control-probe".to_vec();
        client
            .send(ClientMessage::Ping(payload.clone().into()))
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("Ping must receive a Pong")
            .expect("Ping must not close the stream")
            .expect("Ping must not reset the stream");
        assert_eq!(reply, ClientMessage::Pong(payload.into()));
        // An unsolicited Pong is valid too, and must not end the subscription.
        client
            .send(ClientMessage::Pong(Vec::new().into()))
            .await
            .unwrap();
        let second = publish_event(
            &state,
            WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receive(&mut client).await, second);
        server.abort();
    }

    #[tokio::test]
    async fn event_socket_times_out_a_client_that_does_not_answer_its_probe() {
        use tokio::io::AsyncReadExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (mut client, server) = connect_keepalive(&state).await;
        let first = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, first);
        let started = std::time::Instant::now();
        assert_eq!(
            receive_message(&mut client).await,
            ClientMessage::Ping(Default::default())
        );
        let probed = std::time::Instant::now();
        // Reading through tungstenite would automatically answer the Ping.
        // Read TCP directly so this client stays connected but never responds.
        let mut byte = [0];
        assert!(
            tokio::time::timeout(
                TEST_KEEPALIVE.response_timeout / 2,
                client.get_mut().read(&mut byte),
            )
            .await
            .is_err(),
            "the client did not get its response grace period"
        );
        let read = tokio::time::timeout(
            TEST_KEEPALIVE.response_timeout,
            client.get_mut().read(&mut byte),
        )
        .await
        .expect("a half-open socket exceeded the keepalive deadline")
        .unwrap();
        assert_eq!(
            read, 0,
            "the server must close rather than send more probes"
        );
        assert!(probed.elapsed() < TEST_KEEPALIVE.response_timeout * 3 / 2);
        assert_eq!(
            state.store.committed_sequence("epoch").await.unwrap(),
            first.sequence
        );
        eprintln!("half-open socket: probe after {:?}, closed after {:?}; configured {:?} idle + {:?} grace", probed.duration_since(started), started.elapsed(), TEST_KEEPALIVE.probe_after, TEST_KEEPALIVE.response_timeout);
        server.abort();
    }

    #[tokio::test]
    async fn event_socket_pongs_keep_an_idle_subscription_alive() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (mut client, server) = connect_keepalive(&state).await;
        let first = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, first);
        let started = std::time::Instant::now();
        let mut probes = 0;
        // Outlast a whole deadline with Pongs as the only inbound activity.
        while started.elapsed() <= TEST_KEEPALIVE.probe_after + TEST_KEEPALIVE.response_timeout {
            assert_eq!(
                receive_message(&mut client).await,
                ClientMessage::Ping(Default::default())
            );
            // Flush the library's automatic Pong, just as a real client does.
            client.flush().await.unwrap();
            probes += 1;
        }
        assert_eq!(
            state.store.committed_sequence("epoch").await.unwrap(),
            first.sequence
        );
        let next = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, next);
        eprintln!(
            "idle Pong client remained subscribed for {:?} across {probes} probes",
            started.elapsed()
        );
        server.abort();
    }

    #[tokio::test]
    async fn event_socket_valid_acknowledgements_keep_the_subscription_alive() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (mut client, server) = connect_keepalive(&state).await;
        let first = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, first);
        let ack = serde_json::json!({"type":"ack", "hostEpoch":"epoch", "sequence":first.sequence})
            .to_string();
        let started = std::time::Instant::now();
        // Do not read during this interval: no automatic Pongs can mask a
        // failure to recognize valid acknowledgements as inbound activity.
        // The only Pong, flushed by the first send, is at least half a deadline
        // stale by the end, so it cannot be what keeps the subscription open
        // even if the server processed it late.
        let deadline = TEST_KEEPALIVE.probe_after + TEST_KEEPALIVE.response_timeout;
        while started.elapsed() <= deadline * 3 / 2 {
            tokio::time::sleep(TEST_KEEPALIVE.probe_after).await;
            client
                .send(ClientMessage::Text(ack.clone().into()))
                .await
                .unwrap();
        }
        assert_eq!(
            state.store.committed_sequence("epoch").await.unwrap(),
            first.sequence
        );
        let next = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, next);
        eprintln!(
            "valid acknowledgement client remained subscribed for {:?} without reading probes",
            started.elapsed()
        );
        server.abort();
    }

    #[tokio::test]
    async fn event_socket_invalid_acks_and_outbound_events_do_not_extend_the_deadline() {
        use tokio::io::AsyncReadExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (mut client, server) = connect_keepalive(&state).await;
        let first = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, first);
        let second = publish_ready(&state).await;
        assert_eq!(receive(&mut client).await, second);
        let deadline = TEST_KEEPALIVE.probe_after + TEST_KEEPALIVE.response_timeout;
        // Taken before the last valid inbound frame leaves the client, so the
        // server cannot close before `started + deadline`.
        let started = tokio::time::Instant::now();
        client
            .send(ClientMessage::Text(
                serde_json::json!({
                    "type":"ack", "hostEpoch":"epoch", "sequence":second.sequence,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let invalid = [
            "not json".to_owned(),
            serde_json::json!({"type":"other", "hostEpoch":"epoch", "sequence":second.sequence})
                .to_string(),
            serde_json::json!({"type":"ack", "hostEpoch":"other", "sequence":second.sequence})
                .to_string(),
            serde_json::json!({"type":"ack", "hostEpoch":"epoch", "sequence":first.sequence})
                .to_string(),
            serde_json::json!({"type":"ack", "hostEpoch":"epoch", "sequence":u64::MAX}).to_string(),
        ];
        // Keep sending invalid acknowledgements and publishing events more
        // often than the deadline until the server closes. If either counted as
        // liveness the socket would stay open, so the bound only has to allow
        // for a stalled test, not measure the deadline precisely.
        let mut published = 0;
        let unread_frames = tokio::time::timeout_at(started + deadline * 3, async {
            let mut unread_frames = Vec::new();
            for text in invalid.iter().cycle() {
                // A write fails once the server has closed the connection.
                if client
                    .send(ClientMessage::Text(text.clone().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                publish_ready(&state).await;
                published += 1;
                tokio::time::sleep(TEST_KEEPALIVE.probe_after / 2).await;
                // Read the server's events and probes from TCP directly, so
                // tungstenite never sends a Pong.
                let mut buffer = [0; 4096];
                match tokio::time::timeout(
                    TEST_KEEPALIVE.probe_after,
                    client.get_mut().read(&mut buffer),
                )
                .await
                {
                    Ok(Ok(0) | Err(_)) => break,
                    Ok(Ok(read)) => unread_frames.extend_from_slice(&buffer[..read]),
                    Err(_) => {}
                }
            }
            unread_frames
        })
        .await
        .expect("invalid acknowledgements or outbound events extended the deadline");
        assert!(!unread_frames.is_empty());
        assert!(started.elapsed() >= deadline);
        assert_eq!(
            state.store.committed_sequence("epoch").await.unwrap(),
            second.sequence + published
        );
        eprintln!("invalid acknowledgements and outbound events: closed after {:?} and {published} events; configured deadline {:?}", started.elapsed(), deadline);
        server.abort();
    }

    // A paired phone's socket lasts only as long as its session: HTTP stops
    // accepting an expired session, and so does an already open stream.
    #[tokio::test]
    async fn event_socket_closes_when_its_session_expires() {
        use p256::ecdsa::{signature::Signer, Signature, SigningKey};
        use sha2::Digest;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let key = SigningKey::from_slice(&[13_u8; 32]).unwrap();
        let point = key.verifying_key().to_sec1_point(false);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let challenge = {
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
                now_ms() + 1_500,
            );
            pairing
                .issue_fresh_challenge(
                    "phone",
                    "https://wonder.test",
                    &state.host_installation_id,
                    now_ms(),
                )
                .unwrap()
        };
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state.clone());
        let _server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut request = format!("ws://{addr}/api/v1/events")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "cookie",
            "__Host-wonder_session=session-token".parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("origin", state.public_origin.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket.send(ClientMessage::Text(serde_json::json!({"deviceId":"phone", "hostEpoch":"epoch", "lastSequence":0,
            "csrfToken":"csrf-token", "challengeId":challenge.challenge_id, "signature":b64.encode(signature.to_bytes())}).to_string().into())).await.unwrap();
        // The stream is authorized: a committed event arrives.
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
        assert_eq!(receive(&mut socket).await.sequence, 1);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                match socket.next().await {
                    None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "the socket outlived its session");
    }

    #[tokio::test]
    async fn http_snapshot_uses_committed_cursor_without_waiting_for_runtime_history() {
        use tower::ServiceExt;
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
        let _runtime_busy = state.projects.codex.lock().await;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            router(state.clone()).oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9")
                    .header("x-wonder-loopback-capability", "test")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("snapshot must not wait for App Server")
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        crate::tests::validate_http_contract("conversationSnapshot", &snapshot);
        assert_eq!(snapshot["hostEpoch"], "epoch");
        assert_eq!(
            snapshot["lastSequence"],
            state.store.committed_sequence("epoch").await.unwrap()
        );
        assert_eq!(snapshot["codexThreadId"], "thread");
    }

    // Contract: live and replayed thinking upserts carry their summary only
    // while the conversation's provider is set to Thinking; the lifecycle
    // always arrives, and an event outside any conversation never shows it.
    #[tokio::test]
    async fn event_socket_sends_thinking_text_only_at_the_thinking_level() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let chat = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
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
        let publish = |conversation: Option<&str>| {
            let detail = crate::thread_item_upsert_detail_with_authority(
                "turn",
                &serde_json::json!({"id":"rs_1", "type":"reasoning", "status":"completed",
                    "summary":["Reading the parser"]}),
                "completed",
                2,
                None,
            );
            crate::publish_event_with_context(
                &state,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail,
                },
                crate::EventContext {
                    conversation_id: conversation.map(str::to_owned),
                    thread_id: Some("thread".into()),
                    turn_id: Some("turn".into()),
                    item_id: Some("rs_1".into()),
                    ..Default::default()
                },
            )
        };
        let thinking = |event: HostEventEnvelope| match event.event {
            WonderEvent::Activity {
                category,
                detail: Some(detail),
                ..
            } if category == "thread_item_upsert" => {
                Some(serde_json::from_str::<serde_json::Value>(&detail).unwrap())
            }
            _ => None,
        };
        async fn next_thinking(
            socket: &mut Client,
            thinking: impl Fn(HostEventEnvelope) -> Option<serde_json::Value>,
        ) -> serde_json::Value {
            loop {
                if let Some(detail) = thinking(receive(socket).await) {
                    return detail;
                }
            }
        }
        let (mut client, server) = connect(&state, "epoch", 0).await;
        publish(Some(chat)).await.unwrap();
        let hidden = next_thinking(&mut client, thinking).await;
        assert_eq!(hidden["state"], "completed");
        assert!(hidden["item"].get("summary").is_none() && hidden.get("text").is_none());

        state
            .store
            .set_agent_chat_detail(
                wonder_store::AgentFamily::Codex,
                wonder_store::ChatDetail::Thinking,
                "now",
            )
            .await
            .unwrap();
        publish(Some(chat)).await.unwrap();
        let shown = next_thinking(&mut client, thinking).await;
        assert_eq!(
            shown["item"]["summary"],
            serde_json::json!(["Reading the parser"])
        );
        assert_eq!(shown["text"], "Reading the parser");
        publish(None).await.unwrap();
        assert!(next_thinking(&mut client, thinking)
            .await
            .get("text")
            .is_none());
        // A replay follows the current setting, not the one at capture.
        let (mut replay, replay_server) = connect(&state, "epoch", 0).await;
        assert_eq!(
            next_thinking(&mut replay, thinking).await["text"],
            "Reading the parser"
        );
        server.abort();
        replay_server.abort();
    }

    #[tokio::test]
    async fn websocket_reads_durable_order_despite_missing_and_reversed_wakeups() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (mut client, server) = connect(&state, "epoch", 0).await;
        // Projection writes don't send broadcasts. The durable journal still streams them.
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
        let first = receive(&mut client).await;
        assert_eq!(first.sequence, 1);
        assert!(
            matches!(first.event, WonderEvent::Activity { ref category, .. } if category == "projection_changed")
        );
        let second = publish_event(
            &state,
            WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receive(&mut client).await.sequence, second.sequence);
        let _ = state.events.send(second.clone());
        let _ = state.events.send(first);
        let third = publish_event(
            &state,
            WonderEvent::HostStatus {
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receive(&mut client).await.sequence, third.sequence);
        // Reconnect after 1 replays 2,3 exactly once, irrespective of wakeups.
        let (mut reconnected, replay_server) = connect(&state, "epoch", 1).await;
        assert_eq!(receive(&mut reconnected).await.sequence, second.sequence);
        assert_eq!(receive(&mut reconnected).await.sequence, third.sequence);
        let _ = state.revocations.send("test-device".into());
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), reconnected.next())
            .await
            .unwrap();
        assert!(
            closed.is_none() || matches!(closed, Some(Ok(ClientMessage::Close(_))) | Some(Err(_)))
        );
        server.abort();
        replay_server.abort();
    }

    /// A reconnect after more than one replay batch (256 rows) must drain the
    /// whole backlog without waiting for a new commit to wake the stream.
    #[tokio::test]
    async fn backlog_over_one_replay_batch_drains_on_an_idle_host() {
        for batch in [false, true] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            let head = state.store.committed_sequence("epoch").await.unwrap();
            // Another subscriber keeps the shared watcher alive, as on a real host.
            let _other = state
                .journal_wake
                .subscribe(state.store.clone(), state.host_epoch.clone());
            for _ in 0..300 {
                publish_ready(&state).await;
            }
            // Let the watcher see the final cursor, so no further commit wakes
            // the new socket: only its own replay may drain the backlog.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let last = state.store.committed_sequence("epoch").await.unwrap();
            let (mut client, server) = connect_with(&state, "epoch", head, batch).await;
            let mut received = head;
            while received < last {
                let message = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        match client.next().await.expect("closed").expect("failed") {
                            ClientMessage::Text(text) => return text,
                            ClientMessage::Ping(_) | ClientMessage::Pong(_) => {}
                            other => panic!("{other:?}"),
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("batch={batch}: stalled at {received} of {last}"));
                let events: Vec<HostEventEnvelope> = if batch {
                    serde_json::from_str(&message).unwrap()
                } else {
                    vec![serde_json::from_str(&message).unwrap()]
                };
                for event in events {
                    assert_eq!(event.sequence, received + 1);
                    received = event.sequence;
                }
            }
            server.abort();
        }
    }

    async fn receive_frame(socket: &mut Client) -> Vec<HostEventEnvelope> {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn oversized_replay_resyncs_before_exceeding_the_phone_receive_limit() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let head = state.store.committed_sequence("epoch").await.unwrap();
        for _ in 0..2 {
            publish_event(
                &state,
                WonderEvent::AssistantDelta {
                    text: "x".repeat(600 * 1024),
                },
            )
            .await
            .unwrap();
        }
        let (mut client, server) = connect_with(&state, "epoch", head, true).await;
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let bytes = message.to_text().unwrap();
        assert!(
            bytes.len() <= 1_048_576,
            "replay frame was {} bytes",
            bytes.len()
        );
        let event: HostEventEnvelope = serde_json::from_str(bytes).unwrap();
        assert!(matches!(event.event, WonderEvent::ResyncRequired { .. }));
        server.abort();

        // The client installs the checkpoint and reconnects after it. New work
        // must arrive normally instead of repeating the oversized backlog.
        let latest = state.store.committed_sequence("epoch").await.unwrap();
        let (mut recovered, recovery_server) = connect_with(&state, "epoch", latest, true).await;
        let next = publish_event(
            &state,
            WonderEvent::AssistantDelta {
                text: "new reply".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receive_frame(&mut recovered).await, vec![next]);
        recovery_server.abort();
    }

    #[tokio::test]
    async fn replay_budget_includes_array_framing() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let head = state.store.committed_sequence("epoch").await.unwrap();
        let mut event = event_envelope(
            &state,
            WonderEvent::AssistantDelta {
                text: String::new(),
            },
            EventContext::default(),
        );
        event.sequence = head + 1;
        let overhead = serde_json::to_vec(&event).unwrap().len();
        event.event = WonderEvent::AssistantDelta {
            text: "x".repeat(wonder_api::EVENT_FRAME_MAX_BYTES - overhead),
        };
        state.store.commit_event(&mut event).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&event).unwrap().len(),
            wonder_api::EVENT_FRAME_MAX_BYTES
        );
        assert!(matches!(
            state.store.replay_batch("epoch", head).await.unwrap(),
            wonder_store::ReplayBatch::Events(_)
        ));
        let (mut client, server) = connect_with(&state, "epoch", head, true).await;
        let event = receive(&mut client).await;
        assert!(matches!(event.event, WonderEvent::ResyncRequired { .. }));
        server.abort();
    }

    #[tokio::test]
    async fn batching_client_gets_the_first_event_at_once_then_coalesced_frames() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let head = state.store.committed_sequence("epoch").await.unwrap();
        let (mut client, server) = connect_with(&state, "epoch", head, true).await;
        let ready = || WonderEvent::HostStatus {
            state: "ready".into(),
        };
        // The server may send before publish_event returns. Measure from
        // before admission, rather than starting after its timer has begun.
        let started = std::time::Instant::now();
        let first = publish_event(&state, ready()).await.unwrap();
        let frame = receive_frame(&mut client).await;
        assert_eq!(
            frame.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [first.sequence]
        );
        // A burst right after a frame waits for the frame interval and then
        // arrives whole, in order, with no gap or repeat. That only holds when
        // the burst commits within one frame interval: all five events are then
        // committed before a second frame may go out, so at most two frames
        // carry them. A loaded machine can take longer to commit, so retry
        // until one burst is fast enough; every burst must still arrive in order.
        let mut all = Vec::new();
        let mut coalesced = None;
        for _ in 0..20 {
            let committing = std::time::Instant::now();
            let burst =
                futures_util::future::join_all((0..5).map(|_| publish_event(&state, ready())))
                    .await
                    .into_iter()
                    .map(|event| event.unwrap().sequence)
                    .collect::<Vec<_>>();
            let fast = committing.elapsed() < FRAME_INTERVAL;
            let mut sorted = burst.clone();
            sorted.sort_unstable();
            let mut received = Vec::new();
            let mut frames = 0;
            while received.len() < sorted.len() {
                received.extend(receive_frame(&mut client).await.iter().map(|e| e.sequence));
                frames += 1;
            }
            assert_eq!(received, sorted);
            all.extend(sorted);
            if fast {
                coalesced = Some(frames);
                break;
            }
        }
        // Ordering held for every burst above. Coalescing is only observable when a
        // burst commits within one frame interval; a host too loaded for that in
        // 20 tries (seen on shared land boxes) leaves it unobserved, not failed.
        match coalesced {
            Some(frames) => assert!(frames < 5, "the burst was not coalesced"),
            None => eprintln!("no burst committed within {FRAME_INTERVAL:?}; coalescing not observed on this host"),
        }
        assert!(started.elapsed() >= FRAME_INTERVAL);
        // Reconnecting after the first event replays the rest exactly once.
        let (mut replayed, replay_server) =
            connect_with(&state, "epoch", first.sequence, true).await;
        let mut replay = Vec::new();
        while replay.len() < all.len() {
            replay.extend(
                receive_frame(&mut replayed)
                    .await
                    .iter()
                    .map(|e| e.sequence),
            );
        }
        assert_eq!(replay, all);
        server.abort();
        replay_server.abort();
    }

    #[tokio::test]
    async fn websocket_resyncs_future_empty_epoch_and_interior_gaps_without_partial_replay() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-websocket-v1.json"
        ))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for (epoch, cursor) in [("old", 0), ("epoch", u64::MAX)] {
            let (mut client, server) = connect(&state, epoch, cursor).await;
            let event = receive(&mut client).await;
            assert!(matches!(event.event, WonderEvent::ResyncRequired { .. }));
            assert_eq!(event.sequence, 0);
            assert!(validator.is_valid(&serde_json::to_value(event).unwrap()));
            server.abort();
        }
        for _ in 0..3 {
            publish_event(
                &state,
                WonderEvent::HostStatus {
                    state: "ready".into(),
                },
            )
            .await
            .unwrap();
        }
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("DELETE FROM sync_journal WHERE sequence = 2")
            .execute(&pool)
            .await
            .unwrap();
        let (mut client, server) = connect(&state, "epoch", 0).await;
        let event = receive(&mut client).await;
        assert!(matches!(
            event.event,
            WonderEvent::ResyncRequired {
                reason: ResyncReason::ReplayWindowExpired
            }
        ));
        assert_eq!(event.sequence, 3);
        server.abort();
    }
}
