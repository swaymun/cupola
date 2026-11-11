import Foundation
import ScreenCaptureKit

public struct CaptureStateMachine: Sendable {
    public private(set) var status: CaptureStatusSnapshot

    private var requiresScreenRecording = true
    private var configuration: CaptureConfiguration
    private let pickerAvailable: Bool
    private let helperInstanceID: String

    public init(
        configuration: CaptureConfiguration = CaptureConfiguration(),
        helperInstanceID: String = UUID().uuidString,
        pickerAvailable: Bool = false,
        screenRecordingAuthorized: Bool = false
    ) {
        self.configuration = configuration
        self.pickerAvailable = pickerAvailable
        self.helperInstanceID = helperInstanceID
        self.status = CaptureStatusSnapshot(
            state: .idle,
            sessionID: nil,
            generation: 0,
            helperInstanceID: helperInstanceID,
            sourceID: nil,
            viewerCount: 0,
            queuedFrameCount: 0,
            droppedFrameCount: 0,
            frameSequence: 0,
            geometryRevision: 0,
            pickerAvailable: pickerAvailable,
            screenRecordingAuthorized: screenRecordingAuthorized,
            audioEnabled: self.configuration.audioEnabled,
            configuration: self.configuration,
            lastFrame: nil
        )
    }

    @discardableResult
    public mutating func prepare(
        sessionID: String,
        generation: UInt64,
        viewerCount: Int,
        screenRecordingAuthorized: Bool,
        configuration: CaptureConfiguration? = nil,
        sourceKind: CaptureSourceKind = .display
    ) -> CaptureStatusSnapshot {
        requiresScreenRecording = sourceKind != .simulator
        let permitted = screenRecordingAuthorized || !requiresScreenRecording
        if let configuration { self.configuration = configuration }
        let nextGeneration = max(generation, status.generation &+ 1)
        status = CaptureStatusSnapshot(
            state: viewerCount > 0 && permitted ? .awaitingSource : (viewerCount > 0 ? .permissionDenied : .stopped),
            sessionID: sessionID,
            generation: nextGeneration,
            helperInstanceID: helperInstanceID,
            sourceID: nil,
            viewerCount: viewerCount,
            queuedFrameCount: 0,
            droppedFrameCount: 0,
            frameSequence: 0,
            geometryRevision: 0,
            pickerAvailable: pickerAvailable,
            screenRecordingAuthorized: screenRecordingAuthorized,
            audioEnabled: self.configuration.audioEnabled,
            configuration: self.configuration,
            lastFrame: nil,
            message: viewerCount > 0 && permitted ? nil : (viewerCount > 0 ? "Allow Screen Recording for Cupola on your Mac" : nil),
            reason: viewerCount > 0 ? (permitted ? nil : "screen_recording_permission_required") : "no_authorized_viewers"
        )
        return status
    }

    @discardableResult
    public mutating func updateConfiguration(
        _ configuration: CaptureConfiguration, sessionID: String, generation: UInt64
    ) -> CaptureStatusSnapshot? {
        guard status.sessionID == sessionID, status.generation == generation,
              status.state == .capturing || status.state == .paused || status.state == .ready else { return nil }
        self.configuration = configuration
        status = copyStatus(message: status.message, reason: status.reason)
        return status
    }

    @discardableResult
    public mutating func selectSource(_ source: CaptureSourceDescriptor) -> CaptureStatusSnapshot {
        guard status.sessionID != nil,
              (source.kind == .simulator ? !requiresScreenRecording : status.screenRecordingAuthorized),
              status.state == .awaitingSource || status.state == .ready else { return status }
        status = copyStatus(
            state: .ready,
            sourceID: source.id,
            geometryRevision: status.geometryRevision &+ 1,
            message: nil,
            reason: nil
        )
        return status
    }

