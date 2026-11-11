import ActivityKit
import AppIntents
import AVFoundation
import Combine
import SwiftUI
import UIKit
import WonderPairing

// Voice mode: a spoken conversation that replaces the composer. The Mac keeps
// the owner's OpenAI key and mints a short-lived pass per session; this phone
// streams audio to OpenAI Realtime and carries out its tool calls through the
// Mac's existing API. Each open panel has an ID the Mac mints for, limits
// and keeps the transcript under; the session keeps running with the screen
// locked (background audio) with Mute and End on a Live Activity.

// MARK: - Session state

@MainActor final class VoiceController: ObservableObject {
    enum Phase: Equatable {
        case idle, connecting, listening, hearing, working, speaking
        case paused(String)
        case failed(String)
        /// Microphone access is off for Cupola; the panel offers Settings.
        case needsMicrophone
    }

    @Published private(set) var phase: Phase = .idle
    /// The conversation that started this session, pinned until End; nil for standalone voice.
    @Published private(set) var chatID: String?
    @Published private(set) var isActive = false {
        // Voice owns the audio session while active: reading aloud stops.
        didSet {
            guard isActive != oldValue else { return }
            if isActive { SpeechReader.shared.claimMicrophone(self, for: .voice) } else { SpeechReader.shared.releaseMicrophone(self) }
        }
    }
    /// A live or starting session acts through the computer's connection, so
    /// it keeps the connection in the background. A failed one only shows why.
    var needsBackgroundConnection: Bool {
        if case .failed = phase { return false }
        return isActive
    }
    @Published private(set) var muted = false
    /// Settings → Voice, read when voice starts.
    @Published private(set) var talkMode: VoiceTalkMode = .handsFree
    /// Hold to talk: whether the talk button is down or latched by a tap.
    @Published private(set) var talkButton = VoiceTalkButton()
    @Published private(set) var userCaption = ""
    @Published private(set) var agentCaption = ""
    @Published private(set) var activity: String?
    @Published private(set) var pending: VoicePendingAction? {
        didSet { if pending == nil { flushAnnouncements() } }
    }
    // A connection is shared across windows; presenting a sheet in one must
    // not remove the other window's immediately reachable Mute/End controls.
    @Published private var visibleSheets: [UUID: ObjectIdentifier] = [:]
    func sheetVisible(in scene: ShellState) -> Bool { visibleSheets.values.contains(ObjectIdentifier(scene)) }
    func sheetAppeared(_ id: UUID, in scene: ShellState) { visibleSheets[id] = ObjectIdentifier(scene) }
    func sheetDisappeared(_ id: UUID) { visibleSheets.removeValue(forKey: id) }
    @Published private(set) var resolvingPending = false
    /// From the connection to when this phone ends the session; the panel
    /// shows the time left.
    @Published private(set) var sessionWindow: ClosedRange<Date>?
    /// Fast-changing audio level, observed only by the orb.
    let levels = VoiceLevels()

    private weak var model: ConnectionModel?
    /// The computer whose key mints for the open panel (Settings → Voice);
    /// its limits, usage and transcripts apply. Kept with `panelID`.
    private weak var minter: ConnectionModel?
    private weak var scene: ShellState?
    private var connection: VoiceRealtimeConnection?
    private var router: VoiceToolRouter?
    private var audio: VoiceAudio?
    private var sendTask: Task<Void, Never>?
    private var startTask: Task<Void, Never>?
    /// Talk button presses and releases, in order.
    private var talkTask: Task<Void, Never>?
    private var observers: [NSObjectProtocol] = []
    private var generation = 0
    private var lastChat: ChatSummary?
    /// Chats this session talked in, the one it started in first: a spoken
    /// "open" moves the target, and the transcript belongs to each.
    private var visitedChatIDs: [String] = []
    /// Where a spoken "open" moved the target; nil while it is where voice started.
    private var movedTo: VoiceThread?
    /// Set for a few seconds after a spoken "open" moved the target.
    @Published private(set) var targetNotice: String?
    private var noticeTask: Task<Void, Never>?
    private let requestPermission: @MainActor () async -> Bool
    private let microphoneAllowed: @MainActor () -> Bool
    private var realtimeReady = false
    /// The conversation's stage and whether a pause or interruption holds
    /// the microphone; `phase` shows it once the microphone first runs.
    private var turn = VoiceTurn()
    private var presentsTurn = false
    /// The open panel the Mac mints for; kept across Try again, so a
    /// reconnect never extends the session limit. Nil once closed.
    private var panelID: String?
    /// The Mac minted for `panelID`, so it has usage to count and a panel to close.
    private var panelOnMac = false
    private var log = VoiceSessionLog()
    private var actedThreadIDs: [String] = []
    private var limitTask: Task<Void, Never>?
    private var reportTask: Task<Void, Never>?
    private let liveActivity = VoiceLiveActivity()
    /// Speaks up about the pinned chat (a finished reply, an approval or a
    /// question) when the conversation is quiet.
    private var announcer = VoiceAnnouncer(chatID: nil, title: "")
    private var announceTimer: Task<Void, Never>?
    private var announceBaseline: Task<Void, Never>?
    private var attentionBaselineSet = false
    #if WONDER_DIAGNOSTICS
    private var announceScript: Task<Void, Never>?
    #endif
    private var announceSubscriptions: [AnyCancellable] = []
    private var knownAttention = Set<String>()
    #if WONDER_DIAGNOSTICS
    /// What voice announced this session, for scenarios.
    private(set) var announced: [String] = []
    #endif
    private let defaults: UserDefaults
    #if WONDER_DIAGNOSTICS
    private var diagnosticSession: VoiceDiagnosticSession?
    private var diagnosticStartingHistory: (chat: String, snapshot: ConversationSnapshot)?
    var startupForTesting: Task<Void, Never>? { startTask }
    private var toolsFixtureHost: FixtureVoiceHost?
    private var toolsFixtureResults: [String] = []
    var diagnosticState: [String: Any] {
        var snapshot = log
        let transcript = (try? JSONSerialization.jsonObject(with: snapshot.takeTranscript(threadIDs: transcriptThreadIDs).body)) ?? [:]
        return ["active": isActive, "chatID": chatID.map { $0 as Any } ?? NSNull(),
                "targetTitle": targetTitle, "targetNotice": targetNotice.map { $0 as Any } ?? NSNull(),
                "toolResults": toolsFixtureResults, "toolActions": toolsFixtureHost?.log ?? [],
                "muted": muted, "status": statusText,
                "transcript": transcript, "audio": diagnosticAudioRoute, "announced": announced,
                "historyUnchanged": diagnosticStartingHistory.map { model?.snapshots[$0.chat] == $0.snapshot } as Any? ?? NSNull()]
    }
    #endif

    init(model: ConnectionModel, defaults: UserDefaults = .standard, requestPermission: @escaping @MainActor () async -> Bool = {
        await AVAudioApplication.requestRecordPermission()
    }, microphoneAllowed: @escaping @MainActor () -> Bool = { AVAudioApplication.shared.recordPermission == .granted }) {
        self.model = model; self.defaults = defaults; self.requestPermission = requestPermission
        self.microphoneAllowed = microphoneAllowed
    }

    var statusText: String {
        switch phase {
        case .idle: "Voice is off"
        case .connecting: "Connecting…"
        case .listening: muted ? "Muted" : talkMode == .holdToTalk ? "Hold to talk" : "Listening"
        case .hearing:
            switch talkButton.state {
            case .idle: "Hearing you"
            case _ where muted: "Muted. Unmute to talk."
            case .holding: "Release to send"
            case .latched: "Listening. Tap to send."
            }
        case .working: activity ?? "Working on it…"
        case .speaking: "Speaking"
        case .paused(let reason): reason
        case .failed(let message): message
        case .needsMicrophone: "Allow microphone access for Cupola in Settings to talk with voice."
        }
    }

