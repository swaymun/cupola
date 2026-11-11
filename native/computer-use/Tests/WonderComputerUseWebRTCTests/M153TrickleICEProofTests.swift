import XCTest
@preconcurrency import WebRTC
@testable import WonderComputerUse

final class M153TrickleICEProofTests: XCTestCase {
    func testPublisherAppliesNativeAnswerWithCRLFOrLF() throws {
        RTCInitializeSSL()
        let factory = RTCPeerConnectionFactory(
            encoderFactory: RTCDefaultVideoEncoderFactory(),
            decoderFactory: RTCDefaultVideoDecoderFactory()
        )
        for lineEnding in ["\r\n", "\n"] {
            let offered = expectation(description: "publisher offers video")
            let applied = expectation(description: "publisher applies the native answer")
            let signal = PublisherOffer()
            let publisher = WebRTCPublisher { event in
                switch event["event"] as? String {
                case "signal.offer":
                    signal.save(event)
                    offered.fulfill()
                case "signal.answerApplied": applied.fulfill()
                default: break
                }
            }
            let configuration = RTCConfiguration()
            configuration.iceServers = []
            configuration.sdpSemantics = .unifiedPlan
            let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
            let receiver = try XCTUnwrap(factory.peerConnection(
                with: configuration, constraints: constraints, delegate: nil
            ))
            defer {
                publisher.close(reason: "test_finished")
                receiver.close()
            }
            publisher.prepare(sessionID: "native-answer", generation: 1, configuration: .init())
            wait(for: [offered], timeout: 5)
            let offer = try XCTUnwrap(signal.load())
            let answered = expectation(description: "receiver creates its answer")
            receiver.setRemoteDescription(RTCSessionDescription(type: .offer, sdp: offer.sdp)) { error in
                XCTAssertNil(error)
                receiver.answer(for: constraints) { answer, error in
                    XCTAssertNil(error)
                    guard let answer else { return }
                    XCTAssertTrue(answer.sdp.contains("\r\na="), "Exercise native WebRTC line endings")
                    receiver.setLocalDescription(answer) { error in
                        XCTAssertNil(error)
                        // Malformed answers must not consume the peer's one valid answer.
                        for invalid in ["v=0\r\n", "v=0\r\na=x\u{0}", "v=0\ra=x",
                                        "v=0\r\na=" + String(repeating: "x", count: 64 * 1024)] {
                            XCTAssertFalse(publisher.setRemoteAnswer(
                                sessionID: "native-answer", generation: 1, peerRevision: offer.revision, sdp: invalid
                            ))
                        }
                        let sdp = answer.sdp.replacingOccurrences(of: "\r\n", with: lineEnding)
                        let accepted = publisher.setRemoteAnswer(
                            sessionID: "native-answer", generation: 1, peerRevision: offer.revision, sdp: sdp
                        )
                        XCTAssertTrue(accepted)
                        // Report a rejection immediately instead of waiting for an impossible callback.
                        if !accepted { applied.fulfill() }
                        answered.fulfill()
                    }
                }
            }
            wait(for: [answered, applied], timeout: 5)
        }
    }

