//! Sender evidence, not codec availability, owns the hardware frame-rate cap.
use crate::pacing::{HOST_MAX_FPS, SOFTWARE_MAX_FPS};

fn parameter<'a>(fmtp: &'a str, name: &str) -> Option<&'a str> {
    fmtp.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

pub fn supported_h264(fmtp: &str) -> bool {
    parameter(fmtp, "packetization-mode") == Some("1")
        && parameter(fmtp, "profile-level-id")
            .is_some_and(|v| v.len() == 6 && v.is_ascii() && v[..4].eq_ignore_ascii_case("42e0"))
}

pub fn encoder_cap(codec: &str, fmtp: &str, width: u32, height: u32, hardware: bool) -> u32 {
    if !codec.eq_ignore_ascii_case("video/H264") {
        return SOFTWARE_MAX_FPS;
    }
    // H.264 Annex A MaxMBPS. Unknown/malformed level never permits a raise.
    let level = parameter(fmtp, "profile-level-id")
        .filter(|v| v.len() == 6 && v.is_ascii())
        .and_then(|v| u8::from_str_radix(&v[4..], 16).ok());
    let mbps = match level {
        Some(10) => 1_485,
        Some(11) => 3_000,
        Some(12) => 6_000,
        Some(13 | 20) => 11_880,
        Some(21) => 19_800,
        Some(22) => 20_250,
        Some(30) => 40_500,
        Some(31) => 108_000,
        Some(32) => 216_000,
        Some(40 | 41) => 245_760,
        Some(42) => 522_240,
        Some(50) => 589_824,
        Some(51) => 983_040,
        Some(52) => 2_073_600,
        _ => return SOFTWARE_MAX_FPS,
    };
    let macroblocks = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    if macroblocks == 0 {
        return SOFTWARE_MAX_FPS;
    }
    let host = if hardware {
        HOST_MAX_FPS
    } else {
        SOFTWARE_MAX_FPS
    };
    let cap = host.min((mbps / macroblocks).max(1) as u32);
    [60, 30, 15]
        .into_iter()
        .find(|step| *step <= cap)
        .unwrap_or(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_active_h264_hardware_can_raise_the_cap_and_level_still_limits_it() {
        assert_eq!(encoder_cap("video/VP8", "", 1920, 1080, true), 30);
        assert_eq!(
            encoder_cap("video/H264", "profile-level-id=42e033", 1920, 1080, false),
            30
        );
        assert_eq!(
            encoder_cap("video/H264", "profile-level-id=42e033", 1920, 1080, true),
            60
        );
        assert_eq!(
            encoder_cap("video/H264", "profile-level-id=42e02a", 2560, 1440, true),
            30
        );
        assert_eq!(
            encoder_cap("video/H264", "profile-level-id=42e029", 1920, 1080, true),
            30
        );
        for fmtp in ["", "profile-level-id=bad", "profile-level-id=42e0ff"] {
            assert_eq!(encoder_cap("video/H264", fmtp, 1920, 1080, true), 30);
        }
        assert_eq!(
            encoder_cap("video/H264", "profile-level-id=42e033", 0, 0, true),
            30
        );
        assert!(supported_h264(
            "packetization-mode=1; profile-level-id=42e033"
        ));
        assert!(!supported_h264(
            "packetization-mode=0; profile-level-id=42e033"
        ));
        assert!(!supported_h264(
            "packetization-mode=1; profile-level-id=640033"
        ));
    }
}