    func start(chat: ChatSummary?, scene: ShellState? = nil) {
        guard let model, !isActive else { return }
        teardown()
        if panelID == nil {
            panelID = makeVoicePanelID(); log = VoiceSessionLog(); actedThreadIDs = []
            minter = VoiceKeyChoice.shared.minter(for: model)
        }
        let minter = self.minter ?? model
        generation += 1
        let run = generation
        self.scene = scene ?? self.scene
        lastChat = chat
        visitedChatIDs = chat.map { [$0.id] } ?? []
        movedTo = nil
        noticeTask?.cancel(); targetNotice = nil
        isActive = true; chatID = chat?.id; phase = .connecting
        userCaption = ""; agentCaption = ""; activity = nil; pending = nil; muted = false
        talkMode = VoiceTalkMode.load(defaults)
        #if WONDER_DIAGNOSTICS
        diagnosticStartingHistory = nil
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-preview") {
            diagnosticStartingHistory = chat.flatMap { chat in
                model.snapshots[chat.id].map { (chat.id, $0) }
            }
        }
        // Fixture launches choose the mode, whatever an earlier run saved.
        if ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("-diagnostics") }) {
            talkMode = ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-hold-to-talk") ? .holdToTalk : .handsFree
        }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-preview")
            || ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-audio") { startPreview(run: run); return }
        #endif
        let context = Self.context(for: chat, model: model)
        let panel = panelID ?? makeVoicePanelID()
        startTask = Task { [weak self] in await self?.connect(model: model, minter: minter, context: context, panel: panel, run: run) }
    }

    /// `model` is the chat's computer, which voice acts on; `minter` is the
    /// one whose OpenAI key starts the session.
    private func connect(model: ConnectionModel, minter: ConnectionModel, context: VoiceContext, panel: String, run: Int) async {
        let host = ConnectionVoiceHost(model: model, scene: scene)
        // Read while the microphone and session start, so a spoken name finds
        // its thread in any project from the first words.
        async let seeded = Self.seeded(context, host: host)
        #if WONDER_DIAGNOSTICS
        let fixture = VoiceDiagnosticSession.load()
        diagnosticSession = fixture
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-session"), fixture == nil {
            return fail("The voice test audio or short-lived pass is missing. Prepare the fixture and try again.", run: run)
        }
        if fixture != nil, model.connection?.credential.hostInstallationId != DiagnosticSubagentFixture.hostID {
            fixture?.stop()
            return fail("Voice test audio is available only on the isolated fixture computer.", run: run)
        }
        let usesMicrophone = fixture == nil
        // Recorded test audio relies on OpenAI noticing the end of each turn.
        if fixture != nil { talkMode = .handsFree }
        #else
        let usesMicrophone = true
        #endif
        let permitted = usesMicrophone ? await requestPermission() : true
        guard permitted else {
            guard run == generation else { return }
            return deniedMicrophone()
        }
        guard run == generation, isActive, !Task.isCancelled else { return }
        let grant: VoiceSessionGrant
        let mint = try? JSONEncoder().encode(["panelId": panel])
        do {
            #if WONDER_DIAGNOSTICS
            if let fixture { grant = fixture.grant }
            else { grant = try await minter.manage("/api/v1/voice/sessions", method: "POST", body: mint) }
            #else
            grant = try await minter.manage("/api/v1/voice/sessions", method: "POST", body: mint)
            #endif
        } catch PairingFailure.hostMessage(_, let detail) {
            return fail(detail, run: run)
        } catch {
            return fail(minter.macConnected == true ? "Voice couldn’t start. \(managementError(error))"
                                                    : "Connect to \(minter.macName) to use voice.", run: run)
        }
        guard run == generation, isActive, !Task.isCancelled else { return }
        #if WONDER_DIAGNOSTICS
        if fixture == nil { panelOnMac = true }
        #else
        panelOnMac = true
        #endif
        let context = await seeded
        guard run == generation, isActive, !Task.isCancelled else { return }
        let askBefore = VoiceAskBefore.load(defaults)
        let router = VoiceToolRouter(host: host, context: context, askBefore: askBefore)
        // The router, audio and connection are owned by this controller, so
        // their stored callbacks hold it weakly; `end()` releases them all.
        await router.observePending { [weak self] action in
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { if self?.generation == run { self?.pending = action } }
            }
        }
        await router.observeTarget { [weak self] thread in
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { if self?.generation == run { self?.retarget(to: thread) } }
            }
        }
        await router.observeConfirmed { [weak self] action, output in
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { if self?.generation == run, let text = Self.outcome(of: action, output: output) { self?.showNotice(text) } }
            }
        }
        guard run == generation, isActive, !Task.isCancelled else { return }
        let audio = usesMicrophone ? makeAudio(run: run) : nil
        let connection = VoiceRealtimeConnection(grant: grant, router: router,
                                                 instructions: VoiceToolRouter.instructions(context: context, askBefore: askBefore),
                                                 talkMode: talkMode) { [weak self, weak audio] event in
            // Audio goes straight to the player in arrival order; everything
            // else reaches the screen through the main queue, also in order.
            if case .assistantAudio(let pcm) = event {
                audio?.play(pcm)
                #if WONDER_DIAGNOSTICS
                if usesMicrophone { return }
                #else
                return
                #endif
            }
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated {
                    guard self?.generation == run else { return }
                    #if WONDER_DIAGNOSTICS
                    self?.diagnosticSession?.record(event)
                    #endif
                    self?.apply(event)
                }
            }
        }
        self.router = router; self.audio = audio; self.connection = connection
        let (chunks, input) = AsyncStream.makeStream(of: Data.self, bufferingPolicy: .bufferingNewest(50))
        let levels = levels
        audio?.onInput = { pcm, level in
            levels.post(input: level)
            if !pcm.isEmpty { input.yield(pcm) }
        }
        do {
            realtimeReady = false
            try await connection.start()
            // A socket send is not a configured session. Keep Connecting and
            // the microphone off until OpenAI acknowledges our tools/config.
            for _ in 0..<150 {
                guard run == generation, isActive, !Task.isCancelled else { return }
                if realtimeReady { break }
                try await Task.sleep(for: .milliseconds(100))
            }
            guard realtimeReady else { throw URLError(.timedOut) }
            guard run == generation, isActive, !Task.isCancelled else { return }
            try audio?.start()
        } catch {
            return fail("Voice couldn’t use the microphone or reach OpenAI.", run: run)
        }
        guard run == generation else { return }
        sendTask = Task { for await pcm in chunks { await connection.appendAudio(pcm) } }
        if usesMicrophone { observeAudioSession(run: run) }
        presentsTurn = true
        showTurn()
        beginSession(grant: grant, run: run)
        #if WONDER_DIAGNOSTICS
        if let fixture {
            sendTask?.cancel()
            sendTask = Task { [weak self] in
                await fixture.run(connection: connection, scene: self?.scene, model: model)
                guard let self, self.generation == run else { return }
                self.end()
            }
        }
        #endif
    }

    /// Both live voice and the no-network hardware fixture use these audio
    /// callbacks and the same interruption/route recovery owner.
    private func makeAudio(run: Int) -> VoiceAudio {
        let audio = VoiceAudio()
        let levels = levels
        audio.onOutputLevel = { level in levels.post(output: level) }
        audio.onCaptureLost = { [weak self] in
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated {
                    guard let self, self.generation == run else { return }
                    self.hold(.paused)
                }
            }
        }
        audio.onPreempted = { [weak self] in
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated {
                    guard let self, self.generation == run else { return }
                    self.hold(.paused)
                }
            }
        }
        audio.onPlaybackChanged = { [weak self] playing in
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { guard self?.generation == run else { return }; self?.playbackChanged(playing) }
            }
        }
        return audio
    }

    var targetTitle: String { movedTo?.title ?? lastChat?.title ?? "Cupola Voice" }
    var targetLabel: String {
        guard chatID != nil else { return "Cupola Voice" }
        switch phase {
        case .failed, .needsMicrophone: return "Voice in \(targetTitle)"
        default: return targetNotice ?? "Talking in \(targetTitle)"
        }
    }
    var targetHostID: String? { model?.connection?.credential.hostInstallationId }
    var computerName: String { model?.macName ?? "" }
    var canMute: Bool {
        switch phase { case .idle, .connecting, .failed, .needsMicrophone: false; default: true }
    }

    func showsCompactControls(in scene: ShellState) -> Bool {
        isActive && (chatID == nil || scene.selectedConversation?.id != chatID
            || scene.selectedConversation?.host != targetHostID
            || scene.sidebarOpen || scene.settingsOpen || scene.page != nil)
    }

    #if WONDER_DIAGNOSTICS
    /// The fixture invokes the same controller actions as the rendered controls;
    /// AX assertions separately check their labels and visible layout on the Duo.
    func diagnosticControls(scene: ShellState?, compact: Bool) -> [DiagnosticTestDriver.Control] {
        guard isActive else { return [] }
        var controls: [DiagnosticTestDriver.Control] = []
        if compact, chatID != nil {
            controls.append(.init(id: "voice-target:" + (targetHostID ?? "standalone"), label: targetLabel,
                                  tap: { self.showTarget(in: scene) }))
        }
        controls.append(.init(id: compact ? "voice-bar-mute:" + (targetHostID ?? "standalone") : "voice-mute",
                              label: compact ? "\(muted ? "Unmute" : "Mute") voice in \(targetTitle)" : (muted ? "Unmute" : "Mute"),
                              enabled: canMute, tap: { self.toggleMute() }))
        controls.append(.init(id: compact ? "voice-bar-end:" + (targetHostID ?? "standalone") : "voice-end",
                              label: compact ? "End voice in \(targetTitle)" : "End voice", tap: { self.end() }))
        if !compact, let pending {
            controls.append(.init(id: "voice-confirmation-cancel", label: "Cancel",
                                  tap: { self.resolvePending(pending, confirm: false) }))
        }
        return controls
    }
    #endif

    /// Navigation changes the window, never the session's conversation.
    func showTarget(in scene: ShellState? = nil) {
        guard isActive, let chatID, let host = model?.connection?.credential.hostInstallationId else { return }
        (scene ?? self.scene)?.open(host: host, conversation: chatID)
    }

    /// Hold to talk is chosen and the conversation can take a turn now (not
    /// connecting, paused or failed).
    var talkAvailable: Bool {
        guard talkMode == .holdToTalk else { return false }
        switch phase { case .listening, .hearing, .working, .speaking: return true; default: return false }
    }

    /// Hold to talk's button went down, came up, or VoiceOver activated it.
    /// It only starts and sends turns: a proposal still needs Confirm or the
    /// owner's spoken words.
    func talkPressed() {
        guard talkAvailable else { return }
        talk(talkButton.press(at: Date()))
    }

    func talkReleased() { talk(talkButton.release(at: Date())) }

    func talkToggled() {
        guard talkAvailable || talkButton.talking else { return }
        talk(talkButton.toggle())
    }

    private func talk(_ action: VoiceTalkButton.Action) {
        guard let connection else { talkButton.reset(); return }
        let previous = talkTask
        switch action {
        case .none: return
        case .begin: talkTask = Task { await previous?.value; await connection.beginTalking() }
        case .send: talkTask = Task { await previous?.value; await connection.finishTalking() }
        }
    }

    /// A pause took the microphone mid-turn: drop the turn rather than send
    /// half of it.
    private func dropTalking() {
        guard talkButton.talking else { return }
        talkButton.reset()
        guard let connection else { return }
        let previous = talkTask
        talkTask = Task { await previous?.value; await connection.cancelTalking() }
    }

    func toggleMute() {
        guard isActive else { return }
        muted.toggle()
        audio?.setMuted(muted)
        if case .listening = phase { objectWillChange.send() }
        liveActivity.update(muted: muted)
        flushAnnouncements()
    }

    /// Connected: end before the Mac's limit and OpenAI's hour, report use
    /// and the transcript to the Mac now and then, and show the Live Activity.
    private func beginSession(grant: VoiceSessionGrant, run: Int) {
        let connectedAt = Date()
        log.connected(at: connectedAt)
        let (deadline, reason) = VoiceSessionLimit.deadline(grant: grant, connectedAt: connectedAt)
        sessionWindow = connectedAt...max(connectedAt, deadline)
        limitTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0, deadline.timeIntervalSinceNow)))
            guard !Task.isCancelled, let self, self.generation == run else { return }
            self.reachedLimit(reason)
        }
        reportTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(Self.reportInterval))
                guard !Task.isCancelled, let self, self.generation == run else { return }
                self.report()
            }
        }
        liveActivity.start(computer: model?.macName ?? "", endsAt: deadline, muted: muted,
                           mute: { [weak self] in self?.toggleMute() }, end: { [weak self] in self?.end() })
        startAnnouncing(run: run)
    }

    // MARK: Announcements

    /// Voice speaks up about the chat it is pinned to, and only that chat.
    /// Requests already pending when voice starts are not news.
    private func startAnnouncing(run: Int) {
        announceSubscriptions = []
        #if WONDER_DIAGNOSTICS
        announced = []
        #endif
        announcer = VoiceAnnouncer(chatID: chatID, title: lastChat?.title ?? "", enabled: VoiceAnnouncementSetting.isOn(defaults),
                                   minimumGap: Self.announcementGap)
        guard chatID != nil, let model else { return }
        announceSubscriptions = [
            model.replyFinished.receive(on: DispatchQueue.main).sink { [weak self] finished in
                MainActor.assumeIsolated {
                    guard self?.generation == run else { return }
                    self?.noteAnnouncement(.finished, chat: finished.conversation, key: finished.key)
                }
            },
        ]
        // Requests already pending when voice starts are not news, but the list
        // may not be loaded yet: read it first, and listen only after that read
        // landed. A failed read leaves the first list that does arrive as the
        // baseline instead.
        announceBaseline = Task { [weak self, weak model] in
            guard let model else { return }
            await model.loadAttention()
            guard let self, self.generation == run, !Task.isCancelled else { return }
            self.knownAttention = Set(model.attention.map(\.id))
            self.attentionBaselineSet = model.attentionReadLanded
            self.announceSubscriptions.append(model.$attention.receive(on: DispatchQueue.main).sink { [weak self] requests in
                MainActor.assumeIsolated {
                    guard let self, self.generation == run else { return }
                    let pending = Set(requests.map(\.id))
                    if !self.attentionBaselineSet { self.knownAttention = pending; self.attentionBaselineSet = model.attentionReadLanded; return }
                    for gone in self.knownAttention.subtracting(pending) {
                        self.announcer.retract(key: gone)
                        self.approvalAnsweredElsewhere(gone)
                    }
                    for request in requests where !self.knownAttention.contains(request.id) {
                        guard let chat = request.conversationId else { continue }
                        self.noteAnnouncement(request.isQuestion ? .question : .approval, chat: chat, key: request.id)
                    }
                    self.knownAttention = pending
                }
            })
        }
    }

    private static var announcementGap: TimeInterval {
        #if WONDER_DIAGNOSTICS
        let arguments = ProcessInfo.processInfo.arguments
        if let index = arguments.firstIndex(of: "-diagnostics-voice-announce-gap"), arguments.indices.contains(index + 1),
           let seconds = TimeInterval(arguments[index + 1]) { return seconds }
        #endif
        return VoiceAnnouncer.defaultGap
    }

    /// The pinned chat's current title, which the owner or agent may have renamed.
    private func refreshAnnouncementTitle() {
        guard let id = announcer.chatID, let model, let detail = model.projects.details[id] else { return }
        let title = model.projectChat(detail).title
        if title != announcer.title { announcer.rename(title) }
    }

    func noteAnnouncement(_ kind: VoiceAnnouncementKind, chat: String, key: String) {
        announcer.enabled = VoiceAnnouncementSetting.isOn(defaults)
        if announcer.note(kind, chat: chat, key: key, at: Date()) { flushAnnouncements() }
    }

    private var announcementMoment: VoiceAnnouncer.Moment {
        var stageBusy = phase != .listening
        #if WONDER_DIAGNOSTICS
        // The scripted preview keeps one stage; only a waiting proposal busies it.
        if scriptedPreview { stageBusy = false }
        #endif
        return VoiceAnnouncer.Moment(ownerTalking: phase == .hearing || talkButton.talking, muted: muted,
                                     voiceBusy: stageBusy || pending != nil || resolvingPending)
    }

    #if WONDER_DIAGNOSTICS
    private var scriptedPreview: Bool {
        connection == nil && ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-preview")
    }
    #endif

    /// Speaks what is waiting if the conversation is quiet; otherwise a state
    /// change (unmute, reply done, confirmation answered) or the timer retries.
    private func flushAnnouncements() {
        announceTimer?.cancel(); announceTimer = nil
        guard isActive, announcer.hasWaiting else { return }
        announcer.enabled = VoiceAnnouncementSetting.isOn(defaults)
        refreshAnnouncementTitle()
        let now = Date()
        guard let announcement = announcer.take(at: now, moment: announcementMoment) else {
            if let next = announcer.nextCheck(after: now), next.timeIntervalSince(now) > 0.01 { scheduleAnnouncements(at: next) }
            return
        }
        #if WONDER_DIAGNOSTICS
        // The scripted preview has no model to speak: show the line it would say.
        if scriptedPreview {
            announced.append(announcement.sentence)
            apply(.assistantTranscript(announcement.sentence, final: true))
            return
        }
        #endif
        guard let connection else { return announcer.requeue(announcement) }
        let run = generation
        Task { [weak self] in
            let spoken = await connection.announce(announcement.sentence)
            guard let self, self.generation == run else { return }
            if spoken {
                #if WONDER_DIAGNOSTICS
                self.announced.append(announcement.sentence)
                #endif
            } else {
                self.announcer.requeue(announcement)
                self.scheduleAnnouncements(at: Date().addingTimeInterval(2))
            }
        }
    }

    private func scheduleAnnouncements(at date: Date) {
        announceTimer?.cancel()
        announceTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0.05, date.timeIntervalSinceNow)))
            guard !Task.isCancelled else { return }
            self?.flushAnnouncements()
        }
    }

    static let reportInterval: TimeInterval = 30

    private func reachedLimit(_ reason: VoiceSessionLimit.Reason) {
        let message = VoiceSessionLimit.message(reason, macName: (minter ?? model)?.macName ?? "your computer")
        // The Mac refuses this panel after its limit; Try again opens a new one.
        if VoiceSessionLimit.panelEnded(reason) { teardown(); closePanel() }
        end(failure: message)
    }

    /// Sends what the session used and said since the last report. A post
    /// that fails goes again with the next one.
    private func report() {
        guard let model = minter ?? model, let panel = panelID, panelOnMac else { return }
        let usage = log.takeUsage(at: Date())
        let transcript = log.hasUnsentTranscript ? takeTranscript() : nil
        Task { [weak self, weak model] in
            struct Reply: Decodable, Sendable {}
            for (kind, post) in [("usage", Optional(usage)), ("transcript", transcript)] {
                guard let post, let model else { continue }
                do {
                    let _: Reply = try await model.manage("/api/v1/voice/panels/\(panel)/\(kind)", method: "POST", body: post.body)
                } catch {
                    if self?.panelID == panel { self?.log.restore(post.sent) }
                }
            }
        }
    }

    /// The panel closed: a last report, then the Mac stops minting for it.
    private func closePanel() {
        guard let panel = panelID else { return }
        report()
        let onMac = panelOnMac
        let model = minter ?? model
        panelID = nil; panelOnMac = false; minter = nil
        guard let model, onMac else { return }
        Task { [weak model] in
            struct Reply: Decodable, Sendable {}
            let _: Reply? = try? await model?.manage("/api/v1/voice/panels/\(panel)", method: "DELETE")
        }
    }

    /// A request left the waiting list while its answer was on the card: it was
    /// answered somewhere else, so clear the card and say so.
    private func approvalAnsweredElsewhere(_ requestID: String) {
        guard let router, !resolvingPending else { return }
        let run = generation
        Task { [weak self] in
            guard await router.dropApproval(requestID: requestID) != nil else { return }
            guard let self, self.generation == run else { return }
            self.showNotice("That request is no longer waiting. Nothing more to do.")
            await self.connection?.reportOnScreenDecision(
                #"{"status":"already_answered","detail":"The request was answered elsewhere; nothing was sent."}"#)
        }
    }

    private func showNotice(_ notice: String) {
        targetNotice = notice
        UIAccessibility.post(notification: .announcement, argument: notice)
        noticeTask?.cancel()
        noticeTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            self?.targetNotice = nil
        }
    }

    /// What happened to a confirmed approval, in plain words.
    private static func outcome(of action: VoicePendingAction, output: String) -> String? {
        guard case .approval = action.kind else { return nil }
        let value = (try? JSONSerialization.jsonObject(with: Data(output.utf8))) as? [String: Any]
        switch value?["status"] as? String {
        case "resolved": return "Answered. The agent is continuing."
        case "failed":
            let detail = (value?["error"] as? String) ?? ""
            // The host refuses a request that is already gone; anything else may still be waiting.
            return detail.contains("no longer available")
                ? "That request was already answered or has ended."
                : "That did not go through. Check the request before trying again."
        default: return "That is no longer waiting."
        }
    }

    /// The owner tapped Confirm or Cancel on the proposed action.
    func resolvePending(_ action: VoicePendingAction, confirm: Bool) {
        guard let router, let connection, !resolvingPending else {
            #if WONDER_DIAGNOSTICS
            if router == nil { pending = nil }
            #endif
            return
        }
        resolvingPending = true
        let run = generation
        Task { [weak self] in
            let output = await router.resolvePending(id: action.id, confirm: confirm)
            guard let self, self.generation == run, !Task.isCancelled else { return }
            await connection.reportOnScreenDecision(output)
            guard self.generation == run else { return }
            self.resolvingPending = false
        }
    }

    /// The saved transcript belongs to the starting chat, even if a voice
    /// command acted elsewhere. Standalone voice keeps its acted-on threads.
    /// Keep this target through teardown and the final report, too.
    var transcriptThreadIDs: [String] { visitedChatIDs.isEmpty ? actedThreadIDs : visitedChatIDs }

    /// The owner asked to open another thread: "this thread" and the target
    /// chip move there, and the move is announced. Nothing else moves them.
    func retarget(to thread: VoiceThread) {
        guard isActive else { return }
        if let model, let detail = model.projects.details[thread.id] { lastChat = model.projectChat(detail) }
        movedTo = thread
        chatID = thread.id
        if !visitedChatIDs.contains(thread.id) { visitedChatIDs.append(thread.id) }
        showNotice("Now talking in \(thread.title)")
    }

    func takeTranscript() -> (body: Data, sent: VoiceSessionLogBatch) {
        log.takeTranscript(threadIDs: transcriptThreadIDs)
    }

    /// Ends the session. With `failure`, the panel stays up showing why and
    /// Try again reconnects in it; otherwise the panel closes on the Mac too.
    func end(failure: String? = nil) {
        let chat = chatID
        teardown()
        if let failure {
            // Keep the panel up with the reason and a way to retry or close.
            isActive = true; chatID = chat; phase = .failed(failure)
        } else {
            closePanel()
        }
        model?.voiceSessionEnded()
        #if WONDER_DIAGNOSTICS
        recordPreviewAudioRoute("ended")
        #endif
    }

    /// The only teardown: stops the microphone, playback, socket, tool router,
    /// timers and observers.
    private func teardown() {
        generation += 1
        limitTask?.cancel(); limitTask = nil
        reportTask?.cancel(); reportTask = nil
        announceTimer?.cancel(); announceTimer = nil; announceSubscriptions = []
        announceBaseline?.cancel(); announceBaseline = nil
        #if WONDER_DIAGNOSTICS
        announceScript?.cancel(); announceScript = nil
        #endif
        announcer = VoiceAnnouncer(chatID: nil, title: "")
        if sessionWindow != nil { log.disconnected(at: Date()); report() }
        sessionWindow = nil
        liveActivity.end()
        startTask?.cancel(); startTask = nil
        sendTask?.cancel(); sendTask = nil
        talkTask?.cancel(); talkTask = nil; talkButton.reset()
        if let connection { Task { await connection.close() } }
        connection = nil; router = nil
        // Keep the weak window address for Try again, like lastChat. It owns
        // no scene resources and is replaced when another window starts voice.
        audio?.stop(); audio = nil
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
        observers = []
        levels.reset()
        noticeTask?.cancel(); targetNotice = nil
        isActive = false; chatID = nil; phase = .idle; pending = nil; activity = nil
        resolvingPending = false; realtimeReady = false
        turn = VoiceTurn(); presentsTurn = false
        #if WONDER_DIAGNOSTICS
        diagnosticSession?.stop()
        diagnosticSession = nil
        #endif
    }

    private func fail(_ message: String, run: Int) {
        guard run == generation else { return }
        end(failure: message)
    }

    func apply(_ event: VoiceRealtimeEvent) {
        log.record(event, at: Date())
        turn.apply(event)
        switch event {
        case .ready: realtimeReady = true
        case .userSpeechStarted:
            // Whatever was still queued was never heard; cut it from the
            // model's side of the conversation too. A hold already dropped it.
            if turn.live { truncateUnheard(audio?.flushPlayback()) }
            userCaption = ""
        case .userSpeechStopped, .userSpeechDiscarded: activity = nil
        case .userTranscript(let text, _): userCaption = text
        case .assistantTranscript(let text, _): agentCaption = text
        case .toolResult:
            if let router {
                let run = generation
                Task { [weak self] in
                    let ids = await router.actedThreadIDs
                    guard self?.generation == run else { return }
                    self?.actedThreadIDs = ids
                }
            }
        case .assistantAudio, .toolCall, .usage, .responseCompleted, .responseDone: break
        case .toolStarted(let name): activity = Self.activity(for: name)
        case .toolFinished: if !turn.toolsRunning { activity = nil }
        case .notice(let message):
            if phase == .connecting { return fail(message, run: generation) }
            agentCaption = message
        case .closed(let reason):
            return end(failure: reason)
        }
        showTurn()
    }

    private func playbackChanged(_ playing: Bool) {
        turn.playbackChanged(playing)
        showTurn()
    }

    /// The only writer of a connected session's phase: a hold shows Paused
    /// whatever the conversation does underneath.
    private func showTurn() {
        guard presentsTurn, isActive else { return }
        switch turn.display {
        case .held(let hold): phase = .paused(hold.message)
        case .stage(.listening): phase = .listening
        case .stage(.hearing): phase = .hearing
        case .stage(.working): phase = .working
        case .stage(.speaking): phase = .speaking
        }
        flushAnnouncements()
    }

    private func truncateUnheard(_ unplayed: Int?) {
        guard let unplayed, unplayed > 0, let connection else { return }
        Task { await connection.truncateUnheard(unplayedMilliseconds: unplayed) }
    }

    /// Stops the microphone and drops what the voice was still saying, so
    /// nothing is heard or captured until the owner or iOS resumes.
    private func hold(_ reason: VoiceTurn.Hold) {
        dropTalking()
        truncateUnheard(audio?.pause())
        turn.hold(reason)
        showTurn()
        #if WONDER_DIAGNOSTICS
        recordPreviewAudioRoute("paused")
        #endif
    }

    /// Capture runs again; only then does the panel leave Paused.
    /// Only the owner tapping the orb (`reclaim`) takes the audio session
    /// back from dictation; an interruption or route change does not.
    private func resumeCapture(reclaim: Bool = false) -> Bool {
        guard let audio, (try? audio.resume(reclaim: reclaim)) != nil else { return false }
        turn.resumed()
        showTurn()
        #if WONDER_DIAGNOSTICS
        recordPreviewAudioRoute("resumed")
        #endif
        return true
    }

    private static func activity(for tool: String) -> String {
        switch tool {
        case "list_projects": "Checking your Projects…"
        case "list_threads": "Checking chats…"
        case "read_latest_reply": "Reading the latest reply…"
        case "running_work": "Checking running work…"
        default: "Working on it…"
        }
    }

    private func observeAudioSession(run: Int) {
        let center = NotificationCenter.default
        let session = AVAudioSession.sharedInstance()
        observers.append(center.addObserver(forName: AVAudioSession.interruptionNotification, object: session, queue: .main) { [weak self] note in
            let type = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt).flatMap(AVAudioSession.InterruptionType.init)
            let options = (note.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt).map(AVAudioSession.InterruptionOptions.init) ?? []
            MainActor.assumeIsolated {
                guard let self, self.generation == run else { return }
                #if WONDER_DIAGNOSTICS
                self.recordPreviewAudioRoute(type == .began ? "interruption-began" : "interruption-ended")
                #endif
                if type == .began {
                    self.hold(.interrupted)
                } else if type == .ended, self.turn.hold == .interrupted {
                    if !options.contains(.shouldResume) || !self.resumeCapture() { self.hold(.paused) }
                }
            }
        })
        observers.append(center.addObserver(forName: AVAudioSession.routeChangeNotification, object: session, queue: .main) { [weak self] note in
            let reason = (note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt).flatMap(AVAudioSession.RouteChangeReason.init)
            MainActor.assumeIsolated {
                guard let self, self.generation == run else { return }
                #if WONDER_DIAGNOSTICS
                self.recordPreviewAudioRoute("route-changed")
                #endif
                // A headset, car or AirPods joining or leaving changes the
                // input format; rebuild the tap so the microphone keeps going.
                // While paused it stays off: resuming rebuilds the tap anyway.
                guard self.turn.live, reason == .newDeviceAvailable || reason == .oldDeviceUnavailable else { return }
                if !self.resumeCapture() { self.hold(.paused) }
            }
        })
        observers.append(center.addObserver(forName: AVAudioSession.mediaServicesWereResetNotification, object: session, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.generation == run else { return }
                self.end(failure: "Audio restarted on this device. Start voice again.")
            }
        })
        // Background audio keeps the session going with the screen locked or
        // another app in front; Mute and End are on the Live Activity.
    }

    /// Reconnects in the same panel, or a new one after a limit ended it.
    func retry() {
        let chat = lastChat
        teardown()
        isActive = false
        start(chat: chat)
    }

    /// Paused by an interruption: try the microphone again.
    func resumeAfterPause() {
        guard isActive, presentsTurn, turn.hold != nil else { return }
        _ = resumeCapture(reclaim: true)
    }

    /// Microphone access is off: the panel stays up with Open Settings and
    /// End, and voice starts again once access is on when Cupola returns.
    private func deniedMicrophone() {
        let chat = chatID
        teardown()
        isActive = true; chatID = chat; phase = .needsMicrophone
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.recheckMicrophone() }
        })
        model?.voiceSessionEnded()
    }

    /// Back from Settings: start voice if access is now on; otherwise keep
    /// offering Settings.
    func recheckMicrophone() {
        guard phase == .needsMicrophone, microphoneAllowed() else { return }
        retry()
    }

    func openMicrophoneSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        UIApplication.shared.open(url)
    }

    static func providers(_ model: ConnectionModel) -> VoiceProviders {
        var names: [AgentFamily: [VoiceModelMatch.Candidate]] = [:]
        for option in model.projects.options?.models ?? [] where !option.hidden {
            names[option.family, default: []].append(.init(id: option.id, name: option.displayName))
        }
        return VoiceProviders(families: model.projects.families, models: names)
    }

    static func context(for chat: ChatSummary?, model: ConnectionModel) -> VoiceContext {
        guard let chat else { return VoiceContext(providers: Self.providers(model)) }
        let projectName = model.projects.details[chat.id].flatMap { detail in
            model.projects.projects.first { $0.id == detail.projectId }?.name
        }
        return VoiceContext(threadID: chat.id, title: chat.title, projectName: projectName,
                            providers: Self.providers(model))
    }

    /// The Mac's recent threads and project names added to `context`. Voice
    /// starts without them when the Mac is slow or cannot answer.
    static func seeded(_ context: VoiceContext, host: ConnectionVoiceHost, within limit: Duration = .seconds(4)) async -> VoiceContext {
        await withTaskGroup(of: VoiceContext?.self) { group in
            group.addTask { await recentWork(context, host: host) }
            group.addTask { try? await Task.sleep(for: limit); return nil }
            let first = await group.next() ?? nil
            group.cancelAll()
            return first ?? context
        }
    }

    private static func recentWork(_ context: VoiceContext, host: ConnectionVoiceHost) async -> VoiceContext {
        async let threads = try? host.threads(projectID: nil)
        async let projects = try? host.projects()
        let (recent, named) = await (threads, projects)
        return VoiceContext(threadID: context.threadID, title: context.title, projectName: context.projectName,
                            recentThreads: recent ?? [], projectNames: named?.map(\.name) ?? [], providers: context.providers)
    }

    #if WONDER_DIAGNOSTICS
    /// `-diagnostics-voice-tools`: fixture transcripts drive the real tool router
    /// (confirmations, retargeting) against `FixtureVoiceHost`, one scripted
    /// utterance per `wonderctl signal voice-tools-N`. No microphone, network or model.
    private func startToolsFixture(run: Int) {
        guard let model, let host = model.connection?.credential.hostInstallationId else { return }
        phase = .listening
        sessionWindow = Date()...Date().addingTimeInterval(15 * 60)
        let fixtureHost = FixtureVoiceHost(model: model, navigation: VoiceNavigation(scene: scene, host: host))
        toolsFixtureHost = fixtureHost
        let router = VoiceToolRouter(host: fixtureHost, context: Self.context(for: lastChat, model: model))
        self.router = router
        sendTask = Task { [weak self] in
            await router.observePending { [weak self] action in
                DispatchQueue.main.async { MainActor.assumeIsolated { if self?.generation == run { self?.pending = action } } }
            }
            await router.observeTarget { [weak self] thread in
                DispatchQueue.main.async { MainActor.assumeIsolated { if self?.generation == run { self?.retarget(to: thread) } } }
            }
            await router.observeConfirmed { [weak self] action, output in
                DispatchQueue.main.async { MainActor.assumeIsolated { if self?.generation == run, let text = Self.outcome(of: action, output: output) { self?.showNotice(text) } } }
            }
            for step in VoiceToolsFixtureStep.script {
                while !DiagnosticSubagentFixture.signalled("voice-tools-\(step.number)") {
                    do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
                }
                guard let self, self.generation == run, self.isActive, !Task.isCancelled else { return }
                self.apply(.userTranscript(step.said, final: true))
                await router.beginUserTurn(itemID: "fixture-\(step.number)", transcript: step.said)
                var reply = step.reply
                let output: String
                if step.tool == "confirm_action" {
                    let id = await router.pending?.id ?? ""
                    output = await router.handle(name: step.tool, arguments: "{\"confirmation_id\":\"\(id)\"}")
                } else {
                    output = await router.handle(name: step.tool, arguments: step.arguments)
                }
                if await router.pending != nil { await router.confirmationPromptFinished() }
                let status = (try? JSONSerialization.jsonObject(with: Data(output.utf8)) as? [String: Any])?["status"] as? String ?? "error"
                self.toolsFixtureResults.append(step.tool + ":" + status)
                if status == "error" { reply = "That did not work." }
                self.apply(.assistantTranscript(reply, final: true))
                self.apply(.toolResult(name: step.tool, output: output))
            }
        }
    }

    /// A scripted session for screenshots and scenarios, without a network.
    /// Only the explicit voice-audio fixture opts into the real microphone.
    private func startPreview(run: Int) {
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-tools") { startToolsFixture(run: run); return }
        phase = .speaking
        sessionWindow = Date()...Date().addingTimeInterval(15 * 60)
        apply(.userTranscript("Ask the Wonder chat to run the voice tests", final: true))
        apply(.assistantTranscript("I’ll send that to “Voice mode”. Shall I go ahead?", final: true))
        pending = VoicePendingAction(id: "c1", kind: .send(threadID: "preview", text: "Run the voice tests"),
                                     summary: "Send to “Voice mode”", createdAt: Date())
        startAnnouncing(run: run)
        announceScript = Task { [weak self] in
            // `wonderctl signal voice-announce-<event>` stands in for the chat's
            // event stream: the pinned chat's three events, and another chat's.
            let events: [(String, VoiceAnnouncementKind, String)] = [
                ("voice-announce-reply", .finished, "fixture-parent-conversation"),
                ("voice-announce-approval", .approval, "fixture-parent-conversation"),
                ("voice-announce-question", .question, "fixture-parent-conversation"),
                ("voice-announce-other", .finished, "fixture-fork-conversation")]
            var delivered = Set<String>()
            while !Task.isCancelled {
                for (name, kind, chat) in events where !delivered.contains(name) && DiagnosticSubagentFixture.signalled(name) {
                    delivered.insert(name)
                    guard let self, self.generation == run else { return }
                    self.noteAnnouncement(kind, chat: chat, key: name)
                }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
        sendTask = Task { [weak self] in
            while !DiagnosticSubagentFixture.signalled("voice-transcript") {
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
            }
            guard let self, self.generation == run, self.isActive, !Task.isCancelled else { return }
            self.apply(.userTranscript("Keep this transcript in the starting chat.", final: true))
            self.apply(.assistantTranscript("This transcript stays where voice started.", final: true))
        }
        guard ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-audio") else {
            levels.startPreview()
            return
        }
        // Opt-in hardware QA: real capture/route/interruption handling, no
        // socket, mint, recorded PCM or model request. Samples are discarded.
        phase = .connecting
        startTask = Task { [weak self] in
            guard let self else { return }
            let permitted = await self.requestPermission()
            guard self.generation == run, self.isActive, !Task.isCancelled else { return }
            guard permitted else { self.deniedMicrophone(); return }
            let audio = self.makeAudio(run: run)
            let levels = self.levels
            audio.onInput = { _, level in levels.post(input: level) }
            self.audio = audio
            do { try audio.start() }
            catch { self.fail("The voice fixture couldn’t use the microphone.", run: run); return }
            self.presentsTurn = true
            self.showTurn()
            self.observeAudioSession(run: run)
            for (name, reason) in [(UIApplication.didEnterBackgroundNotification, "background"),
                                   (UIApplication.didBecomeActiveNotification, "foreground")] {
                self.observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated {
                        guard self?.generation == run else { return }
                        self?.recordPreviewAudioRoute(reason)
                    }
                })
            }
            self.recordPreviewAudioRoute("started")
        }
    }

    private var diagnosticAudioRoute: [String: Any] {
        let session = AVAudioSession.sharedInstance()
        let route = session.currentRoute
        return ["inputs": route.inputs.map { $0.portType.rawValue },
                "outputs": route.outputs.map { $0.portType.rawValue },
                "sampleRate": session.sampleRate, "engineRunning": audio?.engineForTesting.isRunning ?? false]
    }

    func recordPreviewAudioRoute(_ reason: String) {
        let args = ProcessInfo.processInfo.arguments
        guard args.contains("-diagnostics-voice-audio") else { return }
        // Port types only, never personal Bluetooth/device names or audio.
        let record: [String: Any] = ["event": reason, "at": Date().timeIntervalSince1970,
                                     "run": generation, "active": isActive, "status": statusText,
                                     "audio": diagnosticAudioRoute]
        if let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]),
           let text = String(data: data, encoding: .utf8) { print("VOICE_FIXTURE \(text)") }
    }
    #endif
}

