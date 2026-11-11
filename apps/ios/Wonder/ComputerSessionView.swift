import Combine
import GameController
import ImageIO
import SwiftUI
import UIKit
import WonderPairing
import WonderComputerView
#if WONDER_DIAGNOSTICS
@preconcurrency import WebRTC
import CoreVideo
#endif

private struct ComputerEmptyReply: Decodable, Sendable {}

/// Reading continuity belongs to the paired connection, not the outgoing view.
/// Three pictures at most, 32 MB total, evictable under memory pressure. Never
/// persist pixels, leases, pending input or a live receiver.
@MainActor final class ComputerViewSnapshots {
    final class Snapshot: NSObject {
        let picture: ComputerPicture
        let session: ComputerSession
        init(picture: ComputerPicture, session: ComputerSession) { self.picture = picture; self.session = session }
    }
    private var cache: [String: Snapshot] = [:]
    private var order: [String] = []
    private let maximumBytes = 32 * 1024 * 1024
    private var memoryWarning: AnyCancellable?
    init() {
        memoryWarning = NotificationCenter.default.publisher(for: UIApplication.didReceiveMemoryWarningNotification)
            .sink { [weak self] _ in Task { @MainActor [weak self] in self?.invalidate() } }
    }
    func take(_ key: String) -> Snapshot? {
        let snapshot = cache.removeValue(forKey: key)
        order.removeAll { $0 == key }
        return snapshot
    }
    func save(_ key: String, picture: ComputerPicture, session: ComputerSession) {
        guard picture.hasFrame, picture.byteCost <= maximumBytes else { return }
        _ = take(key)
        cache[key] = Snapshot(picture: picture, session: session)
        order.append(key)
        while order.count > 3 || cache.values.reduce(0, { $0 + $1.picture.byteCost }) > maximumBytes {
            remove(order[0])
        }
    }
    func remove(_ key: String) { cache.removeValue(forKey: key)?.picture.clear(); order.removeAll { $0 == key } }
    func invalidate() { cache.values.forEach { $0.picture.clear() }; cache.removeAll(); order.removeAll() }
}

extension WonderTheme {
    /// The computer view and its Take control area share the chat's page colour, so the
    /// letterbox and header follow the theme instead of a fixed black.
    var computerBackground: Color { page }
    var computerForeground: Color { isDefault ? .primary : primaryText }
}

enum ComputerControlState: Equatable {
    case viewOnly
    case starting
    case active
    case stopping
    case failed(String)

    var title: String {
        switch self {
        case .viewOnly: "View only"
        case .starting: "Starting control…"
        case .active: "Control active"
        case .stopping: "Releasing control…"
        case .failed: "Control unavailable"
        }
    }
}

private struct ComputerControlSnapshot: Equatable, Sendable {
    let sessionID: String
    let generation: UInt64
    let geometryRevision: UInt64
    let sourceID: String?
}

/// The frame rate asked of the computer, and what held it below the person's choice.
struct ComputerFrameRateAsk: Equatable {
    var rate: Int
    var limit: ComputerFrameRateLimit?
}

@MainActor
final class ComputerSessionModel: ObservableObject {
    let chat: ChatSummary
    private let connectionModel: ConnectionModel
    /// Cupola's own dictation (the composer's), shared so the microphone here and in chats never record at once.
    var dictationController: DictationController { connectionModel.dictation }
    /// Takes dictation's words and types them into the computer when it finishes.
    private(set) lazy var dictationEditor = ComputerDictationEditor(model: self)
    /// Stops this chat's dictation without typing: the words could not reach the computer
    /// once control is lost, the view is dismissed or the app leaves the foreground.
    func cancelDictation() {
        let controller = dictationController
        if controller.intent?.conversationID == chat.id || controller.preparingConversationID == chat.id {
            controller.cancel()
        }
    }
    private var platformObservation: AnyCancellable?
    /// The paired computer's name, or a generic reference before status arrives.
    var hostReference: String { connectionModel.hostReference }
    var canShareMacClipboard: Bool { connectionModel.isMacHost }
    var platformControls: ComputerPlatformControls { .forHost(platformKey) }
    /// The host platform selecting key tables, including the Diagnostics Windows override.
    var platformKey: String? { Self.resolvedPlatform(connectionModel.hostPlatform) }

