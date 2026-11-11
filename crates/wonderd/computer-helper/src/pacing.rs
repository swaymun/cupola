//! Frame-rate targets and pacing shared by both capture backends.
use std::time::{Duration, Instant};

/// A rate above 30 requires sender evidence of active hardware H264.
pub const SOFTWARE_MAX_FPS: u32 = 30;
pub const HOST_MAX_FPS: u32 = 60;
/// The slowest rate a request can ask for; matches the phone's 15...120 range.
pub const MIN_FPS: u32 = 15;

/// The phone's ceiling when it sent one, else the quality's default.
pub fn target_fps(quality_default: u32, requested: Option<u64>) -> u32 {
    requested.map_or(quality_default, |r| {
        r.clamp(MIN_FPS as u64, HOST_MAX_FPS as u64) as u32
    })
}

/// Keep the conservative VP8 budget for both codecs; each doubling of fps costs about 1.5x.
/// 30 fps gives 3 / 6 / 8 Mbit/s at 720p / 1080p / 1440p.
pub fn max_bitrate_bps(quality_index: u32, fps: u32) -> u64 {
    let at_30 = match quality_index {
        1 => 6_000_000.0,
        2 => 8_000_000.0,
        _ => 3_000_000.0,
    };
    (at_30 * (fps.clamp(1, HOST_MAX_FPS) as f64 / 30.0).powf(0.585)) as u64
}

/// Admits callbacks that arrive faster than the target (the Windows capture
/// API follows the display's refresh) so about `fps` frames per second pass.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct FramePacer {
    next: Instant,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl FramePacer {
    pub fn new(now: Instant) -> Self {
        Self { next: now }
    }

    /// True when this frame is due. A little early is fine, because the
    /// callback clock jitters around the display's refresh interval.
    pub fn admit(&mut self, now: Instant, fps: u32) -> bool {
        if now + Duration::from_millis(4) < self.next {
            return false;
        }
        let period = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        self.next = (self.next + period).max(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted(arrival_hz: f64, fps: u32, seconds: f64, jitter_ms: f64) -> usize {
        let start = Instant::now();
        let mut pacer = FramePacer::new(start);
        let mut count = 0;
        let mut n = 0u64;
        loop {
            // Deterministic +/- jitter so the run is repeatable.
            let wobble = ((n * 7919 % 11) as f64 / 10.0 - 0.5) * 2.0 * jitter_ms;
            let t = n as f64 / arrival_hz + wobble / 1000.0;
            if t > seconds {
                return count;
            }
            let now = start + Duration::from_secs_f64(t.max(0.0));
            if pacer.admit(now, fps) {
                count += 1;
            }
            n += 1;
        }
    }

    #[test]
    fn a_60_hz_display_passes_30_fps_and_15_fps() {
        for jitter in [0.0, 2.0] {
            let at_30 = admitted(60.0, 30, 10.0, jitter);
            assert!((290..=305).contains(&at_30), "30 fps passed {at_30} frames");
            let at_15 = admitted(60.0, 15, 10.0, jitter);
            assert!((145..=155).contains(&at_15), "15 fps passed {at_15} frames");
        }
    }

    #[test]
    fn a_display_slower_than_the_target_passes_every_frame() {
        let at_24 = admitted(24.0, 30, 10.0, 0.0);
        assert!((238..=241).contains(&at_24), "24 Hz passed {at_24} frames");
    }

    #[test]
    fn a_stall_does_not_burst() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(start);
        assert!(pacer.admit(start, 30));
        // Two seconds of nothing, then frames at 60 Hz: only every other one passes.
        let resume = start + Duration::from_secs(2);
        let passed = (0..12)
            .filter(|i| pacer.admit(resume + Duration::from_micros(16_667 * i), 30))
            .count();
        assert!((5..=7).contains(&passed), "burst passed {passed} of 12");
    }

    #[test]
    fn the_request_is_clamped_to_what_the_host_can_do() {
        assert_eq!(target_fps(15, None), 15);
        assert_eq!(target_fps(15, Some(30)), 30);
        assert_eq!(target_fps(30, Some(120)), HOST_MAX_FPS);
        assert_eq!(target_fps(30, Some(5)), MIN_FPS);
    }

    #[test]
    fn bitrate_rises_with_frame_rate_and_resolution() {
        assert_eq!(max_bitrate_bps(1, 30), 6_000_000);
        for q in 0..3 {
            assert!(max_bitrate_bps(q, 15) < max_bitrate_bps(q, 30));
            assert!(max_bitrate_bps(q, 30) < max_bitrate_bps(q, 60));
        }
        assert!(max_bitrate_bps(0, 30) < max_bitrate_bps(1, 30));
        assert!(max_bitrate_bps(1, 30) < max_bitrate_bps(2, 30));
        assert_eq!(max_bitrate_bps(2, 30), 8_000_000);
    }
}