    func testTwoLocalPeersConnectWithQueuedTrickleICE() throws {
        let factory: RTCPeerConnectionFactory = {
            RTCInitializeSSL()
            return RTCPeerConnectionFactory(
                encoderFactory: RTCDefaultVideoEncoderFactory(),
                decoderFactory: RTCDefaultVideoDecoderFactory()
            )
        }()
        let configuration = RTCConfiguration()
        configuration.iceServers = []
        configuration.sdpSemantics = .unifiedPlan
        configuration.continualGatheringPolicy = .gatherContinually
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        let connected = expectation(description: "both M153 peers reach encrypted ICE")
        connected.expectedFulfillmentCount = 2

        let left = try XCTUnwrap(factory.peerConnection(with: configuration, constraints: constraints, delegate: nil))
        let right = try XCTUnwrap(factory.peerConnection(with: configuration, constraints: constraints, delegate: nil))
        let leftDelegate = PeerDelegate { state in
            if state == .connected || state == .completed { connected.fulfill() }
        }
        let rightDelegate = PeerDelegate { state in
            if state == .connected || state == .completed { connected.fulfill() }
        }
        left.delegate = leftDelegate
        right.delegate = rightDelegate
        let router = CandidateRouter(left: left, right: right)
        leftDelegate.onCandidate = { router.route($0, fromLeft: true) }
        rightDelegate.onCandidate = { router.route($0, fromLeft: false) }
        let source = factory.videoSource(forScreenCast: true)
        let track = factory.videoTrack(with: source, trackId: "m153-proof-video")
        _ = left.add(track, streamIds: ["m153-proof-stream"])

        let offerExpectation = expectation(description: "offer and answer applied")
        let offerConstraints = RTCMediaConstraints(
            mandatoryConstraints: [kRTCMediaConstraintsOfferToReceiveVideo: kRTCMediaConstraintsValueFalse],
            optionalConstraints: nil
        )
        left.offer(for: offerConstraints) { offer, error in
            XCTAssertNil(error)
            guard let offer else { return }
            left.setLocalDescription(offer) { error in
                XCTAssertNil(error)
                right.setRemoteDescription(offer) { error in
                    XCTAssertNil(error)
                    router.markRightRemoteDescriptionSet()
                    right.answer(for: offerConstraints) { answer, error in
                        XCTAssertNil(error)
                        guard let answer else { return }
                        right.setLocalDescription(answer) { error in
                            XCTAssertNil(error)
                            left.setRemoteDescription(answer) { error in
                                XCTAssertNil(error)
                                router.markLeftRemoteDescriptionSet()
                                offerExpectation.fulfill()
                            }
                        }
                    }
                }
            }
        }

        wait(for: [offerExpectation, connected], timeout: 8)
        left.close()
        right.close()
    }
}

private final class PublisherOffer: @unchecked Sendable {
    struct Value: Sendable {
        let sdp: String
        let revision: String
    }

    private let lock = NSLock()
    private var value: Value?

    func save(_ event: [String: Any]) {
        guard let sdp = event["sdp"] as? String, let revision = event["peerRevision"] as? String else { return }
        lock.lock()
        value = Value(sdp: sdp, revision: revision)
        lock.unlock()
    }

    func load() -> Value? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

private final class CandidateRouter: @unchecked Sendable {
    private let lock = NSLock()
    private weak var left: RTCPeerConnection?
    private weak var right: RTCPeerConnection?
    private var leftReady = false
    private var rightReady = false
    private var leftPending: [RTCIceCandidate] = []
    private var rightPending: [RTCIceCandidate] = []

    init(left: RTCPeerConnection, right: RTCPeerConnection) {
        self.left = left
        self.right = right
    }

    func route(_ candidate: RTCIceCandidate, fromLeft: Bool) {
        lock.lock()
        let peer = fromLeft ? right : left
        let ready = fromLeft ? rightReady : leftReady
        if !ready {
            if fromLeft { rightPending.append(candidate) } else { leftPending.append(candidate) }
            lock.unlock()
            return
        }
        lock.unlock()
        peer?.add(candidate) { _ in }
    }

    func markRightRemoteDescriptionSet() {
        lock.lock()
        rightReady = true
        let pending = rightPending
        rightPending.removeAll()
        let peer = right
        lock.unlock()
        pending.forEach { peer?.add($0) { _ in } }
    }

    func markLeftRemoteDescriptionSet() {
        lock.lock()
        leftReady = true
        let pending = leftPending
        leftPending.removeAll()
        let peer = left
        lock.unlock()
        pending.forEach { peer?.add($0) { _ in } }
    }
}

private final class PeerDelegate: NSObject, RTCPeerConnectionDelegate {
    let onState: (RTCIceConnectionState) -> Void
    var onCandidate: ((RTCIceCandidate) -> Void)?

    init(onState: @escaping (RTCIceConnectionState) -> Void) {
        self.onState = onState
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) { onState(newState) }
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) { onCandidate?(candidate) }
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {}
}
