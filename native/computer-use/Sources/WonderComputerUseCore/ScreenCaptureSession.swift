import AppKit
import CoreMedia
import CoreVideo
import Foundation
import ImageIO
@preconcurrency import ScreenCaptureKit
import WonderSimulatorCapture

private struct SourceSelectionRequest: Equatable, Sendable {
    let sessionID: String
    let generation: UInt64
}

/// Transfers one immutable ScreenCaptureKit enumeration result to the AppKit
/// run loop. The result is consumed there and never accessed again by the
/// completion queue.
private final class SourceSelectionPayload: @unchecked Sendable {
    let content: SCShareableContent?
    let requestedSourceID: String?
    let mainDisplayID: CGDirectDisplayID
    let request: SourceSelectionRequest

    init(
        content: SCShareableContent?,
        requestedSourceID: String?,
        mainDisplayID: CGDirectDisplayID,
        request: SourceSelectionRequest
    ) {
        self.content = content
        self.requestedSourceID = requestedSourceID
        self.mainDisplayID = mainDisplayID
        self.request = request
    }
}

/// Focus observed during discovery survives subsequent app-wide window raises.
/// Keep only live identities and a bounded recent history; window-number reuse
/// by another app/process must not inherit the previous window's priority.
final class WindowSourceFocusHistory: @unchecked Sendable {
    private let lock = NSLock()
    private var sequence: UInt64 = 0
    private var lastFocused: UInt32?
    private var recent: [UInt32: (app: String?, process: Int32?, sequence: UInt64)] = [:]

    func observe(candidates: [CaptureWindowCandidate], focusedWindowID: UInt32?) -> [UInt32: UInt64] {
        lock.withLock {
            let live = Dictionary(uniqueKeysWithValues: candidates.map { ($0.windowID, $0) })
            recent = recent.filter { id, record in
                live[id].map { $0.bundleIdentifier == record.app && $0.processID == record.process } == true
            }
            if let id = focusedWindowID, let window = live[id],
               lastFocused != id || recent[id] == nil {
                sequence += 1
                recent[id] = (window.bundleIdentifier, window.processID, sequence)
            }
            lastFocused = focusedWindowID
            if recent.count > 128 {
                recent = Dictionary(uniqueKeysWithValues: recent.sorted { $0.value.sequence > $1.value.sequence }.prefix(128).map { ($0.key, $0.value) })
            }
            return recent.mapValues { $0.sequence }
        }
    }
}

/// A lock or unlock may end the stream; the session starts a new one at most
/// `limit` times for each lock before it reports the stream as failed.
struct LockRestartBudget: Equatable {
    static let limit = 2
    private(set) var used = 0
    mutating func reset() { used = 0 }
    /// True when another restart is allowed; counts it.
    mutating func spend() -> Bool {
        guard used < Self.limit else { return false }
        used += 1
        return true
    }
}

public final class ScreenCaptureSession: NSObject, @unchecked Sendable {
    public typealias EventSink = @Sendable (CaptureEvent) -> Void

    private let lock = NSLock()
    private let captureQueue: DispatchQueue
    private let eventSink: EventSink
    private let frameQueue: LatestVideoFrameSink
    private var machine: CaptureStateMachine
    private var simulator: SimulatorCapture?
    private var stream: SCStream?
    private var lockObservers: [NSObjectProtocol] = []
    private var lockedAt: Date?
    private var restartOnUnlock = false
    private var lockRestarts = LockRestartBudget()
    private var selectedFilter: SCContentFilter?
    private var selectedSource: CaptureSourceDescriptor?
    private var displays: [String: SCDisplay] = [:]
    private var windows: [String: SCWindow] = [:]
    private var pickerObserverRegistered = false
    private var pendingSelectionRequest: SourceSelectionRequest?
    private var pickerSelectionRequest: SourceSelectionRequest?
    private var frameSequence: UInt64 = 0
    private var lastFrameMetadataEmissionUptime: TimeInterval?
    private var intentionalStop = false
    private var configurationUpdateToken: UUID?
    private let maximumSourceCount = 128
    private let screenRecordingAccess: @Sendable () -> Bool
    private let sourceExists: @Sendable (String) async -> Bool
    private static let windowFocusHistory = WindowSourceFocusHistory()

