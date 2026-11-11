import CoreMedia
import CoreVideo
import Foundation

public struct CaptureRect: Codable, Equatable, Sendable {
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

    public static let zero = CaptureRect(x: 0, y: 0, width: 0, height: 0)
}

public enum CaptureLifecycleState: String, Codable, Equatable, Sendable {
    case idle
    case awaitingSource
    case ready
    case starting
    case capturing
    case paused
    case permissionDenied
    case sourceRemoved
    case suspended
    case failed
    case stopped
    case helperRestarted

    /// A suspended window (closed, minimized or hidden) still has its stream,
    /// so the person can switch to another window instead of reopening the view.
    public var acceptsSourceRebind: Bool {
        self == .capturing || self == .suspended
    }
}

public enum CaptureSourceKind: String, Codable, Equatable, Sendable {
    case display
    case simulator
    case window
    case picker
}

public struct CaptureSourceDescriptor: Codable, Equatable, Sendable {
    public let id: String
    public let kind: CaptureSourceKind
    public let title: String
    public let application: String?
    public let width: Int
    public let height: Int
    public let scale: Double
    public let contentRect: CaptureRect

    public init(
        id: String,
        kind: CaptureSourceKind,
        title: String,
        application: String? = nil,
        width: Int,
        height: Int,
        scale: Double,
        contentRect: CaptureRect
    ) {
        self.id = id
        self.kind = kind
        self.title = title
        self.application = application
        self.width = width
        self.height = height
        self.scale = scale
        self.contentRect = contentRect
    }
}

public struct CaptureConfiguration: Codable, Equatable, Sendable {
    public static let maximumWidth = 2_560
    public static let maximumHeight = 1_440
    public let width: Int
    public let height: Int
    public let framesPerSecond: Int
    public let audioEnabled: Bool
    public let queueDepth: Int

    public init(
        width: Int = 1_280,
        height: Int = 720,
        framesPerSecond: Int = VideoEncodingPolicy.defaultFramesPerSecond,
        audioEnabled: Bool = false,
        queueDepth: Int = 2
    ) {
        self.width = max(1, min(width, Self.maximumWidth))
        self.height = max(1, min(height, Self.maximumHeight))
        self.framesPerSecond = max(1, min(framesPerSecond, VideoEncodingPolicy.maximumFramesPerSecond))
        // Audio capture is intentionally outside the first sharing contract.
        self.audioEnabled = false
        // The macOS 27 SDK header documents only a default and maximum of 8
        // for SCStreamConfiguration.queueDepth, no minimum. A shallow queue
        // keeps stale frames from building up ahead of the encoder.
        self.queueDepth = max(1, min(queueDepth, 2))
    }

    /// Keep the encoded canvas fixed while placing the selected source at a
    /// known position. The viewer removes this centered padding before mapping
    /// touches into the source's logical bounds.
    func centeredDestinationRect(for source: CaptureSourceDescriptor) -> CaptureRect {
        let outputWidth = Double(width)
        let outputHeight = Double(height)
        let fullOutput = CaptureRect(x: 0, y: 0, width: outputWidth, height: outputHeight)
        let logicalWidth = source.contentRect.width
        let logicalHeight = source.contentRect.height
        let hasLogicalBounds = logicalWidth.isFinite && logicalHeight.isFinite
            && logicalWidth > 0 && logicalHeight > 0
        let sourceWidth = hasLogicalBounds ? logicalWidth : Double(source.width)
        let sourceHeight = hasLogicalBounds ? logicalHeight : Double(source.height)
        guard sourceWidth > 0, sourceHeight > 0 else { return fullOutput }
        let aspect = sourceWidth / sourceHeight
        guard aspect.isFinite, aspect > 0 else { return fullOutput }
        let fittedWidth = min(outputWidth, outputHeight * aspect)
        let fittedHeight = fittedWidth / aspect
        guard fittedWidth.isFinite, fittedHeight.isFinite,
              fittedWidth > 0, fittedHeight > 0 else { return fullOutput }
        return CaptureRect(
            x: (outputWidth - fittedWidth) / 2,
            y: (outputHeight - fittedHeight) / 2,
            width: fittedWidth,
            height: fittedHeight
        )
    }
}

public enum CaptureVideoQuality: String, Codable, CaseIterable, Sendable {
    case standard
    case medium
    case auto
    case high

