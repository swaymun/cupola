import Foundation

public enum ComputerSessionState: String, Codable, Sendable, CaseIterable, Equatable {
    case preparing
    case awaitingSource
    case live
    case paused
    case stale
    case ended
    case failed
    case unavailable

    public var title: String {
        switch self {
        case .preparing: "Preparing"
        case .awaitingSource: "Choose a source on your computer"
        case .live: "Live"
        case .paused: "Paused"
        case .stale: "Stale"
        case .ended: "Ended"
        case .failed: "Couldn’t connect"
        case .unavailable: "Unavailable"
        }
    }
}

public struct ComputerCapability: Codable, Equatable, Sendable {
    public let available: Bool
    public let action: String
    public let reason: String

    public init(available: Bool, action: String, reason: String) {
        self.available = available
        self.action = action
        self.reason = reason
    }
}

public struct ComputerControlCapability: Codable, Equatable, Sendable {
    public let available: Bool
    public let action: String
    public let reason: String
    public let heartbeatIntervalSeconds: UInt64
    public let leaseExpirySeconds: UInt64

    public init(available: Bool, action: String, reason: String,
                heartbeatIntervalSeconds: UInt64 = 3, leaseExpirySeconds: UInt64 = 10) {
        self.available = available
        self.action = action
        self.reason = reason
        self.heartbeatIntervalSeconds = heartbeatIntervalSeconds
        self.leaseExpirySeconds = leaseExpirySeconds
    }

    public static let unavailable = Self(
        available: false,
        action: "update-host",
        reason: "Take control is unavailable until a verified computer provider is configured on this computer."
    )
}

/// A selected view distinguishes controllable native app windows from booted
/// Simulator device streams, which require their own input bridge.
public enum ComputerViewTarget: Equatable, Sendable, Identifiable {
    case window(ComputerAgentWindowTarget)
    case simulator(ComputerSource)

    public var source: ComputerSource {
        switch self {
        case .window(let target): ComputerSource(id: target.sourceID, name: target.displayName, kind: "window")
        case .simulator(let source): source
        }
    }
    public var id: String { source.id ?? "" }
    public var displayName: String { source.name ?? "Simulator" }
}

public struct ComputerAvailableSources: Codable, Sendable {
    public let available: Bool
    public let sources: [ComputerSource]
    public let capture: CaptureCapability?
    /// The helper reports capture availability, independently of the host OS
    /// and whether there happen to be any open application windows.
    public var canShowComputer: Bool { capture.map { $0.available || $0.recovery != nil } ?? available }

    public var captureRecovery: String? {
        capture?.recovery
    }

    public struct CaptureCapability: Codable, Sendable {
        public let available: Bool
        public let reason: String?

        public init(available: Bool, reason: String? = nil) {
            self.available = available; self.reason = reason
        }

        fileprivate var recovery: String? {
            guard !available else { return nil }
            switch reason {
            case "screen_recording_permission_required":
                return "On your Mac, allow Cupola in System Settings → Privacy & Security → Screen & System Audio Recording."
            case "secure_desktop":
                return "Dismiss the security prompt on your computer to resume screen sharing."
            default: return nil
            }
        }
    }

    public var windowMenuItems: [ComputerWindowMenuItem] {
        guard capture?.available ?? available else { return [] }
        var seen = Set<String>()
        return sources.compactMap(ComputerWindowMenuItem.init).filter { seen.insert($0.id).inserted }.prefix(32).map { $0 }
    }

    public init(available: Bool, sources: [ComputerSource],
                capture: CaptureCapability? = nil) {
        self.available = available; self.sources = sources
        self.capture = capture
    }

}

/// A transport outage may retain a checked source briefly; a host's successful
/// no-display/permission response is authoritative immediately. Owned by one
/// computer session header; reopening its picker retains the list, while a
/// different pairing identity always resets it. Never persisted to disk.
public struct ComputerWindowMenuDiscovery: Sendable {
    private var identity: String?
    private var lastKnownCanShowComputer: Bool?
    public private(set) var captureRecovery: String?
    public private(set) var result: ComputerAvailableSources?
    public private(set) var consecutiveFailures = 0
    public static let failureLimit = 3

    public init() {}

    /// Inventory support is unknown before the first success (including older
    /// hosts that reject this call). Preserve the current computer's Whole screen
    /// action in that state, but never invent eligibility for another computer.
    public func canShowComputer(isCurrent: Bool) -> Bool {
        result?.canShowComputer ?? (isCurrent && lastKnownCanShowComputer != false)
    }

