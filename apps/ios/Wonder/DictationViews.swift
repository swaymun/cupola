import SwiftUI
@preconcurrency import AVFoundation
@preconcurrency import Speech
import WonderPairing

@MainActor protocol DictationEditor: AnyObject {
    var dictationLocale: String { get }
    func beginDictation() -> Bool
    func showDictation(_ words: String) -> Bool
    func endDictation(commitWords: Bool) -> Bool
}

/// A dictation problem, shown only in the chat it happened in.
struct DictationNotice: Equatable {
    let message: String
    let conversationID: String?
    /// Microphone access was denied; the notice offers Settings.
    var opensSettings = false
}

@MainActor final class DictationController: NSObject, ObservableObject {
    @Published private(set) var intent: DictationIntent?
    @Published private(set) var elapsed: TimeInterval = 0
    @Published private(set) var busy = false
    @Published private(set) var notice: DictationNotice?
    @Published private(set) var nativeConversationID: String?
    private weak var editor: (any DictationEditor)?
    private var editorConversationID: String?
    private var nativeCapture: NativeSpeechCapture?
    private var nativeSession: NativeSpeechSession?
    private var transcript = DictationTranscript()
    @Published private(set) var preparingConversationID: String?
    @Published private(set) var preparationStatus: String?
    private var preparation: Task<Void, Never>?
    private var finalization: Task<Void, Never>?
    private var draining: Task<Void, Never>?
    private var nativeWords = "" {
        didSet { if heardWords != !nativeWords.isEmpty { heardWords = !nativeWords.isEmpty } }
    }
    /// Recognized words not yet committed to the draft; they count as content for Send.
    @Published private(set) var heardWords = false
    /// Senders waiting for a capture to retire; resumed with whether its words reached the draft.
    private var retirementWaiters: [CheckedContinuation<Bool, Never>] = []
    private var nativeStartedAt: Date?
    private var nativeFailed = false
    private weak var model: ConnectionModel?
    /// This capture's claim on the shared audio session. While it holds one,
    /// reading aloud stops and Read aloud says why it is unavailable.
    private var audioLease: AudioSessionOwner.Lease? {
        didSet {
            guard (audioLease == nil) != (oldValue == nil) else { return }
            if audioLease != nil { SpeechReader.shared.claimMicrophone(self, for: .dictation) }
            else { SpeechReader.shared.releaseMicrophone(self) }
        }
    }
    private var timer: Task<Void, Never>?
    private var interruptions: Task<Void, Never>?
    private var routeChanges: Task<Void, Never>?
    private var scope: String?
    private var maximum: TimeInterval = 600
    private var captureGeneration = 0
    var recording: Bool { intent?.phase == "recording" }
    var processing: Bool { ["uploading", "processing"].contains(intent?.phase ?? "") }
    init(model: ConnectionModel) { self.model = model; super.init() }