/// The orb's level, kept apart so only the orb redraws as it changes.
@MainActor final class VoiceLevels: ObservableObject {
    @Published private(set) var level: CGFloat = 0
    private nonisolated(unsafe) var lastPost: TimeInterval = 0
    private var previewTimer: Timer?

    /// Called from audio threads; publishes at most about 20 times a second.
    nonisolated func post(input: Float) { post(Float(input)) }
    nonisolated func post(output: Float) { post(Float(output)) }
    private nonisolated func post(_ value: Float) {
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastPost > 0.05 else { return }
        lastPost = now
        let scaled = CGFloat(min(1, max(0, (20 * log10(max(value, 1e-5)) + 50) / 40)))
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.level = scaled }
        }
    }

    func reset() { previewTimer?.invalidate(); previewTimer = nil; level = 0 }

    #if WONDER_DIAGNOSTICS
    func startPreview() {
        level = 0.55
        previewTimer = Timer.scheduledTimer(withTimeInterval: 0.12, repeats: true) { [weak self] _ in
            let phase = Date().timeIntervalSinceReferenceDate
            MainActor.assumeIsolated { self?.level = 0.45 + 0.3 * CGFloat(abs(sin(phase * 2.3))) }
        }
    }
    #endif
}

/// The voice session's Live Activity, started and updated on this phone.
/// It shows only the computer's name, mute state and time left.
@MainActor final class VoiceLiveActivity {
    private var activityID: String?
    private var endsAt = Date()

    func start(computer: String, endsAt: Date, muted: Bool, mute: @escaping () -> Void, end: @escaping () -> Void) {
        self.end()
        VoiceActivityControls.mute = mute
        VoiceActivityControls.end = end
        self.endsAt = endsAt
        guard ActivityAuthorizationInfo().areActivitiesEnabled else { return }
        activityID = try? Activity.request(attributes: VoiceActivityAttributes(computer: computer),
                                           content: ActivityContent(state: .init(muted: muted, endsAt: endsAt), staleDate: endsAt),
                                           pushType: nil).id
    }

    func update(muted: Bool) {
        guard let id = activityID else { return }
        let state = VoiceActivityAttributes.ContentState(muted: muted, endsAt: endsAt), stale = endsAt
        Task.detached { await Self.find(id)?.update(ActivityContent(state: state, staleDate: stale)) }
    }

    func end() {
        VoiceActivityControls.mute = nil
        VoiceActivityControls.end = nil
        guard let id = activityID else { return }
        activityID = nil
        Task.detached { await Self.find(id)?.end(nil, dismissalPolicy: .immediate) }
    }

    /// A session force-quit or cut off by iOS leaves its activity behind;
    /// nothing can act on it, so it goes as soon as Cupola runs again.
    static func endOrphans() {
        Task.detached {
            for activity in Activity<VoiceActivityAttributes>.activities { await activity.end(nil, dismissalPolicy: .immediate) }
        }
    }

    // Activities are not Sendable; reach them by ID.
    nonisolated private static func find(_ id: String) -> Activity<VoiceActivityAttributes>? {
        Activity<VoiceActivityAttributes>.activities.first { $0.id == id }
    }
}

// MARK: - Read aloud

/// Reads one reply aloud with the system voice on this phone: no model and no
/// OpenAI key. One reply at a time. It speaks through `AudioSessionOwner`;
/// voice mode and dictation also tell it while they use the microphone, which
/// stops reading and makes Read aloud unavailable until they stop.
@MainActor final class SpeechReader: NSObject, ObservableObject, AVSpeechSynthesizerDelegate {
    static let shared = SpeechReader()

    /// The message being prepared or read; nil when silent.
    @Published private(set) var readingID: String?
    /// Why Read aloud is unavailable now; nil when it is available.
    @Published private(set) var unavailableReason: String?

    private let synthesizer = AVSpeechSynthesizer()
    private var microphone = MicrophoneClaims()
    private var interruptionObserver: NSObjectProtocol?
    private var prepareTask: Task<Void, Never>?
    /// The current reply's utterances still to finish.
    private var utterances: [AVSpeechUtterance] = []
    /// Stopped ones awaiting their cancel callback, held so a new utterance
    /// never shares an identity with one (bounded: queued ones may not call back).
    private var stopped: [AVSpeechUtterance] = []
    private var generation = 0
    /// The reader's claim on the shared audio session while it speaks.
    private var lease: AudioSessionOwner.Lease?

    override private init() {
        super.init()
        synthesizer.delegate = self
        // A call or another app's audio ends reading rather than leaving it paused.
        interruptionObserver = NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification, object: AVAudioSession.sharedInstance(), queue: .main
        ) { [weak self] note in
            let type = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt).flatMap(AVAudioSession.InterruptionType.init)
            guard type == .began else { return }
            MainActor.assumeIsolated { if self?.readingID != nil { self?.stop() } }
        }
    }

    func read(_ text: String, id: String) {
        guard unavailableReason == nil else { return }
        stop()
        generation += 1
        let run = generation
        readingID = id
        prepareTask = Task { [weak self] in
            let chunks = await Task.detached(priority: .userInitiated) { SpokenReply.chunks(from: text) }.value
            guard let self, !Task.isCancelled, run == self.generation, self.unavailableReason == nil else { return }
            self.speak(chunks)
        }
    }

    nonisolated static func key(chat: String, message: String) -> String { chat + "\n" + message }

    /// Reading belongs to the open conversation: leaving it stops.
    func conversationClosed(_ chatID: String) {
        if readingID?.hasPrefix(chatID + "\n") == true { stop() }
    }

    func stop() {
        generation += 1
        prepareTask?.cancel(); prepareTask = nil
        stopped = Array((stopped + utterances).suffix(32))
        utterances = []
        if synthesizer.isSpeaking || synthesizer.isPaused { synthesizer.stopSpeaking(at: .immediate) }
        readingID = nil
        releaseSession()
    }

    /// Voice or dictation takes over the audio session: reading stops.
    func claimMicrophone(_ owner: AnyObject, for use: MicrophoneClaims.Use) {
        microphone.claim(ObjectIdentifier(owner), for: use)
        unavailableReason = microphone.readAloudUnavailableReason
        stop()
    }

    func releaseMicrophone(_ owner: AnyObject) {
        microphone.release(ObjectIdentifier(owner))
        unavailableReason = microphone.readAloudUnavailableReason
    }

    private func speak(_ chunks: [String]) {
        #if WONDER_DIAGNOSTICS
        // Scenarios check the menu while reading; the simulator's voice would
        // finish a short reply before the menu opens again.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-read-aloud-hold") { return }
        #endif
        guard !chunks.isEmpty else { readingID = nil; return }
        if lease == nil {
            // Chat media playing hands the session over; dictation or voice
            // taking it later stops reading. If configuring fails the
            // synthesizer still plays through the session as it is.
            lease = try? AudioSessionOwner.shared.acquire(.speech) { [weak self] taken in
                Task { @MainActor in self?.sessionTaken(taken) }
            }
        }
        for chunk in chunks {
            let utterance = AVSpeechUtterance(string: chunk)
            // VoiceOver users hear their own voice and rate.
            utterance.prefersAssistiveTechnologySettings = true
            utterances.append(utterance)
            synthesizer.speak(utterance)
        }
    }

    private func finished(_ utterance: ObjectIdentifier) {
        if let index = stopped.firstIndex(where: { ObjectIdentifier($0) == utterance }) {
            stopped.remove(at: index); return
        }
        guard let index = utterances.firstIndex(where: { ObjectIdentifier($0) == utterance }) else { return }
        utterances.remove(at: index)
        guard utterances.isEmpty else { return }
        readingID = nil
        releaseSession()
    }

    /// A stale lease (another owner took the session) releases nothing.
    private func releaseSession() {
        AudioSessionOwner.shared.release(lease)
        lease = nil
    }

    private func sessionTaken(_ taken: AudioSessionOwner.Lease) {
        guard lease == taken else { return }
        lease = nil
        if readingID != nil { stop() }
    }

    nonisolated func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didFinish utterance: AVSpeechUtterance) {
        let id = ObjectIdentifier(utterance)
        Task { @MainActor [weak self] in self?.finished(id) }
    }

    nonisolated func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didCancel utterance: AVSpeechUtterance) {
        let id = ObjectIdentifier(utterance)
        Task { @MainActor [weak self] in self?.finished(id) }
    }
}

