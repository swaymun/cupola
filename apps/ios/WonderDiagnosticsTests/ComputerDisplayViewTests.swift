import CoreVideo
import UIKit
import WonderComputerView
import XCTest
@preconcurrency import WebRTC

/// The computer view draws each new frame on its own display link: the draw rate follows
/// the stream and stops at what the screen can show. Real display link, real layer;
/// the simulator's screen tops out at 60 Hz, a ProMotion phone reports 120.
@MainActor
final class ComputerDisplayViewTests: XCTestCase {
    private func makeFrame(_ stamp: Int64) -> RTCVideoFrame {
        var pixels: CVPixelBuffer?
        CVPixelBufferCreate(nil, 64, 64, kCVPixelFormatType_32BGRA,
                            [kCVPixelBufferIOSurfacePropertiesKey: [String: Any]()] as CFDictionary, &pixels)
        return RTCVideoFrame(buffer: RTCCVPixelBuffer(pixelBuffer: pixels!), rotation: ._0, timeStampNs: stamp)
    }

    /// Sends `rate` frames per second for `seconds` from a background queue, like the decoder does.
    private func pump(_ view: ComputerDisplayView, rate: Int, seconds: Double) async -> Int {
        await withCheckedContinuation { continuation in
            let queue = DispatchQueue(label: "computer-display-pump")
            let timer = DispatchSource.makeTimerSource(queue: queue)
            nonisolated(unsafe) var sent = 0
            let start = DispatchTime.now()
            timer.schedule(deadline: .now(), repeating: .nanoseconds(1_000_000_000 / rate), leeway: .nanoseconds(0))
            timer.setEventHandler {
                if Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1e9 >= seconds {
                    timer.cancel()
                    continuation.resume(returning: sent)
                    return
                }
                sent += 1
                view.renderFrame(self.makeFrame(Int64(sent)))
            }
            timer.resume()
        }
    }

    func testDrawRateFollowsStreamAndStopsAtTheScreenMaximum() async throws {
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 200, height: 200)
        let view = ComputerDisplayView(frame: window.bounds)
        window.addSubview(view)
        window.isHidden = false
        defer { view.removeFromSuperview(); window.isHidden = true }
        let screenMax = scene.screen.maximumFramesPerSecond
        try await Task.sleep(for: .milliseconds(300))
        for inbound in [30, 120] {
            // The link re-reads the stream's rate once a second; measure after it has settled.
            _ = await pump(view, rate: inbound, seconds: 1.5)
            let before = view.framesDrawn
            let seconds = 3.0
            let sent = await pump(view, rate: inbound, seconds: seconds)
            try await Task.sleep(for: .milliseconds(200))
            let drawn = view.framesDrawn - before
            let expected = min(Double(sent) / seconds, Double(screenMax))
            XCTAssertEqual(Double(drawn) / seconds, expected, accuracy: expected * 0.15,
                           "inbound \(inbound) fps on a \(screenMax) Hz screen drew \(drawn) of \(sent) frames")
            print("computer-display-draw-rate inbound=\(inbound) sent=\(sent) drawn=\(drawn) screenMax=\(screenMax) drawFps=\(Double(drawn) / seconds)")
        }
    }

    func testClearedPictureDrawsNothingMore() async throws {
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 200, height: 200)
        let view = ComputerDisplayView(frame: window.bounds)
        window.addSubview(view)
        window.isHidden = false
        defer { view.removeFromSuperview(); window.isHidden = true }
        view.renderFrame(makeFrame(1))
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(view.framesDrawn, 1)
        // The link sleeps while the picture is still: no redraw without a new frame.
        try await Task.sleep(for: .seconds(1))
        XCTAssertEqual(view.framesDrawn, 1)
        view.renderFrame(nil)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(view.framesDrawn, 1)
        // The clear itself wakes the link; let it fall asleep again so the next frame is the
        // only thing that can wake it.
        try await Task.sleep(for: .seconds(1))
        XCTAssertTrue(view.isLinkSleeping, "the link idles after the clear")
        view.renderFrame(makeFrame(2))
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(view.framesDrawn, 2, "a frame after the link slept wakes it")
    }

    /// A link that wakes keeps the rate it measured: a 30 fps stream that pauses must not send a
    /// ProMotion panel back to 120 Hz, and sparse frames settle at the lowest step.
    func testWokenLinkKeepsItsMeasuredRate() async throws {
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 200, height: 200)
        let view = ComputerDisplayView(frame: window.bounds)
        window.addSubview(view)
        window.isHidden = false
        defer { view.removeFromSuperview(); window.isHidden = true }
        let screenMax = scene.screen.maximumFramesPerSecond
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(view.requestedFrameRate, screenMax, "a new link starts at the screen maximum")
        _ = await pump(view, rate: 30, seconds: 2.5)
        XCTAssertEqual(view.requestedFrameRate, 30)
        try await Task.sleep(for: .seconds(1))
        XCTAssertTrue(view.isLinkSleeping)
        view.renderFrame(makeFrame(1_000))
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(view.requestedFrameRate, 30, "waking did not reset the rate to \(screenMax)")

        let sparse = ComputerDisplayView(frame: window.bounds)
        window.addSubview(sparse)
        try await Task.sleep(for: .milliseconds(300))
        for stamp in 1...3 {
            sparse.renderFrame(makeFrame(Int64(stamp)))
            try await Task.sleep(for: .milliseconds(900))
        }
        XCTAssertEqual(sparse.requestedFrameRate, 30, "about one frame per second settles at the lowest step")
        sparse.removeFromSuperview()
    }

    /// Settling at sleep must not undercount a busy stream: the open window holds under a second
    /// of it, so a 60 fps stream that pauses has to keep asking for 60, not 30.
    func testSleepKeepsTheRateOfASixtyFpsStream() async throws {
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 200, height: 200)
        let view = ComputerDisplayView(frame: window.bounds)
        window.addSubview(view)
        window.isHidden = false
        defer { view.removeFromSuperview(); window.isHidden = true }
        let expected = min(60, scene.screen.maximumFramesPerSecond)
        try await Task.sleep(for: .milliseconds(300))
        for pause in 1...3 {
            // Vary where the pause falls in the one-second window.
            _ = await pump(view, rate: 60, seconds: 2.0 + Double(pause) * 0.3)
            XCTAssertEqual(view.requestedFrameRate, expected, "streaming")
            try await Task.sleep(for: .seconds(1))
            XCTAssertTrue(view.isLinkSleeping)
            XCTAssertEqual(view.requestedFrameRate, expected, "pause \(pause) settled a 60 fps stream at \(view.requestedFrameRate)")
        }
    }
}