    @discardableResult
    public mutating func selectSource(
        _ source: CaptureSourceDescriptor,
        sessionID: String,
        generation: UInt64
    ) -> CaptureStatusSnapshot {
        guard status.sessionID == sessionID, status.generation == generation else { return status }
        return selectSource(source)
    }

    @discardableResult
    public mutating func beginPickerSelection() -> CaptureStatusSnapshot {
        guard status.sessionID != nil, status.viewerCount > 0, status.screenRecordingAuthorized || !requiresScreenRecording else { return status }
        return setState(.awaitingSource, message: "Choose what to share on your Mac", reason: nil)
    }

    @discardableResult
    public mutating func requestStart() -> CaptureStatusSnapshot {
        guard status.viewerCount > 0, status.sourceID != nil,
              status.state == .ready || status.state == .paused else { return status }
        return setState(.starting, message: nil, reason: nil)
    }

    @discardableResult
    public mutating func captureStarted() -> CaptureStatusSnapshot {
        guard status.state == .starting else { return status }
        return setState(.capturing, message: nil, reason: nil)
    }

    @discardableResult
    public mutating func pause() -> CaptureStatusSnapshot {
        guard status.state == .capturing || status.state == .starting else { return status }
        return setState(.paused, message: nil, reason: "paused_by_request")
    }

    @discardableResult
    public mutating func resume() -> CaptureStatusSnapshot {
        guard status.state == .paused else { return status }
        return setState(.starting, message: nil, reason: nil)
    }

    @discardableResult
    public mutating func stop(reason: String = "stopped_by_request") -> CaptureStatusSnapshot {
        if status.state == .stopped {
            status = copyStatus(
                queuedFrameCount: 0,
                message: status.message,
                reason: status.reason
            )
            return status
        }
        status = copyStatus(state: .stopped, queuedFrameCount: 0, message: nil, reason: reason)
        return status
    }

    @discardableResult
    public mutating func setViewerCount(_ viewerCount: Int) -> CaptureStatusSnapshot {
        let count = max(0, viewerCount)
        if count == 0, status.state != .idle, status.state != .helperRestarted, status.state != .stopped {
            status = copyStatus(state: .stopped, viewerCount: count, queuedFrameCount: 0, message: nil, reason: "no_authorized_viewers")
        } else {
            status = copyStatus(viewerCount: count, message: status.message, reason: status.reason)
        }
        return status
    }

    @discardableResult
    public mutating func permissionDenied(reason: String = "screen_recording_permission_required") -> CaptureStatusSnapshot {
        status = copyStatus(
            state: .permissionDenied,
            queuedFrameCount: 0,
            screenRecordingAuthorized: false,
            message: "Allow Screen Recording for Cupola on your Mac",
            reason: reason
        )
        return status
    }

    @discardableResult
    public mutating func updateScreenRecordingAuthorization(_ authorized: Bool) -> CaptureStatusSnapshot {
        if !authorized, requiresScreenRecording,
           status.state != .idle,
           status.state != .stopped,
           status.state != .helperRestarted,
           status.state != .permissionDenied {
            return permissionDenied()
        }
        status = copyStatus(screenRecordingAuthorized: authorized, message: status.message, reason: status.reason)
        return status
    }

    @discardableResult
    public mutating func sourceRemoved(reason: String = "source_removed") -> CaptureStatusSnapshot {
        status = copyStatus(state: .sourceRemoved, queuedFrameCount: 0, message: nil, reason: reason)
        return status
    }

    @discardableResult
    public mutating func suspended(reason: String = "source_inactive") -> CaptureStatusSnapshot {
        guard status.state == .capturing || status.state == .starting else { return status }
        status = copyStatus(state: .suspended, queuedFrameCount: 0, message: nil, reason: reason)
        return status
    }

    @discardableResult
    public mutating func resumedFromSuspension() -> CaptureStatusSnapshot {
        guard status.state == .suspended else { return status }
        return setState(.capturing, message: nil, reason: nil)
    }