    public mutating func begin(identity: String) {
        guard self.identity != identity else { return }
        self = Self()
        self.identity = identity
    }

    public mutating func receive(_ result: ComputerAvailableSources) {
        self.result = result
        lastKnownCanShowComputer = result.canShowComputer
        captureRecovery = result.captureRecovery
        consecutiveFailures = 0
    }

    public mutating func recordFailure() {
        consecutiveFailures = min(Self.failureLimit, consecutiveFailures + 1)
        if consecutiveFailures == Self.failureLimit { result = nil }
    }
}

public struct ComputerSource: Codable, Equatable, Sendable {
    public let id: String?
    public let name: String?
    public let kind: String?
    public let width: UInt32?
    public let height: UInt32?
    public let scale: Double?
    public let crop: ComputerCrop?
    public let appId: String?
    public let appName: String?
    public let windowTitle: String?
    /// A helper-generated, bounded PNG, never a URL or a window thumbnail.
    public let iconPNG: String?
    /// A simulator's OS and version, such as "iOS 27.1"; absent from older hosts.
    public let runtime: String?

    public init(id: String? = nil, name: String? = nil, kind: String? = nil, width: UInt32? = nil,
                height: UInt32? = nil, scale: Double? = nil, crop: ComputerCrop? = nil, appId: String? = nil,
                appName: String? = nil, windowTitle: String? = nil, iconPNG: String? = nil, runtime: String? = nil) {
        self.id = id; self.name = name; self.kind = kind; self.width = width
        self.height = height; self.scale = scale; self.crop = crop
        self.appId = appId
        self.appName = appName; self.windowTitle = windowTitle; self.iconPNG = iconPNG
        self.runtime = runtime
    }

    public static let unknown = Self()
}

/// Prepared picker content. Identity is always the host's exact source ID;
/// titles and icon revisions cannot change which source is checked.
public struct ComputerWindowMenuItem: Identifiable, Sendable {
    public let target: ComputerAgentWindowTarget
    public let appName: String
    public let title: String
    public let iconPNG: String?
    public var id: String { target.id }

    public init?(_ source: ComputerSource) {
        guard let target = ComputerAgentWindowTarget(source: source) else { return nil }
        self.target = target
        let fallback = ComputerAgentWindowTarget(app: target.app)?.displayName ?? target.app
        let legacyName = Self.text(source.name)
        let legacyParts = legacyName?.components(separatedBy: " — ") ?? []
        appName = Self.text(source.appName) ?? (legacyParts.count > 1 ? Self.text(legacyParts.first) : nil) ?? fallback
        title = Self.text(source.windowTitle) ?? (legacyParts.count > 1 ? Self.text(legacyParts.dropFirst().joined(separator: " — ")) : legacyName) ?? appName
        iconPNG = source.iconPNG.flatMap { $0.utf8.count <= 3072 ? $0 : nil }
    }

    public func isSelected(sourceID: String?) -> Bool { sourceID == id }

    private static func text(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let text = raw.components(separatedBy: .controlCharacters).joined(separator: " ").trimmingCharacters(in: .whitespacesAndNewlines)
        return text.isEmpty ? nil : String(text.prefix(160))
    }
}

public struct ComputerCrop: Codable, Equatable, Sendable {
    public let x: Double
    public let y: Double
    public let width: Double
    public let height: Double

    public init(x: Double, y: Double, width: Double, height: Double) {
        self.x = x
        self.y = y
        self.width = width
        self.height = height
    }
}

