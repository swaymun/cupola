import CoreMedia
import Foundation
@preconcurrency import WebRTC
import WonderComputerUseCore

/// The helper-side publisher owns the only WebRTC peer in this checkpoint.
/// Signaling is daemon-mediated over the private JSONL pipe; this type never
/// opens a listener and never creates a data channel.
final class WebRTCPublisher: NSObject, RTCPeerConnectionDelegate, @unchecked Sendable {
    private struct PeerIdentity: Equatable {
        let sessionID: String
        let generation: UInt64
        let token: UUID
    }

    private struct CandidateKey: Hashable {
        let sequence: UInt64
        let candidate: String
        let sdpMid: String?
        let sdpMLineIndex: Int32
        let usernameFragment: String?
    }

    private static let encoderFactory = ScreenShareVideoEncoderFactory()
    private static let factory: RTCPeerConnectionFactory = {
        RTCInitializeSSL()
        return RTCPeerConnectionFactory(
            encoderFactory: encoderFactory,
            decoderFactory: RTCDefaultVideoDecoderFactory()
        )
    }()

    /// Holds the frame-rate and bitrate ceilings and keeps resolution fixed:
    /// when bandwidth or the encoder falls short WebRTC drops frames (120, 60,
    /// 30, 15) instead of blurring text. Every quality uses it, Auto included.
    static func applyEncodingLimits(to parameters: RTCRtpParameters, configuration: CaptureConfiguration) {
        parameters.degradationPreference = NSNumber(value: RTCDegradationPreference.maintainResolution.rawValue)
        let bitrate = VideoEncodingPolicy.maxBitrateBps(
            width: configuration.width, height: configuration.height, framesPerSecond: configuration.framesPerSecond)
        for encoding in parameters.encodings {
            encoding.maxFramerate = NSNumber(value: configuration.framesPerSecond)
            encoding.maxBitrateBps = NSNumber(value: bitrate)
        }
    }

    private static let maximumSDPBytes = 64 * 1024
    private static let maximumCandidateBytes = 4 * 1024
    private static let maximumSDPMidBytes = 128
    private static let maximumUsernameFragmentBytes = 256
    private static let maximumPendingCandidates = 128

    private let eventSink: @Sendable ([String: Any]) -> Void
    private let lock = NSLock()
    private var identity: PeerIdentity?
    private var peerConnection: RTCPeerConnection?
    // RTCVideoCapturer.delegate is weak. Keep the source alive for the full
    // peer lifetime so ScreenCaptureKit frames continue reaching WebRTC.
    private var videoSource: RTCVideoSource?
    private var videoCapturer: RTCVideoCapturer?
    private var videoSender: RTCRtpSender?
    private var remoteDescriptionSet = false
    private var pendingRemoteAnswer: String?
    private var remoteAnswer: String?
    private var pendingRemoteCandidates: [CandidateKey: RTCIceCandidate] = [:]
    private var appliedRemoteCandidates: Set<CandidateKey> = []
    private var remoteCandidateBySequence: [UInt64: CandidateKey] = [:]

    init(eventSink: @escaping @Sendable ([String: Any]) -> Void) {
        self.eventSink = eventSink
        super.init()
    }

