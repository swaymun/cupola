import Foundation
@preconcurrency import WebRTC
import WonderPairing

public enum ComputerReceiverState: Equatable, Sendable {
    case preparing
    case awaitingSource
    case connecting
    case live
    case retrying(String)
    case failed(String)
    case closed

    /// The connection dropped and the computer is waiting for it to come back.
    public static let interruptedMessage = "The connection to your computer was interrupted. Reconnecting…"

    public var title: String {
        switch self {
        case .preparing: "Preparing"
        case .awaitingSource: "Choose a source on your computer"
        case .connecting: "Connecting"
        case .live: "Live"
        case .retrying: "Reconnecting"
        case .failed: "Couldn’t connect"
        case .closed: "Closed"
        }
    }

    public var message: String {
        switch self {
        case .preparing: "Preparing a live computer view…"
        case .awaitingSource: "Choose a screen or window from the computer menu."
        case .connecting: "Connecting to your computer…"
        case .live: "Live computer view"
        case .retrying(let reason): reason
        case .failed(let reason): reason
        case .closed: "Computer view closed"
        }
    }
}

/// Everything the receiver's published state is derived from. One owner
/// (`ComputerReceiver.recomputePresentationState`) reads these and nothing else,
/// so a transient signal such as one failed signaling poll cannot, on its own,
/// replace a picture that is still arriving.
struct ComputerPresentationInputs: Equatable, Sendable {
    /// The host's own report from signaling: preparing, awaitingSource, offerReady, live, reconnecting…
    var hostState: String
    var peerConnected: Bool
    /// WebRTC reported a disconnect and has not reconnected yet.
    var peerInterrupted: Bool
    var videoTrackAvailable: Bool
    /// Signaling polls that failed in a row; zero after any success.
    var consecutivePollFailures: Int
    /// A frame arrived within `liveFrameWindow`.
    var framesRecent: Bool

    /// Failed polls tolerated while frames keep arriving.
    static let livePollFailureTolerance = 3
    /// How fresh the last frame must be for a failing signaling channel not to matter.
    static let liveFrameWindow: Duration = .seconds(3)

    var canShowLive: Bool { peerConnected && videoTrackAvailable }

    /// A live picture that is still arriving is not undone by a failing poll
    /// until it fails `livePollFailureTolerance` times or frames stop.
    var holdsLiveDespitePollFailures: Bool {
        hostState == "live" && canShowLive && !peerInterrupted && framesRecent
            && consecutivePollFailures < Self.livePollFailureTolerance
    }

    static let unreachableMessage = "The computer could not be reached. Retrying…"

    /// Nil when nothing is wrong beyond the host's own state; the receiver then
    /// maps `hostState` to a presentation (and starts the connect deadline).
    var interruption: ComputerReceiverState? {
        if peerInterrupted || hostState == "reconnecting" { return .retrying(ComputerReceiverState.interruptedMessage) }
        if consecutivePollFailures > 0, !holdsLiveDespitePollFailures { return .retrying(Self.unreachableMessage) }
        return nil
    }
}

/// What the computer reports about the shown view besides its video.
public struct ComputerHostReport: Equatable, Sendable {
    /// The shared window is closed, minimized or hidden; its last frame stays on screen.
    public var windowHidden: Bool
    /// Typing focus is in a text field there. Nil when the computer doesn't say
    /// (Linux, Windows, an older Cupola, or no one controlling it).
    public var textInputFocused: Bool?

    public init(windowHidden: Bool = false, textInputFocused: Bool? = nil) {
        self.windowHidden = windowHidden
        self.textInputFocused = textInputFocused
    }
}

/// Failure words for the computer view. The computer sends sentences; an
/// older one may still send a bare code, which never reaches the screen.
public enum ComputerFailureText {
    public static let stoppedSharing = "The computer stopped sharing its screen."
    public static let couldNotStart = "The computer’s screen couldn’t be shown on this device."

    public static func plain(_ reason: String?) -> String {
        guard let reason = reason?.trimmingCharacters(in: .whitespacesAndNewlines),
              reason.contains(" ") else { return stoppedSharing }
        return reason
    }
}

public protocol ComputerSignalingTransport: Sendable {
    func poll(sessionID: String, request: ComputerSignalingPollRequest) async throws -> ComputerSignalingResponse
    func submitAnswer(sessionID: String, request: ComputerSignalingAnswerRequest) async throws -> ComputerSignalingMutationResponse
    func sendCandidate(sessionID: String, request: ComputerSignalingCandidateRequest) async throws -> ComputerSignalingMutationResponse
    func close(sessionID: String, request: ComputerSignalingCloseRequest) async throws -> ComputerSignalingMutationResponse
    /// The computer's own words when this session already ended there. The host
    /// stops answering signaling for an ended session, so a failed poll asks.
    func endedReason(sessionID: String, generation: UInt64) async -> String?
}

struct ComputerLocalCandidate: Sendable {
    let sdp: String
    let sdpMid: String?
    let sdpMLineIndex: UInt32
}

enum ComputerPeerState: Sendable {
    case connecting
    case connected
    case disconnected
    case failed
    case closed
}

/// Cumulative receiver counters from one statistics read.
struct ComputerStreamSample: Equatable, Sendable {
    var framesDecoded: Double = 0
    var framesDropped: Double = 0
    var packetsReceived: Double = 0
    var packetsLost: Double = 0
    var framesReceived: Double = 0
    /// Cumulative seconds frames waited in the jitter buffer, and how many left it.
    var jitterBufferDelay: Double = 0
    var jitterBufferEmittedCount: Double = 0
    /// Cumulative seconds spent decoding.
    var totalDecodeTime: Double = 0
    var roundTripMilliseconds: Double?
    /// Latest decoded frame size in pixels (not cumulative); 0 until the first frame.
    var frameWidth: Double = 0
    var frameHeight: Double = 0
    /// Negotiated codec ("H264", "VP8") and the decoder behind it ("VideoToolbox",
    /// "libvpx"), reduced to letters and digits; nil when WebRTC does not report one.
    var codec: String?
    var decoder: String?
    /// Cumulative distinct frames the display layer actually drew (at most the
    /// screen's refresh rate); the receiver fills it in, the peer's statistics do not know it.
    var framesDrawn: Double = 0
    /// `systemUptime` when the receiver read the statistics; 0 when unset.
    var takenAt: TimeInterval = 0
}