public struct ComputerSession: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let clientRequestId: String
    public let ownerDeviceId: String
    public let hostInstallationId: String
    public let conversationId: String
    public let generation: UInt64
    public let state: ComputerSessionState
    public let source: ComputerSource
    public let geometryRevision: UInt64
    public let failureReason: String?
    public let createdAt: String
    public let updatedAt: String
    public let lastStateAt: String
    public let endedAt: String?
    public let capability: ComputerCapability
    public let control: ComputerControlCapability?
    public let videoQuality: ComputerVideoQuality?
    public let supportsLiveQualityChange: Bool?
    /// Absent on older computers, which require releasing and reopening a view.
    public let supportsControlSourceRebind: Bool?
    /// The frame-rate ceiling the computer is publishing at. Absent on computers
    /// that predate the field, which always publish at 15.
    public let videoMaxFramesPerSecond: Int?

    public init(id: String, clientRequestId: String, ownerDeviceId: String, hostInstallationId: String,
                conversationId: String, generation: UInt64, state: ComputerSessionState,
                source: ComputerSource, geometryRevision: UInt64, failureReason: String?,
                createdAt: String, updatedAt: String, lastStateAt: String, endedAt: String?,
                capability: ComputerCapability, control: ComputerControlCapability? = .unavailable,
                videoQuality: ComputerVideoQuality? = nil,
                supportsLiveQualityChange: Bool? = nil, supportsControlSourceRebind: Bool? = nil,
                videoMaxFramesPerSecond: Int? = nil) {
        self.id = id; self.clientRequestId = clientRequestId; self.ownerDeviceId = ownerDeviceId
        self.hostInstallationId = hostInstallationId; self.conversationId = conversationId
        self.generation = generation; self.state = state; self.source = source
        self.geometryRevision = geometryRevision; self.failureReason = failureReason
        self.createdAt = createdAt; self.updatedAt = updatedAt; self.lastStateAt = lastStateAt
        self.endedAt = endedAt; self.capability = capability; self.control = control
        self.videoQuality = videoQuality
        self.supportsLiveQualityChange = supportsLiveQualityChange
        self.supportsControlSourceRebind = supportsControlSourceRebind
        self.videoMaxFramesPerSecond = videoMaxFramesPerSecond
    }
}

/// The frame rates a viewer may ask a computer to publish at.
public enum ComputerVideoFrameRate {
    public static let supported = 15...120

    /// Nil stays nil, so an unset ceiling is not sent and the computer uses 15.
    static func clamped(_ framesPerSecond: Int?) -> Int? {
        framesPerSecond.map { min(max($0, supported.lowerBound), supported.upperBound) }
    }
}

public enum ComputerVideoQuality: String, Codable, CaseIterable, Identifiable, Sendable {
    case auto
    case standard
    case medium
    case high

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .auto: "Auto"
        case .standard: "720p"
        case .medium: "1080p"
        case .high: "1440p"
        }
    }
}

/// The exact view a Take over ends. The host refuses it if that view changed.
public struct ComputerViewerReplacement: Codable, Equatable, Sendable {
    public let sessionId: String
    public let generation: UInt64

    public init(sessionId: String, generation: UInt64) {
        self.sessionId = sessionId; self.generation = generation
    }
}

/// The host's structured refusal of an open: another device is viewing this
/// computer (offer Take over), or the view a Take over named has changed.
public struct ComputerViewerConflict: LocalizedError, Decodable, Equatable, Sendable {
    public enum Code: String, Decodable, Sendable {
        case viewerBusy
        case viewChanged
    }
    public let code: Code
    public let deviceName: String?
    public let sessionId: String
    public let generation: UInt64

    public init(code: Code, deviceName: String?, sessionId: String, generation: UInt64) {
        self.code = code; self.deviceName = deviceName; self.sessionId = sessionId; self.generation = generation
    }

    /// The viewing device's name, or a generic one when the host has none.
    public var viewingDevice: String {
        let name = deviceName?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return name.isEmpty ? "another device" : name
    }

    public var replacement: ComputerViewerReplacement {
        ComputerViewerReplacement(sessionId: sessionId, generation: generation)
    }

    public var errorDescription: String? {
        switch code {
        case .viewerBusy: "This computer is being viewed on \(viewingDevice)."
        case .viewChanged: "The computer view changed on another device. Try again."
        }
    }
}

public struct StartComputerSessionRequest: Encodable, Sendable {
    public let clientRequestId: String
    public let conversationId: String
    public let hostInstallationId: String
    public let generation: UInt64
    public let source: ComputerSource?
    public let videoQuality: ComputerVideoQuality
    public let videoMaxHeight: Int?
    /// The highest frame rate this device will take, 15...120. Omitted, the
    /// computer publishes at 15, as it did before the field existed.
    public let videoMaxFramesPerSecond: Int?
    /// The view another device holds, as this device was told about it.
    public let replaces: ComputerViewerReplacement?
    /// Ends that other device's view and its control; sent only with `replaces`.
    public let takeOver: Bool?