    private static func resolvedPlatform(_ reported: String?) -> String? {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-windows-host") { return "windows" }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-linux-host") { return "linux" }
        #endif
        return reported
    }
    var videoQualityUnavailableMessage: String {
        "Video quality is fixed on \(hostShortName)"
    }
    private var hostSubject: String { connectionModel.hostSubject }
    private var hostSentenceStart: String { connectionModel.hostPlatform == nil ? "Your computer" : connectionModel.macName }
    private var hostSubjectStart: String { connectionModel.hostPlatform == nil ? "The computer" : connectionModel.macName }
    var hostHere: String { connectionModel.hostPlatform == nil ? "this computer" : connectionModel.macName }
    var hostShortName: String { connectionModel.hostPlatform == nil ? "Computer" : connectionModel.macName }
    private(set) var clientRequestID = UUID().uuidString.lowercased()
    private(set) var controlRequestID: String?
    @Published private(set) var session: ComputerSession? {
        didSet {
            if let session { lastSession = session }
            if session?.source != oldValue?.source { resetZoom() }
            receiver.frameRateCeiling = session?.videoMaxFramesPerSecond ?? ComputerFrameRate.legacy
        }
    }
    private var lastSession: ComputerSession?
    var displaySession: ComputerSession? { session ?? lastSession }
    @Published private(set) var isLoading = false
    @Published private(set) var dialogGone = false
    /// Another device is viewing this computer; Take over can end its view.
    @Published private(set) var viewerBusy: ComputerViewerConflict?
    /// The other device's view the next opening ends, set only by Take over.
    private var takingOver: ComputerViewerReplacement?
    @Published private(set) var captureRecovery: String?
    private var openingMacDialog = false
    @Published private(set) var isOpeningDialog = false
    @Published private(set) var receiverState: ComputerReceiverState = .preparing
    @Published private(set) var controlState: ComputerControlState = .viewOnly
    @Published private(set) var controlLease: ComputerControlLease?
    @Published private(set) var controlMessage: String?
    @Published private(set) var clipboardMessage: String?
    private var confirmationTask: Task<Void, Never>?
    @Published private(set) var clipboardSharingEnabled: Bool
    private var clipboardSubscriptionID = UUID().uuidString
    private var clipboardCursor: UInt64 = 0
    @Published private(set) var qualityMessage: String?
    /// Hiding the keyboard (or losing its focus) lets go of whatever this device holds down.
    @Published var keyboardPresented = false {
        didSet {
            if oldValue && !keyboardPresented { releaseHeldInput(.keyboardHidden) }
        }
    }
    @Published var failure: String?
    @Published private(set) var zoomScale: CGFloat = 1
    #if DEBUG || WONDER_DIAGNOSTICS
    fileprivate var zoomRequested = false
    #endif
    @Published private(set) var viewportCenter = CGPoint(x: 0.5, y: 0.5)
    /// Under the Duo's side bar, a letterboxed picture sits under the header
    /// rather than in the middle of the spare height. Video, pointer and
    /// gestures all read it through `viewportTransform(in:)`.
    @Published private(set) var anchorsPictureTop = false
    private var viewportSize: CGSize = .zero
    @Published private(set) var keyboardRevision: UInt64 = 0
    /// How smooth the live view is, from the receiver's measurements.
    @Published private(set) var streamQuality: ComputerStreamQuality = .good
    /// The Mac has been slow to accept recent input.
    @Published private(set) var slowInput = false
    /// The shared window is hidden, and whether the computer's typing focus is in a text field.
    @Published private(set) var hostReport = ComputerHostReport()
    private var hostReportRevision: UInt64 = 0
    /// The keyboard is up because the computer's focus entered a text field,
    /// so focus leaving it lowers it again. A keyboard the person opened stays.
    private var keyboardRaisedForTextField = false
    /// The last focus reading acted on during this control; only a change moves the keyboard.
    private var lastTextFocus: Bool?
    /// The app window this view follows instead of the whole screen.
    @Published private(set) var viewTarget: ComputerViewTarget?
    private var inputRoundTrips: [Double] = []
    @Published private var appSwitcher = ComputerAppSwitcher()
    /// A second tap on the switcher key soon after the first keeps ⌘ held
    /// (shown by an underline) until that key is tapped again. Only meaningful
    /// while the switcher is open.
    @Published private var switcherLatchRequested = false
    private var switcherOpenedAt = Date.distantPast
    /// The last tap that belonged to the switcher itself (its key, Tab, the
    /// arrows, Escape), so the any-tap release does not undo it.
    private var switcherClaimedAt = Date.distantPast
    private let touchEnds = ComputerTouchEndObserver()
    private static let switcherDoubleTapWindow: TimeInterval = 0.4
    @Published private(set) var modifierLatch = ComputerModifierLatch()
    /// The app launcher sheet (the Apps key on a Mac).
    @Published var appLauncherPresented = false
    @Published private(set) var miniDock = ComputerMiniDockContent.unavailable
    /// Apps opened in a Diagnostics fixture, which has no computer to open them on.
    private(set) var openedAppIDs: [String] = []
    private let appIcons: NSCache<NSString, UIImage> = {
        let cache = NSCache<NSString, UIImage>()
        cache.countLimit = 160
        return cache
    }()
    var isAppSwitcherPresented: Bool { appSwitcher.isPresented }
    #if WONDER_DIAGNOSTICS
    var viewportSizeForState: CGSize { viewportSize }
    #endif
    var isSwitcherLatched: Bool { appSwitcher.isPresented && switcherLatchRequested }
    @Published var inputMode: ComputerInputMode {
        didSet {
            guard inputMode != oldValue else { return }
            finishPointerDrag()
            inputPreferences.set(inputMode.rawValue, forKey: "computer.inputMode")
        }
    }
    /// A separate trackpad under the picture (and above the keyboard), in any
    /// layout. The half-folded Duo always has one below the fold.
    @Published private(set) var showsTrackpad: Bool
    /// Raise the system keyboard as soon as control starts (Diagnostics fixtures).
    private var keyboardAtControl = false
    /// The hidden text view takes focus for the system keyboard (and a hardware
    /// keyboard); its shortcut row rides on top of it.
    var presentsSystemKeyboard: Bool { isControlActive && keyboardPresented }
    #if WONDER_DIAGNOSTICS
    /// Keys whose last sent action was a down: what the computer still holds.
    var fixtureHeldKeys: [String] {
        var held: [String] = []
        for case let .key(key, phase, _) in fixtureSentInput {
            if phase == "down" { held.append(key) } else if phase == "up" { held.removeAll { $0 == key } }
        }
        return held
    }

    /// The text typed into the computer so far, in order.
    var fixtureTypedText: String {
        fixtureSentInput.compactMap { if case let .text(text) = $0 { return text } else { return nil } }.joined()
    }

    /// The modifiers each key press carried (⌘ is 8): a latched ⌘ must ride along.
    var fixturePressModifiers: [UInt32] {
        fixtureSentInput.compactMap { action in
            if case let .key(_, "press", modifiers) = action { return modifiers }
            return nil
        }
    }

    var fixtureClipboardOperations: [String] {
        fixtureSentInput.compactMap { action in
            if case .clipboard(let operation, _) = action { return operation }
            return nil
        }
    }
    var savedBottomInput: String { inputPreferences.string(forKey: bottomInputKey) ?? "off" }
    #endif
    private var bottomInputKey: String {
        "computer.bottomInput." + (UIDevice.current.userInterfaceIdiom == .pad ? "pad" : "phone")
    }
    private func saveBottomInput(_ choice: ComputerBottomInput) { inputPreferences.set(choice.rawValue, forKey: bottomInputKey) }
    /// Brings back the keyboard chosen last time on this kind of device.
    func restoreBottomInput() {
        guard isControlActive, !keyboardPresented,
              keyboardAtControl || (!isFixture && inputPreferences.string(forKey: bottomInputKey) == ComputerBottomInput.keyboard.rawValue)
        else { return }
        keyboardPresented = true
    }
    /// Whether the window is tall enough to hold a trackpad under the picture
    /// (a short companion pane is not); set by the viewer's layout.
    @Published private(set) var trackpadRoom = false
    /// The trackpad button stays offered while the keyboard is up, though the
    /// keyboard takes the height the room was measured from.
    @Published private(set) var trackpadOffered = false
    func setTrackpadRoom(_ room: Bool) {
        if trackpadRoom != room { trackpadRoom = room }
        if !keyboardPresented, trackpadOffered != room { trackpadOffered = room }
    }
    /// What a touch on the picture does. With the trackpad shown the picture
    /// is not a second, absolute surface: the pointer moves only by the pad or by dragging.
    var pictureInputMode: ComputerInputMode { showsTrackpad || padColumn ? .trackpad : inputMode }
    /// The iPad landscape pose keeps a trackpad column beside the picture.
    @Published private(set) var padColumn = false
    func setPadColumn(_ on: Bool) { if padColumn != on { padColumn = on } }
    @Published private(set) var videoQuality: ComputerVideoQuality
    /// What the person chose for the frame rate; Auto follows `frameRateGovernor`.
    @Published private(set) var frameRateChoice: ComputerFrameRateChoice
    /// The frame rate asked of the computer, and what held it below the choice.
    @Published private(set) var frameRateRequest = ComputerFrameRateAsk(rate: ComputerFrameRate.legacy, limit: nil)
    /// A short line after a pick that a limit cut short, or a change that failed; clears itself.
    @Published private(set) var frameRateNote: String?
    private var frameRateGovernor = ComputerFrameRateGovernor(rate: ComputerFrameRate.legacy)
    private var frameRateSync: Task<Void, Never>?
    private var frameRateNoteTask: Task<Void, Never>?
    private var frameRateRetryAfter: TimeInterval = 0
    private var powerObservation: AnyCancellable?
    private let inputPreferences: UserDefaults
    private var requestedSource: ComputerSource?
    let pointerState = ComputerPointerState()
    private var pointer: CGPoint {
        get { pointerState.position }
        set { pointerState.move(to: newValue) }
    }
    let receiver: ComputerReceiver
    private let snapshotScope: UUID
    private var recovery = ComputerReconnect()
    private var recoveryTask: Task<Void, Never>?
    @Published private var connectionUnavailable = false
    private var snapshotKey: String {
        chat.id + ":" + (viewTarget?.source.id ?? "screen")
    }
    @Published private var ended = false
    var isClosed: Bool { ended }
    /// Cupola is in the background: sharing stopped, the view and its window stay.
    @Published private(set) var isPaused = false
    /// The paused session the computer confirmed it ended; reopening need not end it again.
    private var confirmedEndedSessionID: String?
    /// A view this model stopped using that the computer has not confirmed it
    /// ended. Opening asks the computer to end it first; if that can't be
    /// confirmed, the computer replaces this device's own view as the new one opens.
    private var unconfirmedEnd: ComputerSession?
    /// Openings whose reply has not arrived yet (or whose late view is still
    /// being ended); a new opening waits for them.
    private var openingsInFlight = 0
    private var openingWaiters: [CheckedContinuation<Void, Never>] = []
    /// Control ended because Cupola left the foreground; resuming says so.
    private var pausedWithControl = false
    /// Pausing's release and session teardown; resuming waits for it, so the
    /// computer has let go before a new view opens or control is taken again.
    private var pauseTeardown: Task<Void, Never>?
    /// Cleared when Cupola leaves again while a resume waits for the teardown.
    private var resumeWanted = false
    private var lifetimeRevision: UInt64 = 0
    private var receiverRevision: UInt64 = 0
    private var controlTask: Task<Void, Never>?
    private var controlSnapshot: ComputerControlSnapshot?
    private var heartbeatTask: Task<Void, Never>?
    private var inputTask: Task<Void, Never>?
    private var applicationsTask: Task<Void, Never>?
    private var inputBuffer = ComputerInputBuffer()
    private var inputEpoch: UInt64 = 0
    private var inputIntentRevision: UInt64 = 0
    /// The pointer button held for a drag and the hardware modifiers it carries.
    private var pointerPress = ComputerPointerPress()
    private var scrollAccumulator = ComputerScrollAccumulator()
    private var releasingControl = false
    private var qualityUpdateInFlight = false
    private var controlRecoveryRevision: UInt64 = 0
    #if DEBUG || WONDER_DIAGNOSTICS
    private var diagnosticSession: ComputerSession?
    #if WONDER_DIAGNOSTICS
    private var fixtureRecoveryTask: Task<Void, Never>?
    private var fixtureOpenCount = 0
    private var fixtureReadyProcessed = false
    private var fixtureControlLossArmed = false
    /// How often held input was let go, for scenarios.
    private(set) var heldInputReleases = 0
    /// The deepest the unsent input queue got, for the key-hold checks.
    var inputQueueMaximumDepth: Int { inputBuffer.maximumDepth }
    #endif
    /// Input the fixture accepted, in order, standing in for the computer.
    @Published private(set) var fixtureSentInput: [ComputerInputAction] = []

    convenience init(model: ConnectionModel, chat: ChatSummary, inputPreferences: UserDefaults,
                     fixtureSession: ComputerSession) {
        self.init(model: model, chat: chat, inputPreferences: inputPreferences)
        diagnosticSession = fixtureSession
    }
    #endif

    init(model: ConnectionModel, chat: ChatSummary, inputPreferences: UserDefaults = .standard,
         viewTarget: ComputerViewTarget? = nil) {
        let snapshot = model.computerPictures.take(chat.id + ":" + (viewTarget?.source.id ?? "screen"))
        receiver = ComputerReceiver(picture: snapshot?.picture ?? ComputerPicture())
        snapshotScope = model.imagePreviewScope
        lastSession = snapshot?.session
        self.inputPreferences = inputPreferences
        self.clipboardSharingEnabled = inputPreferences.bool(forKey: "computer.shareClipboard")
        self.viewTarget = viewTarget
        self.requestedSource = viewTarget.map(Self.source(for:))
        self.inputMode = inputPreferences.string(forKey: "computer.inputMode").flatMap(ComputerInputMode.init(rawValue:)) ?? .trackpad
        let bottomKey = "computer.bottomInput." + (UIDevice.current.userInterfaceIdiom == .pad ? "pad" : "phone")
        if let saved = inputPreferences.string(forKey: bottomKey) {
            self.showsTrackpad = saved == ComputerBottomInput.trackpad.rawValue
        } else {
            // The old on/off preference becomes the first saved choice.
            let legacy = inputPreferences.bool(forKey: "computer.showsTrackpad")
            self.showsTrackpad = legacy
            inputPreferences.set((legacy ? ComputerBottomInput.trackpad : .off).rawValue, forKey: bottomKey)
        }
        self.videoQuality = inputPreferences.string(forKey: "computer.videoQuality").flatMap(ComputerVideoQuality.init(rawValue:)) ?? .auto
        self.frameRateChoice = ComputerFrameRateChoice(storageValue: inputPreferences.string(forKey: "computer.frameRate")) ?? .auto
        // Fill screen (a saved zoom that followed every resize) is gone: the
        // picture always opens fitted and zooms only on a pinch or the ⋯ menu.
        inputPreferences.removeObject(forKey: "computer.fillsViewport")
        self.connectionModel = model
        #if DEBUG || WONDER_DIAGNOSTICS
        Self.modelsMade += 1
        #endif
        self.chat = chat
        platformObservation = model.$hostPlatform.removeDuplicates().sink { [weak self] _ in
            // The shortcut tables follow the computer's platform.
            self?.objectWillChange.send()
        }
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-clipboard-off") {
            clipboardSharingEnabled = false
            inputPreferences.set(false, forKey: "computer.shareClipboard")
        }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-session-fixture")
            || ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-shell-fixture") {
            inputMode = .trackpad
            // The panel is a launch choice in fixtures, not a saved preference.
            showsTrackpad = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-trackpad")
            keyboardAtControl = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-keyboard")
            videoQuality = .auto
        }
        #endif
        receiver.onStateChange = { [weak self] state, revision in
            Task { @MainActor [weak self] in
                guard let self else { return }
                await receiveReceiverState(state, revision: revision, mayResumeAutomatically: receiver.canResumeAutomatically)
            }
        }
        frameRateGovernor.restart(at: frameRatePolicy.startingRate)
        updateFrameRate()
        receiver.onQualityChange = { [weak self] quality in
            Task { @MainActor [weak self] in
                guard let self, !self.ended else { return }
                self.streamQuality = quality
            }
        }
        receiver.onNetworkSample = { [weak self] sample in
            Task { @MainActor [weak self] in
                guard let self, !self.ended else { return }
                if self.frameRateChoice == .auto { self.frameRateGovernor.observe(sample, ceiling: self.frameRatePolicy.ceiling) }
                self.updateFrameRate()
                self.syncFrameRate()
            }
        }
        powerObservation = Publishers.Merge(
            NotificationCenter.default.publisher(for: .NSProcessInfoPowerStateDidChange),
            NotificationCenter.default.publisher(for: ProcessInfo.thermalStateDidChangeNotification)
        ).receive(on: DispatchQueue.main).sink { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, !self.ended else { return }
                let before = self.frameRateRequest.limit
                self.updateFrameRate()
                if let limit = self.frameRateRequest.limit, limit != before { self.announceFrameRateLimit() }
                self.syncFrameRate()
            }
        }
        #if WONDER_DIAGNOSTICS
        receiver.onStreamMetrics = { metrics in
            DiagnosticJournal.shared.record(DiagnosticEvent(operation: "computer.stream", phase: "interval", metrics: metrics))
        }
        #endif
        receiver.onHostReportChange = { [weak self] report, revision in
            Task { @MainActor [weak self] in
                guard let self, !self.ended, revision > self.hostReportRevision else { return }
                self.hostReportRevision = revision
                self.receiveHostReport(report)
            }
        }
    }

    func receiveReceiverState(_ state: ComputerReceiverState, revision: UInt64, mayResumeAutomatically: Bool) async {
        guard revision > receiverRevision else { return }
        receiverRevision = revision
        guard !ended else { return }
        guard !isPaused else { return }
        receiverState = state
        if case .failed(let message) = state { failure = message }
        if state == .live {
            captureRecovery = nil
            failure = nil
            recovery.receive(.live)
            recoveryTask?.cancel(); recoveryTask = nil
        }
        if case .retrying = state {
            controlRecoveryRevision &+= 1
            recovery.receive(.interrupted)
        }
        if case .failed = state { controlRecoveryRevision &+= 1 }
        if state != .live, self.controlLease != nil || self.isStarting {
            // Input can't reach the computer while the picture is paused.
            if case .retrying = state {
                failClosed("The connection was interrupted, so control was released. Take control again once the picture is back.")
            } else {
                failClosed("The computer view is no longer live. Control was released.")
            }
        }
        if state == .awaitingSource || state == .live { await refresh() }
        // A newer state may have arrived while refreshing; it owns what happens next.
        guard receiverRevision == revision else { return }
        if case .failed = state, mayResumeAutomatically { scheduleRecovery() }
    }

    func receiveHostReport(_ report: ComputerHostReport) {
        guard !ended else { return }
        if hostReport != report { hostReport = report }
        guard let focused = report.textInputFocused, focused != lastTextFocus,
              isControlActive else { return }
        lastTextFocus = focused
        if focused {
            guard !keyboardPresented else { return }
            keyboardPresented = true
            keyboardRaisedForTextField = true
        } else if keyboardRaisedForTextField {
            keyboardRaisedForTextField = false
            keyboardPresented = false
        }
    }

    private static func source(for target: ComputerViewTarget) -> ComputerSource {
        target.source
    }

    /// Short live status for the header, in plain words.
    var liveStatus: String {
        if isPaused { return "Paused" }
        if failure != nil { return "Couldn’t connect" }
        if connectionUnavailable { return "Reconnecting" }
        return ComputerViewNotice.liveStatus(receiverState: receiverState, quality: streamQuality, slowInput: slowInput)
    }

    /// The single message over the picture; nil while it is live.
    var notice: ComputerViewNotice? {
        if isPaused { return ComputerViewNotice(message: "Computer view paused. Return to Cupola to reconnect.", canRetry: false) }
        if connectionUnavailable, failure == nil {
            return ComputerViewNotice(message: ComputerReceiverState.interruptedMessage, canRetry: true)
        }
        if !ended, let captureRecovery { return ComputerViewNotice(message: captureRecovery, canRetry: true) }
        return ComputerViewNotice.current(failure: failure, session: session, receiverState: receiverState,
                                   closed: ended, dialogGone: dialogGone, hostHere: hostHere)
    }

    /// The viewer's explicit shortcut asks the host for a current exact dialog.
    /// Release any control before replacing the display; viewing answers nothing.
    func showMacDialog() async {
        guard !ended, !isLoading, !isOpeningDialog else { return }
        isOpeningDialog = true
        defer { isOpeningDialog = false }
        openingMacDialog = true
        dialogGone = false
        viewTarget = nil
        await retry()
    }

    var isFixture: Bool {
        #if WONDER_DIAGNOSTICS
        return diagnosticSession != nil || ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-session-fixture")
            || ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-shell-fixture")
            || connectionModel.previewMode
        #else
        return false
        #endif
    }

    func start() async {
        // A Take over applies to this opening only, never to a later one.
        let replacing = takingOver
        takingOver = nil
        guard session == nil, !ended, !isLoading, !dialogGone else { return }
        captureRecovery = nil
        let revision = lifetimeRevision
        let requestID = clientRequestID
        // Fixture sessions exist only in Diagnostics and Debug builds.
        #if DEBUG || WONDER_DIAGNOSTICS
        if isFixture || connectionModel.previewMode {
            if openingMacDialog {
                #if WONDER_DIAGNOSTICS
                if DiagnosticSubagentFixture.signalled("mac-dialog-gone") {
                    dialogGone = true
                    receiverState = .closed
                    return
                }
                #endif
                viewTarget = .window(ComputerAgentWindowTarget(app: "UserNotificationCenter", windowID: 31_507, windowTitle: "Mac dialog")!)
            }
            let available = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-session-available-fixture")
            #if WONDER_DIAGNOSTICS
            if isSimulator, !(await openSimulatorFixture(revision: revision)) { return }
            // The first open fails the way a computer reports it; Try again then opens.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-view-failed-once"), !Self.fixtureFailedOnce {
                Self.fixtureFailedOnce = true
                failure = "Your computer interrupted screen sharing. Try again."
                receiverState = .failed(failure!)
                return
            }
            // Another device is viewing this computer until Take over ends its view.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-viewer-busy"), replacing == nil {
                showViewerBusy(ComputerViewerConflict(code: .viewerBusy, deviceName: "iPad",
                                                      sessionId: "fixture-other-view", generation: 1))
                return
            }
            #endif
            let permissionRequired = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-menu-permission")
            if permissionRequired {
                captureRecovery = ComputerAvailableSources(available: false, sources: [],
                    capture: .init(available: false, reason: "screen_recording_permission_required")).captureRecovery
            }
            let fixture = diagnosticSession ?? Self.fixture(conversationID: chat.id, available: available && !permissionRequired, window: viewTarget)
            session = fixture
            receiverState = fixture.state == .live ? .live : .preparing
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-picture") {
                receiver.picture.renderFrame(DiagnosticComputerRecovery.frame())
            }
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-reconnect") {
                fixtureOpenCount += 1
                if fixtureOpenCount > 1 || DiagnosticSubagentFixture.signalled("computer-chat-switch") {
                    receiverState = .retrying("Reconnecting to your computer…")
                    isLoading = true
                    while !(DiagnosticSubagentFixture.signalled("computer-recovery-ready")
                            && (!DiagnosticSubagentFixture.signalled("computer-network-change") || fixtureReadyProcessed))
                            && !DiagnosticSubagentFixture.signalled("computer-retry-ready") {
                        guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
                        if DiagnosticSubagentFixture.signalled("computer-recovery-failed") {
                            isLoading = false
                            failure = "The computer could not be reached. Check that Cupola is running, then try again."
                            receiverState = .failed(failure!)
                            return
                        }
                        try? await Task.sleep(for: .milliseconds(50))
                    }
                    guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
                    isLoading = false
                    receiverState = .live
                }
                if !receiver.picture.hasFrame { receiver.picture.renderFrame(DiagnosticComputerRecovery.frame()) }
                installRecoveryFixture()
            }
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-window-hidden") {
                receiveHostReport(ComputerHostReport(windowHidden: true))
            }
            // Control starts as soon as the view is live, for displays whose
            // taps a scenario cannot make (the Duo, #387).
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-control-active"), fixtureOpenCount <= 1 { takeControl() }
            #endif
            if receiverState == .live { recovery.receive(.live) }
            return
        }
        #endif
        guard let connection = connectionModel.connection, !connectionModel.accessEnded else {
            failure = "Connect this device to \(hostReference) before opening computer view."
            return
        }
        ComputerOpenTimeline.shared.begin()
        isLoading = true
        defer { if lifetimeRevision == revision { isLoading = false } }
        // An earlier opening still on its way settles first, so this device
        // never races itself for the computer's one view.
        while openingsInFlight > 0 { await withCheckedContinuation { openingWaiters.append($0) } }
        guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
        // A view left behind (after leaving Cupola, or a close that never
        // arrived) is ended first. If the computer can't confirm it, opening
        // goes ahead: the computer replaces this device's own view.
        let stale = unconfirmedEnd
        if let stale {
            if (try? await endOnHost(stale)) != nil, unconfirmedEnd?.id == stale.id { unconfirmedEnd = nil }
            guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
        }
        openingsInFlight += 1
        defer {
            openingsInFlight -= 1
            if openingsInFlight == 0 {
                let waiters = openingWaiters
                openingWaiters.removeAll()
                waiters.forEach { $0.resume() }
            }
        }
        do {
            let body = try JSONEncoder().encode(StartComputerSessionRequest(
                clientRequestId: requestID,
                conversationId: chat.id,
                hostInstallationId: connection.credential.hostInstallationId,
                source: openingMacDialog ? ComputerSource(id: "mac-dialog", name: "Mac dialog", kind: "window") : requestedSource,
                videoQuality: videoQuality,
                videoMaxFramesPerSecond: frameRateForNewView(),
                takingOver: replacing
            ))
            let created: ComputerSession = try await connectionModel.manage(
                "/api/v1/computer/sessions",
                method: "POST",
                body: body
            )
            guard created.conversationId == chat.id,
                  created.ownerDeviceId == connection.credential.deviceId,
                  created.hostInstallationId == connection.credential.hostInstallationId else {
                throw PairingFailure.wrongHost
            }
            guard !Task.isCancelled, !ended, lifetimeRevision == revision, clientRequestID == requestID,
                  connectionModel.connection?.credential.hostInstallationId == connection.credential.hostInstallationId,
                  connectionModel.connection?.credential.deviceId == connection.credential.deviceId else {
                // End only the late session on the exact Mac that created it.
                // Never apply it to a new connection or restart a closed receiver.
                let late: ComputerEmptyReply? = try? await connectionModel.api.request(
                    "/api/v1/computer/sessions/\(Self.escape(created.id))?generation=\(created.generation)",
                    origin: connection.origin, credential: connection.credential, method: "DELETE"
                )
                // Still the same computer: the next opening ends it first.
                if late == nil, !ended,
                   connectionModel.connection?.credential.hostInstallationId == connection.credential.hostInstallationId,
                   connectionModel.connection?.credential.deviceId == connection.credential.deviceId {
                    unconfirmedEnd = created
                }
                return
            }
            if openingMacDialog && !Self.isExactDialogSource(created.source) {
                let _: ComputerEmptyReply? = try? await connectionModel.api.request(
                    "/api/v1/computer/sessions/\(Self.escape(created.id))?generation=\(created.generation)",
                    origin: connection.origin, credential: connection.credential, method: "DELETE"
                )
                failure = "This dialog's window couldn't be opened. Open it on your computer."
                receiverState = .failed(failure!)
                return
            }
            if openingMacDialog, let target = Self.dialogWindowTarget(created.source) {
                viewTarget = .window(target)
            }
            // The computer ended the view this one replaced.
            if let stale, unconfirmedEnd?.id == stale.id { unconfirmedEnd = nil }
            ComputerOpenTimeline.shared.mark("session-created")
            session = created
            failure = nil
            viewerBusy = nil
            receiverState = .preparing
            startReceiver(for: created, connection: connection)
            await refreshCaptureRecovery(for: created, revision: revision)
        } catch {
            guard !ended, lifetimeRevision == revision, !Task.isCancelled,
                  connectionModel.connection?.credential.hostInstallationId == connection.credential.hostInstallationId,
                  connectionModel.connection?.credential.deviceId == connection.credential.deviceId else { return }
            if openingMacDialog, case PairingFailure.response(410) = error {
                dialogGone = true
                failure = nil
                receiverState = .closed
                return
            }
            if let conflict = error as? ComputerViewerConflict, conflict.code == .viewerBusy {
                showViewerBusy(conflict)
                return
            }
            if openingMacDialog, case PairingFailure.response(503) = error {
                failure = "The dialog couldn’t be brought forward. Run cupola permissions grant on your Mac to allow Accessibility, then try again."
            } else {
                failure = Self.readable(error, host: hostReference, here: hostHere, opening: true)
            }
            receiverState = .failed(failure ?? "Could not open computer view.")
        }
    }

    private func showViewerBusy(_ conflict: ComputerViewerConflict) {
        viewerBusy = conflict
        failure = conflict.errorDescription
        receiverState = .failed(failure ?? "Could not open computer view.")
    }

    /// Ends the other device's view (and any control it had) and opens this
    /// one. The computer refuses it if that view changed in the meantime.
    func takeOver() async {
        guard let busy = viewerBusy, !ended, !isLoading else { return }
        await retry(takingOver: busy.replacement)
    }

    static func isExactDialogSource(_ source: ComputerSource) -> Bool {
        guard source.kind == "window", let id = source.id else { return false }
        if dialogWindowTarget(source) != nil { return true }
        guard id.hasPrefix("window:"), let number = UInt32(id.dropFirst("window:".count)) else { return false }
        return number > 0
    }

    static func dialogWindowTarget(_ source: ComputerSource) -> ComputerAgentWindowTarget? {
        guard source.kind == "window", let id = source.id, id.hasPrefix("app:") else { return nil }
        let parts = id.dropFirst(4).split(separator: "#", omittingEmptySubsequences: false)
        guard parts.count == 2, let number = UInt32(parts[1]) else { return nil }
        return ComputerAgentWindowTarget(app: String(parts[0]), windowID: number, windowTitle: source.name)
    }

    func refresh() async {
        guard let current = session, !ended, !isPaused, !isFixture, !connectionModel.previewMode,
              !qualityUpdateInFlight else { return }
        let revision = lifetimeRevision
        isLoading = true
        defer { if lifetimeRevision == revision { isLoading = false } }
        do {
            let updated: ComputerSession = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(current.id))?generation=\(current.generation)"
            )
            guard !Task.isCancelled, !ended, lifetimeRevision == revision,
                  session?.id == current.id, session?.generation == current.generation else { return }
            guard updated.id == current.id,
                  updated.conversationId == chat.id,
                  updated.ownerDeviceId == current.ownerDeviceId,
                  updated.hostInstallationId == current.hostInstallationId else { throw PairingFailure.wrongHost }
            if let snapshot = controlSnapshot, !Self.snapshotMatches(snapshot, session: updated) {
                failClosed("The computer view changed. Control was released.")
            } else if let lease = controlLease, !Self.bindingMatches(lease, session: updated) {
                failClosed("The computer view changed. Control was released.")
            }
            if (controlLease != nil || isStarting)
                && (updated.state != .live || updated.control?.available != true) {
                failClosed("The computer view is no longer available for control.")
            }
            session = updated
            if case .failed = receiverState {
                // Keep a receiver failure visible when this session refresh
                // completes after the WebRTC path has already failed.
            } else {
                failure = nil
            }
            if updated.generation != current.generation { startReceiver(for: updated, connection: connectionModel.connection) }
            if updated.state != current.state {
                await refreshCaptureRecovery(for: updated, revision: revision)
            }
        } catch {
            guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
            if controlLease != nil { failClosed("The computer view could not be verified. Control was released.") }
            // A working picture stays up; the stream reports its own failure.
            if receiverState != .live { failure = Self.readable(error, host: hostReference, here: hostHere) }
        }
    }

    /// Availability can settle after opening. Ask once when it becomes unavailable;
    /// do not keep a window inventory alive for the viewer.
    private func refreshCaptureRecovery(for current: ComputerSession, revision: UInt64) async {
        guard current.state == .unavailable else { captureRecovery = nil; return }
        let sources: ComputerAvailableSources? = try? await connectionModel.manage("/api/v1/computer/sources/windows")
        guard !Task.isCancelled, !ended, lifetimeRevision == revision,
              session?.id == current.id, session?.generation == current.generation, session?.state == .unavailable,
              connectionModel.connection?.credential.hostInstallationId == current.hostInstallationId,
              connectionModel.connection?.credential.deviceId == current.ownerDeviceId else { return }
        captureRecovery = sources?.captureRecovery
    }

    func retry(preservingSource: Bool = true, takingOver replacement: ComputerViewerReplacement? = nil,
               automatic: Bool = false) async {
        guard !ended, !isPaused, !connectionModel.accessEnded else { return }
        if !automatic {
            recoveryTask?.cancel(); recoveryTask = nil
            recovery.receive(.tryAgain)
        }
        connectionUnavailable = false
        // The Mac resolves the source's current geometry after reconnecting.
        // Before the first session arrives, the window asked for is still the one to show.
        requestedSource = preservingSource ? (session?.source ?? requestedSource)
            : viewTarget.map(Self.source(for:))
        lifetimeRevision &+= 1
        let revision = lifetimeRevision
        await releaseControl()
        receiver.close(reason: "retry", keepingLastFrame: true)
        let alreadyEnded = confirmedEndedSessionID
        confirmedEndedSessionID = nil
        // Opening ends the old view first and says so if the computer can't be reached.
        if let current = session, current.id != alreadyEnded, !isFixture, !connectionModel.previewMode {
            unconfirmedEnd = current
        }
        guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
        isLoading = false
        session = nil
        receiverState = .preparing
        failure = nil
        viewerBusy = nil
        controlMessage = nil
        clipboardMessage = nil
        controlRequestID = nil
        controlSnapshot = nil
        streamQuality = .good
        slowInput = false
        hostReport = ComputerHostReport()
        inputRoundTrips.removeAll()
        ended = false
        clientRequestID = UUID().uuidString.lowercased()
        takingOver = replacement
        await start()
        if automatic, failure != nil, viewerBusy == nil { scheduleRecovery() }
    }

    private func scheduleRecovery() {
        guard !ended, !isPaused, !connectionModel.accessEnded, receiver.picture.hasFrame,
              viewerBusy == nil, !dialogGone, recoveryTask == nil, recovery.receive(.failed) else { return }
        let revision = lifetimeRevision
        failure = nil
        receiverState = .retrying("Reconnecting to your computer…")
        recoveryTask = Task { @MainActor [weak self] in
            do { try await Task.sleep(for: .milliseconds(750)) } catch { return }
            guard let self, !self.ended, !self.isPaused, !self.connectionModel.accessEnded,
                  self.lifetimeRevision == revision else { return }
            self.recoveryTask = nil
            await self.retry(preservingSource: true, automatic: true)
        }
    }

    // MARK: Frame rate

    /// The display, power state and computer the frame rate is chosen against.
    var frameRatePolicy: ComputerFrameRatePolicy {
        var policy = ComputerFrameRatePolicy(
            displayMax: UIApplication.shared.connectedScenes.lazy.compactMap { ($0 as? UIWindowScene)?.screen.maximumFramesPerSecond }.first ?? 60,
            isPhone: UIDevice.current.userInterfaceIdiom != .pad,
            lowPower: ProcessInfo.processInfo.isLowPowerModeEnabled,
            thermalState: ProcessInfo.processInfo.thermalState,
            computerCap: ComputerFrameRatePolicy.computerCap(platform: platformKey, quality: videoQuality))
        #if WONDER_DIAGNOSTICS
        let arguments = ProcessInfo.processInfo.arguments
        func value(after flag: String) -> String? {
            arguments.firstIndex(of: flag).flatMap { $0 + 1 < arguments.count ? arguments[$0 + 1] : nil }
        }
        if let hertz = value(after: "-diagnostics-computer-display-hz").flatMap(Int.init) { policy.displayMax = hertz }
        if arguments.contains("-diagnostics-computer-low-power") { policy.lowPower = true }
        switch value(after: "-diagnostics-computer-thermal") {
        case "serious": policy.thermalState = .serious
        case "critical": policy.thermalState = .critical
        default: break
        }
        #endif
        return policy
    }

    /// Choices this display can show; the shown choice falls back to Auto if a saved one is above it.
    var frameRateChoices: [ComputerFrameRateChoice] { frameRatePolicy.choices }
    var shownFrameRateChoice: ComputerFrameRateChoice { frameRateChoices.contains(frameRateChoice) ? frameRateChoice : .auto }
    /// The computer takes a frame-rate ceiling; older ones always publish at 15.
    var supportsFrameRateChoice: Bool { session?.videoMaxFramesPerSecond != nil }

    /// The menu row: the choice, or the rate it is held at and what holds it.
    var frameRateRowTitle: String {
        guard let limit = frameRateRequest.limit else { return "Frame rate: \(shownFrameRateChoice.title)" }
        let reason = switch limit {
        case .lowPower: "Low Power"
        case .thermal: "Warm"
        case .computer: "Computer limit"
        }
        return "Frame rate: \(frameRateRequest.rate) fps · \(reason)"
    }

    var frameRateLimitNote: String? {
        frameRateRequest.limit.map { limit in
            ComputerFrameRatePolicy.note(for: limit, rate: frameRateRequest.rate,
                device: UIDevice.current.userInterfaceIdiom == .pad ? "iPad" : "iPhone", computer: hostShortName)
        }
    }

    /// The ceiling to open a view at; Auto starts at the class default again.
    private func frameRateForNewView() -> Int {
        frameRateGovernor.restart(at: frameRatePolicy.startingRate)
        updateFrameRate()
        return frameRateRequest.rate
    }

    private func updateFrameRate() {
        let policy = frameRatePolicy
        let result = policy.request(for: frameRateChoice, automaticRate: frameRateGovernor.rate)
        let request = ComputerFrameRateAsk(rate: result.rate, limit: result.limit)
        if request != frameRateRequest { frameRateRequest = request }
    }

    func setFrameRate(_ choice: ComputerFrameRateChoice) {
        guard choice != frameRateChoice else { return }
        frameRateChoice = choice
        if !isFixture { inputPreferences.set(choice.storageValue, forKey: "computer.frameRate") }
        if choice == .auto { frameRateGovernor.restart(at: frameRatePolicy.startingRate) }
        updateFrameRate()
        frameRateRetryAfter = 0
        announceFrameRateLimit()
        syncFrameRate()
    }

    private func announceFrameRateLimit() {
        guard let note = frameRateLimitNote, frameRateChoice != .auto else { return }
        showFrameRateNote(note)
    }

    private func showFrameRateNote(_ note: String) {
        frameRateNote = note
        frameRateNoteTask?.cancel()
        frameRateNoteTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            self?.frameRateNote = nil
        }
    }

    /// Tells the computer the current frame-rate ceiling when it differs from the one it publishes at.
    private func syncFrameRate() {
        guard frameRateSync == nil, !ended, !isFixture, session?.supportsLiveQualityChange == true,
              let applied = session?.videoMaxFramesPerSecond, applied != frameRateRequest.rate,
              Date.timeIntervalSinceReferenceDate >= frameRateRetryAfter else { return }
        frameRateSync = Task { [weak self] in
            await self?.sendFrameRate()
            self?.frameRateSync = nil
        }
    }

    private func sendFrameRate() async {
        let revision = lifetimeRevision
        qualityUpdateInFlight = true
        defer { qualityUpdateInFlight = false }
        while let current = session, !ended, !Task.isCancelled, lifetimeRevision == revision, !isLoading,
              let applied = current.videoMaxFramesPerSecond, applied != frameRateRequest.rate {
            let wanted = frameRateRequest.rate
            do {
                let body = try JSONEncoder().encode(ComputerSessionQualityRequest(
                    generation: current.generation, videoQuality: current.videoQuality ?? videoQuality,
                    videoMaxFramesPerSecond: wanted))
                let updated: ComputerSession = try await connectionModel.manage(
                    "/api/v1/computer/sessions/\(Self.escape(current.id))/quality", method: "POST", body: body)
                guard !Task.isCancelled, !ended, lifetimeRevision == revision,
                      session?.id == current.id, session?.generation == current.generation else { return }
                guard updated.id == current.id, updated.ownerDeviceId == current.ownerDeviceId,
                      updated.hostInstallationId == current.hostInstallationId,
                      updated.videoMaxFramesPerSecond == wanted else { throw PairingFailure.wrongHost }
                session = updated
            } catch {
                guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
                // Auto tries again in a few seconds; a person's pick says so.
                frameRateRetryAfter = Date.timeIntervalSinceReferenceDate + 10
                if frameRateChoice != .auto { showFrameRateNote("Could not change the frame rate. Choose it again to retry.") }
                return
            }
        }
    }

    func setVideoQuality(_ quality: ComputerVideoQuality) {
        guard quality != videoQuality, session?.videoQuality != nil, !isLoading else { return }
        let previous = videoQuality
        videoQuality = quality
        updateFrameRate()
        qualityMessage = nil
        if !isFixture { inputPreferences.set(quality.rawValue, forKey: "computer.videoQuality") }
        if session?.supportsLiveQualityChange == true {
            Task {
                await frameRateSync?.value
                await changeVideoQuality(quality, previous: previous)
            }
        } else {
            Task { await retry(preservingSource: true) }
        }
    }

    private func changeVideoQuality(_ quality: ComputerVideoQuality, previous: ComputerVideoQuality) async {
        guard let current = session, !ended else { return }
        if isFixture { return }
        let revision = lifetimeRevision
        qualityUpdateInFlight = true
        isLoading = true
        defer {
            qualityUpdateInFlight = false
            if lifetimeRevision == revision { isLoading = false }
        }
        do {
            let body = try JSONEncoder().encode(ComputerSessionQualityRequest(
                generation: current.generation, videoQuality: quality,
                videoMaxFramesPerSecond: current.videoMaxFramesPerSecond == nil ? nil : frameRateRequest.rate
            ))
            let updated: ComputerSession = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(current.id))/quality", method: "POST", body: body
            )
            guard !Task.isCancelled, !ended, lifetimeRevision == revision,
                  session?.id == current.id, session?.generation == current.generation else { return }
            guard updated.id == current.id, updated.ownerDeviceId == current.ownerDeviceId,
                  updated.hostInstallationId == current.hostInstallationId,
                  updated.videoQuality == quality else { throw PairingFailure.wrongHost }
            session = updated
            qualityMessage = nil
        } catch {
            guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return }
            videoQuality = previous
            inputPreferences.set(previous.rawValue, forKey: "computer.videoQuality")
            qualityMessage = "Could not change video quality. Choose it again to retry."
        }
    }

    func connectionInterrupted() {
        guard !ended, !isPaused else { return }
        controlRecoveryRevision &+= 1
        connectionUnavailable = true
        recovery.receive(.interrupted)
        if controlLease != nil || isStarting {
            failClosed("The connection to \(hostSubject) was interrupted. Control was released.")
        }
    }

    func connectionRestored() async {
        guard !ended, !isPaused, !connectionModel.accessEnded else { return }
        connectionUnavailable = false
        guard !isLoading else { return }
        if session == nil { await start(); return }
        let failedReceiver: Bool
        if case .failed = receiverState { failedReceiver = true } else { failedReceiver = false }
        if failedReceiver {
            await retry(preservingSource: true)
        } else {
            await refresh()
            if failure != nil || session?.state == .ended || session?.state == .failed || session?.state == .stale {
                await retry(preservingSource: true)
            }
        }
    }

    /// Cupola left the foreground. Control ends (the computer lets go of every
    /// held key and button) and the computer stops sharing its screen, but the
    /// open view and its chosen window stay, so returning reopens the picture.
    /// Pending input is dropped, never replayed.
    func pause() async {
        resumeWanted = false
        guard !ended, !isPaused else { return }
        isPaused = true
        controlRecoveryRevision &+= 1
        recovery.receive(.pause)
        recoveryTask?.cancel(); recoveryTask = nil
        connectionUnavailable = false
        failure = nil
        receiverState = .retrying("Computer view paused. Return to Cupola to reconnect.")
        pausedWithControl = controlLease != nil || isStarting
        isLoading = false
        // An opening or a retry in flight drops its result.
        lifetimeRevision &+= 1
        let teardown = Task { @MainActor [weak self] in
            guard let self else { return }
            await self.tearDownForBackground()
        }
        pauseTeardown = teardown
        await teardown.value
    }

    private func tearDownForBackground() async {
        receiver.close(reason: "background", keepingLastFrame: true)
        // Let go of any held button or key while the lease still stands.
        releaseHeldInput(.background)
        await inputTask?.value
        await releaseControl()
        confirmedEndedSessionID = nil
        guard !ended, let current = session, !isFixture, !connectionModel.previewMode else { return }
        do {
            try await endOnHost(current)
            confirmedEndedSessionID = current.id
        } catch {
            // Not confirmed: reopening asks the computer to end it again first.
            unconfirmedEnd = current
        }
    }

    /// Ends a view on the computer. Ending one that is already gone succeeds;
    /// a thrown error means the computer may still be sharing it.
    private func endOnHost(_ current: ComputerSession) async throws {
        let _: ComputerEmptyReply = try await connectionModel.manage(
            "/api/v1/computer/sessions/\(Self.escape(current.id))?generation=\(current.generation)",
            method: "DELETE"
        )
    }

    /// Back in the foreground: viewing reopens on the same window. Control is
    /// never taken again on its own.
    func resumeAfterBackground() async {
        // Ended access stays ended: the owner closes the view instead.
        guard isPaused, !ended, !connectionModel.accessEnded else { return }
        resumeWanted = true
        await pauseTeardown?.value
        // Left again, closed, or another resume already reopened it.
        guard resumeWanted, isPaused, !ended else { return }
        resumeWanted = false
        pauseTeardown = nil
        isPaused = false
        recovery.receive(.resume)
        let hadControl = pausedWithControl
        pausedWithControl = false
        await retry(preservingSource: true)
        if hadControl, !ended, controlLease == nil {
            controlMessage = "Control ended when you left Cupola. Take control again to continue."
        }
    }

    func close(keepingLastFrame: Bool = false) async {
        guard !ended else { return }
        recovery.receive(.close)
        recoveryTask?.cancel(); recoveryTask = nil
        #if WONDER_DIAGNOSTICS
        fixtureRecoveryTask?.cancel(); fixtureRecoveryTask = nil
        #endif
        let keepPicture = keepingLastFrame && snapshotScope == connectionModel.imagePreviewScope && !connectionModel.accessEnded
        if keepPicture, !openingMacDialog, let current = displaySession {
            connectionModel.computerPictures.save(snapshotKey, picture: receiver.picture, session: current)
        } else if snapshotScope == connectionModel.imagePreviewScope {
            connectionModel.computerPictures.remove(snapshotKey)
        }
        isPaused = false
        ended = true
        lifetimeRevision &+= 1
        isLoading = false
        receiverState = .closed
        receiver.close(keepingLastFrame: keepPicture)
        await releaseControl()
        guard !isFixture, !connectionModel.previewMode else { return }
        let views = [session, unconfirmedEnd].compactMap { $0 }
        unconfirmedEnd = nil
        for (index, current) in views.enumerated()
        where current.id != confirmedEndedSessionID && !views.prefix(index).contains(where: { $0.id == current.id }) {
            // Closing is best-effort after the local receiver has been stopped.
            try? await endOnHost(current)
        }
    }

    private func startReceiver(for session: ComputerSession, connection: SavedConnection?) {
        guard session.capability.available, let connection else { return }
        receiver.start(
            session: session,
            transport: AuthenticatedComputerSignalingTransport(api: connectionModel.api, connection: connection,
                clipboardSubscription: { [weak self] in
                    guard let self, !self.ended, self.session?.id == session.id,
                          self.session?.generation == session.generation,
                          self.clipboardSharingEnabled, self.canShareMacClipboard,
                          UIApplication.shared.applicationState == .active else {
                        return ComputerClipboardSubscription(id: nil, cursor: 0)
                    }
                    return ComputerClipboardSubscription(id: self.clipboardSubscriptionID, cursor: self.clipboardCursor)
                }, receiveClipboard: { [weak self] response, subscription in
                    self?.receiveClipboard(response, requested: subscription, session: session)
                })
        )
    }

    /// Fit: the whole picture, with bars on the longer sides.
    func resetZoom() {
        zoomScale = 1
        #if DEBUG || WONDER_DIAGNOSTICS
        zoomRequested = false
        #endif
        viewportCenter = CGPoint(x: 0.5, y: 0.5)
    }

    /// The viewport's laid-out size. A different width (rotation, the fold, a
    /// pane resize) puts the picture back at Fit: an old zoom would crop a
    /// picture laid out for another shape. A change in height alone (the
    /// keyboard rising) keeps what the person zoomed to.
    func viewportResized(_ size: CGSize) {
        guard size != viewportSize, size.width.isFinite, size.height.isFinite else { return }
        let widthChanged = abs(size.width - viewportSize.width) >= 1
        viewportSize = size
        if widthChanged { resetZoom() }
    }

    func magnify(_ value: CGFloat) {
        guard value.isFinite else { return }
        #if DEBUG || WONDER_DIAGNOSTICS
        zoomRequested = true
        #endif
        let maximum = max(3, ComputerViewportTransform.fillZoom(size: viewportSize, aspect: Self.sourceAspectRatio(displaySession)))
        // Pinching back to nearly the whole picture lands exactly on Fit.
        zoomScale = value < 1.05 ? 1 : min(value, maximum)
        let halfVisible = 0.5 / zoomScale
        viewportCenter = CGPoint(x: min(max(viewportCenter.x, halfVisible), 1 - halfVisible),
                                 y: min(max(viewportCenter.y, halfVisible), 1 - halfVisible))
    }

    func panViewport(by delta: CGSize, in size: CGSize) {
        guard zoomScale > 1 else { return }
        let transform = viewportTransform(in: size)
        viewportCenter = ComputerViewportTransform(size: size, aspect: Self.sourceAspectRatio(displaySession), zoom: zoomScale,
            center: transform.panning(by: delta), anchorsTop: anchorsPictureTop).center
    }

    func setAnchorsPictureTop(_ anchors: Bool) {
        if anchorsPictureTop != anchors { anchorsPictureTop = anchors }
    }

    /// While zoomed, the trackpad pointer pulls the picture along so it never leaves the screen.
    private func keepPointerVisible(in size: CGSize) {
        guard zoomScale > 1 else { return }
        let next = viewportTransform(in: size).centerKeeping(pointer, margin: 24)
        if next != viewportCenter { viewportCenter = next }
    }

    /// A zoom belongs to the width it was made at. A viewport laid out at
    /// another width (a pane resized, the cover opening) draws Fit from its
    /// first frame, before `viewportResized` has put the model back at Fit.
    private func zoomApplies(to size: CGSize) -> Bool {
        viewportSize == .zero || abs(size.width - viewportSize.width) < 1
    }

    func viewportTransform(in size: CGSize) -> ComputerViewportTransform {
        let applies = zoomApplies(to: size)
        return ComputerViewportTransform(size: size, aspect: Self.sourceAspectRatio(displaySession),
                                         zoom: applies ? zoomScale : 1,
                                         center: applies ? viewportCenter : CGPoint(x: 0.5, y: 0.5),
                                         anchorsTop: anchorsPictureTop)
    }

    func recenterPointer() {
        guard isControlActive else { return }
        finishPointerDrag()
        pointer = viewportCenter
        enqueueInput([.pointer(x: Double(pointer.x), y: Double(pointer.y), phase: "move", button: nil)])
    }

    /// The host owns input availability. A window uses the same control
    /// affordance as a display; capture source type is not a permission gate.
    var isViewOnly: Bool {
        guard let session else { return false }
        return session.state == .live && session.control?.available != true
    }

    /// The host's own words, such as "This simulator can be viewed but not
    /// controlled with this version of Xcode."
    var viewOnlyReason: String {
        session?.control?.reason ?? (isSimulator ? "This simulator can't be controlled from here."
                                                 : "Input is unavailable on this computer.")
    }

    var controlAvailable: Bool {
        guard let session else { return false }
        return session.state == .live
            && receiverState == .live
            && session.capability.available
            && session.control?.available == true
            && !isViewOnly
            && !ended
            && !isPaused
            && !connectionUnavailable
    }

    var canTakeControl: Bool {
        controlAvailable && !isPaused && controlLease == nil && controlState != .starting && controlState != .stopping
    }

    var isControlActive: Bool { controlState == .active && controlLease != nil }
    var isStarting: Bool { controlState == .starting }

    func takeControl(restoringPointer: CGPoint? = nil) {
        guard canTakeControl, let session else { return }
        let snapshot = ComputerControlSnapshot(
            sessionID: session.id,
            generation: session.generation,
            geometryRevision: session.geometryRevision,
            sourceID: session.source.id
        )
        let requestID = controlRequestID ?? UUID().uuidString.lowercased()
        controlRequestID = requestID
        controlSnapshot = snapshot
        controlMessage = nil
        clipboardMessage = nil
        controlState = .starting
        controlTask?.cancel()
        controlTask = Task { [weak self] in
            await self?.acquireControl(snapshot: snapshot, requestID: requestID, restoringPointer: restoringPointer)
        }
    }

    func done() async {
        await releaseControl()
    }

    func toggleKeyboard() {
        guard isControlActive else {
            keyboardPresented = false
            return
        }
        cancelAppSwitcher()
        keyboardRaisedForTextField = false
        keyboardPresented.toggle()
        if keyboardPresented {
            saveBottomInput(.keyboard)
        } else {
            saveBottomInput(showsTrackpad ? .trackpad : .off)
        }
    }

    /// A touch on the shortcut row (scrolling it) is not "somewhere else": it keeps a held ⌘.
    func claimSwitcherTouch() {
        if appSwitcher.isPresented { switcherClaimedAt = Date() }
    }

    func sendKey(_ key: String) {
        guard isControlActive else { return }
        keyboardRevision &+= 1
        if appSwitcher.isPresented { switcherClaimedAt = Date() }
        // A latched ⌘ stays down for the keys that follow, like a held ⌘.
        if isSwitcherLatched, !Self.switcherNavigationKeys.contains(key) {
            enqueueInput([.key(key: key, phase: "press", modifiers: platformControls.switcherModifiers)])
            return
        }
        enqueueModifiedInput(appSwitcher.key(key))
    }

    private func withSwitcherModifiers(_ action: ComputerInputAction) -> ComputerInputAction {
        guard case let .key(key, phase, modifiers) = action else { return action }
        return .key(key: key, phase: phase, modifiers: modifiers | platformControls.switcherModifiers)
    }

    private static let switcherNavigationKeys: Set<String> = ["escape", "return", "tab", "left", "right", "up", "down"]

    func toggleAppSwitcher() {
        guard isControlActive else { return }
        switcherClaimedAt = Date()
        // Tapping the key again straight away latches ⌘ instead of choosing.
        if appSwitcher.isPresented, !switcherLatchRequested,
           Date().timeIntervalSince(switcherOpenedAt) < Self.switcherDoubleTapWindow {
            switcherLatchRequested = true
            return
        }
        finishPointerDrag()
        keyboardRevision &+= 1
        let opening = !appSwitcher.isPresented
        let actions = releaseModifiers() + appSwitcher.toggle(controls: platformControls)
        if opening, appSwitcher.isPresented {
            switcherOpenedAt = Date()
            switcherLatchRequested = false
            touchEnds.start { [weak self] in self?.touchEnded() }
        }
        enqueueInput(actions)
    }

    /// A touch ended somewhere. Unless it was on the switcher's own keys, ⌘
    /// is let go; a latched ⌘ waits for its key. The hop lets a key's action
    /// run first whichever order UIKit delivers them in.
    private func touchEnded() {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            guard appSwitcher.isPresented else { touchEnds.stop(); return }
            guard !switcherLatchRequested, isControlActive,
                  Date().timeIntervalSince(switcherClaimedAt) > 0.3 else { return }
            enqueueInput(appSwitcher.finish())
            touchEnds.stop()
        }
    }

    /// The ⌘ released by a click or a drag: none while it is latched.
    private func switcherReleaseForPointer() -> [ComputerInputAction] {
        switcherLatchRequested ? [] : appSwitcher.finish()
    }

    func performControl(_ button: ComputerPlatformControls.Button) {
        guard isControlActive else { return }
        switch button.action {
        case .keys(let actions):
            finishPointerDrag()
            keyboardRevision &+= 1
            enqueueInput(releaseModifiers() + appSwitcher.finish(cancel: true) + actions)
        case .modifier(let key):
            finishPointerDrag(releasingLatch: false)
            keyboardRevision &+= 1
            enqueueInput(appSwitcher.finish(cancel: true) + modifierLatch.toggle(key))
            // Keep the pad's selected modifier visible. Only hardware typing needs focus here.
            if Self.hardwareKeyboardTyping { keyboardPresented = true }
        case .appSwitcher: toggleAppSwitcher()
        case .applications(let open, let browse):
            if offersAppLauncher {
                finishPointerDrag()
                cancelAppSwitcher()
                keyboardPresented = false
                appLauncherPresented = true
            } else { showApplications(open: open, browse: browse) }
        case .rightClick:
            finishPointerDrag()
            keyboardRevision &+= 1
            enqueueInput(releaseModifiers() + appSwitcher.finish(cancel: true) + [
                .pointer(x: Double(pointer.x), y: Double(pointer.y), phase: "move", button: nil),
                .pointer(x: Double(pointer.x), y: Double(pointer.y), phase: "down", button: "right"),
                .pointer(x: Double(pointer.x), y: Double(pointer.y), phase: "up", button: "right")
            ])
        }
    }

    private func releaseModifiers() -> [ComputerInputAction] {
        guard !modifierLatch.keys.isEmpty else { return [] }
        keyboardRevision &+= 1
        return modifierLatch.release()
    }

    private func modifiedInputActions(_ actions: [ComputerInputAction]) -> [ComputerInputAction]? {
        guard !modifierLatch.keys.isEmpty else { return actions }
        guard let modified = modifierLatch.applying(to: actions) else {
            controlMessage = "Release the selected modifier before typing this character."
            return nil
        }
        // A software-keyboard shortcut did not insert its shadow text remotely.
        // Reset that context after consuming the one-shot modifier.
        keyboardRevision &+= 1
        if controlMessage != nil { controlMessage = nil }
        return modified
    }

    private func enqueueModifiedInput(_ actions: [ComputerInputAction]) {
        guard let modified = modifiedInputActions(actions) else { return }
        enqueueInput(modified)
    }

    private func showApplications(open: ComputerInputAction, browse: ComputerInputAction) {
        guard isControlActive, let lease = controlLease, let session else { return }
        finishPointerDrag()
        keyboardRevision &+= 1
        applicationsTask?.cancel()
        // Spotlight needs a moment to open before its Applications browse
        // shortcut is sent. Both keys use the existing authenticated input lease.
        enqueueInput(appSwitcher.finish(cancel: true)
            + [open])
        let intentRevision = inputIntentRevision
        applicationsTask = Task { [weak self] in
            await self?.awaitInputIdle()
            try? await Task.sleep(for: .milliseconds(300))
            guard let self, !Task.isCancelled, self.isControlActive,
                  self.controlLease?.id == lease.id,
                  self.session?.id == session.id,
                  self.session?.generation == session.generation,
                  self.inputIntentRevision == intentRevision else { return }
            self.enqueueInput([browse])
        }
    }

    private func cancelAppSwitcher() {
        let actions = appSwitcher.finish(cancel: true)
        if !actions.isEmpty { enqueueInput(actions) }
    }

    func setClipboardSharing(_ enabled: Bool) {
        guard clipboardSharingEnabled != enabled else { return }
        clipboardSharingEnabled = enabled
        inputPreferences.set(enabled, forKey: "computer.shareClipboard")
        clipboardSubscriptionID = UUID().uuidString
        clipboardCursor = 0
        receiver.noteActivity()
                confirmationTask?.cancel(); confirmationTask = nil
        clipboardMessage = enabled && isFixture ? "Clipboard sharing is unavailable in this preview." : nil
    }

    private func receiveClipboard(_ response: ComputerClipboardPollResponse,
                                  requested: ComputerClipboardSubscription, session expected: ComputerSession) {
        guard !ended, clipboardSharingEnabled, requested.id == clipboardSubscriptionID,
              UIApplication.shared.applicationState == .active,
              session?.id == expected.id, session?.generation == expected.generation,
              response.signaling.sessionId == expected.id,
              response.signaling.generation == expected.generation else { return }
        guard ComputerClipboardUpdate.isCurrent(response.clipboard, subscriptionID: clipboardSubscriptionID) else { return }
        publishClipboardMessage(ComputerClipboardUpdate.message(
            for: response.clipboard, subscriptionID: clipboardSubscriptionID, hostName: hostShortName))
        guard let update = response.clipboard, update.status == "watching",
              update.subscriptionId == clipboardSubscriptionID else { return }
        if update.sequence > clipboardCursor, let text = update.text {
            receiver.noteActivity()
            ComputerPhoneClipboard.write(text)
            confirmClipboardCopy()
        }
        clipboardCursor = max(clipboardCursor, update.sequence)
    }

    /// Standing status (a permission to grant, a Mac that cannot share) never replaces the brief copy confirmation.
    private func publishClipboardMessage(_ message: String?) {
        guard confirmationTask == nil else { return }
        if clipboardMessage != message { clipboardMessage = message }
    }

    /// One short confirmation that text arrived from the Mac.
    private func confirmClipboardCopy() {
        clipboardMessage = "Copied from \(hostShortName)"
        confirmationTask?.cancel()
        confirmationTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(4))
            guard !Task.isCancelled, let self else { return }
            clipboardMessage = nil
            confirmationTask = nil
        }
    }

    func pasteFromPhone(_ text: String) {
        guard isControlActive else { return }
        cancelAppSwitcher()
        guard !text.isEmpty else {
            controlMessage = "There is no text on this device to paste."
            return
        }
        guard text.unicodeScalars.count <= 8_192,
              !text.unicodeScalars.contains(where: { $0.properties.generalCategory == .control && $0 != "\n" && $0 != "\t" }) else {
            controlMessage = "The clipboard text on this device is too large or contains unsupported characters."
            return
        }
        // The daemon intentionally accepts pasteFromPhone only as this one action.
        keyboardRevision &+= 1
        // Clipboard actions stand alone, even after a deck modifier or app
        // switcher. The menu temporarily takes focus from the input surface.
        releaseHeldInput(.focusLost)
        enqueueInput([.clipboard(operation: "pasteFromPhone", text: text)])
    }

    func copyFromMac() {
        guard isControlActive else { return }
        cancelAppSwitcher()
        releaseHeldInput(.focusLost)
        enqueueInput([.clipboard(operation: "copyToPhone", text: nil)])
    }

    func sendKeyboardActions(_ actions: [ComputerInputAction]) {
        guard isControlActive else { return }
        guard actions.allSatisfy({ action in
            guard case .text(let text) = action else { return true }
            return !text.isEmpty && text.unicodeScalars.count <= 4_096
                && !text.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
        }) else {
            failClosed("The keyboard input was too large or contained unsupported characters. Take control again to continue.")
            return
        }
        if !isSwitcherLatched { cancelAppSwitcher() }
        guard let modified = modifiedInputActions(actions) else { return }
        for start in stride(from: 0, to: modified.count, by: 32) {
            enqueueInput(Array(modified[start..<min(start + 32, modified.count)]))
        }
    }

    /// VoiceOver's Click and Right-click: at the pointer, or in Direct touch
    /// mode at the middle of the visible picture (never the empty space beside it).
    func accessibilityClick(in size: CGSize, button: String = "left") {
        tap(at: viewportTransform(in: size).visiblePictureCenter, in: size, button: button)
    }

    /// A click on the picture. `modifiers` are keys held on an iPad's hardware
    /// keyboard; a latched on-screen modifier applies to this click and is then
    /// released, the same as on the trackpad.
    func tap(at location: CGPoint, in size: CGSize, button: String = "left", modifiers: [String] = []) {
        guard isControlActive else { return }
        finishPointerDrag()
        if pictureInputMode == .directTouch {
            guard let point = viewportTransform(in: size).point(location) else { return }
            pointer = point
        }
        keyboardRevision &+= 1
        enqueueInput(pointerPress.click(at: pointer, button: button, physicalModifiers: modifiers, latch: &modifierLatch)
            + switcherReleaseForPointer())
    }

    func movePointer(by delta: CGSize, in size: CGSize) {
        guard isControlActive else { return }
        let next = viewportTransform(in: size).moving(pointer, by: delta)
        // At the edge of the picture the pointer stays put: nothing to send.
        guard next != pointer else { return }
        pointer = next
        keepPointerVisible(in: size)
        enqueueInput([pointerPress.move(to: pointer)])
    }

    /// An iPad pointer moves the Mac pointer to the same spot on the screen.
    func hoverPointer(at location: CGPoint, in size: CGSize) {
        guard isControlActive, !pointerPress.isHeld,
              let point = viewportTransform(in: size).point(location), point != pointer else { return }
        pointer = point
        enqueueInput([.pointer(x: Double(point.x), y: Double(point.y), phase: "move", button: nil)])
    }

    /// A latched modifier stays down for the whole drag (⇧-drag extends a
    /// selection) and is released after the button comes up.
    func dragBegan(at location: CGPoint, in size: CGSize, modifiers: [String] = []) {
        guard isControlActive, !pointerPress.isHeld else { return }
        if pictureInputMode == .directTouch {
            guard let point = viewportTransform(in: size).point(location) else { return }
            pointer = point
        }
        keyboardRevision &+= 1
        enqueueInput(switcherReleaseForPointer() + pointerPress.begin(at: pointer, physicalModifiers: modifiers, latch: modifierLatch))
    }

    func dragMoved(to location: CGPoint, in size: CGSize) {
        guard isControlActive, pointerPress.isHeld,
              let point = viewportTransform(in: size).point(location, clamped: true), point != pointer else { return }
        pointer = point
        enqueueInput([pointerPress.move(to: point)])
    }

    /// Also the cancel path: a drag the system took away still lets go of the
    /// button and its modifiers.
    func dragEnded(at location: CGPoint?, in size: CGSize) {
        if pictureInputMode == .directTouch, pointerPress.isHeld, let location,
           let point = viewportTransform(in: size).point(location, clamped: true) {
            pointer = point
            pointerPress.relocate(to: point)
        }
        finishPointerDrag()
    }

    /// Lets go of a held drag. The one-shot latch goes with it unless the person
    /// is changing the latches themselves.
    private func finishPointerDrag(releasingLatch: Bool = true) {
        guard pointerPress.isHeld else { return }
        guard isControlActive else { pointerPress.reset(); return }
        let actions = pointerPress.end(latch: &modifierLatch, releasingLatch: releasingLatch)
        if releasingLatch { keyboardRevision &+= 1 }
        enqueueInput(actions)
    }

    /// The fold or the window changed shape (the key rows differ): a held drag
    /// and the latches end, so no modifier stays down out of sight.
    func postureChanged() { releaseHeldInput(.poseChanged) }

    /// Lets go of every mouse button and key this device holds down on the
    /// computer: a drag, the keys pressed with it and the latched modifiers.
    /// The keyboard going away, the app losing focus or the background and a
    /// new pose all end up here; the pure rule is `ComputerInputRelease`.
    func releaseHeldInput(_ reason: ComputerInputRelease.Reason) {
        scrollAccumulator.reset()
        guard isControlActive else { return }
        let releasedPress = ComputerInputRelease.release(reason, press: &pointerPress, latch: &modifierLatch, to: self)
        if releasedPress {
            keyboardRevision &+= 1
            #if WONDER_DIAGNOSTICS
            heldInputReleases += 1
            #endif
        }
    }

    /// Shows or hides the separate trackpad. A drag in progress ends first.
    func toggleTrackpad() {
        finishPointerDrag()
        showsTrackpad.toggle()
        // The keyboard remains the selected slot until it is hidden.
        // The pad toggle only changes what returns behind it.
        if !keyboardPresented || Self.hardwareKeyboardTyping { saveBottomInput(showsTrackpad ? .trackpad : .off) }
    }

    /// Scrolls in whole points; a finger delta under a point is carried into the next one.
    func twoFingerScroll(delta: CGSize) {
        guard isControlActive, delta.width.isFinite, delta.height.isFinite,
              let whole = scrollAccumulator.add(CGSize(width: -delta.width, height: -delta.height)) else { return }
        let x = min(max(Double(whole.width), -4_096), 4_096)
        let y = min(max(Double(whole.height), -4_096), 4_096)
        enqueueInput([.scroll(deltaX: x, deltaY: y)])
    }

    // MARK: Simulator input

    /// An iPad with a keyboard attached (Magic Keyboard, Smart Keyboard or Bluetooth).
    static var hardwareKeyboardTyping: Bool {
        #if WONDER_DIAGNOSTICS
        // The text-focus fixture checks the on-screen keyboard, whatever keyboard
        // the simulator has attached.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-text-focus")
            || ProcessInfo.processInfo.arguments.contains("-diagnostics-no-hardware-keyboard") { return false }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-hardware-keyboard") {
            return UIDevice.current.userInterfaceIdiom == .pad
        }
        #endif
        return UIDevice.current.userInterfaceIdiom == .pad && GCKeyboard.coalesced != nil
    }

    /// A view of a booted simulator (the Simulator view), not a computer screen.
    var isSimulator: Bool {
        if let kind = session?.source.kind { return kind == "simulator" }
        if case .simulator = viewTarget { return true }
        return false
    }

    /// One finger action from the simulator screen, in its own batch.
    func simulatorTouch(_ action: ComputerInputAction) {
        guard isSimulator, isControlActive else { return }
        enqueueInput([action])
    }

    /// A whole gesture (VoiceOver's tap, long press and swipes) as one batch.
    func simulatorGesture(_ actions: [ComputerInputAction]) {
        guard isSimulator, isControlActive, !actions.isEmpty else { return }
        enqueueInput(actions)
    }

    /// Home, Lock, Volume up or Volume down, pressed and released.
    func pressSimulatorButton(_ name: String) {
        guard isSimulator, isControlActive else { return }
        keyboardRevision &+= 1
        enqueueInput([.button(name: name, phase: "press")])
    }

    /// The simulator types only what a US keyboard can; the host refuses the
    /// rest before using the lease, so control stays and the text is dropped.
    static let simulatorTextRefusal = "The simulator can only type letters, numbers and symbols from a US keyboard, so that text wasn’t sent."

    private func refuseSimulatorText() {
        // The keyboard's shadow of sent text no longer matches the simulator.
        keyboardRevision &+= 1
        controlMessage = Self.simulatorTextRefusal
    }

    #if DEBUG || WONDER_DIAGNOSTICS
    /// The host's rule (`simulator_typeable`), standing in for it in fixtures.
    private static func simulatorTypeable(_ actions: [ComputerInputAction]) -> Bool {
        actions.allSatisfy { action in
            guard case .text(let text) = action else { return true }
            return text.unicodeScalars.allSatisfy { (0x20...0x7E).contains($0.value) || $0 == "\n" || $0 == "\t" || $0 == "\r" }
        }
    }
    #endif

    // MARK: The separate trackpad (iPhone Duo laptop pose)

    /// One action from the trackpad below the fold. Movement is scaled to the
    /// picture, as if the finger moved on it, whatever the input mode.
    func trackpad(_ output: ComputerTrackpadGestures.Output) {
        guard isControlActive else { return }
        switch output {
        case .move(let delta): movePointer(by: delta, in: viewportSize)
        case .scroll(let delta): twoFingerScroll(delta: delta)
        case .click(let button):
            finishPointerDrag()
            keyboardRevision &+= 1
            // A latched modifier is down during the click (⌘-click) and then released.
            enqueueInput(pointerPress.click(at: pointer, button: button, latch: &modifierLatch) + switcherReleaseForPointer())
        case .dragBegan:
            guard !pointerPress.isHeld else { return }
            keyboardRevision &+= 1
            enqueueInput(switcherReleaseForPointer() + pointerPress.begin(at: pointer, latch: modifierLatch))
        case .dragEnded:
            finishPointerDrag()
        case .coast:
            // The pad turns coasting into scrolls itself.
            break
        }
    }

    /// The trackpad key row's latches: down until the next key or click.
    func toggleModifier(_ key: String) {
        guard isControlActive else { return }
        finishPointerDrag(releasingLatch: false)
        keyboardRevision &+= 1
        enqueueInput(appSwitcher.finish(cancel: true) + modifierLatch.toggle(key))
    }

    // MARK: App launcher

    /// The launcher lists the computer's installed apps (a Mac).
    var offersAppLauncher: Bool { connectionModel.isMacHost || isFixture }

    func installedApps() async throws -> [ComputerInstalledApp] {
        if isFixture { return ComputerInstalledApp.fixture }
        let list: ComputerInstalledAppList = try await connectionModel.manage("/api/v1/computer/apps")
        guard list.available else { throw ComputerAppLauncherFailure(message: list.reason ?? "Opening apps is not available on \(hostShortName).") }
        return list.apps
    }

    /// Dock discovery shares the window discovery's five-second cadence. The
    /// view task owns cancellation; a pairing change cannot publish an old host.
    private var miniDockIdentity: String? {
        connectionModel.connection.map { $0.credential.hostInstallationId + ":" + $0.credential.deviceId }
    }

    func refreshMiniDock() async {
        let connection = miniDockIdentity
        let sessionID = session?.id
        guard !ended, !isPaused, !isSimulator else { miniDock = .unavailable; return }
        #if WONDER_DIAGNOSTICS
        if isFixture {
            miniDock = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-dock")
                && !ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-dock-unavailable")
                ? .fixture(platform: platformKey) : .unavailable
            return
        }
        #endif
        do {
            let content: ComputerMiniDockContent = try await connectionModel.manage("/api/v1/computer/dock")
            guard !Task.isCancelled, miniDockIdentity == connection,
                  session?.id == sessionID, !ended, !isPaused else { return }
            miniDock = content.available ? content : .unavailable
        } catch {
            guard !Task.isCancelled, miniDockIdentity == connection, session?.id == sessionID else { return }
            // Older hosts and unavailable desktops leave no empty dock card.
            miniDock = .unavailable
        }
    }

    func installedAppIcon(_ id: String) async -> UIImage? {
        if let cached = appIcons.object(forKey: id as NSString) { return cached }
        guard !isFixture else { return nil }
        guard let reply: ComputerInstalledAppIcon = try? await connectionModel.manage(
            "/api/v1/computer/apps/icon?id=\(Self.escape(id))"),
              reply.iconPNG.utf8.count <= 64 * 1024,
              let data = Data(base64Encoded: reply.iconPNG) else { return nil }
        // Downsample before decoding, even if an older host sends a large PNG.
        let image = await Task.detached(priority: .utility) { () -> UIImage? in
            guard data.starts(with: [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
                  let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
                  let thumbnail = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                    kCGImageSourceCreateThumbnailFromImageAlways: true,
                    kCGImageSourceThumbnailMaxPixelSize: 64,
                    kCGImageSourceShouldCacheImmediately: true
                  ] as CFDictionary) else { return nil }
            return UIImage(cgImage: thumbnail)
        }.value
        guard !Task.isCancelled else { return nil }
        if let image { appIcons.setObject(image, forKey: id as NSString) }
        return image
    }

    func openDockApp(_ app: ComputerMiniDockContent.App) async {
        guard isControlActive, let session, let lease = controlLease else {
            controlMessage = "Take control to open apps on \(hostShortName)."
            return
        }
        let snapshot = ComputerControlSnapshot(sessionID: session.id, generation: session.generation,
                                               geometryRevision: session.geometryRevision, sourceID: session.source.id)
        // App focus changes end the old app's drag, latches, switcher and IME
        // shadow before the authenticated open can reach the computer.
        releaseHeldInput(.focusLost)
        cancelAppSwitcher()
        keyboardRevision &+= 1
        await awaitInputIdle()
        guard !Task.isCancelled, isControlActive, controlAvailable,
              controlLease?.id == lease.id, let current = self.session,
              Self.snapshotMatches(snapshot, session: current), Self.bindingMatches(lease, session: current) else { return }
        let message = await openApp(.init(id: app.id, name: app.name))
        guard !Task.isCancelled, controlLease?.id == lease.id, let current = self.session,
              Self.snapshotMatches(snapshot, session: current) else { return }
        controlMessage = message
    }

    func selectBottomInput(_ input: ComputerBottomInput, persistentTrackpad: Bool = false) {
        guard isControlActive else { return }
        releaseHeldInput(.keyboardHidden)
        switch input {
        case .keyboard: toggleKeyboard()
        case .trackpad:
            if persistentTrackpad {
                keyboardPresented = false
                return
            }
            let wasTyping = keyboardPresented
            keyboardPresented = false
            if !showsTrackpad || !wasTyping { toggleTrackpad() }
            else { saveBottomInput(.trackpad) }
        case .off: keyboardPresented = false
        }
    }

    /// Opens an app on the computer. Returns why it did not open, or nil.
    func openApp(_ app: ComputerInstalledApp) async -> String? {
        guard isControlActive, let session, let lease = controlLease else {
            return "Take control to open apps on \(hostShortName)."
        }
        if isFixture {
            openedAppIDs.append(app.id)
            return nil
        }
        do {
            let reply: ComputerOpenAppResponse = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(session.id))/apps/open",
                method: "POST",
                body: JSONEncoder().encode(ComputerOpenAppRequest(
                    leaseId: lease.id, generation: lease.generation,
                    geometryRevision: lease.geometryRevision, sourceId: lease.sourceId, bundleId: app.id)),
                decodingStatuses: [404, 409, 502, 503]
            )
            guard reply.opened == true else {
                return reply.reason ?? "\(hostSubjectStart) could not open \(app.name)."
            }
            if let refreshed = reply.lease, controlLease?.id == lease.id { _ = adoptLease(refreshed, for: session) }
            return nil
        } catch is CancellationError {
            return nil
        } catch {
            return "\(app.name) did not open. Check the connection to \(hostShortName) and try again."
        }
    }

    /// Older computers have no app list; the Apps key falls back to Spotlight's Applications.
    func openSpotlightApplications() {
        for button in platformControls.rows.flatMap({ $0 }) {
            if case let .applications(open, browse) = button.action { showApplications(open: open, browse: browse); return }
        }
    }

    static func sourceAspectRatio(_ session: ComputerSession?) -> CGFloat {
        guard let source = session?.source else { return 16 / 9 }
        let width = source.crop?.width ?? Double(source.width ?? 16)
        let height = source.crop?.height ?? Double(source.height ?? 9)
        guard width.isFinite, height.isFinite, width > 0, height > 0 else { return 16 / 9 }
        return CGFloat(width / height)
    }

    static func encodedAspectRatio(_ session: ComputerSession?) -> CGFloat {
        guard let width = session?.source.width, let height = session?.source.height,
              width > 0, height > 0 else { return sourceAspectRatio(session) }
        return CGFloat(width) / CGFloat(height)
    }

    private func acquireControl(snapshot: ComputerControlSnapshot, requestID: String, restoringPointer: CGPoint?) async {
        guard let session, Self.snapshotMatches(snapshot, session: session), controlAvailable else {
            controlState = .viewOnly
            return
        }
        #if DEBUG || WONDER_DIAGNOSTICS
        if isFixture {
            try? await Task.sleep(for: .milliseconds(1_500))
            guard !Task.isCancelled,
                  controlRequestID == requestID,
                  controlState == .starting,
                  Self.snapshotMatches(snapshot, session: session),
                  controlAvailable else { return }
            let lease = Self.fixtureLease(session: session)
            controlLease = lease
            controlState = .active
            controlMessage = nil
            positionControlPointer(restoringPointer)
            startHeartbeat()
            #if WONDER_DIAGNOSTICS
            // On signal control ends the way an interrupted connection ends it.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-control-active"), !fixtureControlLossArmed {
                fixtureControlLossArmed = true
                DiagnosticSubagentFixture.afterSignal("computer-control-lost") { [weak self] in
                    Task { @MainActor [weak self] in
                        guard let self, self.controlLease?.id == lease.id else { return }
                        self.failClosed("The connection to \(self.hostSubject) was interrupted. Control was released.")
                    }
                }
            }
            // The computer reports typing focus in a text field, then (on signal) leaving it.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-text-focus") {
                try? await Task.sleep(for: .seconds(1))
                guard controlLease?.id == lease.id else { return }
                receiveHostReport(ComputerHostReport(textInputFocused: true))
                DiagnosticSubagentFixture.afterSignal("computer-text-focus-left") { [weak self] in
                    Task { @MainActor [weak self] in
                        guard let self, self.controlLease?.id == lease.id else { return }
                        self.receiveHostReport(ComputerHostReport(textInputFocused: false))
                    }
                }
            }
            #endif
            return
        }
        #endif
        do {
            let body = try JSONEncoder().encode(AcquireComputerControlRequest(
                clientRequestId: requestID,
                generation: snapshot.generation,
                geometryRevision: snapshot.geometryRevision,
                sourceId: snapshot.sourceID
            ))
            let response: ComputerControlActionResponse = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(snapshot.sessionID))/control/acquire",
                method: "POST",
                body: body,
                decodingStatuses: [409, 503]
            )
            guard !Task.isCancelled,
                  controlRequestID == requestID,
                  controlState == .starting else {
                if let lease = response.lease { await releaseLease(lease) }
                return
            }
            let current = session
            guard Self.snapshotMatches(snapshot, session: current),
                  controlAvailable else {
                if let lease = response.lease { await releaseLease(lease) }
                failClosed("The computer view changed while control was starting.")
                return
            }
            guard response.granted,
                  response.acknowledged,
                  response.status == "active",
                  response.control.available,
                  let lease = response.lease,
                  Self.leaseMatches(lease, session: current, connection: connectionModel.connection) else {
                if let lease = response.lease { await releaseLease(lease) }
                finishControlFailure(response.reason.isEmpty ? "Control was not granted on \(hostReference)." : response.reason, resetRequest: true)
                return
            }
            controlLease = lease
            controlState = .active
            controlMessage = nil
            positionControlPointer(restoringPointer)
            startHeartbeat()
        } catch is CancellationError {
            if !ended && !releasingControl { controlState = .viewOnly }
        } catch PairingFailure.response(let status) where status == 409 {
            guard !ended, !releasingControl else { return }
            finishControlFailure("Control was not granted on \(hostReference). Try again when you are ready.", resetRequest: true)
        } catch {
            guard !ended, !releasingControl else { return }
            finishControlFailure("\(hostSentenceStart) could not confirm control. Try again.", resetRequest: false)
        }
    }

    private func positionControlPointer(_ position: CGPoint?) {
        // A simulator has no pointer; the host refuses pointer input for it.
        guard !isSimulator else { return }
        guard let position else { recenterPointer(); return }
        pointer = position
        enqueueInput([.pointer(x: Double(position.x), y: Double(position.y), phase: "move", button: nil)])
    }

    private func startHeartbeat() {
        heartbeatTask?.cancel()
        let seconds = max(Int(session?.control?.heartbeatIntervalSeconds ?? 3), 1)
        heartbeatTask = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(seconds)) }
                catch { return }
                guard !Task.isCancelled else { return }
                await self?.sendHeartbeat()
            }
        }
    }

    private func sendHeartbeat() async {
        guard !ended, !releasingControl, let session, let lease = controlLease else { return }
        if isFixture {
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-live-updates") {
                controlLease = lease
            }
            #endif
            return
        }
        let epoch = inputEpoch
        guard Self.leaseMatches(lease, session: session, connection: connectionModel.connection) else {
            failClosed("The computer view changed. Control was released.")
            return
        }
        do {
            let response: ComputerControlActionResponse = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(session.id))/control/heartbeat",
                method: "POST",
                body: JSONEncoder().encode(ComputerControlBindingRequest(
                    leaseId: lease.id,
                    generation: lease.generation,
                    geometryRevision: lease.geometryRevision,
                    sourceId: lease.sourceId
                )),
                decodingStatuses: [409, 503]
            )
            guard !Task.isCancelled, inputEpoch == epoch, !ended, !releasingControl, isControlActive,
                  controlLease?.id == lease.id else { return }
            guard response.acknowledged,
                  response.status == "active",
                  response.control.available,
                  let refreshed = response.lease,
                  adoptLease(refreshed, for: session) else {
                failClosed(response.reason.isEmpty ? "Control expired or is no longer available." : response.reason)
                return
            }
        } catch is CancellationError {
            if inputEpoch == epoch && controlLease?.id == lease.id && !ended && !releasingControl { failClosed("Control was interrupted.") }
        } catch {
            if inputEpoch == epoch && controlLease?.id == lease.id && !ended && !releasingControl { failClosed("Control was interrupted. \(hostSubjectStart) may still be finishing release.") }
        }
    }

    func awaitInputIdle() async {
        await inputTask?.value
    }

    private func enqueueInput(_ actions: [ComputerInputAction]) {
        guard isControlActive, !actions.isEmpty, let lease = controlLease else { return }
        inputIntentRevision &+= 1
        // Typing and clicking keep signaling quick, so text focus and the clipboard follow promptly.
        receiver.noteActivity()
        guard inputBuffer.append(actions) else {
            failClosed("\(hostSentenceStart) is not keeping up with input. Take control again to continue.")
            return
        }
        guard inputTask == nil else { return }
        let epoch = inputEpoch
        let leaseID = lease.id
        inputTask = Task { [weak self] in
            while let self, !Task.isCancelled, self.inputEpoch == epoch,
                  self.controlLease?.id == leaseID, self.isControlActive,
                  let next = self.inputBuffer.popFirst() {
                await self.sendInput(next, epoch: epoch, leaseID: leaseID)
            }
            guard let self, self.inputEpoch == epoch else { return }
            self.inputTask = nil
        }
    }

    private func discardPendingInput() {
        applicationsTask?.cancel()
        applicationsTask = nil
        inputEpoch &+= 1
        inputTask?.cancel()
        inputTask = nil
        inputBuffer.removeAll()
        pointerPress.reset()
        scrollAccumulator.reset()
        // Nothing is held by the time the keyboard goes: control ended, and the
        // computer lets go of everything itself.
        modifierLatch = ComputerModifierLatch()
        keyboardPresented = false
        keyboardRaisedForTextField = false
        lastTextFocus = nil
        appSwitcher = ComputerAppSwitcher()
        keyboardRevision &+= 1
    }

    private func sendInput(_ actions: [ComputerInputAction], epoch: UInt64, leaseID: String) async {
        guard !Task.isCancelled, inputEpoch == epoch, !ended, !releasingControl,
              let session, let lease = controlLease, lease.id == leaseID, isControlActive else { return }
        if isFixture {
            guard lease.lastSequence < UInt64.max else {
                failClosed("The computer view changed. Input was discarded.")
                return
            }
            #if DEBUG || WONDER_DIAGNOSTICS
            if isSimulator, !Self.simulatorTypeable(actions) { refuseSimulatorText(); return }
            #endif
            let sequence = lease.lastSequence + 1
            controlLease = Self.leaseWithSequence(lease, sequence: sequence)
            #if DEBUG || WONDER_DIAGNOSTICS
            fixtureSentInput += actions
            #endif
            if actions.contains(where: {
                if case .clipboard(let operation, _) = $0 { return operation == "copyToPhone" }
                return false
            }) {
                // The diagnostic fixture stands in for the authenticated Mac
                // response; production writes only response.clipboardText.
                ComputerPhoneClipboard.write("Fixture clipboard text")
                confirmClipboardCopy()
            }
            return
        }
        guard Self.leaseMatches(lease, session: session, connection: connectionModel.connection),
              lease.lastSequence < UInt64.max else {
            failClosed("The computer view changed. Input was discarded.")
            return
        }
        let sequence = lease.lastSequence + 1
        let request = ComputerInputBatchRequest(
            leaseId: lease.id,
            generation: lease.generation,
            geometryRevision: lease.geometryRevision,
            sourceId: lease.sourceId,
            sequence: sequence,
            actions: actions
        )
        do {
            let sentAt = ProcessInfo.processInfo.systemUptime
            let response: ComputerControlActionResponse = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(session.id))/control/input",
                method: "POST",
                body: JSONEncoder().encode(request),
                decodingStatuses: [409, 422, 503]
            )
            recordInputRoundTrip((ProcessInfo.processInfo.systemUptime - sentAt) * 1_000)
            guard !Task.isCancelled, inputEpoch == epoch, !ended, !releasingControl,
                  isControlActive, controlLease?.id == leaseID else { return }
            // Text a simulator cannot type is refused before the batch takes a
            // sequence number: the lease is unchanged and control continues.
            if isSimulator, !response.acknowledged, response.status == "unsupported" {
                refuseSimulatorText()
                return
            }
            guard response.acknowledged,
                  response.status == "active",
                  response.control.available,
                  let refreshed = response.lease,
                  adoptLease(refreshed, for: session) else {
                failClosed(response.reason.isEmpty ? "\(hostSubjectStart) rejected this input. Control was released." : response.reason)
                return
            }
            if actions.contains(where: {
                if case .clipboard(let operation, _) = $0 { return operation == "copyToPhone" }
                return false
            }) {
                guard let text = response.clipboardText else {
                    // The host explains why (permission, a private copy, no text);
                    // control stays active.
                    controlMessage = response.reason
                    return
                }
                ComputerPhoneClipboard.write(text)
                confirmClipboardCopy()
            }
        } catch is CancellationError {
            if inputEpoch == epoch && !ended && !releasingControl { failClosed("Input was interrupted. Control was released.") }
        } catch {
            if inputEpoch == epoch && !ended && !releasingControl { failClosed("\(hostSubjectStart) could not receive this input. Control was released.") }
        }
    }

    /// Keeps the last few input round trips; the median over 250 ms reads as
    /// "Slow to respond" so a single slow request does not flip the status.
    private func recordInputRoundTrip(_ milliseconds: Double) {
        guard milliseconds.isFinite, milliseconds >= 0 else { return }
        #if WONDER_DIAGNOSTICS
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "computer.input", phase: "duration", durationMs: milliseconds))
        #endif
        inputRoundTrips.append(milliseconds)
        if inputRoundTrips.count > 8 { inputRoundTrips.removeFirst(inputRoundTrips.count - 8) }
        guard inputRoundTrips.count >= 3 else { return }
        let median = inputRoundTrips.sorted()[inputRoundTrips.count / 2]
        let slow = median > 250
        if slow != slowInput { slowInput = slow }
    }

    private func releaseControl() async {
        controlTask?.cancel()
        controlTask = nil
        heartbeatTask?.cancel()
        heartbeatTask = nil
        discardPendingInput()
        let lease = controlLease
        controlLease = nil
        releasingControl = lease != nil
        if lease != nil { controlState = .stopping }
        if let lease, !isFixture {
            await releaseLease(lease)
            releasingControl = false
            // Control was taken again while this release was in flight: the
            // newer request owns the state now.
            guard controlLease == nil, controlState == .stopping else { return }
        }
        releasingControl = false
        controlState = .viewOnly
        controlMessage = nil
        clipboardMessage = nil
        if lease != nil { controlRequestID = nil }
        controlSnapshot = nil
    }

    private func releaseLease(_ lease: ComputerControlLease) async {
        guard !isFixture else { return }
        do {
            let _: ComputerControlActionResponse = try await connectionModel.manage(
                "/api/v1/computer/sessions/\(Self.escape(lease.sessionId))/control/release",
                method: "POST",
                body: JSONEncoder().encode(ComputerControlBindingRequest(
                    leaseId: lease.id,
                    generation: lease.generation,
                    geometryRevision: lease.geometryRevision,
                    sourceId: lease.sourceId
                ))
            )
        } catch { }
    }

    private func failClosed(_ message: String) {
        guard !ended, !releasingControl else { return }
        let lease = controlLease
        controlLease = nil
        heartbeatTask?.cancel()
        heartbeatTask = nil
        discardPendingInput()
        controlRequestID = nil
        controlSnapshot = nil
        controlMessage = message
        controlState = .failed(message)
        if let lease, !isFixture {
            Task { [weak self] in await self?.releaseLease(lease) }
        }
    }

    private func finishControlFailure(_ message: String, resetRequest: Bool) {
        discardPendingInput()
        controlLease = nil
        heartbeatTask?.cancel()
        heartbeatTask = nil
        pointerPress.reset()
        keyboardPresented = false
        if resetRequest {
            controlRequestID = nil
            controlSnapshot = nil
        }
        controlMessage = message
        controlState = .failed(message)
    }

    private func adoptLease(_ refreshed: ComputerControlLease, for session: ComputerSession) -> Bool {
        guard Self.leaseMatches(refreshed, session: session, connection: connectionModel.connection) else {
            return false
        }
        if let current = controlLease {
            guard let reconciled = Self.reconciledLease(current: current, refreshed: refreshed) else { return false }
            controlLease = reconciled
        } else {
            controlLease = refreshed
        }
        return true
    }

    private static func snapshotMatches(_ snapshot: ComputerControlSnapshot, session: ComputerSession) -> Bool {
        snapshot.sessionID == session.id
            && snapshot.generation == session.generation
            && snapshot.geometryRevision == session.geometryRevision
            && snapshot.sourceID == session.source.id
    }

    private static func bindingMatches(_ lease: ComputerControlLease, session: ComputerSession) -> Bool {
        lease.sessionId == session.id
            && lease.conversationId == session.conversationId
            && lease.generation == session.generation
            && lease.geometryRevision == session.geometryRevision
            && lease.sourceId == session.source.id
    }

    private static func leaseMatches(_ lease: ComputerControlLease, session: ComputerSession, connection: SavedConnection?) -> Bool {
        guard let connection else { return false }
        return bindingMatches(lease, session: session)
            && lease.status == "active"
            && lease.ownerDeviceId == connection.credential.deviceId
            && lease.hostInstallationId == connection.credential.hostInstallationId
    }

    private static func leaseWithSequence(_ lease: ComputerControlLease, sequence: UInt64) -> ComputerControlLease {
        ComputerControlLease(
            id: lease.id,
            sessionId: lease.sessionId,
            ownerDeviceId: lease.ownerDeviceId,
            hostInstallationId: lease.hostInstallationId,
            conversationId: lease.conversationId,
            generation: lease.generation,
            sourceId: lease.sourceId,
            geometryRevision: lease.geometryRevision,
            status: lease.status,
            lastSequence: sequence,
            acquiredAt: lease.acquiredAt,
            updatedAt: lease.updatedAt,
            expiresAt: lease.expiresAt,
            releasedAt: lease.releasedAt
        )
    }

    static func reconciledLease(current: ComputerControlLease,
                                refreshed: ComputerControlLease) -> ComputerControlLease? {
        guard current.id == refreshed.id else { return nil }
        return leaseWithSequence(
            refreshed,
            sequence: max(current.lastSequence, refreshed.lastSequence)
        )
    }

    private static func escape(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? value
    }

    private static func readable(_ error: Error, host: String, here: String, opening: Bool = false) -> String {
        if let conflict = error as? ComputerViewerConflict, let message = conflict.errorDescription { return message }
        if case PairingFailure.response(let status) = error {
            switch status {
            case 401, 403: return "Access to this computer has ended. Reconnect in Settings."
            case 404 where !opening: return "This computer view is no longer available on \(host)."
            // An older computer refuses a second view without naming the device.
            case 409 where opening: return "The computer view is open on another device. Close it there, then try again."
            case 409: return "The computer view changed on \(host). Close this screen and try again."
            case 503: return "Live computer viewing is unavailable on \(here). Try again in a moment."
            default: break
            }
        }
        return "Cupola could not open computer view. Check the connection to \(host)."
    }

    #if WONDER_DIAGNOSTICS
    /// The Simulator view's held states: `-diagnostics-simulator-loading`
    /// stays opening until `signal simulator-loaded`; `-diagnostics-simulator-failed`
    /// fails to open until `signal simulator-recovered`. False when the open
    /// ended here.
    private func openSimulatorFixture(revision: UInt64) async -> Bool {
        let arguments = ProcessInfo.processInfo.arguments
        if arguments.contains("-diagnostics-simulator-loading") {
            isLoading = true
            while !DiagnosticSubagentFixture.signalled("simulator-loaded") {
                try? await Task.sleep(for: .milliseconds(100))
                guard !Task.isCancelled, !ended, lifetimeRevision == revision else { return false }
            }
            isLoading = false
        }
        if arguments.contains("-diagnostics-simulator-failed"), !DiagnosticSubagentFixture.signalled("simulator-recovered") {
            // The host's words for a simulator its helper could not open.
            failure = "This simulator is unavailable."
            receiverState = .failed(failure!)
            return false
        }
        return true
    }
    #endif

    #if DEBUG || WONDER_DIAGNOSTICS
    private static var zoomSignalUsed = false
    /// Models made this launch: a re-layout that rebuilds the model would reset
    /// its zoom without proving anything about resizing.
    static var modelsMade = 0
    /// Frames the viewport drew zoomed without a pinch or menu zoom asking:
    /// open and re-layout must draw Fit from the first frame, not correct
    /// themselves a frame later.
    static var unrequestedZoomFrames = 0
    fileprivate func noteDrawnZoom(width: CGFloat) {
        guard zoomApplies(to: CGSize(width: width, height: 1)) else { return }
        if zoomScale > 1, !zoomRequested { Self.unrequestedZoomFrames += 1 }
    }
    private func installRecoveryFixture() {
        guard fixtureRecoveryTask == nil else { return }
        fixtureRecoveryTask = Task { @MainActor [weak self] in
            var handled: Set<String> = []
            var revision: UInt64 = 10_000
            while !Task.isCancelled {
                guard let self, !self.ended else { return }
                let dockSignals = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-dock")
                    ? ["dock-take-control", "dock-open-recent", "dock-refuse-open", "dock-keyboard", "dock-trackpad", "dock-hold-switcher", "dock-latch-modifier", "dock-open-again", "dock-persistent-trackpad",
                       "dock-switcher-open-1", "dock-switcher-open-2", "dock-switcher-open-3", "dock-switcher-latch", "dock-switcher-latch-2",
                       "dock-switcher-close", "dock-type-a", "dock-dictate", "dock-dictate-finish", "dock-zoom", "dock-key-escape", "dock-keyboard-hide"] : []
                for name in ["background", "foreground", "network-change", "brief-disconnect", "host-restart", "recovery-ready"] + dockSignals {
                    guard !handled.contains(name), DiagnosticSubagentFixture.signalled("computer-" + name) else { continue }
                    handled.insert(name)
                    revision += 1
                    switch name {
                    case "dock-take-control": self.takeControl()
                    case "dock-hold-switcher", "dock-switcher-open-1", "dock-switcher-open-2", "dock-switcher-open-3",
                         "dock-switcher-close": self.toggleAppSwitcher()
                    // The switcher key tapped twice quickly: ⌘ stays held.
                    case "dock-switcher-latch", "dock-switcher-latch-2": self.toggleAppSwitcher(); self.toggleAppSwitcher()
                    case "dock-type-a": self.sendKeyboardActions([.text("a")])
                    case "dock-key-escape": self.sendKey("escape")
                    // Cupola's own dictation through the real controller and editor, no microphone.
                    case "dock-dictate":
                        let controller = self.dictationController
                        controller.attachEditor(self.dictationEditor, conversationID: self.chat.id)
                        if let pending = controller.beginNativeFixture(chat: self.chat) {
                            controller.receiveNativeFixture("hello from dictation", pending: pending)
                        }
                    case "dock-dictate-finish": self.dictationController.finishCapture(submit: false)
                    case "dock-keyboard-hide": self.selectBottomInput(.off)
                    // Once per launch: a layout change can rebuild the model, and
                    // the replayed signal would zoom the new layout too.
                    case "dock-zoom":
                        if !Self.zoomSignalUsed { Self.zoomSignalUsed = true; self.magnify(2) }
                    case "dock-latch-modifier": self.toggleModifier("shift")
                    case "dock-open-recent", "dock-refuse-open", "dock-open-again":
                        if let app = self.miniDock.recent.first { await self.openDockApp(app) }
                    case "dock-keyboard": self.selectBottomInput(.keyboard)
                    case "dock-trackpad": self.selectBottomInput(.trackpad)
                    case "dock-persistent-trackpad": self.selectBottomInput(.trackpad, persistentTrackpad: true)
                    case "background": await self.pause()
                    case "foreground":
                        Task { @MainActor [weak self] in await self?.resumeAfterBackground() }
                    case "host-restart":
                        self.receiver.onStateChange?(.failed("The connection to your computer closed. Try again to reopen the view."), revision)
                    case "recovery-ready":
                        await self.connectionRestored()
                        self.fixtureReadyProcessed = true
                        if self.failure == nil, !self.isLoading { self.receiver.onStateChange?(.live, revision) }
                    default:
                        self.connectionInterrupted()
                        self.receiver.onStateChange?(.retrying(ComputerReceiverState.interruptedMessage), revision)
                    }
                }
                try? await Task.sleep(for: .milliseconds(50))
            }
        }
    }

    private static var fixtureFailedOnce = false
    /// The host's own reason when no media provider is configured (crates/wonderd/src/computer_sessions.rs).
    private static let fixtureUnavailableReason =
        "Live computer viewing is unavailable on this computer. Update Cupola when a configured media provider is available."

    private static func fixture(conversationID: String, available: Bool,
                                window: ComputerViewTarget? = nil) -> ComputerSession {
        if case .simulator(let simulator) = window, available { return simulatorFixture(conversationID: conversationID, simulator) }
        let source = available ? window.map {
            ComputerSource(id: $0.source.id, name: $0.displayName, kind: $0.source.kind, width: 1280, height: 800, scale: 1)
        } ?? ComputerSource(
            id: "display:1", name: "Main display", kind: "display", width: 1920, height: 1080, scale: 1
        ) : .unknown
        let control = available ? ComputerControlCapability(
            available: true, action: "none", reason: "Computer control is disabled on your computer. Run cupola doctor there to check its setup."
        ) : .unavailable
        return ComputerSession(
            id: "fixture-computer-session",
            clientRequestId: "fixture-computer-request",
            ownerDeviceId: "fixture-device",
            hostInstallationId: "fixture-host",
            conversationId: conversationID,
            generation: 1,
            state: available ? .live : .unavailable,
            source: source,
            geometryRevision: available ? 1 : 0,
            failureReason: available ? nil : Self.fixtureUnavailableReason,
            createdAt: "2026-09-12T00:00:00Z",
            updatedAt: "2026-09-12T00:00:00Z",
            lastStateAt: "2026-09-12T00:00:00Z",
            endedAt: nil,
            capability: available ? ComputerCapability(available: true, action: "none", reason: "Live computer viewing is available on this computer.") : ComputerCapability(available: false, action: "update-host", reason: Self.fixtureUnavailableReason),
            control: control,
            videoQuality: .auto,
            supportsLiveQualityChange: true,
            supportsControlSourceRebind: true,
            videoMaxFramesPerSecond: 30
        )
    }

    /// A live simulator at its native size (an iPad or an iPhone 17 Pro).
    /// `-diagnostics-simulator-view-only` gives the host's view-only answer;
    /// `-diagnostics-simulator-stopped` a simulator that stopped.
    private static func simulatorFixture(conversationID: String, _ simulator: ComputerSource) -> ComputerSession {
        let arguments = ProcessInfo.processInfo.arguments
        let iPad = simulator.name?.contains("iPad") == true
        let source = ComputerSource(id: simulator.id, name: simulator.name, kind: "simulator",
                                    width: iPad ? 1640 : 1206, height: iPad ? 2360 : 2622, scale: 1,
                                    crop: ComputerCrop(x: 0, y: 0, width: iPad ? 1640 : 1206, height: iPad ? 2360 : 2622))
        let stopped = arguments.contains("-diagnostics-simulator-stopped")
        let control = arguments.contains("-diagnostics-simulator-view-only")
            ? ComputerControlCapability(available: false, action: "none",
                                        reason: "This simulator can be viewed but not controlled with this version of Xcode.")
            : ComputerControlCapability(available: true, action: "none", reason: "Take control to use this simulator from here.")
        return ComputerSession(
            id: "fixture-simulator-session", clientRequestId: "fixture-simulator-request", ownerDeviceId: "fixture-device",
            hostInstallationId: "fixture-host", conversationId: conversationID, generation: 1,
            state: stopped ? .ended : .live, source: source, geometryRevision: 1,
            failureReason: stopped ? "This simulator has stopped." : nil,
            createdAt: "2026-10-25T00:00:00Z", updatedAt: "2026-10-25T00:00:00Z", lastStateAt: "2026-10-25T00:00:00Z",
            endedAt: stopped ? "2026-10-25T00:00:00Z" : nil,
            capability: ComputerCapability(available: true, action: "none", reason: "Live computer viewing is available on this computer."),
            control: control, videoQuality: .auto, supportsLiveQualityChange: true, supportsControlSourceRebind: false,
            videoMaxFramesPerSecond: 30)
    }

    private static func fixtureLease(session: ComputerSession) -> ComputerControlLease {
        ComputerControlLease(
            id: "fixture-control-lease",
            sessionId: session.id,
            ownerDeviceId: "fixture-device",
            hostInstallationId: "fixture-host",
            conversationId: session.conversationId,
            generation: session.generation,
            sourceId: session.source.id,
            geometryRevision: session.geometryRevision,
            status: "active",
            lastSequence: 0,
            acquiredAt: "2026-09-12T00:00:00Z",
            updatedAt: "2026-09-12T00:00:00Z",
            expiresAt: "2099-01-01T00:00:00Z",
            releasedAt: nil
        )
    }

    #endif
}