extension ComputerStreamSample {
    /// A codec or decoder name safe to use inside a metric key; nil when empty or "unknown".
    static func metricName(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let name = String(raw.unicodeScalars.filter { $0.isASCII && ($0.properties.isAlphabetic || ("0"..."9").contains($0)) }.prefix(32))
        return name.isEmpty || name.lowercased() == "unknown" ? nil : name
    }

    /// Per-interval latency figures for Diagnostics builds (milliseconds, frames per second).
    func intervalMetrics(since previous: ComputerStreamSample, seconds: Double, requestedFps: Int? = nil) -> [String: Double] {
        let decoded = framesDecoded - previous.framesDecoded
        let emitted = jitterBufferEmittedCount - previous.jitterBufferEmittedCount
        let rate = max(seconds, 0.001)
        var metrics: [String: Double] = [
            "fps": max(0, decoded) / rate,
            "inboundFps": max(0, framesReceived - previous.framesReceived) / rate,
            "drawFps": max(0, framesDrawn - previous.framesDrawn) / rate,
            "framesReceived": max(0, framesReceived - previous.framesReceived),
            "framesDecoded": max(0, decoded),
            "framesDropped": max(0, framesDropped - previous.framesDropped),
        ]
        if emitted > 0 { metrics["jitterBufferDelayMs"] = (jitterBufferDelay - previous.jitterBufferDelay) / emitted * 1_000 }
        if decoded > 0 { metrics["decodeMsPerFrame"] = (totalDecodeTime - previous.totalDecodeTime) / decoded * 1_000 }
        if let roundTripMilliseconds { metrics["rttMs"] = roundTripMilliseconds }
        if let requestedFps { metrics["requestedFps"] = Double(requestedFps) }
        if frameWidth > 0, frameHeight > 0 { metrics["frameWidth"] = frameWidth; metrics["frameHeight"] = frameHeight }
        // Metrics are numbers only, so a name rides in the key (the daemon allows letters, digits and . _ [ ]).
        if let codec { metrics["codec.\(codec)"] = 1 }
        if let decoder { metrics["decoder.\(decoder)"] = 1 }
        return metrics
    }
}

protocol ComputerPeer: AnyObject {
    func sample(_ completion: @escaping @Sendable (ComputerStreamSample?) -> Void)
    var onLocalCandidate: ((ComputerLocalCandidate) -> Void)? { get set }
    var onStateChange: ((ComputerPeerState) -> Void)? { get set }
    var onVideoTrack: (() -> Void)? { get set }

    func acceptOffer(_ sdp: String) async throws -> String
    func addRemoteCandidate(_ candidate: ComputerSignalingCandidate)
    func attach(renderer: RTCVideoRenderer)
    func detach(renderer: RTCVideoRenderer)
    func close()
}

extension ComputerPeer {
    func sample(_ completion: @escaping @Sendable (ComputerStreamSample?) -> Void) { completion(nil) }
}

protocol ComputerPeerFactory: Sendable {
    func makePeer() -> ComputerPeer?
}

private struct WebRTCPeerFactory: ComputerPeerFactory, Sendable {
    func makePeer() -> ComputerPeer? { WebRTCPeer.make() }
}

private enum ComputerPeerError: LocalizedError {
    case invalidOffer
    case unavailable
    case operationFailed(String)

    var errorDescription: String? {
        switch self {
        case .invalidOffer, .unavailable: ComputerFailureText.couldNotStart
        case .operationFailed(let reason): reason
        }
    }
}

/// The iOS answerer for the authenticated, LAN-only Mac publisher.
///
/// This type owns no input or data channel and keeps network/WebRTC work off
/// the caller's actor. A generation token makes every callback from an old
/// peer harmless after a replacement or dismissal.
public final class ComputerReceiver: @unchecked Sendable {
    public var onStateChange: ((ComputerReceiverState, UInt64) -> Void)?
    /// How smooth the live view is; reported every few seconds while live.
    public var onQualityChange: ((ComputerStreamQuality) -> Void)?
    /// Every 2 s while live: loss, round trip and drops, for the frame-rate policy.
    public var onNetworkSample: ((ComputerNetworkSample) -> Void)?
    /// The frame rate this view asked the computer for; "choppy" is measured against it.
    public var frameRateCeiling: Int {
        get { lock.withLock { requestedFrameRate } }
        set { lock.withLock { requestedFrameRate = newValue } }
    }
    private var requestedFrameRate = ComputerFrameRate.legacy
    /// Every 2 s while live: fps, jitter-buffer delay, decode time (Diagnostics builds read it).
    public var onStreamMetrics: (([String: Double]) -> Void)?
    /// A changed host report, with a revision that orders deliveries.
    public var onHostReportChange: ((ComputerHostReport, UInt64) -> Void)?

    private struct Identity: Equatable, Sendable {
        let token: UUID
        let sessionID: String
        let generation: UInt64
        let conversationID: String
        let hostInstallationID: String
        var peerRevision: String?
    }

