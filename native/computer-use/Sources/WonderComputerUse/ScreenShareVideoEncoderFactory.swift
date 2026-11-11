import Foundation
@preconcurrency import WebRTC
import WonderComputerUseCore

/// WebRTC's default H264 formats carry level 3.1 (`42e01f`, `640c1f`), which
/// fits 1280x720 at most. VideoToolbox rejects larger frames at that level
/// (`kVTParameterErr`), and WebRTC then switches the screen share to software
/// VP8, which encodes and jitters far more slowly at 1920x1080 and 2560x1440.
/// Encoding at level 5.1 keeps the hardware encoder for every capture size, and
/// level 5.2 carries 2560x1440 above 68 fps. The publisher sets the level for
/// the session before negotiation; only the encoder's bitstream level changes
/// and the negotiated SDP is untouched.
final class ScreenShareVideoEncoderFactory: NSObject, RTCVideoEncoderFactory, @unchecked Sendable {
    static let minimumH264Level = RTCH264Level.level5_1

    private let base = RTCDefaultVideoEncoderFactory()
    private let lock = NSLock()
    private var level = ScreenShareVideoEncoderFactory.minimumH264Level

    /// The level new encoders are created at. It never goes below 5.1.
    var h264Level: RTCH264Level {
        get { lock.lock(); defer { lock.unlock() }; return level }
        set { lock.lock(); level = newValue.rawValue > Self.minimumH264Level.rawValue ? newValue : Self.minimumH264Level; lock.unlock() }
    }

    static func rtcLevel(_ level: ScreenShareH264Level) -> RTCH264Level {
        switch level {
        case .level5_1: .level5_1
        case .level5_2: .level5_2
        }
    }

    func createEncoder(_ info: RTCVideoCodecInfo) -> (any RTCVideoEncoder)? {
        guard info.name == kRTCVideoCodecH264Name,
              let raised = Self.raisingH264Level(info.parameters["profile-level-id"], to: h264Level) else {
            return base.createEncoder(info)
        }
        var parameters = info.parameters
        parameters["profile-level-id"] = raised
        return RTCVideoEncoderH264(codecInfo: RTCVideoCodecInfo(name: info.name, parameters: parameters))
    }

    func supportedCodecs() -> [RTCVideoCodecInfo] {
        base.supportedCodecs()
    }

    /// Returns the profile-level-id at `target`, or nil when the value is
    /// unparseable or already at least that level.
    static func raisingH264Level(_ profileLevelID: String?, to target: RTCH264Level = minimumH264Level) -> String? {
        guard let profileLevelID, profileLevelID.utf8.count == 6 else { return nil }
        let parsed: RTCH264ProfileLevelId? = RTCH264ProfileLevelId(hexString: profileLevelID)
        // `hexString` echoes the input, so rebuild it to confirm WebRTC
        // understood the profile and level.
        let canonical: RTCH264ProfileLevelId? = parsed.flatMap { RTCH264ProfileLevelId(profile: $0.profile, level: $0.level) }
        guard let parsed, canonical?.hexString.lowercased() == profileLevelID.lowercased(),
              parsed.level.rawValue < target.rawValue else { return nil }
        let raised: RTCH264ProfileLevelId? = RTCH264ProfileLevelId(profile: parsed.profile, level: target)
        return raised?.hexString
    }
}