private struct AuthenticatedComputerSignalingTransport: ComputerSignalingTransport {
    let api: PairingAPI
    let connection: SavedConnection
    let clipboardSubscription: @MainActor @Sendable () -> ComputerClipboardSubscription
    let receiveClipboard: @MainActor @Sendable (ComputerClipboardPollResponse, ComputerClipboardSubscription) -> Void

    func poll(sessionID: String, request: ComputerSignalingPollRequest) async throws -> ComputerSignalingResponse {
        let subscription = await clipboardSubscription()
        let response: ComputerClipboardPollResponse = try await api.request(
            "/api/v1/computer/sessions/\(escape(sessionID))/signaling", origin: connection.origin,
            body: JSONEncoder().encode(ComputerClipboardPollRequest(signaling: request, clipboard: subscription)),
            credential: connection.credential)
        try Task.checkCancellation()
        await receiveClipboard(response, subscription)
        return response.signaling
    }

    func submitAnswer(sessionID: String, request: ComputerSignalingAnswerRequest) async throws -> ComputerSignalingMutationResponse {
        try await api.request("/api/v1/computer/sessions/\(escape(sessionID))/signaling/answer",
                              origin: connection.origin,
                              body: JSONEncoder().encode(request), credential: connection.credential)
    }

    func sendCandidate(sessionID: String, request: ComputerSignalingCandidateRequest) async throws -> ComputerSignalingMutationResponse {
        try await api.request("/api/v1/computer/sessions/\(escape(sessionID))/signaling/candidate",
                              origin: connection.origin,
                              body: JSONEncoder().encode(request), credential: connection.credential)
    }

