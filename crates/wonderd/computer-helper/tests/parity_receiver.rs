//! Parity receiver: a libwebrtc answerer standing in for the phone (iOS also
//! decodes with libwebrtc). It answers whichever helper's offer the driver
//! relays, decodes every frame through a video sink and reports counters.
//!
//! Test-only executable (feature parity-tests); never packaged.
//! stdin/stdout: JSON lines {"offer": sdp} / {"candidate": {...}} in,
//! {"answer": sdp} / {"candidate": {...}} out. Metrics: stderr `BENCH {json}`.

use std::{
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use libwebrtc::{
    peer_connection::TrackEvent,
    prelude::*,
    stats::{IceCandidatePairState, RtcStats},
    video_stream::native::NativeVideoStream,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

fn epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn bench(value: Value) {
    let _ = writeln!(std::io::stderr().lock(), "BENCH {value}");
}

fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let factory = PeerConnectionFactory::default();
    let mut config = RtcConfiguration::default();
    config.continual_gathering_policy = ContinualGatheringPolicy::GatherOnce;
    let pc = factory
        .create_peer_connection(config)
        .expect("peer connection");
    let sunk = Arc::new(AtomicU64::new(0));
    let visible = Arc::new(AtomicU64::new(0));

    pc.on_ice_candidate(Some(Box::new(|c: IceCandidate| {
        let text = c.candidate();
        let text = text.strip_prefix("a=").unwrap_or(&text).to_string();
        emit(
            json!({"candidate": {"candidate": text, "sdpMid": c.sdp_mid(),
                                  "sdpMLineIndex": c.sdp_mline_index()}}),
        );
    })));
    pc.on_connection_state_change(Some(Box::new(|s| {
        bench(json!({"k": "peer", "state": format!("{s:?}"), "t": epoch()}));
    })));
    {
        let sunk = sunk.clone();
        let visible = visible.clone();
        let handle = tokio::runtime::Handle::current();
        pc.on_track(Some(Box::new(move |event: TrackEvent| {
            let MediaStreamTrack::Video(track) = event.track else {
                return;
            };
            let sunk = sunk.clone();
            let visible = visible.clone();
            handle.spawn(async move {
                let mut stream = NativeVideoStream::new(track);
                let mut size = (0, 0);
                let mut content_seen = false;
                let mut previous_hash = None;
                let dump = std::env::var("BAKEOFF_DUMP").ok();
                let dump_at = std::env::var("BAKEOFF_DUMP_AT")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(150);
                while let Some(frame) = stream.next().await {
                    let n = sunk.fetch_add(1, Ordering::SeqCst);
                    // Hardware decoders (VideoToolbox) hand back native buffers;
                    // read their pixels through an I420 copy.
                    let converted;
                    let planar = match frame.buffer.as_i420() {
                        Some(buffer) => Some(buffer),
                        None => {
                            converted = frame.buffer.to_i420();
                            Some(&converted)
                        }
                    };
                    if let Some(buffer) = planar {
                        let hash = buffer
                            .data()
                            .0
                            .iter()
                            .step_by(257)
                            .fold(0xcbf29ce484222325u64, |h, b| {
                                (h ^ *b as u64).wrapping_mul(0x100000001b3)
                            });
                        if previous_hash != Some(hash) {
                            visible.fetch_add(1, Ordering::SeqCst);
                            previous_hash = Some(hash);
                        }
                    }
                    if !content_seen
                        && planar.is_some_and(|buffer| {
                            buffer.data().0.iter().step_by(257).any(|y| *y > 24)
                        })
                    {
                        bench(json!({"k":"content_frame","n":n+1,"t":epoch()}));
                        content_seen = true;
                    }
                    // One decoded frame as raw I420 so the driver can render it to PNG.
                    if n == dump_at {
                        if let (Some(path), Some(i420)) = (&dump, planar) {
                            let (y, u, v) = i420.data();
                            let (sy, su, sv) = i420.strides();
                            let (w, h) = (i420.width() as usize, i420.height() as usize);
                            let mut raw = Vec::with_capacity(w * h * 3 / 2);
                            for r in 0..h {
                                raw.extend_from_slice(&y[r * sy as usize..][..w]);
                            }
                            for r in 0..h.div_ceil(2) {
                                raw.extend_from_slice(&u[r * su as usize..][..w.div_ceil(2)]);
                            }
                            for r in 0..h.div_ceil(2) {
                                raw.extend_from_slice(&v[r * sv as usize..][..w.div_ceil(2)]);
                            }
                            let _ = std::fs::write(path, raw);
                            bench(json!({"k": "dumped", "path": path, "width": w, "height": h}));
                        }
                    }
                    let now = (frame.buffer.width(), frame.buffer.height());
                    if n == 0 || now != size {
                        size = now;
                        bench(json!({"k": "sink_frame", "n": n + 1, "t": epoch(),
                                     "width": now.0, "height": now.1}));
                    }
                }
            });
        })));
    }
    {
        let pc = pc.clone();
        let sunk = sunk.clone();
        let visible = visible.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Ok(stats) = pc.get_stats().await else {
                    continue;
                };
                // The nominated pair says which route carried the picture
                // (host/srflx/relay, protocol, addresses) for the soak harness.
                for s in &stats {
                    let RtcStats::CandidatePair(p) = s else {
                        continue;
                    };
                    if !p.candidate_pair.nominated
                        || p.candidate_pair.state != Some(IceCandidatePairState::Succeeded)
                    {
                        continue;
                    }
                    let local = stats.iter().find_map(|c| match c {
                        RtcStats::LocalCandidate(c)
                            if c.rtc.id == p.candidate_pair.local_candidate_id =>
                        {
                            Some(&c.local_candidate)
                        }
                        _ => None,
                    });
                    let remote = stats.iter().find_map(|c| match c {
                        RtcStats::RemoteCandidate(c)
                            if c.rtc.id == p.candidate_pair.remote_candidate_id =>
                        {
                            Some(&c.remote_candidate)
                        }
                        _ => None,
                    });
                    if let (Some(local), Some(remote)) = (local, remote) {
                        bench(json!({"k": "ice_pair", "t": epoch(),
                            "local": format!("{} {} {}:{}", local.protocol,
                                format!("{:?}", local.candidate_type), local.address, local.port),
                            "remote": format!("{} {} {}:{}", remote.protocol,
                                format!("{:?}", remote.candidate_type), remote.address, remote.port),
                            "rtt": p.candidate_pair.current_round_trip_time}));
                    }
                }
                for s in &stats {
                    let RtcStats::InboundRtp(i) = s else { continue };
                    if i.stream.kind != "video" {
                        continue;
                    }
                    let codec = stats.iter().find_map(|c| match c {
                        RtcStats::Codec(c) if c.rtc.id == i.stream.codec_id => {
                            Some(format!("{} {}", c.codec.mime_type, c.codec.sdp_fmtp_line))
                        }
                        _ => None,
                    });
                    bench(json!({"k": "rx_stats", "t": epoch(),
                        "frames_decoded": i.inbound.frames_decoded,
                        "frames_received": i.inbound.frames_received,
                        "frames_dropped": i.inbound.frames_dropped,
                        "key_frames_decoded": i.inbound.key_frames_decoded,
                        "sink_frames": sunk.load(Ordering::SeqCst),
                        "visible_frames": visible.load(Ordering::SeqCst),
                        "bytes_received": i.inbound.bytes_received,
                        "packets_lost": i.received.packets_lost,
                        "packets_received": i.received.packets_received,
                        "freeze_count": i.inbound.freeze_count,
                        "total_decode_time": i.inbound.total_decode_time,
                        "width": i.inbound.frame_width, "height": i.inbound.frame_height,
                        "decoder": i.inbound.decoder_implementation,
                        "power_efficient": i.inbound.power_efficient_decoder,
                        "codec": codec}));
                }
            }
        });
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(raw)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(sdp) = msg.get("offer").and_then(Value::as_str) {
            let offer = SessionDescription::parse(sdp, SdpType::Offer).expect("offer parses");
            if let Err(e) = pc.set_remote_description(offer).await {
                bench(json!({"k": "error", "where": "set_remote", "error": e.to_string()}));
                continue;
            }
            let answer = pc
                .create_answer(AnswerOptions::default())
                .await
                .expect("answer");
            pc.set_local_description(answer.clone())
                .await
                .expect("set local");
            emit(json!({"answer": answer.to_string()}));
        } else if let Some(c) = msg.get("candidate") {
            let text = c
                .get("candidate")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mid = c.get("sdpMid").and_then(Value::as_str).unwrap_or("0");
            let index = c.get("sdpMLineIndex").and_then(Value::as_i64).unwrap_or(0) as i32;
            match IceCandidate::parse(mid, index, text) {
                Ok(c) => {
                    if let Err(e) = pc.add_ice_candidate(c).await {
                        bench(
                            json!({"k": "error", "where": "add_candidate", "error": e.to_string()}),
                        );
                    }
                }
                Err(e) => bench(
                    json!({"k": "error", "where": "parse_candidate", "error": format!("{e:?}")}),
                ),
            }
        } else if msg.get("stop").is_some() {
            break;
        }
    }
    pc.close();
}