// MARK: - Audio

extension AudioSessionOwner {
    /// The app's one owner of the shared audio session: dictation, voice and
    /// conversation media claim it here instead of setting categories directly.
    static let shared = AudioSessionOwner(device: SystemAudioSession())
}

/// AVAudioSession configured for each use.
final class SystemAudioSession: AudioSessionDevice, @unchecked Sendable {
    func activate(for use: AudioSessionUse) throws {
        let session = AVAudioSession.sharedInstance()
        switch use {
        case .dictation:
            try session.setCategory(.record, mode: .measurement)
        case .voice:
            // voiceChat gives echo cancellation and routes to Bluetooth headsets
            // and car systems (hands-free profile) as well as the speaker.
            try session.setCategory(.playAndRecord, mode: .voiceChat,
                                    options: [.allowBluetoothHFP, .defaultToSpeaker])
            try session.setPreferredIOBufferDuration(0.02)
        case .media:
            // Chat audio and video play out loud, like any player the owner starts.
            try session.setCategory(.playback, mode: .default)
        case .speech:
            // Spoken audio pauses other spoken audio (podcasts) and ducks music.
            try session.setCategory(.playback, mode: .spokenAudio, options: [.duckOthers])
        }
        try session.setActive(true)
    }

    func deactivate() {
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}

/// Microphone capture (24 kHz PCM16 with echo cancellation) and playback of
/// the voice's PCM16, on one engine. Safe to call from any thread.
final class VoiceAudio: @unchecked Sendable {
    var onInput: (@Sendable (Data, Float) -> Void)?
    var onOutputLevel: (@Sendable (Float) -> Void)?
    var onPlaybackChanged: (@Sendable (Bool) -> Void)?
    /// The engine stopped and could not be restarted: the microphone is off.
    var onCaptureLost: (@Sendable () -> Void)?
    /// Dictation took the audio session; capture should pause.
    var onPreempted: (@Sendable () -> Void)?
    /// Brings capture back after the engine stopped itself; tests replace it.
    lazy var restart: () throws -> Void = { [unowned self] in try self.resume() }

    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let playFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 24_000, channels: 1, interleaved: false)!
    private let sendFormat = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 24_000, channels: 1, interleaved: true)!
    private let lock = NSLock()
    private var converter: AVAudioConverter?
    private var muted = false
    private var scheduled = 0
    /// Frames queued but not yet played back.
    private var unplayedFrames = 0
    /// Bumped by a flush so stopped buffers' late callbacks are ignored.
    private var epoch = 0
    private var attached = false
    /// Capture should be running (started or resumed, not paused or stopped).
    private var capturing = false
    /// Paused: the voice's audio is dropped rather than queued until resume.
    private var held = false
    private var configurationObserver: NSObjectProtocol?
    /// Recent restarts, so a configuration change that keeps recurring ends
    /// in a pause rather than a loop.
    private var restarts: [Date] = []
    /// This engine's claim on the shared audio session.
    private var lease: AudioSessionOwner.Lease?

    deinit { configurationObserver.map(NotificationCenter.default.removeObserver) }

    func start() throws {
        try claimSession()
        if !attached {
            engine.attach(player)
            attached = true
            // Echo cancellation keeps the voice from hearing itself; the
            // simulator may not offer it.
            try? engine.inputNode.setVoiceProcessingEnabled(true)
            engine.connect(player, to: engine.mainMixerNode, format: playFormat)
            player.installTap(onBus: 0, bufferSize: 1024, format: playFormat) { [weak self] buffer, _ in
                self?.onOutputLevel?(Self.rms(buffer))
            }
        }
        observeConfigurationChanges()
        try installInputTap()
        engine.prepare()
        try engine.start()
        player.play()
        lock.withLock { capturing = true }
    }

    /// AVAudioEngine stops itself when its I/O configuration changes (turning
    /// on voice processing or a new audio route can do that right after
    /// `start`) and posts this notification; it never restarts on its own, so
    /// without a restart the microphone stays silent while voice says Listening.
    func observeConfigurationChanges() {
        guard configurationObserver == nil else { return }
        configurationObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main) { [weak self] _ in
            self?.configurationChanged()
        }
    }

    func configurationChanged(at date: Date = Date()) {
        // nil: nothing to restart (paused, stopped or not started).
        let restart = lock.withLock { () -> Bool? in
            guard capturing else { return nil }
            restarts = restarts.filter { date.timeIntervalSince($0) < 5 } + [date]
            if restarts.count > 3 { capturing = false; return false }
            return true
        }
        guard let restart else { return }
        guard restart else { onCaptureLost?(); return }
        do { try self.restart() } catch {
            lock.withLock { capturing = false }
            onCaptureLost?()
        }
    }

    #if WONDER_DIAGNOSTICS
    /// The engine whose configuration-change notification restarts capture.
    var engineForTesting: AVAudioEngine { engine }
    func markCapturingForTesting() { lock.withLock { capturing = true } }
    #endif

    private func installInputTap() throws {
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else { throw URLError(.cannotOpenFile) }
        let converter = AVAudioConverter(from: format, to: sendFormat)
        lock.withLock { self.converter = converter }
        input.removeTap(onBus: 0)
        input.installTap(onBus: 0, bufferSize: 2048, format: format) { [weak self] buffer, _ in
            self?.capture(buffer)
        }
    }

    private func capture(_ buffer: AVAudioPCMBuffer) {
        let (converter, muted) = lock.withLock { (self.converter, self.muted) }
        guard let converter else { return }
        if muted { onInput?(Data(), 0); return }
        let level = Self.rms(buffer)
        let ratio = sendFormat.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 64
        guard let output = AVAudioPCMBuffer(pcmFormat: sendFormat, frameCapacity: capacity) else { return }
        var supplied = false
        var error: NSError?
        converter.convert(to: output, error: &error) { _, status in
            if supplied { status.pointee = .noDataNow; return nil }
            supplied = true; status.pointee = .haveData; return buffer
        }
        guard error == nil, output.frameLength > 0, let samples = output.int16ChannelData else { return }
        onInput?(Data(bytes: samples[0], count: Int(output.frameLength) * 2), level)
    }

    /// Queues 24 kHz mono PCM16 from the voice.
    func play(_ pcm: Data) {
        let frames = pcm.count / 2
        guard frames > 0, let buffer = AVAudioPCMBuffer(pcmFormat: playFormat, frameCapacity: AVAudioFrameCount(frames)),
              let channel = buffer.floatChannelData?[0] else { return }
        buffer.frameLength = AVAudioFrameCount(frames)
        pcm.withUnsafeBytes { raw in
            let samples = raw.bindMemory(to: Int16.self)
            for index in 0..<frames { channel[index] = Float(Int16(littleEndian: samples[index])) / 32_768 }
        }
        let queued = lock.withLock { () -> (started: Bool, epoch: Int)? in
            guard !held else { return nil }
            scheduled += 1; unplayedFrames += frames
            return (scheduled == 1, epoch)
        }
        guard let queued else { return }
        let current = queued.epoch
        if queued.started { onPlaybackChanged?(true) }
        player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
            guard let self else { return }
            let finished = self.lock.withLock { () -> Bool in
                guard self.epoch == current else { return false }
                self.scheduled = max(0, self.scheduled - 1)
                self.unplayedFrames = max(0, self.unplayedFrames - frames)
                return self.scheduled == 0
            }
            if finished { self.onPlaybackChanged?(false) }
        }
    }

    /// The owner started talking: drop what the voice was still saying.
    /// Returns how many milliseconds of it were never played.
    @discardableResult func flushPlayback() -> Int {
        let unplayed = lock.withLock { () -> Int in
            epoch += 1; scheduled = 0
            defer { unplayedFrames = 0 }
            return unplayedFrames
        }
        player.stop()
        onPlaybackChanged?(false)
        player.play()
        return unplayed / 24
    }

    func setMuted(_ value: Bool) { lock.withLock { muted = value } }

    /// Stops capture and playback and drops queued audio until `resume`.
    /// Returns how many milliseconds of the voice were never played.
    @discardableResult func pause() -> Int {
        let unplayed = lock.withLock { () -> Int in
            capturing = false; held = true; epoch += 1; scheduled = 0
            defer { unplayedFrames = 0 }
            return unplayedFrames
        }
        if attached { player.stop() }
        onPlaybackChanged?(false)
        engine.pause()
        return unplayed / 24
    }

    /// After an interruption or a route change: the input format may differ.
    /// An automatic resume never takes the audio session back from dictation
    /// that started since; the owner tapping the orb (`reclaim`) does.
    func resume(reclaim: Bool = false) throws {
        engine.stop()
        let claim = lock.withLock { lease }
        if let claim, try AudioSessionOwner.shared.reactivate(claim) {
        } else if reclaim {
            try claimSession()
        } else {
            throw CancellationError()
        }
        try installInputTap()
        engine.prepare()
        try engine.start()
        player.play()
        lock.withLock { capturing = true; held = false }
    }

    func stop() {
        lock.withLock { capturing = false }
        engine.inputNode.removeTap(onBus: 0)
        player.stop()
        engine.stop()
        // Releases only this engine's claim; a newer owner keeps the session.
        let claim = lock.withLock { () -> AudioSessionOwner.Lease? in defer { lease = nil }; return lease }
        AudioSessionOwner.shared.release(claim)
    }

    private func claimSession() throws {
        let next = try AudioSessionOwner.shared.acquire(.voice) { [weak self] _ in self?.onPreempted?() }
        lock.withLock { lease = next }
    }

    private static func rms(_ buffer: AVAudioPCMBuffer) -> Float {
        guard let channel = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
        var sum: Float = 0
        for index in 0..<Int(buffer.frameLength) { sum += channel[index] * channel[index] }
        return (sum / Float(buffer.frameLength)).squareRoot()
    }
}

#if WONDER_DIAGNOSTICS
/// Simulator-only audio injection. It feeds the production Realtime connection,
/// never the microphone or a text substitute. The manifest is consumed once;
/// short-lived credentials are removed before any network or model work.
@MainActor final class VoiceDiagnosticSession {
    struct Step: Decodable {
        let text: String
        let audio: String
        let expectedTool: String
        let expectedWordsAny: [String]?
        let expectedStatus: String?
        let expectedConversation: String?
        let expectedProjectName: String?
    }
    struct Manifest: Decodable {
        let runID: String
        let fixtureProject: String
        let grant: VoiceSessionGrant
        let steps: [Step]
    }
    let grant: VoiceSessionGrant
    private let manifest: Manifest
    private let directory: URL
    private var events: [[String: Any]] = []
    private var completed: [[String: Any]] = []
    private var tools: [String] = []
    private var outstanding = 0
    private var finalTranscripts = 0
    private var userTranscripts = 0
    private var responses = 0
    private var lastToolResponse = 0
    private var lastFinalResponse = 0
    private var lastFinalText = ""
    private var finishedTools: [String] = []
    private var usage: [[String: Any]] = []
    private var responseCompletions: [[String: Any]] = []
    private var toolResults: [[String: Any]] = []
    private var output = Data()
    private var terminal: String?
    private var finished = false
    private let files = DispatchQueue(label: "wonder.voice-fixture.files", qos: .utility)

    private init(manifest: Manifest, directory: URL) {
        self.manifest = manifest; self.grant = manifest.grant; self.directory = directory
    }

    static func load() -> VoiceDiagnosticSession? {
        #if targetEnvironment(simulator)
        guard ProcessInfo.processInfo.arguments.contains("-diagnostics-voice-session") else { return nil }
        let directory = URL.documentsDirectory.appendingPathComponent("voice-session")
        let file = directory.appendingPathComponent("session.json")
        guard let data = try? Data(contentsOf: file) else { return nil }
        try? FileManager.default.removeItem(at: file)
        guard let manifest = try? JSONDecoder().decode(Manifest.self, from: data),
              manifest.fixtureProject == "Overnight fixtures 2026-10-18",
              manifest.grant.socketURL != nil, (1...16).contains(manifest.steps.count),
              manifest.steps.allSatisfy({ $0.audio == URL(fileURLWithPath: $0.audio).lastPathComponent }) else { return nil }
        return VoiceDiagnosticSession(manifest: manifest, directory: directory)
        #else
        return nil
        #endif
    }

    func record(_ event: VoiceRealtimeEvent) {
        guard !finished else { return }
        var item: [String: Any] = ["at": Date().timeIntervalSince1970]
        switch event {
        case .assistantAudio(let pcm):
            if output.count + pcm.count <= 16 << 20 { output.append(pcm) }
            else { terminal = "Voice test exceeded its audio limit" }
            return
        case .ready: item["type"] = "ready"
        case .userSpeechStarted: item["type"] = "speech_started"
        case .userSpeechStopped: item["type"] = "speech_stopped"
        case .userSpeechDiscarded: item["type"] = "speech_discarded"
        case .userTranscript(let text, let final):
            guard final else { return }
            userTranscripts += 1
            item.merge(["type": "user_transcript", "text": text, "final": final]) { _, new in new }
        case .assistantTranscript(let text, let final):
            guard final else { return }
            if final { finalTranscripts += 1; lastFinalResponse = responses + 1; lastFinalText = text }
            item.merge(["type": "assistant_transcript", "text": text, "final": final]) { _, new in new }
        case .toolStarted(let name):
            tools.append(name); outstanding += 1; lastToolResponse = responses + 1
            item.merge(["type": "tool_started", "name": name]) { _, new in new }
        case .toolFinished(let name):
            outstanding = max(0, outstanding - 1); finishedTools.append(name)
            item.merge(["type": "tool_finished", "name": name]) { _, new in new }
        case .usage(let source, let id, let json):
            let value = (try? JSONSerialization.jsonObject(with: Data(json.utf8))) ?? NSNull()
            let record: [String: Any] = ["source": source, "id": id, "usage": value]
            usage.append(record)
            item.merge(["type": "usage", "source": source, "id": id, "usage": value]) { _, new in new }
        case .toolCall(let name, let arguments):
            let value = (try? JSONSerialization.jsonObject(with: Data(arguments.utf8))) ?? NSNull()
            item.merge(["type": "tool_call", "name": name, "arguments": value]) { _, new in new }
        case .toolResult(let name, let output):
            let value = (try? JSONSerialization.jsonObject(with: Data(output.utf8))) ?? NSNull()
            let result: [String: Any] = ["name": name, "result": value]
            toolResults.append(result)
            item.merge(["type": "tool_result", "name": name, "result": value]) { _, new in new }
        case .responseCompleted(let id, let status, let details, let usage):
            func decode(_ json: String?) -> Any {
                json.flatMap { try? JSONSerialization.jsonObject(with: Data($0.utf8)) } ?? NSNull()
            }
            let completion: [String: Any] = ["id": id ?? NSNull(), "status": status ?? NSNull(),
                                              "status_details": decode(details), "usage": decode(usage)]
            responseCompletions.append(completion)
            item.merge(completion) { _, new in new }
            item["type"] = "response_completed"
        case .responseDone: responses += 1; item["type"] = "response_done"
        case .notice(let message): terminal = message; item.merge(["type": "error", "message": message]) { _, new in new }
        case .closed(let message): terminal = message ?? "Connection closed"; item["type"] = "closed"
        }
        events.append(item)
    }

    /// Read the scene owner used by the real view, independently of tool output.
    static func sceneCheck(step: Step, scene: ShellState?, model: ConnectionModel, voiceConversation: String?) -> [String: Any] {
        let selected = scene?.selectedConversation
        let requiresScene = ["open_thread", "create_project"].contains(step.expectedTool) || step.expectedStatus == "created"
        let configured = !requiresScene || step.expectedConversation != nil
        let conversationMatches = step.expectedConversation.map { expected in
            selected?.host == model.connection?.credential.hostInstallationId && selected?.id == expected
                && scene?.settingsOpen == false && scene?.sidebarOpen == false
                && model.visibleChat?.id == expected && model.voice.isActive && model.voice.chatID == voiceConversation
        } ?? true
        let project = step.expectedProjectName.flatMap { name in model.projects.projects.first { $0.name == name } }
        let projectMatches = step.expectedProjectName == nil || project != nil
        return ["passed": configured && conversationMatches && projectMatches,
                "expectedConversation": step.expectedConversation ?? NSNull(),
                "host": selected?.host ?? NSNull(), "conversation": selected?.id ?? NSNull(),
                "visibleConversation": model.visibleChat?.id ?? NSNull(),
                "title": selected.flatMap { model.projects.details[$0.id]?.title } ?? NSNull(),
                "settingsOpen": scene?.settingsOpen ?? true, "sidebarOpen": scene?.sidebarOpen ?? true,
                "voiceConversation": model.voice.chatID ?? NSNull(), "voiceActive": model.voice.isActive,
                "expectedProjectName": step.expectedProjectName ?? NSNull(), "createdProjectID": project?.id ?? NSNull()]
    }