    func close(sessionID: String, request: ComputerSignalingCloseRequest) async throws -> ComputerSignalingMutationResponse {
        try await api.request("/api/v1/computer/sessions/\(escape(sessionID))/signaling/close",
                              origin: connection.origin,
                              body: JSONEncoder().encode(request), credential: connection.credential)
    }

    func endedReason(sessionID: String, generation: UInt64) async -> String? {
        guard let session: ComputerSession = try? await api.request(
            "/api/v1/computer/sessions/\(escape(sessionID))?generation=\(generation)",
            origin: connection.origin, credential: connection.credential),
              session.id == sessionID, session.generation == generation,
              [.ended, .failed, .unavailable].contains(session.state) else { return nil }
        return session.failureReason
    }

    private func escape(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? value
    }
}

/// An agent working in the conversation that opened the computer view.
struct ComputerInstalledApp: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let name: String

    static let fixture: [ComputerInstalledApp] = [
        .init(id: "com.apple.ActivityMonitor", name: "Activity Monitor"),
        .init(id: "com.apple.calculator", name: "Calculator"),
        .init(id: "com.apple.iCal", name: "Calendar"),
        .init(id: "com.apple.Notes", name: "Notes"),
        .init(id: "com.apple.Safari", name: "Safari"),
        .init(id: "com.apple.Terminal", name: "Terminal"),
        .init(id: "com.microsoft.VSCode", name: "Visual Studio Code"),
    ]

    /// Case-insensitive name search, in the list's order (by name).
    static func matching(_ query: String, in apps: [ComputerInstalledApp]) -> [ComputerInstalledApp] {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty else { return apps }
        return apps.filter { $0.name.localizedCaseInsensitiveContains(query) }
    }
}

struct ComputerMiniDockContent: Decodable, Equatable, Sendable {
    struct App: Decodable, Identifiable, Equatable, Sendable {
        let id: String
        let name: String
        let running: Bool
        var iconURL: String? = nil
    }
    let available: Bool
    let pinned: [App]
    let recent: [App]
    static let unavailable = Self(available: false, pinned: [], recent: [])

    #if WONDER_DIAGNOSTICS
    static func fixture(platform: String?) -> Self {
        let names: [(String, String)] = switch platform {
        case "windows": [("explorer", "File Explorer"), ("msedge", "Microsoft Edge"), ("notepad", "Notepad"), ("terminal", "Terminal")]
        case "linux": [("org.gnome.Nautilus.desktop", "Files"), ("firefox.desktop", "Firefox"), ("org.gnome.TextEditor.desktop", "Text Editor"), ("org.gnome.Terminal.desktop", "Terminal")]
        default: [("com.apple.finder", "Finder"), ("com.apple.Safari", "Safari"), ("com.apple.Notes", "Notes"), ("com.apple.Terminal", "Terminal")]
        }
        let apps = names.enumerated().map { App(id: $0.element.0, name: $0.element.1, running: $0.offset != 2) }
        return Self(available: true, pinned: Array(apps.prefix(3)), recent: Array(apps.suffix(1)))
    }
    #endif
}

struct ComputerAppLauncherFailure: Error, LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

private struct ComputerInstalledAppList: Decodable, Sendable {
    let available: Bool
    let apps: [ComputerInstalledApp]
    let reason: String?
}

private struct ComputerInstalledAppIcon: Decodable, Sendable { let iconPNG: String }

private struct ComputerOpenAppRequest: Encodable {
    let leaseId: String
    let generation: UInt64
    let geometryRevision: UInt64
    let sourceId: String?
    let bundleId: String
}

private struct ComputerOpenAppResponse: Decodable, Sendable {
    let opened: Bool?
    let reason: String?
    let lease: ComputerControlLease?
}

struct ComputerAgentPresence: Equatable {
    let name: String
    /// The agent's running turn is using the computer right now.
    let isActive: Bool
    /// A computer tool call is in flight: the agent holds the screen at this moment.
    var isActing = false
}

struct ComputerSessionView: View {
    @EnvironmentObject private var library: ConnectionLibrary
    let model: ConnectionModel
    let chat: ChatSummary
    var onDialogGone: () -> Void = {}
    var agent: ComputerAgentPresence? = nil
    /// Whether you control the computer now; false again when control ends,
    /// fails or the view goes away.
    var onControlChanged: (Bool) -> Void = { _ in }
    @State private var selectedModel: ConnectionModel?

    var body: some View {
        computerView(selectedModel ?? model)
    }

    private func computerView(_ currentModel: ConnectionModel) -> some View {
        let selectedChat = currentModel === model ? chat : ComputerSessionView.hostViewChat
        return ComputerConnectionSessionView(model: currentModel, chat: selectedChat, library: library,
                                      agent: currentModel === model ? agent : nil,
                                      onDialogGone: onDialogGone,
                                      controlChanged: onControlChanged) { saved in
            guard let current = library.saved.connections.first(where: {
                $0.credential.hostInstallationId == saved.credential.hostInstallationId
                    && $0.credential.deviceId == saved.credential.deviceId && !$0.requiresPairing
            }) else { return }
            selectedModel = library.model(for: current)
        }
        // A new host/device owns a new receiver and input lease. Chat and draft
        // selection in the presenting screen stay with their original Mac.
        .id(currentModel.connection.map { $0.credential.hostInstallationId + ":" + $0.credential.deviceId } ?? "fixture")
    }

    /// The conversation a view of another computer belongs to: that computer's own host view.
    static let hostViewChat = ChatSummary(
        conversationId: "wonder-host-view", title: "Computer", lastMessagePreview: nil,
        lastMessageAt: nil, messageCount: 0, deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false
    )
}

/// The conversation owns the rail; its computer pane lends it the live session.
/// Owner identity prevents an outgoing pane from clearing a replacement session.
@MainActor final class ComputerRail: ObservableObject {
    @Published private(set) var session: ComputerSessionModel?
    @Published private(set) var agentActive = false
    private var owner: UUID?

    func show(_ session: ComputerSessionModel, owner: UUID, agentActive: Bool) {
        self.owner = owner
        if self.session !== session { self.session = session }
        if self.agentActive != agentActive { self.agentActive = agentActive }
    }
    func clear(owner: UUID) {
        guard self.owner == owner else { return }
        self.owner = nil
        session = nil
        agentActive = false
    }
}

extension EnvironmentValues {
    @Entry var computerRail: ComputerRail? = nil
}

/// Which computers' viewers are on screen. A Mac dialog banner over the chat
/// repeats what the open viewer already shows, so it waits until the viewer closes.
@MainActor final class ComputerViewerPresence: ObservableObject {
    static let shared = ComputerViewerPresence()
    @Published private(set) var hosts: Set<String> = []
    private var counts: [String: Int] = [:]
    func appeared(host: String) {
        counts[host, default: 0] += 1
        hosts.insert(host)
    }
    func disappeared(host: String) {
        guard let count = counts[host] else { return }
        if count <= 1 { counts[host] = nil; hosts.remove(host) } else { counts[host] = count - 1 }
    }
}