    func prepare(sessionID: String, generation: UInt64, configuration capture: CaptureConfiguration) {
        close(reason: "replaced")
        guard !sessionID.isEmpty, sessionID.utf8.count <= 128, generation > 0 else {
            emitError(reason: "invalid_identity")
            return
        }

        let configuration = RTCConfiguration()
        configuration.iceServers = []
        configuration.sdpSemantics = .unifiedPlan
        configuration.continualGatheringPolicy = .gatherContinually
        let constraints = RTCMediaConstraints(
            mandatoryConstraints: nil,
            optionalConstraints: ["DtlsSrtpKeyAgreement": kRTCMediaConstraintsValueTrue]
        )
        guard let peer = Self.factory.peerConnection(with: configuration, constraints: constraints, delegate: self) else {
            emitError(reason: "peer_creation_failed")
            return
        }
        Self.encoderFactory.h264Level = ScreenShareVideoEncoderFactory.rtcLevel(
            VideoEncodingPolicy.h264Level(forSessionCeiling: capture.framesPerSecond))
        let source = Self.factory.videoSource(forScreenCast: true)
        source.adaptOutputFormat(toWidth: Int32(capture.width), height: Int32(capture.height), fps: Int32(capture.framesPerSecond))
        let capturer = RTCVideoCapturer(delegate: source)
        let track = Self.factory.videoTrack(with: source, trackId: "wonder-screen")
        let sender = peer.add(track, streamIds: ["wonder-screen"])
        if let sender {
            let parameters = sender.parameters
            Self.applyEncodingLimits(to: parameters, configuration: capture)
            sender.parameters = parameters
        }

        let peerIdentity = PeerIdentity(sessionID: sessionID, generation: generation, token: UUID())
        lock.lock()
        identity = peerIdentity
        peerConnection = peer
        videoSource = source
        videoCapturer = capturer
        videoSender = sender
        remoteDescriptionSet = false
        pendingRemoteAnswer = nil
        remoteAnswer = nil
        pendingRemoteCandidates.removeAll(keepingCapacity: true)
        appliedRemoteCandidates.removeAll(keepingCapacity: true)
        remoteCandidateBySequence.removeAll(keepingCapacity: true)
        lock.unlock()
        emit([
            "event": "signal.peerReady",
            "sessionID": peerIdentity.sessionID,
            "generation": peerIdentity.generation,
            "peerRevision": peerIdentity.token.uuidString,
        ])

        let offerConstraints = RTCMediaConstraints(
            mandatoryConstraints: [kRTCMediaConstraintsOfferToReceiveVideo: kRTCMediaConstraintsValueFalse],
            optionalConstraints: nil
        )
        peer.offer(for: offerConstraints) { [weak self, weak peer] offer, error in
            guard let self, let peer, let offer, error == nil else {
                self?.emitError(peer: peer, identity: peerIdentity, reason: "offer_failed")
                return
            }
            guard self.isCurrent(peer: peer, identity: peerIdentity) else { return }
            peer.setLocalDescription(offer) { [weak self, weak peer] error in
                guard let self, let peer else { return }
                guard self.isCurrent(peer: peer, identity: peerIdentity) else { return }
                guard error == nil else {
                    self.emitError(peer: peer, identity: peerIdentity, reason: "local_description_failed")
                    return
                }
                guard offer.sdp.utf8.count <= Self.maximumSDPBytes else {
                    self.emitError(peer: peer, identity: peerIdentity, reason: "sdp_too_large")
                    return
                }
                self.emitForCurrentPeer(peer: peer, identity: peerIdentity, object: [
                    "event": "signal.offer",
                    "sessionID": peerIdentity.sessionID,
                    "generation": peerIdentity.generation,
                    "peerRevision": peerIdentity.token.uuidString,
                    "type": "offer",
                    "sdp": offer.sdp,
                ])
            }
        }
    }

    @discardableResult
    func updateQuality(sessionID: String, generation: UInt64, configuration: CaptureConfiguration) -> Bool {
        lock.lock()
        guard identity?.sessionID == sessionID, identity?.generation == generation,
              let source = videoSource, let sender = videoSender else {
            lock.unlock()
            return false
        }
        source.adaptOutputFormat(toWidth: Int32(configuration.width), height: Int32(configuration.height),
                                 fps: Int32(configuration.framesPerSecond))
        let parameters = sender.parameters
        Self.applyEncodingLimits(to: parameters, configuration: configuration)
        sender.parameters = parameters
        lock.unlock()
        return true
    }

    /// Returns false for a stale identity, conflicting answer, or unavailable
    /// peer. Identical in-flight/applied answers are accepted without another
    /// WebRTC call.
    @discardableResult
    func setRemoteAnswer(sessionID: String, generation: UInt64, peerRevision: String, sdp: String) -> Bool {
        guard validSDP(sdp) else {
            emitError(sessionID: sessionID, generation: generation, peerRevision: peerRevision, reason: "sdp_invalid")
            return false
        }
        lock.lock()
        guard let identity, identity.sessionID == sessionID, identity.generation == generation,
              identity.token.uuidString == peerRevision,
              let peer = peerConnection else {
            lock.unlock()
            return false
        }
        if remoteAnswer == sdp || pendingRemoteAnswer == sdp {
            lock.unlock()
            return true
        }
        guard remoteAnswer == nil, pendingRemoteAnswer == nil, !remoteDescriptionSet else {
            lock.unlock()
            return false
        }
        pendingRemoteAnswer = sdp
        lock.unlock()

        let answer = RTCSessionDescription(type: .answer, sdp: sdp)
        peer.setRemoteDescription(answer) { [weak self, weak peer] error in
            guard let self, let peer else { return }
            guard self.isCurrent(peer: peer, identity: identity) else { return }
            guard error == nil else {
                self.lock.lock()
                if self.pendingRemoteAnswer == sdp { self.pendingRemoteAnswer = nil }
                self.lock.unlock()
                self.emitError(peer: peer, identity: identity, reason: "remote_description_failed")
                return
            }
            self.lock.lock()
            guard self.pendingRemoteAnswer == sdp else {
                self.lock.unlock()
                return
            }
            self.pendingRemoteAnswer = nil
            self.remoteAnswer = sdp
            self.remoteDescriptionSet = true
            let pending = self.pendingRemoteCandidates
            self.pendingRemoteCandidates.removeAll(keepingCapacity: true)
            self.lock.unlock()
            for (key, candidate) in pending {
                self.add(candidate, key: key, peer: peer, identity: identity)
            }
            self.emitForCurrentPeer(peer: peer, identity: identity, object: [
                "event": "signal.answerApplied",
                "sessionID": identity.sessionID,
                "generation": identity.generation,
                "peerRevision": identity.token.uuidString,
            ])
        }
        return true
    }