    func run(connection: VoiceRealtimeConnection, scene: ShellState?, model: ConnectionModel) async {
        // "Open X" is the owner's explicit destination change: voice follows it,
        // and only it. Anything else keeps the last target.
        var voiceConversation = model.voice.chatID
        do {
            for (index, step) in manifest.steps.enumerated() {
                try Task.checkCancellation()
                if step.expectedTool == "open_thread", let moved = step.expectedConversation { voiceConversation = moved }
                let inputURL = directory.appendingPathComponent(step.audio)
                let pcm = try await Task.detached(priority: .utility) { try Data(contentsOf: inputURL) }.value
                try Task.checkCancellation()
                guard !pcm.isEmpty, pcm.count <= 48_000 * 30, pcm.count.isMultiple(of: 2) else { throw URLError(.cannotDecodeContentData) }
                let beforeTranscript = finalTranscripts, beforeResponse = responses, beforeTools = tools.count
                let beforeFinished = finishedTools.count, beforeResult = toolResults.count, beforeUser = userTranscripts
                output = Data()
                // Exactly the microphone's 24 kHz mono PCM16 append path.
                let input = pcm + Data(repeating: 0, count: 48_000 * 2)
                for offset in stride(from: 0, to: input.count, by: 4_800) {
                    try Task.checkCancellation()
                    await connection.appendAudio(input.subdata(in: offset..<min(offset + 4_800, input.count)))
                    try await Task.sleep(for: .milliseconds(100))
                }
                let deadline = ContinuousClock.now + .seconds(30)
                while terminal == nil && (userTranscripts == beforeUser || finalTranscripts == beforeTranscript || responses == beforeResponse || outstanding > 0
                    || lastFinalResponse <= lastToolResponse || responses < lastFinalResponse), ContinuousClock.now < deadline {
                    // A real microphone keeps streaming while the owner is
                    // quiet. Semantic VAD's timeout advances with audio, so a
                    // finite fixture must continue supplying that silence.
                    await connection.appendAudio(Data(repeating: 0, count: 4_800))
                    try await Task.sleep(for: .milliseconds(100))
                }
                let called = Array(tools.dropFirst(beforeTools))
                let finished = Array(finishedTools.dropFirst(beforeFinished))
                let wordsMatch = step.expectedWordsAny.map { words in words.contains { lastFinalText.localizedCaseInsensitiveContains($0) } } ?? true
                let results = Array(toolResults.dropFirst(beforeResult))
                let successful = results.contains { result in
                    guard result["name"] as? String == step.expectedTool, let value = result["result"] as? [String: Any] else { return false }
                    return value["error"] == nil && value["status"] as? String != "failed"
                }
                let statusMatches = step.expectedStatus.map { expected in
                    results.contains { ($0["name"] as? String) == step.expectedTool && ($0["result"] as? [String: Any])?["status"] as? String == expected }
                } ?? true
                let sceneState = Self.sceneCheck(step: step, scene: scene, model: model, voiceConversation: voiceConversation)
                let passed = terminal == nil && successful && statusMatches && sceneState["passed"] as? Bool == true
                    && userTranscripts > beforeUser && finalTranscripts > beforeTranscript && called.contains(step.expectedTool)
                    && finished.contains(step.expectedTool) && lastFinalResponse > lastToolResponse && responses >= lastFinalResponse
                    && output.count > 0 && wordsMatch
                completed.append(["index": index + 1, "spoken": step.text, "expectedTool": step.expectedTool,
                                  "tools": called, "finishedTools": finished, "results": results, "spokenResult": lastFinalText,
                                  "passed": passed, "outputAudioBytes": output.count,
                                  "inputAudioBytes": pcm.count, "scene": sceneState])
                try save(passed: false, finished: false, audioIndex: index + 1)
                guard passed else { finish(passed: false, reason: terminal ?? (sceneState["passed"] as? Bool == false
                    ? "Visible scene did not match the command" : "Expected tool or spoken response was missing")); return }
            }
            finish(passed: true)
        } catch {
            finish(passed: false, reason: error is CancellationError ? "Session cancelled" : "Audio injection failed")
        }
    }

    func stop() { if !finished { finish(passed: false, reason: "Voice ended before all commands completed") } }

    private func finish(passed: Bool, reason: String? = nil) {
        guard !finished else { return }
        finished = true
        if let reason { terminal = reason }
        try? save(passed: passed, finished: true)
    }

    private func save(passed: Bool, finished: Bool, audioIndex: Int? = nil) throws {
        let lines = try events.map { try JSONSerialization.data(withJSONObject: $0, options: .sortedKeys) + Data([10]) }
        let transcript = lines.reduce(into: Data()) { $0.append($1) }
        let result: [String: Any] = ["runID": manifest.runID, "passed": passed, "finished": finished, "steps": completed,
                                     "failure": terminal ?? NSNull(), "usage": usage, "costUSD": NSNull(), "toolResults": toolResults,
                                     "responseCount": responses, "transcriptionCount": userTranscripts,
                                     "responseCompletions": responseCompletions,
                                     "usageComplete": usage.filter { $0["source"] as? String == "response" }.count == responses
                                        && usage.filter { $0["source"] as? String == "transcription" }.count == userTranscripts,
                                     "environment": "simulator, real Realtime, synthetic app host"]
        let resultData = try JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
        let directory = directory, audio = output, runID = manifest.runID
        // One ordered writer; publish the result sentinel after its evidence.
        files.async {
            do {
                if let audioIndex { try audio.write(to: directory.appendingPathComponent("output-\(audioIndex).pcm"), options: .atomic) }
                try transcript.write(to: directory.appendingPathComponent("transcripts.jsonl"), options: .atomic)
                try resultData.write(to: directory.appendingPathComponent("result.json"), options: .atomic)
            } catch {
                let failure: [String: Any] = ["runID": runID, "passed": false, "finished": true,
                                               "failure": "Voice evidence could not be written"]
                if let data = try? JSONSerialization.data(withJSONObject: failure) {
                    try? data.write(to: directory.appendingPathComponent("result.json"), options: .atomic)
                }
            }
        }
    }
}
#endif

// MARK: - Navigation

/// Voice's navigation goes through the window's `ShellState`, the same owner
/// the sidebar, header buttons and links use. It changes what is on screen only.
@MainActor struct VoiceNavigation {
    weak var scene: ShellState?
    let host: String

    private func shell() throws -> ShellState {
        guard let scene else {
            throw PairingFailure.hostMessage(409, "The voice window is no longer available. Open voice in a chat and try again.")
        }
        return scene
    }

    func open(_ screen: VoiceScreen, projectID: String?, setting: String?) throws -> String {
        let scene = try shell()
        switch screen {
        case .chats: scene.showChats(); return "Chats"
        case .search: scene.searchChats(); return "Search in Chats"
        case .needsYou: scene.showAttention(); return "Needs you"
        case .newChat:
            scene.newChat(host: host, destination: projectID.map { .project(id: $0) }, exactProject: projectID != nil)
            return "New chat"
        case .automations: scene.show(.automations); return "Automations"
        case .projectSettings:
            guard let projectID else { throw PairingFailure.hostMessage(422, "Name the Project.") }
            scene.show(.projectSettings(host: host, project: projectID)); return "Project settings"
        case .settings:
            guard let setting else { scene.show(.settings); return "Settings" }
            guard let row = SettingsSearch.matches(setting).first else {
                throw PairingFailure.hostMessage(404, "No Settings page matches \"\(setting)\".")
            }
            scene.show(.settings)
            scene.settingsRowRequest = row.id
            return "Settings: " + row.title
        }
    }

    /// The tool opens beside (or over) the conversation on screen; the open
    /// conversation answers the request, so wait briefly to say what happened.
    func open(_ pane: VoicePane) async throws -> String {
        let scene = try shell()
        guard scene.showsConversation else {
            throw PairingFailure.hostMessage(409, "No chat is open to show a tool beside. Open a chat first.")
        }
        let tool: CompanionTool? = switch pane {
        case .computer: .computer
        case .terminal: .terminal
        case .pullRequests: .pullRequests
        case .files: .files(WorkspaceBrowserRequest())
        case .details: .details
        case .close: nil
        }
        let request = ShellState.PaneRequest(tool: tool)
        scene.paneRequest = request
        for _ in 0..<15 where scene.paneRequest == request {
            try await Task.sleep(for: .milliseconds(100))
        }
        guard scene.paneRequest != request else {
            scene.paneRequest = nil
            throw PairingFailure.hostMessage(409, "That tool did not open. Check the chat on screen.")
        }
        return tool?.label ?? "Tool closed"
    }
}

// MARK: - Shared app commands

/// Typed app commands use the same stateful actions as the visible controls.
/// Voice never submits an independent message/approval HTTP request.
enum VoiceAppCommand: Equatable {
    case openThread(String)
    case send(thread: String, text: String)
    case sendNow(thread: String, clientMessageID: String)
    case createProject(name: String)
    case approval(thread: String, request: String, choice: String, nonce: String)
    case stop(String)
}

typealias VoiceAppCommandResult = VoiceCommandResult

@MainActor final class VoiceAppCommands {
    private weak var model: ConnectionModel?
    private weak var scene: ShellState?
    private let scope: String

    init(model: ConnectionModel, scene: ShellState? = nil) {
        self.model = model; self.scene = scene; scope = model.assignmentScope
    }

    private func current() throws -> ConnectionModel {
        guard let model, model.assignmentScope == scope, !model.accessEnded,
              model.connection != nil, !Task.isCancelled else { throw CancellationError() }
        return model
    }

    private func chat(_ id: String) async throws -> ChatSummary {
        let model = try current()
        if model.projects.details[id] == nil { await model.projects.refreshDetail(id) }
        _ = try current()
        guard let detail = model.projects.details[id] else {
            throw PairingFailure.hostMessage(404, "That chat is unavailable. Refresh your chats and try again.")
        }
        return model.projectChat(detail)
    }

    var navigation: VoiceNavigation? {
        guard let host = model?.connection?.credential.hostInstallationId else { return nil }
        return VoiceNavigation(scene: scene, host: host)
    }

    func queuedMessages(thread id: String) async throws -> [VoiceQueuedMessage] {
        let model = try current()
        let chat = try await chat(id)
        try await model.loadQueue(chat)
        _ = try current()
        return (model.queues[id] ?? []).map { VoiceQueuedMessage(id: $0.clientMessageId, text: $0.body) }
    }

    func approvals(thread id: String) async throws -> [VoiceApproval] {
        let model = try current()
        await model.loadAttention()
        _ = try current()
        return model.attention.filter { $0.belongs(to: id) && !$0.isQuestion && !$0.isSecretRequest }.map { request in
            // A reply saved but not confirmed can only be sent again as is.
            let saved = model.savedDecisions[request.id].flatMap { request.savedChoice($0) }
            let choices = saved.map { [$0] } ?? request.phoneChoices.filter { request.canSubmit(choice: $0, answers: [:]) }
            return VoiceApproval(id: request.id, summary: request.plainSummary, choices: choices.map { .init(id: $0.id, title: $0.title) },
                                 nonce: request.actionNonce, isComplete: request.summaryIsComplete)
        }
    }

    /// Questions the agent is waiting on, for voice to read; answering is on screen.
    func questions(thread id: String) async throws -> [VoiceQuestion] {
        let model = try current()
        await model.loadAttention()
        _ = try current()
        return model.attention.filter { $0.belongs(to: id) && $0.isQuestion }.flatMap { request in
            (request.params.questions ?? []).map { question in
                // A secret question is never read aloud.
                question.isSecret == true
                    ? VoiceQuestion(id: request.id, text: "A private question. Answer it on screen.")
                    : VoiceQuestion(id: request.id, text: String(question.question.prefix(500)),
                                    options: (question.options ?? []).prefix(6).map { String($0.label.prefix(80)) })
            }
        }
    }

    func perform(_ command: VoiceAppCommand) async throws -> VoiceAppCommandResult {
        let model = try current()
        switch command {
        case .openThread(let id):
            let chat = try await chat(id)
            guard let scene, let host = model.connection?.credential.hostInstallationId else {
                throw PairingFailure.hostMessage(409, "The voice window is no longer available. Open voice in a chat and try again.")
            }
            // This is the sidebar/button action. ConnectionModel.open alone
            // loads history but cannot change the per-window visible route.
            scene.open(host: host, conversation: id)
            return VoiceAppCommandResult(status: "opened", id: id, name: chat.title)
        case .send(let id, let text):
            let chat = try await chat(id)
            await model.refreshConversation(chat)
            _ = try current()
            switch model.fillEmptyDraft(text, chat: id) {
            case .saved: break
            case .occupied:
                throw PairingFailure.hostMessage(409, "That chat already has an unsent draft or attachments. Finish or clear them before sending by voice.")
            case .saveFailed(let explanation):
                throw PairingFailure.hostMessage(409, explanation)
            }
            guard !model.sendStopsMacReply(chat) else {
                throw PairingFailure.hostMessage(409, "The message is saved as a draft. Voice cannot queue behind this reply on your computer yet; wait for it to finish before sending.")
            }
            guard model.canSend(chat) else {
                throw PairingFailure.hostMessage(409, model.controlErrors[id] ?? model.composerErrors[id] ?? "The message is saved as a draft. Open the chat to check why it cannot send yet.")
            }
            let prior = Set((model.snapshots[id]?.messages ?? []).compactMap(\.clientMessageId)
                + (model.queues[id] ?? []).map(\.clientMessageId)
                + (model.composers[id]?.outbox ?? []).map(\.id))
            await model.send(chat)
            _ = try current()
            if model.composers[id]?.draft == text {
                throw PairingFailure.hostMessage(409, "The message is still saved as a draft. Open the chat to check its send settings.")
            }
            if let error = model.controlErrors[id] ?? model.composerErrors[id] { throw PairingFailure.hostMessage(409, error) }
            if let waiting = model.composers[id]?.outbox?.last(where: { $0.body == text && !prior.contains($0.id) }) {
                return VoiceAppCommandResult(status: "waiting_for_connection", id: waiting.id)
            }
            if let pending = model.composers[id]?.pending, pending.receipt == nil {
                throw PairingFailure.hostMessage(409, "The message is saved, but delivery is not confirmed. Check the chat before trying again.")
            }
            let queued = (model.queues[id] ?? []).last { $0.body == text && !prior.contains($0.clientMessageId) }
            let pending = model.composers[id]?.pending
            let delivered = model.snapshots[id]?.messages.last { $0.body == text && $0.clientMessageId.map { !prior.contains($0) } == true }
            let clientID = queued?.clientMessageId ?? pending?.request.clientMessageId ?? delivered?.clientMessageId
            let waits = queued != nil || pending?.receipt?.queued == true
            return VoiceAppCommandResult(status: waits ? "queued" : "sent", id: clientID)
        case .sendNow(let id, let clientID):
            let chat = try await chat(id)
            await model.refreshConversation(chat)
            try await model.loadQueue(chat)
            _ = try current()
            guard let item = model.queues[id]?.first(where: { $0.clientMessageId == clientID }),
                  let turn = model.sendNowTurn(chat, item: item) else {
                throw PairingFailure.hostMessage(409, "That message can no longer be sent now. Check the chat’s queue.")
            }
            try await model.changeQueue(chat, item: item, turn: turn)
            _ = try current()
            return VoiceAppCommandResult(status: "sent_now", id: clientID)
        case .createProject(let name):
            let project = try await model.projects.createProject(requestID: UUID().uuidString, name: name, newFolderParent: nil)
            _ = try current()
            return VoiceAppCommandResult(status: "created", id: project.id, name: project.name)
        case .approval(let id, let requestID, let choiceID, let nonce):
            await model.loadAttention()
            _ = try current()
            guard let request = model.attention.first(where: { $0.id == requestID && $0.belongs(to: id) }),
                  !request.isQuestion, request.actionNonce == nonce,
                  request.summaryIsComplete || choiceID == "decline" || choiceID == "cancel", let choice = request.phoneChoices.first(where: { $0.id == choiceID }),
                  request.canSubmit(choice: choice, answers: [:]) else {
                throw PairingFailure.hostMessage(409, "That approval or choice is no longer available. Open the request to check it.")
            }
            guard await model.resolve(request, choice: choice, answers: [:]) == .confirmed else {
                throw PairingFailure.hostMessage(409, model.attentionErrors[requestID] ?? "The approval reply was not confirmed. Check the request before trying again.")
            }
            _ = try current()
            return VoiceAppCommandResult(status: "resolved", id: requestID)
        case .stop(let id):
            let chat = try await chat(id)
            await model.refreshConversation(chat)
            _ = try current()
            guard !model.turnRunsElsewhere(id) else {
                throw PairingFailure.hostMessage(409, "That reply is running on your computer. Stop it there.")
            }
            guard model.activeTurn(id) != nil else { return VoiceAppCommandResult(status: "nothing_running") }
            await model.stop(chat)
            _ = try current()
            if let error = model.controlErrors[id] { throw PairingFailure.hostMessage(409, error) }
            return VoiceAppCommandResult(status: "stop_requested")
        }
    }
}

// MARK: - Tools against the Mac