private struct ComputerConnectionSessionView: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    @ObservedObject var library: ConnectionLibrary
    let agent: ComputerAgentPresence?
    let selectConnection: (SavedConnection) -> Void
    let onDialogGone: () -> Void
    let controlChanged: (Bool) -> Void
    private let dismiss = ToolDismiss()
    @Environment(\.wonderTheme) private var theme
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @StateObject private var sessionModel: ComputerSessionModel
    private var presenceHost: String { model.connection?.credential.hostInstallationId ?? "" }
    /// The host counted at onAppear, so onDisappear releases that same host
    /// even if the connection changed in between.
    @State private var presentHost: String?
    /// The iPhone Duo's hinge; nil before its first report and on other devices.
    @State private var hinge: ComputerFoldLayout.Hinge?
    @State private var laptopPose = false
    @Environment(\.companionPane) private var pane
    /// The Duo's system side bar, which takes the place of the navigation bar.
    @State private var verticalBar = false
    /// Above the fold, and in a side pane, the picture sits right under the
    /// header; everywhere else it is centred and the floating title row uses its letterbox.
    private var anchorsPictureTop: Bool { laptopPose || showsPad || pane != nil }
    /// The separate trackpad under the picture: asked for, while controlling.
    /// (The half-folded Duo has its own below the fold.)
    private var showsPad: Bool {
        sessionModel.showsTrackpad && sessionModel.trackpadRoom && sessionModel.isControlActive && !laptopPose
            && !sessionModel.isClosed && !sessionModel.dialogGone && landscape == nil
    }
    /// The landscape layout in use, set from the window's shape (see `landscapeKind`).
    @State private var landscape: ComputerLandscape?
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    @AppStorage(ComputerLandscapePreference.padLayoutKey) private var experimentalPadLayout = false
    private var padLayoutEnabled: Bool {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-ipad-layout") { return true }
        #endif
        return experimentalPadLayout
    }
    /// A phone turned sideways (short window), or a wide, regular-size window
    /// with the experimental iPad layout on, while you control. The Duo's
    /// poses keep their own layouts, and a window that is not wider than tall
    /// stays as it is.
    private func poseMode(_ kind: ComputerLandscape?, resolved: ComputerViewerLayout.Mode) -> ComputerViewerLayout.Mode {
        switch kind {
        case .phone: return .phoneLandscape
        case .pad: return .padLandscape
        case nil: return resolved == .side ? .stacked : resolved
        }
    }

    private func landscapeKind(size: CGSize, folded: Bool) -> ComputerLandscape? {
        guard !folded, hinge == nil, !verticalBar, size.width > size.height else { return nil }
        if verticalSizeClass == .compact { return .phone }
        if padLayoutEnabled, horizontalSizeClass == .regular, sessionModel.isControlActive,
           size.width >= ComputerViewerLayout.minimumPadLandscapeWidth, size.width >= size.height * 1.3 {
            return .pad
        }
        return nil
    }
    /// Beside the Duo's system bar the controls are items in that bar, so the
    /// picture keeps the height a slab under it would take, folded or flat open
    /// (a slab would cross the hinge). The laptop pose keeps its trackpad below the fold.
    /// Full screen flat open keeps its own centred controls over the cover.
    private var controlsInBar: Bool {
        verticalBar && !laptopPose && !(fullScreen && hinge == .fullyOpen)
    }
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @State private var trackpadRoomBeforeFullScreen: Bool?
    /// The Duo's picture on the whole screen, over both halves.
    #if WONDER_DIAGNOSTICS
    /// Native Duo taps are not drivable (#387): a scenario opens the cover by argument.
    @State private var fullScreen = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-full-screen")
    #else
    @State private var fullScreen = false
    #endif
    @Environment(\.computerRail) private var rail
    @State private var railOwner = UUID()
    private var lendsControlsToRail: Bool {
        controlsInBar && pane != nil && !fullScreen && !sessionModel.isClosed && !sessionModel.dialogGone
    }
    #if WONDER_DIAGNOSTICS
    /// How often the picture's view was built: a pose change must not rebuild it.
    @State private var viewportAppearances = 0
    /// The simulated fold opens flat on `wonderctl signal computer-fold-open`
    /// and half-folds again on `computer-fold-close`, so a scenario changes pose
    /// mid-session on any simulator.
    @State private var simulatedFoldOpen = false
    #endif

    init(model: ConnectionModel, chat: ChatSummary, library: ConnectionLibrary,
         agent: ComputerAgentPresence?, onDialogGone: @escaping () -> Void,
         controlChanged: @escaping (Bool) -> Void,
         selectConnection: @escaping (SavedConnection) -> Void) {
        self.model = model
        self.controlChanged = controlChanged
        self.chat = chat
        self.library = library
        self.agent = agent
        self.onDialogGone = onDialogGone
        self.selectConnection = selectConnection
        // Inside the autoclosure, so a parent update does not build (and discard) a model.
        _sessionModel = StateObject(wrappedValue: ComputerSessionModel(
            model: model, chat: chat))
    }

    var body: some View {
        viewerLayout
            .task(id: scenePhase == .active && !model.accessEnded ? presenceHost : nil) {
                guard scenePhase == .active, !model.accessEnded else { return }
                while !Task.isCancelled {
                    await sessionModel.refreshMiniDock()
                    do { try await Task.sleep(for: .seconds(5)) } catch { return }
                }
            }
            .background(theme.computerBackground.ignoresSafeArea())
            .modifier(ComputerBarToolbar(shows: controlsInBar && !fullScreen && (pane == nil || rail == nil),
                                         model: sessionModel, agentActive: agent?.isActive == true, close: closeViewer))
            // The Duo's bar takes the navigation bar's place beside the
            // content, and its items show only while that is not hidden.
            // Elsewhere the header row is the only bar.
            .toolbar(controlsInBar && (pane == nil || rail == nil) ? .visible : .hidden, for: .navigationBar)
            // No bar background or hairline under the Duo's bar items either.
            .toolbarBackgroundVisibility(.hidden, for: .navigationBar)
            .modifier(ComputerSessionLifetime(model: model, session: sessionModel, chatID: chat.id, pausesInBackground: true) {
                await handleLifetimeEnd()
            })
            .onChange(of: lendsControlsToRail, initial: true) { _, lends in
                if lends { rail?.show(sessionModel, owner: railOwner, agentActive: agent?.isActive == true) }
                else { rail?.clear(owner: railOwner) }
            }
            .onChange(of: agent?.isActive) { _, _ in
                if lendsControlsToRail { rail?.show(sessionModel, owner: railOwner, agentActive: agent?.isActive == true) }
            }
            .onDisappear { rail?.clear(owner: railOwner) }
            .onAppear {
                let host = presenceHost
                presentHost = host
                ComputerViewerPresence.shared.appeared(host: host)
            }
            .onDisappear {
                if let host = presentHost { ComputerViewerPresence.shared.disappeared(host: host) }
                presentHost = nil
            }
            // A different computer opened in the same viewer moves the count.
            .onChange(of: presenceHost) { _, host in
                guard let old = presentHost, old != host else { return }
                ComputerViewerPresence.shared.disappeared(host: old)
                ComputerViewerPresence.shared.appeared(host: host)
                presentHost = host
            }
            .onChange(of: sessionModel.dialogGone) { _, gone in if gone { onDialogGone() } }
            .onChange(of: laptopPose) { _, _ in sessionModel.postureChanged() }
            .onChange(of: hinge) { _, _ in sessionModel.postureChanged() }
            // With a hardware keyboard, typing and ⌘ shortcuts go to the Mac as
            // soon as control starts; no on-screen keyboard appears.
            .onChange(of: sessionModel.isControlActive, initial: true) { _, active in
                controlChanged(active)
                if !active { sessionModel.cancelDictation() }
                if active { sessionModel.restoreBottomInput() }
                if active, ComputerSessionModel.hardwareKeyboardTyping { sessionModel.keyboardPresented = true }
            }
            .onDisappear { controlChanged(false); sessionModel.cancelDictation() }
            .onChange(of: scenePhase) { _, phase in if phase != .active { sessionModel.cancelDictation() } }
            .onReceive(NotificationCenter.default.publisher(for: .GCKeyboardDidConnect)) { _ in
                sessionModel.releaseHeldInput(.keyboardHidden)
                if sessionModel.isControlActive, ComputerSessionModel.hardwareKeyboardTyping { sessionModel.keyboardPresented = true }
                sessionModel.objectWillChange.send()
            }
            .onReceive(NotificationCenter.default.publisher(for: .GCKeyboardDidDisconnect)) { _ in
                sessionModel.releaseHeldInput(.keyboardHidden)
                sessionModel.objectWillChange.send()
            }
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("computer-session-container")
            #if WONDER_DIAGNOSTICS
            .background {
                DiagnosticDriverSurface(surface: .init(priority: 20, state: {
                    ["computer": ["sourceID": sessionModel.session?.source.id ?? "",
                                  "sourceKind": sessionModel.session?.source.kind ?? "",
                                  "dialogGone": sessionModel.dialogGone,
                                  "hasPicture": sessionModel.receiver.picture.hasFrame,
                                  "controlling": sessionModel.isControlActive,
                                  "dockAvailable": sessionModel.miniDock.available,
                                  "dockPinned": sessionModel.miniDock.pinned.map(\.id),
                                  "dockRecent": sessionModel.miniDock.recent.map(\.id),
                                  "openedApps": sessionModel.openedAppIDs,
                                  "typedText": sessionModel.fixtureTypedText,
                                  "dictation": sessionModel.dictationController.recording ? "recording" : "idle",
                                  "keyboardPresented": sessionModel.keyboardPresented,
                                  "optionalTrackpad": sessionModel.showsTrackpad,
                                  "appSwitcher": sessionModel.isAppSwitcherPresented,
                                  "switcherLatched": sessionModel.isSwitcherLatched,
                                  "zoomScale": Double(sessionModel.zoomScale),
                                  "frameRateChoice": sessionModel.frameRateChoice.storageValue,
                                  "frameRateOptions": sessionModel.frameRateChoices.map(\.title),
                                  "frameRateAsked": sessionModel.frameRateRequest.rate,
                                  "modelsMade": ComputerSessionModel.modelsMade,
                                  "unrequestedZoomFrames": ComputerSessionModel.unrequestedZoomFrames,
                                  "viewport": "\(Int(sessionModel.viewportSizeForState.width))x\(Int(sessionModel.viewportSizeForState.height))",
                                  "heldKeys": sessionModel.fixtureHeldKeys,
                                  "pressModifiers": sessionModel.fixturePressModifiers,
                                  "clipboardOperations": sessionModel.fixtureClipboardOperations,
                                  "heldModifiers": sessionModel.modifierLatch.keys.sorted()]]
                }, controls: {
                    let count = PushNotifications.shared.macDialogs[model.connection?.credential.hostInstallationId ?? ""]?.count ?? 0
                    var controls: [DiagnosticTestDriver.Control] = []
                    if count > 0 {
                        controls.append(.init(id: "computer-session-dialogs", label: "Mac dialog waiting",
                            value: MacDialogNotice.countLabel(count),
                            enabled: !sessionModel.isClosed && !sessionModel.isLoading
                                && !sessionModel.isOpeningDialog,
                            tap: { Task { await sessionModel.showMacDialog() } }))
                    }
                    // The Duo input canary cannot prove coordinate taps (#387).
                    // Semantic checks invoke the same actions as the visible controls.
                    if let notice = sessionModel.notice, notice.canRetry {
                        if sessionModel.viewerBusy != nil {
                            controls.append(.init(id: "computer-session-take-over", label: "Take over",
                                enabled: !sessionModel.isLoading, tap: { Task { await sessionModel.takeOver() } }))
                            controls.append(.init(id: "computer-session-alternative", label: "Try again",
                                enabled: !sessionModel.isLoading, tap: { Task { await sessionModel.retry() } }))
                        } else {
                            controls.append(.init(id: "computer-session-retry", label: "Try again",
                                enabled: !sessionModel.isLoading, tap: { Task { await sessionModel.retry() } }))
                        }
                    }
                    if sessionModel.isControlActive {
                        controls.append(.init(id: "computer-session-done", label: "Give control back",
                            tap: { Task { await sessionModel.done() } }))
                    } else if sessionModel.canTakeControl {
                        controls.append(.init(id: "computer-session-take-control", label: "Take control",
                            tap: { sessionModel.takeControl() }))
                    }
                    // In a companion pane the visible Computer pill is its exit.
                    // Elsewhere the viewer owns Close. Both use the native exit.
                    controls.append(.init(id: pane == nil && !controlsInBar ? "computer-session-close" : "computer-status-pill",
                        label: pane == nil && !controlsInBar ? "Close" : "View computer", value: "Open", tap: closeViewer))
                    return controls
                }, actions: ["dismiss": { _ in closeViewer() }])).frame(width: 0, height: 0).accessibilityHidden(true)
            }
            #endif
    }

    /// The controls and whatever the pose puts after them: the portrait slot,
    /// or the landscape poses' named strip and columns.
    @ViewBuilder
    private func viewerControls(kind: ComputerLandscape?, mode: ComputerViewerLayout.Mode) -> some View {
        if mode.isLaptop {
            // Below the fold: trackpad and keys; nothing on the fold.
            ComputerTrackpadArea(model: sessionModel, agent: agent)
        } else if controlsInBar {
            // Only what the computer says (a failure, the clipboard);
            // the controls are in the bar.
            ComputerSessionControls(model: sessionModel, agent: agent, controlsInBar: true)
        } else {
            let sideControls = mode == .side
            ComputerSessionControls(model: sessionModel, agent: agent, sidePlacement: sideControls,
                                    stickyModifiers: showsPad && !sideControls,
                                    togglesTrackpad: kind != .pad,
                                    composerShaped: kind == nil,
                                    bottomMargin: pane?.panelBottomMargin ?? ToolPanelMetrics.bottomMargin,
                                    floating: kind == .phone)
                .frame(width: sideControls ? 104 : nil)
                .frame(maxWidth: sideControls ? nil : .infinity,
                       maxHeight: sideControls ? .infinity : nil, alignment: .top)
        }
        if kind == .pad {
            ComputerTrackpadSurface(model: sessionModel, namesItself: true)
                .background(theme.computerBackground)
                .layoutValue(key: ComputerViewerLayout.SlotKey.self, value: .trackpadColumn)
        } else if kind == nil && showsPad && !mode.isLaptop {
            ComputerTrackpadPanel(model: sessionModel)
        }
    }

    @ViewBuilder
    private func viewerPose(in geometry: GeometryProxy) -> some View {
        let fold = ComputerFoldLayout.resolve(size: geometry.size, hinge: hinge, fold: fold(in: geometry))
        let open = !sessionModel.isClosed && !sessionModel.dialogGone
        let resolved = ComputerViewerLayout.Mode(fold, open: open)
        let kind = open && !fullScreen ? landscapeKind(size: geometry.size, folded: resolved.isLaptop) : nil
        // With the controls in the bar, what the computer says spans the
        // picture's width under it instead of a column beside it.
        let mode = poseMode(kind, resolved: resolved)
        // One tree for every pose: a hinge change moves the header and the
        // picture, never rebuilds them (the video view, the hidden keyboard
        // and its IME text, an in-flight drag).
        ComputerViewerLayout(mode: mode,
                             hasPad: kind == nil && showsPad && !mode.isLaptop,
                             aspect: ComputerSessionModel.sourceAspectRatio(sessionModel.displaySession),
                             anchorsTop: laptopPose || pane != nil || (showsPad && !mode.isLaptop)) {
            // Keep the picture and sole native keyboard mounted while the
            // cover owns the visible header/controls. UIKit's AX snapshot
            // includes the presenting view even with accessibilityHidden.
            Group {
                if fullScreen { Color.clear.frame(height: 0) }
                else { header(floating: mode == .stacked || mode == .phoneLandscape) }
            }
            .zIndex(1)
            computerContent
            if open && !fullScreen {
                viewerControls(kind: kind, mode: mode)
            }
        }
        .background(theme.computerBackground)
        // The keyboard's text view sits outside the controls, which differ
        // by pose, so a hinge change keeps its focus and IME composition.
        // The shortcut row lives in the controls row, so it stays without the keyboard.
        .background(alignment: .bottomLeading) {
            ComputerNativeKeyboard(
                presented: sessionModel.presentsSystemKeyboard,
                resetID: sessionModel.keyboardRevision,
                hostReference: sessionModel.hostReference,
                onActions: { sessionModel.sendKeyboardActions($0) },
                onDismiss: { sessionModel.keyboardPresented = false }
            )
            .frame(width: 1, height: 1)
            .clipped()
        }
        .onChange(of: mode.isLaptop, initial: true) { _, laptop in if laptopPose != laptop { laptopPose = laptop } }
        .onChange(of: kind, initial: true) { _, kind in
            if landscape != kind { landscape = kind }
            sessionModel.setPadColumn(kind == .pad)
        }
        // Turning the phone or resizing the window ends a held drag and the latches too.
        .onChange(of: mode) { _, _ in sessionModel.postureChanged() }
        // A phone turned sideways has no height for a separate pad, but the
        // picture itself becomes the trackpad, so the toggle stays.
        .onChange(of: kind == .phone || ComputerViewerLayout.fitsPad(height: geometry.size.height, controlsInBar: controlsInBar), initial: true) { _, room in
            sessionModel.setTrackpadRoom(room)
        }
        .onChange(of: anchorsPictureTop, initial: true) { _, anchors in sessionModel.setAnchorsPictureTop(anchors) }
    }

    private var viewerLayout: some View {
        GeometryReader { geometry in viewerPose(in: geometry) }
        .readsVerticalBar($verticalBar)
        .modifier(ReleasesOnTurn { sessionModel.postureChanged() })
        // In the laptop pose the keyboard covers the trackpad instead of
        // squeezing the picture above the fold.
        .ignoresSafeArea(laptopPose ? .keyboard : [], edges: .bottom)
        // Unfolded, the pane reaches the window's bottom so the panel can end
        // where the composer beside it does.
        .ignoresSafeArea(.container, edges: pane?.alignsPanelToComposer == true ? .bottom : [])
        .modifier(DuoHingeReader(hinge: $hinge))
        #if WONDER_DIAGNOSTICS
        .overlay(alignment: .topLeading) {
            if !fullScreen { viewportProbe(laptop: laptopPose) }
        }
        .task {
            guard Self.simulatesFold else { return }
            while !Task.isCancelled {
                let open = DiagnosticSubagentFixture.signalled("computer-fold-open")
                    && !DiagnosticSubagentFixture.signalled("computer-fold-close")
                if simulatedFoldOpen != open { simulatedFoldOpen = open }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
        #endif
        .sheet(isPresented: Binding(
            get: { sessionModel.appLauncherPresented && !fullScreen },
            set: { sessionModel.appLauncherPresented = $0 }
        )) {
            ComputerAppLauncher(model: sessionModel)
        }
    }

    /// The fold across the inner display of an iPhone Duo (iOS 27.1), in this
    /// view's coordinates. Inactive while the phone lies flat; absent elsewhere.
    #if WONDER_DIAGNOSTICS
    /// The native keyboard stays on the presenting layout, including in full screen.
    private func viewportProbe(laptop: Bool) -> some View {
        let value: String = "appearances: \(viewportAppearances); keyboards: \(ComputerNativeKeyboard.made); laptop: \(laptop); top: \(sessionModel.anchorsPictureTop); trackpad: \(sessionModel.showsTrackpad); releases: \(sessionModel.heldInputReleases); sent: \(sessionModel.fixtureSentInput.count); hardware: \(ComputerSessionModel.hardwareKeyboardTyping); saved: \(sessionModel.savedBottomInput); queue: \(sessionModel.inputQueueMaximumDepth); landscape: \(landscape.map { $0 == .phone ? "phone" : "pad" } ?? "none");"
        return Color.clear.frame(width: 1, height: 1)
            .accessibilityElement()
            .accessibilityIdentifier("computer-viewport-appearances")
            .accessibilityValue(Text(verbatim: value))
    }
    #endif

    private func fold(in geometry: GeometryProxy) -> ComputerFoldLayout.Fold? {
        #if WONDER_DIAGNOSTICS
        // A simulated half-fold across the middle, so the laptop layout can be
        // checked on any simulator. The Duo simulator reports the real one.
        if Self.simulatesFold {
            let size = geometry.size
            return .init(frame: CGRect(x: 0, y: (size.height - 40) / 2, width: size.width, height: 40),
                         isActive: !simulatedFoldOpen)
        }
        #endif
        return DuoFold.region(in: geometry)
    }

    #if WONDER_DIAGNOSTICS
    private static var simulatesFold: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-fold") }
    #endif


    /// The title row. Stacked, it floats over the picture's letterbox: which
    /// computer (the title, switching in its menu), what needs you (Mac
    /// dialogs) and who controls stay put, and so do Close and the window buttons.
    private func header(floating: Bool, balance: EdgeInsets = EdgeInsets()) -> some View {
        let title = ComputerSessionHeader(model: model, library: library, session: sessionModel.session,
                                          receiverState: sessionModel.receiverState,
                                          liveStatus: sessionModel.liveStatus,
                                          liveDetail: sessionModel.receiverState == .live && sessionModel.streamQuality != .good
                                              ? sessionModel.streamQuality.detail : nil) { saved in
            if let saved, saved.credential.hostInstallationId != model.connection?.credential.hostInstallationId {
                await closeComputer()
                guard !Task.isCancelled else { return }
                selectConnection(saved)
            }
        }
        let dialogs = MacDialogViewerChip(host: model.connection?.credential.hostInstallationId ?? "",
                                          enabled: !sessionModel.isClosed && !sessionModel.isLoading
                                              && !sessionModel.isOpeningDialog) {
            Task { await sessionModel.showMacDialog() }
        }
        .id(model.connection?.credential.hostInstallationId ?? "")
        // Close sits first, the computer picker in the middle, More and the
        // window size last. All of them stay put: nothing fades.
        let windowButtons = HStack(spacing: 0) {
            moreMenu
            if fullScreen {
                Button { fullScreen = false } label: {
                    Image(systemName: "arrow.down.right.and.arrow.up.left")
                        .dynamicTypeSize(...DynamicTypeSize.xLarge)
                        .frame(width: 44, height: 44).contentShape(Rectangle())
                }
                .accessibilityLabel("Exit full screen")
                .accessibilityIdentifier("computer-session-exit-full-screen")
            }
            if !fullScreen, pane?.onDisplayWithFold == true {
                // The Duo's pane stays on its side of the fold, so the picture
                // gets the whole screen as its own cover instead.
                Button { fullScreen = true } label: {
                    Label("Full screen", systemImage: "arrow.up.left.and.arrow.down.right").labelStyle(.iconOnly)
                        .dynamicTypeSize(...DynamicTypeSize.xLarge)
                        .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
                }
                .accessibilityIdentifier("computer-session-full-screen")
            }
            if !fullScreen { CompanionExpandButton(inHeader: true) }
        }
        .fixedSize()
        .padding(.horizontal, 4)
        .background(.regularMaterial, in: Capsule())
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("computer-session-window-buttons")
        return ComputerHeaderBarLayout(spacing: 8) {
            if !fullScreen && !controlsInBar {
                ToolCloseControl {
                    ComputerCloseButton(action: closeViewer)
                        .padding(.horizontal, 4)
                        .background(.regularMaterial, in: Capsule())
                }
            }
            dialogs
            title.layoutValue(key: ComputerHeaderBarLayout.Title.self, value: true)
            windowButtons
        }
        .buttonStyle(.plain)
        .foregroundStyle(theme.computerForeground)
        // Beside the composer the pills end on the control panel's edges; a
        // little in from the screen edge so a thumb reaches them (a Duo's
        // rounded corners and side bar sit right there).
        .padding(.horizontal, pane != nil ? 16 : 12)
        .padding(.vertical, 6)
        .padding(balance)
        .modifier(DisplayHeaderCorners(enabled: verticalBar || hinge != nil || pane?.onDisplayWithFold == true))
        .background(floating ? Color.clear : theme.computerBackground)
    }

    /// Everything the picture's window needs that is not who controls: one menu.
    private var moreMenu: some View {
        Menu {
            // The bar has no clipboard button: both directions live here.
            if sessionModel.isControlActive {
                ComputerClipboardActions(model: sessionModel)
            }
            if controlsInBar && !fullScreen && sessionModel.keyboardPresented && sessionModel.trackpadOffered {
                Button(sessionModel.showsTrackpad ? "Hide trackpad" : "Show trackpad") { sessionModel.toggleTrackpad() }
                    .accessibilityIdentifier("computer-session-trackpad-toggle")
            }
            Button("Refresh", systemImage: "arrow.clockwise") {
                Task {
                    if sessionModel.failure != nil { await sessionModel.retry() }
                    else { await sessionModel.refresh() }
                }
            }
            .disabled(sessionModel.isLoading || sessionModel.isClosed || sessionModel.isFixture)
            .accessibilityIdentifier("computer-session-refresh")
            Toggle(sessionModel.canShareMacClipboard ? "Share Mac clipboard" : "Mac clipboard sharing unavailable", isOn: Binding(
                get: { sessionModel.clipboardSharingEnabled },
                set: { sessionModel.setClipboardSharing($0) }
            ))
            .disabled(!sessionModel.canShareMacClipboard)
            .accessibilityHint(sessionModel.canShareMacClipboard
                ? "Off by default. Share new text copies during this view."
                : "Clipboard sharing requires a Mac.")
            .accessibilityIdentifier("computer-session-share-clipboard")
            if !sessionModel.isViewOnly {
                Picker("Pointer mode", selection: $sessionModel.inputMode) {
                    ForEach(ComputerInputMode.allCases) { mode in
                        Text(mode.title).tag(mode)
                    }
                }
                .pickerStyle(.inline)
            }
            if let session = sessionModel.session {
                if session.videoQuality != nil {
                    Picker("Video quality", selection: Binding(
                        get: { sessionModel.videoQuality },
                        set: { sessionModel.setVideoQuality($0) }
                    )) {
                        ForEach(ComputerVideoQuality.allCases) { quality in
                            Text(quality.title).tag(quality)
                        }
                    }
                    .disabled(sessionModel.isLoading)
                    .accessibilityIdentifier("computer-session-video-quality")
                    if sessionModel.supportsFrameRateChoice {
                        Picker(sessionModel.frameRateRowTitle, selection: Binding(
                            get: { sessionModel.shownFrameRateChoice },
                            set: { sessionModel.setFrameRate($0) }
                        )) {
                            ForEach(sessionModel.frameRateChoices, id: \.self) { choice in
                                Text(choice.title).tag(choice)
                            }
                        }
                        .pickerStyle(.menu)
                        .accessibilityValue(sessionModel.frameRateLimitNote ?? "")
                        .accessibilityIdentifier("computer-session-frame-rate")
                    }
                } else {
                    Button(sessionModel.videoQualityUnavailableMessage) { }
                        .disabled(true)
                }
            }
            Button("Recenter pointer", systemImage: "scope") { sessionModel.recenterPointer() }
                .disabled(!sessionModel.isControlActive)
                .accessibilityIdentifier("computer-session-recenter")
            Section("View") {
                Button("Fit", systemImage: "arrow.down.right.and.arrow.up.left") { sessionModel.resetZoom() }
                    .accessibilityIdentifier("computer-session-fit")
                Button("Zoom in", systemImage: "plus.magnifyingglass") { sessionModel.magnify(sessionModel.zoomScale + 0.25) }
                    .accessibilityIdentifier("computer-session-zoom-in")
                Button("Zoom out", systemImage: "minus.magnifyingglass") { sessionModel.magnify(sessionModel.zoomScale - 0.25) }
                    .accessibilityIdentifier("computer-session-zoom-out")
            }
            Menu("Keys", systemImage: "keyboard") {
                ForEach(["Escape", "Tab", "Return", "Delete", "Left", "Right", "Up", "Down"], id: \.self) { key in
                    Button(key) { sessionModel.sendKey(key.lowercased()) }
                        .accessibilityIdentifier("computer-session-key-\(key.lowercased())")
                }
            }
            .disabled(!sessionModel.isControlActive)
            .accessibilityIdentifier("computer-session-keys")
        } label: {
            Image(systemName: "ellipsis")
                .dynamicTypeSize(...DynamicTypeSize.xLarge)
                .frame(width: 44, height: 44)
                .contentShape(Rectangle())
        }
        .accessibilityLabel("More, zoom \(Int((sessionModel.zoomScale * 100).rounded())) percent")
        .accessibilityValue("\(Int((sessionModel.zoomScale * 100).rounded())) percent")
        .accessibilityIdentifier("computer-session-more")
    }

    private var computerContent: some View {
        let tint = ComputerControlChip.tint(model: sessionModel, agent: agent)
        return ComputerViewport(model: sessionModel, sizesModel: !fullScreen)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            // Who controls: the viewport's edge takes the holder's colour.
            .overlay {
                if let tint {
                    Rectangle().strokeBorder(tint, lineWidth: 3)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
            }
            #if WONDER_DIAGNOSTICS
            .onAppear { viewportAppearances += 1 }
            // Native Duo taps are not drivable (#387): signals stand in for the
            // Full screen button and the cover's exit, which resize the picture.
            .task {
                var covered = false, uncovered = false
                while !Task.isCancelled {
                    if !covered, DiagnosticSubagentFixture.signalled("computer-dock-cover") { covered = true; fullScreen = true }
                    if !uncovered, DiagnosticSubagentFixture.signalled("computer-dock-uncover") { uncovered = true; fullScreen = false }
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
            #endif
            .fullScreenCover(isPresented: $fullScreen) {
                fullScreenViewer
            }
            .overlay(alignment: .bottomLeading) {
                if let message = sessionModel.qualityMessage ?? sessionModel.frameRateNote {
                    Text(message)
                        .font(.footnote)
                        .foregroundStyle(theme.computerForeground)
                        .padding(.horizontal, 10).padding(.vertical, 6)
                        .background(.regularMaterial, in: Capsule())
                        .padding(8)
                        .accessibilityIdentifier("computer-session-quality-message")
                }
            }
    }

    private var fullScreenViewer: some View {
        GeometryReader { geometry in
            let fold = ComputerFoldLayout.resolve(size: geometry.size, hinge: hinge, fold: fold(in: geometry))
            let resolved = ComputerViewerLayout.Mode(fold, open: true)
            let mode: ComputerViewerLayout.Mode = resolved == .side ? .stacked : resolved
            let roomForPad = ComputerViewerLayout.fitsPad(height: geometry.size.height, controlsInBar: false)
            let pad = sessionModel.isControlActive && sessionModel.showsTrackpad && roomForPad && !mode.isLaptop
            // The system overlays take more of one side (the Duo's status area
            // at the trailing edge), which would pull the header and controls
            // off the screen's centre: give the other side the same room.
            let inset = geometry.safeAreaInsets
            let balance = EdgeInsets(top: 0, leading: max(0, inset.trailing - inset.leading), bottom: 0,
                                     trailing: max(0, inset.leading - inset.trailing))
            ComputerViewerLayout(mode: mode, hasPad: pad,
                                 aspect: ComputerSessionModel.sourceAspectRatio(sessionModel.displaySession),
                                 anchorsTop: mode.isLaptop || pad) {
                header(floating: false, balance: balance).zIndex(1)
                ComputerViewport(model: sessionModel)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .overlay {
                        if let tint = ComputerControlChip.tint(model: sessionModel, agent: agent) {
                            Rectangle().strokeBorder(tint, lineWidth: 3).allowsHitTesting(false).accessibilityHidden(true)
                        }
                    }
                if mode.isLaptop {
                    ComputerTrackpadArea(model: sessionModel, agent: agent)
                } else {
                    let sideControls = mode == .side
                    ComputerSessionControls(model: sessionModel, agent: agent, sidePlacement: sideControls, stickyModifiers: pad && !sideControls,
                                            balance: balance)
                        .frame(width: sideControls ? 104 : nil)
                        .frame(maxWidth: sideControls ? nil : .infinity, maxHeight: sideControls ? .infinity : nil, alignment: .top)
                }
                if pad { ComputerTrackpadPanel(model: sessionModel) }
            }
            .background(theme.computerBackground)
            #if WONDER_DIAGNOSTICS
            .overlay(alignment: .topLeading) { viewportProbe(laptop: mode.isLaptop) }
            #endif
            .onAppear {
                sessionModel.releaseHeldInput(.poseChanged)
                trackpadRoomBeforeFullScreen = sessionModel.trackpadRoom
                sessionModel.setTrackpadRoom(roomForPad)
            }
            .onChange(of: roomForPad) { _, room in sessionModel.setTrackpadRoom(room) }
            .sheet(isPresented: $sessionModel.appLauncherPresented) {
                ComputerAppLauncher(model: sessionModel)
            }
            .onDisappear {
                sessionModel.releaseHeldInput(.poseChanged)
                if let room = trackpadRoomBeforeFullScreen { sessionModel.setTrackpadRoom(room) }
                trackpadRoomBeforeFullScreen = nil
            }
        }
    }

    private func handleLifetimeEnd() async {
        await closeComputer()
        dismiss()
    }

    private func closeComputer() async {
        await sessionModel.close()
    }

    private func closeViewer() {
        Task { await closeComputer(); dismiss() }
    }
}

/// A computer or simulator view's session lives only as long as its screen:
/// it starts when shown and closes when the view goes away, the app moves to
/// the background, access to the computer ends, or the computer or
/// conversation changes underneath it (`end` closes and dismisses). A lost
/// connection releases control; a restored one reconnects the picture.
struct ComputerSessionLifetime: ViewModifier {
    @ObservedObject var model: ConnectionModel
    let session: ComputerSessionModel
    let chatID: String
    /// Leaving Cupola pauses the view (control ends, sharing stops, the window
    /// stays) and returning reopens it, instead of ending it. Ended access and
    /// a different computer still end it.
    var pausesInBackground = false
    let end: () async -> Void
    @Environment(\.scenePhase) private var scenePhase
    /// Leaving and returning run strictly in order, however quickly they follow.
    @State private var phaseChange: Task<Void, Never>?

    func body(content: Content) -> some View {
        content
            .task { await session.start() }
            .onDisappear { Task { await session.close(keepingLastFrame: true) } }
            .onChange(of: scenePhase) { _, phase in
                // Losing focus (a call, the app switcher) lets go of held keys and buttons.
                if phase == .inactive { session.releaseHeldInput(.focusLost) }
                let previous = phaseChange
                if phase == .background {
                    phaseChange = Task {
                        await previous?.value
                        if pausesInBackground { await session.pause() } else { await end() }
                    }
                } else if phase == .active, pausesInBackground, !model.accessEnded {
                    phaseChange = Task {
                        await previous?.value
                        await session.resumeAfterBackground()
                    }
                }
            }
            .onChange(of: model.accessEnded) { _, ended in
                if ended { Task { await end() } }
            }
            .onChange(of: model.macConnected) { _, connected in
                if connected == false { session.connectionInterrupted() }
                else if connected == true { Task { await session.connectionRestored() } }
            }
            .onChange(of: model.connection?.credential.hostInstallationId) { _, _ in Task { await end() } }
            .onChange(of: chatID) { _, _ in Task { await end() } }
    }
}

/// Turning the device from one landscape side to the other keeps the same
/// layout, but the keys move: whatever the keys hold goes up first. Only the
/// window's own landscape-left/right flip counts; tilting the device flat,
/// turning it on a rotation-locked phone or on the portrait-locked Duo does
/// not move the keys, so it must not drop a held drag or latch.
private struct ReleasesOnTurn: ViewModifier {
    let turned: () -> Void
    @State private var side = LandscapeSide()

    func body(content: Content) -> some View {
        content
            .onAppear {
                UIDevice.current.beginGeneratingDeviceOrientationNotifications()
                side.last = LandscapeSide.current()
            }
            .onDisappear { UIDevice.current.endGeneratingDeviceOrientationNotifications() }
            .onReceive(NotificationCenter.default.publisher(for: UIDevice.orientationDidChangeNotification)) { _ in
                // The window can take a moment to follow the device.
                check()
                Task { @MainActor in
                    try? await Task.sleep(for: .milliseconds(400))
                    check()
                }
            }
    }

    private func check() {
        let now = LandscapeSide.current()
        defer { if now != nil { side.last = now } }
        if let last = side.last, let now, last != now { turned() }
    }
}

/// The side the interface is turned to while it is landscape, else nil.
private final class LandscapeSide {
    var last: UIInterfaceOrientation?

    @MainActor static func current() -> UIInterfaceOrientation? {
        let scene = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .first { $0.activationState == .foregroundActive }
        guard let orientation = scene?.effectiveGeometry.interfaceOrientation, orientation.isLandscape else { return nil }
        return orientation
    }
}

/// The computer view's header, picture and controls in every pose. Children:
/// the header, the picture, then optionally the controls (or, in the laptop
/// pose, the trackpad). Only frames change between poses, so the header and
/// picture keep their identity across a hinge change.
private struct ComputerViewerLayout: Layout {
    enum Mode: Equatable {
        /// Controls in a row below the picture.
        case stacked
        /// Controls in a column beside the picture.
        case side
        /// Header and picture above the fold, controls below it; the fold band
        /// between them stays empty.
        case laptop(foldTop: CGFloat, foldBottom: CGFloat)
        /// A phone turned sideways: the picture fills the screen; the header
        /// and controls float along its top.
        case phoneLandscape
        /// A wide iPad window: trackpad column, picture and controls.
        case padLandscape

        init(_ fold: ComputerFoldLayout, open: Bool) {
            switch fold {
            case let .laptop(_, band, _) where open: self = .laptop(foldTop: band.minY, foldBottom: band.maxY)
            case .side: self = .side
            default: self = .stacked
            }
        }

        var isLaptop: Bool {
            if case .laptop = self { return true }
            return false
        }
    }

    /// What a child after the controls is for. Portrait poses use one `pad`
    /// child; the iPad landscape pose names its trackpad column.
    enum Slot: Equatable { case pad, trackpadColumn }
    struct SlotKey: LayoutValueKey { static let defaultValue = Slot.pad }

    var mode: Mode
    /// A fourth child, the separate trackpad, sits under the picture and above
    /// the keys (or the keyboard). It takes what the picture does not need.
    var hasPad = false
    /// The least height the trackpad gets; the picture gives way before it.
    static let minimumPad: CGFloat = 88
    /// The trackpad column of the iPad landscape pose.
    static func landscapePadColumnWidth(in width: CGFloat) -> CGFloat { min(236, width * 0.2) }
    /// The narrowest window that keeps a usable picture between two columns.
    static let minimumPadLandscapeWidth: CGFloat = 960
    /// The window height that holds a header, a usable picture, the controls and
    /// the trackpad; with the controls in the Duo's bar they take no room.
    static func fitsPad(height: CGFloat, controlsInBar: Bool) -> Bool { height >= (controlsInBar ? 320 : 480) }
    /// The shared screen's shape, and whether it hugs the top of its area.
    /// Stacked, the title row floats over the picture's letterbox and moves
    /// the picture down only by what the letterbox cannot absorb.
    var aspect: CGFloat = 16 / 9
    var anchorsTop = false

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        proposal.replacingUnspecifiedDimensions()
    }

    /// How far the picture must start below the top so the floating row never covers it.
    private func floatingReserve(headerHeight: CGFloat, in size: CGSize) -> CGFloat {
        guard size.width > 0, size.height > 0, aspect > 0 else { return 0 }
        if anchorsTop { return headerHeight }
        let gap = (size.height - min(size.height, size.width / aspect)) / 2
        return 2 * max(0, headerHeight - gap)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        guard subviews.count >= 2 else { return }
        let header = subviews[0], picture = subviews[1]
        let controls = subviews.count > 2 ? subviews[2] : nil
        let pad = hasPad && subviews.count > 3 ? subviews[3] : nil
        func slot(_ wanted: Slot) -> LayoutSubview? { subviews.dropFirst(3).first { $0[SlotKey.self] == wanted } }
        let headerHeight = header.sizeThatFits(ProposedViewSize(width: bounds.width, height: nil)).height
        header.place(at: bounds.origin, proposal: ProposedViewSize(width: bounds.width, height: headerHeight))
        let pictureTop = bounds.minY + headerHeight
        func place(_ view: LayoutSubview, _ frame: CGRect) {
            view.place(at: frame.origin, proposal: ProposedViewSize(width: max(0, frame.width), height: max(0, frame.height)))
        }
        switch mode {
        case let .laptop(foldTop, foldBottom):
            place(picture, CGRect(x: bounds.minX, y: pictureTop, width: bounds.width, height: bounds.minY + foldTop - pictureTop))
            if let controls {
                place(controls, CGRect(x: bounds.minX, y: bounds.minY + foldBottom,
                                       width: bounds.width, height: bounds.maxY - bounds.minY - foldBottom))
            }
        case .phoneLandscape:
            // The picture owns the whole screen; the controls float along its top.
            place(picture, bounds)
            if let controls {
                let height = controls.sizeThatFits(ProposedViewSize(width: bounds.width, height: nil)).height
                place(controls, CGRect(x: bounds.minX, y: pictureTop, width: bounds.width, height: height))
            }
        case .padLandscape:
            let areaHeight = max(0, bounds.maxY - pictureTop)
            let padView = slot(.trackpadColumn)
            let padWidth = padView == nil ? 0 : Self.landscapePadColumnWidth(in: bounds.width)
            let middle = CGRect(x: bounds.minX + padWidth, y: pictureTop,
                                width: max(0, bounds.width - padWidth), height: areaHeight)
            if let padView {
                place(padView, CGRect(x: bounds.minX, y: pictureTop, width: padWidth, height: areaHeight))
            }
            // Under the picture: the controls, centred on the screen. The
            // column makes the middle's centre differ from the screen's.
            var bottom = middle.maxY
            if let controls {
                let centred = Self.centredSpan(of: middle, in: bounds)
                let height = min(middle.height, controls.sizeThatFits(ProposedViewSize(width: centred.width, height: nil)).height)
                bottom -= height
                place(controls, CGRect(x: centred.minX, y: bottom, width: centred.width, height: height))
            }
            place(picture, CGRect(x: middle.minX, y: middle.minY, width: middle.width, height: max(0, bottom - middle.minY)))
        case .side:
            let width = controls?.sizeThatFits(ProposedViewSize(width: nil, height: bounds.maxY - pictureTop)).width ?? 0
            let columnWidth = bounds.width - width
            let padHeight = pad == nil ? 0
                : padHeight(available: bounds.maxY - pictureTop, picture: columnWidth / max(aspect, 0.1), cap: 0.4)
            place(picture, CGRect(x: bounds.minX, y: pictureTop, width: columnWidth, height: bounds.maxY - pictureTop - padHeight))
            if let pad {
                place(pad, CGRect(x: bounds.minX, y: bounds.maxY - padHeight, width: columnWidth, height: padHeight))
            }
            if let controls {
                place(controls, CGRect(x: bounds.maxX - width, y: pictureTop, width: width, height: bounds.maxY - pictureTop))
            }
        case .stacked:
            let height = controls?.sizeThatFits(ProposedViewSize(width: bounds.width, height: nil)).height ?? 0
            let reserveFor = { (area: CGSize) in min(floatingReserve(headerHeight: headerHeight, in: area), max(0, area.height)) }
            // The picture keeps the height it needs at this width; the trackpad gets the rest.
            let available = bounds.height - height - reserveFor(CGSize(width: bounds.width, height: bounds.height - height))
            let padHeight = pad == nil ? 0
                : padHeight(available: available, picture: bounds.width / max(aspect, 0.1), cap: 1)
            let area = CGSize(width: bounds.width, height: bounds.height - height - padHeight)
            let reserve = reserveFor(area)
            place(picture, CGRect(x: bounds.minX, y: bounds.minY + reserve, width: bounds.width, height: area.height - reserve))
            // Picture, then the keys, then the trackpad: it ends up right above the keyboard.
            if let controls {
                place(controls, CGRect(x: bounds.minX, y: bounds.minY + area.height, width: bounds.width, height: height))
            }
            if let pad {
                place(pad, CGRect(x: bounds.minX, y: bounds.maxY - padHeight, width: bounds.width, height: padHeight))
            }
        }
    }

    /// The widest part of `column` that is centred on `bounds` (the whole of it when it already is).
    static func centredSpan(of column: CGRect, in bounds: CGRect) -> CGRect {
        let half = max(0, min(bounds.midX - column.minX, column.maxX - bounds.midX))
        return CGRect(x: bounds.midX - half, y: column.minY, width: 2 * half, height: column.height)
    }

    /// The trackpad's height: what is left once the picture has the height its
    /// width gives it, never under `minimumPad`, and at most `cap` of the room.
    private func padHeight(available: CGFloat, picture: CGFloat, cap: CGFloat) -> CGFloat {
        guard available > 0 else { return 0 }
        return min(max(available - picture, Self.minimumPad), max(Self.minimumPad, available * cap), available)
    }
}

/// The computer view's top bar: leading buttons, the computer name (marked
/// with `Title`), then trailing buttons. The name is centred on the bar when
/// both button groups leave it room; otherwise it moves only as far as needed
/// and is offered the space between them, so a long or large-text name
/// truncates instead of overlapping a button.
struct ComputerHeaderBarLayout: Layout {
    enum Title: LayoutValueKey { static let defaultValue = false }
    var spacing: CGFloat
    /// The narrowest title worth truncating to keep it centred; a name that
    /// would need less moves off centre instead, so it stays readable.
    static let minimumCentredTitle: CGFloat = 100

    private func arrange(width: CGFloat?, subviews: Subviews) -> (frames: [CGRect], size: CGSize) {
        let sizes = subviews.map { $0.sizeThatFits(.unspecified) }
        let titleIndex = subviews.firstIndex { $0[Title.self] }
        // Hidden controls report no width and take no spacing.
        func span(_ range: Range<Int>) -> CGFloat {
            range.filter { $0 != titleIndex && sizes[$0].width > 0 }.reduce(CGFloat(0)) { $0 + sizes[$1].width + spacing }
        }
        let leadingWidth = span(0..<(titleIndex ?? subviews.count))
        let trailingWidth = titleIndex.map { span(($0 + 1)..<subviews.count) } ?? 0
        let titleIdeal = titleIndex.map { sizes[$0].width } ?? 0
        let barWidth = width ?? (leadingWidth + titleIdeal + trailingWidth)
        var titleSize = CGSize.zero
        var titleX: CGFloat = leadingWidth
        if let titleIndex {
            let room = max(0, barWidth - leadingWidth - trailingWidth)
            // A long name truncates to stay centred when the room either side
            // of the bar's middle allows a usable title; only a bar too
            // crowded for that moves it off centre.
            let centredRoom = max(0, barWidth - 2 * max(leadingWidth, trailingWidth))
            let offered = min(titleIdeal, room) > centredRoom && centredRoom >= Self.minimumCentredTitle
                ? centredRoom : min(titleIdeal, room)
            titleSize = subviews[titleIndex].sizeThatFits(ProposedViewSize(width: offered, height: nil))
            titleSize.width = min(titleSize.width, offered)
            titleX = min(max((barWidth - titleSize.width) / 2, leadingWidth), barWidth - trailingWidth - titleSize.width)
        }
        let height = max(titleSize.height, sizes.enumerated().filter { $0.offset != titleIndex }.map(\.element.height).max() ?? 0)
        var frames: [CGRect] = []
        var leadingX: CGFloat = 0
        var trailingX = barWidth - trailingWidth + spacing
        for (index, size) in sizes.enumerated() {
            if index == titleIndex {
                frames.append(CGRect(x: titleX, y: (height - titleSize.height) / 2, width: titleSize.width, height: titleSize.height))
            } else if titleIndex.map({ index > $0 }) ?? false {
                frames.append(CGRect(x: trailingX, y: (height - size.height) / 2, width: size.width, height: size.height))
                if size.width > 0 { trailingX += size.width + spacing }
            } else {
                frames.append(CGRect(x: leadingX, y: (height - size.height) / 2, width: size.width, height: size.height))
                if size.width > 0 { leadingX += size.width + spacing }
            }
        }
        return (frames, CGSize(width: barWidth, height: height))
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        arrange(width: proposal.width, subviews: subviews).size
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let frames = arrange(width: bounds.width, subviews: subviews).frames
        for (subview, frame) in zip(subviews, frames) {
            subview.place(at: CGPoint(x: bounds.minX + frame.minX, y: bounds.minY + frame.minY),
                          proposal: ProposedViewSize(width: frame.width, height: frame.height))
        }
    }
}

/// Below the fold: the usual controls, a key row with latching modifiers, and
/// the trackpad (tap to click, two fingers to right-click or scroll, hold to drag).
/// The lower screen is about as tall as the keyboard, so with the keyboard up
/// it keeps just the trackpad above the keys, and the controls return when it goes.
private struct ComputerTrackpadArea: View {
    @ObservedObject var model: ComputerSessionModel
    let agent: ComputerAgentPresence?
    @Environment(\.wonderTheme) private var theme
    @State private var areaBottom: CGFloat = 0
    @State private var areaHeight: CGFloat = 0
    @State private var keyboardTop: CGFloat?
    /// How far the keyboard rises into this area, in points, leaving the pad its
    /// least height: a keyboard with its shortcut row is taller than the lower
    /// half, and padding the whole area away made the layout pass never finish.
    private var keyboardOverlap: CGFloat {
        guard let keyboardTop else { return 0 }
        return max(0, min(areaBottom - keyboardTop, areaHeight - ComputerViewerLayout.minimumPad))
    }

    var body: some View { padWithControls }

    private var overlaidPad: some View {
        ComputerTrackpadSurface(model: model)
        .overlay(alignment: .topTrailing) {
            // The controls are covered, but the keyboard's own button and
            // the trackpad's stay: using the pad puts the keyboard away.
            HStack(spacing: 0) {
                Button { model.selectBottomInput(.trackpad, persistentTrackpad: true) } label: {
                    Image(systemName: "rectangle.and.hand.point.up.left")
                        .foregroundStyle(theme.accent)
                        .frame(width: 44, height: 44)
                        .contentShape(Rectangle())
                }
                .accessibilityLabel("Use trackpad")
                .accessibilityIdentifier("computer-session-trackpad-toggle")
                Button { model.toggleKeyboard() } label: {
                    Image(systemName: "keyboard.chevron.compact.down")
                        .foregroundStyle(theme.accent)
                        .frame(width: 44, height: 44)
                        .contentShape(Rectangle())
                }
                .accessibilityLabel("Hide keyboard")
                .accessibilityIdentifier("computer-session-keyboard")
            }
            .buttonStyle(.plain)
            .padding(.top, 8).padding(.trailing, 12)
        }
    }

    private var padWithControls: some View {
        ViewThatFits(in: .vertical) {
            VStack(spacing: 0) {
                ComputerSessionControls(model: model, agent: agent, stickyModifiers: true, togglesTrackpad: false)
                ComputerTrackpadSurface(model: model).frame(minHeight: ComputerViewerLayout.minimumPad)
            }
            // Too short for the controls: the shortcut row (Esc, app keys) stays above the
            // pad, since the keyboard no longer carries it.
            VStack(spacing: 0) {
                if model.isControlActive { ComputerKeyboardShortcuts(model: model) }
                overlaidPad
            }
            overlaidPad
        }
        .padding(.bottom, keyboardOverlap)
        // The area fills the room it is given, so its bottom is the slot's, not
        // the content's plus the padding it is measured to set: measuring a
        // size that grows with the overlap re-lays out without end once the
        // keyboard is up (the app hangs at full CPU).
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(theme.computerBackground)
        .background {
            GeometryReader { geometry in
                Color.clear
                    .onChange(of: geometry.frame(in: .global).maxY, initial: true) { _, bottom in areaBottom = bottom }
                    .onChange(of: geometry.size.height, initial: true) { _, height in areaHeight = height }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillChangeFrameNotification)) { note in
            guard let frame = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect else { return }
            keyboardTop = frame.minY < UIScreen.main.bounds.height ? frame.minY : nil
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            keyboardTop = nil
        }
    }
}

/// The trackpad under the picture and above the keyboard, in the layouts that
/// are not the half-folded Duo. The keys sit above it, so with the keyboard up
/// it is the part of the screen next to the keyboard.
private struct ComputerTrackpadPanel: View {
    @ObservedObject var model: ComputerSessionModel
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        ComputerTrackpadSurface(model: model)
            .background(theme.computerBackground, ignoresSafeAreaEdges: .bottom)
    }
}

/// The pad itself: touches go through `ComputerTrackpadGestures`.
private struct ComputerTrackpadSurface: View {
    @ObservedObject var model: ComputerSessionModel
    var verticalPadding: CGFloat = 8
    /// A pad on its own, with nothing around it to say what it is, keeps its name showing.
    var namesItself = false
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(theme.surface)
            if !model.isControlActive {
                Text(model.controlAvailable ? "Take control to use the trackpad." : "Trackpad")
                    .font(.footnote)
                    .foregroundStyle(theme.secondaryText)
                    .accessibilityHidden(true)
            } else if namesItself {
                VStack(spacing: 6) {
                    Image(systemName: "rectangle.and.hand.point.up.left")
                        .font(.title2)
                    Text("Trackpad")
                        .font(.footnote)
                }
                .foregroundStyle(theme.secondaryText.opacity(0.7))
                .accessibilityHidden(true)
            }
            ComputerTrackpadPad(allowsInput: model.isControlActive) { model.trackpad($0) }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, verticalPadding)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Trackpad")
        .accessibilityHint(model.isControlActive
            ? "Drag to move the pointer. Tap to click, tap with two fingers to right-click, drag two fingers to scroll, hold to drag."
            : "Take control to use the trackpad.")
        .accessibilityAddTraits(.allowsDirectInteraction)
        .accessibilityAction(named: Text("Click")) { model.trackpad(.click(button: "left")) }
        .accessibilityAction(named: Text("Right-click")) { model.trackpad(.click(button: "right")) }
        .accessibilityIdentifier("computer-session-trackpad")
    }
}