    private let lock = NSLock()
    private let peerFactory: any ComputerPeerFactory
    private var identity: Identity?
    private var peer: (any ComputerPeer)?
    private var transport: (any ComputerSignalingTransport)?
    private var pollTask: Task<Void, Never>?
    private var state: ComputerReceiverState = .closed
    private var stateRevision: UInt64 = 0
    private var remoteDescriptionSet = false
    private var hostState = "preparing"
    private var hostReport = ComputerHostReport()
    private var hostReportRevision: UInt64 = 0
    private var peerConnected = false
    /// The connection dropped and has not come back yet. The last frame stays
    /// on screen (dimmed by the view) while WebRTC reconnects.
    private var peerInterrupted = false
    private var videoTrackAvailable = false
    private var consecutivePollFailures = 0
    private var lastFrameAt: ContinuousClock.Instant?
    private var pendingRemoteCandidates: [UInt64: ComputerSignalingCandidate] = [:]
    private var seenRemoteCandidates: [UInt64: ComputerSignalingCandidate] = [:]
    private var localCandidates: [UInt64: ComputerLocalCandidate] = [:]
    private var sentLocalCandidateSequences: Set<UInt64> = []
    private var nextLocalCandidateSequence: UInt64 = 0
    public let picture: ComputerPicture
    private var frameSink: ComputerFrameSink?
    private var interruptionTask: Task<Void, Never>?
    private let reconnectTimeout: Duration
    private var automaticResume = false
    public var canResumeAutomatically: Bool {
        lock.withLock { if case .failed = state { return automaticResume }; return false }
    }
    private var qualityTask: Task<Void, Never>?
    /// Distinct frames the display layer drew; guarded by `lock`. Drives the draw rate in Diagnostics.
    private var framesDrawn: Double = 0
    /// Timestamps of the last few frames drawn, so two views drawing out of order count a frame once.
    private var recentDrawnTimestamps: [Int64] = []
    /// Bumped by `noteActivity()`; a change keeps signaling at its quick pace.
    private var activityRevision: UInt64 = 0

    public init(picture: ComputerPicture = ComputerPicture()) {
        self.picture = picture
        reconnectTimeout = .seconds(12)
        peerFactory = WebRTCPeerFactory()
    }

    internal init(peerFactory: any ComputerPeerFactory, reconnectTimeout: Duration = .seconds(12)) {
        picture = ComputerPicture()
        self.reconnectTimeout = reconnectTimeout
        self.peerFactory = peerFactory
    }

    /// The person is using the view (input, clipboard): poll at the quick pace
    /// so clipboard and text-focus updates arrive promptly.
    public func noteActivity() {
        lock.withLock { activityRevision &+= 1 }
    }

    private func currentActivity() -> UInt64 {
        lock.withLock { activityRevision }
    }

    public var currentState: ComputerReceiverState {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    public func start(session: ComputerSession, transport: any ComputerSignalingTransport) {
        ComputerOpenTimeline.shared.mark("receiver-start")
        reset(reason: "replaced", publishClosed: false, keepingLastFrame: true)
        guard session.capability.available else {
            publish(.failed(session.capability.reason))
            return
        }

        let token = UUID()
        guard let peer = peerFactory.makePeer() else {
            publish(.failed(ComputerFailureText.couldNotStart))
            return
        }
        let identity = Identity(
            token: token,
            sessionID: session.id,
            generation: session.generation,
            conversationID: session.conversationId,
            hostInstallationID: session.hostInstallationId,
            peerRevision: nil
        )
        peer.onLocalCandidate = { [weak self] candidate in
            self?.recordLocalCandidate(candidate, token: token)
        }
        peer.onStateChange = { [weak self] state in
            self?.peerStateChanged(state, token: token)
        }
        peer.onVideoTrack = { [weak self] in
            self?.videoTrackReceived(token: token)
        }

        lock.lock()
        self.identity = identity
        self.peer = peer
        self.transport = transport
        self.remoteDescriptionSet = false
        self.hostState = "preparing"
        self.hostReport = ComputerHostReport()
        self.peerConnected = false
        self.peerInterrupted = false
        self.videoTrackAvailable = false
        self.consecutivePollFailures = 0
        self.lastFrameAt = nil
        self.pendingRemoteCandidates.removeAll(keepingCapacity: true)
        self.seenRemoteCandidates.removeAll(keepingCapacity: true)
        self.localCandidates.removeAll(keepingCapacity: true)
        self.sentLocalCandidateSequences.removeAll(keepingCapacity: true)
        self.nextLocalCandidateSequence = 0
        self.automaticResume = false
        let sink = ComputerFrameSink { [weak self] frame in self?.receiveFrame(frame, token: token) }
        self.frameSink = sink
        lock.unlock()

        publish(.preparing)
        peer.attach(renderer: sink)
        pollTask = Task.detached(priority: .userInitiated) { [weak self] in
            await self?.pollLoop(session: session, token: token, peer: peer, transport: transport)
        }
        qualityTask = Task.detached(priority: .utility) { [weak self] in
            var previous: ComputerStreamSample?
            var reported: ComputerStreamQuality?
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                guard !Task.isCancelled, let self, self.isCurrent(token: token) else { return }
                guard self.currentState == .live else { previous = nil; continue }
                guard let sample = await self.sample(token: token) else { continue }
                defer { previous = sample }
                guard let previous else { continue }
                guard self.isCurrent(token: token) else { return }
                let elapsed = sample.takenAt - previous.takenAt
                let ceiling = self.frameRateCeiling
                self.onStreamMetrics?(sample.intervalMetrics(since: previous, seconds: elapsed > 0 ? elapsed : 2, requestedFps: ceiling))
                self.onNetworkSample?(Self.networkSample(from: previous, to: sample, seconds: elapsed > 0 ? elapsed : 2))
                let quality = Self.quality(from: previous, to: sample, seconds: 2, frameRateCeiling: ceiling)
                guard quality != reported else { continue }
                reported = quality
                self.onQualityChange?(quality)
            }
        }
    }

    private func sample(token: UUID) async -> ComputerStreamSample? {
        guard let peer = lock.withLock({ identity?.token == token ? self.peer : nil }) else { return nil }
        return await withCheckedContinuation { (continuation: CheckedContinuation<ComputerStreamSample?, Never>) in
            peer.sample { sample in
                var sample = sample
                sample?.takenAt = ProcessInfo.processInfo.systemUptime
                sample?.framesDrawn = self.drawnFrameCount
                continuation.resume(returning: sample)
            }
        }
    }