    public convenience init(
        configuration: CaptureConfiguration = CaptureConfiguration(),
        eventSink: @escaping EventSink = { _ in },
        frameSink: @escaping @Sendable (CapturedVideoFrame) -> Void = { _ in }
    ) {
        let pickerAvailable: Bool
        if #available(macOS 14.0, *) {
            pickerAvailable = true
        } else {
            pickerAvailable = false
        }
        self.init(
            machine: CaptureStateMachine(configuration: configuration, pickerAvailable: pickerAvailable),
            queueDepth: configuration.queueDepth, eventSink: eventSink, frameSink: frameSink,
            screenRecordingAccess: { CGPreflightScreenCaptureAccess() }, sourceExists: Self.shareableSourceExists
        )
    }

    /// Tests start from a given capture state, with stand-ins for the Screen
    /// Recording check and the shareable-content lookup.
    init(
        machine: CaptureStateMachine,
        queueDepth: Int = CaptureConfiguration().queueDepth,
        eventSink: @escaping EventSink = { _ in },
        frameSink: @escaping @Sendable (CapturedVideoFrame) -> Void = { _ in },
        screenRecordingAccess: @escaping @Sendable () -> Bool,
        sourceExists: @escaping @Sendable (String) async -> Bool
    ) {
        self.captureQueue = DispatchQueue(label: "com.wonder.computer-use.capture", qos: .userInitiated)
        self.eventSink = eventSink
        self.frameQueue = LatestVideoFrameSink(capacity: queueDepth, consumer: frameSink)
        self.machine = machine
        self.screenRecordingAccess = screenRecordingAccess
        self.sourceExists = sourceExists
        super.init()
        observeScreenLock()
    }

    deinit {
        for token in lockObservers { DistributedNotificationCenter.default().removeObserver(token) }
        stopStream()
    }

    public func status() -> CaptureStatusSnapshot {
        let authorized = screenRecordingAccess()
        lock.lock()
        let previousState = machine.status.state
        _ = machine.updateScreenRecordingAuthorization(authorized)
        let permissionWasRevoked = previousState != .permissionDenied && machine.status.state == .permissionDenied
        let currentStream = permissionWasRevoked ? stream : nil
        if permissionWasRevoked {
            intentionalStop = true
            stream = nil
            selectedFilter = nil
            simulator?.stop()
            simulator = nil
            selectedSource = nil
            pendingSelectionRequest = nil
            pickerSelectionRequest = nil
            frameQueue.clear()
        }
        let snapshot = currentStatusLocked()
        lock.unlock()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        return snapshot
    }

    /// Front-to-back WindowServer hit test. Ignore invisible surfaces, but
    /// retain elevated panels: a covered click must never hit another app.
    public static func windowReceivesInput(_ id: UInt32, at point: CGPoint, windows: [[String: Any]]) -> Bool {
        // The pointer is listed as its own window above everything at its
        // location; it never takes input, so it cannot cover the target.
        let cursorLevel = Int(CGWindowLevelForKey(.cursorWindow))
        for window in windows {
            guard (window[kCGWindowLayer as String] as? NSNumber)?.intValue ?? 0 < cursorLevel,
                  (window[kCGWindowAlpha as String] as? NSNumber)?.doubleValue != 0,
                  let bounds = window[kCGWindowBounds as String] as? NSDictionary,
                  let rect = CGRect(dictionaryRepresentation: bounds as CFDictionary), rect.contains(point),
                  let number = (window[kCGWindowNumber as String] as? NSNumber)?.uint32Value else { continue }
            return number == id
        }
        return false
    }

    /// Pin the process from the capture enumeration, so a recycled window
    /// number cannot redirect input to a different application.
    public func selectedWindowProcessID() -> pid_t? {
        lock.lock()
        defer { lock.unlock() }
        guard let source = selectedSource, source.kind == .window else { return nil }
        return windows[source.id]?.owningApplication?.processID
    }

    /// Returns the exact source geometry selected for the current capture.
    /// Control coordinates are translated against this value, never against a
    /// caller-supplied or whole-desktop rectangle.
    public func selectedSourceDescriptor() -> CaptureSourceDescriptor? {
        lock.lock()
        defer { lock.unlock() }
        return selectedSource
    }

    public func prepare(
        sessionID: String,
        generation: UInt64,
        viewerCount: Int,
        configuration: CaptureConfiguration? = nil,
        sourceKind: CaptureSourceKind = .display
    ) -> CaptureOperationResult {
        stopStream()
        frameQueue.reset()
        lock.lock()
        configurationUpdateToken = nil
        frameSequence = 0
        lastFrameMetadataEmissionUptime = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        let snapshot = machine.prepare(
            sessionID: sessionID,
            generation: generation,
            viewerCount: viewerCount,
            screenRecordingAuthorized: CGPreflightScreenCaptureAccess(),
            configuration: configuration, sourceKind: sourceKind
        )
        lock.unlock()
        emit("capture.prepared", status: snapshot)
        return CaptureOperationResult(accepted: viewerCount > 0, action: "prepare", status: snapshot)
    }

    public func updateConfiguration(
        sessionID: String, generation: UInt64, configuration: CaptureConfiguration
    ) async -> CaptureOperationResult {
        let token = UUID()
        let prepared: (SCStream?, SCStreamConfiguration)? = lock.withLock {
            guard configurationUpdateToken == nil, machine.status.sessionID == sessionID,
                  machine.status.generation == generation,
                  machine.status.state == .capturing || machine.status.state == .paused || machine.status.state == .ready,
                  let source = selectedSource else { return nil }
            configurationUpdateToken = token
            return (stream, Self.makeStreamConfiguration(configuration, source: source))
        }
        guard let (currentStream, streamConfiguration) = prepared else {
            return CaptureOperationResult(accepted: false, action: "setQuality", status: status(),
                                          errorCode: "invalid_capture_state")
        }

        do {
            if let currentStream { try await currentStream.updateConfiguration(streamConfiguration) }
        } catch {
            return lock.withLock {
                if configurationUpdateToken == token { configurationUpdateToken = nil }
                return CaptureOperationResult(accepted: false, action: "setQuality", status: machine.status,
                                              errorCode: "capture_configuration_failed")
            }
        }

        return lock.withLock {
            guard configurationUpdateToken == token else {
                return CaptureOperationResult(accepted: false, action: "setQuality", status: machine.status,
                                              errorCode: "capture_session_changed")
            }
            configurationUpdateToken = nil
            simulator?.configure(configuration)
            guard stream === currentStream,
                  let snapshot = machine.updateConfiguration(configuration, sessionID: sessionID, generation: generation) else {
                return CaptureOperationResult(accepted: false, action: "setQuality", status: machine.status,
                                              errorCode: "capture_session_changed")
            }
            return CaptureOperationResult(accepted: true, action: "setQuality", status: snapshot)
        }
    }

    public func listSources() -> CaptureOperationResult {
        guard screenRecordingIsAvailable() else {
            let snapshot = markPermissionDenied()
            return CaptureOperationResult(
                accepted: false,
                action: "listSources",
                status: snapshot,
                message: "Allow Screen Recording for Cupola on your Mac",
                errorCode: "screen_recording_permission_required"
            )
        }

        let pending = status()
        SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { [weak self] content, error in
            guard let self else { return }
            guard let content else {
                let snapshot = self.markPermissionDenied(reason: error == nil ? "shareable_content_unavailable" : "shareable_content_unavailable")
                self.emit("capture.sourcesFailed", status: snapshot, message: "Mac content is unavailable")
                return
            }
            let sources = self.cacheSources(content)
            self.emit("capture.sources", status: self.status(), sources: sources)
        }
        return CaptureOperationResult(
            accepted: true,
            action: "listSources",
            status: pending,
            message: "Loading shareable Mac windows and displays"
        )
    }

    public func pick(sourceID: String? = nil) -> CaptureOperationResult {
        if sourceID?.isEmpty == true {
            return rejected("pick", errorCode: "source_not_found")
        }
        let current = status()
        guard current.sessionID != nil else {
            return rejected("pick", errorCode: "capture_not_prepared")
        }
        let isSimulator = sourceID?.hasPrefix("simulator:") == true
        guard isSimulator || current.screenRecordingAuthorized else {
            return CaptureOperationResult(
                accepted: false,
                action: "pick",
                status: current,
                message: current.message,
                errorCode: current.reason ?? "screen_recording_permission_required"
            )
        }

        lock.lock()
        let pending = machine.beginPickerSelection()
        let request: SourceSelectionRequest?
        if pending.state == .awaitingSource,
           let sessionID = pending.sessionID {
            request = SourceSelectionRequest(sessionID: sessionID, generation: pending.generation)
            pendingSelectionRequest = request
            pickerSelectionRequest = nil
        } else {
            request = nil
        }
        lock.unlock()
        guard let request else {
            return CaptureOperationResult(
                accepted: false,
                action: "pick",
                status: pending,
                message: pending.message,
                errorCode: pending.reason ?? "invalid_capture_state"
            )
        }

        if isSimulator, let sourceID {
            let capture = SimulatorCapture(configuration: pending.configuration)
            lock.withLock { simulator?.stop(); simulator = capture }
            capture.attach(sourceID: sourceID, selected: { [weak self, weak capture] source in
                guard let self, let capture else { return }
                self.completeSimulatorSelection(source, capture: capture, request: request)
            }, frame: { [weak self, weak capture] pixel, source in
                guard let self, let capture else { return }
                self.recordSimulatorFrame(pixel, source: source, capture: capture, request: request)
            }, removed: { [weak self, weak capture] in
                guard let self, let capture else { return }
                self.removeSimulator(capture, request: request)
            })
            return CaptureOperationResult(accepted: true, action: "pick", status: pending)
        }
        let mainDisplayID = CGMainDisplayID()
        SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { [weak self] content, error in
            // AppKit and the system picker are main-run-loop APIs. Keep the
            // asynchronous enumeration callback from touching either surface
            // on ScreenCaptureKit's unspecified completion queue.
            let payload = SourceSelectionPayload(
                content: content,
                requestedSourceID: sourceID,
                mainDisplayID: mainDisplayID,
                request: request
            )
            DispatchQueue.main.async { [weak self, payload] in
                self?.completeSourceSelection(
                    content: payload.content,
                    requestedSourceID: payload.requestedSourceID,
                    mainDisplayID: payload.mainDisplayID,
                    request: payload.request
                )
            }
        }
        return CaptureOperationResult(
            accepted: true,
            action: "pick",
            status: pending,
            message: sourceID == nil ? "Selecting your Mac display" : "Loading the selected Mac source"
        )
    }

    /// Select a new source on the existing stream, keeping its WebRTC publisher
    /// and session alive. Awaiting selection never starts a second SCStream.
    public func rebind(sessionID: String, generation: UInt64, sourceID: String?) async -> CaptureOperationResult {
        guard acceptsRebind(sessionID: sessionID, generation: generation) else {
            return rejected("rebind", errorCode: "invalid_capture_state")
        }
        let revision = status().geometryRevision
        if let sourceID {
            // Resolve before pick changes the capture state. A menu item can
            // disappear while the phone is selecting it; retain the old stream.
            guard await sourceExists(sourceID) else { return rejected("rebind", errorCode: "source_not_found") }
            guard acceptsRebind(sessionID: sessionID, generation: generation),
                  status().geometryRevision == revision else {
                return rejected("rebind", errorCode: "invalid_capture_session")
            }
        }
        let selection = pick(sourceID: sourceID)
        guard selection.accepted else { return selection }
        for _ in 0..<200 {
            let snapshot = status()
            guard snapshot.sessionID == sessionID, snapshot.generation == generation else {
                return rejected("rebind", errorCode: "invalid_capture_session")
            }
            if snapshot.state == .ready && snapshot.geometryRevision > revision { break }
            if snapshot.state != .awaitingSource { return rejected("rebind", errorCode: snapshot.reason ?? "source_not_found") }
            do { try await Task.sleep(for: .milliseconds(25)) }
            catch { return rejected("rebind", errorCode: "source_selection_cancelled") }
        }
        let selected = lock.withLock { () -> (SCStream, SCContentFilter, SCStreamConfiguration)? in
            guard machine.status.state == .ready, let stream, let selectedFilter, let selectedSource else { return nil }
            return (stream, selectedFilter, Self.makeStreamConfiguration(machine.status.configuration, source: selectedSource))
        }
        guard let (currentStream, filter, configuration) = selected else {
            return rejected("rebind", errorCode: "source_selection_timeout")
        }
        do {
            try await currentStream.updateContentFilter(filter)
            try await currentStream.updateConfiguration(configuration)
            // Drop callbacks queued under the previous filter before assigning
            // the new geometry to any frame from this same SCStream.
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                captureQueue.async { continuation.resume() }
            }
            guard isTrackedStream(currentStream), matchesIdentity(sessionID: sessionID, generation: generation),
                  status().state == .ready else { return rejected("rebind", errorCode: "invalid_capture_session") }
            let rebound = lock.withLock { () -> CaptureStatusSnapshot in
                frameQueue.clear()
                _ = machine.requestStart()
                return machine.captureStarted()
            }
            emit("capture.rebound", status: rebound, source: selectedSourceDescriptor())
            return CaptureOperationResult(accepted: true, action: "rebind", status: rebound)
        } catch {
            return rejected("rebind", errorCode: "source_rebind_failed")
        }
    }

    public func start(sessionID: String, generation: UInt64) -> CaptureOperationResult {
        guard matchesIdentity(sessionID: sessionID, generation: generation) else {
            return rejected("start", errorCode: "invalid_capture_session")
        }
        if let result = startSimulator(sessionID: sessionID, generation: generation, resuming: false) { return result }
        guard screenRecordingIsAvailable() else {
            let snapshot = markPermissionDenied()
            return CaptureOperationResult(
                accepted: false,
                action: "start",
                status: snapshot,
                message: "Allow Screen Recording for Cupola on your Mac",
                errorCode: "screen_recording_permission_required"
            )
        }

        lock.lock()
        guard let source = selectedSource, let filter = selectedFilter else {
            let snapshot = machine.status
            lock.unlock()
            return CaptureOperationResult(accepted: false, action: "start", status: snapshot, errorCode: "source_not_selected")
        }
        let snapshot = machine.requestStart()
        let config = Self.makeStreamConfiguration(machine.status.configuration, source: source)
        lock.unlock()
        guard snapshot.state == .starting else {
            return CaptureOperationResult(accepted: false, action: "start", status: snapshot, errorCode: "invalid_capture_state")
        }

        let newStream = SCStream(filter: filter, configuration: config, delegate: self)
        lock.lock()
        stream = newStream
        selectedSource = source
        intentionalStop = false
        lock.unlock()
        Task { [weak self, newStream] in
            guard let self else { return }
            do {
                try newStream.addStreamOutput(self, type: .screen, sampleHandlerQueue: self.captureQueue)
                guard self.isCurrentStream(newStream) else {
                    try? await newStream.stopCapture()
                    return
                }
                try await newStream.startCapture()
                guard self.isCurrentStream(newStream) else { return }
                let started = self.markStarted()
                self.emit("capture.started", status: started)
            } catch {
                guard self.isTrackedStream(newStream) else { return }
                let failed = self.markFailed(reason: self.reason(for: error))
                self.emit("capture.failed", status: failed, message: "Mac sharing could not attach its capture output")
            }
        }
        return CaptureOperationResult(accepted: true, action: "start", status: status(), message: "Starting Mac sharing")
    }

    public func pause(sessionID: String, generation: UInt64) -> CaptureOperationResult {
        guard matchesIdentity(sessionID: sessionID, generation: generation) else {
            return rejected("pause", errorCode: "invalid_capture_session")
        }
        lock.lock()
        let snapshot = machine.pause()
        simulator?.pause()
        let currentStream = stream
        intentionalStop = true
        lock.unlock()
        guard snapshot.state == .paused else {
            return CaptureOperationResult(accepted: false, action: "pause", status: snapshot, errorCode: "invalid_capture_state")
        }
        if let currentStream {
            Task { [weak self, currentStream] in
                do {
                    try await currentStream.stopCapture()
                    guard self?.isTrackedStream(currentStream) == true else { return }
                    self?.emit("capture.paused", status: self?.status() ?? snapshot)
                } catch {
                    guard self?.isTrackedStream(currentStream) == true else { return }
                    let failed = self?.markFailed(reason: "pause_failed") ?? snapshot
                    self?.emit("capture.failed", status: failed, message: "Mac sharing could not pause")
                }
            }
        }
        return CaptureOperationResult(accepted: true, action: "pause", status: snapshot)
    }

    public func resume(sessionID: String, generation: UInt64) -> CaptureOperationResult {
        guard matchesIdentity(sessionID: sessionID, generation: generation) else {
            return rejected("resume", errorCode: "invalid_capture_session")
        }
        if let result = startSimulator(sessionID: sessionID, generation: generation, resuming: true) { return result }
        guard screenRecordingIsAvailable() else {
            let snapshot = markPermissionDenied()
            return CaptureOperationResult(accepted: false, action: "resume", status: snapshot, errorCode: "screen_recording_permission_required")
        }
        lock.lock()
        let snapshot = machine.resume()
        let currentStream = stream
        intentionalStop = false
        lock.unlock()
        guard snapshot.state == .starting, let currentStream else {
            return CaptureOperationResult(accepted: false, action: "resume", status: snapshot, errorCode: "capture_not_paused")
        }
        Task { [weak self, currentStream] in
            guard let self else { return }
            do {
                guard self.isCurrentStream(currentStream) else { return }
                try await currentStream.startCapture()
                guard self.isCurrentStream(currentStream) else { return }
                let started = self.markStarted()
                self.emit("capture.resumed", status: started)
            } catch {
                guard self.isTrackedStream(currentStream) else { return }
                let failed = self.markFailed(reason: self.reason(for: error))
                self.emit("capture.failed", status: failed, message: "Mac sharing could not resume")
            }
        }
        return CaptureOperationResult(accepted: true, action: "resume", status: snapshot)
    }

    public func stop(sessionID: String, generation: UInt64, reason: String = "stopped_by_request") -> CaptureOperationResult {
        if !matchesIdentity(sessionID: sessionID, generation: generation) {
            return rejected("stop", errorCode: "invalid_capture_session")
        }
        lock.lock()
        configurationUpdateToken = nil
        let wasStopped = machine.status.state == .stopped
        let snapshot = machine.stop(reason: reason)
        intentionalStop = true
        let currentStream = stream
        stream = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        if !wasStopped { emit("capture.stopped", status: snapshot) }
        return CaptureOperationResult(accepted: true, action: "stop", status: snapshot)
    }

    public func markSourceRemoved(sessionID: String, generation: UInt64) -> CaptureOperationResult {
        guard matchesIdentity(sessionID: sessionID, generation: generation) else {
            return rejected("sourceRemoved", errorCode: "invalid_capture_session")
        }
        lock.lock()
        let snapshot = machine.sourceRemoved()
        intentionalStop = true
        let currentStream = stream
        stream = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        emit("capture.sourceRemoved", status: snapshot, message: "The selected Mac window is no longer available")
        return CaptureOperationResult(accepted: true, action: "sourceRemoved", status: snapshot)
    }

    public func setViewerCount(_ count: Int, sessionID: String, generation: UInt64) -> CaptureOperationResult {
        guard matchesIdentity(sessionID: sessionID, generation: generation) else {
            return rejected("viewerCount", errorCode: "invalid_capture_session")
        }
        lock.lock()
        let snapshot = machine.setViewerCount(count)
        let shouldStop = snapshot.state == .stopped && snapshot.reason == "no_authorized_viewers"
        let currentStream = shouldStop ? stream : nil
        if shouldStop {
            simulator?.stop()
            simulator = nil
            stream = nil
            pendingSelectionRequest = nil
            pickerSelectionRequest = nil
        }
        lock.unlock()
        if shouldStop {
            if let currentStream {
                Task { try? await currentStream.stopCapture() }
            }
            frameQueue.clear()
            emit("capture.stopped", status: snapshot)
        }
        return CaptureOperationResult(accepted: true, action: "viewerCount", status: snapshot)
    }

    public func restart() -> CaptureOperationResult {
        stopStream()
        frameQueue.reset()
        let snapshot: CaptureStatusSnapshot = {
            lock.lock()
            defer { lock.unlock() }
            let snapshot = machine.helperRestarted()
            selectedFilter = nil
            simulator?.stop()
            simulator = nil
            selectedSource = nil
            pendingSelectionRequest = nil
            pickerSelectionRequest = nil
            frameSequence = 0
            lastFrameMetadataEmissionUptime = nil
            return snapshot
        }()
        emit("capture.helperRestarted", status: snapshot)
        return CaptureOperationResult(accepted: true, action: "restart", status: snapshot)
    }

    public func shutdown(reason: String = "helper_stopped") -> CaptureStatusSnapshot {
        lock.lock()
        let wasStopped = machine.status.state == .stopped
        let snapshot = machine.stop(reason: reason)
        intentionalStop = true
        let currentStream = stream
        stream = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        if !wasStopped { emit("capture.stopped", status: snapshot) }
        return snapshot
    }

    // Resolve Xcode once per helper lifetime, so simulator discovery also avoids
    // a fresh xcode-select subprocess on every five-second poll.
    private static let discoveryDeveloperDirectory = SimulatorCapture.developerDirectory()

    public static func simulatorSources() -> [[String: String]]? {
        guard let developer = discoveryDeveloperDirectory else { return nil }
        return try? WonderSimulatorCapture.bootedDevices(developer)
    }

    /// List actual windows for the apps the harness reported. Discovery never
    /// activates an app or chooses a frontmost window on the person's behalf.
    /// `browserTab` is Chrome's AppleScript report for the tab an agent's browser
    /// tools named (`BrowserTabWindow`); the completion then also says which
    /// listed window holds it.
    public enum CaptureAvailability: Sendable {
        case available, noDisplay, permissionRequired

        init(hasDisplay: Bool, permissionGranted: Bool) {
            self = !hasDisplay ? .noDisplay : permissionGranted ? .available : .permissionRequired
        }

        public var available: Bool { self == .available }
        public var json: [String: Any] {
            var result: [String: Any] = ["available": available, "liveQualityChange": true]
            switch self {
            case .available: break
            case .noDisplay: result["reason"] = "no_display"
            case .permissionRequired: result["reason"] = "screen_recording_permission_required"
            }
            return result
        }
    }

    /// Discovery and negotiation share this capability. A display that needs
    /// Screen Recording permission is recoverable; a headless host is not.
    public static var captureCapability: CaptureAvailability {
        var count: UInt32 = 0
        let hasDisplay = CGGetOnlineDisplayList(0, nil, &count) == .success && count > 0
        return CaptureAvailability(hasDisplay: hasDisplay, permissionGranted: CGPreflightScreenCaptureAccess())
    }

    public static func windowSources(appIDs: Set<String>, allWindows: Bool = false, browserTab: [BrowserTabWindow.ScriptWindow]? = nil,
                                     completion: @escaping @Sendable ([[String: String]]?, BrowserTabWindow.Match?, Bool) -> Void) {
        guard captureCapability.available, allWindows || (!appIDs.isEmpty && appIDs.count <= 8) else {
            completion(nil, nil, false)
            return
        }
        let frontmostProcessID = NSWorkspace.shared.frontmostApplication?.processIdentifier
        SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { content, _ in
            // Permission loss is authoritative; an enumeration error is transient
            // and must not overwrite the daemon's last validated snapshot.
            guard let content else { completion(nil, nil, true); return }
            let info = windowServerInfo()
            let candidates = content.windows.map { window in
                CaptureWindowCandidate(
                    windowID: window.windowID,
                    bundleIdentifier: window.owningApplication?.bundleIdentifier,
                    applicationName: window.owningApplication?.applicationName,
                    processID: window.owningApplication?.processID,
                    title: window.title, width: window.frame.width, height: window.frame.height,
                    isOnScreen: window.isOnScreen, windowLayer: window.windowLayer,
                    zOrder: info[window.windowID]?.order,
                    opacity: info[window.windowID]?.opacity ?? 1,
                    isRegularApplication: window.owningApplication.flatMap { NSRunningApplication(processIdentifier: $0.processID) }?.activationPolicy == .regular
                )
            }
            let focused = candidates.filter {
                frontmostProcessID != nil && $0.processID == frontmostProcessID && $0.zOrder != nil && eligibleDiscoveryWindow($0)
            }.min { ($0.zOrder ?? Int.max) < ($1.zOrder ?? Int.max) }?.windowID
            let focusRecency = windowFocusHistory.observe(candidates: candidates, focusedWindowID: focused)
            let sources = discoveredWindowSources(candidates: candidates, appIDs: appIDs,
                                                  allWindows: allWindows, focusRecency: focusRecency)
            var match = browserTab.map { script in
                BrowserTabWindow.match(script, screen: content.windows.filter {
                    $0.owningApplication?.bundleIdentifier == BrowserTabWindow.bundleIdentifier
                        && $0.isOnScreen && $0.windowLayer == 0
                        && $0.frame.width >= CaptureWindowResolver.minimumWidth
                        && $0.frame.height >= CaptureWindowResolver.minimumHeight && $0.windowID > 0
                }.map { BrowserTabWindow.ScreenWindow(id: $0.windowID, frame: $0.frame, title: $0.title) })
            }
            if case let .exact(window) = match,
               !sources.contains(where: { $0["id"] == CaptureWindowTarget(appIdentifier: BrowserTabWindow.bundleIdentifier, windowNumber: window)?.sourceID }) {
                // A tab in an older window outside the cap cannot bind a pill.
                match = .notFound
            }
            let tabMatch = match
            windowIconQueue.async {
                let decorated = sources.map { source in
                    var source = source
                    if let app = source["appId"], let icon = windowIcon(app) { source["iconPNG"] = icon }
                    return source
                }
                let bounded = boundedWindowSources(decorated)
                var boundedMatch = tabMatch
                if case let .exact(window) = boundedMatch,
                   !bounded.contains(where: { $0["id"] == CaptureWindowTarget(appIdentifier: BrowserTabWindow.bundleIdentifier, windowNumber: window)?.sourceID }) {
                    boundedMatch = .notFound
                }
                completion(bounded, boundedMatch, false)
            }
        }
    }

    private static func eligibleDiscoveryWindow(_ window: CaptureWindowCandidate) -> Bool {
        window.isOnScreen && window.windowLayer == 0 && window.windowID > 0
            && window.width >= CaptureWindowResolver.minimumWidth && window.height >= CaptureWindowResolver.minimumHeight
            && window.opacity > 0
            // These are system/agent overlays, not application documents. Keep
            // title matching out of this filter: a document can be named anything.
            && !["com.openai.sky.CUAService", "com.apple.dock", "com.apple.WindowManager",
                 "com.apple.controlcenter", "com.apple.notificationcenterui", "com.apple.SystemUIServer"].contains(window.bundleIdentifier ?? "")
            && (window.isRegularApplication || !(window.title?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ?? true))
    }

    /// Observed focus comes first; window-server order seeds windows not yet
    /// observed in focus. Filter before cutting so unrelated/hidden windows
    /// cannot displace the latest 32 eligible windows.
    static func discoveredWindowSources(candidates: [CaptureWindowCandidate], appIDs: Set<String>, allWindows: Bool = false,
                                        focusRecency: [UInt32: UInt64] = [:]) -> [[String: String]] {
        let eligible = candidates.filter {
            eligibleDiscoveryWindow($0) && $0.bundleIdentifier.map { allWindows || appIDs.contains($0) } == true
        }.sorted {
            let leftFocus = focusRecency[$0.windowID] ?? 0
            let rightFocus = focusRecency[$1.windowID] ?? 0
            if leftFocus != rightFocus { return leftFocus > rightFocus }
            let left = $0.zOrder ?? Int.max
            let right = $1.zOrder ?? Int.max
            return left == right ? $0.windowID < $1.windowID : left < right
        }
        return Array(eligible.compactMap { window -> [String: String]? in
            guard let appID = window.bundleIdentifier,
                  !appID.isEmpty, appID.utf8.count <= 150,
                  appID.utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || [45, 46, 95].contains($0) }),
                  let target = CaptureWindowTarget(appIdentifier: appID, windowNumber: window.windowID),
                  target.sourceID.utf8.count <= 160 else { return nil }
            let app = window.applicationName ?? appID
            let title = window.title?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            var name = String((title.isEmpty ? app : "\(app) — \(title)")
                .components(separatedBy: .controlCharacters).joined(separator: " ").prefix(160))
            while name.utf8.count > 160 { name.removeLast() }
            var source = ["id": target.sourceID, "kind": "window", "appId": appID, "name": name,
                          "appName": discoveryText(app)]
            if !title.isEmpty { source["windowTitle"] = discoveryText(title) }
            return source
        }.prefix(32))
    }

    private static func discoveryText(_ text: String) -> String {
        var result = String(text.components(separatedBy: .controlCharacters).joined(separator: " ").prefix(160))
        while result.utf8.count > 160 { result.removeLast() }
        return result
    }

    static func boundedWindowSources(_ sources: [[String: String]]) -> [[String: String]] {
        var result = Array(sources.prefix(32))
        // Reserve room for browser-tab metadata and the JSONL newline. Escaped
        // titles can expand despite the per-field and per-icon bounds.
        while !result.isEmpty,
              (try? JSONSerialization.data(withJSONObject: ["available": true, "sources": result]).count) ?? Int.max > 128 * 1024 - 512 {
            result.removeLast()
        }
        return result
    }

    private static let windowIconQueue = DispatchQueue(label: "wonder.window-icons", qos: .utility)
    // Accessed only by windowIconQueue. App metadata changes invalidate the key;
    // the bounded cache never retains decoded full-size app artwork.
    nonisolated(unsafe) private static let windowIcons: NSCache<NSString, NSString> = {
        let cache = NSCache<NSString, NSString>()
        cache.countLimit = 64
        cache.totalCostLimit = 256 * 1024
        return cache
    }()

    private static func windowIcon(_ app: String) -> String? {
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: app) else { return nil }
        let modified = (try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate)?.timeIntervalSince1970 ?? 0
        let key = "\(url.path):\(modified)" as NSString
        if let icon = windowIcons.object(forKey: key) { return icon as String }
        guard let data = NSWorkspace.shared.icon(forFile: url.path).tiffRepresentation,
              let input = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        for size in [32, 24, 16] {
            guard let thumbnail = CGImageSourceCreateThumbnailAtIndex(input, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceThumbnailMaxPixelSize: size,
                kCGImageSourceShouldCacheImmediately: true
            ] as CFDictionary),
                  let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
                  let context = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                                          bytesPerRow: size * 4, space: colorSpace,
                                          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { continue }
            // AppKit may supply 16-bit TIFF artwork. Normalize to 8-bit before
            // PNG encoding so a sharp 32px icon fits the wire's byte budget.
            context.interpolationQuality = .high
            context.draw(thumbnail, in: CGRect(x: 0, y: 0, width: size, height: size))
            guard let image = context.makeImage() else { continue }
            let png = NSMutableData()
            guard let output = CGImageDestinationCreateWithData(png, "public.png" as CFString, 1, nil) else { continue }
            CGImageDestinationAddImage(output, image, nil)
            guard CGImageDestinationFinalize(output) else { continue }
            let encoded = (png as Data).base64EncodedString()
            if encoded.utf8.count <= 3072 {
                windowIcons.setObject(encoded as NSString, forKey: key, cost: encoded.utf8.count)
                return encoded
            }
        }
        return nil
    }

    private static func windowServerOrder() -> [CGWindowID: Int] {
        windowServerInfo().mapValues(\.order)
    }

    private static func windowServerInfo() -> [CGWindowID: (order: Int, opacity: Double)] {
        var infoByID: [CGWindowID: (order: Int, opacity: Double)] = [:]
        if let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] {
            for (index, entry) in info.enumerated() {
                guard let number = entry[kCGWindowNumber as String] as? NSNumber else { continue }
                infoByID[CGWindowID(number.uint32Value)] = (index, (entry[kCGWindowAlpha as String] as? NSNumber)?.doubleValue ?? 1)
            }
        }
        return infoByID
    }

    private func completeSimulatorSelection(_ source: CaptureSourceDescriptor?, capture: SimulatorCapture,
                                            request: SourceSelectionRequest) {
        lock.lock()
        guard simulator === capture, isCurrentSelectionLocked(request) else { lock.unlock(); capture.stop(); return }
        guard let source else {
            let failed = machine.failed(reason: "simulator_unavailable")
            simulator = nil
            pendingSelectionRequest = nil
            lock.unlock()
            capture.stop()
            emit("capture.failed", status: failed, message: "The selected simulator is unavailable")
            return
        }
        let snapshot = machine.selectSource(source)
        guard snapshot.state == .ready else {
            simulator = nil
            pendingSelectionRequest = nil
            let failed = machine.failed(reason: "invalid_capture_state")
            lock.unlock()
            capture.stop()
            emit("capture.failed", status: failed)
            return
        }
        selectedSource = source
        pendingSelectionRequest = nil
        lock.unlock()
        emit("capture.sourceSelected", status: snapshot, source: source)
    }

    private func removeSimulator(_ capture: SimulatorCapture, request: SourceSelectionRequest) {
        lock.lock()
        guard simulator === capture, machine.status.sessionID == request.sessionID,
              machine.status.generation == request.generation else { lock.unlock(); return }
        let snapshot = machine.sourceRemoved()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        lock.unlock()
        capture.stop()
        frameQueue.clear()
        emit("capture.sourceRemoved", status: snapshot, message: "The selected simulator has stopped")
    }

    /// Sends the Simulator's current picture again; nothing for other sources.
    public func redrawSimulatorFrame() {
        lock.lock()
        let capture = simulator
        lock.unlock()
        capture?.redraw()
    }

    private func startSimulator(sessionID: String, generation: UInt64, resuming: Bool) -> CaptureOperationResult? {
        lock.lock()
        guard let simulator, let source = selectedSource, source.kind == .simulator else { lock.unlock(); return nil }
        let starting = resuming ? machine.resume() : machine.requestStart()
        guard starting.state == .starting else {
            lock.unlock()
            return CaptureOperationResult(accepted: false, action: resuming ? "resume" : "start", status: starting, errorCode: "invalid_capture_state")
        }
        let started = machine.captureStarted()
        lock.unlock()
        emit(resuming ? "capture.resumed" : "capture.started", status: started)
        simulator.start(source: source)
        return CaptureOperationResult(accepted: true, action: resuming ? "resume" : "start", status: started)
    }

    private func recordSimulatorFrame(_ pixel: CVPixelBuffer, source: CaptureSourceDescriptor,
                                       capture: SimulatorCapture, request: SourceSelectionRequest) {
        lock.lock()
        guard simulator === capture, machine.status.sessionID == request.sessionID,
              machine.status.generation == request.generation, machine.status.state == .capturing else { lock.unlock(); return }
        let dimensionsChanged = machine.status.lastFrame?.sourceWidth != CVPixelBufferGetWidth(pixel)
            || machine.status.lastFrame?.sourceHeight != CVPixelBufferGetHeight(pixel)
        let geometryChanged = selectedSource?.contentRect != source.contentRect
        let revision = machine.status.geometryRevision &+ (geometryChanged ? 1 : 0)
        selectedSource = source
        frameSequence &+= 1
        let now = ProcessInfo.processInfo.systemUptime
        let metadata = CaptureFrameMetadata.make(timestamp: now,
            sourceWidth: CVPixelBufferGetWidth(pixel), sourceHeight: CVPixelBufferGetHeight(pixel), source: source,
            geometryRevision: revision, frameSequence: frameSequence,
            sessionID: request.sessionID, generation: request.generation)
        frameQueue.offer(CapturedVideoFrame(pixelBuffer: pixel, timestamp: CMTime(seconds: now, preferredTimescale: 1_000_000), metadata: metadata))
        let stats = frameQueue.statistics()
        let snapshot = machine.recordFrame(metadata, queuedFrameCount: stats.count, droppedFrameCount: stats.droppedCount)
        let send = lastFrameMetadataEmissionUptime.map { now - $0 >= 1 } ?? true
        if send { lastFrameMetadataEmissionUptime = now }
        lock.unlock()
        if send || geometryChanged || dimensionsChanged { emit("capture.frameMetadata", status: snapshot, metadata: metadata, source: source) }
    }

    private func completeSourceSelection(
        content: SCShareableContent?,
        requestedSourceID: String?,
        mainDisplayID: CGDirectDisplayID,
        request: SourceSelectionRequest
    ) {
        guard isCurrentSelection(request) else { return }

        guard screenRecordingIsAvailable() else {
            guard let denied = markPermissionDenied(for: request) else { return }
            emit("capture.failed", status: denied, message: "Allow Screen Recording for Cupola on your Mac")
            return
        }

        guard let content else {
            guard let failed = markSourceSelectionFailed(for: request, reason: "shareable_content_unavailable") else { return }
            emit("capture.failed", status: failed, message: "Mac content is unavailable")
            return
        }

        // An agent's app target ("app:<bundle id or name>[#window]") resolves
        // to one concrete window before the ordinary plan runs.
        var plannedSourceID = requestedSourceID
        var pinnedWindowID: CGWindowID?
        if let requestedSourceID, let target = CaptureWindowTarget(sourceID: requestedSourceID) {
            guard let windowID = Self.resolveWindow(target, in: content) else {
                guard let failed = markSourceSelectionFailed(for: request, reason: "window_not_found") else { return }
                emit("capture.failed", status: failed, message: "That app has no open window on your Mac")
                return
            }
            pinnedWindowID = windowID
            plannedSourceID = "window:\(windowID)"
        }

        guard let sources = cacheSources(content, for: request, pinnedWindowID: pinnedWindowID) else { return }
        let plan = CaptureSourceSelectionPlanner.plan(
            requestedSourceID: plannedSourceID,
            displayIDs: Set(content.displays.map { UInt32($0.displayID) }),
            sourceIDs: Set(sources.map(\.id)),
            mainDisplayID: UInt32(mainDisplayID),
            pickerAvailable: pickerIsAvailable(),
            preferredDisplayID: SharedDisplayPreference().preferredDisplayID(
                in: content.displays.map(\.displayID)
            )
        )

        switch plan {
        case let .select(sourceID):
            guard let source = source(for: sourceID), let filter = makeFilter(for: source) else {
                guard let failed = markSourceSelectionFailed(for: request, reason: "source_unavailable") else { return }
                emit("capture.failed", status: failed, message: "The selected Mac source is unavailable")
                return
            }
            selectSource(source, filter: filter, for: request)

        case .presentSharingPicker:
            guard presentSystemPicker(for: request) else { return }
            guard let pending = currentSelectionStatus(for: request) else { return }
            emit("capture.pickerPresented", status: pending, message: "Choose a window or display on your Mac")

        case let .unavailable(reason):
            guard let failed = markSourceSelectionFailed(for: request, reason: reason) else { return }
            let message = reason == "system_picker_unavailable"
                ? "No display is available and the Mac sharing picker is unavailable"
                : "The requested Mac source is unavailable"
            emit("capture.failed", status: failed, message: message)
        }
    }

    private func cacheSources(_ content: SCShareableContent) -> [CaptureSourceDescriptor] {
        let cache = makeSourceCache(content)
        lock.lock()
        displays = cache.displays
        windows = cache.windows
        lock.unlock()
        return cache.descriptors
    }

    private func cacheSources(
        _ content: SCShareableContent,
        for request: SourceSelectionRequest,
        pinnedWindowID: CGWindowID? = nil
    ) -> [CaptureSourceDescriptor]? {
        let cache = makeSourceCache(content, pinnedWindowID: pinnedWindowID)
        lock.lock()
        guard isCurrentSelectionLocked(request) else {
            lock.unlock()
            return nil
        }
        displays = cache.displays
        windows = cache.windows
        lock.unlock()
        return cache.descriptors
    }

    /// Resolves an app target against every shareable window, not only the
    /// bounded source list, using the window server's front-to-back order.
    private static func resolveWindow(_ target: CaptureWindowTarget, in content: SCShareableContent) -> CGWindowID? {
        let zOrder = windowServerOrder()
        let candidates = content.windows.map { window in
            CaptureWindowCandidate(
                windowID: window.windowID,
                bundleIdentifier: window.owningApplication?.bundleIdentifier,
                applicationName: window.owningApplication?.applicationName,
                processID: window.owningApplication?.processID,
                title: window.title,
                width: window.frame.width,
                height: window.frame.height,
                isOnScreen: window.isOnScreen,
                windowLayer: window.windowLayer,
                zOrder: zOrder[window.windowID]
            )
        }
        return CaptureWindowResolver.resolve(target, candidates: candidates)
    }

    /// `pinnedWindowID` keeps a resolved window in the cache even when it
    /// falls outside the bounded source list.
    private func makeSourceCache(_ content: SCShareableContent, pinnedWindowID: CGWindowID? = nil) -> (
        descriptors: [CaptureSourceDescriptor],
        displays: [String: SCDisplay],
        windows: [String: SCWindow]
    ) {
        let displays = content.displays.map { display in
            let scale = NSScreen.screens.first(where: { $0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID == display.displayID })?.backingScaleFactor ?? 1
            let descriptor = CaptureSourceDescriptor(
                id: "display:\(display.displayID)",
                kind: .display,
                title: "Display \(display.displayID)",
                width: display.width,
                height: display.height,
                scale: scale,
                contentRect: CaptureRect(x: display.frame.origin.x, y: display.frame.origin.y, width: display.frame.width, height: display.frame.height)
            )
            return (descriptor, display)
        }
        let remainingSourceCount = max(0, maximumSourceCount - displays.count)
        var listedWindows = Array(content.windows.prefix(remainingSourceCount))
        if let pinnedWindowID, !listedWindows.contains(where: { $0.windowID == pinnedWindowID }),
           let pinned = content.windows.first(where: { $0.windowID == pinnedWindowID }) {
            listedWindows.append(pinned)
        }
        let windows = listedWindows.compactMap { window -> (CaptureSourceDescriptor, SCWindow)? in
            guard window.isOnScreen else { return nil }
            let title = boundedLabel(window.title, fallback: "Window \(window.windowID)")
            let application = boundedOptionalLabel(window.owningApplication?.applicationName)
            let descriptor = CaptureSourceDescriptor(
                id: "window:\(window.windowID)",
                kind: .window,
                title: title,
                application: application,
                width: Int(window.frame.width),
                height: Int(window.frame.height),
                scale: 1,
                contentRect: CaptureRect(x: window.frame.origin.x, y: window.frame.origin.y, width: window.frame.width, height: window.frame.height)
            )
            return (descriptor, window)
        }
        return (
            descriptors: (displays.map(\.0) + windows.map(\.0)).sorted { $0.id < $1.id },
            displays: Dictionary(uniqueKeysWithValues: displays.map { ("display:\($0.1.displayID)", $0.1) }),
            windows: Dictionary(uniqueKeysWithValues: windows.map { ("window:\($0.1.windowID)", $0.1) })
        )
    }

    private func source(for id: String) -> CaptureSourceDescriptor? {
        lock.lock()
        defer { lock.unlock() }
        if id.hasPrefix("display:"), let display = displays[id] {
            return CaptureSourceDescriptor(
                id: id,
                kind: .display,
                title: "Display \(display.displayID)",
                width: display.width,
                height: display.height,
                scale: 1,
                contentRect: CaptureRect(x: display.frame.origin.x, y: display.frame.origin.y, width: display.frame.width, height: display.frame.height)
            )
        }
        if id.hasPrefix("window:"), let window = windows[id] {
            return CaptureSourceDescriptor(
                id: id,
                kind: .window,
                title: boundedLabel(window.title, fallback: "Window \(window.windowID)"),
                application: boundedOptionalLabel(window.owningApplication?.applicationName),
                width: Int(window.frame.width),
                height: Int(window.frame.height),
                scale: 1,
                contentRect: CaptureRect(x: window.frame.origin.x, y: window.frame.origin.y, width: window.frame.width, height: window.frame.height)
            )
        }
        return nil
    }

    private func makeFilter(for source: CaptureSourceDescriptor) -> SCContentFilter? {
        lock.lock()
        defer { lock.unlock() }
        switch source.kind {
        case .display:
            guard let display = displays[source.id] else { return nil }
            return SCContentFilter(display: display, excludingWindows: [])
        case .window:
            guard let window = windows[source.id] else { return nil }
            return SCContentFilter(desktopIndependentWindow: window)
        case .simulator:
            return nil
        case .picker:
            return selectedFilter
        }
    }

    static func makeStreamConfiguration(_ configuration: CaptureConfiguration, source: CaptureSourceDescriptor) -> SCStreamConfiguration {
        let streamConfiguration = SCStreamConfiguration()
        streamConfiguration.width = configuration.width
        streamConfiguration.height = configuration.height
        let destination = configuration.centeredDestinationRect(for: source)
        streamConfiguration.destinationRect = CGRect(
            x: destination.x, y: destination.y,
            width: destination.width, height: destination.height
        )
        streamConfiguration.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(configuration.framesPerSecond))
        // Native 4:2:0 is what the H.264 encoder consumes, so capture it directly
        // instead of paying a BGRA conversion on every frame (FPS0 loopback
        // median encode: 1080p 5.4 -> 4.4 ms, 1440p 8.9 -> 7.2 ms).
        streamConfiguration.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        streamConfiguration.queueDepth = configuration.queueDepth
        streamConfiguration.capturesAudio = configuration.audioEnabled
        streamConfiguration.showsCursor = true
        streamConfiguration.scalesToFit = true
        if #available(macOS 14.0, *) {
            streamConfiguration.preservesAspectRatio = true
        }
        return streamConfiguration
    }

    private func presentSystemPicker(for request: SourceSelectionRequest) -> Bool {
        guard #available(macOS 14.0, *) else { return false }
        lock.lock()
        guard isCurrentSelectionLocked(request) else {
            lock.unlock()
            return false
        }
        pickerSelectionRequest = request
        lock.unlock()

        let picker = SCContentSharingPicker.shared
        if !pickerObserverRegistered {
            picker.add(self)
            pickerObserverRegistered = true
        }
        var configuration = picker.defaultConfiguration
        configuration.allowedPickerModes = [.singleWindow, .singleDisplay]
        picker.defaultConfiguration = configuration
        picker.isActive = true
        picker.present(using: .display)
        return true
    }

    private func screenRecordingIsAvailable() -> Bool {
        CGPreflightScreenCaptureAccess()
    }

    private func pickerIsAvailable() -> Bool {
        if #available(macOS 14.0, *) { return true }
        return false
    }

    private func matchesIdentity(sessionID: String, generation: UInt64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return machine.status.sessionID == sessionID && machine.status.generation == generation
    }

    /// A rebind moves the stream of this exact session: one that is capturing,
    /// or suspended because its window closed, was minimized or hidden.
    private func acceptsRebind(sessionID: String, generation: UInt64) -> Bool {
        matchesIdentity(sessionID: sessionID, generation: generation) && status().state.acceptsSourceRebind
    }

    private static func shareableSourceExists(_ sourceID: String) async -> Bool {
        await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { content, _ in
                guard let content else { continuation.resume(returning: false); return }
                let found: Bool
                if let target = CaptureWindowTarget(sourceID: sourceID) {
                    found = resolveWindow(target, in: content) != nil
                } else if sourceID.hasPrefix("window:"), let id = UInt32(sourceID.dropFirst(7)) {
                    found = content.windows.contains { $0.windowID == id }
                } else if sourceID.hasPrefix("display:"), let id = UInt32(sourceID.dropFirst(8)) {
                    found = content.displays.contains { $0.displayID == id }
                } else { found = false }
                continuation.resume(returning: found)
            }
        }
    }

    private func isCurrentSelection(_ request: SourceSelectionRequest) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return isCurrentSelectionLocked(request)
    }

    private func isCurrentSelectionLocked(_ request: SourceSelectionRequest) -> Bool {
        pendingSelectionRequest == request
            && machine.status.sessionID == request.sessionID
            && machine.status.generation == request.generation
            && machine.status.viewerCount > 0
            && machine.status.screenRecordingAuthorized
            && machine.status.state == .awaitingSource
    }

    private func currentSelectionStatus(for request: SourceSelectionRequest) -> CaptureStatusSnapshot? {
        lock.lock()
        defer { lock.unlock() }
        guard isCurrentSelectionLocked(request) else { return nil }
        return machine.status
    }

    private func selectSource(
        _ source: CaptureSourceDescriptor,
        filter: SCContentFilter,
        for request: SourceSelectionRequest
    ) {
        lock.lock()
        guard isCurrentSelectionLocked(request) else {
            lock.unlock()
            return
        }
        let snapshot = machine.selectSource(source, sessionID: request.sessionID, generation: request.generation)
        if snapshot.state == .ready {
            selectedSource = source
            selectedFilter = filter
            pendingSelectionRequest = nil
            pickerSelectionRequest = nil
        }
        lock.unlock()
        guard snapshot.state == .ready else { return }
        emit("capture.sourceSelected", status: snapshot, source: source)
    }

    private func currentStatusLocked() -> CaptureStatusSnapshot {
        let base = machine.status
        return CaptureStatusSnapshot(
            state: base.state,
            sessionID: base.sessionID,
            generation: base.generation,
            helperInstanceID: base.helperInstanceID,
            sourceID: base.sourceID,
            viewerCount: base.viewerCount,
            queuedFrameCount: frameQueue.statistics().count,
            droppedFrameCount: frameQueue.statistics().droppedCount,
            frameSequence: base.frameSequence,
            geometryRevision: base.geometryRevision,
            pickerAvailable: base.pickerAvailable,
            screenRecordingAuthorized: base.screenRecordingAuthorized,
            audioEnabled: base.audioEnabled,
            configuration: base.configuration,
            lastFrame: base.lastFrame,
            message: base.message,
            reason: base.reason
        )
    }

    private func markStarted() -> CaptureStatusSnapshot {
        lock.lock()
        let snapshot = machine.captureStarted()
        lock.unlock()
        return snapshot
    }

    private func markFailed(reason: String) -> CaptureStatusSnapshot {
        lock.lock()
        let snapshot = machine.failed(reason: reason)
        stream = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        return snapshot
    }

    private func markPermissionDenied(reason: String = "screen_recording_permission_required") -> CaptureStatusSnapshot {
        lock.lock()
        let snapshot = machine.permissionDenied(reason: reason)
        intentionalStop = true
        let currentStream = stream
        stream = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        return snapshot
    }

    private func markPermissionDenied(for request: SourceSelectionRequest) -> CaptureStatusSnapshot? {
        lock.lock()
        guard isCurrentSelectionLocked(request) else {
            lock.unlock()
            return nil
        }
        let snapshot = machine.permissionDenied()
        intentionalStop = true
        let currentStream = stream
        stream = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        return snapshot
    }

    private func markSourceSelectionFailed(
        for request: SourceSelectionRequest,
        reason: String
    ) -> CaptureStatusSnapshot? {
        lock.lock()
        guard isCurrentSelectionLocked(request) else {
            lock.unlock()
            return nil
        }
        let snapshot = machine.failed(reason: reason)
        intentionalStop = true
        let currentStream = stream
        stream = nil
        selectedFilter = nil
        simulator?.stop()
        simulator = nil
        selectedSource = nil
        pendingSelectionRequest = nil
        pickerSelectionRequest = nil
        lock.unlock()
        frameQueue.clear()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
        return snapshot
    }

    /// True while locked, or within seconds of a lock or unlock event.
    private var lockJustHappened: Bool {
        lock.lock()
        let recent = lockedAt.map { Date().timeIntervalSince($0) < 15 } ?? false
        lock.unlock()
        return recent || Self.isScreenLocked
    }

    /// `CGSSessionScreenIsLocked` in the current session dictionary.
    static var isScreenLocked: Bool {
        (CGSessionCopyCurrentDictionary() as? [String: Any])?["CGSSessionScreenIsLocked"] as? Bool ?? false
    }

    private func observeScreenLock() {
        let center = DistributedNotificationCenter.default()
        for (name, unlocked) in [("com.apple.screenIsLocked", false), ("com.apple.screenIsUnlocked", true)] {
            lockObservers.append(center.addObserver(forName: Notification.Name(name), object: nil, queue: nil) { [weak self] _ in
                guard let self else { return }
                self.lock.lock()
                self.lockedAt = Date()
                if !unlocked { self.lockRestarts.reset() }
                let retry = unlocked && self.restartOnUnlock
                self.restartOnUnlock = false
                self.lock.unlock()
                if retry { self.restartStream(afterUnlock: true) }
            })
        }
    }

    /// A new stream on the same source after a lock or unlock killed the old
    /// one; if it cannot start while locked, try once more after the unlock.
    private func restartStream(afterUnlock: Bool) {
        lock.lock()
        guard let source = selectedSource, let filter = selectedFilter,
              [.capturing, .starting, .suspended].contains(machine.status.state) else {
            lock.unlock()
            return
        }
        // A stream that keeps dying around a lock is failed, not restarted forever.
        guard lockRestarts.spend() else {
            lock.unlock()
            let failed = markFailed(reason: "stream_restart_limit")
            emit("capture.failed", status: failed, message: "Mac sharing stopped")
            return
        }
        let config = Self.makeStreamConfiguration(machine.status.configuration, source: source)
        let newStream = SCStream(filter: filter, configuration: config, delegate: self)
        stream = newStream
        intentionalStop = false
        lock.unlock()
        Task { [weak self, newStream] in
            guard let self else { return }
            do {
                try newStream.addStreamOutput(self, type: .screen, sampleHandlerQueue: self.captureQueue)
                guard self.isCurrentStream(newStream) else { try? await newStream.stopCapture(); return }
                try await newStream.startCapture()
            } catch {
                guard self.isTrackedStream(newStream) else { return }
                if !afterUnlock, Self.isScreenLocked {
                    self.deferRestartToUnlock()
                    return
                }
                let failed = self.markFailed(reason: self.reason(for: error))
                self.emit("capture.failed", status: failed, message: "Mac sharing stopped")
            }
        }
    }

    private func deferRestartToUnlock() {
        lock.lock()
        restartOnUnlock = true
        lock.unlock()
    }

    private func rejected(_ action: String, errorCode: String) -> CaptureOperationResult {
        CaptureOperationResult(accepted: false, action: action, status: status(), errorCode: errorCode)
    }

    private func emit(_ event: String, status: CaptureStatusSnapshot, metadata: CaptureFrameMetadata? = nil, sources: [CaptureSourceDescriptor]? = nil, source: CaptureSourceDescriptor? = nil, message: String? = nil) {
        eventSink(CaptureEvent(event: event, status: status, metadata: metadata, sources: sources, source: source, message: message))
    }

    private func reason(for error: Error) -> String {
        let nsError = error as NSError
        return "stream_error_\(nsError.domain)_\(nsError.code)"
    }

    private func boundedLabel(_ value: String?, fallback: String) -> String {
        guard let value, !value.isEmpty else { return fallback }
        return String(value.prefix(256))
    }

    private func boundedOptionalLabel(_ value: String?) -> String? {
        guard let value, !value.isEmpty else { return nil }
        return String(value.prefix(256))
    }

    private func stopStream() {
        lock.lock()
        simulator?.stop()
        simulator = nil
        intentionalStop = true
        let currentStream = stream
        stream = nil
        lock.unlock()
        if let currentStream {
            Task { try? await currentStream.stopCapture() }
        }
    }

    private func isCurrentStream(_ candidate: SCStream) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return stream === candidate && machine.status.state == .starting
    }

    private func isTrackedStream(_ candidate: SCStream) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return stream === candidate
    }
}