/// Voice's tools, each an existing Wonder action on the paired Mac.
@MainActor final class ConnectionVoiceHost: VoiceHost {
    private weak var model: ConnectionModel?
    private let commands: VoiceAppCommands
    private let scope: String
    init(model: ConnectionModel, scene: ShellState? = nil) {
        self.model = model; commands = VoiceAppCommands(model: model, scene: scene); scope = model.assignmentScope
    }

    private func connected() throws -> ConnectionModel {
        guard let model, model.assignmentScope == scope, !model.accessEnded, model.connection != nil, !Task.isCancelled else { throw PairingFailure.response(401) }
        return model
    }

    func projects() async throws -> [VoiceProject] {
        let response: ProjectsResponse = try await connected().manage("/api/v1/projects")
        return response.projects.filter(\.isIncluded).map { VoiceProject(id: $0.id, name: $0.name) }
    }

    /// `GET /api/v1/threads/recent`: the Mac's newest threads across included projects.
    private struct RecentThreadsPage: Decodable, Sendable {
        struct Row: Decodable, Sendable {
            let projectId: String
            let projectName: String
            let thread: ProjectThreadSummary
        }
        let threads: [Row]
    }

    func threads(projectID: String?) async throws -> [VoiceThread] {
        let model = try connected()
        let scope = model.assignmentScope
        let result: [VoiceThread]
        if let projectID {
            if let project = try await projects().first(where: { $0.id == projectID }) {
                result = try await threads(in: project, limit: 15)
            } else { result = [] }
        } else {
            result = try await recentThreads()
        }
        guard scope == model.assignmentScope, !model.accessEnded, !Task.isCancelled else { throw CancellationError() }
        var seen = Set<String>()
        return result.filter { seen.insert($0.id).inserted }
    }

    /// The Mac's own cross-project list, read fresh rather than from whatever
    /// the sidebar loaded. A Mac without it gets every project's first page,
    /// a few at a time, merged by activity.
    private func recentThreads() async throws -> [VoiceThread] {
        do {
            let page: RecentThreadsPage = try await connected().manage("/api/v1/threads/recent?limit=\(VoiceContext.recentLimit)")
            return page.threads.map { Self.thread($0.thread, projectID: $0.projectId, projectName: $0.projectName) }
        } catch PairingFailure.response(404) {
            var merged: [VoiceThread] = []
            let available = try await projects()
            for start in stride(from: 0, to: available.count, by: 6) {
                let batch = available[start..<min(start + 6, available.count)]
                merged += try await withThrowingTaskGroup(of: [VoiceThread].self) { group in
                    for project in batch { group.addTask { [self] in try await threads(in: project, limit: 15) } }
                    var pages: [VoiceThread] = []
                    for try await page in group { pages += page }
                    return pages
                }
            }
            return Array(merged.sorted { $0.updatedAt > $1.updatedAt }.prefix(VoiceContext.recentLimit))
        }
    }

    private func threads(in project: VoiceProject, limit: Int) async throws -> [VoiceThread] {
        let page: ProjectThreadsPage = try await connected().manage(
            "/api/v1/projects/\(ConnectionModel.escape(project.id))/threads?limit=\(limit)")
        return page.threads.map { Self.thread($0, projectID: project.id, projectName: project.name) }
    }

    private static func thread(_ summary: ProjectThreadSummary, projectID: String, projectName: String) -> VoiceThread {
        VoiceThread(id: summary.conversationId ?? unattached(projectID: projectID, reference: summary.reference),
                    title: summary.title, projectName: projectName, provider: summary.family,
                    isWorking: summary.isWorking, updatedAt: summary.updatedAt)
    }

    /// Message search on the Mac, one row per thread, in any included project.
    /// Search returns messages, so one busy thread can fill a page; later
    /// pages are read (bounded) until enough distinct threads are found.
    func searchThreads(query: String) async throws -> [VoiceThread] {
        let model = try connected()
        let names = Dictionary(try await projects().map { ($0.id, $0.name) }, uniquingKeysWith: { first, _ in first })
        var seen = Set<String>()
        var found: [VoiceThread] = []
        var cursor: String?
        for _ in 0..<Self.searchPageBudget {
            try Task.checkCancellation()
            let page = try await model.searchMessages(query, cursor: cursor)
            for result in page.results {
                let projectID = result.projectId ?? result.conversationId.flatMap { model.projects.details[$0]?.projectId }
                guard let id = result.conversationId
                        ?? result.projectId.flatMap({ project in result.reference.map { Self.unattached(projectID: project, reference: $0) } }),
                      seen.insert(id).inserted else { continue }
                found.append(VoiceThread(id: id, title: result.title, projectName: projectID.flatMap { names[$0] },
                                         updatedAt: result.updatedDate.map { Int($0.timeIntervalSince1970) } ?? 0))
            }
            guard found.count < Self.searchThreadLimit, let next = page.nextCursor, next != cursor else { break }
            cursor = next
        }
        return Array(found.prefix(Self.searchThreadLimit))
    }

    private static let searchPageBudget = 5
    private static let searchThreadLimit = 10

    /// A provider thread Wonder has not opened yet is attached on first use.
    private static func unattached(projectID: String, reference: String) -> String {
        "attach:" + projectID + "|" + reference
    }

    private func conversation(_ threadID: String) async throws -> String {
        guard threadID.hasPrefix("attach:") else { return threadID }
        let parts = threadID.dropFirst("attach:".count).split(separator: "|", maxSplits: 1).map(String.init)
        guard parts.count == 2, let family = AgentFamily(rawValue: String(parts[1].prefix { $0 != ":" })) else {
            throw PairingFailure.response(404)
        }
        let summary = ProjectThreadSummary(reference: parts[1], conversationId: nil, title: "", family: family, updatedAt: 0)
        return try await connected().projects.attach(parts[0], thread: summary)
    }

    private func snapshot(_ conversationID: String) async throws -> ConversationSnapshot {
        try await connected().manage("/api/v1/conversations/\(ConnectionModel.escape(conversationID))")
    }

    func latestReply(threadID: String) async throws -> VoiceReply? {
        let snapshot = try await snapshot(conversation(threadID))
        let reply = snapshot.rows(author: "Agent").last { !$0.isUser && ($0.item == nil || $0.item?.type == "agentMessage") }
        guard let reply else { return nil }
        return VoiceReply(text: reply.text, isWorking: snapshot.activeTurnID != nil)
    }

    func send(threadID: String, text: String) async throws {
        let id = try await conversation(threadID)
        let result = try await commands.perform(.send(thread: id, text: text))
        if result.status == "waiting_for_connection" {
            throw PairingFailure.hostMessage(409, "The message is saved on this device and will send when your computer reconnects. It has not reached the agent yet; you do not need to send it again.")
        }
    }

    func openThread(threadID: String) async throws -> VoiceThread {
        let id = try await conversation(threadID)
        _ = try await commands.perform(.openThread(id))
        let model = try connected()
        guard let detail = model.projects.details[id] else { throw PairingFailure.response(404) }
        return VoiceThread(id: id, title: detail.title, projectName: detail.projectName, provider: detail.family)
    }

    func sendResult(threadID: String, text: String) async throws -> VoiceCommandResult {
        try await commands.perform(.send(thread: conversation(threadID), text: text))
    }

    func queuedMessages(threadID: String) async throws -> [VoiceQueuedMessage] {
        try await commands.queuedMessages(thread: conversation(threadID))
    }

    func sendNow(threadID: String, clientMessageID: String) async throws -> VoiceCommandResult {
        try await commands.perform(.sendNow(thread: conversation(threadID), clientMessageID: clientMessageID))
    }

    func approvals(threadID: String) async throws -> [VoiceApproval] {
        try await commands.approvals(thread: conversation(threadID))
    }

    func questions(threadID: String) async throws -> [VoiceQuestion] {
        try await commands.questions(thread: conversation(threadID))
    }

    func answerApproval(threadID: String, requestID: String, choiceID: String, nonce: String) async throws -> VoiceCommandResult {
        try await commands.perform(.approval(thread: conversation(threadID), request: requestID, choice: choiceID, nonce: nonce))
    }

    func createProject(name: String) async throws -> VoiceCommandResult {
        try await commands.perform(.createProject(name: name))
    }

    func startThread(projectID: String, provider: AgentFamily, model spoken: String?, text: String) async throws -> VoiceThread {
        let model = try connected()
        guard let device = model.connection?.credential.deviceId else { throw PairingFailure.response(401) }
        let options: AgentOptions = try await model.manage("/api/v1/agent-options")
        let models = options.models.filter { $0.family == provider && !$0.hidden }
        let choice: AgentOptions.Model
        if let spoken {
            let candidates = models.map { VoiceModelMatch.Candidate(id: $0.id, name: $0.displayName) }
            switch VoiceModelMatch.resolve(spoken, among: candidates) {
            case .match(let found):
                guard let option = models.first(where: { $0.id == found.id }) else { throw PairingFailure.response(422) }
                choice = option
            case .none:
                throw PairingFailure.hostMessage(422, "There is no \(provider.title) model called “\(spoken)”. Available: \(candidates.map(\.name).joined(separator: ", ")).")
            case .ambiguous(let several):
                throw PairingFailure.hostMessage(422, "“\(spoken)” fits several \(provider.title) models: \(several.map(\.name).joined(separator: ", ")).")
            }
        } else {
            let remembered = model.projects.projects.first { $0.id == projectID }?.lastModel(for: provider)
            guard let option = models.first(where: { $0.id == remembered?.model })
                    ?? models.first(where: { $0.isDefault == true }) ?? models.first else {
                throw PairingFailure.hostMessage(422, "\(provider.title) isn’t available on \(model.macName).")
            }
            choice = option
        }
        let fields: [String: Any] = ["deviceId": device, "clientMessageId": UUID().uuidString, "family": provider.rawValue,
                                     "model": choice.id, "accessMode": "workspace", "body": text, "prepareOnly": false]
        let response: CreateProjectThreadResponse = try await model.manage(
            "/api/v1/projects/\(ConnectionModel.escape(projectID))/threads", method: "POST",
            body: JSONSerialization.data(withJSONObject: fields))
        Task { await model.projects.loadThreads(projectID) }
        let thread = response.conversation
        return VoiceThread(id: thread.conversationId ?? thread.reference, title: thread.title.isEmpty ? "New chat" : thread.title,
                           provider: thread.family, isWorking: true)
    }

    func runningWork() async throws -> [VoiceThread] {
        try await threads(projectID: nil).filter(\.isWorking)
    }

    private func navigation() throws -> VoiceNavigation {
        _ = try connected()
        guard let navigation = commands.navigation else { throw PairingFailure.response(401) }
        return navigation
    }

    func openProject(projectID: String) async throws -> VoiceThread? {
        let navigation = try navigation()
        guard let project = try await projects().first(where: { $0.id == projectID }) else { throw PairingFailure.response(404) }
        let newest = try await threads(in: project, limit: 5).max { $0.updatedAt < $1.updatedAt }
        if let newest { return try await openThread(threadID: newest.id) }
        _ = try navigation.open(.newChat, projectID: projectID, setting: nil)
        return nil
    }

    func openScreen(_ screen: VoiceScreen, projectID: String?, setting: String?) async throws -> String {
        try navigation().open(screen, projectID: projectID, setting: setting)
    }

    func openPane(_ pane: VoicePane) async throws -> String {
        try await navigation().open(pane)
    }

    func overview() async throws -> VoiceOverview {
        let model = try connected()
        async let working = runningWork()
        await model.loadAttention()
        let waiting = model.attention.count
        return VoiceOverview(computer: model.macName, connected: model.macConnected == true,
                             working: try await working, needsYou: waiting)
    }

    func stopReply(threadID: String) async throws -> Bool {
        let id = try await conversation(threadID)
        return try await commands.perform(.stop(id)).status != "nothing_running"
    }
}

#if WONDER_DIAGNOSTICS
/// One scripted owner utterance and the tool call the model would make for it.
struct VoiceToolsFixtureStep {
    let number: Int
    let said: String
    let tool: String
    let arguments: String
    let reply: String