    public init(clientRequestId: String, conversationId: String, hostInstallationId: String,
                generation: UInt64 = 1, source: ComputerSource? = nil,
                videoQuality: ComputerVideoQuality = .auto, videoMaxFramesPerSecond: Int? = nil,
                takingOver: ComputerViewerReplacement? = nil) {
        self.clientRequestId = clientRequestId; self.conversationId = conversationId
        self.hostInstallationId = hostInstallationId; self.generation = generation; self.source = source
        self.replaces = takingOver
        self.takeOver = takingOver == nil ? nil : true
        // Older Macs already capture Auto at 1080p and ignore unknown request fields.
        // Updated Macs use the height hint to keep 1080p fixed.
        self.videoQuality = videoQuality == .medium ? .auto : videoQuality
        self.videoMaxHeight = videoQuality == .medium ? 1_080 : nil
        self.videoMaxFramesPerSecond = ComputerVideoFrameRate.clamped(videoMaxFramesPerSecond)
    }
}

public struct ComputerSessionQualityRequest: Encodable, Sendable {
    public let generation: UInt64
    public let videoQuality: ComputerVideoQuality
    public let videoMaxHeight: Int?
    /// Omitted, the computer keeps the ceiling the view already has.
    public let videoMaxFramesPerSecond: Int?

    public init(generation: UInt64, videoQuality: ComputerVideoQuality, videoMaxFramesPerSecond: Int? = nil) {
        self.generation = generation
        self.videoQuality = videoQuality == .medium ? .auto : videoQuality
        self.videoMaxHeight = videoQuality == .medium ? 1_080 : nil
        self.videoMaxFramesPerSecond = ComputerVideoFrameRate.clamped(videoMaxFramesPerSecond)
    }
}

public struct ComputerSignalingBinding: Encodable, Sendable {
    public let generation: UInt64
    public let conversationId: String
    public let hostInstallationId: String
    public let peerRevision: String?

    public init(generation: UInt64, conversationId: String, hostInstallationId: String,
                peerRevision: String? = nil) {
        self.generation = generation
        self.conversationId = conversationId
        self.hostInstallationId = hostInstallationId
        self.peerRevision = peerRevision
    }
}

public struct ComputerSignalingPollRequest: Encodable, Sendable {
    public let generation: UInt64
    public let conversationId: String
    public let hostInstallationId: String
    public let peerRevision: String?
    public let cursor: UInt64

    public init(binding: ComputerSignalingBinding, cursor: UInt64 = 0) {
        generation = binding.generation
        conversationId = binding.conversationId
        hostInstallationId = binding.hostInstallationId
        peerRevision = binding.peerRevision
        self.cursor = cursor
    }
}

public struct ComputerSignalingAnswerRequest: Encodable, Sendable {
    public let generation: UInt64
    public let conversationId: String
    public let hostInstallationId: String
    public let peerRevision: String
    public let type: String
    public let sdp: String

    public init?(binding: ComputerSignalingBinding, sdp: String) {
        guard let peerRevision = binding.peerRevision else { return nil }
        generation = binding.generation
        conversationId = binding.conversationId
        hostInstallationId = binding.hostInstallationId
        self.peerRevision = peerRevision
        type = "answer"
        self.sdp = sdp
    }
}

public struct ComputerSignalingCandidateRequest: Encodable, Sendable {
    public let generation: UInt64
    public let conversationId: String
    public let hostInstallationId: String
    public let peerRevision: String
    public let sequence: UInt64
    public let candidate: String
    public let sdpMid: String?
    public let sdpMLineIndex: UInt32
    public let usernameFragment: String?

    public init?(binding: ComputerSignalingBinding, sequence: UInt64, candidate: String,
                 sdpMid: String?, sdpMLineIndex: UInt32, usernameFragment: String? = nil) {
        guard let peerRevision = binding.peerRevision else { return nil }
        generation = binding.generation
        conversationId = binding.conversationId
        hostInstallationId = binding.hostInstallationId
        self.peerRevision = peerRevision
        self.sequence = sequence
        self.candidate = candidate
        self.sdpMid = sdpMid
        self.sdpMLineIndex = sdpMLineIndex
        self.usernameFragment = usernameFragment
    }
}

public struct ComputerSignalingCloseRequest: Encodable, Sendable {
    public let generation: UInt64
    public let conversationId: String
    public let hostInstallationId: String
    public let peerRevision: String
    public let reason: String?