    /// Loss, round trip and drops over the interval between two cumulative samples.
    static func networkSample(from previous: ComputerStreamSample, to current: ComputerStreamSample,
                              seconds: Double) -> ComputerNetworkSample {
        let received = max(0, current.packetsReceived - previous.packetsReceived)
        let lost = max(0, current.packetsLost - previous.packetsLost)
        return ComputerNetworkSample(
            seconds: seconds,
            lossFraction: received + lost > 0 ? lost / (received + lost) : 0,
            roundTripMilliseconds: current.roundTripMilliseconds,
            framesDropped: max(0, current.framesDropped - previous.framesDropped)
        )
    }

    /// The quality over the interval between two cumulative samples.
    static func quality(from previous: ComputerStreamSample, to current: ComputerStreamSample,
                        seconds: Double, frameRateCeiling: Int? = nil) -> ComputerStreamQuality {
        let network = networkSample(from: previous, to: current, seconds: seconds)
        return ComputerStreamQuality.classify(
            framesPerSecond: max(0, current.framesDecoded - previous.framesDecoded) / max(seconds, 0.001),
            framesDropped: network.framesDropped,
            lossFraction: network.lossFraction,
            roundTripMilliseconds: network.roundTripMilliseconds,
            frameRateCeiling: frameRateCeiling
        )
    }

    public func close(reason: String = "closed_by_viewer", keepingLastFrame: Bool = false) {
        reset(reason: reason, publishClosed: true, keepingLastFrame: keepingLastFrame)
    }

    private func reset(reason: String, publishClosed: Bool, keepingLastFrame: Bool) {
        lock.lock()
        let oldIdentity = identity
        let oldTransport = transport
        let oldPeer = peer
        let oldSink = frameSink
        frameSink = nil
        interruptionTask?.cancel()
        interruptionTask = nil
        pollTask?.cancel()
        pollTask = nil
        qualityTask?.cancel()
        qualityTask = nil
        identity = nil
        peer = nil
        transport = nil
        remoteDescriptionSet = false
        hostState = "preparing"
        hostReport = ComputerHostReport()
        peerConnected = false
        peerInterrupted = false
        videoTrackAvailable = false
        consecutivePollFailures = 0
        lastFrameAt = nil
        automaticResume = false
        pendingRemoteCandidates.removeAll(keepingCapacity: true)
        seenRemoteCandidates.removeAll(keepingCapacity: true)
        localCandidates.removeAll(keepingCapacity: true)
        sentLocalCandidateSequences.removeAll(keepingCapacity: true)
        nextLocalCandidateSequence = 0
        lock.unlock()

        if let oldSink { oldPeer?.detach(renderer: oldSink) }
        if !keepingLastFrame { picture.clear() }
        if let oldPeer { oldPeer.close() }
        if publishClosed { publish(.closed) }

        guard let oldIdentity, let oldTransport, let peerRevision = oldIdentity.peerRevision,
              let request = ComputerSignalingCloseRequest(
                binding: ComputerSignalingBinding(
                    generation: oldIdentity.generation,
                    conversationId: oldIdentity.conversationID,
                    hostInstallationId: oldIdentity.hostInstallationID,
                    peerRevision: peerRevision
                ), reason: reason
              ) else { return }
        Task.detached(priority: .utility) {
            _ = try? await oldTransport.close(sessionID: oldIdentity.sessionID, request: request)
        }
    }

    internal func attach(renderer: RTCVideoRenderer) {
        picture.attach(renderer)
    }

    internal func detach(renderer: RTCVideoRenderer) {
        picture.detach(renderer)
    }

    /// A display view drew the frame with this timestamp. Two views showing
    /// the picture, or a redraw of the same frame, count once, in either order.
    internal func noteFrameDrawn(timestampNs: Int64) {
        lock.withLock {
            guard !recentDrawnTimestamps.contains(timestampNs) else { return }
            recentDrawnTimestamps.append(timestampNs)
            if recentDrawnTimestamps.count > 8 { recentDrawnTimestamps.removeFirst() }
            framesDrawn += 1
        }
    }

    /// Distinct frames drawn since the session began.
    internal var drawnFrameCount: Double { lock.withLock { framesDrawn } }

    private func receiveFrame(_ frame: RTCVideoFrame?, token: UUID) {
        guard let frame else { return }
        let accepted = lock.withLock { () -> Bool in
            guard identity?.token == token else { return false }
            if case .failed = state { return false }
            if state == .closed { return false }
            picture.renderFrame(frame)
            videoTrackAvailable = true
            lastFrameAt = .now
            ComputerOpenTimeline.shared.mark("first-frame")
            return true
        }
        if accepted { recomputePresentationState(token: token) }
    }