    /// Navigation, a new chat, a stop and an approval, each confirmed the way
    /// the router requires: a proposal, then a new final transcript naming the action.
    static let script: [VoiceToolsFixtureStep] = [
        .init(number: 1, said: "Open the Fork thread.", tool: "open_thread",
              arguments: #"{"thread":"fixture-fork-conversation"}"#, reply: "Opened. Now in Fork thread."),
        .init(number: 2, said: "Open the voice settings.", tool: "open_screen",
              arguments: #"{"screen":"settings","setting":"voice"}"#, reply: "Voice settings are open."),
        .init(number: 3, said: "Start a new Codex chat in Read status to check the build.", tool: "start_thread",
              arguments: #"{"project":"Read status","provider":"codex","text":"Check the build"}"#,
              reply: "Start a Codex thread in Read status? Say yes, start it."),
        .init(number: 4, said: "Yes, start the thread.", tool: "confirm_action", arguments: "", reply: "Started."),
        .init(number: 5, said: "Open the Fixture thread.", tool: "open_thread",
              arguments: #"{"thread":"fixture-parent-conversation"}"#, reply: "Opened. Now in Fixture thread."),
        .init(number: 6, said: "Open the terminal.", tool: "open_pane", arguments: #"{"pane":"terminal"}"#, reply: "Terminal is open."),
        .init(number: 7, said: "Close it.", tool: "open_pane", arguments: #"{"pane":"close"}"#, reply: "Closed."),
        .init(number: 8, said: "Stop the reply.", tool: "stop_reply", arguments: #"{"thread":"this"}"#,
              reply: "Stop the reply in Fixture thread? Say yes, stop the reply."),
        .init(number: 9, said: "Yes, stop the reply.", tool: "confirm_action", arguments: "", reply: "Stopped."),
        .init(number: 10, said: "Is anything waiting for me?", tool: "list_approvals", arguments: #"{"thread":"this"}"#,
              reply: "One approval: run the checks."),
        .init(number: 11, said: "Approve it.", tool: "answer_approval",
              arguments: #"{"thread":"this","request_id":"fixture-approval","choice_id":"accept"}"#,
              reply: "Allow once? Say yes, approve."),
        .init(number: 12, said: "Yes, approve it.", tool: "confirm_action", arguments: "", reply: "Approved."),
        .init(number: 13, said: "Show what needs me.", tool: "open_screen", arguments: #"{"screen":"needs_you"}"#,
              reply: "Needs you is open."),
    ]
}

/// The data side of voice's tools for fixture runs: threads, a running reply, an
/// approval and a new chat live in memory (the fixture computer serves none of
/// them), while every navigation call goes through the app's real `ShellState`.
@MainActor final class FixtureVoiceHost: VoiceHost {
    private weak var model: ConnectionModel?
    private let navigation: VoiceNavigation
    /// What the fixture computer was asked to do, in order.
    private(set) var log: [String] = []
    private var running = true
    private var approvalOpen = true
    private let project = VoiceProject(id: "read-project", name: "Read status")

    init(model: ConnectionModel, navigation: VoiceNavigation) { self.model = model; self.navigation = navigation }

    private func thread(_ id: String) -> VoiceThread {
        let title = model?.projects.details[id]?.title ?? (id == DiagnosticSubagentFixture.forkID ? "Fork chat" : "Fixture chat")
        return VoiceThread(id: id, title: title, projectName: project.name, provider: .codex,
                           isWorking: running && id == DiagnosticSubagentFixture.parentID)
    }
    private var threadList: [VoiceThread] { [DiagnosticSubagentFixture.parentID, DiagnosticSubagentFixture.forkID].map(thread) }

    func projects() async throws -> [VoiceProject] { [project] }
    func threads(projectID: String?) async throws -> [VoiceThread] { threadList }
    func latestReply(threadID: String) async throws -> VoiceReply? { VoiceReply(text: "Working on it.", isWorking: running) }
    func send(threadID: String, text: String) async throws { log.append("send:\(threadID)") }
    func startThread(projectID: String, provider: AgentFamily, model: String?, text: String) async throws -> VoiceThread {
        log.append("start:\(projectID):\(provider.rawValue):\(text)")
        return VoiceThread(id: "fixture-voice-new", title: "Check the build", projectName: project.name, provider: provider, isWorking: true)
    }
    func runningWork() async throws -> [VoiceThread] { threadList.filter(\.isWorking) }
    func stopReply(threadID: String) async throws -> Bool {
        log.append("stop:\(threadID)")
        defer { running = false }
        return running
    }
    func openThread(threadID: String) async throws -> VoiceThread {
        guard let scene = navigation.scene else { throw PairingFailure.response(404) }
        scene.open(host: navigation.host, conversation: threadID)
        log.append("open:\(threadID)")
        return thread(threadID)
    }
    func approvals(threadID: String) async throws -> [VoiceApproval] {
        approvalOpen ? [.init(id: "fixture-approval", summary: "Run the checks",
                              choices: [.init(id: "accept", title: "Allow once"), .init(id: "decline", title: "Decline")])] : []
    }
    func answerApproval(threadID: String, requestID: String, choiceID: String, nonce: String) async throws -> VoiceCommandResult {
        log.append("approval:\(requestID):\(choiceID)")
        approvalOpen = false
        return VoiceCommandResult(status: "resolved", id: requestID)
    }
    func openProject(projectID: String) async throws -> VoiceThread? { try await openThread(threadID: DiagnosticSubagentFixture.parentID) }
    func openScreen(_ screen: VoiceScreen, projectID: String?, setting: String?) async throws -> String {
        log.append("screen:\(screen.rawValue)")
        return try navigation.open(screen, projectID: projectID, setting: setting)
    }
    func openPane(_ pane: VoicePane) async throws -> String {
        log.append("pane:\(pane.rawValue)")
        return try await navigation.open(pane)
    }
    func overview() async throws -> VoiceOverview {
        VoiceOverview(computer: model?.macName ?? "", connected: true, working: threadList.filter(\.isWorking), needsYou: approvalOpen ? 1 : 0)
    }
}
#endif

// MARK: - Screen

private struct CompactVoiceBarKey: EnvironmentKey {
    static let defaultValue = false
}
extension EnvironmentValues {
    var compactVoiceBarVisible: Bool {
        get { self[CompactVoiceBarKey.self] }
        set { self[CompactVoiceBarKey.self] = newValue }
    }
}
struct VoiceBarVisibilityPreference: PreferenceKey {
    static let defaultValue = false
    static func reduce(value: inout Bool, nextValue: () -> Bool) { value = nextValue() || value }
}

/// The session's controls live above navigation, so Chats, New chat and chats
/// on other computers never depend on a Live Activity to mute or end voice.
struct VoiceSessionBar: View {
    @ObservedObject var controller: VoiceController
    @ObservedObject var scene: ShellState
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var showingVoice = false

    var body: some View {
        if controller.showsCompactControls(in: scene) {
            // A stable owner presents the sheet; remove its compact controls
            // entirely while the full surface owns both touch and VoiceOver.
            VStack(spacing: 0) {
                if !controller.sheetVisible(in: scene) { compactControls }
            }
            .preference(key: VoiceBarVisibilityPreference.self, value: !controller.sheetVisible(in: scene))
            .sheet(isPresented: $showingVoice) {
                VoiceSheet(controller: controller, macName: controller.computerName, scene: scene)
            }
        }
    }

    private var compactControls: some View {
        Group {
            if typeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 0) {
                    target
                    HStack { Spacer(minLength: 0); actions }
                }
            } else {
                HStack(spacing: 6) {
                    target
                    actions
                }
            }
        }
        .font(.footnote)
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
        .background(theme.chrome)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Voice on \(controller.computerName)")
        .accessibilityIdentifier("voice-session-bar:" + (controller.targetHostID ?? "standalone"))
        .transition(.identity)
        .transaction { if reduceMotion { $0.animation = nil } }
    }

    private var target: some View {
        Button {
            if controller.chatID != nil { controller.showTarget(in: scene) }
            else { showingVoice = true }
        } label: {
            Text(controller.targetLabel)
                .fontWeight(.medium)
                // The accessibility layout already gives the target its own
                // row. Keep that row compact so the enlarged actions and
                // keyboard leave Send reachable; VoiceOver keeps the full name.
                .lineLimit(typeSize.isAccessibilitySize ? 1 : 2)
                .multilineTextAlignment(.leading)
                .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(theme.accent)
        .accessibilityLabel(controller.targetLabel)
        .accessibilityHint("Opens the voice chat")
        .accessibilityIdentifier("voice-target:" + (controller.targetHostID ?? "standalone"))
    }

    private var actions: some View {
        HStack(spacing: 8) {
            Button { controller.toggleMute() } label: {
                Text(controller.muted ? "Unmute" : "Mute")
                    .frame(minWidth: 44, minHeight: 30)
                    .contentShape(Rectangle())
            }
                .tint(theme.accent)
                .disabled(!controller.canMute)
                .accessibilityLabel("\(controller.muted ? "Unmute" : "Mute") voice in \(controller.targetTitle)")
                .accessibilityIdentifier("voice-bar-mute:" + (controller.targetHostID ?? "standalone"))
            Button(role: .destructive) { controller.end() } label: {
                Text("End").frame(minWidth: 44, minHeight: 30).contentShape(Rectangle())
            }
                .tint(.red)
                .accessibilityLabel("End voice in \(controller.targetTitle)")
                .accessibilityIdentifier("voice-bar-end:" + (controller.targetHostID ?? "standalone"))
        }
        .font(.subheadline.weight(.semibold))
        .buttonStyle(.bordered)
        .controlSize(.regular)
        .fixedSize(horizontal: true, vertical: false)
    }
}

/// Only the starting chat replaces its composer with the full voice panel.
struct VoiceAwareComposer<Composer: View>: View {
    @ObservedObject var controller: VoiceController
    let chatID: String
    @ViewBuilder let composer: () -> Composer

    var body: some View {
        if controller.isActive, controller.chatID == chatID {
            VoicePanel(controller: controller)
        } else {
            composer()
        }
    }
}

struct VoicePanel: View {
    @ObservedObject var controller: VoiceController
    var large = false
    var fillsAvailableSpace = false
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @GestureState private var talkPressed = false

    var body: some View {
        VStack(spacing: 12) {
            if let pending = controller.pending {
                confirmation(pending)
            } else {
                ScrollView { captions }
                    .scrollBounceBehavior(.basedOnSize)
                    .frame(maxHeight: large ? 240 : 140)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(alignment: .center) {
                roundButton(controller.muted ? "mic.slash.fill" : "mic.fill",
                            label: controller.muted ? "Unmute" : "Mute", id: "voice-mute",
                            tint: controller.muted ? Color.orange : theme.text) { controller.toggleMute() }
                    .disabled(!controller.canMute)
                Spacer(minLength: 12)
                VStack(spacing: 6) {
                    if controller.talkAvailable || controller.talkButton.talking {
                        talkOrb
                    } else {
                        Button { controller.resumeAfterPause() } label: {
                            VoiceOrb(levels: controller.levels, phase: controller.phase, size: orbSize)
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel("Cupola Voice")
                        .accessibilityValue(controller.statusText)
                        // Only a paused session takes a tap; otherwise it is a status.
                        .accessibilityHint({ if case .paused = controller.phase { "Double-tap to listen again." } else { "" } }())
                        .accessibilityIdentifier("voice-orb")
                    }
                    // At large text, the pending action already explains what
                    // needs attention. Keep room to read it; the orb still
                    // exposes the live status to accessibility, and errors stay visible.
                    if !typeSize.isAccessibilitySize || controller.pending == nil || failed {
                        Text(controller.statusText)
                            .font(.footnote.weight(.medium))
                            .foregroundStyle(failed ? Color.red : theme.secondaryText)
                            .multilineTextAlignment(.center)
                            .lineLimit(3)
                            .accessibilityHidden(true)
                            .accessibilityIdentifier("voice-status")
                    }
                    if let window = controller.sessionWindow, !failed {
                        // Counts down by itself; nothing republishes per second.
                        Text("\(Text(timerInterval: window, countsDown: true)) left")
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(theme.secondaryText)
                            .accessibilityIdentifier("voice-time-left")
                    }
                }
                Spacer(minLength: 12)
                if controller.phase == .needsMicrophone {
                    roundButton("gear", label: "Open Settings", id: "voice-open-settings", tint: theme.text) {
                        controller.openMicrophoneSettings()
                    }
                } else if case .failed = controller.phase {
                    roundButton("arrow.clockwise", label: "Try again", id: "voice-retry", tint: theme.text) { controller.retry() }
                }
                roundButton("xmark", label: "End voice", id: "voice-end", tint: .white, fill: .red) { controller.end() }
            }
        }
        .padding(.horizontal, 16)
        .padding(.top, 10)
        .padding(.bottom, 8)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("voice-panel")
    }

    private var failed: Bool {
        switch controller.phase { case .failed, .needsMicrophone: true; default: false }
    }

    /// Hold to talk makes the orb a larger target.
    private var orbSize: CGFloat {
        if typeSize.isAccessibilitySize { return controller.talkMode == .holdToTalk ? 84 : 72 }
        return controller.talkMode == .holdToTalk ? (large ? 148 : 112) : (large ? 132 : 84)
    }

    /// Hold to talk: hold the orb while talking and let go to send, or tap
    /// once to start and again to send. VoiceOver's double-tap toggles the
    /// same way. The gesture state resets if the system cancels the touch,
    /// which sends what was said like a release.
    private var talkOrb: some View {
        let talking = controller.talkButton.talking
        return VoiceOrb(levels: controller.levels, phase: controller.phase, size: orbSize)
            .overlay {
                Circle().stroke(theme.accent, lineWidth: 4).scaleEffect(1.12).opacity(talking ? 1 : 0)
            }
            .contentShape(Circle())
            .gesture(DragGesture(minimumDistance: 0).updating($talkPressed) { _, pressed, _ in pressed = true })
            .onChange(of: talkPressed) { _, pressed in
                if pressed { controller.talkPressed() } else { controller.talkReleased() }
            }
            .accessibilityElement()
            .accessibilityAddTraits(.isButton)
            .accessibilityLabel(talking ? "Send" : "Talk")
            .accessibilityValue(controller.statusText)
            .accessibilityHint(talking ? "Sends what you said." : "Hold while you talk, or double-tap to start and double-tap again to send.")
            .accessibilityAction { controller.talkToggled() }
            .accessibilityIdentifier("voice-hold")
    }

    @ViewBuilder private var captions: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let notice = controller.targetNotice {
                Text(notice)
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(theme.accent)
                    .accessibilityIdentifier("voice-target-notice")
            }
            if !controller.userCaption.isEmpty {
                Text(controller.userCaption)
                    .font(.subheadline)
                    .foregroundStyle(theme.secondaryText)
                    .lineLimit(2, reservesSpace: false)
                    .accessibilityLabel("You said: \(controller.userCaption)")
                    .accessibilityIdentifier("voice-user-caption")
            }
            if !controller.agentCaption.isEmpty {
                Text(controller.agentCaption)
                    .font(.body)
                    .foregroundStyle(theme.text)
                    .lineLimit(large ? 8 : 3, reservesSpace: false)
                    .accessibilityLabel("Cupola: \(controller.agentCaption)")
                    .accessibilityIdentifier("voice-agent-caption")
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .animation(reduceMotion ? nil : .easeOut(duration: 0.15), value: controller.agentCaption)
    }

    private func confirmation(_ action: VoicePendingAction) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    Text(action.summary)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(theme.text)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("voice-confirmation-summary")
                    if let message = action.message {
                        Text(message)
                            .font(.subheadline)
                            .foregroundStyle(theme.text)
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .accessibilityIdentifier("voice-confirmation-message")
                    }
                }
            }
            .scrollBounceBehavior(.basedOnSize)
            .frame(maxHeight: fillsAvailableSpace ? .infinity : (large ? 240 : 180))
            .fixedSize(horizontal: false, vertical: !fillsAvailableSpace && !typeSize.isAccessibilitySize)
            let layout = typeSize.isAccessibilitySize
                ? AnyLayout(VStackLayout(spacing: 8)) : AnyLayout(HStackLayout())
            layout {
                Button { controller.resolvePending(action, confirm: false) } label: {
                    Text("Cancel").frame(maxWidth: typeSize.isAccessibilitySize ? .infinity : nil)
                }
                .buttonStyle(.bordered)
                .accessibilityIdentifier("voice-confirmation-cancel")
                if !typeSize.isAccessibilitySize { Spacer() }
                Button { controller.resolvePending(action, confirm: true) } label: {
                    Text(confirmTitle(action)).frame(maxWidth: typeSize.isAccessibilitySize ? .infinity : nil)
                }
                .buttonStyle(.wonderProminent)
                .accessibilityIdentifier("voice-confirmation-confirm")
            }
            .disabled(controller.resolvingPending)
        }
        .padding(12)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("voice-confirmation")
    }

    private func confirmTitle(_ action: VoicePendingAction) -> String {
        switch action.kind {
        case .send: "Send"
        case .sendNow: "Send now"
        case .startThread: "Start"
        case .stop: "Stop reply"
        case .createProject: "Create Project"
        case .approval(_, _, let choice, _): action.choiceTitle ?? (choice == "accept" ? "Allow once" : choice == "decline" ? "Decline" : "Confirm")
        }
    }

    private func roundButton(_ symbol: String, label: String, id: String, tint: Color, fill: Color? = nil,
                             action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 20, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 52, height: 52)
                .background(fill ?? theme.surface, in: Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
        .accessibilityIdentifier(id)
    }
}

/// The voice bubble: grows with the audio level; with Reduce Motion it keeps
/// its size and shows the level as a ring's brightness instead.
struct VoiceOrb: View {
    @ObservedObject var levels: VoiceLevels
    let phase: VoiceController.Phase
    let size: CGFloat
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let level = levels.level
        let speaking = phase == .speaking
        let dimmed: Bool = { switch phase { case .connecting, .paused, .failed, .needsMicrophone, .idle: true; default: false } }()
        ZStack {
            Circle()
                .fill(theme.accent.opacity(dimmed ? 0.12 : 0.22))
                .scaleEffect(reduceMotion ? 1.18 : 1 + 0.35 * level)
            Circle()
                .stroke(theme.accent.opacity(reduceMotion ? 0.25 + 0.6 * level : 0), lineWidth: 3)
                .scaleEffect(1.12)
            Circle()
                .fill(LinearGradient(colors: [theme.accent.opacity(dimmed ? 0.45 : 1),
                                              theme.accent.opacity(dimmed ? 0.3 : (speaking ? 0.55 : 0.75))],
                                     startPoint: .topLeading, endPoint: .bottomTrailing))
                .scaleEffect(reduceMotion ? 1 : 1 + 0.12 * level)
            if phase == .connecting {
                ProgressView().tint(.white)
            } else {
                Image(systemName: speaking ? "waveform" : "mic.fill")
                    .font(.system(size: size * 0.28, weight: .semibold))
                    .foregroundStyle(.white)
            }
        }
        .frame(width: size, height: size)
        .animation(reduceMotion ? nil : .easeOut(duration: 0.12), value: level)
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.25), value: speaking)
    }
}

/// Voice opened from the Action button or Shortcuts with no conversation on
/// screen.
struct VoiceSheet: View {
    @ObservedObject var controller: VoiceController
    let macName: String
    let scene: ShellState
    @State private var presentationID = UUID()
    @ScaledMetric(relativeTo: .body) private var compactHeight: CGFloat = 360
    @Environment(\.dismiss) private var dismiss
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        content
        .background { ThemeBackdrop() }
        .onChange(of: controller.isActive) { _, active in if !active { dismiss() } }
        .onAppear { controller.sheetAppeared(presentationID, in: scene) }
        .onDisappear { controller.sheetDisappeared(presentationID) }
        .presentationDetents(typeSize >= .xxxLarge ? [.large] : [.height(compactHeight), .large]).presentationDragIndicator(.hidden)
    }

    private var content: some View {
        VStack(spacing: 0) {
            HStack {
                Text(macName).font(.subheadline).foregroundStyle(theme.secondaryText)
                    .lineLimit(2).fixedSize(horizontal: false, vertical: true)
                    .accessibilityLabel("Voice on \(macName)")
                    .accessibilityIdentifier("voice-sheet-computer")
                Spacer()
            }
            .padding(.horizontal, 20).padding(.top, 20)
            if controller.pending == nil { Spacer(minLength: 0) }
            VoicePanel(controller: controller, fillsAvailableSpace: controller.pending != nil)
        }
    }
}

// MARK: - Settings

/// Settings → Voice: which paired computer's OpenAI key voice uses, kept on
/// this device. Voice started in any chat mints on that computer; with no
/// choice (or a computer since removed) it uses the chat's own computer.
@MainActor final class VoiceKeyChoice: ObservableObject {
    static let shared = VoiceKeyChoice()
    @Published private(set) var host: String?
    private weak var library: ConnectionLibrary?
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        host = defaults.string(forKey: VoiceKeySource.storageKey)
    }

    func attach(_ library: ConnectionLibrary) { self.library = library }

    func choose(_ host: String?) {
        guard host != self.host else { return }
        self.host = host
        if let host { defaults.set(host, forKey: VoiceKeySource.storageKey) }
        else { defaults.removeObject(forKey: VoiceKeySource.storageKey) }
    }

    /// The computer whose key starts a voice session begun in a chat on `model`'s computer.
    func minter(for model: ConnectionModel) -> ConnectionModel {
        guard let library, let chatHost = model.connection?.credential.hostInstallationId else { return model }
        let paired = library.paired
        let host = VoiceKeySource.minter(chosen: host, paired: paired.map(\.credential.hostInstallationId), chatHost: chatHost)
        guard host != chatHost, let saved = paired.first(where: { $0.credential.hostInstallationId == host }) else { return model }
        return library.model(for: saved)
    }
}

/// Settings → Voice: whose OpenAI key voice uses, what voice asks before
/// doing (this device's choice), and that computer's limits and transcripts.
struct VoiceSettingsView: View {
    @ObservedObject var library: ConnectionLibrary
    @State private var searchHost: String?
    @ObservedObject private var choice = VoiceKeyChoice.shared
    /// Each paired computer's key status, by computer; absent until read.
    @State private var statuses: [String: VoiceStatus] = [:]
    /// Computers whose status could not be read.
    @State private var unreadable: Set<String> = []
    @State private var checking = false
    @State private var savingKey = false
    @State private var keyMessage: (text: String, failed: Bool)?
    @State private var adding = false
    @State private var removing = false
    @State private var askBefore = VoiceAskBefore.load(.standard)
    @State private var talkMode = VoiceTalkMode.load(.standard)
    @AppStorage(VoiceAnnouncementSetting.defaultsKey) private var announce = true
    @State private var limits: VoiceLimits?
    @State private var limitsHost: String?
    @State private var loadingLimits = false
    @State private var savingOptions = false
    @State private var optionsMessage: (text: String, failed: Bool)?
    @State private var deleting = false

    init(library: ConnectionLibrary, settingsHost: String? = nil) {
        self.library = library
        _searchHost = State(initialValue: settingsHost)
    }

    private var hosts: [String] { library.paired.map(\.credential.hostInstallationId) }

    private func model(_ host: String) -> ConnectionModel? {
        library.paired.first { $0.credential.hostInstallationId == host }.map { library.model(for: $0) }
    }

    private func name(_ host: String) -> String { model(host)?.macName ?? "your computer" }

    /// The computers the picker offers: those with a key, and the chosen one
    /// while it is still paired (it may be offline).
    private var options: [String] {
        hosts.filter { statuses[$0]?.configured == true || $0 == choice.host || $0 == searchHost }
    }