    /// A closed or hidden window can report suspension without a video image.
    /// Native notifications must not undo a person's Pause or a terminal state.
    @discardableResult
    public mutating func recordFrameStatus(_ frameStatus: SCFrameStatus) -> CaptureStatusSnapshot {
        switch frameStatus {
        case .suspended, .blank:
            return suspended()
        case .complete, .started:
            return resumedFromSuspension()
        case .stopped:
            guard status.state == .capturing || status.state == .starting || status.state == .suspended else { return status }
            return stop(reason: "capture_stream_stopped")
        case .idle:
            return status
        @unknown default:
            return status
        }
    }

    @discardableResult
    public mutating func failed(reason: String = "capture_failed") -> CaptureStatusSnapshot {
        status = copyStatus(state: .failed, queuedFrameCount: 0, message: nil, reason: reason)
        return status
    }

    @discardableResult
    public mutating func helperRestarted() -> CaptureStatusSnapshot {
        status = CaptureStatusSnapshot(
            state: .helperRestarted,
            sessionID: nil,
            generation: status.generation &+ 1,
            helperInstanceID: helperInstanceID,
            sourceID: nil,
            viewerCount: 0,
            queuedFrameCount: 0,
            droppedFrameCount: 0,
            frameSequence: 0,
            geometryRevision: 0,
            pickerAvailable: pickerAvailable,
            screenRecordingAuthorized: status.screenRecordingAuthorized,
            audioEnabled: configuration.audioEnabled,
            configuration: configuration,
            lastFrame: nil,
            message: nil,
            reason: "helper_restarted"
        )
        return status
    }

    @discardableResult
    public mutating func recordFrame(_ metadata: CaptureFrameMetadata, queuedFrameCount: Int, droppedFrameCount: UInt64) -> CaptureStatusSnapshot {
        guard status.sessionID == metadata.sessionID,
              status.generation == metadata.generation else { return status }
        status = copyStatus(
            queuedFrameCount: queuedFrameCount,
            droppedFrameCount: droppedFrameCount,
            frameSequence: metadata.frameSequence,
            geometryRevision: metadata.geometryRevision,
            lastFrame: metadata
        )
        return status
    }

    private mutating func setState(_ state: CaptureLifecycleState, message: String?, reason: String?) -> CaptureStatusSnapshot {
        status = copyStatus(state: state, message: message, reason: reason)
        return status
    }

    private func copyStatus(
        state: CaptureLifecycleState? = nil,
        sessionID: String? = nil,
        generation: UInt64? = nil,
        sourceID: String? = nil,
        viewerCount: Int? = nil,
        queuedFrameCount: Int? = nil,
        droppedFrameCount: UInt64? = nil,
        frameSequence: UInt64? = nil,
        geometryRevision: UInt64? = nil,
        screenRecordingAuthorized: Bool? = nil,
        lastFrame: CaptureFrameMetadata? = nil,
        message: String? = nil,
        reason: String? = nil
    ) -> CaptureStatusSnapshot {
        CaptureStatusSnapshot(
            state: state ?? status.state,
            sessionID: sessionID ?? status.sessionID,
            generation: generation ?? status.generation,
            helperInstanceID: helperInstanceID,
            sourceID: sourceID ?? status.sourceID,
            viewerCount: viewerCount ?? status.viewerCount,
            queuedFrameCount: queuedFrameCount ?? status.queuedFrameCount,
            droppedFrameCount: droppedFrameCount ?? status.droppedFrameCount,
            frameSequence: frameSequence ?? status.frameSequence,
            geometryRevision: geometryRevision ?? status.geometryRevision,
            pickerAvailable: pickerAvailable,
            screenRecordingAuthorized: screenRecordingAuthorized ?? status.screenRecordingAuthorized,
            audioEnabled: configuration.audioEnabled,
            configuration: configuration,
            lastFrame: lastFrame ?? status.lastFrame,
            message: message,
            reason: reason
        )
    }
}