extension ScreenCaptureSession: SCStreamOutput {
    public func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let rawStatus = attachments.first?[.status] as? Int,
              let frameStatus = SCFrameStatus(rawValue: rawStatus) else { return }
        // ScreenCaptureKit sends window suspension with no image buffer.
        recordFrameStatus(frameStatus, from: stream)
        guard frameStatus == .complete || frameStatus == .started,
              let imageBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        lock.lock()
        guard self.stream === stream,
              machine.status.state == .capturing,
              let source = selectedSource,
              let sessionID = machine.status.sessionID else {
            lock.unlock()
            return
        }
        frameSequence &+= 1
        let sequence = frameSequence
        let generation = machine.status.generation
        let geometryRevision = machine.status.geometryRevision
        let width = CVPixelBufferGetWidth(imageBuffer)
        let height = CVPixelBufferGetHeight(imageBuffer)
        let timestamp = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let timestampSeconds = CMTimeGetSeconds(timestamp)
        let seconds = timestamp.isValid && timestampSeconds.isFinite && timestampSeconds >= 0
            ? timestampSeconds
            : Date().timeIntervalSince1970
        let metadata = CaptureFrameMetadata.make(
            timestamp: seconds,
            sourceWidth: width,
            sourceHeight: height,
            source: source,
            geometryRevision: geometryRevision,
            frameSequence: sequence,
            sessionID: sessionID,
            generation: generation
        )
        frameQueue.offer(CapturedVideoFrame(pixelBuffer: imageBuffer, timestamp: timestamp, metadata: metadata))
        let stats = frameQueue.statistics()
        let snapshot = machine.recordFrame(metadata, queuedFrameCount: stats.count, droppedFrameCount: stats.droppedCount)
        let now = ProcessInfo.processInfo.systemUptime
        let shouldEmitMetadata = lastFrameMetadataEmissionUptime.map { now - $0 >= 1 } ?? true
        if shouldEmitMetadata {
            lastFrameMetadataEmissionUptime = now
        }
        lock.unlock()
        // The frame itself never leaves this callback. Coalesce bounded status
        // metadata so the JSONL control pipe is not driven at the video frame rate.
        if shouldEmitMetadata {
            emit("capture.frameMetadata", status: snapshot, metadata: metadata)
        }
    }

    private func recordFrameStatus(_ frameStatus: SCFrameStatus, from stream: SCStream) {
        lock.lock()
        guard self.stream === stream else {
            lock.unlock()
            return
        }
        let previousState = machine.status.state
        let snapshot = machine.recordFrameStatus(frameStatus)
        if snapshot.state == .suspended || snapshot.state == .stopped { frameQueue.clear() }
        if previousState != .stopped && snapshot.state == .stopped {
            self.stream = nil
            selectedFilter = nil
            simulator?.stop()
            simulator = nil
            selectedSource = nil
            pendingSelectionRequest = nil
            pickerSelectionRequest = nil
            configurationUpdateToken = nil
        }
        lock.unlock()
        guard previousState != snapshot.state else { return }
        switch snapshot.state {
        case .suspended:
            emit("capture.suspended", status: snapshot, message: "The selected Mac window is not available")
        case .capturing:
            emit("capture.resumed", status: snapshot)
        case .stopped:
            emit("capture.stopped", status: snapshot)
        default:
            break
        }
    }
}