    /// The computer shown as chosen.
    private var selected: String? {
        if let host = searchHost, hosts.contains(host) { return host }
        if let chosen = choice.host, hosts.contains(chosen) { return chosen }
        return VoiceKeySource.effective(chosen: nil, keyed: hosts.filter { statuses[$0]?.configured == true })
    }

    var body: some View {
        WonderForm {
            if hosts.isEmpty {
                Section { Text("Pair a computer to use voice.").foregroundStyle(.secondary) }
            } else {
                if !options.isEmpty { keySourceSection.id(SettingsSearch.Anchor.voiceSource.id) }
                keySection.id(SettingsSearch.Anchor.voiceKey.id)
                talkSection.id(SettingsSearch.Anchor.voiceTalking.id)
                askBeforeSection.id(SettingsSearch.Anchor.voiceAsk.id)
                announceSection.id(SettingsSearch.Anchor.voiceAnnounce.id)
                if let selected, let model = model(selected) { macSections(model, host: selected) }
            }
        }
        .accessibilityIdentifier("voice-settings-list")
        .settingsNavigationTitle("Voice").navigationBarTitleDisplayMode(.inline)
        .task(id: hosts) { await checkKeys() }
        .task(id: selected) { await loadLimits() }
        .sheet(isPresented: $adding) {
            VoiceKeyEntry(computers: hosts.compactMap { host in model(host).map { (host, $0) } },
                          initial: selected) { host, key in Task { await save(key, on: host) } }
        }
        .confirmationDialog(selected.map { "Remove the OpenAI key from \(name($0))?" } ?? "", isPresented: $removing, titleVisibility: .visible) {
            Button("Remove key", role: .destructive) { if let selected { Task { await remove(from: selected) } } }
        } message: { Text("Voice uses another computer’s key, or stops working until you add one again.") }
        .confirmationDialog(selected.map { "Delete saved voice transcripts on \(name($0))?" } ?? "", isPresented: $deleting, titleVisibility: .visible) {
            Button("Delete transcripts", role: .destructive) {
                if let selected, let model = model(selected) { Task { await deleteTranscripts(model) } }
            }
        } message: { Text("Messages voice sent stay in their chats. Today’s voice use still counts toward your limits.") }
    }

    // MARK: Key

    private var keySourceSection: some View {
        Section {
            Picker("Use the key on", selection: Binding(get: { selected ?? "" }, set: { searchHost = nil; choice.choose($0) })) {
                ForEach(options, id: \.self) { host in Text(name(host)).tag(host) }
            }
            .accessibilityIdentifier("voice-key-computer").settingsRow("voice-key-computer")
        } header: { Text("Voice computer") }
    }

    private var keySection: some View {
        Section {
            if options.isEmpty {
                if checking { ProgressView("Checking voice…") }
                else {
                    Text("No computer has an OpenAI key yet.").foregroundStyle(.secondary)
                        .accessibilityIdentifier("voice-key-none")
                }
            }
            if !unreadable.isEmpty, !checking {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Couldn’t check \(unreadable.sorted().map(name).formatted(.list(type: .and))). Connect to see whether it has a key.")
                        .font(.subheadline).foregroundStyle(.secondary)
                    Button("Try again") { Task { await checkKeys() } }
                }
                .accessibilityIdentifier("voice-key-unreadable")
            }
            Button("Add a key…") { keyMessage = nil; adding = true }
                .disabled(savingKey || hosts.allSatisfy { model($0)?.macConnected != true })
                .accessibilityIdentifier("voice-key-add").settingsRow("voice-key-add")
            if let selected, statuses[selected]?.configured == true {
                Button("Remove key from \(name(selected))", role: .destructive) { removing = true }
                    .disabled(savingKey || model(selected)?.macConnected != true)
                    .accessibilityIdentifier("voice-key-remove").settingsRow("voice-key-remove")
            }
            if savingKey { ProgressView("Checking the key with OpenAI…") }
            if let keyMessage {
                Text(keyMessage.text).font(.caption).foregroundStyle(keyMessage.failed ? Color.red : Color.secondary)
                    .accessibilityIdentifier("voice-key-message")
            }
        } header: {
            Text("OpenAI key")
        } footer: {
            Text("Voice uses your own OpenAI account. The key stays in its computer’s Keychain; this device gets a short-lived pass for each voice session. OpenAI bills voice by use, roughly $0.10 per minute of talking.")
        }
    }

    private func checkKeys() async {
        guard !checking else { return }
        checking = true; defer { checking = false }
        let computers = hosts.compactMap { host in model(host).map { (host, $0) } }
        var read: [String: VoiceStatus] = [:]
        var failed: Set<String> = []
        // Every computer is asked at once; leaving the page cancels the reads.
        let reads = computers.map { host, model in
            (host, Task { @MainActor () -> VoiceStatus? in try? await model.manage("/api/v1/voice") })
        }
        let tasks = reads.map(\.1)
        await withTaskCancellationHandler {
            for (host, task) in reads {
                if let status = await task.value { read[host] = status } else { failed.insert(host) }
            }
        } onCancel: { tasks.forEach { $0.cancel() } }
        guard !Task.isCancelled else { return }
        statuses = read
        unreadable = failed
        // Keep what the page shows chosen: the stored computer unless it is
        // gone or answered that it has no key.
        if let chosen = choice.host, hosts.contains(chosen), read[chosen]?.configured != false { return }
        choice.choose(VoiceKeySource.effective(chosen: nil, keyed: hosts.filter { read[$0]?.configured == true }))
    }

    private func save(_ key: String, on host: String) async {
        guard let model = model(host) else { return }
        savingKey = true; keyMessage = nil
        defer { savingKey = false }
        do {
            statuses[host] = try await model.manage("/api/v1/voice/openai-key", method: "PUT",
                                                    body: JSONEncoder().encode(["apiKey": key]))
            unreadable.remove(host)
            searchHost = nil
            choice.choose(host)
            keyMessage = ("Key saved on \(model.macName). Voice uses it.", false)
        } catch PairingFailure.hostMessage(_, let detail) {
            keyMessage = (detail, true)
        } catch {
            keyMessage = ("The key wasn’t saved. \(managementError(error))", true)
        }
    }

    private func remove(from host: String) async {
        guard let model = model(host) else { return }
        savingKey = true; keyMessage = nil
        defer { savingKey = false }
        do {
            statuses[host] = try await model.manage("/api/v1/voice/openai-key", method: "DELETE")
            searchHost = nil
            choice.choose(VoiceKeySource.effective(chosen: nil, keyed: hosts.filter { statuses[$0]?.configured == true }))
            keyMessage = ("Key removed from \(model.macName).", false)
        } catch PairingFailure.hostMessage(_, let detail) {
            keyMessage = (detail, true)
        } catch {
            keyMessage = ("The key wasn’t removed. \(managementError(error))", true)
        }
    }

    // MARK: Taking turns

    private var talkSection: some View {
        Section {
            Picker("Talking", selection: $talkMode) {
                Text("Hands-free").tag(VoiceTalkMode.handsFree)
                Text("Hold to talk").tag(VoiceTalkMode.holdToTalk)
            }
            .accessibilityIdentifier("voice-talk-mode").settingsRow("voice-talk-mode")
        } footer: {
            Text("Hands-free notices when you stop talking, so noise can cut you off. With Hold to talk, voice hears you only while you hold the orb and answers when you let go; you can also tap once to start and again to send. Changes apply the next time you start voice.")
        }
        .onChange(of: talkMode) { _, value in value.save(.standard) }
    }

    // MARK: Ask before

    private var askBeforeSection: some View {
        Section {
            Toggle("Sending messages", isOn: $askBefore.send).systemSwitch().accessibilityIdentifier("voice-ask-send").settingsRow("voice-ask-send")
            Toggle("Starting chats", isOn: $askBefore.startThread).systemSwitch().accessibilityIdentifier("voice-ask-start-thread").settingsRow("voice-ask-start-thread")
            Toggle("Stopping replies", isOn: $askBefore.stopReply).systemSwitch().accessibilityIdentifier("voice-ask-stop").settingsRow("voice-ask-stop")
            Toggle("Answering approvals", isOn: $askBefore.answerApproval).systemSwitch().accessibilityIdentifier("voice-ask-approval").settingsRow("voice-ask-approval")
            Toggle("Creating Projects", isOn: $askBefore.createProject).systemSwitch().accessibilityIdentifier("voice-ask-create-project").settingsRow("voice-ask-create-project")
        } header: {
            Text("Ask before")
        } footer: {
            Text("Voice shows what it will do and waits until you confirm by voice or tap. Turn an action off to have voice do it as soon as you ask. Sending includes Send now. Changes apply the next time you start voice.")
        }
        .onChange(of: askBefore) { _, value in value.save(.standard) }
    }

    // MARK: Announcements

    private var announceSection: some View {
        Section {
            Toggle("Replies, approvals and questions", isOn: $announce).systemSwitch()
                .accessibilityIdentifier("voice-announce").settingsRow("voice-announce")
        } header: {
            Text("Announcements")
        } footer: {
            Text("While voice is on, it tells you out loud when the chat it started in finishes a reply, needs an approval or asks a question, and offers to read it. It waits while you are talking or muted.")
        }
    }

    // MARK: Limits and transcripts on the chosen computer

    @ViewBuilder private func macSections(_ model: ConnectionModel, host: String) -> some View {
        if let limits, limitsHost == host {
            limitsSection(limits, model: model).id(SettingsSearch.Anchor.voiceLimits.id)
            transcriptsSection(limits, model: model).id(SettingsSearch.Anchor.voiceTranscripts.id)
        } else if loadingLimits {
            Section { ProgressView("Checking voice settings…") }
        } else {
            Section {
                Text(model.macConnected == true ? "Cupola couldn’t read voice settings on \(model.macName)." : "Connect to \(model.macName) to change voice limits.")
                    .foregroundStyle(.secondary)
                Button("Try again") { Task { await loadLimits() } }.disabled(loadingLimits)
            }
        }
        if let optionsMessage {
            Section {
                Text(optionsMessage.text).font(.caption).foregroundStyle(optionsMessage.failed ? Color.red : Color.secondary)
                    .accessibilityIdentifier("voice-options-message")
            }
        }
    }

    private func busy(_ model: ConnectionModel) -> Bool {
        savingOptions || loadingLimits || model.accessEnded || model.macConnected != true
    }

    private func limitsSection(_ limits: VoiceLimits, model: ConnectionModel) -> some View {
        Section {
            Picker("Session length", selection: Binding(get: { limits.sessionLimitMinutes },
                                                        set: { minutes in save(["sessionLimitMinutes": minutes], model) })) {
                ForEach(Self.options([5, 10, 15, 30, 45, limits.sessionLimitMaxMinutes], current: limits.sessionLimitMinutes)
                    .filter { $0 <= limits.sessionLimitMaxMinutes }, id: \.self) { Text("\($0) min").tag($0) }
            }
            .accessibilityIdentifier("voice-session-limit").settingsRow("voice-session-limit")
            Picker("Daily minutes", selection: Binding(get: { limits.dailyMinutes ?? 0 },
                                                       set: { minutes in save(["dailyMinutes": minutes == 0 ? NSNull() : minutes], model) })) {
                Text("No limit").tag(0)
                ForEach(Self.options([15, 30, 60, 120, 240], current: limits.dailyMinutes), id: \.self) { Text("\($0) min").tag($0) }
            }
            .accessibilityIdentifier("voice-daily-minutes").settingsRow("voice-daily-minutes")
            Picker("Daily spending", selection: Binding(get: { limits.dailyDollars.map { Int(($0 * 100).rounded()) } ?? 0 },
                                                        set: { cents in save(["dailyDollars": cents == 0 ? NSNull() : Double(cents) / 100], model) })) {
                Text("No limit").tag(0)
                ForEach(Self.options([100, 200, 500, 1000, 2000], current: limits.dailyDollars.map { Int(($0 * 100).rounded()) }), id: \.self) {
                    Text(Self.dollars(Double($0) / 100)).tag($0)
                }
            }
            .accessibilityIdentifier("voice-daily-dollars").settingsRow("voice-daily-dollars")
            if let today = limits.today {
                LabeledContent("Used today", value: "\(Self.minutes(today.spokenSeconds)) · about \(Self.dollars(today.estimatedDollars))")
                    .accessibilityIdentifier("voice-used-today")
            }
        } header: {
            Text("Limits").accessibilityIdentifier("voice-limits-header")
        } footer: {
            Text("Voice ends a session when it reaches its length. When today’s minutes or spending run out, voice doesn’t start again until midnight. Spending is an estimate from OpenAI’s prices; your OpenAI bill is the final amount.")
        }
        .disabled(busy(model))
    }

    private func transcriptsSection(_ limits: VoiceLimits, model: ConnectionModel) -> some View {
        Section {
            Toggle("Keep transcripts", isOn: Binding(get: { limits.keepTranscripts }, set: { keep in save(["keepTranscripts": keep], model) }))
                .systemSwitch().accessibilityIdentifier("voice-keep-transcripts").settingsRow("voice-keep-transcripts")
            Button("Delete saved transcripts", role: .destructive) { deleting = true }
                .accessibilityIdentifier("voice-delete-transcripts").settingsRow("voice-delete-transcripts")
        } header: {
            Text("Transcripts")
        } footer: {
            Text("What you and voice said is kept as text on \(model.macName), with the chats voice acted on. No audio is kept.")
        }
        .disabled(busy(model))
    }

    private static func options(_ values: [Int], current: Int?) -> [Int] {
        Array(Set(values + (current.map { [$0] } ?? []))).filter { $0 > 0 }.sorted()
    }

    private static func dollars(_ value: Double) -> String { value.formatted(.currency(code: "USD")) }

    private static func minutes(_ seconds: Int) -> String {
        seconds < 60 ? "under 1 min" : "\(seconds / 60) min"
    }

    private func loadLimits() async {
        guard let host = selected, let model = model(host) else { limits = nil; limitsHost = nil; return }
        loadingLimits = true; defer { loadingLimits = false }
        optionsMessage = nil
        do {
            let read: VoiceLimits = try await model.manage("/api/v1/voice/settings")
            guard !Task.isCancelled, selected == host else { return }
            limits = read; limitsHost = host
        } catch is CancellationError {
        } catch {
            guard selected == host else { return }
            limits = nil; limitsHost = nil
        }
    }

    private func save(_ change: [String: Any], _ model: ConnectionModel) {
        guard !savingOptions, let host = limitsHost, let body = try? JSONSerialization.data(withJSONObject: change) else { return }
        savingOptions = true; optionsMessage = nil
        Task {
            defer { savingOptions = false }
            do {
                let saved: VoiceLimits = try await model.manage("/api/v1/voice/settings", method: "PUT", body: body)
                if limitsHost == host { limits = saved }
            } catch is CancellationError {
            } catch {
                optionsMessage = ("The change wasn’t saved. \(managementError(error))", true)
            }
        }
    }

    private func deleteTranscripts(_ model: ConnectionModel) async {
        savingOptions = true; optionsMessage = nil
        defer { savingOptions = false }
        struct Deleted: Decodable, Sendable { let deletedSessions: Int }
        do {
            let deleted: Deleted = try await model.manage("/api/v1/voice/transcripts", method: "DELETE")
            optionsMessage = (deleted.deletedSessions == 0 ? "No saved transcripts." : "Saved transcripts deleted from \(model.macName).", false)
        } catch is CancellationError {
        } catch {
            optionsMessage = ("The transcripts weren’t deleted. \(managementError(error))", true)
        }
    }
}

/// Adds or replaces an OpenAI key on one paired computer.
private struct VoiceKeyEntry: View {
    let computers: [(String, ConnectionModel)]
    let save: (String, String) -> Void
    @State private var host: String
    @State private var draftKey = ""
    @Environment(\.dismiss) private var dismiss

    init(computers: [(String, ConnectionModel)], initial: String?, save: @escaping (String, String) -> Void) {
        self.computers = computers; self.save = save
        let connected = computers.filter { $0.1.macConnected == true }.map(\.0)
        _host = State(initialValue: initial.flatMap { connected.contains($0) ? $0 : nil } ?? connected.first ?? "")
    }

    private var name: String { computers.first { $0.0 == host }?.1.macName ?? "your computer" }

    var body: some View {
        NavigationStack {
            WonderForm {
                Section {
                    Picker("Save on", selection: $host) {
                        ForEach(computers.filter { $0.1.macConnected == true }, id: \.0) { Text($0.1.macName).tag($0.0) }
                    }
                    .accessibilityIdentifier("voice-key-entry-computer")
                    SecureField("sk-…", text: $draftKey)
                        .textContentType(.password)
                        .autocorrectionDisabled().textInputAutocapitalization(.never)
                        .accessibilityLabel("OpenAI API key")
                        .accessibilityIdentifier("voice-key-field")
                } footer: {
                    Text("Create a key at platform.openai.com. Cupola sends it to \(name) over your paired connection and checks it with OpenAI; it isn’t stored on this device. A key already there is replaced.")
                }
            }
            .navigationTitle("OpenAI key").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { draftKey = ""; dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        let key = draftKey.trimmingCharacters(in: .whitespacesAndNewlines)
                        draftKey = ""
                        save(host, key)
                        dismiss()
                    }
                    .disabled(draftKey.trimmingCharacters(in: .whitespacesAndNewlines).count < 20 || host.isEmpty)
                    .accessibilityIdentifier("voice-key-save")
                }
            }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
    }
}

// MARK: - Action button and Shortcuts

/// Set by the intent; the app shell starts voice when it sees it.
@MainActor final class VoiceLaunch: ObservableObject {
    static let shared = VoiceLaunch()
    @Published var requested = false
}

struct StartWonderVoiceIntent: AppIntent {
    static let title: LocalizedStringResource = "Start Cupola Voice"
    static let description = IntentDescription("Opens Cupola and starts a voice session with your agents.")
    static let openAppWhenRun = true

    @MainActor func perform() async throws -> some IntentResult {
        VoiceLaunch.shared.requested = true
        return .result()
    }
}