/// Sill-style launcher: search the computer's installed apps and open one.
struct ComputerAppLauncher: View {
    @ObservedObject var model: ComputerSessionModel
    @Environment(\.dismiss) private var dismiss
    @Environment(\.wonderTheme) private var theme
    @State private var apps: [ComputerInstalledApp] = []
    @State private var loading = true
    @State private var loadFailure: String?
    @State private var query = ""
    @State private var opening: String?
    @State private var openFailure: String?

    var body: some View {
        NavigationStack {
            WonderList {
                if let openFailure {
                    Text(openFailure)
                        .font(.footnote)
                        .foregroundStyle(theme.secondaryText)
                        .accessibilityIdentifier("computer-apps-open-failure")
                }
                if loading {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("Reading apps on \(model.hostShortName)…").font(.footnote)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("computer-apps-loading")
                } else if let loadFailure {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(loadFailure).font(.footnote)
                        Button("Try again") { Task { await load() } }
                            .accessibilityIdentifier("computer-apps-retry")
                    }
                    .accessibilityIdentifier("computer-apps-failure")
                } else {
                    let shown = ComputerInstalledApp.matching(query, in: apps)
                    if shown.isEmpty {
                        Text(query.isEmpty ? "No apps found on \(model.hostShortName)." : "No apps match “\(query)”.")
                            .font(.footnote)
                            .foregroundStyle(theme.secondaryText)
                            .accessibilityIdentifier("computer-apps-empty")
                    }
                    ForEach(shown) { app in
                        Button { Task { await open(app) } } label: {
                            HStack(spacing: 12) {
                                ComputerAppIcon(model: model, id: app.id)
                                Text(app.name).foregroundStyle(theme.primaryText)
                                Spacer(minLength: 0)
                                if opening == app.id { ProgressView().controlSize(.small) }
                            }
                            .contentShape(Rectangle())
                        }
                        .disabled(opening != nil)
                        .accessibilityLabel("Open \(app.name)")
                        .accessibilityIdentifier("computer-app-\(app.id)")
                    }
                }
            }
            .searchable(text: $query, placement: .navigationBarDrawer(displayMode: .always),
                        prompt: "Search apps on \(model.hostShortName)")
            .navigationTitle("Apps")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                        .accessibilityIdentifier("computer-apps-close")
                }
            }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
        .accessibilityIdentifier("computer-apps")
        .task { await load() }
    }

    private func load() async {
        loading = true
        loadFailure = nil
        do {
            apps = try await model.installedApps()
        } catch PairingFailure.response(404) {
            // A computer from before the app list: open Spotlight's Applications instead.
            dismiss()
            model.openSpotlightApplications()
        } catch is CancellationError {
        } catch let failure as ComputerAppLauncherFailure {
            loadFailure = failure.message
        } catch {
            loadFailure = "Could not read the apps on \(model.hostShortName). Check the connection and try again."
        }
        loading = false
    }

    private func open(_ app: ComputerInstalledApp) async {
        opening = app.id
        openFailure = nil
        let failure = await model.openApp(app)
        opening = nil
        if let failure { openFailure = failure } else { dismiss() }
    }
}

private struct ComputerAppIcon: View {
    let model: ComputerSessionModel
    let id: String
    @State private var image: UIImage?

    var body: some View {
        Group {
            if let image {
                Image(uiImage: image).resizable()
            } else {
                Image(systemName: "app.dashed").resizable().foregroundStyle(.secondary).padding(4)
            }
        }
        .frame(width: 32, height: 32)
        .accessibilityHidden(true)
        .task(id: id) { image = await model.installedAppIcon(id) }
    }
}

private struct ComputerSessionHeader: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ConnectionLibrary
    let session: ComputerSession?
    let receiverState: ComputerReceiverState
    let liveStatus: String
    let liveDetail: String?
    let selectSource: (SavedConnection?) async -> Void
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @State private var connectionMenu: ConnectionMenu?
    @State private var menuContentHeight: CGFloat = 220
    @State private var switching = false
    @State private var requestedSource: SourceRequest?
    @State private var captureAvailability: [String: Bool] = [:]
    @State private var discoveries: [String: ComputerWindowMenuDiscovery] = [:]

    private func discovery(for connection: ConnectionModel) -> Binding<ComputerWindowMenuDiscovery> {
        let key = connection.connection?.credential.hostInstallationId ?? "current"
        return Binding(get: { discoveries[key] ?? ComputerWindowMenuDiscovery() }, set: { discoveries[key] = $0 })
    }

    /// Prepare models and present them atomically. Separate state writes can
    /// present an iPad popover with the previous (empty) connection collection.
    private struct ConnectionMenu: Identifiable {
        let id = UUID()
        let candidates: [String: ConnectionModel]
    }
    private struct SourceRequest: Identifiable {
        let id = UUID()
        let connection: SavedConnection?
    }
    private var windowName: String? { session?.source.kind == "window" ? session?.source.name : nil }
    private var statusTitle: String {
        let mac = model.accessEnded ? "Access ended" : model.macConnected == true ? "Connected" : model.macConnected == false ? "Offline" : "Connecting"
        let screen = session?.state == .unavailable ? "screen unavailable" : "screen \(liveStatus.lowercased())"
        return [mac, windowName, screen, liveDetail].compactMap { $0 }.joined(separator: ", ")
    }
    private var statusLine: String {
        guard session?.state != .unavailable else { return "Unavailable" }
        return windowName.map { "\($0) · \(liveStatus)" } ?? liveStatus
    }
    private var sourceDescription: String {
        guard let source = session?.source, let name = source.name ?? source.kind else { return "Source not selected" }
        if let width = source.width, let height = source.height { return "\(name) · \(width) × \(height)" }
        return name
    }
    private var otherConnections: [SavedConnection] {
        library.saved.connections.filter { $0.credential.hostInstallationId != model.connection?.credential.hostInstallationId }
    }
    private func request(_ saved: SavedConnection?) {
        connectionMenu = nil
        switching = true
        requestedSource = SourceRequest(connection: saved)
    }

    private func openSourceMenu() {
        captureAvailability = [:]
        let candidates = Dictionary(uniqueKeysWithValues: otherConnections.map {
            ($0.credential.hostInstallationId, library.model(for: $0))
        })
        let current = model.connection?.credential.hostInstallationId ?? "current"
        discoveries = discoveries.filter { $0.key == current || candidates[$0.key] != nil }
        connectionMenu = ConnectionMenu(candidates: candidates)
    }

    var body: some View {
        HStack {
            Button(action: openSourceMenu) {
                // The computer is the title: its glyph and name lead, the window
                // (when one is shown) and the link's state sit under it.
                HStack(spacing: 8) {
                    Image(systemName: ConceptSymbol.computer)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 0) {
                        Text(model.macName).lineLimit(dynamicTypeSize.isAccessibilitySize ? 2 : 1)
                        Text(statusLine).font(.caption2)
                            .foregroundStyle(liveDetail == nil ? Color.secondary : Color.orange).lineLimit(1)
                            .accessibilityIdentifier("computer-session-live-status")
                    }
                    Circle()
                        .fill(model.macConnected == true && !model.accessEnded ? Color.green : Color.secondary.opacity(0.6))
                        .frame(width: 8, height: 8).accessibilityHidden(true)
                    if switching { ProgressView().controlSize(.small) }
                    else { Image(systemName: "chevron.down").font(.caption.weight(.semibold)) }
                }
                // Bounded so the header leaves the picture room in short windows.
                .dynamicTypeSize(...DynamicTypeSize.accessibility3)
                .padding(.horizontal, 12).padding(.vertical, 4)
                .frame(minHeight: 44)
                .background(.regularMaterial, in: Capsule())
                .contentShape(Capsule())
            }
            .font(.footnote.weight(.semibold)).foregroundStyle(.primary).buttonStyle(.plain)
            .disabled(switching)
            .accessibilityLabel("Computer, \(model.macName)")
            .accessibilityValue(statusTitle)
            .accessibilityIdentifier("computer-session-connection-picker")
            .pillPopover(item: $connectionMenu) { menu in
                ScrollView {
                    VStack(alignment: .leading, spacing: 8) {
                        ComputerConnectionChoices(model: model, discovery: discovery(for: model), selected: true,
                                                  reportCapture: { captureAvailability["current"] = $0 }) {
                            connectionMenu = nil
                        }
                        ForEach(otherConnections, id: \.credential.hostInstallationId) { saved in
                            if let candidate = menu.candidates[saved.credential.hostInstallationId] {
                                ComputerConnectionChoices(model: candidate, discovery: discovery(for: candidate), selected: false,
                                                          reportCapture: { captureAvailability[saved.credential.hostInstallationId] = $0 }) {
                                    request(saved)
                                }
                                .disabled(saved.requiresPairing)
                            }
                        }
                        if !captureAvailability.values.contains(true) {
                            if captureAvailability.count < 1 + menu.candidates.count {
                                ProgressView("Loading computers").frame(maxWidth: .infinity, minHeight: 44)
                            } else {
                                Text("No computers can share a screen").font(.footnote).foregroundStyle(.secondary)
                                    .frame(maxWidth: .infinity, minHeight: 44)
                                    .accessibilityIdentifier("computer-session-no-screens")
                            }
                        }
                    }
                    .font(.body).foregroundStyle(.primary).buttonStyle(.plain)
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height in
                        menuContentHeight = height
                    }
                }
                .scrollBounceBehavior(.basedOnSize)
                .accessibilityIdentifier("computer-session-sources")
                .frame(idealWidth: 300, maxWidth: 320,
                       idealHeight: min(420, menuContentHeight),
                       maxHeight: 420)
            }
            #if WONDER_DIAGNOSTICS
            .task {
                if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-sources-open") { openSourceMenu() }
            }
            #endif
            .task(id: requestedSource?.id) {
                guard let requestedSource else { return }
                await selectSource(requestedSource.connection)
                guard !Task.isCancelled else { return }
                switching = false
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityValue(sourceDescription)
        .accessibilityIdentifier("computer-session-header")
    }
}