    public init?(binding: ComputerSignalingBinding, reason: String? = nil) {
        guard let peerRevision = binding.peerRevision else { return nil }
        generation = binding.generation
        conversationId = binding.conversationId
        hostInstallationId = binding.hostInstallationId
        self.peerRevision = peerRevision
        self.reason = reason
    }
}

public struct ComputerIceServer: Decodable, Equatable, Sendable {
    public let urls: [String]
    public let username: String?
    public let credential: String?
}

public struct ComputerSignalingDescription: Decodable, Equatable, Sendable {
    public let type: String
    public let sdp: String
}

public struct ComputerSignalingCandidate: Decodable, Equatable, Sendable {
    public let sequence: UInt64
    public let candidate: String
    public let sdpMid: String?
    public let sdpMLineIndex: UInt32
    public let usernameFragment: String?
}

public struct ComputerSignalingResponse: Decodable, Equatable, Sendable {
    public let sessionId: String
    public let generation: UInt64
    public let peerRevision: String?
    public let state: String
    public let captureState: String
    public let peerState: String
    public let offer: ComputerSignalingDescription?
    public let candidates: [ComputerSignalingCandidate]
    public let cursor: UInt64
    public let nextCursor: UInt64
    public let failureReason: String?
    public let iceServers: [ComputerIceServer]
    /// While this device controls the computer: typing focus is in a text
    /// field there. Absent when the computer doesn't report focus.
    public let textInputFocused: Bool?
}

public struct ComputerSignalingMutationResponse: Decodable, Equatable, Sendable {
    public let accepted: Bool
    public let duplicate: Bool?
    public let closed: Bool?
    public let sessionId: String?
    public let generation: UInt64?
}

public struct AcquireComputerControlRequest: Encodable, Sendable {
    public let clientRequestId: String
    public let generation: UInt64
    public let geometryRevision: UInt64
    public let sourceId: String?

    public init(clientRequestId: String, generation: UInt64, geometryRevision: UInt64, sourceId: String?) {
        self.clientRequestId = clientRequestId
        self.generation = generation
        self.geometryRevision = geometryRevision
        self.sourceId = sourceId
    }
}

public struct ComputerControlBindingRequest: Codable, Sendable {
    public let leaseId: String
    public let generation: UInt64
    public let geometryRevision: UInt64
    public let sourceId: String?

    public init(leaseId: String, generation: UInt64, geometryRevision: UInt64, sourceId: String?) {
        self.leaseId = leaseId
        self.generation = generation
        self.geometryRevision = geometryRevision
        self.sourceId = sourceId
    }
}

public struct ComputerControlLease: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let sessionId: String
    public let ownerDeviceId: String
    public let hostInstallationId: String
    public let conversationId: String
    public let generation: UInt64
    public let sourceId: String?
    public let geometryRevision: UInt64
    public let status: String
    public let lastSequence: UInt64
    public let acquiredAt: String
    public let updatedAt: String
    public let expiresAt: String
    public let releasedAt: String?

    public init(id: String, sessionId: String, ownerDeviceId: String, hostInstallationId: String,
                conversationId: String, generation: UInt64, sourceId: String?, geometryRevision: UInt64,
                status: String, lastSequence: UInt64, acquiredAt: String, updatedAt: String,
                expiresAt: String, releasedAt: String?) {
        self.id = id
        self.sessionId = sessionId
        self.ownerDeviceId = ownerDeviceId
        self.hostInstallationId = hostInstallationId
        self.conversationId = conversationId
        self.generation = generation
        self.sourceId = sourceId
        self.geometryRevision = geometryRevision
        self.status = status
        self.lastSequence = lastSequence
        self.acquiredAt = acquiredAt
        self.updatedAt = updatedAt
        self.expiresAt = expiresAt
        self.releasedAt = releasedAt
    }
}

public enum ComputerInputAction: Codable, Equatable, Sendable {
    case pointer(x: Double, y: Double, phase: String, button: String?)
    case scroll(deltaX: Double, deltaY: Double)
    case key(key: String, phase: String, modifiers: UInt32)
    case text(String)
    case clipboard(operation: String, text: String?)
    /// Simulator views only: one finger at a normalized point of the
    /// simulator's native display; phase down, move or up, optionally after a
    /// pause of at most 1 s.
    case touch(x: Double, y: Double, phase: String, delayMs: UInt32?)
    /// Simulator views only: home, lock, volumeUp or volumeDown; phase press,
    /// down or up.
    case button(name: String, phase: String)
    case releaseAll