    /// Returns false for malformed data, conflicting sequence reuse, stale
    /// identity, or a full candidate set. Identical pending/applied candidates
    /// are accepted without another addIceCandidate call.
    @discardableResult
    func addRemoteCandidate(
        sessionID: String,
        generation: UInt64,
        peerRevision: String,
        sequence: UInt64,
        sdp: String,
        sdpMid: String?,
        sdpMLineIndex: Int32,
        usernameFragment: String?
    ) -> Bool {
        guard sequence > 0, sequence <= Self.maximumPendingCandidates,
              validCandidate(sdp),
              validOptionalText(sdpMid, maximumBytes: Self.maximumSDPMidBytes),
              validOptionalText(usernameFragment, maximumBytes: Self.maximumUsernameFragmentBytes),
              sdpMLineIndex >= 0 else {
            emitError(sessionID: sessionID, generation: generation, peerRevision: peerRevision, reason: "candidate_invalid")
            return false
        }
        let key = CandidateKey(
            sequence: sequence,
            candidate: sdp,
            sdpMid: sdpMid,
            sdpMLineIndex: sdpMLineIndex,
            usernameFragment: usernameFragment
        )
        let candidate = RTCIceCandidate(sdp: sdp, sdpMLineIndex: sdpMLineIndex, sdpMid: sdpMid)
        lock.lock()
        guard let identity, identity.sessionID == sessionID, identity.generation == generation,
              identity.token.uuidString == peerRevision,
              let peer = peerConnection else {
            lock.unlock()
            return false
        }
        if let existing = remoteCandidateBySequence[sequence] {
            let duplicate = existing == key
            lock.unlock()
            return duplicate
        }
        guard remoteCandidateBySequence.count < Self.maximumPendingCandidates else {
            lock.unlock()
            return false
        }
        remoteCandidateBySequence[sequence] = key
        if !remoteDescriptionSet {
            pendingRemoteCandidates[key] = candidate
            lock.unlock()
            return true
        }
        appliedRemoteCandidates.insert(key)
        lock.unlock()
        add(candidate, key: key, peer: peer, identity: identity)
        return true
    }

    func push(_ frame: CapturedVideoFrame) {
        lock.lock()
        guard let identity, identity.sessionID == frame.metadata.sessionID,
              identity.generation == frame.metadata.generation,
              let capturer = videoCapturer else {
            lock.unlock()
            return
        }
        lock.unlock()
        let seconds = CMTimeGetSeconds(frame.timestamp)
        let safeSeconds = frame.timestamp.isValid && seconds.isFinite && seconds >= 0
            ? min(seconds, Double(Int64.max) / 1_000_000_000)
            : ProcessInfo.processInfo.systemUptime
        let timestamp = Int64(max(0, safeSeconds) * 1_000_000_000)
        let videoFrame = RTCVideoFrame(
            buffer: RTCCVPixelBuffer(pixelBuffer: frame.pixelBuffer),
            rotation: ._0,
            timeStampNs: timestamp
        )
        capturer.delegate?.capturer(capturer, didCapture: videoFrame)
    }

    func close(reason: String) {
        lock.lock()
        let peer = peerConnection
        let oldIdentity = identity
        peerConnection = nil
        videoSource = nil
        videoCapturer = nil
        videoSender = nil
        identity = nil
        remoteDescriptionSet = false
        pendingRemoteAnswer = nil
        remoteAnswer = nil
        pendingRemoteCandidates.removeAll(keepingCapacity: true)
        appliedRemoteCandidates.removeAll(keepingCapacity: true)
        remoteCandidateBySequence.removeAll(keepingCapacity: true)
        lock.unlock()
        peer?.close()
        if let oldIdentity, reason != "replaced" {
            emit([
                "event": "signal.closed",
                "sessionID": oldIdentity.sessionID,
                "generation": oldIdentity.generation,
                "peerRevision": oldIdentity.token.uuidString,
                "reason": String(reason.prefix(128)),
            ])
        }
    }

    @discardableResult
    func close(sessionID: String, generation: UInt64, peerRevision: String, reason: String) -> Bool {
        lock.lock()
        let matches = identity?.sessionID == sessionID && identity?.generation == generation
            && identity?.token.uuidString == peerRevision
        lock.unlock()
        guard matches else { return false }
        close(reason: reason)
        return true
    }

