import XCTest
import os
@preconcurrency import WebRTC
@testable import WonderComputerView
import WonderPairing

final class ComputerReceiverTests: XCTestCase {
    func testStreamIntervalMetricsCarryRatesFrameSizeCodecAndDecoder() {
        var previous = ComputerStreamSample()
        previous.framesReceived = 100; previous.framesDecoded = 100; previous.framesDrawn = 100
        var sample = previous
        sample.framesReceived = 160; sample.framesDecoded = 158; sample.framesDrawn = 150
        sample.frameWidth = 1920; sample.frameHeight = 1080
        sample.codec = "H264"; sample.decoder = "VideoToolbox"
        let metrics = sample.intervalMetrics(since: previous, seconds: 2)
        XCTAssertEqual(metrics["inboundFps"], 30)
        XCTAssertEqual(metrics["fps"], 29)
        XCTAssertEqual(metrics["drawFps"], 25)
        XCTAssertEqual(metrics["frameWidth"], 1920)
        XCTAssertEqual(metrics["frameHeight"], 1080)
        XCTAssertEqual(metrics["codec.H264"], 1)
        XCTAssertEqual(metrics["decoder.VideoToolbox"], 1)
        XCTAssertNil(ComputerStreamSample().intervalMetrics(since: previous, seconds: 2)["frameWidth"])
        XCTAssertEqual(ComputerStreamSample.metricName("video/H.264 (hw)"), "videoH264hw")
        XCTAssertNil(ComputerStreamSample.metricName("unknown"))
        XCTAssertNil(ComputerStreamSample.metricName(""))
    }

    /// The display link follows the stream up to the screen: a 30 fps stream must not hold a
    /// ProMotion panel at 120 Hz, and nothing asks for more than the screen shows.
    func testDisplayRateFollowsStreamUpToScreenMaximum() {
        let cases: [(Double?, Int, Int)] = [
            (nil, 120, 120), (nil, 60, 60), (0, 60, 60),
            (15, 120, 30), (30, 120, 30), (31, 120, 30), (45, 120, 60), (59, 120, 60), (61, 120, 60),
            (100, 120, 120), (119, 120, 120), (240, 120, 120),
            (60, 60, 60), (120, 60, 60),
        ]
        for (inbound, screen, expected) in cases {
            XCTAssertEqual(ComputerDisplayRate.preferred(inboundFps: inbound, screenMax: screen), expected,
                           "inbound \(String(describing: inbound)) on a \(screen) Hz screen")
        }
    }

    /// The decode thread overwrites; the display link draws the newest frame once, and a
    /// cleared picture wins over frames it replaces.
    func testFrameMailboxKeepsOnlyTheNewestFrameAndWakesOnceWhenArmed() {
        let woke = expectation(description: "woke on the main queue")
        woke.expectedFulfillmentCount = 1
        woke.assertForOverFulfill = true
        let mailbox = ComputerFrameMailbox { woke.fulfill() }
        XCTAssertTrue(mailbox.armWake())
        let first = RTCVideoFrame(buffer: RTCI420Buffer(width: 8, height: 8), rotation: ._0, timeStampNs: 1)
        let second = RTCVideoFrame(buffer: RTCI420Buffer(width: 8, height: 8), rotation: ._0, timeStampNs: 2)
        mailbox.renderFrame(first)
        mailbox.renderFrame(second)
        wait(for: [woke], timeout: 10)
        XCTAssertEqual(mailbox.framesArrived, 2)
        XCTAssertFalse(mailbox.armWake(), "a waiting frame keeps the link running")
        let drawn = mailbox.take()
        XCTAssertEqual(drawn.picture?.timestampNs, 2)
        XCTAssertFalse(drawn.clear)
        XCTAssertNil(mailbox.take().picture)
        mailbox.renderFrame(first)
        mailbox.renderFrame(nil)
        let cleared = mailbox.take()
        XCTAssertNil(cleared.picture)
        XCTAssertTrue(cleared.clear)
    }