    func restore(force: Bool = false) {
        guard let model else { return }
        guard force || scope != model.assignmentScope else { return }
        cancelPreparation()
        discardNative(); stopCapture()
        scope = model.assignmentScope; intent = nil; busy = false; notice = nil
        // Legacy Mac recordings remain untouched on disk; new dictation is ephemeral.
    }
    func connectionChanged() {
        cancelPreparation(); discardNative(); stopCapture()
        timer?.cancel(); scope = nil; intent = nil; busy = false
    }
    func foreground(_ active: Bool) {
        if !active {
            if preparingConversationID != nil, UIApplication.shared.applicationState != .background { return }
            cancelPreparation()
            finishNativeImmediately()
        } else { restore(); refreshMicrophonePermission() }
    }
    /// The notice for this chat; another chat's problem stays with that chat.
    func visibleNotice(for conversationID: String) -> DictationNotice? {
        notice?.conversationID == conversationID ? notice : nil
    }
    /// Access granted in Settings retires the denied notice. Only reads the
    /// permission; never asks for it.
    func refreshMicrophonePermission() {
        guard notice?.opensSettings == true, AVAudioApplication.shared.recordPermission == .granted else { return }
        notice = nil
    }
    private func report(_ message: String, in conversationID: String?, opensSettings: Bool = false) {
        notice = DictationNotice(message: message, conversationID: conversationID, opensSettings: opensSettings)
    }
    func captureControlsHidden(conversationID: String) {
        if preparingConversationID == conversationID { cancelPreparation() }
        if nativeConversationID == conversationID { finishNativeImmediately() }
    }
    /// Dictation blocks Send only while the microphone starts. Recording and the
    /// recognizer's drain do not: Send finishes the capture first (`finishForSending`).
    func blocksSending(conversationID: String) -> Bool {
        preparingConversationID == conversationID
    }
    func capturing(conversationID: String) -> Bool {
        nativeConversationID == conversationID && intent?.conversationID == conversationID && recording
    }
    /// This conversation's capture is recording or draining its final words.
    func ownsCapture(conversationID: String) -> Bool {
        nativeConversationID == conversationID && intent?.conversationID == conversationID && (recording || processing)
    }
    func hasUncommittedWords(conversationID: String) -> Bool { ownsCapture(conversationID: conversationID) && heardWords }
    /// Send during dictation: stop the microphone (if still recording), wait for the
    /// recognizer's final words (bounded by the same 2 s limit as Finish) and commit
    /// them to the draft. Returns false when the words could not be committed, so
    /// nothing partial is sent.
    func finishForSending(conversationID: String) async -> Bool {
        guard ownsCapture(conversationID: conversationID) else { return true }
        return await withCheckedContinuation { continuation in
            retirementWaiters.append(continuation)
            finishNative()
        }
    }
    private func resumeRetirementWaiters(committed: Bool) {
        let waiters = retirementWaiters; retirementWaiters = []
        for waiter in waiters { waiter.resume(returning: committed) }
    }
    func begin(chat: ChatSummary) {
        guard !busy, intent == nil else { return }
        restore()
        guard intent == nil else { return }
        preparation = Task { [weak self] in await self?.start(chat: chat) }
    }
    private func cancelPreparation() {
        captureGeneration += 1
        preparation?.cancel(); preparation = nil
        if preparingConversationID != nil {
            preparingConversationID = nil; preparationStatus = nil; busy = false
            let session = nativeSession; nativeSession = nil
            Task { await session?.cancel() }
        }
    }
    private func start(chat: ChatSummary) async {
        guard let model, !model.previewMode, !busy, intent == nil, !model.accessEnded, !Task.isCancelled else { return }
        // A new attempt retires any earlier problem, in this chat or another.
        busy = true; notice = nil
        // The mic button's spinner covers the usual short start. Only a
        // language download, which can take a while, is explained in text.
        preparingConversationID = chat.id; preparationStatus = nil
        let initialScope = model.assignmentScope, generation = captureGeneration
        defer {
            if initialScope == model.assignmentScope, generation == captureGeneration {
                busy = false; preparingConversationID = nil; preparationStatus = nil; preparation = nil
            }
        }
        _ = await startNative(chat: chat, scope: initialScope, generation: generation)
    }
    func attachEditor(_ editor: any DictationEditor, conversationID: String) {
        guard nativeConversationID == nil || (editorConversationID == conversationID && self.editor === editor) else { return }
        self.editor = editor; editorConversationID = conversationID
    }
    func cancelNativeForExternalEdit() {
        guard let conversationID = nativeConversationID else { return }
        discardNative(); stopCapture(); clear()
        report("Dictation stopped because the draft changed. Your current draft is kept.", in: conversationID)
    }
    private func startNative(chat: ChatSummary, scope: String, generation: Int) async -> Bool {
        guard editorConversationID == chat.id, let editor else { return false }
        let session = NativeSpeechSession()
        nativeSession = session
        func ownsPreparation() -> Bool {
            model?.assignmentScope == scope && captureGeneration == generation &&
                model?.accessEnded == false && !Task.isCancelled && editorConversationID == chat.id && self.editor === editor
        }
        do {
            let granted = await withCheckedContinuation { continuation in
                AVAudioApplication.requestRecordPermission { @Sendable in continuation.resume(returning: $0) }
            }
            guard ownsPreparation() else { await session.cancel(); return true }
            guard granted else {
                await session.cancel(); nativeSession = nil
                report("Allow microphone access in Settings to dictate.", in: chat.id, opensSettings: true); return true
            }
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-dictation-delayed-preparation") {
                try await Task.sleep(for: .seconds(6))
                guard ownsPreparation() else { await session.cancel(); return true }
            }
            #endif
            let supported = try await session.prepare(locale: Locale(identifier: editor.dictationLocale)) { [weak self] in
                await MainActor.run {
                    guard let self, self.captureGeneration == generation else { return }
                    self.preparationStatus = "Downloading speech language…"
                }
            }
            guard ownsPreparation() else { await session.cancel(); return true }
            guard supported else {
                await session.cancel(); nativeSession = nil
                report("On-device dictation is unavailable for this language or device. You can use the keyboard microphone.", in: chat.id)
                return false
            }
            guard await Self.waitForPermissionDismissal(), ownsPreparation(),
                  let model, let saved = model.connection else {
                await session.cancel(); return true
            }
            audioLease = try AudioSessionOwner.shared.acquire(.dictation) { [weak self] lease in
                Task { @MainActor [weak self] in self?.audioSessionTaken(lease) }
            }
            let pending = DictationIntent(hostID: saved.credential.hostInstallationId, deviceID: saved.credential.deviceId,
                conversationID: chat.id, conversationTitle: chat.title, modelID: "native")
            guard editor.beginDictation() else { throw CancellationError() }
            intent = pending; nativeConversationID = chat.id
            nativeWords = ""; transcript = DictationTranscript(); nativeFailed = false
            let capture = try await session.start() { [weak self] event in
                await self?.receiveNative(event, pending: pending)
            }
            guard ownsPreparation(), matches(pending), nativeConversationID == chat.id else {
                await session.cancel(); return true
            }
            nativeCapture = capture
            nativeStartedAt = Date(); elapsed = 0; maximum = 600
            startTimer(); observeInterruptions()
            return true
        } catch {
            await session.cancel()
            guard ownsPreparation() else { return true }
            discardNative(); stopCapture(); clear()
            report("Dictation could not start. Check your speech language and try again.", in: chat.id)
            return false
        }
    }
    private static func waitForPermissionDismissal() async -> Bool {
        // TCC may return before its dialog restores the active state. Wait only
        // for that short transition; never resume recording from the background.
        for _ in 0..<20 {
            guard !Task.isCancelled else { return false }
            switch UIApplication.shared.applicationState {
            case .active: return true
            case .background: return false
            default: break
            }
            do { try await Task.sleep(for: .milliseconds(100)) }
            catch { return false }
        }
        return false
    }
    private func receiveNative(_ event: NativeSpeechEvent, pending: DictationIntent) {
        guard matches(pending), nativeConversationID == pending.conversationID else { return }
        switch event {
        case let .words(words, start, end, final):
            guard transcript.update(words, start: start, end: end, isFinal: final) else { return }
            let cumulative = transcript.text
            guard editor?.showDictation(cumulative) == true else {
                finishNativeImmediately()
                report("Dictation stopped. The draft could not accept more text.", in: pending.conversationID)
                return
            }
            nativeWords = cumulative
        case .failed:
            nativeFailed = true; finishNativeImmediately()
        }
    }
    #if WONDER_DIAGNOSTICS
    /// Drives the real editor/session lifecycle without microphone or network.
    @discardableResult func beginNativeFixture(chat: ChatSummary) -> DictationIntent? {
        guard intent == nil, editorConversationID == chat.id, let saved = model?.connection,
              editor?.beginDictation() == true else { return nil }
        let pending = DictationIntent(hostID: saved.credential.hostInstallationId, deviceID: saved.credential.deviceId,
            conversationID: chat.id, conversationTitle: chat.title, modelID: "native")
        intent = pending; nativeConversationID = chat.id; nativeWords = ""; nativeFailed = false; transcript = DictationTranscript()
        observeInterruptions()
        return pending
    }
    func receiveNativeFixture(_ words: String, pending: DictationIntent, final: Bool = false, failed: Bool = false,
                              start: Double = 0, end: Double = 1) {
        receiveNative(.words(words, start: start, end: end, final: final), pending: pending)
        if failed { receiveNative(.failed, pending: pending) }
    }
    #endif
    private func finishNative() {
        guard nativeConversationID != nil, finalization == nil else { return }
        stopNativeAudio()
        intent?.phase = "processing"
        let id = intent?.requestID
        let session = nativeSession
        draining = Task { [weak self] in
            guard let session else { return }
            await session.finish()
            guard let self, self.intent?.requestID == id else { return }
            self.finishNativeImmediately()
        }
        finalization = Task { [weak self] in
            do { try await Task.sleep(for: .seconds(2)) } catch { return }
            guard let self, self.intent?.requestID == id else { return }
            self.finishNativeImmediately()
        }
    }
    func finishNativeImmediately() {
        guard nativeConversationID != nil else { return }
        // Retire callback ownership before cancelling the recognizer.
        let hasWords = !nativeWords.isEmpty
        let conversationID = intent?.conversationID ?? nativeConversationID
        let failed = nativeFailed
        nativeConversationID = nil
        finalization?.cancel(); finalization = nil
        draining?.cancel(); draining = nil
        let session = nativeSession; nativeSession = nil
        Task { await session?.cancel() }
        stopCapture()
        if hasWords {
            guard editor?.endDictation(commitWords: true) == true else {
                // The editor retains the projection until its binding accepts it.
                nativeConversationID = intent?.conversationID
                intent?.phase = "recording"
                report("Dictation stopped. Finish to save the words, or cancel to keep the original draft.", in: conversationID)
                resumeRetirementWaiters(committed: false)
                return
            }
            clear()
            if failed { report("On-device dictation stopped. The words shown were kept in your draft.", in: conversationID) }
        } else {
            _ = editor?.endDictation(commitWords: false)
            clear()
            if failed { report("Dictation stopped before words were recognized. Try again.", in: conversationID) }
        }
        nativeWords = ""
        resumeRetirementWaiters(committed: true)
    }
    private func discardNative() {
        nativeConversationID = nil
        finalization?.cancel(); finalization = nil
        draining?.cancel(); draining = nil
        let session = nativeSession; nativeSession = nil
        Task { await session?.cancel() }
        _ = editor?.endDictation(commitWords: false)
        nativeWords = ""
        resumeRetirementWaiters(committed: false)
    }
    func finishCapture(submit: Bool = true) {
        if submit { finishNative() } else { finishNativeImmediately() }
    }
    private func observeInterruptions() {
        routeChanges?.cancel()
        routeChanges = Task { [weak self] in
            for await notification in NotificationCenter.default.notifications(named: AVAudioSession.routeChangeNotification) {
                guard !Task.isCancelled else { return }
                let reason = notification.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt
                guard reason == AVAudioSession.RouteChangeReason.oldDeviceUnavailable.rawValue ||
                      reason == AVAudioSession.RouteChangeReason.newDeviceAvailable.rawValue else { continue }
                if self?.nativeConversationID != nil { self?.finishNativeImmediately() }
                else if self?.recording == true { self?.finishCapture(submit: false) }
            }
        }
        interruptions?.cancel()
        interruptions = Task { [weak self] in
            for await _ in NotificationCenter.default.notifications(named: AVAudioSession.interruptionNotification) {
                guard !Task.isCancelled else { return }
                if self?.nativeConversationID != nil { self?.finishNativeImmediately() }
                else if self?.recording == true { self?.finishCapture(submit: false) }
            }
        }
    }
    private func startTimer() {
        timer?.cancel()
        timer = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, self.intent != nil else { return }
                if self.recording, let started = self.nativeStartedAt {
                    self.elapsed = Date().timeIntervalSince(started)
                    if self.elapsed >= self.maximum { self.finishCapture() }
                }
                do { try await Task.sleep(for: .seconds(1)) } catch { return }
            }
        }
    }
    func cancel() {
        cancelPreparation(); discardNative(); stopCapture(); clear()
    }
    func clear() {
        timer?.cancel(); interruptions?.cancel(); routeChanges?.cancel()
        intent = nil; busy = false; notice = nil
    }
    func forget() { cancel() }
    private func stopNativeAudio() {
        if let capture = nativeCapture {
            capture.stop()
        }
        nativeCapture = nil; nativeStartedAt = nil
    }
    private func stopCapture() {
        stopNativeAudio()
        interruptions?.cancel(); interruptions = nil
        routeChanges?.cancel(); routeChanges = nil
        // Stopping the recorder alone does not release the app's audio session.
        // Revocation and connection replacement must release it just like Finish.
        // Restoring an idle controller must not activate the audio service at launch.
        // Only this capture's own claim is released; a newer owner keeps it.
        if let audioLease {
            AudioSessionOwner.shared.release(audioLease)
            self.audioLease = nil
        }
    }
    /// Voice mode took the audio session: keep the words heard so far.
    private func audioSessionTaken(_ lease: AudioSessionOwner.Lease) {
        guard audioLease == lease else { return }
        audioLease = nil
        finishNativeImmediately()
    }
    private func matchingConnection(_ pending: DictationIntent) -> SavedConnection? {
        guard let model, !model.accessEnded, let saved = model.connection,
            saved.credential.hostInstallationId == pending.hostID, saved.credential.deviceId == pending.deviceID else { return nil }
        return saved
    }
    private func matches(_ pending: DictationIntent) -> Bool { intent?.requestID == pending.requestID && matchingConnection(pending) != nil }
}