/// The helper owns capture availability. Headless computers are hidden; a
/// missing permission keeps the computer and its recovery step visible.
private struct ComputerConnectionChoices: View {
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var model: ConnectionModel
    @Binding var discovery: ComputerWindowMenuDiscovery
    let selected: Bool
    let reportCapture: (Bool) -> Void
    let select: () -> Void
    private var canShowComputer: Bool { discovery.canShowComputer(isCurrent: selected) }

    private var discoveryIdentity: String? {
        guard scenePhase == .active, !model.accessEnded else { return nil }
        #if WONDER_DIAGNOSTICS
        if oldHostFixture { return "old-host:fixture-phone" }
        #endif
        return model.connection.map { $0.credential.hostInstallationId + ":" + $0.credential.deviceId }
    }

    #if WONDER_DIAGNOSTICS
    private var oldHostFixture: Bool {
        ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-menu-old-host")
    }
    #endif

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if canShowComputer && !model.accessEnded {
                Button(action: select) {
                    HStack(spacing: 8) {
                        Image(systemName: "desktopcomputer").frame(width: 20)
                        Text(model.macName).lineLimit(2)
                        Circle().fill(model.macConnected == true ? Color.green : Color.secondary.opacity(0.6))
                            .frame(width: 7, height: 7).accessibilityHidden(true)
                        Spacer(minLength: 8)
                        if selected { Image(systemName: "checkmark") }
                    }
                    .frame(minHeight: 44).contentShape(Rectangle())
                }
                .disabled(discovery.captureRecovery != nil)
                .accessibilityLabel(model.macName)
                .accessibilityValue(selected ? "Selected" : "")
                .accessibilityIdentifier(selected ? "computer-session-whole-screen" : "computer-session-connection:" + (model.connection?.credential.hostInstallationId ?? "fixture"))
                if let recovery = discovery.captureRecovery {
                    Text(recovery).font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true).padding(.vertical, 4)
                        .accessibilityIdentifier("computer-session-capture-permission")
                }
            }
            // Screens of other computers that an agent added over SSH; nothing shows until there is one.
            RemoteScreensSection(model: model)
        }
        .disabled(model.accessEnded)
        .task(id: discoveryIdentity) {
            #if WONDER_DIAGNOSTICS
            if !oldHostFixture && (ProcessInfo.processInfo.arguments.contains("-computer-agent-preview")
                || ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-menu-fixture")) {
                let available = !ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-menu-unavailable")
                let permissionRequired = ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-menu-permission")
                discovery.receive(ComputerAvailableSources(available: available && !permissionRequired, sources: [],
                    capture: permissionRequired ? .init(available: false, reason: "screen_recording_permission_required") : nil))
                reportCapture(canShowComputer)
                return
            }
            #endif
            guard let identity = discoveryIdentity, !model.accessEnded else {
                reportCapture(canShowComputer && !model.accessEnded)
                return
            }
            discovery.begin(identity: identity)
            reportCapture(canShowComputer)
            do {
                #if WONDER_DIAGNOSTICS
                // Older hosts reject discovery; keep the current computer reachable.
                if oldHostFixture { throw PairingFailure.response(400) }
                #endif
                let result: ComputerAvailableSources = try await model.manage("/api/v1/computer/sources/windows")
                guard !Task.isCancelled, identity == discoveryIdentity else { return }
                discovery.receive(result)
                reportCapture(result.canShowComputer)
            } catch {
                guard !Task.isCancelled, identity == discoveryIdentity else { return }
                discovery.recordFailure()
                reportCapture(canShowComputer)
            }
        }
    }
}

/// Who controls the computer, in one fixed spot: you, the agent or nobody.
/// The chip names the holder and is the one-tap way to take or give control
/// (the viewport's edge takes the holder's colour). Nobody holds it: Take
/// control. The agent works: Take over. You hold it: tap to give it back.
struct ComputerControlChip: View {
    @ObservedObject var model: ComputerSessionModel
    let agent: ComputerAgentPresence?
    var agentActive = false
    var compact = false
    /// The Take control button's identifier; the simulator view keeps its own.
    var takeControlID = "computer-session-take-control"
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @State private var showingViewOnlyReason = false

    enum Holder { case you, agent, nobody }

    private var agentWorking: Bool { agent?.isActive == true || agentActive }

    /// The viewport's border: only while the agent is acting; your own control
    /// is shown by the chip, not a frame around the picture.
    static func tint(model: ComputerSessionModel, agent: ComputerAgentPresence?) -> Color? {
        guard !model.isClosed, !model.dialogGone else { return nil }
        if model.isControlActive || model.isStarting { return nil }
        return agent?.isActing == true && model.controlAvailable ? .purple : nil
    }

    var body: some View {
        if model.isClosed || model.dialogGone {
            EmptyView()
        } else if model.isControlActive {
            chip(symbol: "hand.point.up.left.fill", title: "In control", tint: .blue, filled: false) {
                Task { await model.done() }
            }
            .accessibilityLabel("Give control back")
            .accessibilityValue("You are in control")
            .accessibilityIdentifier("computer-session-done")
        } else if model.isStarting {
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                if !compact && !dynamicTypeSize.isAccessibilitySize { Text("Starting").lineLimit(1) }
            }
            .font(compact ? .system(size: 16, weight: .semibold) : .footnote.weight(.semibold))
            .padding(.horizontal, 12).padding(.vertical, 7)
            .background(.regularMaterial, in: Capsule())
            .frame(minWidth: compact ? 44 : nil, minHeight: 44)
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Starting control…")
            .accessibilityIdentifier("computer-session-waiting")
        } else if model.isViewOnly {
            chip(symbol: "eye", title: "View only", tint: nil, filled: false) { showingViewOnlyReason = true }
                .popover(isPresented: $showingViewOnlyReason) {
                    Text(model.viewOnlyReason).font(.callout).padding().frame(width: 280)
                        .presentationCompactAdaptation(.popover)
                }
                .accessibilityLabel("View only")
                .accessibilityValue(model.viewOnlyReason)
                .accessibilityHint("Shows why this computer can't be controlled")
                .accessibilityIdentifier("computer-session-window-view-only")
        } else if model.controlAvailable {
            chip(symbol: agentWorking ? nil : "hand.point.up.left",
                 title: agentWorking ? "\(agent?.name ?? "Agent") is working" : "Take control",
                 tint: agentWorking ? .purple : nil, filled: !agentWorking) {
                model.takeControl()
            }
            .disabled(!model.canTakeControl)
            .accessibilityLabel(agentWorking ? "Take over" : "Take control")
            .accessibilityValue(agentWorking ? "\(agent?.name ?? "The agent") is using \(model.hostReference)" : "Nobody is controlling")
            .accessibilityHint(agentWorking ? "Use \(model.hostReference) yourself while the agent is working" : "")
            .accessibilityIdentifier(takeControlID)
        }
    }

    private func chip(symbol: String?, title: String, tint: Color?, filled: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 6) {
                // The working pill is text; the icon returns only where text is hidden.
                if symbol != nil || compact || dynamicTypeSize.isAccessibilitySize { Image(systemName: symbol ?? "hand.point.up.left") }
                if !compact && !dynamicTypeSize.isAccessibilitySize { Text(title).lineLimit(1) }
            }
            .font(compact ? .system(size: 16, weight: .semibold) : .footnote.weight(.semibold))
            .foregroundStyle(filled ? theme.page : (tint ?? theme.computerForeground))
            .padding(.horizontal, 12).padding(.vertical, 7)
            .background {
                if filled { Capsule().fill(theme.accent) }
                else { Capsule().fill(.regularMaterial).overlay { Capsule().strokeBorder(tint ?? .clear, lineWidth: 2) } }
            }
            .frame(minWidth: compact ? 44 : nil, minHeight: 44)
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .dynamicTypeSize(...DynamicTypeSize.accessibility2)
    }
}

struct ComputerViewport: View {
    @ObservedObject var model: ComputerSessionModel
    /// A tap on the picture while you only watch.
    var onWatchTap: () -> Void = {}
    /// Only the viewport on top sizes the model: the pane while it is shown,
    /// the full-screen cover while it is open, so Fit and zoom follow the screen
    /// being looked at. The pane reports its size again when the cover closes.
    var sizesModel = true
    @State private var lastSize: CGSize = .zero
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @State private var recoveryDetails: String?

    var body: some View {
        // Fit the source inside the viewport; gestures also need its letterbox area.
        GeometryReader { proxy in
            #if DEBUG || WONDER_DIAGNOSTICS
            let _ = { if sizesModel { model.noteDrawnZoom(width: proxy.size.width) } }()
            #endif
            let transform = model.viewportTransform(in: proxy.size)
            let videoFrame = transform.videoFrame(encodedAspect: ComputerSessionModel.encodedAspectRatio(model.displaySession))
            ZStack {
                theme.computerBackground
                if model.displaySession?.capability.available == true {
                    ComputerVideoView(receiver: model.receiver)
                        .frame(width: videoFrame.width, height: videoFrame.height)
                        .position(x: videoFrame.midX, y: videoFrame.midY)
                }
                if model.isControlActive {
                    ComputerPhonePointer(pointer: model.pointerState, transform: transform)
                }
            }
            .overlay {
                ComputerGestureSurface(
                    allowsInput: sizesModel && model.isControlActive,
                    mode: model.pictureInputMode,
                    zoomScale: model.zoomScale,
                    onTap: { point, size, button, modifiers in
                        model.tap(at: point, in: size, button: button, modifiers: modifiers)
                    },
                    onMove: { delta, size in model.movePointer(by: delta, in: size) },
                    onDragBegan: { point, size, modifiers in model.dragBegan(at: point, in: size, modifiers: modifiers) },
                    onDragMoved: { point, size in model.dragMoved(to: point, in: size) },
                    onDragEnded: { point, size in model.dragEnded(at: point, in: size) },
                    onScroll: { model.twoFingerScroll(delta: $0) },
                    onPan: { delta, size in model.panViewport(by: delta, in: size) },
                    onPinch: { model.magnify($0) },
                    onHover: { point, size in model.hoverPointer(at: point, in: size) },
                    onWatchTap: onWatchTap
                )
            }
            .accessibilityAction(named: Text("Click")) { model.accessibilityClick(in: proxy.size) }
            .accessibilityAction(named: Text("Right-click")) { model.accessibilityClick(in: proxy.size, button: "right") }
            .accessibilityAction(named: Text("Recenter pointer")) { model.recenterPointer() }
            .onGeometryChange(for: CGSize.self) { $0.size } action: {
                lastSize = $0
                if sizesModel { model.viewportResized($0) }
            }
            // A viewport that appears (the pane opening, the cover, a new layout)
            // starts at Fit even when it is the same size as the last one.
            .onAppear {
                lastSize = proxy.size
                if sizesModel { model.resetZoom(); model.viewportResized(proxy.size) }
            }
            .onChange(of: sizesModel) { _, shown in
                // The viewport on top starts at Fit, whatever the other one zoomed.
                if shown, lastSize != .zero { model.resetZoom(); model.viewportResized(lastSize) }
            }
        }
        .clipped()
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Computer screen preview")
        .accessibilityValue(model.receiver.picture.hasFrame && model.notice != nil
                            ? "\(model.liveStatus), showing the last picture" : model.liveStatus)
        .accessibilityIdentifier(sizesModel ? "computer-session-preview" : "computer-session-background-preview")
        .accessibilityHidden(!sizesModel)
        // Above the gesture surface so Try again can be tapped, and outside the
        // combined preview element so VoiceOver reads it once and can press it.
        .overlay {
            if sizesModel, let notice = model.notice, let busy = model.viewerBusy {
                // Another device has the view: taking it over is the way in;
                // Try again checks whether it has since let go.
                ComputerViewNoticeView(
                    notice: notice, busy: model.isLoading,
                    recovery: "Take over to view it here. The view on \(busy.viewingDevice) closes, and any control from there ends.",
                    retryTitle: "Take over", retryIdentifier: "computer-session-take-over",
                    retry: { Task { await model.takeOver() } },
                    alternative: ("Try again", { Task { await model.retry() } }))
            } else if sizesModel, let notice = model.notice {
                ZStack {
                    // Dims the last frame kept while reconnecting, so it reads as paused.
                    Color.black.opacity(model.receiver.picture.hasFrame ? 0.4 : 0)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                    if model.receiver.picture.hasFrame {
                        ComputerStalePill(message: notice.message, status: model.liveStatus, canRetry: notice.canRetry,
                                          busy: model.isLoading, accessibilitySize: dynamicTypeSize.isAccessibilitySize,
                                          showDetails: { recoveryDetails = notice.message },
                                          retry: { Task { await model.retry() } })
                    } else {
                        ComputerViewNoticeView(notice: notice, busy: model.isLoading) { Task { await model.retry() } }
                    }
                }
            } else if model.hostReport.windowHidden {
                Label("This window is hidden or closed on \(model.hostHere). Show it there or reopen Computer for the whole screen.",
                      systemImage: "eye.slash")
                    .font(.footnote.weight(.medium))
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .padding(12)
                    .frame(maxHeight: .infinity, alignment: .bottom)
                    .allowsHitTesting(false)
                    .accessibilityIdentifier("computer-session-window-hidden")
            }
        }
        .alert("Computer view", isPresented: Binding(get: { recoveryDetails != nil }, set: { if !$0 { recoveryDetails = nil } })) {
            Button("OK", role: .cancel) { recoveryDetails = nil }
        } message: {
            Text(recoveryDetails ?? "")
        }
    }
}

/// A stalled link over the dimmed last frame: how old that frame is and one
/// way back, in a small pill instead of a banner. At accessibility sizes the
/// full reason opens on demand rather than covering the picture.
private struct ComputerStalePill: View {
    let message: String
    let status: String
    let canRetry: Bool
    let busy: Bool
    let accessibilitySize: Bool
    let showDetails: () -> Void
    let retry: () -> Void
    @State private var since = Date()

    var body: some View {
        VStack {
            Spacer(minLength: 0)
            HStack(spacing: 8) {
                Circle().fill(Color.orange).frame(width: 8, height: 8).accessibilityHidden(true)
                if accessibilitySize {
                    Button(action: showDetails) {
                        Label(status, systemImage: "info.circle").font(.footnote.weight(.medium))
                    }
                    .accessibilityLabel("Connection details: \(status)")
                    .accessibilityIdentifier("computer-session-recovery-details")
                } else {
                    VStack(alignment: .leading, spacing: 0) {
                        Text(message).font(.footnote.weight(.medium)).lineLimit(2)
                            .accessibilityIdentifier("computer-session-notice")
                        TimelineView(.periodic(from: since, by: 1)) { context in
                            Text(Self.age(context.date.timeIntervalSince(since)))
                                .font(.caption2).foregroundStyle(.secondary)
                                .accessibilityIdentifier("computer-session-frame-age")
                        }
                    }
                }
                if canRetry {
                    Button("Try again", action: retry)
                        .font(.footnote.weight(.semibold))
                        .frame(minHeight: 44)
                        .disabled(busy)
                        .accessibilityIdentifier("computer-session-retry")
                }
            }
            .padding(.horizontal, 14).padding(.vertical, 4)
            .background(.regularMaterial, in: Capsule())
            .padding(12)
        }
        .frame(maxWidth: .infinity)
        .foregroundStyle(.primary)
    }

    /// "Last picture 2 s ago": a tick under a second is just now.
    static func age(_ seconds: TimeInterval) -> String {
        let whole = Int(max(0, seconds))
        if whole < 1 { return "Last picture just now" }
        if whole < 60 { return "Last picture \(whole) s ago" }
        return "Last picture \(whole / 60) min ago"
    }
}

/// The computer view's one status or failure message, with Try again when it can help.
struct ComputerViewNoticeView: View {
    let notice: ComputerViewNotice
    let busy: Bool
    var symbol = "desktopcomputer"
    /// What to do about it, under a failure that can be retried.
    var recovery: String?
    var identifierPrefix = "computer-session"
    /// The main action's words and identifier, when it is not Try again.
    var retryTitle = "Try again"
    var retryIdentifier: String?
    let retry: () -> Void
    /// A second way out, such as choosing another simulator.
    var alternative: (title: String, action: () -> Void)?
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        VStack(spacing: 12) {
            VStack(spacing: 12) {
                Image(systemName: symbol)
                    .font(.system(size: 34, weight: .light))
                    .accessibilityHidden(true)
                Text(notice.message).font(.headline).multilineTextAlignment(.center)
                    .accessibilityIdentifier(identifierPrefix + "-notice")
                if notice.canRetry, let recovery {
                    Text(recovery).font(.subheadline).foregroundStyle(theme.secondaryText).multilineTextAlignment(.center)
                        .accessibilityIdentifier(identifierPrefix + "-recovery")
                }
            }
            .allowsHitTesting(false)
            if notice.canRetry {
                Button(retryTitle, action: retry)
                    .buttonStyle(.wonderProminent)
                    .disabled(busy)
                    .accessibilityIdentifier(retryIdentifier ?? identifierPrefix + "-retry")
                if let alternative {
                    Button(alternative.title, action: alternative.action)
                        .frame(minHeight: 44)
                        .accessibilityIdentifier(identifierPrefix + "-alternative")
                }
            }
        }
        .foregroundStyle(theme.computerForeground)
        .padding(.horizontal, 28)
        .frame(maxWidth: 520)
    }
}

private struct ComputerPhonePointer: View {
    @ObservedObject var pointer: ComputerPointerState
    let transform: ComputerViewportTransform

    var body: some View {
        let location = transform.location(for: pointer.position)
        ComputerPointerShape()
            .fill(.black)
            .overlay { ComputerPointerShape().stroke(.white, lineWidth: 2) }
            .frame(width: 24, height: 30)
            .shadow(color: .black.opacity(0.4), radius: 2, x: 0, y: 1)
            // The tip, not the center of the enlarged arrow, is the hot spot.
            .position(x: location.x + 12, y: location.y + 15)
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }
}

private struct ComputerPointerShape: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        let points: [CGPoint] = [
            CGPoint(x: 0, y: 0), CGPoint(x: 0, y: 0.82),
            CGPoint(x: 0.28, y: 0.62), CGPoint(x: 0.49, y: 1),
            CGPoint(x: 0.7, y: 0.91), CGPoint(x: 0.5, y: 0.54),
            CGPoint(x: 1, y: 0.54)
        ]
        path.addLines(points.map { CGPoint(x: rect.minX + $0.x * rect.width, y: rect.minY + $0.y * rect.height) })
        path.closeSubpath()
        return path
    }
}

/// One centered row below the picture, also used by the full-screen and laptop
/// layouts: who controls, the Mac's dock, the shortcut icons, then keyboard,
/// trackpad and dictation. In a companion pane too narrow for all of it (the
/// Duo's half) it stays one row and scrolls: control and the inputs first, then
/// the dock and shortcuts, with a fade on the edge that has more. A phone-width
/// window has no pane beside a composer to match, so it stacks two rows instead.
private struct ComputerControlBar: View {
    @ObservedObject var model: ComputerSessionModel
    let agent: ComputerAgentPresence?
    var persistentTrackpad = false
    /// The chip at one end and the inputs at the other, the room between them
    /// free (a phone's floating bar shares the top with the picture).
    var spreadsEnds = false
    @Environment(\.wonderTheme) private var theme
    @State private var hiddenEdges = ScrollHiddenEdges()

    /// One row: who controls (Take control, centred when nothing else is
    /// there), the inputs, then the Mac's dock. A window too narrow for it
    /// scrolls the dock instead of growing a second line.
    var body: some View {
        row
        .frame(minHeight: ToolPanelMetrics.rowHeight)
        .buttonStyle(.plain)
        .foregroundStyle(theme.computerForeground)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("computer-session-control-bar")
    }

    private var chip: some View {
        ComputerControlChip(model: model, agent: agent)
            .fixedSize(horizontal: true, vertical: false)
    }

    /// Where a vertical hinge runs through the row (x range in the row's own
    /// coordinates); nil on every device without one.
    @State private var hinge: ClosedRange<CGFloat>?

    private var row: some View {
        rowContent
            .onGeometryChange(for: ClosedRange<CGFloat>?.self) { proxy in
                guard let frame = DuoFold.region(in: proxy)?.frame, frame.height > frame.width,
                      frame.minX > 0, frame.minX < proxy.size.width else { return nil }
                return frame.minX...frame.maxX
            } action: { hinge = $0 }
    }

    @ViewBuilder private var rowContent: some View {
        if let hinge, !spreadsEnds, model.isControlActive {
            // Across the unfolded Duo the chip stays centred on the picture and
            // the inputs start past the hinge, so no button sits on it.
            HStack(spacing: 0) { Spacer(minLength: 0); chip; Spacer(minLength: 0) }
                .overlay(alignment: .leading) {
                    inputs.padding(.leading, hinge.upperBound + 8)
                }
        } else {
            plainRow
        }
    }

    private var plainRow: some View {
        HStack(spacing: 4) {
            if spreadsEnds {
                chip
                Spacer(minLength: 0)
                if model.isControlActive { inputs }
            } else {
                Spacer(minLength: 0)
                chip
                if model.isControlActive { inputs }
                Spacer(minLength: 0)
            }
        }
    }

    /// Too narrow for `row`: who controls and the inputs stay put and only the
    /// dock scrolls, instead of the row growing a second line.
    private var inputs: some View {
        HStack(spacing: 0) {
            inputButton(.keyboard, symbol: "keyboard", selected: model.keyboardPresented,
                        label: model.keyboardPresented ? "Hide keyboard" : "Show keyboard", id: "computer-session-keyboard")
            if model.trackpadOffered || persistentTrackpad {
                inputButton(.trackpad, symbol: "rectangle.and.hand.point.up.left", selected: (persistentTrackpad || model.showsTrackpad) && !model.keyboardPresented,
                            label: persistentTrackpad ? (model.keyboardPresented ? "Use trackpad" : "Trackpad") : (model.showsTrackpad ? "Hide trackpad" : "Show trackpad"), id: "computer-session-trackpad-toggle")
            }
            ComputerDictationButton(model: model)
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("computer-session-input-mode")
    }

    private func inputButton(_ input: ComputerBottomInput, symbol: String, selected: Bool, label: String, id: String) -> some View {
        Button { model.selectBottomInput(input, persistentTrackpad: persistentTrackpad) } label: {
            Image(systemName: symbol)
                .dynamicTypeSize(...DynamicTypeSize.xLarge)
                .frame(width: 44, height: 44)
                .background(selected ? theme.accent.opacity(0.18) : .clear, in: RoundedRectangle(cornerRadius: 8))
                .contentShape(Rectangle())
        }
        .accessibilityLabel(label)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityIdentifier(id)
    }
}

/// The row above the system keyboard: Esc, the app switcher (⌘ Tab or Alt Tab),
/// the platform's window and app keys and the clipboard. The keyboard's own
/// microphone is the dictation entry. Names live in the accessibility labels.
struct ComputerKeyboardShortcuts: View {
    @ObservedObject var model: ComputerSessionModel
    @Environment(\.wonderTheme) private var theme

    /// The app switcher first, then the platform's other keys in their table order.
    private var platformButtons: [ComputerPlatformControls.Button] {
        let all = model.platformControls.rows.flatMap { $0 }
        return all.filter { $0.action == .appSwitcher } + all.filter { $0.action != .appSwitcher }
    }

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 2) {
                keyButton(key: "escape", label: "Escape") { Text("Esc").font(.system(size: 14, weight: .semibold)) }
                ComputerClipboardMenu(model: model)
                ForEach(platformButtons) { platformButton($0) }
                keyButton(key: "tab", label: "Tab") { Image(systemName: "arrow.right.to.line") }
            }
            .padding(.horizontal, 8)
            .frame(maxWidth: .infinity)
        }
        .scrollIndicators(.hidden)
        .simultaneousGesture(DragGesture(minimumDistance: 0)
            .onChanged { _ in model.claimSwitcherTouch() }
            .onEnded { _ in model.claimSwitcherTouch() })
        .frame(height: 48)
        .foregroundStyle(theme.accent)
        .buttonStyle(.plain)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("computer-session-shortcuts-row")
    }

    private var isMac: Bool { ComputerKeyboardPlatform(host: model.platformKey) == .macos }

    @ViewBuilder private func glyph(_ button: ComputerPlatformControls.Button, symbol: String) -> some View {
        if button.action == .appSwitcher && isMac {
            CommandTabGlyph()
        } else if symbol == "keyboard" {
            // Modifier keys have no symbol of their own: the key's name, short.
            Text(button.title.components(separatedBy: " ").first ?? button.title)
                .font(.system(size: 14, weight: .semibold))
        } else {
            Image(systemName: symbol)
        }
    }

    private func platformButton(_ button: ComputerPlatformControls.Button) -> some View {
        let shown = ComputerSessionControls.presentation(button, model: model)
        return Button { model.performControl(button) } label: {
            glyph(button, symbol: shown.symbol)
                .dynamicTypeSize(...DynamicTypeSize.xLarge)
                .frame(width: 44, height: 44)
                .background(shown.selected ? theme.accent.opacity(0.15) : .clear, in: RoundedRectangle(cornerRadius: 8))
                .overlay(alignment: .bottom) {
                    // A kept-held ⌘ is underlined, like a latched key.
                    if button.action == .appSwitcher, model.isSwitcherLatched {
                        Capsule().fill(theme.accent).frame(width: 20, height: 2).padding(.bottom, 4)
                            .accessibilityIdentifier("computer-session-switcher-latched")
                    }
                }
                .contentShape(Rectangle())
        }
        .accessibilityLabel(shown.label)
        .accessibilityValue(shown.value)
        .accessibilityHint(shown.hint)
        .accessibilityIdentifier("computer-session-shortcut-" + button.id)
    }

    private func keyButton<Glyph: View>(key: String, label: String, @ViewBuilder glyph: () -> Glyph) -> some View {
        Button { model.sendKey(key) } label: {
            glyph()
                .dynamicTypeSize(...DynamicTypeSize.xLarge)
                .frame(width: 44, height: 44)
                .contentShape(Rectangle())
        }
        .accessibilityLabel(label)
        .accessibilityIdentifier("computer-session-key-\(key)")
    }
}

/// ⌘ and ⇥ drawn as one glyph for the Mac's app switcher.
private struct CommandTabGlyph: View {
    var body: some View {
        HStack(spacing: 1) {
            Image(systemName: "command").font(.system(size: 15, weight: .medium))
            Image(systemName: "arrow.right.to.line").font(.system(size: 13, weight: .medium))
        }
        .accessibilityHidden(true)
    }
}

/// The destination of Cupola's own dictation in the computer view: the words heard are
/// kept here and typed into the computer once, when dictation finishes.
@MainActor final class ComputerDictationEditor: DictationEditor {
    private weak var model: ComputerSessionModel?
    private var words = ""
    init(model: ComputerSessionModel) { self.model = model }
    var dictationLocale: String { Locale.preferredLanguages.first ?? Locale.current.identifier }
    func beginDictation() -> Bool {
        guard model?.isControlActive == true else { return false }
        words = ""
        return true
    }
    func showDictation(_ words: String) -> Bool {
        guard model?.isControlActive == true else { return false }
        self.words = words
        return true
    }
    func endDictation(commitWords: Bool) -> Bool {
        defer { if commitWords { words = "" } }
        guard commitWords, !words.isEmpty else { words = ""; return true }
        guard let model, model.isControlActive else { return false }
        // Typed text goes in pieces below the input limit; line breaks become spaces.
        let clean = String(String.UnicodeScalarView(words.unicodeScalars.map {
            $0.properties.generalCategory == .control ? " " : $0
        }))
        var rest = Substring(clean)
        while !rest.isEmpty {
            let piece = rest.prefix(1_000)
            model.sendKeyboardActions([.text(String(piece))])
            rest = rest.dropFirst(piece.count)
        }
        return true
    }
}

/// Starts Cupola's own dictation and types what it hears into the computer. Tap again to stop.
private struct ComputerDictationButton: View {
    @ObservedObject var model: ComputerSessionModel
    var body: some View { Content(model: model, controller: model.dictationController) }

    private struct Content: View {
        @ObservedObject var model: ComputerSessionModel
        @ObservedObject var controller: DictationController
        private var chat: ChatSummary { model.chat }
        private var recording: Bool { controller.intent?.conversationID == chat.id && controller.recording }
        private var preparing: Bool { controller.preparingConversationID == chat.id }
        private var finishing: Bool { controller.intent?.conversationID == chat.id && controller.processing }

        var body: some View {
            Button {
                if preparing || finishing { controller.cancel() }
                else if recording { controller.finishCapture() }
                else {
                    controller.attachEditor(model.dictationEditor, conversationID: chat.id)
                    controller.begin(chat: chat)
                }
            } label: {
                Group {
                    if preparing || finishing { ProgressView() }
                    else { Image(systemName: recording ? "mic.fill" : "mic") }
                }
                .foregroundStyle(recording ? Color.red : Color.primary)
                .modifier(ComposerStatusPill(isActive: recording))
                .environment(\.composerPillsInBar, true)
                .frame(width: 44, height: 44)
                .contentShape(Rectangle())
            }
            .disabled(!model.isControlActive && !recording && !preparing && !finishing)
            .accessibilityLabel(recording ? "Stop dictation" : (preparing || finishing ? "Cancel dictation" : "Dictate"))
            .accessibilityHint(recording ? "Types what you said into the computer."
                : "Starts dictation and types what you say into the computer.")
            .accessibilityValue(recording ? "Recording" : (finishing ? "Finishing" : "Idle"))
            .accessibilityIdentifier("computer-session-microphone")
            .overlay(alignment: .top) {
                if let notice = controller.visibleNotice(for: chat.id) {
                    Text(notice.message).font(.caption2).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true).frame(width: 220)
                        .offset(y: -44).accessibilityIdentifier("computer-session-dictation-notice")
                }
            }
        }
    }
}

/// The Mac's pinned and recent apps. A strip of its own (folded), or the part
/// of the bar's row that sits between who controls and the shortcuts.
private struct ComputerMiniDock: View {
    @ObservedObject var model: ComputerSessionModel
    /// A strip that scrolls along its own axis, instead of the plain row.
    var scrolls = true

    var body: some View {
        if model.miniDock.available {
            Group {
                if scrolls {
                    ScrollView(.horizontal) { apps }
                        .scrollIndicators(.hidden)
                } else { apps }
            }
            .frame(height: 44)
            .accessibilityElement(children: .contain)
            .accessibilityLabel("Pinned and recent apps")
            .accessibilityIdentifier("computer-session-mini-dock")
        }
    }