    /// Software-decoded frames reach the display as NV12 buffers made on the delivering
    /// thread, with Y, U and V in the right places.
    func testI420FramesBecomeNV12BuffersBeforeTheMainThreadSeesThem() throws {
        let i420 = RTCMutableI420Buffer(width: 8, height: 4)
        for row in 0..<4 { for column in 0..<8 { i420.mutableDataY[row * Int(i420.strideY) + column] = UInt8(row * 10 + column) } }
        for row in 0..<2 { for column in 0..<4 {
            i420.mutableDataU[row * Int(i420.strideU) + column] = UInt8(100 + row * 10 + column)
            i420.mutableDataV[row * Int(i420.strideV) + column] = UInt8(200 + row * 10 + column)
        } }
        let mailbox = ComputerFrameMailbox {}
        let done = expectation(description: "delivered from a background thread")
        DispatchQueue.global().async {
            mailbox.renderFrame(RTCVideoFrame(buffer: i420, rotation: ._0, timeStampNs: 7))
            done.fulfill()
        }
        wait(for: [done], timeout: 10)
        let buffer = try XCTUnwrap(mailbox.take().picture?.buffer)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(buffer), kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        XCTAssertEqual(CVPixelBufferGetWidth(buffer), 8)
        XCTAssertEqual(CVPixelBufferGetHeight(buffer), 4)
        let planes = Self.planes(buffer)
        XCTAssertEqual(planes.luma(row: 3, column: 5), 35)
        XCTAssertEqual(planes.chroma(row: 1, column: 2), [112, 212])
    }

    /// `RTCMTLVideoView` honoured the frame's rotation; the display layer has no such setting,
    /// so the conversion turns the pixels clockwise.
    func testRotatedFramesAreTurnedClockwise() throws {
        let i420 = RTCMutableI420Buffer(width: 8, height: 4)
        for row in 0..<4 { for column in 0..<8 { i420.mutableDataY[row * Int(i420.strideY) + column] = UInt8(row * 10 + column) } }
        for row in 0..<2 { for column in 0..<4 {
            i420.mutableDataU[row * Int(i420.strideU) + column] = UInt8(100 + row * 10 + column)
            i420.mutableDataV[row * Int(i420.strideV) + column] = UInt8(200 + row * 10 + column)
        } }
        let converter = ComputerPictureConverter()
        let quarter = try XCTUnwrap(converter.picture(for: RTCVideoFrame(buffer: i420, rotation: ._90, timeStampNs: 1))).buffer
        XCTAssertEqual(CVPixelBufferGetWidth(quarter), 4)
        XCTAssertEqual(CVPixelBufferGetHeight(quarter), 8)
        var planes = Self.planes(quarter)
        // Clockwise: the old bottom-left corner is the new top-left, the old top-left the new top-right.
        XCTAssertEqual(planes.luma(row: 0, column: 0), 30)
        XCTAssertEqual(planes.luma(row: 0, column: 3), 0)
        XCTAssertEqual(planes.luma(row: 7, column: 0), 37)
        XCTAssertEqual(planes.chroma(row: 0, column: 0), [110, 210])
        XCTAssertEqual(planes.chroma(row: 0, column: 1), [100, 200])
        let half = try XCTUnwrap(converter.picture(for: RTCVideoFrame(buffer: i420, rotation: ._180, timeStampNs: 2))).buffer
        XCTAssertEqual(CVPixelBufferGetWidth(half), 8)
        planes = Self.planes(half)
        XCTAssertEqual(planes.luma(row: 0, column: 0), 37)
        XCTAssertEqual(planes.luma(row: 3, column: 7), 0)
    }

    private struct Planes {
        let luma: (Int, Int) -> UInt8
        let chromaPair: (Int, Int) -> [UInt8]
        func luma(row: Int, column: Int) -> UInt8 { luma(row, column) }
        func chroma(row: Int, column: Int) -> [UInt8] { chromaPair(row, column) }
    }