    private enum CodingKeys: String, CodingKey {
        case type, x, y, phase, button, deltaX, deltaY, key, modifiers, text, operation, delayMs
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .type) {
        case "pointer":
            self = .pointer(
                x: try container.decode(Double.self, forKey: .x),
                y: try container.decode(Double.self, forKey: .y),
                phase: try container.decode(String.self, forKey: .phase),
                button: try container.decodeIfPresent(String.self, forKey: .button)
            )
        case "scroll":
            self = .scroll(
                deltaX: try container.decode(Double.self, forKey: .deltaX),
                deltaY: try container.decode(Double.self, forKey: .deltaY)
            )
        case "key":
            self = .key(
                key: try container.decode(String.self, forKey: .key),
                phase: try container.decode(String.self, forKey: .phase),
                modifiers: try container.decode(UInt32.self, forKey: .modifiers)
            )
        case "text":
            self = .text(try container.decode(String.self, forKey: .text))
        case "clipboard":
            self = .clipboard(
                operation: try container.decode(String.self, forKey: .operation),
                text: try container.decodeIfPresent(String.self, forKey: .text)
            )
        case "touch":
            self = .touch(
                x: try container.decode(Double.self, forKey: .x),
                y: try container.decode(Double.self, forKey: .y),
                phase: try container.decode(String.self, forKey: .phase),
                delayMs: try container.decodeIfPresent(UInt32.self, forKey: .delayMs)
            )
        case "button":
            self = .button(name: try container.decode(String.self, forKey: .button),
                           phase: try container.decode(String.self, forKey: .phase))
        case "releaseAll":
            self = .releaseAll
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "unsupported computer input action"
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .pointer(x, y, phase, button):
            try container.encode("pointer", forKey: .type)
            try container.encode(x, forKey: .x)
            try container.encode(y, forKey: .y)
            try container.encode(phase, forKey: .phase)
            try container.encodeIfPresent(button, forKey: .button)
        case let .scroll(deltaX, deltaY):
            try container.encode("scroll", forKey: .type)
            try container.encode(deltaX, forKey: .deltaX)
            try container.encode(deltaY, forKey: .deltaY)
        case let .key(key, phase, modifiers):
            try container.encode("key", forKey: .type)
            try container.encode(key, forKey: .key)
            try container.encode(phase, forKey: .phase)
            try container.encode(modifiers, forKey: .modifiers)
        case let .text(text):
            try container.encode("text", forKey: .type)
            try container.encode(text, forKey: .text)
        case let .clipboard(operation, text):
            try container.encode("clipboard", forKey: .type)
            try container.encode(operation, forKey: .operation)
            try container.encodeIfPresent(text, forKey: .text)
        case let .touch(x, y, phase, delayMs):
            try container.encode("touch", forKey: .type)
            try container.encode(x, forKey: .x)
            try container.encode(y, forKey: .y)
            try container.encode(phase, forKey: .phase)
            try container.encodeIfPresent(delayMs, forKey: .delayMs)
        case let .button(name, phase):
            try container.encode("button", forKey: .type)
            try container.encode(name, forKey: .button)
            try container.encode(phase, forKey: .phase)
        case .releaseAll:
            try container.encode("releaseAll", forKey: .type)
        }
    }
}

public struct ComputerInputBatchRequest: Encodable, Sendable {
    public let leaseId: String
    public let generation: UInt64
    public let geometryRevision: UInt64
    public let sourceId: String?
    public let sequence: UInt64
    public let actions: [ComputerInputAction]

    public init(leaseId: String, generation: UInt64, geometryRevision: UInt64, sourceId: String?,
                sequence: UInt64, actions: [ComputerInputAction]) {
        self.leaseId = leaseId
        self.generation = generation
        self.geometryRevision = geometryRevision
        self.sourceId = sourceId
        self.sequence = sequence
        self.actions = actions
    }
}

public struct ComputerControlActionResponse: Decodable, Sendable {
    public let granted: Bool
    public let acknowledged: Bool
    public let status: String
    public let reason: String
    public let lease: ComputerControlLease?
    public let control: ComputerControlCapability
    public let clipboardText: String?
}


