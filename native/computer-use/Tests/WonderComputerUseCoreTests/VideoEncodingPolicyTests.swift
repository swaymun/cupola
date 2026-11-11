import XCTest
@testable import WonderComputerUseCore

final class VideoEncodingPolicyTests: XCTestCase {
    func testCeilingRangeIs15To120() {
        XCTAssertEqual(VideoEncodingPolicy.defaultFramesPerSecond, 15)
        XCTAssertFalse(VideoEncodingPolicy.isValid(framesPerSecond: 14))
        XCTAssertTrue(VideoEncodingPolicy.isValid(framesPerSecond: 15))
        XCTAssertTrue(VideoEncodingPolicy.isValid(framesPerSecond: 120))
        XCTAssertFalse(VideoEncodingPolicy.isValid(framesPerSecond: 121))
    }

    /// The FPS0 table: 1/2/4 Mbit/s at 15 fps for 720p/1080p/1440p, scaling with fps.
    func testMaxBitrateMatchesTheFPS0Table() {
        let table: [(Int, Int, [Int])] = [
            (1_280, 720, [1, 2, 4, 8]),
            (1_920, 1_080, [2, 4, 8, 16]),
            (2_560, 1_440, [4, 8, 16, 32]),
        ]
        for (width, height, megabits) in table {
            for (fps, expected) in zip([15, 30, 60, 120], megabits) {
                XCTAssertEqual(VideoEncodingPolicy.maxBitrateBps(width: width, height: height, framesPerSecond: fps),
                               expected * 1_000_000, "\(height)p\(fps)")
            }
        }
        // Tiers follow pixel count, so a narrow canvas gets the tier its area fits.
        XCTAssertEqual(VideoEncodingPolicy.maxBitrateBps(width: 600, height: 1_080, framesPerSecond: 30), 2_000_000)
        XCTAssertEqual(VideoEncodingPolicy.maxBitrateBps(width: 1_600, height: 900, framesPerSecond: 30), 4_000_000)
    }

    func testLevel52OnlyWhenTheMacroblockRateNeedsIt() {
        XCTAssertEqual(VideoEncodingPolicy.h264Level(width: 1_920, height: 1_080, framesPerSecond: 120), .level5_1)
        XCTAssertEqual(VideoEncodingPolicy.h264Level(width: 2_560, height: 1_440, framesPerSecond: 60), .level5_1)
        XCTAssertEqual(VideoEncodingPolicy.h264Level(width: 2_560, height: 1_440, framesPerSecond: 68), .level5_1)
        XCTAssertEqual(VideoEncodingPolicy.h264Level(width: 2_560, height: 1_440, framesPerSecond: 69), .level5_2)
        XCTAssertEqual(VideoEncodingPolicy.h264Level(width: 2_560, height: 1_440, framesPerSecond: 120), .level5_2)
        XCTAssertEqual(VideoEncodingPolicy.h264Level(forSessionCeiling: 60), .level5_1)
        XCTAssertEqual(VideoEncodingPolicy.h264Level(forSessionCeiling: 120), .level5_2)
    }
}
