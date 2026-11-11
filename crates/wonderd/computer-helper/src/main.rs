//! Wonder computer helper: the existing JSON-lines protocol over a native H264/VP8 publisher.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod encoder;
mod pacing;
#[cfg(target_os = "linux")]
#[path = "platform/linux.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "platform/windows.rs"]
mod platform;
use pacing::{max_bitrate_bps, target_fps, SOFTWARE_MAX_FPS};
use platform::{capture_capability, discover_sources, display_size};
use std::{
    collections::BTreeSet,
    future::Future,
    io::Write,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use libwebrtc::{
    peer_connection_factory::native::PeerConnectionFactoryExt, prelude::*, stats::RtcStats,
    video_source::native::NativeVideoSource,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, Mutex},
};
const MAX_LINE_BYTES: usize = 128 * 1024;
/// Each quality's default frame rate; a request's `videoMaxFramesPerSecond` replaces it.
const QUALITY_FPS: [(&str, u32); 4] =
    [("standard", 15), ("medium", 30), ("auto", 30), ("high", 30)];

/// Epoch microseconds of the first captured frame handed to libwebrtc (0 = none yet).

fn epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn bench(value: Value) {
    #[cfg(feature = "parity-tests")]
    if std::env::var("WONDER_COMPUTER_STATS").as_deref() == Ok("1") {
        eprintln!("WONDER_COMPUTER_STATS {value}");
    }
    #[cfg(not(feature = "parity-tests"))]
    let _ = value;
}

fn trace(what: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "LOG {:.3} {what}", epoch());
}

fn quality_fps(quality: &str) -> Option<u32> {
    #[cfg(feature = "parity-tests")]
    if quality == "benchmark30" {
        return Some(30);
    }
    QUALITY_FPS
        .iter()
        .find(|(name, _)| *name == quality)
        .map(|(_, fps)| *fps)
}

/// The frame rate a prepare/setQuality request asks for; None when the
/// quality is unknown or the optional ceiling is not a number.
fn requested_fps(quality: &str, params: &Value) -> Option<u32> {
    let ceiling = match params.get("videoMaxFramesPerSecond") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64()?),
    };
    Some(target_fps(quality_fps(quality)?, ceiling))
}

/// Make the sender's frame-rate and bitrate ceilings follow the live quality.
fn apply_send_limits(pc: &PeerConnection, sender: &RtpSender, live: &Live) -> Result<(), String> {
    let fps = live.fps.load(Ordering::SeqCst);
    let max_bps = max_bitrate_bps(live.quality.load(Ordering::SeqCst), fps);
    let mut params = sender.parameters();
    for encoding in &mut params.encodings {
        encoding.max_framerate = Some(fps as f64);
        encoding.max_bitrate = Some(max_bps);
    }
    sender.set_parameters(params).map_err(|e| e.to_string())?;
    pc.set_bitrate(BitrateSettings {
        min_bitrate_bps: None,
        start_bitrate_bps: None,
        max_bitrate_bps: Some(max_bps),
    })
    .map_err(|e| e.to_string())
}

/// Fit without upscaling; physical geometry remains in source metadata.
fn encoded_size(width: u32, height: u32, quality: u32) -> (u32, u32) {
    let (max_w, max_h) = match quality {
        1 => (1920, 1080),
        2 => (2560, 1440),
        _ => (1280, 720),
    };
    let factor = (max_w as f64 / width as f64)
        .min(max_h as f64 / height as f64)
        .min(1.0);
    (
        ((width as f64 * factor) as u32 & !1).max(2),
        ((height as f64 * factor) as u32 & !1).max(2),
    )
}
fn quality_index(quality: &str) -> u32 {
    match quality {
        "medium" => 1,
        "auto" | "high" => 2,
        _ => 0,
    }
}

/// Limit queued bytes as well as message count, allowing normal ICE bursts.
#[derive(Clone)]
struct Out {
    tx: mpsc::Sender<(Value, usize)>,
    bytes: Arc<AtomicUsize>,
    failed: Arc<tokio::sync::Notify>,
}
impl Out {
    fn send(&self, value: Value) -> Result<(), ()> {
        let size = value.to_string().len() + 1;
        if size > MAX_LINE_BYTES {
            self.failed.notify_one();
            return Err(());
        }
        let previous = self.bytes.fetch_add(size, Ordering::SeqCst);
        if previous + size > 1024 * 1024 || self.tx.try_send((value, size)).is_err() {
            self.bytes.fetch_sub(size, Ordering::SeqCst);
            self.failed.notify_one();
            return Err(());
        }
        Ok(())
    }
}

/// A small always-on-top window on the computer's own screen: a message and
/// its buttons. `choice` receives the index of the button the person
/// pressed; closing the window counts as the last button. Dropping `window`
/// closes it without an answer, which `choice` reports as closed.
struct Panel {
    choice: tokio::sync::oneshot::Receiver<usize>,
    window: Box<dyn Send>,
}