    private static func planes(_ buffer: CVPixelBuffer) -> Planes {
        // The buffer stays locked for the test's lifetime; it is a small throwaway.
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        let y = CVPixelBufferGetBaseAddressOfPlane(buffer, 0)!.assumingMemoryBound(to: UInt8.self)
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let c = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)!.assumingMemoryBound(to: UInt8.self)
        let cStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
        return Planes(luma: { y[$0 * yStride + $1] },
                      chromaPair: { [c[$0 * cStride + $1 * 2], c[$0 * cStride + $1 * 2 + 1]] })
    }

    /// Two views showing one picture can draw frames out of order; each frame counts once.
    func testDrawnFramesCountOnceWhateverTheDrawOrder() {
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: FakePeer()))
        for stamp: Int64 in [1, 2, 1, 3, 2, 3, 4] { receiver.noteFrameDrawn(timestampNs: stamp) }
        XCTAssertEqual(receiver.drawnFrameCount, 4)
        for stamp in Int64(10)..<30 { receiver.noteFrameDrawn(timestampNs: stamp) }
        XCTAssertEqual(receiver.drawnFrameCount, 24)
    }

    /// The Diagnostics `drawFps` field is the receiver's count of frames the view drew, carried
    /// through the stream sample into the interval metrics.
    func testDrawFpsMetricReportsFramesTheViewDrew() async throws {
        let transport = FakeTransport(responses: [
            .success(response(peerRevision: "peer-1", offer: offer, state: "offerReady")),
            .success(response(peerRevision: "peer-1", state: "live")),
        ], fallback: response(peerRevision: "peer-1", state: "live"))
        let peer = FakePeer()
        let sampled = OSAllocatedUnfairLock(initialState: 0)
        peer.samples = {
            let count = sampled.withLock { $0 += 1; return $0 }
            var sample = ComputerStreamSample()
            sample.framesReceived = Double(count) * 100
            return sample
        }
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        let metrics = OSAllocatedUnfairLock<[[String: Double]]>(initialState: [])
        receiver.onStreamMetrics = { value in metrics.withLock { $0.append(value) } }
        receiver.start(session: session, transport: transport)
        try await waitUntil { await transport.answerRequests.count == 1 }
        peer.emit(.connected)
        peer.emitVideoTrack()
        try await waitUntil { receiver.currentState == .live }
        // The first metric arrives with the second sample, after both baselines have read the
        // draw count; frames drawn from here on fall wholly inside the next interval.
        try await waitUntil(timeout: 15) { metrics.withLock { !$0.isEmpty } }
        for stamp in Int64(1)...40 { receiver.noteFrameDrawn(timestampNs: stamp) }
        try await waitUntil(timeout: 15) { metrics.withLock { $0.count >= 2 } }
        let drawFps = try XCTUnwrap(metrics.withLock { $0[1]["drawFps"] })
        XCTAssertGreaterThan(drawFps, 10, "40 frames over a ~2 s interval is about 20 fps")
        XCTAssertLessThan(drawFps, 40)
        receiver.close()
    }

    /// Regression: a frame delivered off the main thread (as WebRTC does) must
    /// reach the first-frame listener without trapping, and fire exactly once.
    func testFirstFrameListenerHopsToMainOnceFromBackgroundThread() {
        let picture = ComputerPicture()
        let fired = expectation(description: "first frame on main")
        let calls = OSAllocatedUnfairLock(initialState: 0)
        let listener = FirstFrameListener {
            XCTAssertTrue(Thread.isMainThread)
            calls.withLock { $0 += 1 }
            fired.fulfill()
        }
        picture.attach(listener)
        DispatchQueue.global().async {
            XCTAssertFalse(Thread.isMainThread)
            picture.renderFrame(FakePeer.frame(1))
            picture.renderFrame(FakePeer.frame(2))
        }
        wait(for: [fired], timeout: 10)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        XCTAssertEqual(calls.withLock { $0 }, 1)
    }

    func testOfferAnswerOrderAndCandidateBuffering() async throws {
        let transport = FakeTransport(responses: [
            .success(response(peerRevision: "peer-1", nextCursor: 1, candidates: [candidate(1)], offer: nil)),
            .success(response(peerRevision: "peer-1", nextCursor: 2, candidates: [candidate(2)], offer: offer)),
        ])
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)

        peer.emitLocalCandidate()
        try await waitUntil { await transport.answerRequests.count == 1 && peer.addedSequences == [1, 2] }

        let polls = await transport.pollRequests
        XCTAssertGreaterThanOrEqual(polls.count, 2)
        XCTAssertEqual(polls[0].cursor, 0)
        XCTAssertEqual(polls[1].cursor, 1)
        let answers = await transport.answerRequests
        let localCandidates = await transport.candidateRequests
        XCTAssertEqual(answers.first?.peerRevision, "peer-1")
        XCTAssertEqual(localCandidates.map(\.sequence), [1])
        XCTAssertTrue(peer.remoteOfferAccepted)

        receiver.close()
        XCTAssertEqual(receiver.currentState, .closed)
    }

    func testOutOfRangeRemoteCandidateIsIgnoredAndFollowingValidCandidatesAreDeliveredInOrder() async throws {
        let transport = FakeTransport(responses: [
            .success(response(
                peerRevision: "peer-1",
                nextCursor: 3,
                candidates: [
                    candidate(1, sdpMLineIndex: UInt64(Int32.max) + 1),
                    candidate(2),
                    candidate(3),
                ],
                offer: offer
            )),
        ])
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)

        try await waitUntil { peer.remoteOfferAccepted && peer.addedSequences == [2, 3] }

        XCTAssertEqual(peer.addedSequences, [2, 3])
        receiver.close()
    }

    func testStaleSessionGenerationAndPeerRevisionAreIgnored() async throws {
        let transport = FakeTransport(responses: [
            .success(response(sessionID: "other-session", generation: 99, peerRevision: "stale", nextCursor: 20, candidates: [candidate(20)], offer: offer)),
            .success(response(peerRevision: "peer-current", nextCursor: 1)),
            .success(response(peerRevision: "peer-old", nextCursor: 2, offer: offer)),
        ])
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)

        try await waitUntil { (await transport.pollRequests).count >= 3 }
        XCTAssertEqual(peer.acceptedOfferCount, 0)
        let answers = await transport.answerRequests
        let candidates = await transport.candidateRequests
        XCTAssertEqual(answers.count, 0)
        XCTAssertEqual(candidates.count, 0)
        receiver.close()
    }

    func testRetryAndTeardownCancelPollingAndCloseSignaling() async throws {
        let transport = FakeTransport(responses: [
            .failure,
            .success(response(peerRevision: "peer-1", nextCursor: 0)),
        ])
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)

        try await waitUntil(timeout: 2) { receiver.currentState == .retrying("The computer could not be reached. Retrying…") }
        try await waitUntil(timeout: 2) { await transport.pollRequests.count >= 2 }
        receiver.close(reason: "dismissed")

        XCTAssertEqual(receiver.currentState, .closed)
        XCTAssertTrue(peer.closed)
        try await waitUntil { await transport.closeRequests.count == 1 }
        let closes = await transport.closeRequests
        XCTAssertEqual(closes.first?.reason, "dismissed")

        let count = await transport.pollRequests.count
        try await Task.sleep(for: .milliseconds(350))
        let pollsAfterClose = await transport.pollRequests.count
        XCTAssertEqual(pollsAfterClose, count)
    }

    func testLiveRequiresHostCaptureConnectedPeerAndVideoTrack() async throws {
        let transport = FakeTransport(responses: [
            .success(response(peerRevision: "peer-1", offer: offer, state: "offerReady")),
            .success(response(peerRevision: "peer-1", state: "live")),
        ], fallback: response(peerRevision: "peer-1", state: "live"))
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)

        try await waitUntil { await transport.answerRequests.count == 1 }
        peer.emit(.connected)
        XCTAssertNotEqual(receiver.currentState, .live)
        peer.emitVideoTrack(withFrame: false)
        XCTAssertNotEqual(receiver.currentState, .live, "an empty track is not a live picture")
        peer.emitFrame(1)
        try await waitUntil { receiver.currentState == .live }
        receiver.close()
    }

    func testRetainedFrameSurvivesSuspensionRetryRendererReplacementAndRejectsOldPeer() async throws {
        let live = response(peerRevision: "peer-1", state: "live")
        let transport = FakeTransport(responses: [.success(live)], fallback: live)
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)
        let firstRenderer = FrameLog()
        receiver.attach(renderer: firstRenderer)
        peer.emitVideoTrack()
        peer.emit(.connected)
        try await waitUntil { receiver.currentState == .live }
        XCTAssertEqual(firstRenderer.lastTimestamp, 1)
        let oldSink = peer.frameRenderer

        receiver.close(reason: "background", keepingLastFrame: true)
        XCTAssertTrue(receiver.picture.hasFrame)
        XCTAssertEqual(firstRenderer.lastTimestamp, 1)
        receiver.detach(renderer: firstRenderer)
        let restored = FrameLog()
        receiver.attach(renderer: restored)
        XCTAssertEqual(restored.lastTimestamp, 1, "replacement view replays the exact retained frame")
        receiver.start(session: session, transport: transport)
        XCTAssertEqual(restored.lastTimestamp, 1, "starting a replacement peer preserves the frame")
        oldSink?.renderFrame(FakePeer.frame(99))
        XCTAssertEqual(restored.lastTimestamp, 1, "old peer frames cannot replace this chat's picture")
        peer.emitVideoTrack()
        peer.emitFrame(2)
        peer.emit(.connected)
        try await waitUntil { receiver.currentState == .live }
        XCTAssertEqual(restored.lastTimestamp, 2)
        receiver.close()
        XCTAssertFalse(receiver.picture.hasFrame, "an explicit close clears the private picture")
        XCTAssertNil(restored.lastTimestamp)
    }

    func testDeadPeerStopsReconnectingAndKeepsFrameForTryAgain() async throws {
        let live = response(peerRevision: "peer-1", state: "live")
        for hostReconnects in [false, true] {
            let transport = FakeTransport(responses: [.success(live)], fallback: live)
            let peer = FakePeer()
            let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer), reconnectTimeout: .milliseconds(100))
            receiver.start(session: session, transport: transport)
            peer.emitVideoTrack()
            peer.emit(.connected)
            try await waitUntil { receiver.currentState == .live }
            if hostReconnects {
                await transport.replaceFallback(response(peerRevision: "peer-1", state: "reconnecting"))
            } else { peer.emit(.disconnected) }
            try await waitUntil(timeout: 10) { if case .failed = receiver.currentState { return true }; return false }
            XCTAssertTrue(receiver.canResumeAutomatically)
            XCTAssertTrue(receiver.picture.hasFrame)
            XCTAssertTrue(peer.closed)
            peer.emitFrame(99)
            peer.emit(.connected)
            XCTAssertNotEqual(receiver.currentState, .live, "late callbacks cannot revive a failed peer")
            receiver.close()
        }
    }

    func testReconnectBudgetForegroundAndCloseNeverReplayActions() {
        var recovery = ComputerReconnect()
        XCTAssertFalse(recovery.receive(.interrupted))
        XCTAssertEqual(recovery.phase, .reconnecting)
        XCTAssertFalse(recovery.receive(.live))
        XCTAssertEqual(recovery.phase, .viewing)
        XCTAssertTrue(recovery.receive(.failed))
        XCTAssertTrue(recovery.receive(.failed))
        XCTAssertFalse(recovery.receive(.failed))
        XCTAssertEqual(recovery.phase, .needsRetry)
        XCTAssertTrue(recovery.receive(.tryAgain))
        XCTAssertEqual(recovery.attempts, 0)
        XCTAssertFalse(recovery.receive(.pause))
        XCTAssertFalse(recovery.receive(.failed))
        XCTAssertFalse(recovery.receive(.live))
        XCTAssertEqual(recovery.phase, .paused)
        XCTAssertTrue(recovery.receive(.resume))
        XCTAssertFalse(recovery.receive(.close))
        for event in [ComputerReconnect.Event.live, .resume, .failed, .tryAgain, .interrupted] {
            XCTAssertFalse(recovery.receive(event))
            XCTAssertEqual(recovery.phase, .closed)
        }
    }

    func testConnectedTrackWithoutAFrameTimesOutInsteadOfClaimingLive() async throws {
        let live = response(peerRevision: "peer-1", state: "live")
        let transport = FakeTransport(responses: [.success(live)], fallback: live)
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer), reconnectTimeout: .milliseconds(100))
        receiver.start(session: session, transport: transport)
        peer.emit(.connected)
        peer.emitVideoTrack(withFrame: false)
        try await waitUntil { if case .failed = receiver.currentState { return true }; return false }
        XCTAssertFalse(receiver.picture.hasFrame)
        XCTAssertTrue(peer.closed)
        receiver.close()
    }

    func testTerminalFailureIsNotOverwrittenByLatePeerClose() async throws {
        let transport = FakeTransport(responses: [])
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        receiver.start(session: session, transport: transport)

        peer.emit(.failed)
        try await waitUntil {
            if case .failed = receiver.currentState { return true }
            return false
        }
        peer.emit(.closed)
        try await Task.sleep(for: .milliseconds(50))
        guard case .failed = receiver.currentState else {
            return XCTFail("A late peer callback replaced the terminal failure state")
        }
        receiver.close()
    }

    // A signaling poll that fails while the picture still arrives must not
    // change what the person sees (SM-04).
    func testSingleFailedPollWhileVideoIsLiveKeepsLive() async throws {
        let (receiver, peer, transport, log) = try await liveReceiver()
        let livePolls = await transport.pollRequests.count
        await transport.failNext(1)
        try await waitUntil { await transport.pollRequests.count > livePolls + 2 }
        peer.emitFrame(2)
        try await Task.sleep(for: .milliseconds(300))
        receiver.close()
        let afterLive = Array(log.states.drop(while: { $0 != .live }))
        XCTAssertEqual(afterLive, [.live, .closed], "one failed poll changed the state while video kept flowing")
    }

    func testRepeatedFailedPollsDegradeLiveThenRecover() async throws {
        let (receiver, peer, transport, _) = try await liveReceiver()
        await transport.failNext(ComputerPresentationInputs.livePollFailureTolerance)
        try await waitUntil { receiver.currentState == .retrying(ComputerPresentationInputs.unreachableMessage) }
        // The failing polls end; the next good one restores the live picture.
        peer.emitFrame(3)
        try await waitUntil { receiver.currentState == .live }
        receiver.close()
    }

    func testFailedPollWithStalledFramesDegradesLive() async throws {
        let (receiver, _, transport, _) = try await liveReceiver()
        try await Task.sleep(for: .seconds(3.2))
        await transport.failNext(1)
        try await waitUntil { receiver.currentState == .retrying(ComputerPresentationInputs.unreachableMessage) }
        receiver.close()
    }

    func testPresentationInputsHoldLiveOnlyWhilePictureArrives() {
        let live = ComputerPresentationInputs(hostState: "live", peerConnected: true, peerInterrupted: false,
                                              videoTrackAvailable: true, consecutivePollFailures: 0, framesRecent: true)
        XCTAssertNil(live.interruption)
        var failing = live
        failing.consecutivePollFailures = 1
        XCTAssertNil(failing.interruption, "one failed poll with frames arriving")
        failing.consecutivePollFailures = ComputerPresentationInputs.livePollFailureTolerance
        XCTAssertEqual(failing.interruption, .retrying(ComputerPresentationInputs.unreachableMessage))
        failing.consecutivePollFailures = 1
        failing.framesRecent = false
        XCTAssertEqual(failing.interruption, .retrying(ComputerPresentationInputs.unreachableMessage))
        var connecting = live
        connecting.videoTrackAvailable = false
        connecting.consecutivePollFailures = 1
        XCTAssertEqual(connecting.interruption, .retrying(ComputerPresentationInputs.unreachableMessage))
        var interrupted = live
        interrupted.peerInterrupted = true
        XCTAssertEqual(interrupted.interruption, .retrying(ComputerReceiverState.interruptedMessage))
    }

    private func liveReceiver() async throws -> (ComputerReceiver, FakePeer, FakeTransport, StateLog) {
        let transport = FakeTransport(responses: [
            .success(response(peerRevision: "peer-1", offer: offer, state: "offerReady")),
        ], fallback: response(peerRevision: "peer-1", state: "live"))
        let peer = FakePeer()
        let receiver = ComputerReceiver(peerFactory: FakePeerFactory(peer: peer))
        let log = StateLog()
        receiver.onStateChange = { state, _ in log.append(state) }
        receiver.start(session: session, transport: transport)
        try await waitUntil { await transport.answerRequests.count == 1 }
        peer.emit(.connected)
        peer.emitVideoTrack(withFrame: false)
        peer.emitFrame(1)
        try await waitUntil { receiver.currentState == .live }
        return (receiver, peer, transport, log)
    }

    private var session: ComputerSession {
        ComputerSession(
            id: "session",
            clientRequestId: "request",
            ownerDeviceId: "device",
            hostInstallationId: "host",
            conversationId: "conversation",
            generation: 1,
            state: .preparing,
            source: ComputerSource(width: 1280, height: 720),
            geometryRevision: 0,
            failureReason: nil,
            createdAt: "now",
            updatedAt: "now",
            lastStateAt: "now",
            endedAt: nil,
            capability: ComputerCapability(available: true, action: "stream", reason: "available")
        )
    }

    private var offer: ComputerSignalingDescription {
        decode("{\"type\":\"offer\",\"sdp\":\"v=0\\r\\nm=video 9 UDP/TLS/RTP/SAVPF 96\\r\\na=sendonly\"}")
    }

    private func candidate(_ sequence: UInt64, sdpMLineIndex: UInt64 = 0) -> ComputerSignalingCandidate {
        decode("{\"sequence\":\(sequence),\"candidate\":\"candidate:\(sequence)\",\"sdpMid\":\"0\",\"sdpMLineIndex\":\(sdpMLineIndex),\"usernameFragment\":null}")
    }

    private func response(sessionID: String = "session", generation: UInt64 = 1,
                          peerRevision: String? = nil, nextCursor: UInt64 = 0,
                          candidates: [ComputerSignalingCandidate] = [],
                          offer: ComputerSignalingDescription? = nil,
                          state: String = "connecting", captureState: String = "ready",
                          failureReason: String? = nil, textInputFocused: Bool? = nil) -> ComputerSignalingResponse {
        var object: [String: Any] = [
            "sessionId": sessionID,
            "generation": generation,
            "state": state,
            "captureState": captureState,
            "peerState": "new",
            "candidates": candidates.map { [
                "sequence": $0.sequence,
                "candidate": $0.candidate,
                "sdpMid": $0.sdpMid as Any,
                "sdpMLineIndex": $0.sdpMLineIndex,
                "usernameFragment": $0.usernameFragment as Any,
            ] },
            "cursor": 0,
            "nextCursor": nextCursor,
            "failureReason": failureReason ?? NSNull(),
            "iceServers": [],
        ]
        if let textInputFocused { object["textInputFocused"] = textInputFocused }
        object["peerRevision"] = peerRevision ?? NSNull()
        object["offer"] = offer.map { ["type": $0.type, "sdp": $0.sdp] } ?? NSNull()
        return try! JSONDecoder().decode(
            ComputerSignalingResponse.self,
            from: JSONSerialization.data(withJSONObject: object)
        )
    }

    private func decode<T: Decodable>(_ value: String) -> T {
        try! JSONDecoder().decode(T.self, from: Data(value.utf8))
    }

    private func waitUntil(timeout: TimeInterval = 10, _ condition: @escaping () async -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await condition() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("Timed out waiting for deterministic receiver condition")
    }
}

