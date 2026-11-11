//! Synthetic loopback with real codec factories, suitable for SSH without a desktop.
//! Repeated creation, decoding, and teardown exercises the optional-driver path.
use futures::StreamExt;
use libwebrtc::{
    peer_connection_factory::native::PeerConnectionFactoryExt,
    prelude::*,
    rtp_sender::VideoEncoderBackend,
    stats::RtcStats,
    video_frame::{I420Buffer, VideoFrame, VideoRotation},
    video_source::native::NativeVideoSource,
    video_stream::native::NativeVideoStream,
};
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

struct Peers(PeerConnection, PeerConnection);
impl Drop for Peers {
    fn drop(&mut self) {
        self.0.on_ice_candidate(None);
        self.1.on_ice_candidate(None);
        self.1.on_track(None);
        self.0.close();
        self.1.close();
    }
}

async fn run(
    codec: &str,
    software: bool,
    require_software: bool,
    mut decoded_ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), String> {
    let factory = PeerConnectionFactory::default();
    let peers = Peers(
        factory
            .create_peer_connection(RtcConfiguration::default())
            .map_err(|e| e.to_string())?,
        factory
            .create_peer_connection(RtcConfiguration::default())
            .map_err(|e| e.to_string())?,
    );
    let (ice_tx, mut ice_rx) = mpsc::unbounded_channel();
    let tx = ice_tx.clone();
    peers.0.on_ice_candidate(Some(Box::new(move |c| {
        let _ = tx.send((true, c));
    })));
    peers.1.on_ice_candidate(Some(Box::new(move |c| {
        let _ = ice_tx.send((false, c));
    })));
    let (tracks, mut track_rx) = mpsc::unbounded_channel();
    peers.1.on_track(Some(Box::new(move |event| {
        if let MediaStreamTrack::Video(track) = event.track {
            let _ = tracks.send(track);
        }
    })));
    let source = NativeVideoSource::new(
        VideoResolution {
            width: 1920,
            height: 1080,
        },
        true,
    );
    let track = factory.create_video_track("encoder-probe", source.clone());
    let transceiver = peers
        .0
        .add_transceiver(
            track.into(),
            RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendOnly,
                stream_ids: vec!["probe".into()],
                send_encodings: vec![RtpEncodingParameters {
                    max_framerate: Some(30.0),
                    max_bitrate: Some(6_000_000),
                    ..Default::default()
                }],
            },
        )
        .map_err(|e| e.to_string())?;
    let formats: Vec<_> = factory
        .get_rtp_sender_capabilities(MediaType::Video)
        .codecs
        .into_iter()
        .filter(|c| {
            c.mime_type.eq_ignore_ascii_case(codec)
                && (codec != "video/H264"
                    || c.sdp_fmtp_line
                        .as_deref()
                        .is_some_and(|f| f.contains("42e0") && f.contains("packetization-mode=1")))
        })
        .collect();
    if formats.is_empty() {
        return Err(format!("{codec} absent from compiled factory"));
    }
    transceiver
        .set_codec_preferences(formats)
        .map_err(|e| e.to_string())?;
    if software {
        transceiver
            .sender()
            .set_video_encoder_backend(VideoEncoderBackend::Software);
    }
    let offer = peers
        .0
        .create_offer(OfferOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    peers
        .0
        .set_local_description(offer.clone())
        .await
        .map_err(|e| e.to_string())?;
    peers
        .1
        .set_remote_description(offer)
        .await
        .map_err(|e| e.to_string())?;
    let answer = peers
        .1
        .create_answer(AnswerOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    peers
        .1
        .set_local_description(answer.clone())
        .await
        .map_err(|e| e.to_string())?;
    peers
        .0
        .set_remote_description(answer)
        .await
        .map_err(|e| e.to_string())?;
    let track = track_rx.recv().await.ok_or("no receiver track")?;
    let mut stream = NativeVideoStream::new(track);
    let mut tick = tokio::time::interval(Duration::from_micros(33_333));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut sent, mut decoded) = (0, 0);
    let mut last_luma = None;
    let mut changed = false;
    while sent < 151 {
        tokio::select! {
            Some((to_receiver, candidate)) = ice_rx.recv() => {
                let pc = if to_receiver { &peers.1 } else { &peers.0 };
                pc.add_ice_candidate(candidate).await.map_err(|e| e.to_string())?;
            }
            _ = tick.tick() => {
                let mut buffer = I420Buffer::new(1920, 1080);
                let (y,u,v) = buffer.data_mut();
                y.fill(48 + (sent % 128) as u8); u.fill(128); v.fill(128);
                source.capture_frame(&VideoFrame { rotation: VideoRotation::VideoRotation0,
                    frame_metadata: None, timestamp_us: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as i64, buffer });
                sent += 1;
            }
            frame = stream.next() => {
                let frame = frame.ok_or("decoder stream ended")?;
                if frame.buffer.width() != 1920 || frame.buffer.height() != 1080 { return Err("unexpected encoded size".into()); }
                let buffer = frame.buffer.to_i420();
                let luma = buffer.data().0[0];
                changed |= last_luma.is_some_and(|old| old != luma);
                last_luma = Some(luma);
                decoded += 1;
                if let Some(ready) = decoded_ready.take() { let _ = ready.send(()); }
            }
        }
    }
    let stats = peers.0.get_stats().await.map_err(|e| e.to_string())?;
    let outbound = stats
        .iter()
        .find_map(|s| match s {
            RtcStats::OutboundRtp(o) if o.stream.kind == "video" => Some(o),
            _ => None,
        })
        .ok_or("no sender stats")?;
    let actual_codec = stats
        .iter()
        .find_map(|s| match s {
            RtcStats::Codec(c) if c.rtc.id == outbound.stream.codec_id => Some(&c.codec),
            _ => None,
        })
        .ok_or("no codec stats")?;
    let result = json!({"requested": codec, "codec": actual_codec.mime_type, "fmtp": actual_codec.sdp_fmtp_line,
        "implementation": outbound.outbound.encoder_implementation, "hardware": outbound.outbound.power_efficient_encoder,
        "captured": sent, "decoded": decoded, "changedPixels": changed,
        "width": outbound.outbound.frame_width, "height": outbound.outbound.frame_height});
    println!("{result}");
    if codec == "video/H264"
        && !actual_codec
            .sdp_fmtp_line
            .contains("profile-level-id=42e033")
    {
        return Err("H264 level 5.1 was not negotiated".into());
    }
    if decoded < 15 || !changed || !actual_codec.mime_type.eq_ignore_ascii_case(codec) {
        return Err("real codec did not decode changing frames".into());
    }
    if require_software && outbound.outbound.power_efficient_encoder {
        return Err("expected software fallback".into());
    }
    drop(stream); // detach the native sink before closing its peer
    drop(peers);
    Ok(())
}