/// Shows a panel, or `None` when the computer cannot show one.
type ShowPanel = dyn Fn(&'static str, &'static [&'static str]) -> Option<Panel> + Send + Sync;

/// Whether the person at the computer approves each Take control.
enum LocalApproval {
    /// Linux shares its own virtual screen, so a paired owner's request is
    /// granted after the identity checks.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    NotNeeded,
    /// Windows controls the signed-in desktop: ask on the screen for every
    /// request, then keep a "being controlled" notice with Stop on screen.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Ask(Arc<ShowPanel>),
}

#[cfg(target_os = "windows")]
fn local_approval() -> LocalApproval {
    LocalApproval::Ask(Arc::new(platform::show_panel))
}
#[cfg(not(target_os = "windows"))]
fn local_approval() -> LocalApproval {
    LocalApproval::NotNeeded
}

const ASK_TEXT: &str = "Your paired iPhone or iPad wants to control this computer: it would use your mouse and keyboard. Allow it this time?";
const ASK_BUTTONS: &[&str] = &["Allow", "Don\u{2019}t Allow"];
const CONTROLLED_TEXT: &str =
    "Your iPhone or iPad is controlling this computer. Press Stop to take it back.";
const CONTROLLED_BUTTONS: &[&str] = &["Stop"];
/// Unanswered prompts close before wonderd's two-minute consent wait ends.
const ASK_SECONDS: u64 = 110;

/// A panel the helper shows. Dropping it closes the window and wakes
/// whatever waits on its answer, so ending control never waits on the window.
struct OnScreen {
    token: u64,
    _window: Box<dyn Send>,
    _gone: tokio::sync::oneshot::Sender<()>,
}

impl OnScreen {
    fn new(st: &mut State, window: Box<dyn Send>) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        st.panel_token += 1;
        let (gone, closed) = tokio::sync::oneshot::channel();
        let shown = Self {
            token: st.panel_token,
            _window: window,
            _gone: gone,
        };
        (shown, closed)
    }
}

/// The on-screen question for one request.
struct Prompt {
    identity: Value,
    shown: OnScreen,
}

/// A request's answer: now, or once the person at the computer decides.
enum Reply {
    Now(Value),
    Later(Pin<Box<dyn Future<Output = Value> + Send>>),
}

fn granted(replayed: bool) -> Value {
    json!({"granted": true, "decision": "allowOnce", "replayed": replayed})
}
fn denied(reason: &str) -> Value {
    json!({"granted": false, "decision": "deny", "replayed": false, "reason": reason})
}

struct Peer {
    pc: PeerConnection,
    sender: RtpSender,
    source: NativeVideoSource,
    revision: String,
    stats: tokio::task::JoinHandle<()>,
    encoder: Option<(String, String, bool)>,
}

/// Frame counters shared with the capture thread.
#[derive(Default)]
struct Live {
    sequence: AtomicU64,
    frame_gate: std::sync::Mutex<()>,
    last_frame: std::sync::Mutex<Option<Value>>,
    fps: AtomicU32,
    desired_fps: AtomicU32,
    quality: AtomicU32,
    metadata_at: std::sync::Mutex<Option<Instant>>,
    publish_pending: AtomicBool,
}

struct State {
    session_id: Option<String>,
    generation: i64,
    state: &'static str,
    reason: Option<String>,
    message: Option<&'static str>,
    quality: String,
    viewer_count: i64,
    /// The selected source as reported to wonderd (id, kind, title, size, contentRect).
    source: Option<Value>,
    geometry_revision: u64,
    peer: Option<Peer>,
    capture_stop: Option<Arc<AtomicBool>>,
    live: Arc<Live>,
    /// Events written after the current request's response (the Python
    /// helper schedules capture.sourceSelected the same way).
    deferred: Vec<Value>,
    /// Set by capture.prepare: the offer is made after its response, as in
    /// the Python helper's peer task.
    start_peer: bool,
    /// The revision of the peer being set up, so a failure or timeout can
    /// name the peer the host is waiting on.
    pending_peer: Option<String>,
    // ---- control lease
    lease: Option<Value>,
    lease_expires: Instant,
    consents: Vec<(String, Value)>,
    /// The question on screen for a request that has no answer yet.
    prompt: Option<Prompt>,
    /// The "being controlled" notice with Stop, shown while a lease is active.
    indicator: Option<OnScreen>,
    panel_token: u64,
    payloads: Vec<(i64, String)>,
    last_input_sequence: i64,
}

/// Input owns its held state independently of capture/control admission.
struct InputState {
    source: Option<Value>,
    held_buttons: BTreeSet<String>,
    held_keys: BTreeSet<String>,
    execution: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    deadline: Option<Instant>,
    #[cfg(all(test, target_os = "linux"))]
    command: Option<String>,
}
#[derive(Clone)]
struct InputPermission {
    execution: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    deadline: Option<Instant>,
}
impl InputPermission {
    fn check(&self) -> Result<(), String> {
        if *self.execution.borrow() != self.generation {
            Err("lease_not_active".into())
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Err("input_timed_out".into())
        } else {
            Ok(())
        }
    }
}
impl InputState {
    fn permission(&self) -> InputPermission {
        InputPermission {
            execution: self.execution.clone(),
            generation: self.generation,
            deadline: self.deadline,
        }
    }
    fn check(&self) -> Result<(), String> {
        self.permission().check()
    }
}

struct Helper {
    factory: PeerConnectionFactory,
    out: Out,
    instance: String,
    local: LocalApproval,
    state: Mutex<State>,
    input: Mutex<InputState>,
    batches: Mutex<()>,
    execution: tokio::sync::watch::Sender<u64>,
}

fn revision() -> String {
    // Unique enough for one helper process; wonderd only compares it.
    format!(
        "{:032X}",
        (epoch() * 1e9) as u128 ^ (std::process::id() as u128) << 64
    )
}

fn instance_id() -> String {
    let t = (epoch() * 1e9) as u128 ^ (std::process::id() as u128) << 80;
    let h = format!("{t:032x}");
    format!(
        "{}-{}-4{}-a{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

fn param_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

impl Helper {
    fn signal(&self, st: &State, event: &str, revision: &str, fields: Value) {
        trace(&format!("event {event}"));
        let mut v = json!({"event": event, "sessionID": st.session_id,
                           "generation": st.generation, "peerRevision": revision});
        if let (Some(obj), Value::Object(extra)) = (v.as_object_mut(), fields) {
            obj.extend(extra);
        }
        let _ = self.out.send(v);
    }

    fn configuration(&self, st: &State) -> Value {
        let (w, h) = st
            .source
            .as_ref()
            .map(|s| {
                (
                    s["width"].as_u64().unwrap_or(1280),
                    s["height"].as_u64().unwrap_or(720),
                )
            })
            .unwrap_or((1280, 720));
        let (w, h) = encoded_size(w as u32, h as u32, quality_index(&st.quality));
        json!({"width": w, "height": h, "framesPerSecond": st.live.fps.load(Ordering::SeqCst),
               "audioEnabled": false, "queueDepth": 2})
    }

    fn status(&self, st: &State) -> Value {
        json!({"state": st.state, "sessionID": st.session_id, "generation": st.generation,
               "helperInstanceID": self.instance,
               "sourceID": st.source.as_ref().map(|s| s["id"].clone()),
               "viewerCount": st.viewer_count, "queuedFrameCount": 0, "droppedFrameCount": 0,
               "frameSequence": st.live.sequence.load(Ordering::SeqCst),
               "geometryRevision": st.geometry_revision,
               "pickerAvailable": false, "screenRecordingAuthorized": true, "audioEnabled": false,
               "configuration": self.configuration(st),
               "lastFrame": st.live.last_frame.lock().unwrap().clone(),
               "message": st.message, "reason": st.reason})
    }

    fn emit_capture(&self, st: &State, event: &str, source: Option<Value>) -> Value {
        let mut payload = json!({"event": event, "status": self.status(st)});
        if let Some(source) = source {
            payload["source"] = source;
        }
        if event == "capture.frameMetadata" {
            payload["metadata"] = payload["status"]["lastFrame"].clone();
        }
        payload
    }

    fn result(&self, st: &State, action: &str, accepted: bool, error: Option<&str>) -> Value {
        let mut result = json!({"accepted": accepted, "action": action, "status": self.status(st)});
        if let Some(code) = error {
            result["errorCode"] = json!(code);
        }
        result
    }

    fn require_identity(st: &State, params: &Value) -> Result<(), String> {
        if param_str(params, "sessionID") != st.session_id.as_deref()
            || params.get("generation").and_then(Value::as_i64) != Some(st.generation)
        {
            return Err("invalid_capture_session".into());
        }
        Ok(())
    }

    fn stop_capture(st: &mut State) {
        let _frame = st.live.frame_gate.lock().unwrap();
        if let Some(stop) = st.capture_stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
    }

    fn close_peer(st: &mut State) {
        if let Some(peer) = st.peer.take() {
            peer.stats.abort();
            peer.pc.on_connection_state_change(None);
            peer.pc.on_ice_candidate(None);
            peer.pc.close();
        }
    }

    // ---- control

    async fn revoke(&self, st: &mut State, reason: &str) {
        // Ending control also closes an unanswered prompt and the notice.
        st.prompt = None;
        st.indicator = None;
        let lease = st.lease.take();
        st.payloads.clear();
        self.cancel_input().await;
        if let Some(Value::Object(mut lease)) = lease {
            trace(&format!("event control.revoked {reason}"));
            lease.insert("event".into(), json!("control.revoked"));
            lease.insert("reason".into(), json!(reason));
            let _ = self.out.send(Value::Object(lease));
        }
    }

    async fn expire(&self, st: &mut State) {
        if st.lease.is_some() && st.lease_expires <= Instant::now() {
            self.revoke(st, "lease_expired").await;
        }
    }

    fn control_identity(st: &State, params: &Value) -> Result<Value, String> {
        Self::require_identity(st, params)?;
        let source_id = st.source.as_ref().and_then(|s| s["id"].as_str());
        if source_id.is_none()
            || param_str(params, "sourceID") != source_id
            || params.get("geometryRevision").and_then(Value::as_u64) != Some(st.geometry_revision)
        {
            return Err("invalid_capture_session".into());
        }
        for key in ["leaseID", "requestID", "sessionID", "sourceID"] {
            if !param_str(params, key).is_some_and(|v| (1..=160).contains(&v.len())) {
                return Err("invalid_input".into());
            }
        }
        let mut identity = serde_json::Map::new();
        for key in [
            "leaseID",
            "requestID",
            "sessionID",
            "generation",
            "geometryRevision",
            "sourceID",
        ] {
            identity.insert(key.into(), params[key].clone());
        }
        Ok(Value::Object(identity))
    }

    // ---- WebRTC

    async fn start_peer(self: &Arc<Self>, st: &mut State, rev: String) -> Result<(), String> {
        Self::close_peer(st);
        st.live.fps.store(
            st.live
                .desired_fps
                .load(Ordering::SeqCst)
                .min(SOFTWARE_MAX_FPS),
            Ordering::SeqCst,
        );
        let (w, h) = display_size().map_err(|error| peer_display_error(&error))?;
        let mut config = RtcConfiguration::default();
        config.ice_servers = vec![];
        config.continual_gathering_policy = ContinualGatheringPolicy::GatherContinually;
        let pc = self
            .factory
            .create_peer_connection(config)
            .map_err(|_| "peer_setup_failed")?;
        let fps = st.live.fps.load(Ordering::SeqCst);
        let max_bps = max_bitrate_bps(st.live.quality.load(Ordering::SeqCst), fps);
        let source = NativeVideoSource::new(
            VideoResolution {
                width: w,
                height: h,
            },
            false,
        );
        let track = self.factory.create_video_track("screen", source.clone());
        let transceiver = pc
            .add_transceiver(
                track.into(),
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::SendOnly,
                    stream_ids: vec!["screen".into()],
                    send_encodings: vec![RtpEncodingParameters {
                        max_bitrate: Some(max_bps),
                        max_framerate: Some(fps as f64),
                        ..Default::default()
                    }],
                },
            )
            .map_err(|e| e.to_string())?;
        let mut codecs: Vec<_> = self
            .factory
            .get_rtp_sender_capabilities(MediaType::Video)
            .codecs
            .into_iter()
            .filter(|c| {
                matches!(
                    c.mime_type.to_ascii_lowercase().as_str(),
                    "video/h264" | "video/vp8" | "video/rtx"
                )
            })
            .collect();
        if !codecs
            .iter()
            .any(|c| c.mime_type.eq_ignore_ascii_case("video/VP8"))
        {
            return Err("vp8_unavailable".into());
        }
        // Hardware factories and OpenH264 share constrained baseline. Prefer
        // packetization mode 1; keep VP8 for receivers without H264.
        codecs.retain(|c| {
            !c.mime_type.eq_ignore_ascii_case("video/H264")
                || encoder::supported_h264(c.sdp_fmtp_line.as_deref().unwrap_or_default())
        });
        codecs.sort_by_key(|c| {
            if c.mime_type.eq_ignore_ascii_case("video/H264") {
                0
            } else {
                1
            }
        });
        transceiver
            .set_codec_preferences(codecs)
            .map_err(|_| "peer_setup_failed")?;
        let sender = transceiver.sender();
        let mut params = sender.parameters();
        params.set_degradation_preference(DegradationPreference::MaintainResolution);
        sender.set_parameters(params).map_err(|e| e.to_string())?;
        // aiortc's VP8 encoder starts at 1.5 Mbit/s; match it so the first seconds compare.
        pc.set_bitrate(BitrateSettings {
            min_bitrate_bps: None,
            start_bitrate_bps: Some(1_500_000),
            max_bitrate_bps: Some(max_bps),
        })
        .map_err(|e| e.to_string())?;

        let (sid, generation) = (st.session_id.clone(), st.generation);
        {
            let out = self.out.clone();
            let (sid, rev) = (sid.clone(), rev.clone());
            pc.on_ice_candidate(Some(Box::new(move |c: IceCandidate| {
                let text = c.candidate();
                let text = text.strip_prefix("a=").unwrap_or(&text).to_string();
                let _ = out.send(json!({"event": "signal.candidate", "sessionID": sid,
                    "generation": generation, "peerRevision": rev, "candidate": text,
                    "sdpMid": c.sdp_mid(), "sdpMLineIndex": c.sdp_mline_index()}));
            })));
        }
        {
            let out = self.out.clone();
            let (sid, rev) = (sid.clone(), rev.clone());
            let weak = Arc::downgrade(self);
            let runtime = tokio::runtime::Handle::current();
            pc.on_connection_state_change(Some(Box::new(move |s: PeerConnectionState| {
                let Some((mapped, terminal)) = peer_state_report(s) else {
                    return;
                };
                if terminal {
                    if let Some(helper) = weak.upgrade() {
                        let rev = rev.clone();
                        runtime.spawn(async move {
                            let mut st = helper.state.lock().await;
                            if st.peer.as_ref().is_some_and(|p| p.revision == rev) {
                                Helper::stop_capture(&mut st);
                                helper.revoke(&mut st, "peer_disconnected").await;
                            }
                        });
                    }
                }
                bench(json!({"k": "peer", "state": mapped, "t": epoch()}));
                trace(&format!("event signal.peerState {mapped}"));
                let _ = out.send(json!({"event": "signal.peerState", "sessionID": sid,
                    "generation": generation, "peerRevision": rev, "state": mapped}));
            })));
        }
        self.signal(st, "signal.peerReady", &rev, json!({}));
        let offer = pc
            .create_offer(OfferOptions::default())
            .await
            .map_err(|e| e.to_string())?;
        pc.set_local_description(offer.clone())
            .await
            .map_err(|e| e.to_string())?;
        self.signal(
            st,
            "signal.offer",
            &rev,
            json!({"type": "offer", "sdp": offer.to_string()}),
        );
        let stats = tokio::spawn(stats_loop(pc.clone(), Arc::downgrade(self), rev.clone()));
        st.peer = Some(Peer {
            pc,
            sender,
            source,
            revision: rev,
            stats,
            encoder: None,
        });
        Ok(())
    }

    // ---- requests

    async fn cancel_input(&self) {
        self.execution.send_modify(|generation| *generation += 1);
        let mut input = self.input.lock().await;
        self.release_held(&mut input).await;
    }

    async fn input(self: &Arc<Self>, params: &Value) -> Result<Value, String> {
        let queued_generation = *self.execution.borrow();
        let _batch = self.batches.lock().await;
        // A busy executor never holds the control-state lock while waiting.
        let (mut guard, mut input) = loop {
            let guard = self.state.lock().await;
            if let Ok(input) = self.input.try_lock() {
                break (guard, input);
            }
            drop(guard);
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        let st = &mut *guard;
        if *self.execution.borrow() != queued_generation {
            return Err("lease_not_active".into());
        }
        self.expire_without_input(st)?;

        let identity = Self::control_identity(st, params)?;
        if st.lease.as_ref() != Some(&identity) {
            return Ok(json!({"accepted": false, "reason": "lease_not_active"}));
        }
        if st.state != "capturing" {
            return Ok(json!({"accepted": false, "reason": "capture_not_live"}));
        }
        let sequence = params
            .get("sequence")
            .and_then(Value::as_i64)
            .filter(|s| *s > 0);
        let actions = params
            .get("actions")
            .and_then(Value::as_array)
            .filter(|a| (1..=32).contains(&a.len()));
        let (Some(sequence), Some(actions)) = (sequence, actions) else {
            return Err("invalid_input".into());
        };
        // serde_json without preserve_order sorts keys: a canonical payload.
        let payload = Value::Array(actions.clone()).to_string();
        if let Some((_, seen)) = st.payloads.iter().find(|(s, _)| *s == sequence) {
            let matching = *seen == payload;
            return Ok(json!({"accepted": matching, "duplicate": matching,
                        "reason": if matching { Value::Null } else { json!("sequence_payload_conflict") }}));
        }
        if sequence != st.last_input_sequence + 1 {
            return Ok(json!({"accepted": false, "reason": "input_sequence_must_increase_by_one"}));
        }

        let generation = *self.execution.borrow();
        input.generation = generation;
        input.execution.borrow_and_update();
        input.source = st.source.clone();
        input.deadline = Some(Instant::now() + Duration::from_secs(8));
        drop(guard);
        let mut result = json!({"accepted": true, "duplicate": false});
        let kinds: Vec<&str> = actions.iter().filter_map(|a| a["type"].as_str()).collect();
        trace(&format!("control.input {sequence} {}", kinds.join(",")));
        let deadline = Instant::now() + Duration::from_secs(8);
        let delivered = async {
            for action in actions {
                input.check()?;
                if Instant::now() >= deadline {
                    return Err("input_timed_out".to_string());
                }
                self.deliver(&mut input, action, &mut result).await?;
                input.check()?;
            }
            Ok(())
        }
        .await;
        // Cancelled subprocesses have kill-on-drop; release owns the
        // same mutex, so no subsequent batch can overtake cleanup.
        drop(input);
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        if *self.execution.borrow() != generation {
            return Err("lease_not_active".into());
        }
        if let Err(code) = delivered {
            // Partial delivery is uncertain: revoke rather than replay.
            self.revoke(st, "input_delivery_failed").await;
            return Err(code);
        }
        st.last_input_sequence = sequence;
        st.payloads.push((sequence, payload));
        if st.payloads.len() > 64 {
            st.payloads.remove(0);
        }
        Ok(result)
    }

    fn expire_without_input(&self, st: &State) -> Result<(), String> {
        if st.lease.is_some() && st.lease_expires <= Instant::now() {
            Err("lease_expired".into())
        } else {
            Ok(())
        }
    }

    async fn handle(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value, String> {
        if method == "control.input" {
            return self.input(params).await;
        }
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        self.expire(st).await;
        let peer_ok = |st: &State| {
            st.peer
                .as_ref()
                .is_some_and(|p| Some(p.revision.as_str()) == param_str(params, "peerRevision"))
        };
        match method {
            "capabilities" => {
                let mut capture = capture_capability();
                capture["liveQualityChange"] = json!(true);
                Ok(
                    json!({"protocolVersion": 1, "capture": capture, "control": platform::control_capability()}),
                )
            }
            "capture.prepare" | "prepare" => {
                let quality = param_str(params, "videoQuality")
                    .unwrap_or("standard")
                    .to_string();
                let Some(fps) = requested_fps(&quality, params) else {
                    return Err("invalid_input".into());
                };
                self.revoke(st, "capture_replaced").await;
                st.consents.clear();
                Self::stop_capture(st);
                Self::close_peer(st);
                st.session_id = Some(
                    param_str(params, "sessionID")
                        .map(String::from)
                        .unwrap_or_else(revision),
                );
                st.generation = params
                    .get("generation")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                st.viewer_count = params
                    .get("viewerCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(1);
                st.quality = quality;
                st.live = Arc::new(Live::default());
                st.live.desired_fps.store(fps, Ordering::SeqCst);
                st.live
                    .fps
                    .store(fps.min(SOFTWARE_MAX_FPS), Ordering::SeqCst);
                st.live
                    .quality
                    .store(quality_index(&st.quality), Ordering::SeqCst);
                st.source = None;
                st.geometry_revision = 1;
                st.reason = None;
                st.message = None;
                st.state = if st.viewer_count > 0 {
                    "awaitingSource"
                } else {
                    "stopped"
                };
                let _ = self
                    .out
                    .send(self.emit_capture(st, "capture.prepared", None));
                trace("event capture.prepared");
                st.start_peer = st.viewer_count > 0;
                Ok(self.result(st, "prepare", st.viewer_count > 0, None))
            }
            "capture.rebind" => {
                Self::require_identity(st, params)?;
                if st.source.as_ref().and_then(|s| s["id"].as_str())
                    != param_str(params, "sourceID")
                    || params["geometryRevision"].as_u64() != Some(st.geometry_revision)
                {
                    return Err("invalid_capture_session".into());
                }
                if st.lease.is_some() {
                    if st.lease.as_ref() != Some(&Self::control_identity(st, params)?) {
                        return Err("lease_not_active".into());
                    }
                } else if !params["leaseID"].is_null() {
                    return Err("lease_not_active".into());
                }
                let source = match platform::resolve_source(param_str(params, "nextSourceID")) {
                    Ok(source) => source,
                    Err(_) => {
                        return Ok(self.result(st, "rebind", false, Some("source_not_found")))
                    }
                };
                // Stop before releasing input; old in-flight frames check their own stop flag.
                let was_capturing = st.capture_stop.is_some();
                Self::stop_capture(st);
                self.cancel_input().await;
                st.rebind_source(source.clone());
                if was_capturing {
                    self.begin_capture(st)?;
                }
                let mut result = self.result(st, "rebind", true, None);
                result["source"] = source;
                Ok(result)
            }
            "capture.pick" | "pick" => {
                if st.session_id.is_none() {
                    return Ok(self.result(st, "pick", false, Some("capture_not_prepared")));
                }
                if params.get("sessionID").is_some() {
                    Self::require_identity(st, params)?;
                }
                let source = match platform::resolve_source(param_str(params, "sourceID")) {
                    Ok(source) => source,
                    Err(error) => {
                        // The window closed before sharing began: say so
                        // rather than leave the view preparing. A Wayland
                        // login has no X11 screen at all.
                        let reason = if error == "wayland_unsupported" {
                            "wayland_unsupported"
                        } else {
                            "source_not_found"
                        };
                        Self::stop_capture(st);
                        st.state = "failed";
                        st.reason = Some(reason.into());
                        return Ok(self.result(st, "pick", false, Some(reason)));
                    }
                };
                Self::stop_capture(st);
                self.revoke(st, "source_replaced").await;
                *st.live.last_frame.lock().unwrap() = None;
                *st.live.metadata_at.lock().unwrap() = None;
                st.source = Some(source.clone());
                st.state = "ready";
                st.message = Some("Selecting the source");
                let pending = self.result(st, "pick", true, None);
                st.message = None;
                let event = self.emit_capture(st, "capture.sourceSelected", Some(source));
                st.deferred.push(event);
                Ok(pending)
            }
            "capture.start" | "startCapture" => {
                Self::require_identity(st, params)?;
                if st.source.is_none() {
                    return Ok(self.result(st, "start", false, Some("source_not_selected")));
                }
                let Some(_peer) = st.peer.as_ref() else {
                    return Ok(self.result(st, "start", false, Some("capture_not_prepared")));
                };
                if matches!(st.state, "ready" | "paused") && st.capture_stop.is_none() {
                    self.begin_capture(st)?;
                    st.state = "starting";
                }
                Ok(self.result(st, "start", true, None))
            }
            "capture.stop" | "stopCapture" => {
                Self::require_identity(st, params)?;
                Self::stop_capture(st);
                st.state = "stopped";
                st.reason = Some(
                    param_str(params, "reason")
                        .unwrap_or("stopped_by_request")
                        .into(),
                );
                self.revoke(st, "capture_stopped").await;
                Self::close_peer(st);
                Ok(self.result(st, "stop", true, None))
            }
            "capture.setQuality" => {
                Self::require_identity(st, params)?;
                // A request that only moves the frame-rate ceiling keeps the quality.
                let quality = param_str(params, "videoQuality")
                    .or_else(|| {
                        params
                            .get("videoMaxFramesPerSecond")
                            .map(|_| st.quality.as_str())
                    })
                    .unwrap_or_default()
                    .to_string();
                let Some(fps) = requested_fps(&quality, params) else {
                    return Err("invalid_input".into());
                };
                st.quality = quality;
                st.live.desired_fps.store(fps, Ordering::SeqCst);
                let config = self.configuration(st);
                let cap = st.peer.as_ref().and_then(|p| p.encoder.as_ref()).map_or(
                    SOFTWARE_MAX_FPS,
                    |(codec, fmtp, hardware)| {
                        encoder::encoder_cap(
                            codec,
                            fmtp,
                            config["width"].as_u64().unwrap_or(0) as u32,
                            config["height"].as_u64().unwrap_or(0) as u32,
                            *hardware,
                        )
                    },
                );
                st.live.fps.store(fps.min(cap), Ordering::SeqCst);
                st.live
                    .quality
                    .store(quality_index(&st.quality), Ordering::SeqCst);
                if let Some(peer) = &st.peer {
                    if let Err(e) = apply_send_limits(&peer.pc, &peer.sender, &st.live) {
                        trace(&format!("send limits {e}"));
                    }
                }
                trace(&format!("quality {} fps {fps}", st.quality));
                Ok(self.result(st, "setQuality", true, None))
            }
            "capture.status" | "status" => Ok(self.status(st)),
            "signal.answer" => {
                if !peer_ok(st) {
                    return Err("invalid_input".into());
                }
                let sdp = param_str(params, "sdp").unwrap_or_default();
                let desc = SessionDescription::parse(sdp, SdpType::Answer)
                    .map_err(|e| format!("{e:?}"))?;
                let pc = st.peer.as_ref().unwrap().pc.clone();
                pc.set_remote_description(desc)
                    .await
                    .map_err(|e| e.to_string())?;
                let rev = st.peer.as_ref().unwrap().revision.clone();
                self.signal(st, "signal.answerApplied", &rev, json!({}));
                Ok(json!({"accepted": true}))
            }
            "signal.candidate" => {
                if !peer_ok(st) {
                    return Err("invalid_input".into());
                }
                let text = param_str(params, "candidate").unwrap_or_default();
                if text.starts_with("candidate:") && !text.contains(".local ") {
                    let mid = param_str(params, "sdpMid").unwrap_or("0");
                    let index = params
                        .get("sdpMLineIndex")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let c = IceCandidate::parse(mid, index as i32, text)
                        .map_err(|e| format!("{e:?}"))?;
                    let pc = st.peer.as_ref().unwrap().pc.clone();
                    pc.add_ice_candidate(c).await.map_err(|e| e.to_string())?;
                }
                Ok(json!({"accepted": true}))
            }
            "signal.close" => {
                if peer_ok(st) {
                    Self::stop_capture(st);
                    self.revoke(st, "peer_closed").await;
                    Self::close_peer(st);
                }
                Ok(json!({"closed": true, "sessionID": st.session_id, "generation": st.generation}))
            }
            // The dispatcher answers consent through `consent`, which may wait.
            "control.consent" => Err("unsupported_action".into()),
            "control.activate" => {
                let identity = Self::control_identity(st, params)?;
                if let Some(lease) = &st.lease {
                    let same = *lease == identity;
                    return Ok(json!({"activated": same, "replayed": same,
                        "reason": if same { Value::Null } else { json!("control_busy") }}));
                }
                let request = identity["requestID"].as_str().unwrap();
                if !st
                    .consents
                    .iter()
                    .any(|(r, v)| r == request && *v == identity)
                {
                    return Ok(json!({"activated": false, "reason": "consent_required"}));
                }
                if let LocalApproval::Ask(show) = &self.local {
                    // Without the notice and its Stop button, no control.
                    let Some(Panel { choice, window }) = show(CONTROLLED_TEXT, CONTROLLED_BUTTONS)
                    else {
                        return Ok(
                            json!({"activated": false, "reason": "local_approval_unavailable"}),
                        );
                    };
                    let (shown, closed) = OnScreen::new(st, window);
                    let token = shown.token;
                    st.indicator = Some(shown);
                    let helper = Arc::clone(self);
                    tokio::spawn(async move {
                        let pressed = tokio::select! {
                            pressed = choice => pressed.is_ok(),
                            // Control already ended and closed the notice.
                            _ = closed => false,
                        };
                        if pressed {
                            let mut st = helper.state.lock().await;
                            if st.indicator.as_ref().map(|s| s.token) == Some(token) {
                                trace("control stopped on the computer");
                                // Stop withdraws every approval: a replayed
                                // request must ask on the computer again.
                                st.consents.clear();
                                helper.revoke(&mut st, "local_stop").await;
                            }
                        }
                    });
                }
                st.lease = Some(identity);
                st.lease_expires = Instant::now() + Duration::from_secs(10);
                st.payloads.clear();
                st.last_input_sequence = 0;
                trace("control activated");
                Ok(json!({"activated": true, "replayed": false}))
            }
            "control.heartbeat" => {
                let identity = Self::control_identity(st, params)?;
                let renewed = st.lease.as_ref() == Some(&identity);
                if renewed {
                    st.lease_expires = Instant::now() + Duration::from_secs(10);
                }
                Ok(json!({"renewed": renewed}))
            }
            "control.release" | "control.releaseAll" | "control.revoke" => {
                let released = if method == "control.release" {
                    let identity = Self::control_identity(st, params)?;
                    if st.prompt.as_ref().is_some_and(|p| p.identity == identity) {
                        st.prompt = None;
                        return Ok(json!({"released": true}));
                    }
                    st.lease.as_ref() == Some(&identity)
                } else {
                    st.prompt = None;
                    st.lease.is_some()
                };
                if released {
                    self.revoke(st, "released").await;
                }
                Ok(json!({"released": released}))
            }
            _ => Err("unsupported_action".into()),
        }
    }

    /// `control.consent`: a replayed or locally unneeded answer comes back
    /// now; otherwise the computer asks, and the answer follows later so
    /// heartbeats, release and stop keep flowing meanwhile. Release, stop, a
    /// new view or the helper's end close the prompt and deny the request.
    async fn consent(self: &Arc<Self>, params: &Value) -> Result<Reply, String> {
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        self.expire(st).await;
        let identity = Self::control_identity(st, params)?;
        let request = identity["requestID"].as_str().unwrap().to_string();
        let previous = st
            .consents
            .iter()
            .find(|(r, _)| *r == request)
            .map(|(_, v)| v.clone());
        if previous.as_ref().is_some_and(|p| *p != identity) {
            return Err("request_identity_conflict".into());
        }
        if previous.is_some() {
            return Ok(Reply::Now(granted(true)));
        }
        let show = match &self.local {
            LocalApproval::NotNeeded => {
                st.grant(request, identity);
                return Ok(Reply::Now(granted(false)));
            }
            LocalApproval::Ask(show) => show,
        };
        if st.lease.is_some() || st.prompt.is_some() {
            return Ok(Reply::Now(denied("control_busy")));
        }
        let Some(Panel { choice, window }) = show(ASK_TEXT, ASK_BUTTONS) else {
            return Ok(Reply::Now(denied("local_approval_unavailable")));
        };
        let (shown, closed) = OnScreen::new(st, window);
        let token = shown.token;
        st.prompt = Some(Prompt {
            identity: identity.clone(),
            shown,
        });
        trace("control consent asked on the computer");
        let helper = Arc::clone(self);
        Ok(Reply::Later(Box::pin(async move {
            let answer = tokio::select! {
                answer = tokio::time::timeout(Duration::from_secs(ASK_SECONDS), choice) => answer,
                _ = closed => return denied("consent_cancelled"),
            };
            let mut st = helper.state.lock().await;
            if st.prompt.as_ref().map(|p| p.shown.token) != Some(token) {
                return denied("consent_cancelled");
            }
            st.prompt = None;
            match answer {
                Ok(Ok(0)) => {
                    trace("control consent allowed on the computer");
                    st.grant(request, identity);
                    granted(false)
                }
                Err(_) => denied("consent_timed_out"),
                _ => denied("consent_denied"),
            }
        })))
    }

    /// After the response: deferred events, then the offer (prepare).
    async fn after_response(self: &Arc<Self>) {
        let mut guard = self.state.lock().await;
        let st = &mut *guard;
        for event in std::mem::take(&mut st.deferred) {
            trace(&format!(
                "event {}",
                event["event"].as_str().unwrap_or_default()
            ));
            let _ = self.out.send(event);
        }
        if std::mem::take(&mut st.start_peer) {
            // Mint the revision before anything can fail, so the failure
            // names the peer the host is waiting on.
            let rev = revision();
            st.pending_peer = Some(rev.clone());
            if let Err(e) = self.start_peer(st, rev.clone()).await {
                eprintln!("wonder-computer: peer setup failed: {e}");
                self.fail_peer_setup(st, &rev, peer_failure_code(&e));
            }
            st.pending_peer = None;
        }
    }

    /// Peer setup did not finish in time (after_response was cancelled).
    async fn peer_setup_timed_out(&self) {
        let mut st = self.state.lock().await;
        st.start_peer = false;
        match st.pending_peer.take() {
            Some(rev) => self.fail_peer_setup(&mut st, &rev, "peer_setup_timeout"),
            None => Self::close_peer(&mut st),
        }
    }

    /// Retire capture and the half-built peer, and tell the host why.
    fn fail_peer_setup(&self, st: &mut State, rev: &str, reason: &str) {
        Self::stop_capture(st);
        Self::close_peer(st);
        st.state = "failed";
        st.reason = Some(reason.into());
        self.signal(st, "signal.failed", rev, json!({"reason": reason}));
    }
}

/// A Linux login with no X11 display to share is told apart from a missing screen.
fn peer_display_error(error: &str) -> &'static str {
    if error == "wayland_unsupported" {
        "wayland_unsupported"
    } else {
        "no_display"
    }
}

/// The host's reason code for a peer setup error; other errors are an offer
/// that could not be made.
fn peer_failure_code(error: &str) -> &'static str {
    match error {
        "vp8_unavailable" => "vp8_unavailable",
        "no_display" => "no_display",
        "wayland_unsupported" => "wayland_unsupported",
        "peer_setup_failed" => "peer_setup_failed",
        _ => "offer_failed",
    }
}

#[cfg(target_os = "linux")]
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Observe the active encoder for the lifetime of this peer to enforce its cap.
/// Only benchmark builds record content-free counters; release logs transitions.
async fn stats_loop(pc: PeerConnection, helper: std::sync::Weak<Helper>, revision: String) {
    let mut last_encoder = None;
    let mut last_frames = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if pc.connection_state() == PeerConnectionState::Closed {
            return;
        }
        let Ok(stats) = pc.get_stats().await else {
            continue;
        };
        for item in &stats {
            let RtcStats::OutboundRtp(o) = item else {
                continue;
            };
            if o.stream.kind != "video" || o.outbound.frames_encoded == 0 {
                continue;
            }
            let codec = stats.iter().find_map(|item| match item {
                RtcStats::Codec(c) if c.rtc.id == o.stream.codec_id => Some(&c.codec),
                _ => None,
            });
            let Some(codec) = codec else { continue };
            let hardware = codec.mime_type.eq_ignore_ascii_case("video/H264")
                && o.outbound.power_efficient_encoder;
            let cap = encoder::encoder_cap(
                &codec.mime_type,
                &codec.sdp_fmtp_line,
                o.outbound.frame_width,
                o.outbound.frame_height,
                hardware,
            );
            let identity = (
                codec.mime_type.clone(),
                o.outbound.encoder_implementation.clone(),
                hardware,
            );
            if last_encoder.as_ref() != Some(&identity) {
                trace(&format!(
                    "encoder codec={} implementation={} mode={} cap={cap}",
                    identity.0,
                    identity.1,
                    if hardware { "hardware" } else { "software" }
                ));
                eprintln!("WONDER_COMPUTER_CODEC {}", codec.mime_type);
                last_encoder = Some(identity);
            }
            // The peer owns this task. Upgrade only while applying a sample;
            // never keep Helper alive across sleep/get_stats or stale sessions.
            if o.outbound.frames_encoded > last_frames {
                if let Some(helper) = helper.upgrade() {
                    let mut st = helper.state.lock().await;
                    if let Some(peer) = st.peer.as_mut().filter(|p| p.revision == revision) {
                        peer.encoder = Some((
                            codec.mime_type.clone(),
                            codec.sdp_fmtp_line.clone(),
                            hardware,
                        ));
                        let sender = peer.sender.clone();
                        // A quality change can precede its first encoded frame.
                        // Honor both sizes until the new-size stats arrive.
                        let config = helper.configuration(&st);
                        let cap = cap.min(encoder::encoder_cap(
                            &codec.mime_type,
                            &codec.sdp_fmtp_line,
                            config["width"].as_u64().unwrap_or(0) as u32,
                            config["height"].as_u64().unwrap_or(0) as u32,
                            hardware,
                        ));
                        let wanted = st.live.desired_fps.load(Ordering::SeqCst).min(cap);
                        let old = st.live.fps.swap(wanted, Ordering::SeqCst);
                        if old != wanted {
                            if let Err(e) = apply_send_limits(&pc, &sender, &st.live) {
                                // Keep capture at the conservative limit if the sender rejects a raise.
                                st.live.fps.store(old.min(wanted), Ordering::SeqCst);
                                trace(&format!("encoder rate update failed: {e}"));
                            } else {
                                trace(&format!("encoder rate {old}->{wanted}"));
                                let _ = helper.out.send(helper.emit_capture(
                                    &st,
                                    "capture.frameMetadata",
                                    None,
                                ));
                            }
                        }
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            }
            last_frames = o.outbound.frames_encoded;
            #[cfg(feature = "parity-tests")]
            bench(
                json!({"encoder": o.outbound.encoder_implementation, "hardware": hardware, "encoderCap": cap, "codec": codec.mime_type, "framesEncoded": o.outbound.frames_encoded, "framesSent": o.outbound.frames_sent,
                "bytesSent": o.sent.bytes_sent, "width": o.outbound.frame_width, "height": o.outbound.frame_height,
                "fps": o.outbound.frames_per_second, "targetBitrate": o.outbound.target_bitrate,
                "totalEncodeTime": o.outbound.total_encode_time,
                "limit": match o.outbound.quality_limitation_reason {libwebrtc::stats::QualityLimitationReason::None => "none",
                libwebrtc::stats::QualityLimitationReason::Cpu => "cpu", libwebrtc::stats::QualityLimitationReason::Bandwidth => "bandwidth", _ => "other"}}),
            );
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!(
            "wonder-computer-helper {} protocol=1 codecs=H264,VP8 verificationOnly={} {}",
            env!("CARGO_PKG_VERSION"),
            cfg!(feature = "parity-tests"),
            concat!(
                "WONDER_LINKED_WEBRTC_SHA256=",
                env!("WONDER_LINKED_WEBRTC_SHA256")
            )
        );
        return;
    }
    if std::env::args().any(|a| a == "--codec-capabilities") {
        let factory = PeerConnectionFactory::default();
        let codecs: Vec<_> = factory
            .get_rtp_sender_capabilities(MediaType::Video)
            .codecs
            .into_iter()
            .map(|c| json!({"mimeType": c.mime_type, "fmtp": c.sdp_fmtp_line}))
            .collect();
        println!("{}", json!({"codecs": codecs}));
        return;
    }
    if std::env::args().any(|a| a == "--discover-sources") {
        discover_sources();
        return;
    }
    // One-shot dock answers for the daemon's `GET /api/v1/computer/dock`.
    if std::env::args().any(|a| a == "--dock") {
        println!("{}", platform::dock());
        return;
    }
    if let Some(at) = std::env::args().position(|a| a == "--launch-app") {
        let id = std::env::args().nth(at + 1).unwrap_or_default();
        if let Err(reason) = platform::launch_app(&id) {
            eprintln!("{reason}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "windows")]
    if std::env::args().any(|a| a == "--input-owner") {
        platform::input_owner();
        return;
    }
    bench(json!({"k": "start", "t": epoch()}));
    let handshake = std::env::var("WONDER_COMPUTER_USE_HANDSHAKE").unwrap_or_default();
    platform::initialize();
    let (sender, mut rx) = mpsc::channel::<(Value, usize)>(128);
    let failed = Arc::new(tokio::sync::Notify::new());
    let tx = Out {
        tx: sender,
        failed: failed.clone(),
        bytes: Arc::new(AtomicUsize::new(0)),
    };
    let writer_failed = failed.clone();
    let writer_bytes = tx.bytes.clone();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some((v, size)) = rx.recv().await {
            let data = format!("{v}\n");
            let write = async {
                out.write_all(data.as_bytes()).await?;
                out.flush().await
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), write).await,
                Ok(Ok(()))
            ) {
                writer_failed.notify_one();
                break;
            }
            writer_bytes.fetch_sub(size, Ordering::SeqCst);
        }
    });
    let (execution, _) = tokio::sync::watch::channel(0);
    let helper = Arc::new(Helper {
        factory: PeerConnectionFactory::default(),
        out: tx.clone(),
        instance: instance_id(),
        local: local_approval(),
        state: Mutex::new(State::new()),
        input: Mutex::new(InputState {
            source: None,
            held_buttons: BTreeSet::new(),
            held_keys: BTreeSet::new(),
            execution: execution.subscribe(),
            generation: 0,
            deadline: None,
            #[cfg(all(test, target_os = "linux"))]
            command: None,
        }),
        execution,
        batches: Mutex::new(()),
    });
    let watchdog = {
        let helper = Arc::clone(&helper);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let mut st = helper.state.lock().await;
                helper.expire(&mut st).await;
            }
        })
    };
    let focus_stop = Arc::new(AtomicBool::new(false));
    let focus_thread = {
        let helper = helper.clone();
        let stop = focus_stop.clone();
        std::thread::spawn(move || {
            let mut previous = None;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                let binding = helper.state.try_lock().ok().and_then(|st| {
                    st.lease
                        .clone()
                        .zip(st.source.clone())
                        .map(|(identity, source)| (identity, source, *helper.execution.borrow()))
                });
                let Some((identity, source, execution)) = binding else {
                    previous = None;
                    continue;
                };
                let focused = platform::text_input_focused(&source);
                if previous.as_ref() == Some(&(identity.clone(), execution, focused)) {
                    continue;
                }
                if let Ok(st) = helper.state.try_lock() {
                    if st.lease.as_ref() == Some(&identity)
                        && *helper.execution.borrow() == execution
                    {
                        let mut event = identity.clone();
                        event["event"] = json!("control.textFocus");
                        event["textInput"] = json!(focused);
                        let _ = helper.out.send(event);
                        previous = Some((identity, execution, focused));
                    }
                }
            }
        })
    };
    let mut input_tasks = tokio::task::JoinSet::new();
    let mut reader = BufReader::new(tokio::io::stdin());
    loop {
        while input_tasks.try_join_next().is_some() {}
        let mut raw = Vec::new();
        let read = async {
            (&mut reader)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut raw)
                .await
        };
        let count =
            tokio::select! { value = read => value.unwrap_or(0), _ = failed.notified() => 0 };
        if count == 0 || raw.len() > MAX_LINE_BYTES || raw.last() != Some(&b'\n') {
            break;
        }
        let Ok(raw) = std::str::from_utf8(&raw) else {
            break;
        };
        let Ok(request) = serde_json::from_str::<Value>(&raw) else {
            break;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        if handshake.is_empty()
            || params.get("handshake").and_then(Value::as_str) != Some(&handshake)
        {
            let _ = tx.send(json!({"id": id, "error": {"code": "handshake_required"}}));
            continue;
        }
        if method == "stop" {
            let _ = tx.send(json!({"id": id, "result": {"stopped": true}}));
            break;
        }
        if method == "control.input" {
            // Bound pending batches. A stalled client cannot allocate unbounded tasks.
            if input_tasks.len() >= 8 {
                let _ = tx
                    .send(json!({"id": id, "result": {"accepted":false,"reason":"control_busy"}}));
                continue;
            }
            let helper = helper.clone();
            let tx = tx.clone();
            input_tasks.spawn(async move {
                let reply = match helper.input(&params).await {
                    Ok(mut result) => {
                        result["action"] = json!("control.input");
                        json!({"id":id,"result":result})
                    }
                    Err(code) => json!({"id":id,"error":{"code":code}}),
                };
                let _ = tx.send(reply);
            });
            continue;
        }
        let handled = tokio::time::timeout(Duration::from_secs(10), async {
            if method == "control.consent" {
                helper.consent(&params).await
            } else {
                helper.handle(&method, &params).await.map(Reply::Now)
            }
        })
        .await;
        let handled = match handled {
            Ok(Ok(Reply::Later(answer))) => {
                // The person at the computer answers; keep reading requests.
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut result = answer.await;
                    result["action"] = json!("control.consent");
                    let _ = tx.send(json!({"id": id, "result": result}));
                });
                continue;
            }
            Ok(Ok(Reply::Now(value))) => Ok(value),
            Ok(Err(code)) => Err(code),
            Err(_) => {
                let mut st = helper.state.lock().await;
                helper.revoke(&mut st, "request_timed_out").await;
                Err("request_timed_out".into())
            }
        };
        match handled {
            Ok(mut result) => {
                if let Some(obj) = result.as_object_mut() {
                    obj.entry("action").or_insert(json!(method));
                }
                let _ = tx.send(json!({"id": id, "result": result}));
            }
            Err(code) => {
                let _ = tx.send(json!({"id": id, "error": {"code": code}}));
            }
        }
        if tokio::time::timeout(Duration::from_secs(10), helper.after_response())
            .await
            .is_err()
        {
            helper.peer_setup_timed_out().await;
        }
    }
    focus_stop.store(true, Ordering::SeqCst);
    // Focus has bounded library calls and never owns injected input.
    drop(focus_thread);
    watchdog.abort();
    let _ = watchdog.await;
    {
        let mut guard = helper.state.lock().await;
        let st = &mut *guard;
        Helper::stop_capture(st);
        helper.revoke(st, "helper_stopped").await;
        Helper::close_peer(st);
    }
    input_tasks.abort_all();
    while input_tasks.join_next().await.is_some() {}
    drop(tx);
    drop(helper);
    let mut writer = writer;
    if tokio::time::timeout(Duration::from_secs(3), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

impl State {
    /// Records an allowed request, keeping the most recent 64.
    fn grant(&mut self, request: String, identity: Value) {
        self.consents.push((request, identity));
        if self.consents.len() > 64 {
            self.consents.remove(0);
        }
    }

    fn rebind_source(&mut self, source: Value) {
        self.source = Some(source.clone());
        self.geometry_revision += 1;
        *self.live.last_frame.lock().unwrap() = None;
        *self.live.metadata_at.lock().unwrap() = None;
        if let Some(lease) = self.lease.as_mut() {
            lease["sourceID"] = source["id"].clone();
            lease["geometryRevision"] = json!(self.geometry_revision);
            self.lease_expires = Instant::now() + Duration::from_secs(10);
            if let Some((_, consent)) = self
                .consents
                .iter_mut()
                .find(|(id, _)| Some(id.as_str()) == lease["requestID"].as_str())
            {
                *consent = lease.clone();
            }
        }
    }
}

impl Helper {
    fn begin_capture(self: &Arc<Self>, st: &mut State) -> Result<(), String> {
        let peer = st.peer.as_ref().ok_or("capture_not_prepared")?;
        let stop = Arc::new(AtomicBool::new(false));
        let job = CaptureJob {
            helper: self.clone(),
            runtime: tokio::runtime::Handle::current(),
            source: peer.source.clone(),
            live: st.live.clone(),
            stop: stop.clone(),
            selected_source: st.source.clone().ok_or("source_not_selected")?,
            session_id: st.session_id.clone(),
            generation: st.generation,
        };
        std::thread::Builder::new()
            .name("computer-capture".into())
            .spawn(move || job.run())
            .map_err(|_| "capture_unavailable")?;
        st.capture_stop = Some(stop);
        Ok(())
    }
}

#[derive(Clone)]
struct CaptureJob {
    helper: Arc<Helper>,
    runtime: tokio::runtime::Handle,
    source: NativeVideoSource,
    live: Arc<Live>,
    stop: Arc<AtomicBool>,
    selected_source: Value,
    session_id: Option<String>,
    generation: i64,
}
impl CaptureJob {
    fn submit(&self, mut buffer: I420Buffer) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let (w, h) = (buffer.width(), buffer.height());
        let encoded = encoded_size(w, h, self.live.quality.load(Ordering::SeqCst));
        if encoded != (w, h) {
            buffer = buffer.scale(encoded.0 as i32, encoded.1 as i32);
        }
        // Synchronize only publication with source replacement. Scaling and
        // input never hold this gate, and stop waits for the final old frame.
        let frame_guard = self.live.frame_gate.lock().unwrap();
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        self.source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: (epoch() * 1e6) as i64,
            frame_metadata: None,
            buffer,
        });
        drop(frame_guard);
        let Ok(mut st) = self.helper.state.try_lock() else {
            // A busy control request must not swallow the readiness frame: a
            // static Windows window can produce only this one. Publish it
            // from the runtime once the lock frees (one pending task at most).
            if !self.live.publish_pending.swap(true, Ordering::SeqCst) {
                let job = self.clone();
                self.runtime.spawn(async move {
                    let mut st = job.helper.state.lock().await;
                    job.live.publish_pending.store(false, Ordering::SeqCst);
                    job.publish(&mut st, w, h).await;
                });
            }
            return;
        };
        self.runtime.block_on(self.publish(&mut st, w, h));
    }
    /// Record the newest frame's metadata and, for the first frame, make the
    /// capture live. The caller holds the control-state lock.
    async fn publish(&self, st: &mut State, w: u32, h: u32) {
        if self.stop.load(Ordering::SeqCst)
            || st.session_id != self.session_id
            || st.generation != self.generation
        {
            return;
        }
        let resized = st.source.as_ref().is_some_and(|s| {
            (
                s["width"].as_u64().unwrap_or(0) as u32 & !1,
                s["height"].as_u64().unwrap_or(0) as u32 & !1,
            ) != (w & !1, h & !1)
        });
        if resized {
            self.helper.revoke(st, "source_geometry_changed").await;
            st.geometry_revision += 1;
            if let Some(source) = st.source.as_mut() {
                source["width"] = json!(w);
                source["height"] = json!(h);
                source["contentRect"] = json!({"x":0,"y":0,"width":w,"height":h});
            }
        }
        let sequence = self.live.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let mut metadata_at = self.live.metadata_at.lock().unwrap();
        let emit = resized || metadata_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if emit {
            *metadata_at = Some(Instant::now());
        }
        drop(metadata_at);
        let mut last_frame = self.live.last_frame.lock().unwrap();
        let frame = json!({"captureTimestamp": epoch(), "geometryRevision": st.geometry_revision,
            "sourceWidth": w, "sourceHeight": h, "scale": 1.0,
            "contentRect": {"x":0,"y":0,"width":w,"height":h}, "cropRect": {"x":0,"y":0,"width":w,"height":h},
            "frameSequence":sequence,"sessionID":self.session_id,"generation":self.generation});
        // Keep full latest metadata while throttling event emission separately.
        *last_frame = Some(frame);
        drop(last_frame);
        if st.state == "starting" {
            st.state = "capturing";
        }
        if emit {
            let _ =
                self.helper
                    .out
                    .send(self.helper.emit_capture(st, "capture.frameMetadata", None));
        }
    }
    fn run(self) {
        let Err(error) = platform::capture_loop(&self) else {
            return;
        };
        if !self.stop.load(Ordering::SeqCst) {
            let helper = self.helper.clone();
            self.runtime.spawn(async move {
                let mut st = helper.state.lock().await;
                if self.stop.load(Ordering::SeqCst)
                    || st.session_id != self.session_id
                    || st.generation != self.generation
                {
                    return;
                }
                Helper::stop_capture(&mut st);
                let (state, event, reason) = capture_ended(&error);
                st.state = state;
                st.reason = Some(reason.clone());
                helper.revoke(&mut st, &reason).await;
                let _ = helper.out.send(helper.emit_capture(&st, event, None));
            });
        }
    }
}