struct DictationControls: View {
    @ObservedObject var controller: DictationController
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if controller.preparingConversationID == chat.id, let status = controller.preparationStatus {
                Text(status).font(.caption).foregroundStyle(.secondary)
            }
            if let notice = controller.visibleNotice(for: chat.id) {
                FailureDetails("Dictation unavailable", message: notice.message)
                if notice.opensSettings { Button("Open Settings") { if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) } }.font(.caption) }
            }
        }
        .task(id: model.assignmentScope) { controller.restore(); controller.refreshMicrophonePermission() }
        // Permission sheets also make scenes inactive. The app-level background
        // notification retires preparation even if scene activity was already false.
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didEnterBackgroundNotification)) { _ in
            controller.foreground(false)
        }
    }
}

/// Only sending controls need to redraw when capture ownership changes.
/// Their content also re-evaluates the normal Send/Guide eligibility and styling.
struct DictationSendControls<Content: View>: View {
    @ObservedObject var controller: DictationController
    let conversationID: String
    @ViewBuilder var content: () -> Content
    var body: some View {
        content().disabled(controller.blocksSending(conversationID: conversationID))
    }
}

struct DictationButton: View {
    @ObservedObject var controller: DictationController
    let chat: ChatSummary
    let unavailable: Bool
    var prepare: () -> Bool = { true }
    private var ownsCapture: Bool { controller.intent?.conversationID == chat.id && controller.recording }
    private var preparing: Bool { controller.preparingConversationID == chat.id }
    private var finishing: Bool { controller.intent?.conversationID == chat.id && controller.processing }
    var body: some View {
        Button {
            if preparing { controller.cancel() }
            else if ownsCapture { controller.finishCapture() }
            else if finishing { controller.cancel() }
            else if prepare() { controller.begin(chat: chat) }
        } label: {
            ComposerVoiceActionLabel(title: preparing || finishing ? "Cancel" : (ownsCapture ? "Stop" : "Dictate"),
                systemImage: ownsCapture ? "mic.fill" : "mic", loading: preparing || finishing, recording: ownsCapture)
        }
        .accessibilityLabel(preparing ? "Cancel dictation preparation" : (ownsCapture ? "Stop dictation" : (finishing ? "Cancel dictation" : "Dictate message")))
        .accessibilityValue(ownsCapture ? "Recording" : (finishing ? "Finishing" : "Idle"))
        .accessibilityIdentifier("dictate-message")
        .accessibilityAddTraits(ownsCapture ? .isSelected : [])
        .disabled(!ownsCapture && !preparing && !finishing && (unavailable || controller.busy || controller.intent != nil))
        .contextMenu {
            if ownsCapture || preparing || finishing {
                Button("Cancel dictation", role: .destructive) { controller.cancel() }
                    .accessibilityIdentifier("cancel-dictation")
            } else {
                Button("Dictate message", systemImage: "mic") {
                    if prepare() { controller.begin(chat: chat) }
                }
            }
        }
        .accessibilityAction(named: "Cancel dictation") {
            if ownsCapture || preparing || finishing { controller.cancel() }
        }
    }
}

