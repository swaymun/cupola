import XCTest
@preconcurrency import WebRTC
@testable import WonderComputerUse
import WonderComputerUseCore

final class ScreenShareVideoEncoderFactoryTests: XCTestCase {
    func testRaisesDefaultH264LevelsSoVideoToolboxAcceptsLargeScreens() {
        XCTAssertEqual(ScreenShareVideoEncoderFactory.raisingH264Level("42e01f"), "42e033")
        XCTAssertEqual(ScreenShareVideoEncoderFactory.raisingH264Level("640c1f"), "640c33")
        XCTAssertNil(ScreenShareVideoEncoderFactory.raisingH264Level("640c34"))
        XCTAssertNil(ScreenShareVideoEncoderFactory.raisingH264Level("zzzzzz"))
        XCTAssertNil(ScreenShareVideoEncoderFactory.raisingH264Level(nil))
    }

    func testEveryAdvertisedH264FormatIsRaised() {
        let h264 = ScreenShareVideoEncoderFactory().supportedCodecs().filter { $0.name == kRTCVideoCodecH264Name }
        XCTAssertFalse(h264.isEmpty)
        for info in h264 {
            XCTAssertNotNil(ScreenShareVideoEncoderFactory.raisingH264Level(info.parameters["profile-level-id"]), info.parameters.description)
        }
    }

    func testLevelFollowsTheSessionAndNeverDropsBelow51() {
        let factory = ScreenShareVideoEncoderFactory()
        XCTAssertEqual(factory.h264Level, .level5_1)
        factory.h264Level = ScreenShareVideoEncoderFactory.rtcLevel(VideoEncodingPolicy.h264Level(forSessionCeiling: 120))
        XCTAssertEqual(factory.h264Level, .level5_2)
        XCTAssertEqual(ScreenShareVideoEncoderFactory.raisingH264Level("640c33", to: .level5_2), "640c34")
        XCTAssertNil(ScreenShareVideoEncoderFactory.raisingH264Level("640c34", to: .level5_2))
        factory.h264Level = .level3_1
        XCTAssertEqual(factory.h264Level, .level5_1)
    }

    func testSenderCarriesTheFrameRateAndBitrateCeilingsAndKeepsResolution() {
        RTCInitializeSSL()
        let factory = RTCPeerConnectionFactory(encoderFactory: ScreenShareVideoEncoderFactory(),
                                               decoderFactory: RTCDefaultVideoDecoderFactory())
        let peer = factory.peerConnection(with: RTCConfiguration(),
                                          constraints: RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil),
                                          delegate: nil)
        let track = factory.videoTrack(with: factory.videoSource(forScreenCast: true), trackId: "fps")
        guard let sender = peer?.add(track, streamIds: ["fps"]) else { return XCTFail("no sender") }
        for (fps, megabits) in [(15, 4), (60, 16), (120, 32)] {
            let parameters = sender.parameters
            WebRTCPublisher.applyEncodingLimits(
                to: parameters, configuration: CaptureVideoQuality.high.configuration(framesPerSecond: fps))
            sender.parameters = parameters
            let applied = sender.parameters
            XCTAssertEqual(applied.degradationPreference?.intValue, RTCDegradationPreference.maintainResolution.rawValue)
            XCTAssertEqual(applied.encodings.first?.maxFramerate?.intValue, fps)
            XCTAssertEqual(applied.encodings.first?.maxBitrateBps?.intValue, megabits * 1_000_000)
        }
        peer?.close()
    }
}