/// A capture that ends on its own. Only a vanished source is "removed"; any other
/// platform error stays a failure, keeping its code when it is one.
fn capture_ended(error: &str) -> (&'static str, &'static str, String) {
    if error == "source_not_found" {
        return ("sourceRemoved", "capture.sourceRemoved", error.into());
    }
    let code = !error.is_empty()
        && error.len() <= 64
        && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    let reason = if code { error } else { "capture_failed" };
    ("failed", "capture.failed", reason.into())
}

fn map_point(x: f64, y: f64, ox: i32, oy: i32, w: u32, h: u32) -> Result<(i32, i32), String> {
    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) || w == 0 || h == 0 {
        return Err("invalid_input".into());
    }
    Ok((
        ox + (x * (w - 1) as f64).round() as i32,
        oy + (y * (h - 1) as f64).round() as i32,
    ))
}

impl State {
    fn new() -> Self {
        Self {
            session_id: None,
            generation: 0,
            state: "idle",
            reason: None,
            message: None,
            quality: "standard".into(),
            viewer_count: 0,
            source: None,
            geometry_revision: 1,
            peer: None,
            capture_stop: None,
            live: Arc::new(Live::default()),
            deferred: Vec::new(),
            start_peer: false,
            pending_peer: None,
            lease: None,
            lease_expires: Instant::now(),
            consents: Vec::new(),
            prompt: None,
            indicator: None,
            panel_token: 0,
            payloads: Vec::new(),
            last_input_sequence: 0,
        }
    }
}