    public var configuration: CaptureConfiguration {
        configuration(framesPerSecond: VideoEncodingPolicy.defaultFramesPerSecond)
    }

    public func configuration(framesPerSecond: Int) -> CaptureConfiguration {
        switch self {
        case .standard: CaptureConfiguration(width: 1_280, height: 720, framesPerSecond: framesPerSecond)
        case .medium: CaptureConfiguration(width: 1_920, height: 1_080, framesPerSecond: framesPerSecond)
        case .auto: CaptureConfiguration(width: 2_560, height: 1_440, framesPerSecond: framesPerSecond)
        case .high: CaptureConfiguration(width: 2_560, height: 1_440, framesPerSecond: framesPerSecond)
        }
    }
}

/// A frame is deliberately an in-memory, non-codable value. The capture
/// boundary hands it to the media publisher only; it must never be persisted,
/// logged, or placed in a daemon/chat event.
public struct CapturedVideoFrame: @unchecked Sendable {
    public let pixelBuffer: CVPixelBuffer
    public let timestamp: CMTime
    public let metadata: CaptureFrameMetadata

    public init(pixelBuffer: CVPixelBuffer, timestamp: CMTime, metadata: CaptureFrameMetadata) {
        self.pixelBuffer = pixelBuffer
        self.timestamp = timestamp
        self.metadata = metadata
    }
}

/// Delivers only the newest pending frame on a worker queue. The capture
/// callback never waits for WebRTC encoding, and a slow encoder cannot create
/// an unbounded latency backlog.
public final class LatestVideoFrameSink: @unchecked Sendable {
    public struct Statistics: Equatable, Sendable {
        public let count: Int
        public let droppedCount: UInt64
    }

    private let queue: LatestFrameQueue<CapturedVideoFrame>
    private let deliveryQueue: DispatchQueue
    private let consumer: @Sendable (CapturedVideoFrame) -> Void
    private let lock = NSLock()
    private var draining = false

    public init(
        capacity: Int = 2,
        label: String = "com.wonder.computer-use.video",
        consumer: @escaping @Sendable (CapturedVideoFrame) -> Void
    ) {
        self.queue = LatestFrameQueue(capacity: min(capacity, 2))
        self.deliveryQueue = DispatchQueue(label: label, qos: .userInitiated)
        self.consumer = consumer
    }

    @discardableResult
    public func offer(_ frame: CapturedVideoFrame) -> Bool {
        let dropped = queue.offer(frame)
        lock.lock()
        let shouldDrain = !draining
        if shouldDrain { draining = true }
        lock.unlock()
        if shouldDrain {
            deliveryQueue.async { [weak self] in self?.drain() }
        }
        return dropped
    }

    public func statistics() -> Statistics {
        let statistics = queue.statistics()
        return Statistics(count: statistics.count, droppedCount: statistics.droppedCount)
    }

    public func clear() {
        queue.clear()
    }

    public func reset() {
        queue.reset()
    }

    private func drain() {
        while let frame = queue.takeLatest() {
            consumer(frame)
        }
        lock.lock()
        draining = false
        let shouldRestart = queue.statistics().count > 0
        if shouldRestart { draining = true }
        lock.unlock()
        if shouldRestart { deliveryQueue.async { [weak self] in self?.drain() } }
    }
}

public struct CaptureFrameMetadata: Codable, Equatable, Sendable {
    public let captureTimestamp: Double
    public let geometryRevision: UInt64
    public let sourceWidth: Int
    public let sourceHeight: Int
    public let scale: Double
    public let contentRect: CaptureRect
    public let cropRect: CaptureRect
    public let frameSequence: UInt64
    public let sessionID: String
    public let generation: UInt64

    public init(
        captureTimestamp: Double,
        geometryRevision: UInt64,
        sourceWidth: Int,
        sourceHeight: Int,
        scale: Double,
        contentRect: CaptureRect,
        cropRect: CaptureRect,
        frameSequence: UInt64,
        sessionID: String,
        generation: UInt64
    ) {
        self.captureTimestamp = captureTimestamp
        self.geometryRevision = geometryRevision
        self.sourceWidth = sourceWidth
        self.sourceHeight = sourceHeight
        self.scale = scale
        self.contentRect = contentRect
        self.cropRect = cropRect
        self.frameSequence = frameSequence
        self.sessionID = sessionID
        self.generation = generation
    }