    /// Capture lifecycle commands are emitted by the already-authenticated
    /// helper supervisor, rather than by the viewer. They still bind to the
    /// current session and generation so an old capture callback cannot close
    /// a replacement peer.
    @discardableResult
    func close(sessionID: String, generation: UInt64, reason: String) -> Bool {
        lock.lock()
        let matches = identity?.sessionID == sessionID && identity?.generation == generation
        lock.unlock()
        guard matches else { return false }
        close(reason: reason)
        return true
    }

    private func add(_ candidate: RTCIceCandidate, key: CandidateKey, peer: RTCPeerConnection, identity: PeerIdentity) {
        peer.add(candidate) { [weak self, weak peer] error in
            guard let self, let peer, self.isCurrent(peer: peer, identity: identity) else { return }
            if error != nil {
                self.emitError(peer: peer, identity: identity, reason: "candidate_rejected")
            }
            _ = key
        }
    }

    private func validSDP(_ value: String) -> Bool {
        // Swift treats CRLF as one Character; native WebRTC uses that line ending.
        value.utf8.count <= Self.maximumSDPBytes && !value.isEmpty && value.hasPrefix("v=0")
            && (value.contains("\r\na=") || value.contains("\na="))
            && !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control && $0 != "\r" && $0 != "\n" && $0 != "\t" })
    }

    private func validCandidate(_ value: String) -> Bool {
        value.utf8.count <= Self.maximumCandidateBytes && value.hasPrefix("candidate:")
            && !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
    }

    private func validOptionalText(_ value: String?, maximumBytes: Int) -> Bool {
        guard let value else { return true }
        return !value.isEmpty && value.utf8.count <= maximumBytes
            && !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
    }

    private func isCurrent(peer: RTCPeerConnection, identity: PeerIdentity) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return self.identity == identity && peerConnection === peer
    }

    private func currentIdentity(for peer: RTCPeerConnection) -> PeerIdentity? {
        lock.lock()
        defer { lock.unlock() }
        guard let identity, peerConnection === peer else { return nil }
        return identity
    }

    private func emitError(peer: RTCPeerConnection? = nil, identity: PeerIdentity? = nil, reason: String) {
        if let peer, let identity {
            emitForCurrentPeer(peer: peer, identity: identity, object: [
                "event": "signal.failed",
                "sessionID": identity.sessionID,
                "generation": identity.generation,
                "peerRevision": identity.token.uuidString,
                "reason": String(reason.prefix(128)),
            ])
            return
        }
        emitError(sessionID: nil, generation: nil, reason: reason)
    }

    private func emitError(sessionID: String?, generation: UInt64?, peerRevision: String? = nil, reason: String) {
        lock.lock()
        let current = identity
        lock.unlock()
        guard let current, (sessionID == nil || sessionID == current.sessionID),
              (generation == nil || generation == current.generation),
              (peerRevision == nil || peerRevision == current.token.uuidString) else { return }
        emit([
            "event": "signal.failed",
            "sessionID": current.sessionID,
            "generation": current.generation,
            "peerRevision": current.token.uuidString,
            "reason": String(reason.prefix(128)),
        ])
    }

    private func emitForCurrentPeer(peer: RTCPeerConnection, identity: PeerIdentity, object: [String: Any]) {
        guard isCurrent(peer: peer, identity: identity) else { return }
        emit(object)
    }

    private func emit(_ object: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(object) else { return }
        eventSink(object)
    }

    // MARK: RTCPeerConnectionDelegate

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {
        guard let identity = currentIdentity(for: peerConnection) else { return }
        let state: String
        switch newState {
        case .new: state = "new"
        case .checking: state = "checking"
        case .connected: state = "connected"
        case .completed: state = "completed"
        case .failed: state = "failed"
        case .disconnected: state = "disconnected"
        case .closed: state = "closed"
        case .count: state = "unknown"
        @unknown default: state = "unknown"
        }
        emitForCurrentPeer(peer: peerConnection, identity: identity, object: [
            "event": "signal.peerState",
            "sessionID": identity.sessionID,
            "generation": identity.generation,
            "peerRevision": identity.token.uuidString,
            "state": state,
        ])
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        guard let identity = currentIdentity(for: peerConnection),
              validCandidate(candidate.sdp), candidate.sdpMLineIndex >= 0,
              validOptionalText(candidate.sdpMid, maximumBytes: Self.maximumSDPMidBytes) else { return }
        emitForCurrentPeer(peer: peerConnection, identity: identity, object: [
            "event": "signal.candidate",
            "sessionID": identity.sessionID,
            "generation": identity.generation,
            "peerRevision": identity.token.uuidString,
            "candidate": candidate.sdp,
            "sdpMid": candidate.sdpMid ?? NSNull(),
            "sdpMLineIndex": candidate.sdpMLineIndex,
        ])
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        dataChannel.close()
    }
}