/// What the daemon hears about the connection to the phone, and whether it
/// ends the view here. A dropped connection is reported but not terminal: it
/// often comes back after a Wi-Fi roam, and the daemon ends the view if not.
fn peer_state_report(state: PeerConnectionState) -> Option<(&'static str, bool)> {
    Some(match state {
        PeerConnectionState::Connecting => ("checking", false),
        PeerConnectionState::Connected => ("connected", false),
        PeerConnectionState::Disconnected => ("disconnected", false),
        PeerConnectionState::Failed => ("failed", true),
        PeerConnectionState::Closed => ("closed", true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_connection_is_reported_without_ending_the_view() {
        assert_eq!(
            peer_state_report(PeerConnectionState::Disconnected),
            Some(("disconnected", false))
        );
        assert_eq!(
            peer_state_report(PeerConnectionState::Connected),
            Some(("connected", false))
        );
        assert_eq!(
            peer_state_report(PeerConnectionState::Failed),
            Some(("failed", true))
        );
        assert_eq!(
            peer_state_report(PeerConnectionState::Closed),
            Some(("closed", true))
        );
        assert_eq!(peer_state_report(PeerConnectionState::New), None);
    }
    #[test]
    fn frame_rate_follows_the_quality_and_the_phones_ceiling() {
        let fps = |quality: &str, params: Value| requested_fps(quality, &params);
        assert_eq!(fps("standard", json!({})), Some(15));
        assert_eq!(fps("auto", json!({})), Some(30));
        assert_eq!(fps("high", json!({})), Some(30));
        assert_eq!(
            fps("auto", json!({"videoMaxFramesPerSecond": 15})),
            Some(15)
        );
        assert_eq!(
            fps("standard", json!({"videoMaxFramesPerSecond": 30})),
            Some(30)
        );
        assert_eq!(
            fps("high", json!({"videoMaxFramesPerSecond": 120})),
            Some(60)
        );
        assert_eq!(
            fps("high", json!({"videoMaxFramesPerSecond": null})),
            Some(30)
        );
        assert_eq!(fps("high", json!({"videoMaxFramesPerSecond": "60"})), None);
        assert_eq!(fps("bogus", json!({})), None);
    }
    fn host_with(local: LocalApproval) -> (Arc<Helper>, mpsc::Receiver<(Value, usize)>) {
        let (tx, rx) = mpsc::channel(8);
        let (execution, _) = tokio::sync::watch::channel(0);
        let helper = Arc::new(Helper {
            factory: PeerConnectionFactory::default(),
            out: Out {
                tx,
                failed: Arc::new(tokio::sync::Notify::new()),
                bytes: Arc::new(AtomicUsize::new(0)),
            },
            instance: instance_id(),
            local,
            state: Mutex::new(State::new()),
            input: Mutex::new(InputState {
                source: None,
                held_buttons: BTreeSet::new(),
                held_keys: BTreeSet::new(),
                execution: execution.subscribe(),
                generation: 0,
                deadline: None,
                #[cfg(all(test, target_os = "linux"))]
                command: None,
            }),
            execution,
            batches: Mutex::new(()),
        });
        (helper, rx)
    }
    pub(super) fn host() -> Arc<Helper> {
        host_with(LocalApproval::NotNeeded).0
    }
    /// A panel the fake screen showed: its text, the person's buttons and
    /// whether the helper has closed it.
    struct Shown {
        text: &'static str,
        press: Option<tokio::sync::oneshot::Sender<usize>>,
        closed: Arc<AtomicBool>,
    }
    struct Closed(Arc<AtomicBool>);
    impl Drop for Closed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[derive(Clone, Default)]
    struct Screen {
        shown: Arc<std::sync::Mutex<Vec<Shown>>>,
        /// Panels the screen can still show; `None` is unlimited.
        room: Arc<std::sync::Mutex<Option<usize>>>,
    }
    impl Screen {
        fn approval(&self) -> LocalApproval {
            let screen = self.clone();
            LocalApproval::Ask(Arc::new(move |text, _buttons| {
                if let Some(room) = screen.room.lock().unwrap().as_mut() {
                    *room = room.checked_sub(1)?;
                }
                let (press, choice) = tokio::sync::oneshot::channel();
                let closed = Arc::new(AtomicBool::new(false));
                screen.shown.lock().unwrap().push(Shown {
                    text,
                    press: Some(press),
                    closed: closed.clone(),
                });
                Some(Panel {
                    choice,
                    window: Box::new(Closed(closed)),
                })
            }))
        }
        fn press(&self, index: usize, button: usize) {
            let press = self.shown.lock().unwrap()[index].press.take().unwrap();
            let _ = press.send(button);
        }
        fn closed(&self, index: usize) -> bool {
            self.shown.lock().unwrap()[index]
                .closed
                .load(Ordering::SeqCst)
        }
        fn texts(&self) -> Vec<&'static str> {
            self.shown.lock().unwrap().iter().map(|s| s.text).collect()
        }
    }
    fn windows_host() -> (Arc<Helper>, Screen, mpsc::Receiver<(Value, usize)>) {
        let screen = Screen::default();
        let (helper, rx) = host_with(screen.approval());
        (helper, screen, rx)
    }
    async fn consent(host: &Arc<Helper>, params: &Value) -> Result<Value, String> {
        match host.consent(params).await? {
            Reply::Now(value) => Ok(value),
            Reply::Later(answer) => Ok(answer.await),
        }
    }
    /// Starts a consent that waits for the person at the computer.
    async fn ask(host: &Arc<Helper>) -> tokio::task::JoinHandle<Value> {
        match host.consent(&identity()).await.unwrap() {
            Reply::Later(answer) => tokio::spawn(answer),
            Reply::Now(value) => panic!("answered without asking: {value}"),
        }
    }
    fn host_with_output() -> (Arc<Helper>, mpsc::Receiver<(Value, usize)>) {
        host_with(LocalApproval::NotNeeded)
    }
    fn signals(rx: &mut mpsc::Receiver<(Value, usize)>) -> Vec<Value> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|(value, _)| value)
            .filter(|value| {
                value["event"]
                    .as_str()
                    .is_some_and(|e| e.starts_with("signal."))
            })
            .collect()
    }
    fn identity() -> Value {
        json!({"sessionID":"fixture","generation":1,"sourceID":"app:editor#41","geometryRevision":1,"leaseID":"lease","requestID":"request"})
    }
    async fn live(host: &Helper) {
        let mut st = host.state.lock().await;
        st.session_id = Some("fixture".into());
        st.generation = 1;
        st.state = "capturing";
        st.source = Some(json!({"id":"app:editor#41","kind":"window","width":800,"height":600}));
    }
    async fn activate(host: &Arc<Helper>) {
        consent(host, &identity()).await.unwrap();
        assert_eq!(
            host.handle("control.activate", &identity()).await.unwrap()["activated"],
            true
        );
    }
    fn input(sequence: i64, actions: Value) -> Value {
        let mut p = identity();
        p["sequence"] = json!(sequence);
        p["actions"] = actions;
        p
    }

    #[test]
    fn quality_bounds_even_pixels_without_changing_source_geometry() {
        for (quality, expected) in [
            ("standard", (1280, 720)),
            ("medium", (1920, 1080)),
            ("auto", (2560, 1440)),
            ("high", (2560, 1440)),
        ] {
            assert_eq!(encoded_size(3840, 2160, quality_index(quality)), expected);
        }
        assert_eq!(encoded_size(600, 800, 0), (540, 720));
        assert_eq!(encoded_size(641, 481, 0), (640, 480));
    }

    #[tokio::test]
    async fn first_frame_survives_control_state_contention() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        let mut st = h.state.lock().await;
        st.state = "starting";
        let job = CaptureJob {
            helper: h.clone(),
            runtime: tokio::runtime::Handle::current(),
            source: NativeVideoSource::new(
                VideoResolution {
                    width: 800,
                    height: 600,
                },
                false,
            ),
            live: st.live.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            selected_source: st.source.clone().unwrap(),
            session_id: st.session_id.clone(),
            generation: st.generation,
        };
        // A static Windows window may produce just this frame. A concurrent
        // capture/control request must not discard its readiness publication.
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                job.submit(I420Buffer::new(800, 600));
            }),
        )
        .await
        .unwrap()
        .unwrap();
        drop(st);
        tokio::time::timeout(Duration::from_secs(10), async {
            while h.handle("capture.status", &json!({})).await.unwrap()["state"] != "capturing" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the submitted frame must make capture live without another frame");
        assert_eq!(
            h.handle("control.input", &input(1, json!([{"type":"releaseAll"}])))
                .await
                .unwrap()["accepted"],
            true
        );
    }

    #[tokio::test]
    async fn video_quality_changes_encoded_frames_without_revising_physical_control_geometry() {
        use futures::StreamExt;
        use libwebrtc::video_stream::native::NativeVideoStream;
        let h = host();
        live(&h).await;
        activate(&h).await;
        let live = {
            let mut st = h.state.lock().await;
            st.source.as_mut().unwrap()["width"] = json!(3840);
            st.source.as_mut().unwrap()["height"] = json!(2160);
            st.live.clone()
        };
        let source = NativeVideoSource::new(
            VideoResolution {
                width: 3840,
                height: 2160,
            },
            false,
        );
        let track = h
            .factory
            .create_video_track("quality-fixture", source.clone());
        let mut frames = NativeVideoStream::new(track);
        let job = CaptureJob {
            helper: h.clone(),
            runtime: tokio::runtime::Handle::current(),
            source,
            live,
            stop: Arc::new(AtomicBool::new(false)),
            selected_source: json!({}),
            session_id: Some("fixture".into()),
            generation: 1,
        };
        for (quality, expected) in [("high", (2560, 1440)), ("standard", (1280, 720))] {
            h.handle(
                "capture.setQuality",
                &json!({"sessionID":"fixture","generation":1,"videoQuality":quality}),
            )
            .await
            .unwrap();
            let frame_job = job.clone();
            tokio::task::spawn_blocking(move || frame_job.submit(I420Buffer::new(3840, 2160)))
                .await
                .unwrap();
            let frame = tokio::time::timeout(Duration::from_secs(10), frames.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!((frame.buffer.width(), frame.buffer.height()), expected);
            let st = h.state.lock().await;
            assert_eq!(st.geometry_revision, 1);
            assert!(st.lease.is_some());
            assert_eq!(
                (
                    st.source.as_ref().unwrap()["width"].as_u64(),
                    st.source.as_ref().unwrap()["height"].as_u64()
                ),
                (Some(3840), Some(2160))
            );
            let metadata = st.live.last_frame.lock().unwrap();
            let metadata = metadata.as_ref().unwrap();
            assert_eq!(metadata["scale"], json!(1.0));
            assert_eq!(
                (
                    metadata["sourceWidth"].as_u64(),
                    metadata["sourceHeight"].as_u64()
                ),
                (Some(3840), Some(2160))
            );
        }
        frames.close();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn slow_input_keeps_heartbeat_and_frames_live_and_release_stops_later_effects() {
        use std::os::unix::fs::PermissionsExt;
        let h = host();
        live(&h).await;
        activate(&h).await;
        h.state.lock().await.source.as_mut().unwrap()["kind"] = json!("display");
        let folder = std::env::temp_dir().join(format!("wonder-input-{}", revision()));
        std::fs::create_dir_all(&folder).unwrap();
        let program = folder.join("input");
        let started = folder.join("started");
        let effect = folder.join("effect");
        std::fs::write(&program,format!("#!/usr/bin/python3\nimport time,pathlib\npathlib.Path({:?}).touch()\ntime.sleep(2.5)\npathlib.Path({:?}).touch()\n",started.to_string_lossy(),effect.to_string_lossy())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        h.input.lock().await.command = Some(program.to_string_lossy().into_owned());
        let task = {
            let h = h.clone();
            tokio::spawn(async move {
                h.handle("control.input",&input(1,json!([{"type":"text","text":"slow"},{"type":"text","text":"must not run"}]))).await
            })
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let heartbeat = tokio::time::timeout(
            Duration::from_secs(10),
            h.handle("control.heartbeat", &identity()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(heartbeat["renewed"], true);
        let live = h.state.lock().await.live.clone();
        let source = NativeVideoSource::new(
            VideoResolution {
                width: 800,
                height: 600,
            },
            false,
        );
        let job = CaptureJob {
            helper: h.clone(),
            runtime: tokio::runtime::Handle::current(),
            source,
            live: live.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            selected_source: json!({}),
            session_id: Some("fixture".into()),
            generation: 1,
        };
        tokio::task::spawn_blocking(move || job.submit(I420Buffer::new(800, 600)))
            .await
            .unwrap();
        assert!(live.sequence.load(Ordering::SeqCst) > 0);
        assert!(!task.is_finished());
        let released = tokio::time::timeout(
            Duration::from_secs(10),
            h.handle("control.release", &identity()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(released["released"], true);
        assert_eq!(task.await.unwrap().unwrap_err(), "lease_not_active");
        assert!(
            !effect.exists(),
            "the cancelled command must be killed and drained"
        );
        assert!(h.state.lock().await.payloads.is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn normalized_points_follow_current_origin_and_reject_invalid_coordinates() {
        assert_eq!(
            map_point(0.25, 0.5, -800, 120, 641, 481).unwrap(),
            (-640, 360)
        );
        assert_eq!(map_point(1.0, 1.0, 500, 200, 100, 50).unwrap(), (599, 249));
        for (x, y) in [
            (f64::NAN, 0.5),
            (1.01, 0.5),
            (0.5, -0.1),
            (f64::INFINITY, 0.0),
        ] {
            assert!(map_point(x, y, 0, 0, 800, 600).is_err());
        }
        assert!(map_point(0.0, 0.0, 0, 0, 0, 600).is_err());
    }
    #[tokio::test]
    async fn consent_is_identity_bound_replayed_and_bounded() {
        let h = host();
        live(&h).await;
        assert_eq!(consent(&h, &identity()).await.unwrap()["replayed"], false);
        assert_eq!(consent(&h, &identity()).await.unwrap()["replayed"], true);
        let mut conflict = identity();
        conflict["leaseID"] = json!("other");
        assert_eq!(
            consent(&h, &conflict).await.unwrap_err(),
            "request_identity_conflict"
        );
        for i in 0..70 {
            let mut p = identity();
            p["requestID"] = json!(format!("r{i}"));
            consent(&h, &p).await.unwrap();
        }
        assert_eq!(h.state.lock().await.consents.len(), 64);
    }
    #[tokio::test]
    async fn activation_requires_consent_and_preserves_an_existing_lease() {
        let h = host();
        live(&h).await;
        assert_eq!(
            h.handle("control.activate", &identity()).await.unwrap()["reason"],
            "consent_required"
        );
        activate(&h).await;
        assert_eq!(
            h.handle("control.activate", &identity()).await.unwrap()["replayed"],
            true
        );
        let mut other = identity();
        other["leaseID"] = json!("other");
        assert_eq!(
            h.handle("control.activate", &other).await.unwrap()["reason"],
            "control_busy"
        );
        assert_eq!(h.state.lock().await.lease, Some(identity()));
    }
    #[tokio::test]
    async fn input_receipts_reject_replay_changes_and_sequence_gaps() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        let p = input(1, json!([{"type":"releaseAll"}]));
        assert_eq!(
            h.handle("control.input", &p).await.unwrap()["duplicate"],
            false
        );
        assert_eq!(
            h.handle("control.input", &p).await.unwrap()["duplicate"],
            true
        );
        let changed = input(1, json!([{"type":"releaseAll"},{"type":"releaseAll"}]));
        assert_eq!(
            h.handle("control.input", &changed).await.unwrap()["reason"],
            "sequence_payload_conflict"
        );
        assert_eq!(
            h.handle("control.input", &input(3, json!([{"type":"releaseAll"}])))
                .await
                .unwrap()["reason"],
            "input_sequence_must_increase_by_one"
        );
        assert_eq!(h.state.lock().await.last_input_sequence, 1);
    }
    #[tokio::test]
    async fn stale_capture_identity_cannot_heartbeat_or_deliver() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        for field in ["sessionID", "sourceID", "leaseID", "requestID"] {
            let mut p = identity();
            p[field] = json!("stale");
            let result = h.handle("control.heartbeat", &p).await;
            assert!(result.is_err() || result.unwrap()["renewed"] == false);
        }
        let mut p = identity();
        p["generation"] = json!(2);
        assert_eq!(
            h.handle("control.heartbeat", &p).await.unwrap_err(),
            "invalid_capture_session"
        );
        let mut p = identity();
        p["geometryRevision"] = json!(2);
        assert_eq!(
            h.handle("control.heartbeat", &p).await.unwrap_err(),
            "invalid_capture_session"
        );
    }
    #[tokio::test]
    async fn invalid_input_batches_and_partial_failure_do_not_create_receipts() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        for actions in [json!([]), json!(vec![json!({"type":"releaseAll"}); 33])] {
            assert_eq!(
                h.handle("control.input", &input(1, actions))
                    .await
                    .unwrap_err(),
                "invalid_input"
            );
        }
        assert!(h
            .handle(
                "control.input",
                &input(1, json!([{"type":"releaseAll"},{"type":"unknown"}]))
            )
            .await
            .is_err());
        let st = h.state.lock().await;
        assert!(st.lease.is_none());
        assert!(st.payloads.is_empty());
    }
    #[tokio::test]
    async fn expired_lease_is_revoked_before_any_request() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        h.state.lock().await.lease_expires = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            h.handle("control.heartbeat", &identity()).await.unwrap()["renewed"],
            false
        );
        assert!(h.state.lock().await.lease.is_none());
    }
    #[tokio::test]
    async fn switching_rebinds_consent_but_preserves_sequence_and_receipts() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        h.handle("control.input", &input(1, json!([{"type":"releaseAll"}])))
            .await
            .unwrap();
        let old_payloads = h.state.lock().await.payloads.clone();
        {
            let mut st = h.state.lock().await;
            st.rebind_source(
                json!({"id":"app:editor#42","kind":"window","width":900,"height":700}),
            );
            assert_eq!(st.last_input_sequence, 1);
            assert_eq!(st.payloads, old_payloads);
            assert_eq!(st.geometry_revision, 2);
            assert_eq!(st.consents[0].1, st.lease.clone().unwrap());
        }
        assert_eq!(
            h.handle("control.heartbeat", &identity())
                .await
                .unwrap_err(),
            "invalid_capture_session"
        );
        let mut next = identity();
        next["sourceID"] = json!("app:editor#42");
        next["geometryRevision"] = json!(2);
        assert_eq!(
            h.handle("control.heartbeat", &next).await.unwrap()["renewed"],
            true
        );
        next["sequence"] = json!(1);
        next["actions"] = json!([{"type":"releaseAll"}]);
        assert_eq!(
            h.handle("control.input", &next).await.unwrap()["duplicate"],
            true
        );
    }
    #[tokio::test]
    async fn prepare_and_stop_cancel_previous_capture_and_control() {
        let h = host();
        live(&h).await;
        activate(&h).await;
        let flag = Arc::new(AtomicBool::new(false));
        h.state.lock().await.capture_stop = Some(flag.clone());
        h.handle(
            "capture.prepare",
            &json!({"sessionID":"new","generation":2,"videoQuality":"high"}),
        )
        .await
        .unwrap();
        assert!(flag.load(Ordering::SeqCst));
        let st = h.state.lock().await;
        assert!(st.lease.is_none());
        assert!(st.consents.is_empty());
        assert!(st.source.is_none());
        drop(st);
        h.handle("capture.stop", &json!({"sessionID":"new","generation":2}))
            .await
            .unwrap();
        assert_eq!(h.state.lock().await.state, "stopped");
    }
    #[tokio::test]
    async fn peer_setup_names_its_peer_and_a_failure_retires_capture() {
        let (h, mut rx) = host_with_output();
        h.handle(
            "capture.prepare",
            &json!({"sessionID":"fixture","generation":1,"videoQuality":"standard"}),
        )
        .await
        .unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        h.state.lock().await.capture_stop = Some(flag.clone());
        h.after_response().await;
        let events = signals(&mut rx);
        assert!(!events.is_empty());
        // The host drops a signal without the peer revision as a protocol
        // error, losing the reason.
        assert!(events.iter().all(|event| event["peerRevision"]
            .as_str()
            .is_some_and(|rev| !rev.is_empty())));
        if let Some(failed) = events
            .iter()
            .find(|event| event["event"] == "signal.failed")
        {
            // A machine without a display (CI, a headless VM) cannot set up.
            assert_eq!(failed["reason"], "no_display");
            let st = h.state.lock().await;
            assert_eq!(
                (st.state, st.reason.as_deref()),
                ("failed", Some("no_display"))
            );
            assert!(flag.load(Ordering::SeqCst));
            assert!(st.peer.is_none() && st.pending_peer.is_none());
        }

        // A setup that outlives its deadline says so under the same revision.
        // With a display the first peer was set up, and its ICE candidate and
        // state callbacks keep arriving asynchronously; only the failure
        // report belongs to this step.
        h.state.lock().await.pending_peer = Some("rev-timeout".into());
        let flag = Arc::new(AtomicBool::new(false));
        h.state.lock().await.capture_stop = Some(flag.clone());
        h.peer_setup_timed_out().await;
        let failures: Vec<_> = signals(&mut rx)
            .into_iter()
            .filter(|event| event["event"] == "signal.failed")
            .collect();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0]["peerRevision"], "rev-timeout");
        assert_eq!(failures[0]["reason"], "peer_setup_timeout");
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(h.state.lock().await.state, "failed");
    }
    #[tokio::test]
    async fn picking_a_window_that_is_gone_fails_the_capture() {
        let h = host();
        h.handle(
            "capture.prepare",
            &json!({"sessionID":"fixture","generation":1,"videoQuality":"standard"}),
        )
        .await
        .unwrap();
        let result = h
            .handle(
                "capture.pick",
                &json!({"sessionID":"fixture","generation":1,"sourceID":"app:gone#2147483646"}),
            )
            .await
            .unwrap();
        assert_eq!(result["accepted"], false);
        assert_eq!(result["errorCode"], "source_not_found");
        assert_eq!(result["status"]["state"], "failed");
        assert_eq!(result["status"]["reason"], "source_not_found");
    }
    #[tokio::test]
    async fn only_the_current_peer_can_apply_answers() {
        let h = host();
        live(&h).await;
        assert!(h
            .handle("signal.answer", &json!({"peerRevision":"stale","sdp":""}))
            .await
            .is_err());
        // Missing revision must not match an absent peer.
        assert!(h.handle("signal.answer", &json!({"sdp":""})).await.is_err());
    }
    #[test]
    fn only_a_vanished_source_ends_capture_as_removed() {
        assert_eq!(
            capture_ended("source_not_found"),
            (
                "sourceRemoved",
                "capture.sourceRemoved",
                "source_not_found".into()
            )
        );
        // A capture Windows refuses to start (e.g. an older Windows 10) is a failure, not a closed window.
        assert_eq!(
            capture_ended("capture_unavailable"),
            ("failed", "capture.failed", "capture_unavailable".into())
        );
        // Platform error text is never forwarded as if it were words for the phone.
        for raw in ["X11 error: BadWindow", "", &"a".repeat(65)] {
            assert_eq!(
                capture_ended(raw),
                ("failed", "capture.failed", "capture_failed".into())
            );
        }
    }
    fn editor() -> WindowTraits {
        WindowTraits {
            visible: true,
            minimized: false,
            cloaked: false,
            owned: false,
            class: "Notepad".into(),
            ex_style: 0,
            transparent: false,
            exe: "notepad.exe".into(),
            title: "notes.txt - Notepad".into(),
        }
    }
    #[test]
    fn a_minimized_selected_window_stays_shareable_but_is_not_offered() {
        let mut t = editor();
        assert!(shareable_window(&t, false) && shareable_window(&t, true));
        t.minimized = true;
        assert!(!shareable_window(&t, false));
        assert!(shareable_window(&t, true));
    }
    #[test]
    fn selected_windows_keep_the_discovery_isolation_rules() {
        type Change = fn(&mut WindowTraits);
        let changes: [Change; 9] = [
            |t| t.visible = false,
            |t| t.cloaked = true,
            |t| t.owned = true,
            |t| t.class = "Shell_TrayWnd".into(),
            |t| t.transparent = true,
            |t| t.ex_style = 0x80,
            |t| t.title.clear(),
            |t| t.exe = "bad name.exe".into(),
            |t| t.exe = "a".repeat(151),
        ];
        for change in changes {
            let mut t = editor();
            t.minimized = true;
            change(&mut t);
            assert!(!shareable_window(&t, true) && !shareable_window(&t, false));
        }
    }
    #[test]
    fn a_locked_or_uac_screen_uses_the_reason_the_picker_recognises() {
        assert_eq!(
            secure_desktop_capability(true),
            json!({"available":false,"reason":"secure_desktop"})
        );
        assert_eq!(secure_desktop_capability(false), json!({"available":true}));
    }
    #[tokio::test]
    async fn windows_grants_control_only_after_allow_on_the_computer() {
        let (h, screen, _rx) = windows_host();
        live(&h).await;
        let answer = ask(&h).await;
        assert_eq!(screen.texts(), [ASK_TEXT]);
        // The question does not hold the helper: other requests still answer.
        let busy = tokio::time::timeout(
            Duration::from_secs(10),
            h.handle("control.activate", &identity()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(busy["reason"], "consent_required");
        assert_eq!(
            consent(&h, &identity()).await.unwrap()["reason"],
            "control_busy"
        );
        screen.press(0, 0);
        let answer = answer.await.unwrap();
        assert_eq!(answer["granted"], true);
        assert!(screen.closed(0));
        assert_eq!(
            h.handle("control.activate", &identity()).await.unwrap()["activated"],
            true
        );
        assert_eq!(screen.texts(), [ASK_TEXT, CONTROLLED_TEXT]);
        assert!(!screen.closed(1));
        // A replayed request was already allowed; it does not ask again.
        assert_eq!(consent(&h, &identity()).await.unwrap()["replayed"], true);
        assert_eq!(screen.texts().len(), 2);
    }
    #[tokio::test]
    async fn windows_refuses_when_declined_closed_or_unanswered() {
        for (button, reason) in [(Some(1), "consent_denied"), (None, "consent_denied")] {
            let (h, screen, _rx) = windows_host();
            live(&h).await;
            let answer = ask(&h).await;
            match button {
                Some(button) => screen.press(0, button),
                // The window went away without a choice.
                None => drop(screen.shown.lock().unwrap()[0].press.take()),
            }
            let answer = answer.await.unwrap();
            assert_eq!(
                (answer["granted"].clone(), answer["reason"].clone()),
                (json!(false), json!(reason))
            );
            assert_eq!(
                h.handle("control.activate", &identity()).await.unwrap()["reason"],
                "consent_required"
            );
        }
    }
    #[tokio::test(start_paused = true)]
    async fn windows_closes_an_unanswered_question_before_wonderd_gives_up() {
        let (h, screen, _rx) = windows_host();
        live(&h).await;
        let answer = ask(&h).await;
        tokio::time::advance(Duration::from_secs(ASK_SECONDS + 1)).await;
        assert_eq!(answer.await.unwrap()["reason"], "consent_timed_out");
        assert!(screen.closed(0));
        assert!(ASK_SECONDS < 120, "wonderd waits two minutes for consent");
    }
    #[tokio::test]
    async fn release_stop_and_a_new_view_close_the_question_and_deny() {
        for (method, params) in [
            ("control.release", identity()),
            ("control.releaseAll", identity()),
            (
                "capture.stop",
                json!({"sessionID":"fixture","generation":1}),
            ),
            ("capture.prepare", json!({"sessionID":"new","generation":2})),
        ] {
            let (h, screen, _rx) = windows_host();
            live(&h).await;
            let answer = ask(&h).await;
            let result = tokio::time::timeout(Duration::from_secs(10), h.handle(method, &params))
                .await
                .unwrap()
                .unwrap();
            if method == "control.release" {
                assert_eq!(result["released"], true);
            }
            assert!(screen.closed(0), "{method} left the question open");
            assert_eq!(
                answer.await.unwrap()["reason"],
                "consent_cancelled",
                "{method}"
            );
            assert!(h.state.lock().await.consents.is_empty(), "{method}");
        }
    }
    #[tokio::test]
    async fn stop_on_the_computer_revokes_control() {
        let (h, screen, mut rx) = windows_host();
        live(&h).await;
        let answer = ask(&h).await;
        screen.press(0, 0);
        answer.await.unwrap();
        h.handle("control.activate", &identity()).await.unwrap();
        screen.press(1, 0);
        let (event, _) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event["event"], "control.revoked");
        assert_eq!(event["reason"], "local_stop");
        assert_eq!(event["leaseID"], "lease");
        assert!(screen.closed(1));
        // Stop withdraws the approval: replaying the same request asks again.
        assert_eq!(
            h.handle("control.activate", &identity()).await.unwrap()["reason"],
            "consent_required"
        );
        assert!(h.state.lock().await.lease.is_none());
        let again = ask(&h).await;
        assert_eq!(screen.texts(), [ASK_TEXT, CONTROLLED_TEXT, ASK_TEXT]);
        screen.press(2, 1);
        assert_eq!(again.await.unwrap()["reason"], "consent_denied");
        let mut p = identity();
        p["sequence"] = json!(1);
        p["actions"] = json!([{"type":"releaseAll"}]);
        assert_eq!(
            h.handle("control.input", &p).await.unwrap()["reason"],
            "lease_not_active"
        );
        assert_eq!(
            h.handle("control.heartbeat", &identity()).await.unwrap()["renewed"],
            false
        );
    }
    #[tokio::test]
    async fn ending_control_from_the_phone_removes_the_notice() {
        let (h, screen, _rx) = windows_host();
        live(&h).await;
        let answer = ask(&h).await;
        screen.press(0, 0);
        answer.await.unwrap();
        h.handle("control.activate", &identity()).await.unwrap();
        h.handle("control.release", &identity()).await.unwrap();
        assert!(screen.closed(1));
        assert!(h.state.lock().await.indicator.is_none());
    }
    #[tokio::test]
    async fn windows_without_a_prompt_or_notice_refuses_control() {
        let (h, screen, _rx) = windows_host();
        live(&h).await;
        *screen.room.lock().unwrap() = Some(0);
        assert_eq!(
            consent(&h, &identity()).await.unwrap()["reason"],
            "local_approval_unavailable"
        );
        // Allowed, but the notice cannot be shown: control stays off.
        *screen.room.lock().unwrap() = Some(1);
        let answer = ask(&h).await;
        screen.press(0, 0);
        assert_eq!(answer.await.unwrap()["granted"], true);
        let activation = h.handle("control.activate", &identity()).await.unwrap();
        assert_eq!(activation["reason"], "local_approval_unavailable");
        assert!(h.state.lock().await.lease.is_none());
    }
    #[tokio::test]
    async fn stalled_output_requests_shutdown_and_stays_bounded() {
        let (tx, _rx) = mpsc::channel(1);
        let failed = Arc::new(tokio::sync::Notify::new());
        let out = Out {
            tx,
            failed: failed.clone(),
            bytes: Arc::new(AtomicUsize::new(0)),
        };
        assert!(out.send(json!({"one":1})).is_ok());
        assert!(out.send(json!({"two":2})).is_err());
        tokio::time::timeout(Duration::from_millis(100), failed.notified())
            .await
            .unwrap();
    }
}

/// A fixed 20px PNG with an uncompressed zlib block stays below the wire cap.
/// Native icons are data, never window thumbnails or executable image loaders.
fn icon_png(rgba: &[u8]) -> Option<String> {
    use base64::Engine;
    if rgba.len() != 20 * 20 * 4 {
        return None;
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        out.extend(kind);
        out.extend(data);
        let mut crc = !0u32;
        for b in kind.iter().chain(data) {
            crc ^= *b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
            }
        }
        out.extend((!crc).to_be_bytes());
    }
    let mut raw = Vec::with_capacity(1620);
    for row in rgba.chunks_exact(80) {
        raw.push(0);
        raw.extend(row);
    }
    let mut z = vec![0x78, 0x01, 0x01];
    let n = raw.len() as u16;
    z.extend(n.to_le_bytes());
    z.extend((!n).to_le_bytes());
    z.extend(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for byte in raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::from(20u32.to_be_bytes());
    header.extend(20u32.to_be_bytes());
    header.extend([8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    Some(base64::engine::general_purpose::STANDARD.encode(png))
}

/// What Windows reports about a top-level window when deciding whether it can be shared.
#[cfg(any(target_os = "windows", test))]
struct WindowTraits {
    visible: bool,
    minimized: bool,
    cloaked: bool,
    owned: bool,
    class: String,
    ex_style: u32,
    /// Layered and fully transparent.
    transparent: bool,
    exe: String,
    title: String,
}

/// Discovery offers only windows on screen. A selected window stays itself while
/// minimized, so control restores it instead of treating it as closed.
#[cfg(any(target_os = "windows", test))]
fn shareable_window(t: &WindowTraits, selected: bool) -> bool {
    const SHELL: [&str; 7] = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "#32768",
        "SysShadow",
        "tooltips_class32",
    ];
    // Click-through, tool and no-activate windows are never sources.
    t.visible
        && (selected || !t.minimized)
        && !t.cloaked
        && !t.owned
        && !SHELL.contains(&t.class.as_str())
        && !t.transparent
        && t.ex_style & (0x20 | 0x80 | 0x0800_0000) == 0
        && !t.title.is_empty()
        && t.exe.len() <= 150
        && t.exe
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A locked or UAC screen hides every source. `secure_desktop` is the reason the
/// host's picker recognises; input refusals say `input_desktop_unavailable`.
#[cfg(any(target_os = "windows", test))]
fn secure_desktop_capability(secure: bool) -> Value {
    if secure {
        json!({"available":false,"reason":"secure_desktop"})
    } else {
        json!({"available":true})
    }
}

#[cfg(target_os = "windows")]
fn bound_title(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut end = clean.len().min(160);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean[..end].trim().to_owned()
}