    private func pollLoop(session: ComputerSession, token: UUID, peer: any ComputerPeer,
                          transport: any ComputerSignalingTransport) async {
        var cursor: UInt64 = 0
        var retries = 0
        var pendingAnswer: String?
        var remoteOfferSDP: String?
        var staleResponseCount = 0
        var cadence = ComputerSignalingCadence()
        var lastSignature: ComputerSignalingCadence.Signature?
        var lastActivity = currentActivity()

        while !Task.isCancelled, isCurrent(token: token) {
            if case .failed = currentState { return }
            if case .closed = currentState { return }
            do {
                let binding = currentBinding(session: session, token: token)
                let response = try await transport.poll(
                    sessionID: session.id,
                    request: ComputerSignalingPollRequest(binding: binding, cursor: cursor)
                )
                guard !Task.isCancelled, isCurrent(token: token) else { return }
                guard response.sessionId == session.id,
                      response.generation == session.generation else {
                    staleResponseCount += 1
                    if staleResponseCount > 6 {
                        fail("The computer view changed on your computer. Close this screen and try again.", token: token, peer: peer)
                        return
                    }
                    try? await Task.sleep(for: .milliseconds(100))
                    continue
                }
                staleResponseCount = 0
                retries = 0
                setPollFailures(0, token: token)

                if let revision = response.peerRevision {
                    guard adoptPeerRevision(revision, token: token) else {
                        // A response from an old/replaced publisher must not
                        // alter this peer or move its stateless cursor.
                        try? await Task.sleep(for: .milliseconds(100))
                        continue
                    }
                }
                if !response.iceServers.isEmpty {
                    fail("This computer view requires a direct local connection.", token: token, peer: peer)
                    return
                }
                if response.state == "failed" {
                    fail(ComputerFailureText.plain(response.failureReason), token: token, peer: peer)
                    return
                }
                if response.state == "closed" {
                    finishClosed(token: token, peer: peer)
                    return
                }
                updateHostState(response.state, token: token)
                updateHostReport(ComputerHostReport(windowHidden: response.captureState == "suspended",
                                                    textInputFocused: response.textInputFocused), token: token)

                if let offer = response.offer {
                    ComputerOpenTimeline.shared.mark("offer-received")
                    guard offer.type == "offer", currentBinding(session: session, token: token).peerRevision != nil else {
                        fail(ComputerFailureText.couldNotStart, token: token, peer: peer)
                        return
                    }
                    if let remoteOfferSDP, remoteOfferSDP != offer.sdp {
                        fail("The computer’s screen changed while connecting.", token: token, peer: peer)
                        return
                    }
                    if remoteOfferSDP == nil {
                        do {
                            pendingAnswer = try await peer.acceptOffer(offer.sdp)
                            guard !Task.isCancelled, isCurrent(token: token) else { return }
                            remoteOfferSDP = offer.sdp
                            markRemoteDescriptionSet(token: token, peer: peer)
                        } catch {
                            // WebRTC's own errors describe SDP, not anything a person can act on.
                            fail(ComputerFailureText.couldNotStart, token: token, peer: peer)
                            return
                        }
                    }
                }

                for candidate in response.candidates {
                    receiveRemoteCandidate(candidate, token: token, peer: peer)
                }

                if let answer = pendingAnswer {
                    guard let request = ComputerSignalingAnswerRequest(
                        binding: currentBinding(session: session, token: token), sdp: answer
                    ) else { return }
                    do {
                        let result = try await transport.submitAnswer(sessionID: session.id, request: request)
                        guard result.accepted else { throw ComputerPeerError.operationFailed("The computer did not accept the stream answer.") }
                        pendingAnswer = nil
                        ComputerOpenTimeline.shared.mark("answer-accepted")
                    } catch {
                        publish(.retrying("Connecting to your computer…"), token: token)
                        try? await Task.sleep(for: .milliseconds(250))
                        continue
                    }
                }

                try await sendPendingLocalCandidates(session: session, token: token, transport: transport)
                let signature = ComputerSignalingCadence.Signature(
                    state: response.state, captureState: response.captureState, peerState: response.peerState,
                    textInputFocused: response.textInputFocused, offered: response.offer != nil,
                    candidates: response.candidates.count, cursor: response.nextCursor)
                let activity = currentActivity()
                let changed = signature != lastSignature || activity != lastActivity
                lastSignature = signature
                lastActivity = activity
                cursor = max(cursor, response.nextCursor)
                try await Task.sleep(for: cadence.next(live: currentState == .live, changed: changed))
            } catch is CancellationError {
                return
            } catch {
                if let reason = await transport.endedReason(sessionID: session.id, generation: session.generation) {
                    guard isCurrent(token: token) else { return }
                    fail(ComputerFailureText.plain(reason), token: token, peer: peer)
                    return
                }
                retries += 1
                if retries > 6 {
                    fail("The computer could not be reached. Check that Cupola is running on the same local network, then try again.", token: token, peer: peer, recoverable: true)
                    return
                }
                let seconds = min(4.0, 0.25 * pow(2.0, Double(retries - 1)))
                setPollFailures(retries, token: token)
                try? await Task.sleep(for: .seconds(seconds))
            }
        }
    }

    private func currentBinding(session: ComputerSession, token: UUID) -> ComputerSignalingBinding {
        lock.lock()
        let revision = identity?.token == token ? identity?.peerRevision : nil
        lock.unlock()
        return ComputerSignalingBinding(
            generation: session.generation,
            conversationId: session.conversationId,
            hostInstallationId: session.hostInstallationId,
            peerRevision: revision
        )
    }