private enum FakeTransportError: Error, Sendable { case unavailable }

private final class ReportLog: @unchecked Sendable {
    private let lock = NSLock()
    private var entries: [(ComputerHostReport, UInt64)] = []
    func append(_ report: ComputerHostReport, _ revision: UInt64) { lock.withLock { entries.append((report, revision)) } }
    var reports: [ComputerHostReport] { lock.withLock { entries.map(\.0) } }
    var revisions: [UInt64] { lock.withLock { entries.map(\.1) } }
}

private actor FakeTransport: ComputerSignalingTransport {
    enum Result: Sendable { case success(ComputerSignalingResponse); case failure }

    private var results: [Result]
    private var fallback: ComputerSignalingResponse
    private let fallbackFailure: Bool
    private let ended: String?
    private(set) var pollRequests: [ComputerSignalingPollRequest] = []
    private(set) var answerRequests: [ComputerSignalingAnswerRequest] = []
    private(set) var candidateRequests: [ComputerSignalingCandidateRequest] = []
    private(set) var closeRequests: [ComputerSignalingCloseRequest] = []

    init(responses: [Result], fallback: ComputerSignalingResponse? = nil,
         fallbackFailure: Bool = false, endedReason: String? = nil) {
        self.results = responses
        self.fallbackFailure = fallbackFailure
        self.ended = endedReason
        self.fallback = fallback ?? (try! JSONDecoder().decode(
            ComputerSignalingResponse.self,
            from: Data("{\"sessionId\":\"session\",\"generation\":1,\"peerRevision\":\"peer-1\",\"state\":\"connecting\",\"captureState\":\"ready\",\"peerState\":\"new\",\"offer\":null,\"candidates\":[],\"cursor\":0,\"nextCursor\":0,\"failureReason\":null,\"iceServers\":[]}".utf8)
        ))
    }

    func poll(sessionID: String, request: ComputerSignalingPollRequest) async throws -> ComputerSignalingResponse {
        pollRequests.append(request)
        let result = results.isEmpty ? (fallbackFailure ? .failure : .success(fallback)) : results.removeFirst()
        switch result {
        case .success(let response): return response
        case .failure: throw FakeTransportError.unavailable
        }
    }

    func failNext(_ n: Int) { results = Array(repeating: .failure, count: n) }
    func replaceFallback(_ response: ComputerSignalingResponse) { fallback = response }

    func endedReason(sessionID: String, generation: UInt64) async -> String? { ended }

    func submitAnswer(sessionID: String, request: ComputerSignalingAnswerRequest) async throws -> ComputerSignalingMutationResponse {
        answerRequests.append(request)
        return mutation()
    }

    func sendCandidate(sessionID: String, request: ComputerSignalingCandidateRequest) async throws -> ComputerSignalingMutationResponse {
        candidateRequests.append(request)
        return mutation()
    }

    func close(sessionID: String, request: ComputerSignalingCloseRequest) async throws -> ComputerSignalingMutationResponse {
        closeRequests.append(request)
        return mutation()
    }

    private func mutation() -> ComputerSignalingMutationResponse {
        try! JSONDecoder().decode(ComputerSignalingMutationResponse.self, from: Data("{\"accepted\":true}".utf8))
    }
}