/// The single platform-to-controls table. Nil is the legacy Mac host contract;
/// an unrecognized platform gets ordinary keyboard controls, never Mac shortcuts.
public struct ComputerPlatformControls: Equatable, Sendable {
    public struct Button: Identifiable, Equatable, Sendable {
        public let id: String
        public let title: String
        public let symbol: String
        public let label: String
        public let action: Action
    }
    public enum Action: Equatable, Sendable {
        case keys([ComputerInputAction])
        case modifier(String)
        case appSwitcher
        case applications(open: ComputerInputAction, browse: ComputerInputAction)
        case rightClick
    }
    public let rows: [[Button]]
    public let switcherKey: String
    public let switcherModifiers: UInt32
    public let switcherName: String
    public let switcherTitle: String
    public let switcherSymbol: String
    public let limitation: String?

    public static func forHost(_ platform: String?) -> Self {
        table[(platform ?? "macos").lowercased()] ?? unknown
    }

    /// Latching modifiers for the trackpad's key row (iPhone Duo laptop pose):
    /// each stays down until the next key or click, then releases. Windows and
    /// Linux rows already carry their Ctrl/Alt/Super latches.
    public static func stickyModifiers(forHost platform: String?) -> [Button] {
        switch (platform ?? "macos").lowercased() {
        case "macos":
            return [
                Button(id: "control", title: "⌃", symbol: "control", label: "Control", action: .modifier("control")),
                Button(id: "option", title: "⌥", symbol: "option", label: "Option", action: .modifier("option")),
                Button(id: "command", title: "⌘", symbol: "command", label: "Command", action: .modifier("command")),
                Button(id: "shift", title: "⇧", symbol: "shift", label: "Shift", action: .modifier("shift")),
            ]
        case "windows", "linux":
            return [Button(id: "shift", title: "Shift", symbol: "shift", label: "Shift", action: .modifier("shift"))]
        default:
            return []
        }
    }

    private static func key(_ id: String, _ title: String, _ symbol: String, _ label: String,
                            _ key: String, _ modifiers: UInt32 = 0) -> Button {
        Button(id: id, title: title, symbol: symbol, label: label,
               action: .keys([.key(key: key, phase: "press", modifiers: modifiers)]))
    }
    private static func modifier(_ key: String, _ title: String) -> Button {
        Button(id: key, title: title, symbol: "keyboard", label: title,
               action: .modifier(key))
    }
    private static let switcher = Button(id: "app-switcher", title: "", symbol: "arrow.left.arrow.right",
                                         label: "", action: .appSwitcher)
    private static let rightClick = Button(id: "right-click", title: "Right click", symbol: "computermouse",
                                           label: "Right click at pointer", action: .rightClick)
    private static func chord(_ id: String, _ title: String, _ symbol: String, _ label: String,
                              _ modifier: String, _ key: String) -> Button {
        Button(id: id, title: title, symbol: symbol, label: label, action: .keys([
            .key(key: modifier, phase: "down", modifiers: 0),
            .key(key: key, phase: "press", modifiers: 0),
            .key(key: modifier, phase: "up", modifiers: 0)
        ]))
    }
    private static let table: [String: Self] = [
        "macos": Self(rows: [[
            key("all-windows", "Windows", "rectangle.3.group", "All windows, Control Up Arrow", "up", 2),
            key("app-windows", "This app", "macwindow", "App windows, Control Down Arrow", "down", 2),
            key("next-window", "Next", "rectangle.on.rectangle", "Next window, Command Grave", "`", 8),
            Button(id: "applications", title: "Apps", symbol: "square.grid.3x3",
                   label: "Show Applications, Command Space then Command 1", action: .applications(
                    open: .key(key: "space", phase: "press", modifiers: 8),
                    browse: .key(key: "1", phase: "press", modifiers: 8))),
            switcher
        ]], switcherKey: "command", switcherModifiers: 8, switcherName: "Command", switcherTitle: "⌘ Tab", switcherSymbol: "command", limitation: nil),
        "windows": Self(rows: [
            [key("win", "Win (Start)", "keyboard", "Win, Start menu", "win"), modifier("ctrl", "Ctrl"), modifier("alt", "Alt")],
            [switcher,
             chord("task-view", "Task View", "rectangle.3.group", "Task View, Win Tab", "win", "tab"),
             chord("show-desktop", "Desktop", "desktopcomputer", "Show desktop, Win D", "win", "d"),
             rightClick]
        ], switcherKey: "alt", switcherModifiers: 4, switcherName: "Alt", switcherTitle: "Alt Tab", switcherSymbol: "arrow.left.arrow.right",
           limitation: "Ctrl-Alt-Del cannot be sent. UAC prompts on the secure desktop cannot be controlled from Cupola."),
        "linux": Self(rows: [
            [modifier("super", "Super"), modifier("ctrl", "Ctrl"), modifier("alt", "Alt")],
            [switcher, key("activities", "Activities", "square.grid.3x3", "Activities overview, Super", "super"), rightClick]
        ], switcherKey: "alt", switcherModifiers: 4, switcherName: "Alt", switcherTitle: "Alt Tab", switcherSymbol: "arrow.left.arrow.right", limitation: nil)
    ]
    private static let unknown = Self(rows: [], switcherKey: "", switcherModifiers: 0,
                                      switcherName: "", switcherTitle: "", switcherSymbol: "keyboard", limitation: nil)
}