    private func adoptPeerRevision(_ revision: String, token: UUID) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard identity?.token == token else { return false }
        guard revision.utf8.count <= 128, !revision.isEmpty else { return false }
        if let current = identity?.peerRevision { return current == revision }
        identity?.peerRevision = revision
        return true
    }

    private func recordLocalCandidate(_ candidate: ComputerLocalCandidate, token: UUID) {
        guard candidate.sdp.hasPrefix("candidate:"), candidate.sdp.utf8.count <= 4 * 1024 else { return }
        lock.lock()
        guard identity?.token == token, nextLocalCandidateSequence < 128 else {
            lock.unlock()
            return
        }
        nextLocalCandidateSequence += 1
        localCandidates[nextLocalCandidateSequence] = candidate
        lock.unlock()
    }

    private func sendPendingLocalCandidates(session: ComputerSession, token: UUID,
                                            transport: any ComputerSignalingTransport) async throws {
        let pending: [(UInt64, ComputerLocalCandidate, ComputerSignalingCandidateRequest)] = {
            lock.lock()
            defer { lock.unlock() }
            guard identity?.token == token, let peerRevision = identity?.peerRevision else { return [] }
            return localCandidates.keys.sorted().compactMap { sequence in
                guard !sentLocalCandidateSequences.contains(sequence), let candidate = localCandidates[sequence],
                      let request = ComputerSignalingCandidateRequest(
                        binding: ComputerSignalingBinding(
                            generation: session.generation,
                            conversationId: session.conversationId,
                            hostInstallationId: session.hostInstallationId,
                            peerRevision: peerRevision
                        ), sequence: sequence, candidate: candidate.sdp,
                        sdpMid: candidate.sdpMid, sdpMLineIndex: candidate.sdpMLineIndex
                      ) else { return nil }
                return (sequence, candidate, request)
            }
        }()
        for (sequence, _, request) in pending {
            guard !Task.isCancelled, isCurrent(token: token) else { return }
            let result = try await transport.sendCandidate(sessionID: session.id, request: request)
            guard !Task.isCancelled, isCurrent(token: token) else { return }
            guard result.accepted else { throw ComputerPeerError.operationFailed("The computer did not accept an ICE candidate.") }
            markLocalCandidateSent(sequence, token: token)
        }
    }

    private func markLocalCandidateSent(_ sequence: UInt64, token: UUID) {
        lock.lock()
        if identity?.token == token { sentLocalCandidateSequences.insert(sequence) }
        lock.unlock()
    }

    private func receiveRemoteCandidate(_ candidate: ComputerSignalingCandidate, token: UUID, peer: any ComputerPeer) {
        guard candidate.sequence > 0, candidate.sequence <= 128,
              candidate.candidate.hasPrefix("candidate:"), candidate.candidate.utf8.count <= 4 * 1024,
              candidate.sdpMLineIndex <= UInt32(Int32.max) else { return }
        lock.lock()
        guard identity?.token == token else {
            lock.unlock()
            return
        }
        if let existing = seenRemoteCandidates[candidate.sequence] {
            lock.unlock()
            if existing != candidate {
                fail(ComputerFailureText.couldNotStart, token: token, peer: peer)
            }
            return
        }
        seenRemoteCandidates[candidate.sequence] = candidate
        if remoteDescriptionSet {
            lock.unlock()
            peer.addRemoteCandidate(candidate)
        } else {
            pendingRemoteCandidates[candidate.sequence] = candidate
            lock.unlock()
        }
    }

    private func markRemoteDescriptionSet(token: UUID, peer: any ComputerPeer) {
        lock.lock()
        guard identity?.token == token else {
            lock.unlock()
            return
        }
        remoteDescriptionSet = true
        let pending = pendingRemoteCandidates.values.sorted { $0.sequence < $1.sequence }
        pendingRemoteCandidates.removeAll(keepingCapacity: true)
        lock.unlock()
        for candidate in pending { peer.addRemoteCandidate(candidate) }
    }

    private func peerStateChanged(_ state: ComputerPeerState, token: UUID) {
        guard isCurrent(token: token) else { return }
        switch currentState {
        case .failed, .closed: return
        default: break
        }
        switch state {
        case .connecting:
            setPeerConnected(false, token: token)
            recomputePresentationState(token: token)
        case .connected:
            ComputerOpenTimeline.shared.mark("peer-connected")
            setPeerConnected(true, token: token)
            recomputePresentationState(token: token)
        case .disconnected:
            // Usually a Wi-Fi roam: WebRTC reconnects on its own, and the
            // computer keeps the view for a while. Keep the last frame.
            setPeerConnected(false, interrupted: true, token: token)
            recomputePresentationState(token: token)
        case .failed:
            if let peer = lock.withLock({ self.peer }) {
                fail("The connection to your computer failed. Check the local network and try again.", token: token, peer: peer, recoverable: true)
            }
        case .closed:
            if let peer = lock.withLock({ self.peer }) {
                fail("The connection to your computer closed. Try again to reopen the view.", token: token, peer: peer, recoverable: true)
            }
        }
    }

    private func videoTrackReceived(token: UUID) {
        guard isCurrent(token: token) else { return }
        switch currentState {
        case .failed, .closed: return
        default: break
        }
        // A track alone is not evidence that a picture arrived.
        recomputePresentationState(token: token)
    }

    private func updateHostState(_ next: String, token: UUID) {
        lock.lock()
        guard identity?.token == token else {
            lock.unlock()
            return
        }
        if hostState == "reconnecting", next != "reconnecting" { videoTrackAvailable = false }
        hostState = next
        lock.unlock()
        recomputePresentationState(token: token)
    }

    private func updateHostReport(_ next: ComputerHostReport, token: UUID) {
        lock.lock()
        guard identity?.token == token, hostReport != next else {
            lock.unlock()
            return
        }
        hostReport = next
        hostReportRevision += 1
        let revision = hostReportRevision
        let callback = onHostReportChange
        lock.unlock()
        callback?(next, revision)
    }

    private func setPeerConnected(_ connected: Bool, interrupted: Bool? = nil, token: UUID) {
        lock.lock()
        if identity?.token == token {
            peerConnected = connected
            if connected {
                if peerInterrupted { videoTrackAvailable = false }
                peerInterrupted = false
            } else if let interrupted {
                peerInterrupted = interrupted
            }
        }
        lock.unlock()
    }

    private func setPollFailures(_ count: Int, token: UUID) {
        let changed = lock.withLock { () -> Bool in
            guard identity?.token == token, consecutivePollFailures != count else { return false }
            consecutivePollFailures = count
            return true
        }
        if changed { recomputePresentationState(token: token) }
    }

    /// The only place the published state is chosen: inputs in, one state out.
    private func recomputePresentationState(token: UUID) {
        lock.lock()
        guard identity?.token == token else {
            lock.unlock()
            return
        }
        if case .failed = state { lock.unlock(); return }
        if state == .closed { lock.unlock(); return }
        let inputs = ComputerPresentationInputs(
            hostState: hostState, peerConnected: peerConnected, peerInterrupted: peerInterrupted,
            videoTrackAvailable: videoTrackAvailable, consecutivePollFailures: consecutivePollFailures,
            framesRecent: lastFrameAt.map { .now - $0 < ComputerPresentationInputs.liveFrameWindow } ?? false)
        lock.unlock()
        if let interruption = inputs.interruption {
            publish(interruption, token: token)
            // Poll failures end through the poll loop's retry limit, not the connect deadline.
            if inputs.peerInterrupted || inputs.hostState == "reconnecting" { beginInterruptionDeadline(token: token) }
            return
        }
        switch inputs.hostState {
        case "awaitingSource":
            lock.withLock { interruptionTask?.cancel(); interruptionTask = nil }
            publish(.awaitingSource, token: token)
        case "preparing":
            publish(.preparing, token: token)
            beginInterruptionDeadline(token: token)
        case "live" where inputs.canShowLive:
            lock.withLock { interruptionTask?.cancel(); interruptionTask = nil }
            publish(.live, token: token)
        default:
            publish(.connecting, token: token)
            beginInterruptionDeadline(token: token)
        }
    }

    private func beginInterruptionDeadline(token: UUID) {
        lock.withLock {
            guard identity?.token == token, interruptionTask == nil else { return }
            interruptionTask = Task.detached { [weak self, reconnectTimeout] in
                do { try await Task.sleep(for: reconnectTimeout) } catch { return }
                guard let self, let peer = self.lock.withLock({ self.identity?.token == token ? self.peer : nil }) else { return }
                self.fail("The computer view couldn’t connect. Check the network and that Cupola is running on your computer, then try again.", token: token, peer: peer, recoverable: true, fromDeadline: true)
            }
        }
    }

    private func fail(_ message: String, token: UUID, peer: any ComputerPeer, recoverable: Bool = false, fromDeadline: Bool = false) {
        guard isCurrent(token: token) else { return }
        lock.withLock { automaticResume = recoverable }
        guard publish(.failed(message), token: token, fromDeadline: fromDeadline) else { return }
        lock.withLock {
            interruptionTask?.cancel(); interruptionTask = nil
            qualityTask?.cancel(); qualityTask = nil
            pollTask?.cancel(); pollTask = nil
        }
        peer.close()
    }

    private func finishClosed(token: UUID, peer: any ComputerPeer) {
        guard isCurrent(token: token) else { return }
        lock.withLock {
            automaticResume = false
            interruptionTask?.cancel(); interruptionTask = nil
            qualityTask?.cancel(); qualityTask = nil
        }
        publish(.closed, token: token)
        peer.close()
    }

    private func isCurrent(token: UUID) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return identity?.token == token
    }

    @discardableResult private func publish(_ next: ComputerReceiverState, token: UUID? = nil, fromDeadline: Bool = false) -> Bool {
        lock.lock()
        if let token, identity?.token != token {
            lock.unlock()
            return false
        }
        if token != nil {
            switch state {
            case .failed, .closed: lock.unlock(); return false
            default: break
            }
        }
        if fromDeadline, interruptionTask == nil || state == .live || state == .awaitingSource {
            lock.unlock()
            return false
        }
        guard state != next else {
            lock.unlock()
            return false
        }
        state = next
        stateRevision += 1
        let revision = stateRevision
        let callback = onStateChange
        lock.unlock()
        callback?(next, revision)
        return true
    }
}