fn resources() -> (usize, usize) {
    #[cfg(target_os = "linux")]
    {
        (
            std::fs::read_dir("/proc/self/fd").unwrap().count(),
            std::fs::read_dir("/proc/self/task").unwrap().count(),
        )
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        let mut count = 0;
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count).unwrap() };
        (count as usize, 0)
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    let software = args.iter().any(|a| a == "--software");
    let require = args.iter().any(|a| a == "--require-software");
    let mut after_close = Vec::new();
    // Warm all codecs once, then check resource counts across repeated native
    // factory/peer lifetimes. On no-GPU Windows this also exercises failed
    // NVENC/MF initialization before the real software decode succeeds.
    for _ in 0..3 {
        for codec in ["video/H264", "video/VP8", "video/H264"] {
            match tokio::time::timeout(Duration::from_secs(20), run(codec, software, require, None))
                .await
            {
                Ok(Ok(())) => {}
                other => {
                    eprintln!("encoder probe failed: {other:?}");
                    std::process::exit(1);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        after_close.push(resources());
    }
    // Cancellation also drops the peer guard and detaches callbacks/sinks.
    let (ready, decoded) = tokio::sync::oneshot::channel();
    let cancelled = tokio::spawn(run("video/H264", software, require, Some(ready)));
    tokio::time::timeout(Duration::from_secs(20), decoded)
        .await
        .expect("cancellation fixture did not decode")
        .expect("cancellation fixture exited");
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(500)).await;
    after_close.push(resources());
    println!(
        "{}",
        json!({"resourcesAfterClose": after_close, "resourceKind": if cfg!(target_os = "windows") {"handles"} else {"fds,threads"}})
    );
    let baseline = after_close[0];
    for current in after_close.iter().skip(1) {
        assert!(
            current.0 <= baseline.0 + 4 && current.1 <= baseline.1 + 2,
            "native resources did not return near the warm baseline: {after_close:?}"
        );
    }
}