private enum NativeSpeechEvent: Sendable {
    case words(String, start: Double, end: Double, final: Bool)
    case failed
}

/// Asset loading, engine setup and result consumption stay off the main actor.
private actor NativeSpeechSession {
    private var analyzer: SpeechAnalyzer?
    private var transcriber: SpeechTranscriber?
    private var dictationTranscriber: DictationTranscriber?
    private var module: (any SpeechModule)?
    private var results: Task<Void, Never>?
    private var installation: AssetInstallationRequest?
    private var capture: NativeSpeechCapture?
    private var cancelled = false

    func prepare(locale: Locale, downloading: @Sendable () async -> Void) async throws -> Bool {
        let module: any SpeechModule
        if SpeechTranscriber.isAvailable,
           let supported = await SpeechTranscriber.supportedLocale(equivalentTo: locale) {
            let speech = SpeechTranscriber(locale: supported, preset: .progressiveTranscription)
            transcriber = speech; module = speech
        } else if let supported = await DictationTranscriber.supportedLocale(equivalentTo: locale) {
            let dictation = DictationTranscriber(locale: supported, preset: .progressiveLongDictation)
            dictationTranscriber = dictation; module = dictation
        } else { return false }
        try checkCancellation()
        let status = await AssetInventory.status(forModules: [module])
        try checkCancellation()
        guard status != .unsupported else { return false }
        if let request = try await AssetInventory.assetInstallationRequest(supporting: [module]) {
            try checkCancellation()
            installation = request
            await downloading()
            try await request.downloadAndInstall()
            installation = nil
        }
        try checkCancellation()
        self.module = module
        analyzer = SpeechAnalyzer(modules: [module])
        return true
    }
    func start(receive: @escaping @Sendable (NativeSpeechEvent) async -> Void) async throws -> NativeSpeechCapture {
        try checkCancellation()
        guard let analyzer, let module,
              let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [module]) else {
            throw DictationFailure(errorCategory: "no_audio")
        }
        try checkCancellation()
        try await analyzer.prepareToAnalyze(in: format)
        try checkCancellation()
        let (stream, continuation) = AsyncThrowingStream<AnalyzerInput, Error>.makeStream(bufferingPolicy: .bufferingOldest(64))
        let capture = try NativeSpeechCapture(format: format, input: continuation)
        self.capture = capture
        results = Task {
            do {
                if let transcriber {
                    for try await result in transcriber.results {
                        try Task.checkCancellation()
                        await receive(.words(String(result.text.characters), start: result.range.start.seconds,
                                             end: result.range.end.seconds, final: result.isFinal))
                    }
                } else if let dictationTranscriber {
                    for try await result in dictationTranscriber.results {
                        try Task.checkCancellation()
                        await receive(.words(String(result.text.characters), start: result.range.start.seconds,
                                             end: result.range.end.seconds, final: result.isFinal))
                    }
                }
            } catch { if !Task.isCancelled { await receive(.failed) } }
        }
        try await analyzer.start(inputSequence: stream)
        try checkCancellation()
        try capture.start()
        return capture
    }
    func finish() async {
        capture?.stop()
        do { try await analyzer?.finalizeAndFinishThroughEndOfInput() }
        catch { /* The controller commits the last visible words on timeout/error. */ }
        await results?.value
    }
    func cancelRecognition() async {
        results?.cancel(); results = nil
        await analyzer?.cancelAndFinishNow()
        analyzer = nil; transcriber = nil; dictationTranscriber = nil; module = nil
    }
    func cancel() async {
        cancelled = true
        installation?.progress.cancel(); installation = nil
        capture?.stop(); capture = nil
        await cancelRecognition()
    }
    private func checkCancellation() throws {
        try Task.checkCancellation()
        if cancelled { throw CancellationError() }
    }
}