extension ScreenCaptureSession: SCStreamDelegate {
    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        lock.lock()
        guard self.stream === stream else {
            lock.unlock()
            return
        }
        let wasIntentional = intentionalStop
        intentionalStop = false
        lock.unlock()
        guard !wasIntentional else { return }
        // Locking the Mac can end the stream with an error. The lock screen is
        // shareable, so start a new stream instead of failing.
        if lockJustHappened { restartStream(afterUnlock: false); return }
        let failed = markFailed(reason: reason(for: error))
        emit("capture.failed", status: failed, message: "Mac sharing stopped")
    }

    @available(macOS 15.2, *)
    public func streamDidBecomeInactive(_ stream: SCStream) {
        recordFrameStatus(.suspended, from: stream)
    }

    @available(macOS 15.2, *)
    public func streamDidBecomeActive(_ stream: SCStream) {
        recordFrameStatus(.complete, from: stream)
    }
}

@available(macOS 14.0, *)
extension ScreenCaptureSession: SCContentSharingPickerObserver {
    public func contentSharingPicker(_ picker: SCContentSharingPicker, didUpdateWith filter: SCContentFilter, for stream: SCStream?) {
        let descriptor: CaptureSourceDescriptor
        let info = SCShareableContent.info(for: filter)
        descriptor = CaptureSourceDescriptor(
            id: "picker-selection",
            kind: .picker,
            title: "Selected Mac content",
            width: Int(info.contentRect.width * CGFloat(info.pointPixelScale)),
            height: Int(info.contentRect.height * CGFloat(info.pointPixelScale)),
            scale: Double(info.pointPixelScale),
            contentRect: CaptureRect(x: info.contentRect.origin.x, y: info.contentRect.origin.y, width: info.contentRect.width, height: info.contentRect.height)
        )
        lock.lock()
        guard let request = pickerSelectionRequest, isCurrentSelectionLocked(request) else {
            lock.unlock()
            return
        }
        let snapshot = machine.selectSource(descriptor, sessionID: request.sessionID, generation: request.generation)
        if snapshot.state == .ready {
            selectedFilter = filter
            selectedSource = descriptor
            pendingSelectionRequest = nil
            pickerSelectionRequest = nil
        }
        lock.unlock()
        guard snapshot.state == .ready else { return }
        emit("capture.sourceSelected", status: snapshot, source: descriptor, message: "Mac content selected")
    }

    public func contentSharingPicker(_ picker: SCContentSharingPicker, didCancelFor stream: SCStream?) {
        lock.lock()
        let request = pickerSelectionRequest
        let isCurrent = request.map { isCurrentSelectionLocked($0) } ?? false
        let snapshot = isCurrent ? machine.status : nil
        lock.unlock()
        guard let snapshot else { return }
        emit("capture.pickerCancelled", status: snapshot, message: "Choose what to share on your Mac")
    }

    public func contentSharingPickerStartDidFailWithError(_ error: Error) {
        lock.lock()
        let request = pickerSelectionRequest
        lock.unlock()
        guard let request,
              let failed = markSourceSelectionFailed(for: request, reason: reason(for: error)) else { return }
        emit("capture.failed", status: failed, message: "The Mac sharing picker could not open")
    }
}