    private var apps: some View {
        HStack(spacing: 2) {
            ForEach(model.miniDock.pinned) { app in
                ComputerMiniDockApp(model: model, app: app)
            }
            if !model.miniDock.pinned.isEmpty && !model.miniDock.recent.isEmpty {
                Divider().frame(height: 24).padding(.horizontal, 4)
                    .accessibilityHidden(true)
            }
            ForEach(model.miniDock.recent) { app in
                ComputerMiniDockApp(model: model, app: app)
            }
        }
    }
}

private struct ComputerMiniDockApp: View {
    @ObservedObject var model: ComputerSessionModel
    let app: ComputerMiniDockContent.App
    @State private var icon: UIImage?
    @State private var opening = false
    @Environment(\.wonderTheme) private var theme
    private var fallback: String {
        let name = app.name.lowercased()
        if name.contains("terminal") { return "terminal" }
        if name.contains("safari") || name.contains("firefox") || name.contains("edge") { return "globe" }
        if name.contains("finder") || name.contains("files") || name.contains("explorer") { return "folder" }
        return "app"
    }
    var body: some View {
        Button {
            opening = true
            Task {
                await model.openDockApp(app)
                opening = false
            }
        } label: {
            VStack(spacing: 3) {
                Group {
                    if opening { ProgressView().controlSize(.small) }
                    else if let icon { Image(uiImage: icon).resizable().scaledToFit() }
                    else { Image(systemName: fallback).font(.system(size: 23)) }
                }
                .frame(width: 28, height: 28)
                Capsule()
                    .fill(app.running ? theme.computerForeground : .clear)
                    .frame(width: model.platformKey == "windows" ? 14 : 4, height: 3)
            }
            .frame(width: 44, height: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!model.isControlActive || opening)
        .accessibilityLabel("Open \(app.name)")
        .accessibilityValue(app.running ? "Running" : "Not running")
        .accessibilityHint(model.isControlActive ? "Opens or focuses this app on \(model.hostShortName)" : "Take control to open apps")
        .accessibilityIdentifier("computer-session-dock-app:" + app.id)
        .task(id: app.iconURL) {
            // The host advertises icon availability, but all loads use our
            // authenticated, fixed icon route; no arbitrary URL is followed.
            if app.iconURL != nil { icon = await model.installedAppIcon(app.id) }
        }
    }
}

/// Which edges of a horizontal scroll have more behind them, so the row fades
/// there and says it scrolls.
private struct ScrollHiddenEdges: Equatable {
    var leading = false
    var trailing = false

    var fade: LinearGradient {
        LinearGradient(stops: [
            .init(color: leading ? .clear : .black, location: 0),
            .init(color: .black, location: leading ? 0.08 : 0),
            .init(color: .black, location: trailing ? 0.92 : 1),
            .init(color: trailing ? .clear : .black, location: 1),
        ], startPoint: .leading, endPoint: .trailing)
    }
}

struct ComputerSessionControls: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ComputerSessionModel
    var agent: ComputerAgentPresence? = nil
    var sidePlacement = false
    /// The controls are items in the Duo's system bar (`ComputerBarItem`);
    /// this shows only what the computer says and the dock strip.
    var controlsInBar = false
    /// The trackpad's key row: latching modifiers above Esc and Tab.
    var stickyModifiers = false
    /// Shows the button that adds or removes the separate trackpad. The
    /// half-folded Duo always has one.
    var togglesTrackpad = true
    /// Draws the bar as the composer's twin (see `ToolPanelMetrics`) instead of
    /// a full-width slab; only the split viewer sits beside a composer.
    var composerShaped = false
    /// The margin under the panel so it ends at the composer's bottom edge
    /// beside it (see `CompanionPaneChrome.panelBottomMargin`).
    var bottomMargin = ToolPanelMetrics.bottomMargin
    /// Over the picture (a phone turned sideways): a material, not the slab.
    var floating = false
    /// Extra room on one side so the row centres on the screen when the
    /// system takes more of the other (see `fullScreenViewer`).
    var balance = EdgeInsets()

    private var panel: Bool { composerShaped && !controlsInBar && !sidePlacement }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            notices
            if panel {
                VStack(alignment: .leading, spacing: 0) { rows }
                    .toolPanelSurface(theme, bottomMargin: bottomMargin)
                    .padding(.top, 4)
            } else {
                VStack(alignment: .leading, spacing: 0) { rows }
                    .padding(.horizontal, sidePlacement ? 2 : 8)
                    .padding(balance)
                    // In the bar's place nothing under the picture takes height until there is something to say.
                    .padding(.vertical, controlsInBar || !model.isControlActive ? 0 : 4)
            }
        }
        .padding(.bottom, controlsInBar && (model.controlMessage != nil || model.clipboardMessage != nil) ? 8 : 0)
        .font(.body)
        .foregroundStyle(theme.accent)
        .buttonStyle(.plain)
        // Only the home indicator strip shares the slab's colour; it stops at
        // the Duo's side bar instead of running under it.
        .background(floating ? AnyShapeStyle(.regularMaterial)
                    : AnyShapeStyle(model.isControlActive && !sidePlacement && !controlsInBar && !panel ? theme.surface : theme.computerBackground),
                    ignoresSafeAreaEdges: .bottom)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("computer-session-controls-row")
    }

    /// What the computer says: a failure once, the clipboard, what the platform cannot send.
    @ViewBuilder private var notices: some View {
        // A failed view says why once, over the picture.
        if let message = model.controlMessage, model.notice?.canRetry != true {
            Text(message)
                .font(.caption)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 12)
                .padding(.top, 8)
                .accessibilityIdentifier("computer-session-control-message")
        }
        if let message = model.clipboardMessage {
            Text(message)
                .font(.caption)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 12)
                .padding(.top, 8)
                .accessibilityIdentifier("computer-session-clipboard-message")
        }
        if model.isControlActive, !sidePlacement, let limitation = model.platformControls.limitation {
            Text(limitation)
                .font(.caption)
                .foregroundStyle(theme.secondaryText)
                .padding(.horizontal, 12)
                .padding(.top, 4)
                .accessibilityIdentifier("computer-session-input-limitation")
        }
    }

    @ViewBuilder private var rows: some View {
        // Row 1: the shortcuts, centred and present with or without the keyboard.
        if model.isControlActive {
            ComputerKeyboardShortcuts(model: model)
        }
        // The Mac's dock is a strip of its own, never in the Duo's vertical
        // rail; a phone keeps the room for the keyboard instead.
        if controlsInBar {
            ComputerMiniDock(model: model)
                .padding(.horizontal, 8)
        } else if !model.keyboardPresented {
            ComputerMiniDock(model: model)
                .padding(.horizontal, 8)
        }
        // Row 2: who controls, keyboard, trackpad and dictation.
        if !controlsInBar {
            ComputerControlBar(model: model, agent: agent, persistentTrackpad: !togglesTrackpad,
                               spreadsEnds: floating)
        }
        if model.isControlActive {
            if !model.keyboardPresented, stickyModifiers {
                HStack(spacing: 4) {
                    ForEach(ComputerPlatformControls.stickyModifiers(forHost: model.platformKey)) { modifierButton($0) }
                }
            }
            Color.clear.frame(height: 1)
                .accessibilityElement()
                .accessibilityLabel("Computer control active")
                .accessibilityIdentifier("computer-session-control-active")
        }
    }

    /// A platform button's words and state, wherever it is drawn: the app
    /// switcher's change while it is held open, a modifier shows it is held.
    struct Presentation {
        var title: String
        var label: String
        var symbol: String
        var selected: Bool
        var value: String
        var hint: String
    }

    static func presentation(_ button: ComputerPlatformControls.Button, model: ComputerSessionModel) -> Presentation {
        let switcher = button.action == .appSwitcher
        let selected: Bool = {
            if switcher { return model.isAppSwitcherPresented }
            if case .modifier(let key) = button.action { return model.modifierLatch.keys.contains(key) }
            return false
        }()
        let controls = model.platformControls
        return Presentation(
            title: switcher ? (selected ? "Choose app" : controls.switcherTitle) : button.title,
            label: switcher ? (selected ? "Choose highlighted app" : "Show app switcher, \(controls.switcherName) Tab") : button.label,
            symbol: switcher ? controls.switcherSymbol : button.symbol,
            selected: selected,
            value: switcher ? (selected ? (model.isSwitcherLatched ? "Open, \(controls.switcherName) kept held" : "Open, \(controls.switcherName) held") : "Closed") : (selected ? "Selected for next key" : ""),
            hint: switcher
                ? "Holds the app switcher open until your next tap or key. Double-tap to keep it open; use Tab to move, tap again to choose, or Escape to cancel."
                : (selected ? "Tap again to release, or type the next key." : ""))
    }

    private func modifierButton(_ button: ComputerPlatformControls.Button) -> some View {
        let selected: Bool = {
            if case .modifier(let key) = button.action { return model.modifierLatch.keys.contains(key) }
            return false
        }()
        return Button {
            if case .modifier(let key) = button.action { model.toggleModifier(key) }
        } label: {
            Text(button.title)
                .font(.body.weight(.medium))
                .frame(minWidth: 44, maxWidth: .infinity, minHeight: 44)
                .contentShape(Rectangle())
        }
        .background(selected ? theme.accent.opacity(0.15) : .clear, in: Capsule())
        .accessibilityLabel(button.label)
        .accessibilityValue(selected ? "Held for the next key or click" : "")
        .accessibilityHint(selected ? "Tap again to release." : "Holds \(button.label) for the next key or click.")
        .accessibilityIdentifier("computer-session-modifier-" + button.id)
    }
}

/// Shared by the keyboard's shortcut row and the folded rail.
private struct ComputerClipboardMenu: View {
    @ObservedObject var model: ComputerSessionModel
    var inRail = false

    var body: some View {
        Menu {
            ComputerClipboardActions(model: model)
        } label: {
            if inRail {
                Label("Clipboard", systemImage: "doc.on.clipboard")
                    .labelStyle(.iconOnly)
                    .modifier(ComposerStatusPill(isActive: false))
                    .environment(\.composerPillsInBar, true)
            } else {
                Image(systemName: "doc.on.clipboard")
                    // The glyph stays inside its fixed target; menu text keeps
                    // the person's full Dynamic Type size.
                    .dynamicTypeSize(...DynamicTypeSize.xLarge)
                    .frame(width: 48, height: 44)
                    .contentShape(Rectangle())
            }
        }
        .disabled(!model.isControlActive)
        .frame(minWidth: 44, minHeight: 44)
        .contentShape(Rectangle())
        .accessibilityLabel("Clipboard")
        .accessibilityIdentifier("computer-session-clipboard")
    }
}

private struct ComputerClipboardActions: View {
    @ObservedObject var model: ComputerSessionModel

    var body: some View {
        PasteButton(payloadType: String.self) { values in
            if let text = values.first { model.pasteFromPhone(text) }
        }
        .accessibilityLabel("Paste from this \(UIDevice.current.model)")
        .accessibilityIdentifier("computer-session-paste-from-phone")
        Button("Copy from \(model.hostShortName)", systemImage: "doc.on.doc") { model.copyFromMac() }
            .accessibilityIdentifier("computer-session-copy-from-mac")
    }
}

/// What the bottom input slot last held, saved per kind of device.
enum ComputerBottomInput: String {
    case off, keyboard, trackpad
}

/// Saved choices for the landscape computer layouts.
enum ComputerLandscapePreference {
    /// Settings › Computer › Experimental iPad layout; off by default.
    static let padLayoutKey = "computer.experimentalPadLayout"
}

/// The landscape layout the viewer is in. Only a phone turned sideways or a
/// wide iPad window with the experimental layout on has one.
enum ComputerLandscape: Equatable {
    case phone, pad
}

/// One computer control in the iPhone Duo's system bar, which stands beside
/// the computer view on the cover and in the side pane. While controlling it
/// holds the slab's keys, at most seven, so the conversation's Chats, context
/// and approvals still fit next to them. The host's other window and app keys
/// live in the picture's More menu. Take and give control occupy one bar item;
/// Close stays in the header. The Mac's dock is a strip above the shortcuts,
/// never an item in this vertical bar.
enum ComputerBarItem: Hashable, Identifiable {
    case control, keyboard, trackpad, microphone, clipboard

    var id: Self { self }

    static let maximumCount = 7

    static func items(controlActive: Bool, trackpadRoom: Bool = true) -> [Self] {
        guard controlActive else { return [.control] }
        // The same five in every keyboard state: the shortcut row rides on the keyboard itself.
        return [.control, .keyboard] + (trackpadRoom ? [.trackpad] : []) + [.microphone, .clipboard]
    }

}

/// The computer controls as items in the Duo's system bar. Systems without
/// the bar (and SDKs before iOS 27.1) get nothing; the slab stays.
private struct ComputerBarToolbar: ViewModifier {
    let shows: Bool
    @ObservedObject var model: ComputerSessionModel
    let agentActive: Bool
    let close: () -> Void

    func body(content: Content) -> some View {
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *) {
            content.toolbar {
                if shows {
                    ToolbarItem(id: "computer-rail", placement: .topBarTrailing) {
                        DuoRailControls(selection: ComposerPill.computer.id) {
                            VStack(spacing: DuoRailLayout.spacing) {
                                ComputerDock(isPresented: true, open: close)
                                    .environment(\.composerPillsInBar, true)
                                    .id(ComposerPill.computer.id)
                                if !model.isClosed, !model.dialogGone {
                                    ComputerRailButtons(model: model, agentActive: agentActive)
                                }
                            }
                        }
                    }
                    .duoRail(true)
                }
            }
        } else {
            content
        }
        #else
        content
        #endif
    }
}

/// All companion computer actions join the conversation's one rail owner.
struct ComputerRailButtons: View {
    @ObservedObject var model: ComputerSessionModel
    let agentActive: Bool
    var body: some View {
        VStack(spacing: DuoRailLayout.spacing) {
            ForEach(ComputerBarItem.items(controlActive: model.isControlActive, trackpadRoom: model.trackpadOffered)) { item in
                ComputerBarControl(item: item, model: model, agentActive: agentActive)
                    .buttonStyle(.plain)
                    .labelStyle(.iconOnly)
                    .environment(\.composerPillsInBar, true)
                    .id("computer-control-" + String(describing: item))
            }
        }
    }
}

/// One `ComputerBarItem`, with the same actions and identifiers as the slab's control.
private struct ComputerBarControl: View {
    let item: ComputerBarItem
    @ObservedObject var model: ComputerSessionModel
    let agentActive: Bool

    private func railLabel(_ title: String, systemImage: String, selected: Bool = false) -> some View {
        Label(title, systemImage: systemImage)
            .labelStyle(.iconOnly)
            .modifier(ComposerStatusPill(isActive: selected))
            .environment(\.composerPillsInBar, true)
    }

    var body: some View {
        switch item {
        case .control:
            ComputerControlChip(model: model, agent: nil, agentActive: agentActive, compact: true)
        case .microphone:
            ComputerDictationButton(model: model)
        case .keyboard:
            Button { model.selectBottomInput(.keyboard) } label: {
                railLabel(model.keyboardPresented ? "Hide keyboard" : "Show keyboard",
                      systemImage: model.keyboardPresented ? "keyboard.chevron.compact.down" : "keyboard", selected: model.keyboardPresented)
            }
            .accessibilityIdentifier("computer-session-keyboard")
        case .trackpad:
            Button { model.selectBottomInput(.trackpad) } label: {
                railLabel(model.showsTrackpad ? "Hide trackpad" : "Show trackpad",
                          systemImage: model.showsTrackpad ? "rectangle.and.hand.point.up.left.fill" : "rectangle.and.hand.point.up.left",
                          selected: model.showsTrackpad)
            }
            .accessibilityIdentifier("computer-session-trackpad-toggle")
        case .clipboard:
            ComputerClipboardMenu(model: model, inRail: true)
                .controlSize(.large)

        }
    }
}

#if WONDER_DIAGNOSTICS
/// An identifiable decoded frame: recovery scenarios can judge readability,
/// rather than mistaking a black fixture canvas for a retained picture.
private enum DiagnosticComputerRecovery {
    static func frame() -> RTCVideoFrame {
        let size = CGSize(width: 1280, height: 800)
        var buffer: CVPixelBuffer?
        CVPixelBufferCreate(kCFAllocatorDefault, 1280, 800, kCVPixelFormatType_32BGRA,
                            [kCVPixelBufferCGImageCompatibilityKey: true,
                             kCVPixelBufferCGBitmapContextCompatibilityKey: true,
                             kCVPixelBufferMetalCompatibilityKey: true,
                             kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer)
        let pixels = buffer!
        CVPixelBufferLockBaseAddress(pixels, [])
        let context = CGContext(data: CVPixelBufferGetBaseAddress(pixels), width: 1280, height: 800,
                                bitsPerComponent: 8, bytesPerRow: CVPixelBufferGetBytesPerRow(pixels),
                                space: CGColorSpaceCreateDeviceRGB(),
                                bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)!
        context.translateBy(x: 0, y: size.height)
        context.scaleBy(x: 1, y: -1)
        UIGraphicsPushContext(context)
        UIColor(red: 0.92, green: 0.94, blue: 0.97, alpha: 1).setFill()
        context.fill(CGRect(origin: .zero, size: size))
        UIColor.white.setFill()
        context.fill(CGRect(x: 100, y: 70, width: 1080, height: 660))
        for (text, y, font) in [("Trip notes", 120.0, UIFont.systemFont(ofSize: 54, weight: .bold)),
                                ("Saturday · 10:30 AM", 235.0, UIFont.systemFont(ofSize: 36)),
                                ("Meet at the station", 320.0, UIFont.systemFont(ofSize: 42)),
                                ("Bring the blue notebook", 415.0, UIFont.systemFont(ofSize: 42)),
                                ("Last shared picture · fixture 01", 605.0, UIFont.systemFont(ofSize: 28))] {
            (text as NSString).draw(at: CGPoint(x: 160, y: y), withAttributes: [.font: font, .foregroundColor: UIColor.black])
        }
        UIGraphicsPopContext()
        CVPixelBufferUnlockBaseAddress(pixels, [])
        return RTCVideoFrame(buffer: RTCCVPixelBuffer(pixelBuffer: pixels), rotation: ._0, timeStampNs: 1)
    }
}

struct ComputerSessionDiagnosticFixtureView: View {
    /// The dock fixture is paired (to nothing reachable) so the real dictation controller accepts its fixture words.
    @StateObject private var model = ConnectionModel(
        saved: ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-dock") ? {
            let credential = try! JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"computer-fixture-session","deviceId":"computer-fixture-device","csrfToken":"computer-fixture-csrf","hostInstallationId":"computer-fixture-host"}"#.utf8))
            return SavedConnection(origin: "https://computer-fixture.invalid", credential: credential, hostName: "Fixture Mac")
        }() : nil, persistConnection: { _ in })
    @State private var isPresented = true
    private let chat: ChatSummary = {
        let data = Data(#"{"conversationId":"computer-fixture","title":"Computer fixture","messageCount":0,"hasUnread":false,"isArchived":false,"isPinned":false}"#.utf8)
        return try! JSONDecoder().decode(ChatSummary.self, from: data)
    }()

    var body: some View {
        Text("Computer viewer closed")
            .fullScreenCover(isPresented: $isPresented) {
                NavigationStack { ComputerSessionView(model: model, chat: chat) } // theme-exempt: Diagnostics fixture host
            }
    }
}
#endif

/// A close glyph with the header's other buttons: no fill of its own, so it
/// reads as one capsule with More.
struct ComputerCloseButton: View {
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            Image(systemName: "xmark")
                .dynamicTypeSize(...DynamicTypeSize.xLarge)
                .frame(minWidth: 44, minHeight: 44)
                .contentShape(Rectangle())
        }
        .accessibilityLabel("Close")
        .accessibilityIdentifier("computer-session-close")
    }
}

/// The session's lease-bound input queue, as the release rule's channel.
extension ComputerSessionModel: ComputerInputChannel {
    func send(_ actions: [ComputerInputAction]) { enqueueInput(actions) }
}

// MARK: - Remote screens (Experimental)

/// A screen of another computer that an agent asked to show, or is showing, over SSH
/// (`GET /api/v1/computer/streams`). View-only.
struct RemoteScreen: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let name: String
    let host: String
    let transport: String?
    /// awaiting_approval, allowed, connecting, live, stopped or failed.
    let state: String
    let reason: String?
    let agent: String
    let intent: String

    var isWaiting: Bool { state == "awaiting_approval" }
    var isRunning: Bool { ["allowed", "connecting", "live"].contains(state) }
    var isLive: Bool { state == "live" }
    /// Stopped ones are gone from the list; a failed one stays so the reason can be read.
    var isListed: Bool { state != "stopped" }

    var transportName: String {
        switch transport {
        case "x11": "X11"
        case "vnc": "VNC"
        case "screenshot": "Screenshots"
        default: ""
        }
    }
}

private struct RemoteScreenList: Decodable, Sendable { let streams: [RemoteScreen] }
private struct RemoteScreenAnswer: Decodable, Sendable { let state: String }
/// `GET /api/v1/computer/streams/{id}/frame?after=N`: the picture, or only its number when nothing changed.
private struct RemoteScreenFrame: Decodable, Sendable {
    let sequence: Int
    let imageBase64: String?
}

private struct ExperimentalLabel: View {
    var body: some View {
        Text("Experimental").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            .accessibilityLabel("Experimental feature")
    }
}

/// Under the computer icon: screens agents added over SSH, each with Stop (or Allow and Decline while it
/// waits for you), as the simulator rows do.
struct RemoteScreensSection: View {
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var model: ConnectionModel
    @State private var screens: [RemoteScreen] = []
    @State private var working: Set<String> = []
    @State private var failures: [String: String] = [:]
    @State private var viewing: RemoteScreen?
    @State private var refresh = 0

    private var identity: String? {
        guard scenePhase == .active, !model.accessEnded else { return nil }
        return model.connection.map { $0.credential.hostInstallationId + ":" + $0.credential.deviceId } ?? "fixture"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if !screens.isEmpty {
                HStack(spacing: 8) {
                    Text("Remote screens").font(.footnote.weight(.semibold)).foregroundStyle(.secondary)
                    ExperimentalLabel()
                }
                .padding(.top, 12).padding(.bottom, 2)
                .accessibilityElement(children: .combine)
                .accessibilityAddTraits(.isHeader)
                ForEach(screens) { screen in row(screen) }
            }
        }
        .task(id: [identity ?? "", String(refresh)]) {
            #if WONDER_DIAGNOSTICS
            if let fixture = Self.fixture { screens = fixture; return }
            #endif
            guard identity != nil, model.connection != nil else { screens = []; return }
            let current = identity
            while !Task.isCancelled {
                let list: RemoteScreenList? = try? await model.manage("/api/v1/computer/streams")
                if let list, !Task.isCancelled, current == identity {
                    screens = list.streams.filter(\.isListed)
                }
                do { try await Task.sleep(for: .seconds(5)) } catch { return }
            }
        }
        .sheet(item: $viewing) { screen in
            RemoteScreenViewer(model: model, screen: screen) { refresh += 1 }
        }
    }

    private func subtitle(_ screen: RemoteScreen) -> String {
        if let failure = failures[screen.id] { return failure }
        switch screen.state {
        case "awaiting_approval": return "\(screen.agent) asks to show \(screen.intent) on \(screen.host)."
        case "failed": return screen.reason ?? "This screen stopped."
        case "connecting", "allowed": return "Connecting to \(screen.host)…"
        default: return [screen.host, screen.transportName].filter { !$0.isEmpty }.joined(separator: " · ")
        }
    }

    private func row(_ screen: RemoteScreen) -> some View {
        let busy = working.contains(screen.id)
        return HStack(spacing: 0) {
            Button {
                if screen.isLive { viewing = screen }
            } label: {
                HStack(spacing: 10) {
                    Image(systemName: "display").frame(width: 24).accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 6) {
                            Text(screen.name).lineLimit(2)
                            if screen.isLive {
                                Circle().fill(Color.green).frame(width: 8, height: 8).accessibilityHidden(true)
                            }
                        }
                        Text(subtitle(screen)).font(.caption).foregroundStyle(.secondary)
                            .lineLimit(3).fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer(minLength: 8)
                    if busy { ProgressView().controlSize(.small) }
                }
                .padding(.vertical, 4).frame(minHeight: 52).contentShape(Rectangle())
            }
            .disabled(!screen.isLive)
            .accessibilityLabel(screen.name + ", " + subtitle(screen))
            .accessibilityHint(screen.isLive ? "Shows this screen, view only" : "")
            .accessibilityIdentifier("remote-screen:" + screen.id)
            if screen.isWaiting {
                action("Decline", screen, "decline", prominent: false)
                action("Allow once", screen, "allow", prominent: true)
            } else if screen.isRunning {
                action("Stop", screen, "stop", prominent: true)
            }
        }
    }

    private func action(_ title: String, _ screen: RemoteScreen, _ verb: String, prominent: Bool) -> some View {
        Button {
            Task { await send(verb, screen) }
        } label: {
            Text(title).font(.footnote.weight(.semibold)).foregroundStyle(prominent ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
                .frame(minWidth: 44, minHeight: 44, alignment: .trailing).contentShape(Rectangle())
        }
        .disabled(working.contains(screen.id))
        .accessibilityLabel("\(title) \(screen.name)")
        .accessibilityIdentifier("remote-screen-\(verb):" + screen.id)
    }

    /// Allow, Decline or Stop: the computer answers once it has done it, so a failure says so here.
    private func send(_ verb: String, _ screen: RemoteScreen) async {
        working.insert(screen.id)
        failures[screen.id] = nil
        defer { working.remove(screen.id) }
        #if WONDER_DIAGNOSTICS
        if Self.fixture != nil {
            try? await Task.sleep(for: .seconds(1))
            screens = screens.compactMap { item in
                guard item.id == screen.id else { return item }
                switch verb {
                case "allow": return RemoteScreen(id: item.id, name: item.name, host: item.host, transport: item.transport, state: "live", reason: nil, agent: item.agent, intent: item.intent)
                default: return nil
                }
            }
            return
        }
        #endif
        do {
            let _: RemoteScreenAnswer = try await model.manage("/api/v1/computer/streams/\(screen.id)/\(verb)", method: "POST", body: Data("{}".utf8))
            refresh += 1
        } catch {
            guard !Task.isCancelled else { return }
            if case PairingFailure.response(404) = error { refresh += 1; return }
            failures[screen.id] = managementError(error)
        }
    }

    #if WONDER_DIAGNOSTICS
    /// `-diagnostics-remote-screens-fixture`: one waiting for you, one live and one that failed.
    static var fixture: [RemoteScreen]? {
        guard ProcessInfo.processInfo.arguments.contains("-diagnostics-remote-screens-fixture") else { return nil }
        return [
            RemoteScreen(id: "fixture-waiting", name: "build-box", host: "build-box", transport: nil, state: "awaiting_approval", reason: nil,
                         agent: "Claude · Fix the flaky test", intent: "an empty private display (1280×800), stopped with the stream"),
            RemoteScreen(id: "fixture-live", name: "Staging desktop", host: "staging", transport: "x11", state: "live", reason: nil,
                         agent: "Codex · Check the deploy", intent: "the screen of display :0"),
            RemoteScreen(id: "fixture-failed", name: "gpu-1", host: "gpu-1", transport: "vnc", state: "failed",
                         reason: "That VNC server needs a password, which Cupola does not hold. Use x11, or a VNC server without a password on the computer’s loopback.",
                         agent: "Claude · Render check", intent: "the screen served on VNC port 5900"),
        ]
    }
    #endif
}

/// The latest picture of one remote screen, view only, refreshed about once a second while it is open.
private struct RemoteScreenViewer: View {
    @Environment(\.dismiss) private var dismiss
    @ObservedObject var model: ConnectionModel
    let screen: RemoteScreen
    let changed: () -> Void
    @State private var image: UIImage?
    @State private var sequence = 0
    @State private var message = "Waiting for the first picture…"
    @State private var stopping = false

    var body: some View {
        NavigationStack {
            ZStack {
                if let image {
                    Image(uiImage: image).resizable().scaledToFit()
                        .accessibilityLabel("Screen of \(screen.host), view only")
                        .accessibilityIdentifier("remote-screen-image")
                } else {
                    Text(message).font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
                        .padding(24).accessibilityIdentifier("remote-screen-message")
                }
            }
            .wonderPage()
            .navigationTitle(screen.name)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) { ExperimentalLabel() }
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Stop") { Task { await stop() } }
                        .disabled(stopping)
                        .accessibilityIdentifier("remote-screen-viewer-stop")
                }
            }
        }
        .task { await poll() }
        .presentationDragIndicator(.visible)
    }

    private func poll() async {
        #if WONDER_DIAGNOSTICS
        if RemoteScreensSection.fixture != nil {
            image = UIGraphicsImageRenderer(size: CGSize(width: 640, height: 400)).image { context in
                UIColor(red: 0.12, green: 0.16, blue: 0.22, alpha: 1).setFill()
                context.fill(CGRect(x: 0, y: 0, width: 640, height: 400))
                UIColor(white: 0.9, alpha: 1).setFill()
                context.fill(CGRect(x: 40, y: 40, width: 360, height: 24))
                context.fill(CGRect(x: 40, y: 80, width: 520, height: 12))
                context.fill(CGRect(x: 40, y: 104, width: 440, height: 12))
            }
            return
        }
        #endif
        while !Task.isCancelled {
            do {
                let frame: RemoteScreenFrame = try await model.manage("/api/v1/computer/streams/\(screen.id)/frame?after=\(sequence)")
                sequence = frame.sequence
                if let encoded = frame.imageBase64, let data = Data(base64Encoded: encoded), let next = UIImage(data: data) {
                    image = next
                }
            } catch {
                guard !Task.isCancelled else { return }
                if case PairingFailure.response(404) = error {
                    // No picture yet, or the screen has ended.
                    message = image == nil ? "Waiting for the first picture…" : "This screen has stopped."
                    if image != nil { image = nil }
                } else {
                    message = managementError(error)
                }
            }
            do { try await Task.sleep(for: .seconds(1)) } catch { return }
        }
    }

    private func stop() async {
        stopping = true
        defer { stopping = false }
        #if WONDER_DIAGNOSTICS
        if RemoteScreensSection.fixture != nil { dismiss(); return }
        #endif
        do {
            let _: RemoteScreenAnswer = try await model.manage("/api/v1/computer/streams/\(screen.id)/stop", method: "POST", body: Data("{}".utf8))
            changed()
            dismiss()
        } catch {
            guard !Task.isCancelled else { return }
            message = managementError(error)
            image = nil
        }
    }
}