/// The analyzer receives owned buffers. Overflow stops recognition without
/// dropping audio silently or replacing the last visible words.
private final class NativeSpeechCapture: @unchecked Sendable {
    private let engine = AVAudioEngine()
    private var input: AsyncThrowingStream<AnalyzerInput, Error>.Continuation?
    private let converter: AVAudioConverter
    private let analysisFormat: AVAudioFormat
    private var tapped = false
    private let fileLock = NSLock()
    private let engineLock = NSLock()
    init(format: AVAudioFormat, input: AsyncThrowingStream<AnalyzerInput, Error>.Continuation) throws {
        self.input = input; analysisFormat = format
        let source = engine.inputNode.outputFormat(forBus: 0)
        guard source.sampleRate > 0, source.channelCount > 0,
              let converter = AVAudioConverter(from: source, to: format) else { throw DictationFailure(errorCategory: "no_audio") }
        self.converter = converter
        // Avoid priming latency and preserve the first spoken samples.
        converter.primeMethod = .none
        engine.inputNode.installTap(onBus: 0, bufferSize: 1024, format: source) { [weak self] buffer, _ in
            self?.consume(buffer)
        }
        tapped = true
    }
    private func consume(_ buffer: AVAudioPCMBuffer) {
        fileLock.lock(); defer { fileLock.unlock() }
        guard let input else { return }
        let capacity = AVAudioFrameCount(ceil(Double(buffer.frameLength) * analysisFormat.sampleRate / buffer.format.sampleRate)) + 32
        guard let output = AVAudioPCMBuffer(pcmFormat: analysisFormat, frameCapacity: capacity) else {
            input.finish(throwing: DictationFailure(errorCategory: "no_audio")); self.input = nil; return
        }
        var supplied = false
        var error: NSError?
        let status = converter.convert(to: output, error: &error) { _, state in
            if supplied { state.pointee = .noDataNow; return nil }
            supplied = true; state.pointee = .haveData; return buffer
        }
        if status == .error || error != nil {
            input.finish(throwing: error ?? DictationFailure(errorCategory: "no_audio") as NSError); self.input = nil
        } else if output.frameLength > 0, case .dropped = input.yield(AnalyzerInput(buffer: output)) {
            input.finish(throwing: DictationFailure(errorCategory: "interrupted")); self.input = nil
        }
    }
    func start() throws {
        engineLock.lock(); defer { engineLock.unlock() }
        guard tapped else { throw CancellationError() }
        engine.prepare(); try engine.start()
    }
    func stop() {
        engineLock.lock(); defer { engineLock.unlock() }
        engine.stop()
        if tapped { engine.inputNode.removeTap(onBus: 0); tapped = false }
        fileLock.lock(); input?.finish(); input = nil; fileLock.unlock()
    }
    deinit { stop() }
}