/// The selected platform's modifier stays down until selection or cancellation.
/// The session's release-all path owns cleanup after disconnect or lease loss.
public struct ComputerAppSwitcher: Sendable {
    public private(set) var isPresented = false
    private var heldKey = ""
    private var modifiers: UInt32 = 0
    public init() {}

    public mutating func toggle(controls: ComputerPlatformControls = .forHost(nil)) -> [ComputerInputAction] {
        if isPresented { return finish() }
        guard !controls.switcherKey.isEmpty else { return [] }
        heldKey = controls.switcherKey
        modifiers = controls.switcherModifiers
        isPresented = true
        return [.key(key: heldKey, phase: "down", modifiers: modifiers),
                .key(key: "tab", phase: "press", modifiers: modifiers)]
    }

    public mutating func finish(cancel: Bool = false) -> [ComputerInputAction] {
        guard isPresented else { return [] }
        isPresented = false
        let release = ComputerInputAction.key(key: heldKey, phase: "up", modifiers: 0)
        return cancel ? [.key(key: "escape", phase: "press", modifiers: modifiers), release] : [release]
    }

    public mutating func key(_ key: String) -> [ComputerInputAction] {
        if isPresented {
            if key == "escape" { return finish(cancel: true) }
            if key == "return" { return finish() }
            if ["tab", "left", "right", "up", "down"].contains(key) {
                return [.key(key: key, phase: "press", modifiers: modifiers)]
            }
        }
        return finish(cancel: true) + [.key(key: key, phase: "press", modifiers: 0)]
    }
}

/// On-screen modifiers apply to the next key. Native text composition stays
/// unchanged unless a modifier is selected; modified non-ASCII text is refused.
public struct ComputerModifierLatch: Sendable {
    public private(set) var keys: [String] = []
    public init() {}
    public mutating func toggle(_ key: String) -> [ComputerInputAction] {
        if keys.contains(key) {
            keys.removeAll { $0 == key }
            return [.key(key: key, phase: "up", modifiers: 0)]
        }
        keys.append(key)
        return [.key(key: key, phase: "down", modifiers: 0)]
    }
    public mutating func release() -> [ComputerInputAction] {
        let actions = keys.reversed().map { ComputerInputAction.key(key: $0, phase: "up", modifiers: 0) }
        keys.removeAll()
        return actions
    }
    public mutating func applying(to actions: [ComputerInputAction]) -> [ComputerInputAction]? {
        guard !keys.isEmpty else { return actions }
        var result: [ComputerInputAction] = []
        for action in actions {
            if case .text(let text) = action {
                guard text.count == 1 else { return nil }
                for character in text {
                    let shifted = Array("~!@#$%^&*()_+{}|:\"<>?")
                    let base = Array("`1234567890-=[]\\;',./")
                    let name: String
                    let modifiers: UInt32
                    if let index = shifted.firstIndex(of: character) {
                        name = String(base[index]); modifiers = 1
                    } else if character == " " { name = "space"; modifiers = 0 }
                    else if character == "\n" { name = "return"; modifiers = 0 }
                    else if character == "\t" { name = "tab"; modifiers = 0 }
                    else {
                        guard character.isASCII, let scalar = character.unicodeScalars.first,
                              scalar.value >= 33, scalar.value <= 126 else { return nil }
                        name = String(character).lowercased()
                        modifiers = character.isUppercase ? 1 : 0
                    }
                    result.append(.key(key: name, phase: "press", modifiers: modifiers))
                }
            } else { result.append(action) }
        }
        return result + release()
    }
}
