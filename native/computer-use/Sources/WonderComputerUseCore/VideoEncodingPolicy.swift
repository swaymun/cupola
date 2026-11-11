import Foundation

/// H.264 bitstream levels the screen share asks VideoToolbox for. Level 5.1
/// allows 983,040 macroblocks per second, level 5.2 allows 2,073,600.
public enum ScreenShareH264Level: Int, Sendable {
    case level5_1 = 51
    case level5_2 = 52

    public var maximumMacroblocksPerSecond: Int {
        switch self {
        case .level5_1: 983_040
        case .level5_2: 2_073_600
        }
    }
}

/// The frame-rate ceiling a viewer asks for and what the Mac publisher derives
/// from it. Ceilings are 15...120; a request without one means 15, which is
/// what every client before the field existed received.
public enum VideoEncodingPolicy {
    public static let minimumFramesPerSecond = 15
    public static let maximumFramesPerSecond = 120
    public static let defaultFramesPerSecond = minimumFramesPerSecond

    public static func isValid(framesPerSecond: Int) -> Bool {
        (minimumFramesPerSecond...maximumFramesPerSecond).contains(framesPerSecond)
    }

    /// Sender `maxBitrateBps`. The ceiling scales with fps, because each frame
    /// needs about the same bits at 15 and at 120: 1/2/4 Mbit/s at 15 fps for
    /// 720p/1080p/1440p (the FPS0 table). A flat cap made 1080p30 spend longer
    /// in the jitter buffer than 1080p15.
    public static func maxBitrateBps(width: Int, height: Int, framesPerSecond: Int) -> Int {
        let fifteenFramesPerSecond: Int
        switch width * height {
        case ...(1_280 * 720): fifteenFramesPerSecond = 1_000_000
        case ...(1_920 * 1_080): fifteenFramesPerSecond = 2_000_000
        default: fifteenFramesPerSecond = 4_000_000
        }
        return fifteenFramesPerSecond * max(1, framesPerSecond) / minimumFramesPerSecond
    }

    /// The lowest level that carries `width`x`height` at `framesPerSecond`.
    public static func h264Level(width: Int, height: Int, framesPerSecond: Int) -> ScreenShareH264Level {
        let macroblocks = ((width + 15) / 16) * ((height + 15) / 16) * max(1, framesPerSecond)
        return macroblocks <= ScreenShareH264Level.level5_1.maximumMacroblocksPerSecond ? .level5_1 : .level5_2
    }

    /// The level for a whole session. A live quality change can grow the canvas
    /// to the largest size, and the encoder keeps the level it was created with,
    /// so size it for that canvas.
    public static func h264Level(forSessionCeiling framesPerSecond: Int) -> ScreenShareH264Level {
        h264Level(width: CaptureConfiguration.maximumWidth, height: CaptureConfiguration.maximumHeight,
                  framesPerSecond: framesPerSecond)
    }
}