/// A peer owns this generation-bound sink, never the retained picture itself.
private final class ComputerFrameSink: NSObject, RTCVideoRenderer {
    let receive: (RTCVideoFrame?) -> Void
    init(receive: @escaping (RTCVideoFrame?) -> Void) { self.receive = receive }
    func setSize(_ size: CGSize) {}
    func renderFrame(_ frame: RTCVideoFrame?) { receive(frame) }
}

private final class WebRTCPeer: NSObject, ComputerPeer, RTCPeerConnectionDelegate, @unchecked Sendable {
    private static let factory: RTCPeerConnectionFactory = {
        RTCInitializeSSL()
        return RTCPeerConnectionFactory(
            encoderFactory: RTCDefaultVideoEncoderFactory(),
            decoderFactory: RTCDefaultVideoDecoderFactory()
        )
    }()

    private let lock = NSLock()
    private let peerConnection: RTCPeerConnection
    private var videoTrack: RTCVideoTrack?
    private var renderer: RTCVideoRenderer?

    var onLocalCandidate: ((ComputerLocalCandidate) -> Void)?
    var onStateChange: ((ComputerPeerState) -> Void)?
    var onVideoTrack: (() -> Void)?

    static func make() -> WebRTCPeer? {
        let configuration = RTCConfiguration()
        configuration.iceServers = []
        configuration.sdpSemantics = .unifiedPlan
        configuration.continualGatheringPolicy = .gatherContinually
        let constraints = RTCMediaConstraints(
            mandatoryConstraints: nil,
            optionalConstraints: ["DtlsSrtpKeyAgreement": kRTCMediaConstraintsValueTrue]
        )
        return WebRTCPeer(configuration: configuration, constraints: constraints)
    }

    private init?(configuration: RTCConfiguration, constraints: RTCMediaConstraints) {
        guard let peer = Self.factory.peerConnection(with: configuration, constraints: constraints, delegate: nil) else { return nil }
        peerConnection = peer
        super.init()
        peer.delegate = self
    }

    func acceptOffer(_ sdp: String) async throws -> String {
        guard sdp.utf8.count <= 64 * 1024, sdp.hasPrefix("v=0"), sdp.contains("m=video"),
              !sdp.contains("m=audio"), !sdp.contains("m=application") else {
            throw ComputerPeerError.invalidOffer
        }
        try await setRemoteDescription(RTCSessionDescription(type: .offer, sdp: sdp))
        let constraints = RTCMediaConstraints(
            mandatoryConstraints: [kRTCMediaConstraintsOfferToReceiveVideo: kRTCMediaConstraintsValueTrue],
            optionalConstraints: nil
        )
        let answer = try await createAnswer(constraints: constraints)
        try await setLocalDescription(answer)
        return answer.sdp
    }