private final class FakePeerFactory: ComputerPeerFactory, @unchecked Sendable {
    let peer: FakePeer

    init(peer: FakePeer) { self.peer = peer }
    func makePeer() -> ComputerPeer? { peer }
}

private final class FakePeer: ComputerPeer, @unchecked Sendable {
    var onLocalCandidate: ((ComputerLocalCandidate) -> Void)?
    var onStateChange: ((ComputerPeerState) -> Void)?
    var onVideoTrack: (() -> Void)?
    private(set) var acceptedOfferCount = 0
    private(set) var remoteOfferAccepted = false
    private(set) var addedSequences: [UInt64] = []
    private(set) var closed = false
    private(set) var frameRenderer: RTCVideoRenderer?
    var samples: (@Sendable () -> ComputerStreamSample)?

    func sample(_ completion: @escaping @Sendable (ComputerStreamSample?) -> Void) { completion(samples?()) }

    func acceptOffer(_ sdp: String) async throws -> String {
        acceptedOfferCount += 1
        remoteOfferAccepted = true
        return #"v=0\r\na=answer"#
    }

    func addRemoteCandidate(_ candidate: ComputerSignalingCandidate) {
        addedSequences.append(candidate.sequence)
    }

    func attach(renderer: RTCVideoRenderer) { frameRenderer = renderer }
    func detach(renderer: RTCVideoRenderer) { if frameRenderer === renderer { frameRenderer = nil } }
    func close() { closed = true; frameRenderer?.renderFrame(nil) }