    public static func make(
        timestamp: Double,
        sourceWidth: Int,
        sourceHeight: Int,
        source: CaptureSourceDescriptor,
        geometryRevision: UInt64,
        frameSequence: UInt64,
        sessionID: String,
        generation: UInt64
    ) -> CaptureFrameMetadata {
        CaptureFrameMetadata(
            captureTimestamp: timestamp.isFinite ? timestamp : Date().timeIntervalSince1970,
            geometryRevision: geometryRevision,
            sourceWidth: max(0, sourceWidth),
            sourceHeight: max(0, sourceHeight),
            scale: max(0.01, source.scale),
            contentRect: source.contentRect,
            cropRect: CaptureRect(x: 0, y: 0, width: Double(max(0, sourceWidth)), height: Double(max(0, sourceHeight))),
            frameSequence: frameSequence,
            sessionID: sessionID,
            generation: generation
        )
    }
}

public struct CaptureStatusSnapshot: Codable, Equatable, Sendable {
    public let state: CaptureLifecycleState
    public let sessionID: String?
    public let generation: UInt64
    public let helperInstanceID: String
    public let sourceID: String?
    public let viewerCount: Int
    public let queuedFrameCount: Int
    public let droppedFrameCount: UInt64
    public let frameSequence: UInt64
    public let geometryRevision: UInt64
    public let pickerAvailable: Bool
    public let screenRecordingAuthorized: Bool
    public let audioEnabled: Bool
    public let configuration: CaptureConfiguration
    public let lastFrame: CaptureFrameMetadata?
    public let message: String?
    public let reason: String?

    public init(
        state: CaptureLifecycleState,
        sessionID: String?,
        generation: UInt64,
        helperInstanceID: String,
        sourceID: String?,
        viewerCount: Int,
        queuedFrameCount: Int,
        droppedFrameCount: UInt64,
        frameSequence: UInt64,
        geometryRevision: UInt64,
        pickerAvailable: Bool,
        screenRecordingAuthorized: Bool,
        audioEnabled: Bool,
        configuration: CaptureConfiguration,
        lastFrame: CaptureFrameMetadata?,
        message: String? = nil,
        reason: String? = nil
    ) {
        self.state = state
        self.sessionID = sessionID
        self.generation = generation
        self.helperInstanceID = helperInstanceID
        self.sourceID = sourceID
        self.viewerCount = max(0, viewerCount)
        self.queuedFrameCount = max(0, queuedFrameCount)
        self.droppedFrameCount = droppedFrameCount
        self.frameSequence = frameSequence
        self.geometryRevision = geometryRevision
        self.pickerAvailable = pickerAvailable
        self.screenRecordingAuthorized = screenRecordingAuthorized
        self.audioEnabled = audioEnabled
        self.configuration = configuration
        self.lastFrame = lastFrame
        self.message = message
        self.reason = reason
    }
}

public struct CaptureOperationResult: Codable, Equatable, Sendable {
    public let accepted: Bool
    public let action: String
    public let status: CaptureStatusSnapshot
    public let message: String?
    public let errorCode: String?
    public let sources: [CaptureSourceDescriptor]?

    public init(
        accepted: Bool,
        action: String,
        status: CaptureStatusSnapshot,
        message: String? = nil,
        errorCode: String? = nil,
        sources: [CaptureSourceDescriptor]? = nil
    ) {
        self.accepted = accepted
        self.action = action
        self.status = status
        self.message = message
        self.errorCode = errorCode
        self.sources = sources
    }
}

public struct CaptureEvent: Codable, Equatable, Sendable {
    public let event: String
    public let status: CaptureStatusSnapshot
    public let metadata: CaptureFrameMetadata?
    public let sources: [CaptureSourceDescriptor]?
    public let source: CaptureSourceDescriptor?
    public let message: String?

    public init(
        event: String,
        status: CaptureStatusSnapshot,
        metadata: CaptureFrameMetadata? = nil,
        sources: [CaptureSourceDescriptor]? = nil,
        source: CaptureSourceDescriptor? = nil,
        message: String? = nil
    ) {
        self.event = event
        self.status = status
        self.metadata = metadata
        self.sources = sources
        self.source = source
        self.message = message
    }
}