    func sample(_ completion: @escaping @Sendable (ComputerStreamSample?) -> Void) {
        peerConnection.statistics { report in
            var sample = ComputerStreamSample()
            var foundVideo = false
            var codecID: String?
            for statistic in report.statistics.values {
                let values = statistic.values
                func number(_ key: String) -> Double? { (values[key] as? NSNumber)?.doubleValue }
                if statistic.type == "inbound-rtp", (values["kind"] as? String) == "video" {
                    foundVideo = true
                    sample.framesDecoded += number("framesDecoded") ?? 0
                    sample.framesDropped += number("framesDropped") ?? 0
                    sample.packetsReceived += number("packetsReceived") ?? 0
                    sample.packetsLost += max(0, number("packetsLost") ?? 0)
                    sample.framesReceived += number("framesReceived") ?? 0
                    sample.jitterBufferDelay += number("jitterBufferDelay") ?? 0
                    sample.jitterBufferEmittedCount += number("jitterBufferEmittedCount") ?? 0
                    sample.totalDecodeTime += number("totalDecodeTime") ?? 0
                    sample.frameWidth = number("frameWidth") ?? sample.frameWidth
                    sample.frameHeight = number("frameHeight") ?? sample.frameHeight
                    sample.decoder = ComputerStreamSample.metricName(values["decoderImplementation"] as? String)
                    codecID = values["codecId"] as? String
                } else if statistic.type == "candidate-pair", (values["nominated"] as? NSNumber)?.boolValue == true,
                          let roundTrip = number("currentRoundTripTime") {
                    sample.roundTripMilliseconds = roundTrip * 1_000
                }
            }
            if let codecID, let mime = report.statistics[codecID]?.values["mimeType"] as? String {
                sample.codec = ComputerStreamSample.metricName(mime.split(separator: "/").last.map(String.init))
            }
            completion(foundVideo ? sample : nil)
        }
    }

    func addRemoteCandidate(_ candidate: ComputerSignalingCandidate) {
        let ice = RTCIceCandidate(
            sdp: candidate.candidate,
            sdpMLineIndex: Int32(candidate.sdpMLineIndex),
            sdpMid: candidate.sdpMid
        )
        peerConnection.add(ice) { _ in }
    }

    func attach(renderer: RTCVideoRenderer) {
        lock.lock()
        if self.renderer === renderer {
            lock.unlock()
            return
        }
        let old = self.renderer
        self.renderer = renderer
        let track = videoTrack
        lock.unlock()
        old.map { track?.remove($0) }
        if let track { track.add(renderer) }
    }

    func detach(renderer: RTCVideoRenderer) {
        lock.lock()
        guard self.renderer === renderer else {
            lock.unlock()
            return
        }
        self.renderer = nil
        let track = videoTrack
        lock.unlock()
        track?.remove(renderer)
    }

    func close() {
        lock.lock()
        let currentRenderer = renderer
        let currentTrack = videoTrack
        renderer = nil
        videoTrack = nil
        lock.unlock()
        if let currentRenderer { currentTrack?.remove(currentRenderer) }
        currentRenderer?.renderFrame(nil)
        peerConnection.close()
    }

    private func setRemoteDescription(_ description: RTCSessionDescription) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            peerConnection.setRemoteDescription(description) { error in
                if let error { continuation.resume(throwing: error) }
                else { continuation.resume(returning: ()) }
            }
        }
    }

    private func createAnswer(constraints: RTCMediaConstraints) async throws -> RTCSessionDescription {
        try await withCheckedThrowingContinuation { continuation in
            peerConnection.answer(for: constraints) { answer, error in
                if let error { continuation.resume(throwing: error) }
                else if let answer { continuation.resume(returning: answer) }
                else { continuation.resume(throwing: ComputerPeerError.unavailable) }
            }
        }
    }

    private func setLocalDescription(_ description: RTCSessionDescription) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            peerConnection.setLocalDescription(description) { error in
                if let error { continuation.resume(throwing: error) }
                else { continuation.resume(returning: ()) }
            }
        }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {
        let state: ComputerPeerState
        switch newState {
        case .new, .checking: state = .connecting
        case .connected, .completed: state = .connected
        case .disconnected: state = .disconnected
        case .failed: state = .failed
        case .closed: state = .closed
        case .count: return
        @unknown default: return
        }
        onStateChange?(state)
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        guard candidate.sdp.hasPrefix("candidate:"), candidate.sdp.utf8.count <= 4 * 1024,
              candidate.sdpMLineIndex >= 0 else { return }
        onLocalCandidate?(ComputerLocalCandidate(
            sdp: candidate.sdp,
            sdpMid: candidate.sdpMid,
            sdpMLineIndex: UInt32(candidate.sdpMLineIndex)
        ))
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        dataChannel.close()
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd rtpReceiver: RTCRtpReceiver,
                        streams: [RTCMediaStream]) {
        guard let track = rtpReceiver.track as? RTCVideoTrack else { return }
        lock.lock()
        videoTrack = track
        let currentRenderer = renderer
        lock.unlock()
        if let currentRenderer { track.add(currentRenderer) }
        onVideoTrack?()
    }
}

/// How long the receiver waits between signaling polls. Connecting polls
/// quickly; a live view polls every 500 ms while anything changes (the person's
/// input, the host's state, the clipboard) and eases to `idleMaximum` after a
/// few quiet polls. Each poll also renews the 30 s viewer lease on the
/// computer, so the slowest pace stays far inside it.
public struct ComputerSignalingCadence: Sendable {
    public static let connecting: Duration = .milliseconds(150)
    public static let active: Duration = .milliseconds(500)
    public static let idleMaximum: Duration = .seconds(2)
    /// Quiet live polls before each doubling of the wait.
    public static let quietPollsPerStep = 4

    /// What a poll reported that would make a change visible.
    public struct Signature: Equatable, Sendable {
        public var state: String
        public var captureState: String
        public var peerState: String
        public var textInputFocused: Bool?
        public var offered: Bool
        public var candidates: Int
        public var cursor: UInt64

        public init(state: String, captureState: String, peerState: String, textInputFocused: Bool?,
                    offered: Bool, candidates: Int, cursor: UInt64) {
            self.state = state
            self.captureState = captureState
            self.peerState = peerState
            self.textInputFocused = textInputFocused
            self.offered = offered
            self.candidates = candidates
            self.cursor = cursor
        }
    }

    private var quietPolls = 0

    public init() {}

    public mutating func next(live: Bool, changed: Bool) -> Duration {
        guard live, !changed else {
            quietPolls = 0
            return live ? Self.active : Self.connecting
        }
        quietPolls = min(quietPolls + 1, 64)
        var delay = Self.active
        for _ in 0..<(quietPolls / Self.quietPollsPerStep) where delay < Self.idleMaximum { delay *= 2 }
        return min(delay, Self.idleMaximum)
    }
}