    func emit(_ state: ComputerPeerState) { onStateChange?(state) }
    func emitVideoTrack(withFrame: Bool = true) { onVideoTrack?(); if withFrame { emitFrame(1) } }
    func emitFrame(_ timestamp: Int64) { frameRenderer?.renderFrame(Self.frame(timestamp)) }
    static func frame(_ timestamp: Int64) -> RTCVideoFrame {
        RTCVideoFrame(buffer: RTCI420Buffer(width: 8, height: 8), rotation: ._0, timeStampNs: timestamp)
    }
    func emitLocalCandidate() {
        onLocalCandidate?(ComputerLocalCandidate(sdp: "candidate:local", sdpMid: "0", sdpMLineIndex: 0))
    }
}

private final class FrameLog: NSObject, RTCVideoRenderer, @unchecked Sendable {
    private let lock = NSLock()
    private var timestamp: Int64?
    var lastTimestamp: Int64? { lock.withLock { timestamp } }
    func setSize(_ size: CGSize) {}
    func renderFrame(_ frame: RTCVideoFrame?) { lock.withLock { timestamp = frame?.timeStampNs } }
}

private final class StateLog: @unchecked Sendable {
    private let lock = NSLock()
    private var all: [ComputerReceiverState] = []
    func append(_ state: ComputerReceiverState) { lock.withLock { all.append(state) } }
    var states: [ComputerReceiverState] { lock.withLock { all } }
}
