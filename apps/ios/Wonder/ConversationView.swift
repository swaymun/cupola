import SwiftUI
import Combine
import AVKit
import ImageIO
import PhotosUI
import WonderPairing
import WonderComputerView
import UniformTypeIdentifiers
import PDFKit
import WebKit
import VisionKit
import Photos

struct FailureDetails: View {
    let title: String
    let message: String
    @State private var isOpen: Bool
    /// `open` shows the reason at once (a refused approval answer: the reason is the point).
    init(_ title: String = "Action failed", message: String, open: Bool = false) {
        self.title = title
        self.message = message
        _isOpen = State(initialValue: open)
    }
    var body: some View {
        DisclosureGroup(isExpanded: $isOpen) {
            Text(message).font(.caption).foregroundStyle(.secondary)
        } label: {
            Text(title).font(.caption).foregroundStyle(.secondary)
        }.tint(.secondary)
    }
}

/// Why a send, attachment, Stop or draft save failed. It stays until the
/// problem is resolved or the owner closes it: a reason that fades while Send
/// is still blocked would leave the draft looking idle. Closing clears it in
/// the model, so the same failure happening again shows again.
struct ComposerIssue: View {
    let message: String
    let id: String
    /// A recovery step, such as starting over after an unreadable draft.
    var action: (title: String, hint: String, run: () -> Void)? = nil
    let dismiss: () -> Void
    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "exclamationmark.circle").foregroundStyle(.orange).accessibilityHidden(true)
            Text(message).font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier(id)
            if let action {
                Button(action.title, action: action.run)
                    .font(.caption.weight(.semibold))
                    .frame(minHeight: 44)
                    .accessibilityHint(action.hint)
                    .accessibilityIdentifier(id + "-action")
            }
            Button(action: dismiss) {
                Image(systemName: "xmark").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    .frame(width: 44, height: 44).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Dismiss")
            .accessibilityIdentifier(id + "-dismiss")
        }
        .accessibilityElement(children: .contain)
    }
}

/// An Edit from here the Mac never confirmed: the message stays saved with the
/// draft until the chat's history shows whether it happened.
struct UnconfirmedRewindNotice: View {
    let computer: String
    let checking: Bool
    let check: () -> Void
    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "exclamationmark.circle").foregroundStyle(.orange).accessibilityHidden(true)
            Text("Couldn’t confirm the earlier edit with \(computer). Your message is saved here.")
                .font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("rewind-unconfirmed")
            if checking {
                ProgressView().frame(minHeight: 44).accessibilityLabel("Checking").accessibilityIdentifier("rewind-unconfirmed-checking")
            } else {
                Button("Check again", action: check)
                    .font(.caption.weight(.semibold))
                    .frame(minHeight: 44)
                    .accessibilityHint("Reads the chat again to see whether the edit happened.")
                    .accessibilityIdentifier("rewind-unconfirmed-check")
            }
        }
        .accessibilityElement(children: .contain)
    }
}

/// Per-file progress while Send uploads attachments: the file going now, and
/// how many of the message's files are already on the computer.
struct ComposerUploadProgress: View {
    let progress: ConnectionModel.UploadProgress
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(progress.label).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
            ProgressView(value: Double(progress.receivedBytes), total: Double(max(1, progress.totalBytes)))
            Text("\(ByteCountFormatter.string(fromByteCount: Int64(progress.receivedBytes), countStyle: .file)) of \(ByteCountFormatter.string(fromByteCount: Int64(progress.totalBytes), countStyle: .file))")
                .font(.caption2).foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("composer-upload-progress")
    }
}

/// A Claude chat another app on the Mac holds (Claude Code in a terminal, or
/// an app still running background work in it) keeps its conversation in
/// memory there, so Wonder holds a message until that app lets go, or until
/// Take over takes it over. The note names the app that actually holds it.
struct OpenOnMacNotice: View {
    let waiting: Bool
    let holder: MacHolder?
    let computer: String
    /// ChatGPT's desktop app takes the message into the Codex thread it runs.
    var relayed = false
    /// What T3 Code's send can't carry in this message (`ConnectionModel.t3CodeLimit`).
    var t3CodeLimit: String? = nil
    /// The note, or nil when there is nothing to say: a message goes into
    /// T3 Code's thread there as it is, like any send.
    static func text(holder: MacHolder?, computer: String, relayed: Bool = false, t3CodeLimit: String? = nil) -> String? {
        guard let holder else { return "Sends after you exit Claude Code on your computer." }
        if relayed { return "Sending through \(holder.app) on \(computer). Approvals appear there." }
        // T3 Code takes a message into its thread once Cupola is connected to it.
        switch holder.t3Code {
        case "sends" where t3CodeLimit == nil: return nil
        case "sends", "cantSend":
            let what = switch t3CodeLimit {
            case "attachments": "Files this large"
            case "skills": "Skills"
            case "model": "A model change"
            default: "This message"
            }
            // Take over cannot stop the codex app-server T3 Code runs: the message waits for its reply.
            let then = holder.canTakeOver ? "it sends after you close it there, or hold Send to take over." : "it sends when the reply there finishes."
            return "\(what) can’t be sent through \(holder.app); \(then)"
        case "connect", "notFound":
            // Why it can't go into T3 Code's thread, and what to do: never a silent queue.
            if let note = T3CodeNote.text(delivery: holder.t3Code, reason: holder.t3CodeReason,
                                          computer: computer, canTakeOver: holder.canTakeOver) { return note }
        default: break
        }
        // An app built on the Agent SDK (T3 Code) holds the chat while it is open
        // there, idle or not; only closing it there or Take over releases it.
        let when = holder.kind == "terminal" ? "Sends after you exit it there."
            : holder.kind == "app" ? "Sends after you close it there."
            : holder.background ? "Sends when its background work finishes." : "Sends when it’s free there."
        let takeOver = holder.canTakeOver ? " Touch and hold Send to take over." : ""
        return "Open in \(holder.app) on \(computer). \(when)\(takeOver)"
    }
    var body: some View {
        if waiting, let text = Self.text(holder: holder, computer: computer, relayed: relayed, t3CodeLimit: t3CodeLimit) {
            Label(text, systemImage: holder?.kind == "terminal" || holder == nil ? "terminal" : "macwindow")
                .font(.caption).foregroundStyle(.secondary)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("open-elsewhere")
        }
    }
}

/// The thread waits for the owner in the app on the computer (a permission
/// prompt or a question there) and Cupola can't answer it: one line above the
/// composer, with View computer to answer it on the computer's screen. When
/// the screen can't be shown, the button stays disabled and the line says why.
struct ComputerWaitingNotice: View {
    let agent: String
    let computer: String
    var approvalInT3Code = false
    /// Changes when the answer to `unavailable` can (the connection, the pairing).
    let checkIdentity: String
    let check: () async -> String?
    let open: () -> Void
    @Environment(\.scenePhase) private var phase
    @Environment(\.dynamicTypeSize) private var typeSize
    /// Why the screen can't be shown; nil while checking or when it can.
    @State private var unavailable: String?
    static func text(agent: String, computer: String) -> String { "\(agent) is waiting for you on \(computer)" }
    var body: some View {
        let stacked = approvalInT3Code && typeSize.isAccessibilitySize
        let layout = stacked ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4)) : AnyLayout(HStackLayout(spacing: 8))
        VStack(alignment: .leading, spacing: 2) {
            layout {
                Label(approvalInT3Code ? T3CodePermissions.waiting(computer: computer) : Self.text(agent: agent, computer: computer), systemImage: "desktopcomputer")
                    .font(.caption).foregroundStyle(.secondary)
                    .lineLimit(stacked ? nil : 2)
                    .fixedSize(horizontal: false, vertical: stacked)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button("View computer", action: open)
                    .font(.caption.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: stacked)
                    .buttonStyle(.borderless)
                    .disabled(unavailable != nil)
                    .accessibilityHint(unavailable ?? "Shows the screen of \(computer) to answer there")
                    .accessibilityIdentifier("computer-waiting-view")
            }
            if let unavailable {
                Text(unavailable)
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("computer-waiting-unavailable")
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("computer-waiting")
        // Checks again while shown and Cupola is in front: waking the screen
        // or granting permission on the computer brings the button back.
        .task(id: checkIdentity + ":" + String(phase == .active)) {
            guard phase == .active else { return }
            await Self.watch(check: check, pause: { reason in
                try await Task.sleep(for: reason == nil ? Self.availableRecheck : Self.unavailableRecheck)
            }) { reason in
                if unavailable != reason { unavailable = reason }
            }
        }
    }
    static let unavailableRecheck: Duration = .seconds(5)
    static let availableRecheck: Duration = .seconds(30)
    /// Checks, reports and pauses until cancelled (or `pause` throws): soon
    /// again while the screen can't be shown, rarely once it can.
    static func watch(check: () async -> String?, pause: (String?) async throws -> Void,
                      update: (String?) -> Void) async {
        while !Task.isCancelled {
            let reason = await check()
            guard !Task.isCancelled else { return }
            update(reason)
            do { try await pause(reason) } catch { return }
        }
    }
}

/// The conversation's visible nodes and disclosure, kept for one complete set
/// of inputs, so a re-render that changes none of them (a notice, a pill, a
/// typed character that changed what Send shows) does not group the whole
/// history again. An uncached timeline (the Files view) is never remembered.
final class ConversationFeedPresenter {
    struct Key: Equatable {
        let timeline: ConversationTimeline.Key
        let disclosure: ActivityDisclosurePolicy.State
        let searchFocusRowID: String?
        let chosenDetail: ChatDetailLevel
        let expandedCommandRuns: Set<String>
        let openDetails: Set<String>
        /// Each placed question's entry, ID, state and answer: these change
        /// without a new timeline revision.
        let questions: [String]
    }
    struct Output {
        let expandedActivityIDs: Set<String>
        let nodes: [ChatFeedNode]
    }
    private var cached: (key: Key, output: Output)?
    /// How many times the feed was projected rather than reused.
    private(set) var preparations = 0

    func output(for timeline: ConversationTimeline, disclosure: ActivityDisclosurePolicy.State, searchFocusRowID: String?,
                chosenDetail: ChatDetailLevel, expandedCommandRuns: Set<String>, openDetails: Set<String> = [],
                questions: [String: [AsyncQuestion]] = [:]) -> Output {
        let questionKey = questions.sorted { $0.key < $1.key }.flatMap { entry, placed in
            placed.map { [entry, $0.id, $0.state, $0.response?.answers.joined(separator: "\u{1F}") ?? "", String($0.response?.skip ?? false)]
                .joined(separator: "\u{1E}") }
        }
        let key = timeline.key.map { Key(timeline: $0, disclosure: disclosure, searchFocusRowID: searchFocusRowID,
                                         chosenDetail: chosenDetail, expandedCommandRuns: expandedCommandRuns,
                                         openDetails: openDetails, questions: questionKey) }
        if let key, let cached, cached.key == key { return cached.output }
        preparations += 1
        let reconciled = ActivityDisclosurePolicy.reconciled(disclosure, entries: timeline.disclosureEntries,
                                                             retainedConversationID: timeline.chatID,
                                                             retainedTurnIDs: timeline.retainedTurnIDs)
        let searchReveal: Set<ActivityDisclosurePolicy.Key> = {
            guard let searchFocusRowID,
                  let entry = timeline.entries.first(where: { $0.rows.contains(where: { $0.id == searchFocusRowID }) }),
                  let turnID = entry.rows.first?.turnId else { return [] }
            return [ActivityDisclosurePolicy.Key(conversationID: timeline.chatID, turnID: turnID, entryID: entry.id)]
        }()
        let expanded = ActivityDisclosurePolicy.expandedEntryIDs(entries: timeline.disclosureEntries, state: reconciled,
                                                                 searchReveal: searchReveal)
        // A search result inside hidden work shows the whole conversation, so it can be revealed.
        let detail = searchReveal.isEmpty || chosenDetail == .thinking ? chosenDetail : .everything
        let output = Output(expandedActivityIDs: expanded,
                            nodes: ChatFeedNode.visible(timeline.entries, expanded: expanded.union(expandedCommandRuns), detail: detail,
                                                        openDetails: openDetails, outcomes: timeline.outcomes, questions: questions))
        cached = key.map { (key: $0, output: output) }
        return output
    }
}

/// Send's slot in the composer: Stop while a reply runs and there is nothing
/// to send, Stop beside Send while there is (`ComposerTrailingButton`). It
/// watches dictation, so words being heard bring Send back before they reach
/// the draft.
private struct ComposerTrailingSlot<SendButton: View, StopButton: View>: View {
    @ObservedObject var dictation: DictationController
    let conversationID: String
    let composer: ComposerIntent?
    let preparing: Bool
    let offersStop: Bool
    @ViewBuilder var send: () -> SendButton
    /// Stop, told whether it sits beside Send rather than in its place.
    @ViewBuilder var stop: (_ besideSend: Bool) -> StopButton
    var body: some View {
        switch ComposerTrailingButton.resolve(composer: composer, hearsWords: dictation.hasUncommittedWords(conversationID: conversationID),
                                              preparing: preparing, offersStop: offersStop) {
        case .stop: stop(false)
        case .send: send()
        case .sendAndStop: HStack(spacing: 4) { stop(true); send() }
        }
    }
}

private extension ComposerPillRow {
    /// No pill to show: the row then takes no space (an empty padded strip above the composer).
    var hasPills: Bool { !(leading.isEmpty && scrolling.isEmpty && trailing.isEmpty) }
}

/// The pills above the composer. The middle scrolls when it is wider than the
/// screen; pinned pills (questions and approvals) sit outside it, so a waiting
/// request stays in sight however the row was scrolled. The scrolling part is
/// no wider than its pills, so a trailing pin follows them rather than the edge.
private struct ComposerPillRowView<Pill: View>: View {
    let row: ComposerPillRow
    var selection: ComposerPill?
    @ViewBuilder var pill: (ComposerPill) -> Pill
    @State private var scrollingWidth: CGFloat?
    private struct ReadingTarget: Hashable {
        let selection: ComposerPill?
        let width: CGFloat?
    }

    var body: some View {
        HStack(alignment: .bottom, spacing: 4) {
            ForEach(row.leading) { pill($0) }
            ScrollViewReader { proxy in
                ScrollView(.horizontal) {
                    HStack(alignment: .bottom, spacing: 4) {
                        ForEach(row.scrolling) { pill($0).id($0) }
                    }
                    // The pills' own width; it changes with content, not scrolling.
                    .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { scrollingWidth = $0.rounded(.up) }
                }
                .task(id: ReadingTarget(selection: selection, width: scrollingWidth)) {
                    await Task.yield()
                    guard !Task.isCancelled, let selection else { return }
                    // Reveal only as far as needed; don't recenter the row.
                    proxy.scrollTo(selection)
                }
            }
            .scrollIndicators(.hidden)
            .scrollBounceBehavior(.basedOnSize)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: scrollingWidth)
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("composer-pill-row")
            ForEach(row.trailing) { pill($0) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A usage limit stopped this thread; the computer continues it at `at`
/// unless the owner cancels (or sends something themselves first).
private struct UsageResumeNotice: View {
    @ObservedObject var library: ProjectLibrary
    let conversationID: String
    let at: Date
    /// The composer already says the limit was reached; this then says only when it resumes.
    var limitShown = false
    var body: some View {
        let cancelling = library.cancellingResume.contains(conversationID)
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Label((limitShown ? "" : "Usage limit reached. ") + "\(UsageResume.label(at)).", systemImage: "clock.arrow.circlepath")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("usage-resume")
                Spacer(minLength: 0)
                if cancelling { ProgressView().controlSize(.small) }
                Button("Cancel") { Task { await library.cancelResume(conversationID) } }
                    .font(.caption.weight(.semibold))
                    .disabled(cancelling)
                    .accessibilityLabel("Cancel resume")
                    .accessibilityIdentifier("usage-resume-cancel")
            }
            if let failure = library.resumeFailures[conversationID] {
                Text(failure).font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("usage-resume-error")
            }
        }
    }
}

/// Settings-backed, per device: whether Take over on a thread another app on
/// the computer holds asks first. "Don't show this again" turns it off;
/// Settings turns it back on.
enum TakeOverWarning {
    static let storageKey = "wonder.sendNow.takeOverWarning"
}

/// What a plain tap on Send does while a reply runs (T3 Code's follow-up
/// behavior). Holding Send always offers the other choice.
/// A labelled switch whose accessibility element is the switch itself, so
/// VoiceOver and a touch at its centre both reach the control.
struct SwitchRow: View {
    let title: String
    @Binding var isOn: Bool
    let id: String
    var body: some View {
        HStack {
            Text(title).accessibilityHidden(true)
            Spacer(minLength: 8)
            Toggle(title, isOn: $isOn).labelsHidden().systemSwitch().accessibilityIdentifier(id)
        }
    }
}

/// What Send's hold menu (and its VoiceOver actions) offers while a reply
/// runs, named for what each does: Send now puts the message into the running
/// reply; Take over stops the chat in the app on the computer that holds it
/// and continues it here; T3 Code's restart starts its reply over there, so it
/// never says it continues here.
enum SendHoldAction: Equatable {
    case sendNow, takeOver, restartInT3Code

    /// Send now and Take over share the menu's first place: a chat another
    /// app holds can't be steered from here, only taken over.
    static func offered(takesOver: Bool, restartsT3Code: Bool) -> [SendHoldAction] {
        [takesOver ? .takeOver : .sendNow] + (restartsT3Code ? [.restartInT3Code] : [])
    }

    var title: String {
        switch self {
        case .sendNow: "Send now"
        case .takeOver: "Take over"
        case .restartInT3Code: "Restart reply in T3 Code"
        }
    }

    var systemImage: String {
        switch self {
        case .sendNow: "arrow.turn.up.right"
        case .takeOver: "arrow.uturn.right"
        case .restartInT3Code: "arrow.counterclockwise"
        }
    }

    var accessibilityID: String {
        switch self {
        case .sendNow: "send-now"
        case .takeOver: "send-take-over"
        case .restartInT3Code: "send-restart-t3-code"
        }
    }
}

/// Take over's warning before Wonder stops the app on the computer that holds
/// a thread: what stops, and a way not to be asked again.
struct TakeOverConfirmation: View {
    static let title = "Take over this chat?"
    static func message(app: String, computer: String) -> String {
        "Taking over a chat open in \(app) on \(computer) stops it there and continues it here."
    }
    let app: String
    let computer: String
    let stops: String?
    let send: (_ dontShowAgain: Bool) -> Void
    @State private var dontShowAgain = false
    @Environment(\.dismiss) private var dismiss
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(Self.title).font(.headline).accessibilityAddTraits(.isHeader)
                .accessibilityIdentifier("take-over-confirmation")
            Text(Self.message(app: app, computer: computer))
                .fixedSize(horizontal: false, vertical: true)
            if let stops { Text(stops).fixedSize(horizontal: false, vertical: true) }
            SwitchRow(title: "Don’t show this again", isOn: $dontShowAgain, id: "take-over-dont-show")
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    .accessibilityIdentifier("take-over-cancel")
                Spacer()
                Button(SendHoldAction.takeOver.title) { send(dontShowAgain) }
                    .buttonStyle(.wonderProminent)
                    .accessibilityIdentifier("take-over-send")
            }
        }
        .padding(20)
        .frame(maxWidth: 520, alignment: .leading)
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
    }
}

/// Shows its content only once the condition that mounted it has lasted, so a
/// brief reconnect does not flash a status line.
struct SettledNotice<Content: View>: View {
    let delay: Duration
    @ViewBuilder var content: () -> Content
    @State private var settled = false
    var body: some View {
        Group { if settled { content() } }
            .task {
                try? await Task.sleep(for: delay)
                if !Task.isCancelled { settled = true }
            }
    }
}

/// One quiet retry covers a brief network drop; only a failure that outlasts
/// it asks the reader to act.
private struct HistoryRetryNotice: View {
    let reload: () async -> Void
    @State private var quietRetryFailed = false
    var body: some View {
        Group {
            if quietRetryFailed {
                TemporaryNotice(key: "history") {
                    Button("Couldn’t check for new messages. Try again") { Task { await reload() } }
                        .font(.caption).frame(minHeight: 44)
                        .accessibilityIdentifier("native-history-retry")
                }
            }
        }
        .task {
            try? await Task.sleep(for: .seconds(4))
            guard !Task.isCancelled else { return }
            await reload()
            // Still mounted means the failure persisted.
            if !Task.isCancelled { quietRetryFailed = true }
        }
    }
}

/// A composer notice that fades after a short read. A new message (a different
/// key) shows again; VoiceOver users get longer to reach it.
struct TemporaryNotice<Content: View>: View {
    let key: String
    @ViewBuilder var content: () -> Content
    @State private var dismissedKey: String?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        if dismissedKey != key {
            content()
                .transition(.opacity)
                .task(id: key) {
                    let seconds = UIAccessibility.isVoiceOverRunning ? 30.0 : 10.0
                    try? await Task.sleep(for: .seconds(seconds))
                    guard !Task.isCancelled else { return }
                    withAnimation(reduceMotion ? nil : .easeOut(duration: 0.3)) { dismissedKey = key }
                }
        }
    }
}

/// A short confirmation near the top of the chat. It announces itself to
/// VoiceOver and fades after a few seconds.
private struct ChatNotice: View {
    let text: String
    let dismiss: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        Text(text)
            .font(.footnote.weight(.medium))
            .padding(.horizontal, 14).padding(.vertical, 8)
            .background(.regularMaterial, in: Capsule())
            .padding(.top, 8)
            .transition(reduceMotion ? .opacity : .move(edge: .top).combined(with: .opacity))
            .accessibilityIdentifier("chat-notice")
            .task {
                UIAccessibility.post(notification: .announcement, argument: text)
                #if WONDER_DIAGNOSTICS
                try? await DiagnosticSubagentFixture.holdForkNotice()
                guard !Task.isCancelled else { return }
                #endif
                try? await Task.sleep(for: .seconds(UIAccessibility.isVoiceOverRunning ? 6 : 2.5))
                guard !Task.isCancelled else { return }
                withAnimation(reduceMotion ? nil : .easeOut(duration: 0.25)) { dismiss() }
            }
    }
}

/// The receipt of the latest row while its end is inside the viewport. The row
/// reports visibility rather than its frame, so scrolling changes the
/// scroller's state only when visibility flips, not on every frame.
private struct LatestReadVisibilityKey: PreferenceKey {
    static var defaultValue: VisibleReadReceipt? { nil }
    static func reduce(value: inout VisibleReadReceipt?, nextValue: () -> VisibleReadReceipt?) { if let next = nextValue() { value = next } }
}
private struct ConversationReadViewportKey: EnvironmentKey {
    static let defaultValue: CGRect = .zero
}
private struct ConversationInitialTargetKey: EnvironmentKey {
    static let defaultValue: String? = nil
}
private struct ConversationPositionReadyKey: EnvironmentKey {
    static let defaultValue = true
}
extension EnvironmentValues {
    fileprivate var conversationReadViewport: CGRect {
        get { self[ConversationReadViewportKey.self] }
        set { self[ConversationReadViewportKey.self] = newValue }
    }
    fileprivate var conversationInitialTarget: String? {
        get { self[ConversationInitialTargetKey.self] }
        set { self[ConversationInitialTargetKey.self] = newValue }
    }
    fileprivate var conversationPositionReady: Bool {
        get { self[ConversationPositionReadyKey.self] }
        set { self[ConversationPositionReadyKey.self] = newValue }
    }
    /// How many computers this phone has paired. A conversation names its
    /// computer under the title only when there is more than one.
    @Entry var pairedComputerCount = 1
}
private struct InitialConversationTargetFrame: Equatable {
    let id: String
    let frame: CGRect
}
private struct InitialConversationTargetFrameKey: PreferenceKey {
    static var defaultValue: InitialConversationTargetFrame? { nil }
    static func reduce(value: inout InitialConversationTargetFrame?, nextValue: () -> InitialConversationTargetFrame?) {
        if let next = nextValue() { value = next }
    }
}
private struct VisibleConversationNodesKey: PreferenceKey {
    static var defaultValue: Set<String> { [] }
    static func reduce(value: inout Set<String>, nextValue: () -> Set<String>) { value.formUnion(nextValue()) }
}
private struct ConversationRowMarker: View {
    let id: String
    let receipt: VisibleReadReceipt?
    @Environment(\.conversationReadViewport) private var viewport
    @Environment(\.conversationInitialTarget) private var initialTarget
    var body: some View {
        GeometryReader { geometry in
            Color.clear.preference(key: LatestReadVisibilityKey.self,
                value: ReadVisibility.latestEndIsVisible(frame: geometry.frame(in: .global), viewport: viewport) ? receipt : nil)
                .preference(key: VisibleConversationNodesKey.self,
                    value: geometry.frame(in: .global).intersects(viewport) ? [id] : [])
                .preference(key: InitialConversationTargetFrameKey.self,
                    value: initialTarget == id ? InitialConversationTargetFrame(id: id, frame: geometry.frame(in: .global)) : nil)
        }
    }
}
private struct BottomFrameKey: PreferenceKey {
    static var defaultValue: CGRect? { nil }
    static func reduce(value: inout CGRect?, nextValue: () -> CGRect?) { if let next = nextValue() { value = next } }
}
private struct ReadViewportKey: PreferenceKey {
    static var defaultValue: CGRect { .zero }
    static func reduce(value: inout CGRect, nextValue: () -> CGRect) {
        let next = nextValue()
        // Descendants without a measurement contribute the default value.
        // They must not erase the scroll view's actual visible bounds.
        if !next.isEmpty { value = next }
    }
}

private struct PhotoAttachmentPicker: ViewModifier {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    @Binding var isPresented: Bool
    let scope: String?
    @State private var selection: [PhotosPickerItem] = []

    func body(content: Content) -> some View {
        content
            // Several photos at once, up to the free attachment slots, in the order picked.
            .photosPicker(isPresented: $isPresented, selection: $selection,
                          maxSelectionCount: max(1, 4 - (model.composers[chat.id]?.attachmentCount ?? 0)),
                          selectionBehavior: .ordered, matching: .any(of: [.images, .videos]), preferredItemEncoding: .current)
            .task(id: selection) {
                guard !selection.isEmpty else { return }
                guard scope == model.assignmentScope else { selection = []; return }
                await model.stagePhotos(selection, chat: chat)
                selection = []
            }
    }
}

/// The list mounts this row when the reader approaches the beginning.
/// Each cursor loads once; failures expose recovery without a permanent paging button.
private struct OlderMessagesLoader: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let cursor: String
    @State private var failure: String?
    @State private var retry = 0
    @Environment(\.conversationPositionReady) private var positionReady
    var body: some View {
        Group {
            if let failure {
                Button { retry += 1 } label: {
                    Label(failure, systemImage: "arrow.clockwise")
                        .font(.footnote).frame(minHeight: 44)
                }.accessibilityLabel("Retry loading earlier messages")
            } else {
                ProgressView().accessibilityLabel("Loading earlier messages")
            }
        }
        .frame(maxWidth: .infinity).frame(minHeight: 28)
        .task(id: positionReady ? cursor + "-" + String(retry) : nil) {
            guard positionReady else { return }
            failure = nil
            while model.loadingConversationIDs.contains(chat.id) {
                do { try await Task.sleep(for: .milliseconds(200)) } catch { return }
            }
            guard !Task.isCancelled else { return }
            failure = await model.loadOlder(chat)
        }
    }
}

private struct SketchRequest: Identifiable {
    let id = UUID()
    let scope: String
}

/// Scroll position and geometry change frequently. Keep them below the view that
/// prepares the full message timeline and composer.
private struct ConversationScrollRequest: Equatable {
    let id: String
    var anchor: UnitPoint? = nil
    var animated = false
}

private struct ConversationScrollPosition: ViewModifier {
    @Binding var request: ConversationScrollRequest?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        ScrollViewReader { proxy in
            content
                .onChange(of: request, initial: true) { _, value in
                    guard let value else { return }
                    let animate = value.animated && !reduceMotion
                    var transaction = Transaction(animation: animate ? .easeOut(duration: 0.2) : nil)
                    transaction.disablesAnimations = !animate
                    withTransaction(transaction) {
                        proxy.scrollTo(value.id, anchor: value.id == "conversation-bottom" ? .bottom : value.anchor)
                    }
                    request = nil
                }
        }
    }
}

// SwiftUI appearance runs during the navigation transition. Restore cached
// reading intent after UIKit has finished presenting the final scroll viewport.
private struct ConversationPresentationReady: UIViewControllerRepresentable {
    let onReady: () -> Void
    func makeUIViewController(context: Context) -> Controller { Controller(onReady: onReady) }
    func updateUIViewController(_ controller: Controller, context: Context) { controller.onReady = onReady }
    final class Controller: UIViewController {
        var onReady: () -> Void
        init(onReady: @escaping () -> Void) { self.onReady = onReady; super.init(nibName: nil, bundle: nil) }
        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
        override func loadView() { view = UIView(); view.isUserInteractionEnabled = false }
        override func viewDidAppear(_ animated: Bool) {
            super.viewDidAppear(animated)
            if let coordinator = transitionCoordinator, coordinator.isAnimated,
               coordinator.animate(alongsideTransition: nil, completion: { [weak self] context in
                   guard !context.isCancelled else { return }
                   self?.onReady()
               }) { return }
            onReady()
        }
    }
}

private enum ConversationMinimapStyle {
    // Equal turn spacing keeps the index legible regardless of reply length.
    static let spacing: ConversationMinimapProjection.Spacing = .compact
}

/// The turn bands depend only on content and rail size, never scroll position.
/// Equatable prevents rebuilding the Canvas when only its cursor moves.
private struct ConversationMinimapRail: View, @MainActor Equatable {
    @Environment(\.wonderTheme) private var theme
    let projection: ConversationMinimapProjection
    let spacing: ConversationMinimapProjection.Spacing
    let height: CGFloat
    let width: CGFloat

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.spacing == rhs.spacing && lhs.projection.drawsSameRail(as: rhs.projection, spacing: lhs.spacing) && lhs.height == rhs.height && lhs.width == rhs.width
    }

    var body: some View {
        #if WONDER_DIAGNOSTICS
        let started = ProcessInfo.processInfo.systemUptime
        #endif
        let bands = projection.layout(spacing: spacing).drawingBands(height: height)
        var strokes = Path()
        for band in bands {
            strokes.addRoundedRect(in: CGRect(x: width - 12, y: band.start, width: 8, height: band.end - band.start), cornerSize: CGSize(width: 1, height: 1))
        }
        #if WONDER_DIAGNOSTICS
        let _ = DiagnosticJournal.shared.record(DiagnosticEvent(operation: "minimap.paths", durationMs: (ProcessInfo.processInfo.systemUptime - started) * 1000, count: UInt64(bands.count)))
        #endif
        let path = strokes
        return Canvas { context, _ in context.fill(path, with: .color(theme.text.opacity(0.55))) }
            .accessibilityHidden(true)
    }
}

/// While scrubbing: the selected message of yours and the start of its reply.
private struct ConversationMinimapCallout: View {
    @Environment(\.wonderTheme) private var theme
    let mark: ConversationMinimapProjection.Mark
    let index: Int
    let count: Int
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Your message \(index + 1) of \(count)").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
            Text(ChatPlainText.of(mark.prompt).isEmpty ? "Attachment" : ChatPlainText.of(mark.prompt))
                .font(.footnote.weight(.semibold)).lineLimit(2)
            if let reply = mark.reply.map(ChatPlainText.of), !reply.isEmpty {
                Text(reply).font(.footnote).foregroundStyle(.secondary).lineLimit(2)
            }
        }
        .foregroundStyle(theme.text)
        .padding(10)
        .frame(width: 240, alignment: .leading)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: 10))
        .shadow(color: .black.opacity(0.15), radius: 6, y: 2)
    }
}

/// Only this small overlay observes scrubbing/idle state. The list remains the
/// sole virtualization owner and publishes visible IDs rather than scroll pixels.
/// One stroke per turn: a tap jumps to that turn's message, a drag scrubs
/// through turns with the selected one named beside the rail.
private struct ConversationMinimap: View {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver
    @Environment(\.scenePhase) private var phase
    /// Marks and cursor stay inside the row's 16 pt trailing margin, so no
    /// message text sits under the rail (52 pt before; the marks overlapped bubble ends).
    static let width: CGFloat = 28
    /// Touch target: 44 pt wide, drawn at `width` against the trailing edge.
    static let hitWidth: CGFloat = 44
    let projection: ConversationMinimapProjection
    let position: String?
    let atBottom: Bool
    let scrolling: Bool
    let height: CGFloat
    let searchTarget: String?
    let onSelect: (ConversationMinimapProjection.Mark) -> Void
    @GestureState private var holding = false
    @State private var heldProjection: ConversationMinimapProjection?
    @State private var heldHeight: CGFloat?
    @State private var selection: Int?
    @State private var adjustedMarkID: String?
    @State private var readySelection: Int?
    @State private var lastJumpedID: String?
    @State private var lingering: Bool = {
        #if WONDER_DIAGNOSTICS
        return ProcessInfo.processInfo.arguments.contains("-diagnostics-minimap-visible")
        #else
        return false
        #endif
    }()

    private var visible: Bool { phase == .active && (scrolling || holding || lingering || voiceOver) }
    private var displayedHeight: CGFloat { heldHeight ?? height }
    private var displayedProjection: ConversationMinimapProjection { heldProjection ?? projection }
    private var marks: [ConversationMinimapProjection.Mark] { displayedProjection.marks }
    private var layout: ConversationMinimapProjection.Layout { displayedProjection.layout(spacing: ConversationMinimapStyle.spacing) }
    private var currentIndex: Int? { heldProjection == nil ? projection.index(for: position, selectedMarkID: adjustedMarkID, atBottom: atBottom) : selection }
    private func y(_ index: Int) -> CGFloat { layout.band(at: index, height: displayedHeight)?.center ?? 8 }

    private func select(at y: CGFloat) {
        begin()
        let index = layout.index(atY: y, height: displayedHeight)
        if selection != index { selection = index }
    }

    private func begin() {
        guard heldProjection == nil else { return }
        // Paging/streaming can change the projection during a drag. Freeze only
        // the tiny navigation projection, including its geometry. Resolve the
        // selected ID against current history before scrolling.
        adjustedMarkID = nil
        heldProjection = projection
        heldHeight = height
        readySelection = nil
        lastJumpedID = nil
        lingering = true
    }

    private func jumpSelection() {
        guard phase == .active, let selection, let heldProjection, heldProjection.marks.indices.contains(selection) else { return }
        let mark = heldProjection.marks[selection]
        guard mark.id != lastJumpedID else { return }
        lastJumpedID = mark.id
        onSelect(mark)
    }

    private func finish() {
        guard heldProjection != nil else { return }
        jumpSelection()
        heldProjection = nil; heldHeight = nil
        selection = nil
        readySelection = nil
        lastJumpedID = nil
        lingering = true
    }

    private func adjust(_ index: Int) {
        guard projection.marks.indices.contains(index) else { return }
        adjustedMarkID = projection.marks[index].id
        onSelect(projection.marks[index])
    }

    private var control: some View {
        ConversationMinimapRail(projection: displayedProjection, spacing: ConversationMinimapStyle.spacing, height: displayedHeight, width: Self.width)
            .equatable()
            .overlay(alignment: .topLeading) {
                if let index = currentIndex {
                    RoundedRectangle(cornerRadius: 2)
                        .fill(holding ? Color.accentColor : theme.text).frame(width: 12, height: 4)
                        .offset(x: Self.width - 16, y: y(index) - 2)
                        .allowsHitTesting(false).accessibilityHidden(true)
                }
            }
            .frame(width: Self.width, height: displayedHeight)
            .frame(width: Self.hitWidth, height: displayedHeight, alignment: .trailing)
            .contentShape(Rectangle())
            // A touch selects the turn under it at once: a tap jumps there and
            // a drag scrubs. The rail takes touches only while it is shown.
            .gesture(DragGesture(minimumDistance: 0)
                .updating($holding) { _, state, _ in state = true }
                .onChanged { drag in
                    select(at: drag.location.y)
                }
                .onEnded { _ in finish() })
        .overlay(alignment: .topTrailing) {
            if holding, let index = currentIndex, marks.indices.contains(index) {
                ConversationMinimapCallout(mark: marks[index], index: index, count: marks.count)
                    .fixedSize(horizontal: false, vertical: true)
                    .offset(x: -(Self.hitWidth + 4), y: max(0, min(displayedHeight - 40, y(index) - 24)))
                    .allowsHitTesting(false).accessibilityHidden(true)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityIdentifier("conversation-minimap")
        .accessibilityLabel("Chat mini-map")
        .accessibilityValue(currentIndex.flatMap { marks.indices.contains($0) ? $0 : nil }
            .map { "Your message \($0 + 1) of \(marks.count): " + ChatPlainText.of(marks[$0].prompt) } ?? "No messages")
        .accessibilityHint("Swipe up or down to move between your messages.")
        .accessibilityAdjustableAction { direction in
            guard let currentIndex else { return }
            switch direction {
            case .increment: adjust(min(projection.marks.count - 1, currentIndex + 1))
            case .decrement: adjust(max(0, currentIndex - 1))
            @unknown default: break
            }
        }
        .accessibilityAction(named: "Your first message") { adjust(0) }
        .accessibilityAction(named: "Your latest message") { adjust(projection.marks.count - 1) }
    }

    var body: some View {
        // Keep scroll/fade state alive, but remove the adjustable control itself
        // while hidden. Opacity and hit testing alone leave a focusable AXSlider.
        ZStack {
            if visible { control.transition(.opacity) }
        }
        #if WONDER_DIAGNOSTICS
        .background {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-minimap-comparison") {
                DiagnosticDriverSurface(surface: .init(priority: 11, active: { visible }, state: {
                    ["minimap": ["spacing": ConversationMinimapStyle.spacing == .content ? "content" : "compact",
                                 "height": Double(displayedHeight), "extent": Double(layout.drawnExtent(height: displayedHeight)),
                                 "boundaries": layout.boundaries.map { Double($0) },
                                 "drawnBands": layout.drawingBands(height: displayedHeight).count]]
                }, controls: {
                    [.init(id: "fixture-minimap-tap-tenth", label: "Tap one tenth of the mini-map", tap: {
                        select(at: 8 + layout.drawnExtent(height: displayedHeight) * 0.1)
                        finish()
                    })]
                })).frame(width: 0, height: 0).accessibilityHidden(true)
            }
        }
        #endif
        .animation(reduceMotion ? nil : .easeOut(duration: 0.18), value: visible)
        .allowsHitTesting(visible)
        .accessibilityHidden(!visible)
        .onChange(of: scrolling) { _, active in
            if active { adjustedMarkID = nil; lingering = true }
        }
        .onChange(of: atBottom) { _, latest in if latest { adjustedMarkID = nil } }
        .onChange(of: searchTarget) { _, _ in adjustedMarkID = nil }
        .onChange(of: holding) { _, active in if !active { finish() } }
        .onChange(of: readySelection) { _, _ in
            // This closure is refreshed when a page changes the projection;
            // the long-lived sampling task must not retain an old scroll action.
            if holding { jumpSelection() }
        }
        .task(id: holding && phase == .active) {
            guard holding, phase == .active else { return }
            // Coalesce scrub requests, not history or list geometry. The final
            // target is flushed on release even between these bounded ticks.
            while !Task.isCancelled {
                if readySelection != selection { readySelection = selection }
                do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
            }
        }
        .task(id: scrolling || holding || phase != .active) {
            guard !scrolling, !holding, phase == .active else { return }
            do { try await Task.sleep(for: .milliseconds(1300)) } catch { return }
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-minimap-held") {
                while !DiagnosticSubagentFixture.signalled("minimap-idle") {
                    do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
                }
            }
            #endif
            lingering = false
        }
        .onChange(of: phase) { _, next in
            if next != .active { heldProjection = nil; heldHeight = nil; selection = nil; adjustedMarkID = nil; readySelection = nil; lingering = false }
        }
        .onDisappear { heldProjection = nil; heldHeight = nil; selection = nil; adjustedMarkID = nil; readySelection = nil; lingering = false }
    }
}

#if WONDER_DIAGNOSTICS
private struct ConversationRestoreFixtureRow: ViewModifier {
    private static let requested = ProcessInfo.processInfo.arguments.contains("-diagnostics-loading-blank")
    let id: String
    func body(content: Content) -> some View {
        // The captured Roblox anchor measured 502.33 pt in a 504 pt viewport,
        // starting 6 pt below its top. Reproduce that boundary without private text.
        content.frame(minHeight: Self.requested && id == "layout-turn-6/layout-reply-6" ? 502 : nil, alignment: .top)
    }
}
#endif

private struct ConversationScroller<Content: View>: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ConnectionModel
    @ObservedObject var projects: ProjectLibrary
    let chat: ChatSummary
    let entries: [ChatFeedEntry]
    let minimap: ConversationMinimapProjection
    let nodeIDs: [String]
    /// Outcome and question node IDs, by the entry each follows.
    let nodeOwners: [String: String]
    let isCovered: Bool
    /// The composer's middle (window x), so the scroll-to-bottom arrow shares its centre line.
    var composerMidX: CGFloat?
    let attachmentLayoutReady: Bool
    /// Room left at the top for a title pill floating over the timeline.
    var headerClearance: CGFloat = 0
    @Binding var requestedScrollID: String?
    @ViewBuilder let content: Content
    @State private var position: String?
    @State private var scrollRequest: ConversationScrollRequest?
    @State private var restored = false
    @State private var initialTarget: String?
    @State private var presented = false
    @State private var visibleLatestReceipt: VisibleReadReceipt?
    @State private var readViewport: CGRect = .zero
    @State private var bottomVisible = false
    /// The reader was at the end and has not scrolled away since.
    @State private var followsBottom = false
    @State private var lastNodeID: String?
    @State private var userScrolling = false
    /// Lazy rows can settle taller than estimated just after Wonder scrolls to
    /// the end; correct only within that short window, never after user actions.
    @State private var bottomSettleUntil = Date.distantPast
    @State private var bottomCorrections = 0
    @Environment(\.scenePhase) private var phase
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.horizontalSizeClass) private var sizeClass
    private let bottomID = "conversation-bottom"

    private var showsScrollToBottom: Bool { restored && !bottomVisible && !entries.isEmpty && readViewport.height >= 240 }
    private var readingPosition: String? { restored ? (bottomVisible ? bottomID : position) : nil }
    private var waitingForPosition: Bool { !restored && !nodeIDs.isEmpty }

    private var visibleReadReceipt: VisibleReadReceipt? {
        guard restored, phase == .active, !model.previewMode, !model.accessEnded,
              !isCovered,
              model.macConnected == true, !model.cachedConversationIds.contains(chat.id),
              model.hasUnread(chat.id),
              !projects.manuallyUnread.contains(chat.id),
              let receipt = visibleLatestReceipt,
              receipt == model.readReceipt(for: chat.id) else { return nil }
        return receipt
    }

    private func pinBottom(animated: Bool = false) {
        bottomSettleUntil = Date().addingTimeInterval(3)
        bottomCorrections = 0
        scrollRequest = ConversationScrollRequest(id: bottomID, animated: animated)
    }

    private func scrollToMinimapMark(_ selected: ConversationMinimapProjection.Mark) {
        // Re-resolve after a page prepends/merges a group. Use an existing node;
        // a removed message cannot leave a pending request blocking restoration.
        guard let target = minimap.scrollTarget(for: selected.id, visibleNodeIDs: Set(nodeIDs)) else { return }
        followsBottom = false
        bottomSettleUntil = .distantPast
        scrollRequest = ConversationScrollRequest(id: target, anchor: .top)
    }

    /// Only beside a wide conversation (iPad, an unfolded Duo, a large iPhone on
    /// its side). On a narrow screen its marks crowded bubbles and disclosure
    /// chevrons at the trailing edge, so the owner chose to hide it there.
    private var minimapEnabled: Bool {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-minimap-disabled") { return false }
        #endif
        return sizeClass != .compact
    }

    private func restorePositionIfNeeded() {
        guard presented, !nodeIDs.isEmpty, restored || attachmentLayoutReady else { return }
        defer { lastNodeID = nodeIDs.last }
        if restored {
            // A new final row (a sent message or reply) keeps a reader who was
            // at the end there. Rows inserted above it, such as expanded
            // activity, keep the visible anchor instead.
            if followsBottom, !userScrolling, let last = nodeIDs.last, last != lastNodeID {
                pinBottom()
                return
            }
            if let anchor = ChatFeedNode.survivingAnchor(for: position, entries: entries, nodeIDs: nodeIDs, nodeOwners: nodeOwners), anchor != position {
                scrollRequest = ConversationScrollRequest(id: anchor)
            }
            return
        }
        let focus = model.searchFocus.flatMap { $0.conversationID == chat.id ? $0.scrollID : nil }
        let saved = model.position(for: chat.id)
        // Restoration waits for this target's frame, so it must be a node that
        // renders at this detail level (a Working header is hidden at
        // Conversation); the resolver returns only visible nodes, else the end.
        var target = ChatFeedNode.survivingAnchor(for: focus, entries: entries, nodeIDs: nodeIDs, nodeOwners: nodeOwners)
            ?? ChatFeedNode.survivingAnchor(for: saved, entries: entries, nodeIDs: nodeIDs, nodeOwners: nodeOwners)
        #if WONDER_DIAGNOSTICS
        // Duo input is gated (#387), so scroll-dependent captures open at the top instead of swiping.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-scroll-top"), let first = nodeIDs.first { target = first }
        #endif
        initialTarget = target ?? bottomID
        if let target { scrollRequest = ConversationScrollRequest(id: target, anchor: .top) }
        else { pinBottom() }
        if initialTarget == bottomID && bottomVisible { restored = true; followsBottom = true }
    }

    var body: some View {
        // A message sent while the Mac is unreachable waits below the
        // conversation; a reader at the end keeps seeing it.
        scroller.onChange(of: model.composers[chat.id]?.outbox?.count ?? 0) { old, new in
            if new > old, restored, followsBottom, !userScrolling { pinBottom(animated: true) }
        }
        // The person's own message always comes into view when it is sent (Send
        // now from a sheet included), wherever they were reading.
        .onChange(of: model.composers[chat.id]?.pending?.request.clientMessageId) { _, sent in
            guard sent != nil, restored else { return }
            followsBottom = true
            pinBottom(animated: true)
        }
    }

    private var scroller: some View {
        Group {
            List { // theme-exempt: the conversation timeline over ConversationView's ThemeBackdrop
                content
                    .listRowInsets(EdgeInsets(top: 6, leading: 16, bottom: 6, trailing: 16))
                    .listRowSeparator(.hidden)
                    .listRowBackground(Color.clear)
            }
            .accessibilityIdentifier("conversation-scroll")
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            // Override the app's hard edge for the floating conversation title.
            // The system fades only the content that scrolls beneath its bar.
            .scrollEdgeEffectStyle(.soft, for: .top)
            // The mini map shows where the reader is; the system bar would sit on top of it.
            .scrollIndicators(.hidden)
            .environment(\.defaultMinListRowHeight, 0)
            .buttonStyle(.borderless)
            .contentMargins(.horizontal, 0, for: .scrollContent)
            .contentMargins(.top, 16 + headerClearance, for: .scrollContent)
            .contentMargins(.bottom, 0, for: .scrollContent)
            .modifier(ConversationScrollPosition(request: $scrollRequest))
            // Fade transcript edges before adding controls, so the bottom arrow
            // and minimap retain their full shape and touch surface. Applied inside
            // the arrow's strip padding below, so the bottom fade is at the list's own edge.
            .modifier(TimelineScrollEdgeFade(bottomFade: showsScrollToBottom ? 32 : 14))
            // The timeline runs under the scroll-to-bottom arrow with only the longer
            // bottom fade; no layout strip, so the list never changes size as the arrow appears.
            .animation(reduceMotion ? nil : .easeOut(duration: 0.15), value: showsScrollToBottom)
            .background { ConversationPresentationReady { presented = true }.frame(width: 0, height: 0).accessibilityHidden(true) }
            // Keyboard/rotation changes resize only the viewport; the latest
            // row re-evaluates its visibility against the new bounds.
            .environment(\.conversationReadViewport, readViewport)
            .environment(\.conversationInitialTarget, restored ? nil : initialTarget)
            .environment(\.conversationPositionReady, !waitingForPosition)
            .background { GeometryReader { geometry in Color.clear.preference(key: ReadViewportKey.self, value: geometry.frame(in: .global)) } }
            .onPreferenceChange(LatestReadVisibilityKey.self) { receipt in
                if visibleLatestReceipt != receipt { visibleLatestReceipt = receipt }
            }
            .onPreferenceChange(VisibleConversationNodesKey.self) { visibleIDs in
                if let first = nodeIDs.first(where: visibleIDs.contains), first != position { position = first }
                #if WONDER_DIAGNOSTICS
                if DiagnosticTestDriver.requested { DiagnosticTestDriver.shared.visibleTimeline[chat.id] = visibleIDs }
                #endif
            }
            .onPreferenceChange(ReadViewportKey.self) { viewport in
                guard readViewport != viewport else { return }
                // A composer notice or the keyboard can shrink the viewport after
                // the end is shown; keep the reader at the end instead of
                // leaving the latest reply below the fold.
                let keepBottom = restored && followsBottom && !userScrolling && viewport.maxY < readViewport.maxY - 1
                readViewport = viewport
                if keepBottom { pinBottom() }
            }
            // The viewport changes in steps while the keyboard and a companion
            // pane animate, and the rows reflow after it: a pin made mid-resize
            // lands short and the end's marker, now unloaded, reports nothing
            // more to correct it (measured on the unfolded Duo with the
            // terminal's keyboard: the last reply stayed cut off under the
            // arrow). Once the size has held still, finish the job. Keyed on the
            // viewport's frame, which scrolling does not change.
            .task(id: readViewport) {
                do { try await Task.sleep(for: .milliseconds(400)) } catch { return }
                if !restored {
                    // The model/effort choices can finish loading and resize
                    // the composer before the initial target is visible.
                    // Retry against the settled viewport, not each scroll.
                    restorePositionIfNeeded()
                } else if followsBottom, !userScrolling, !bottomVisible { pinBottom() }
            }
            // A row that settles taller after the pin window (a highlighted code
            // block in a slower theme) moves the end out of view while the reader
            // still follows it: once the end has been out of view and still for a
            // moment, pin it again, as the viewport task above does for resizes.
            // A lazy list can land short of the end more than once while rows settle.
            .task(id: [restored, followsBottom, bottomVisible]) {
                guard restored, followsBottom, !bottomVisible else { return }
                for _ in 0..<8 {
                    do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
                    guard followsBottom, !userScrolling, !bottomVisible else { return }
                    pinBottom()
                }
            }
            .onPreferenceChange(InitialConversationTargetFrameKey.self) { target in
                guard !restored, attachmentLayoutReady, let target, target.id == initialTarget, !readViewport.isEmpty else { return }
                let frame = target.frame
                let fits = frame.minY >= readViewport.minY - 1 && frame.maxY <= readViewport.maxY + 1
                // The row's top inset consumes viewport space too. A row can
                // overflow from its aligned start while shorter than the viewport.
                let availableHeight = readViewport.maxY - max(readViewport.minY, frame.minY)
                let tallStartVisible = frame.height > availableHeight && abs(frame.minY - readViewport.minY) <= 24
                if fits || tallStartVisible { restored = true }
            }
            .onPreferenceChange(BottomFrameKey.self) { frame in
                let visible = frame.map { $0.maxY <= readViewport.maxY + 24 && $0.maxY >= readViewport.minY } ?? false
                if bottomVisible != visible { bottomVisible = visible }
                if !restored, attachmentLayoutReady, initialTarget == bottomID, visible { restored = true; followsBottom = true }
                if restored, visible { followsBottom = true }
                else if restored, followsBottom, !userScrolling, bottomCorrections < 6, Date() < bottomSettleUntil {
                    bottomCorrections += 1
                    scrollRequest = ConversationScrollRequest(id: bottomID)
                }
            }
            // A scroll request is not layout completion. Keep the real list
            // mounted but covered until its intended row/end is in the viewport.
            .opacity(waitingForPosition ? 0 : 1)
            .accessibilityHidden(waitingForPosition)
            .allowsHitTesting(!waitingForPosition)
            .overlay {
                if waitingForPosition {
                    ProgressView("Loading chat…")
                        .accessibilityIdentifier("conversation-loading")
                }
            }
            .scrollDismissesKeyboard(.interactively)
            // Observe single taps without taking links or message menus away
            // from their own gestures. Only the composer can resign here.
            .simultaneousGesture(TapGesture().onEnded { ComposerKeyboard.dismiss() })
            // Only the reader's own scrolling decides whether to stay at the end.
            .onScrollPhaseChange { _, next in
                let wasUserScrolling = userScrolling
                userScrolling = next == .tracking || next == .interacting || next == .decelerating
                if next == .idle, wasUserScrolling, restored { followsBottom = bottomVisible }
            }
            // The mini map floats over the trailing edge rather than taking chat
            // width: its marks sit at the edge, mostly in the row margin, so
            // appearing and fading never lay out bubbles again. It takes
            // touches only while shown.
            .overlay(alignment: .trailing) {
                if restored, minimap.marks.count > 1, !isCovered, minimapEnabled {
                    let height = minimap.layout(spacing: ConversationMinimapStyle.spacing).railHeight(viewportHeight: readViewport.height)
                    ConversationMinimap(projection: minimap, position: position.map { nodeOwners[$0] ?? $0 }, atBottom: bottomVisible,
                                        scrolling: userScrolling, height: height,
                                        searchTarget: model.searchFocus?.scrollID, onSelect: scrollToMinimapMark)
                        .frame(width: ConversationMinimap.hitWidth)
                }
            }
            // A floating icon centred under the timeline, with no fill or shadow. On a Duo whose hinge runs
            // down the page it centres on the larger display, never on the hinge. Not shown when so little
            // timeline is left that the strip would take too much of it (a tall draft with the keyboard up).
            .overlay(alignment: .bottom) {
                if showsScrollToBottom {
                    Button {
                        #if WONDER_DIAGNOSTICS
                        Diagnostics.shared.interaction("scroll.bottom")
                        #endif
                        followsBottom = true
                        pinBottom(animated: true)
                    } label: {
                        Image(systemName: "arrow.down")
                            .font(.system(size: 20, weight: .semibold))
                            .frame(width: 44, height: 44)
                            .foregroundStyle(.primary)
                            .contentShape(Circle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Scroll to bottom")
                    .accessibilityIdentifier("scroll-to-bottom")
                    .modifier(ConversationHeaderFoldPlacement(composerMidX: composerMidX))
                    // Keep the whole touch target inside the viewport.
                    .padding(.bottom, 8)
                }
            }
            .task(id: presented ? nodeIDs : []) {
                restorePositionIfNeeded()
            }
            #if WONDER_DIAGNOSTICS
            .onChange(of: restored) { _, ready in
                if ready { DiagnosticPerformanceRun.threadVisible(chat.id); LatencyHops.mark("open.visible", chat.id) }
            }
            #endif
            .onChange(of: model.snapshots[chat.id]?.lastSequence) { _, _ in
                // Streaming grows a stable row; its ID does not change when
                // the next lines arrive. Keep following until the reader leaves.
                if restored, followsBottom, !userScrolling { pinBottom() }
            }
            .onChange(of: attachmentLayoutReady) { _, ready in
                if ready { restorePositionIfNeeded() }
            }
            .onChange(of: model.searchFocus?.id) { _, _ in
                if let focus = model.searchFocus, focus.conversationID == chat.id {
                    followsBottom = false
                    scrollRequest = ConversationScrollRequest(id: focus.scrollID)
                }
            }
            .onChange(of: requestedScrollID) { _, target in
                guard let target else { return }
                followsBottom = false
                withAnimation(reduceMotion ? nil : .easeOut(duration: 0.2)) { scrollRequest = ConversationScrollRequest(id: target, anchor: .top, animated: true) }
                requestedScrollID = nil
            }
            .onAppear {
                if let target = requestedScrollID {
                    followsBottom = false
                    scrollRequest = ConversationScrollRequest(id: target, anchor: .top, animated: true)
                    requestedScrollID = nil
                }
            }
            .onDisappear {
                model.savePosition(readingPosition, chat: chat.id)
                #if WONDER_DIAGNOSTICS
                DiagnosticTestDriver.shared.visibleTimeline[chat.id] = nil
                #endif
            }
            .task(id: readingPosition) {
                do { try await Task.sleep(for: .milliseconds(300)) } catch { return }
                model.savePosition(readingPosition, chat: chat.id)
            }
            .onChange(of: phase) { _, next in
                if next != .active { model.savePosition(readingPosition, chat: chat.id) }
            }
            .task(id: visibleReadReceipt) {
                guard let receipt = visibleReadReceipt else { return }
                // A transient failure or concurrent list refresh must not leave
                // this stationary, visible message unread until the next scroll.
                // The receipt clears once the host confirms, ending the loop.
                var delay = 750
                while !Task.isCancelled {
                    do { try await Task.sleep(for: .milliseconds(delay)) } catch { return }
                    guard visibleReadReceipt == receipt, !Task.isCancelled else { return }
                    await model.acknowledgeVisibleRead(receipt)
                    delay = min(delay * 2, 30_000)
                }
            }
        }
    }
}

/// Custom preview bars share the scene with iPadOS window controls. Let the
/// system reserve their actual region instead of assuming a fixed inset.
struct PreviewWindowControlsLayout: ViewModifier {
    @ViewBuilder func body(content: Content) -> some View {
        if #available(iOS 26.0, *), UIDevice.current.userInterfaceIdiom == .pad {
            content.containerCornerOffset(.horizontal, sizeToFit: true)
        } else {
            content
        }
    }
}

// Native row insets provide the outer margin; wide layouts keep the readable
// content centered in a bounded column.
struct ConversationColumn: ViewModifier {
    func body(content: Content) -> some View {
        content.frame(maxWidth: 768, alignment: .leading)
            .frame(maxWidth: .infinity)
    }
}

/// Notices work started, progressing or finished in another app on the Mac
/// (Claude or Codex desktop, a terminal, another Agent SDK app) while this
/// Project chat is open: a cheap activity check as soon as the Mac says the
/// chat's files changed, or every few seconds as the fallback, and a coalesced
/// history reload on notices, activity changes or periodically while running.
/// While Cupola's own reply runs, only which app on the Mac has the chat open
/// is checked, and the full check runs again as soon as that reply ends. What
/// the last check said is forgotten when checking stops.
private struct RunningElsewhereRefresh: ViewModifier {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let readOnly: Bool
    @Environment(\.scenePhase) private var phase
    private var watching: Bool { !readOnly && phase == .active && model.isProject(chat) }
    func body(content: Content) -> some View {
        content.task(id: chat.id + ":" + String(watching)) {
            guard watching else { return }
            // Nothing checks the holder once this ends: don't show its last answer.
            defer { model.forgetNativeHolder(chat.id) }
            var lastReload = ContinuousClock.now
            _ = model.takeNativeWake(chat.id)
            // The baseline a wake-up is compared with (and the Mac's notices at once).
            _ = await model.nativeActivityChanged(chat, holderOnly: !model.shouldPollNativeActivity(chat.id))
            while !Task.isCancelled {
                let running = model.turnRunsElsewhere(chat.id)
                // Cupola's own reply: wake-ups are its own writes, so only the
                // interval checks the holder, and the reply ending checks at once.
                let paused = !model.shouldPollNativeActivity(chat.id)
                // The Mac wakes the check when another app writes this chat;
                // the interval stays as the fallback (and for older Macs). A
                // live event stream carries those wake-ups, so it rarely needs it.
                // Otherwise the wait sleeps until either; it does not poll.
                let deadline = ContinuousClock.now + .seconds(running ? 3 : model.eventStreamLive ? 15 : 5)
                var woken = false
                if paused {
                    // Cupola's own reply: its writes wake the chat, so wake-ups
                    // don't count; check again at the interval or as soon as
                    // the reply ends (a light poll only while it runs).
                    while ContinuousClock.now < deadline, !model.shouldPollNativeActivity(chat.id) {
                        do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
                    }
                    _ = model.takeNativeWake(chat.id)
                } else {
                    woken = await model.waitForNativeWake(chat.id, until: deadline)
                }
                guard !Task.isCancelled else { return }
                guard model.shouldPollNativeActivity(chat.id) else {
                    _ = await model.nativeActivityChanged(chat, holderOnly: true)
                    continue
                }
                let changed = await model.nativeActivityChanged(chat)
                guard !Task.isCancelled else { return }
                // Claude can update an existing message without changing the
                // latest turn, status or item count. The native notice itself
                // invalidates history; the activity summary is not a revision.
                if woken || changed || (model.turnRunsElsewhere(chat.id) && ContinuousClock.now - lastReload >= .seconds(8)) {
                    lastReload = .now
                    model.requestNativeHistoryRefresh(chat, urgent: changed && model.nativeHistoryNeedsPromptRefresh(chat.id), activityChanged: changed)
                }
            }
        }
    }
}

private struct ComposerUsageMonitor: ViewModifier {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let readOnly: Bool
    @Environment(\.scenePhase) private var phase
    private var scope: String {
        [model.assignmentScope, chat.id, model.usageModel(chat), String(phase == .active)].joined(separator: ":")
    }
    func body(content: Content) -> some View {
        content.task(id: scope) {
            guard !readOnly, phase == .active else { return }
            while !Task.isCancelled {
                await model.refreshComposerUsage(chat)
                do { try await Task.sleep(for: .seconds(60)) } catch { return }
            }
        }
    }
}

/// One owner for tool sheet/pane selection.
extension CompanionTool.Kind: Codable {
    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer().decode(String.self)
        let migrated = value == "changes" || value == "pullRequests" ? "pull-requests" : value
        guard let kind = Self(rawValue: migrated) else {
            throw DecodingError.dataCorruptedError(in: try decoder.singleValueContainer(), debugDescription: "Unknown companion tool")
        }
        self = kind
    }
    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

struct ToolPresentation {
    /// A viewing preference, never a session or permission to control a computer.
    struct Saved: Codable, Equatable {
        let kind: CompanionTool.Kind
        var expanded = false
        var targetID: String?
    }
    var saved: Saved? {
        guard let tool else { return nil }
        let target: String?
        switch tool {
        case .agentTask(let child): target = child.id
        case .replyChanges(let turnID): target = turnID
        default: target = nil
        }
        return Saved(kind: tool.kind, expanded: expanded, targetID: target)
    }
    mutating func restore(_ saved: Saved?, layout: Layout, task: (String) -> ProjectSubagentSummary? = { _ in nil }) {
        self.layout = layout
        tool = saved.flatMap { saved in
            switch saved.kind {
            case .computer: .computer
            case .simulators: .simulators
            case .terminal: .terminal
            case .pullRequests: .pullRequests
            case .details: .details
            case .files: .files(WorkspaceBrowserRequest())
            case .agentTask: saved.targetID.flatMap(task).map(CompanionTool.agentTask)
            case .replyChanges: saved.targetID.map { .replyChanges(turnID: $0) }
            }
        }
        expanded = layout == .regular && tool != nil && saved?.expanded == true
        identity &+= 1
    }
    enum Layout { case compact, regular }
    enum Action {
        case open(CompanionTool, toggle: Bool = false)
        case layoutChanged(Layout)
        case dismiss(presentation: UInt64)
        case close
        case expand(Bool)
    }

    private(set) var layout: Layout = .compact
    private(set) var tool: CompanionTool?
    private(set) var expanded = false
    /// A sheet dismissal belongs to the presentation that created its binding.
    /// Folding/unfolding or replacing a tool makes an old callback harmless.
    private(set) var identity: UInt64 = 0
    var pane: CompanionTool? { layout == .regular ? tool : nil }
    var phone: CompanionTool? { layout == .compact ? tool : nil }

    mutating func reduce(_ action: Action) {
        switch action {
        case .open(let next, let toggle):
            if toggle, tool?.kind == next.kind {
                reduce(.close)
            } else if tool != next {
                tool = next
                identity &+= 1
            }
        case .layoutChanged(let next):
            guard layout != next else { return }
            layout = next
            expanded = false
            identity &+= 1
        case .dismiss(let presentation):
            guard identity == presentation else { return }
            reduce(.close)
        case .close:
            tool = nil
            expanded = false
            identity &+= 1
        case .expand(let value):
            expanded = pane != nil && value
        }
    }
}

/// Centre the title on the larger available side of a vertical fold. With
/// Chats open, the small remainder of the leading display may not fit a pill.
private struct ConversationHeaderFoldPlacement: ViewModifier {
    /// The composer's middle in window coordinates: the pill and arrow share its centre line.
    var composerMidX: CGFloat?
    private struct Placement: Equatable {
        let width: CGFloat
        let trailing: Bool
        /// Window x of the frame's middle before any shift; the offset to the composer's middle follows.
        var frameMidX: CGFloat = 0
        /// Fallback centre (no composer measured yet): the middle of the chosen display.
        var displayMidX: CGFloat = 0
    }
    @State private var placement: Placement?

    func body(content: Content) -> some View {
        content
            .frame(maxWidth: placement?.width ?? .infinity)
            .frame(maxWidth: .infinity, alignment: placement?.trailing == true ? .trailing : .leading)
            .offset(x: placement.map { (composerMidX ?? $0.displayMidX) - $0.frameMidX } ?? 0)
            .onGeometryChange(for: Placement?.self) { proxy in
                guard let fold = DuoFold.region(in: proxy)?.frame, fold.height > fold.width else { return nil }
                // The two displays are equal halves of the window about the fold's middle. Centre on
                // the one with more of this column: its own middle, not the column's (a rail or margin
                // beside the column, or the fold's band, must not pull the title off it).
                let origin = proxy.frame(in: .global).minX
                let middle = origin + fold.midX
                let columnStart = origin, columnEnd = origin + proxy.size.width
                let leadingStart = max(columnStart, 0), leadingEnd = min(columnEnd, middle)
                let trailingStart = max(columnStart, middle), trailingEnd = min(columnEnd, 2 * middle)
                let leadingWidth = max(0, leadingEnd - leadingStart)
                let trailingWidth = max(0, trailingEnd - trailingStart)
                guard leadingWidth > 0 || trailingWidth > 0 else { return nil }
                let onTrailing = trailingWidth > leadingWidth
                let visible = onTrailing ? (trailingStart, trailingEnd) : (leadingStart, leadingEnd)
                let display = onTrailing ? middle * 1.5 : middle / 2
                let width = max(0, visible.1 - visible.0 - 24)
                // The frame hugs the column's leading edge, or its trailing edge on the right display.
                let anchoredTrailing = onTrailing && visible.1 >= columnEnd
                let frameCentre = anchoredTrailing ? columnEnd - width / 2 : columnStart + width / 2
                return Placement(width: width, trailing: anchoredTrailing, frameMidX: frameCentre, displayMidX: display)
            } action: { placement = $0 }
    }
}

struct ConversationView: View {
    @Environment(\.voiceScene) private var voiceScene
    @Environment(\.compactVoiceBarVisible) private var compactVoiceBarVisible
    @State private var composerControlWidth: CGFloat = 0
    @State private var composerBottomInset: CGFloat = 0
    @State private var composerMidX: CGFloat?
    @State private var composerModelPresentation = ComposerModelPresentation()
    private var separatesComposerChoices: Bool { typeSize.isAccessibilitySize || composerControlWidth < 340 }
    @Environment(\.openURL) private var openWebLink
    @Environment(\.scenePhase) private var phase
    @State private var activityDisclosure = ActivityDisclosurePolicy.State()
    /// Identifies this window's conversation screen to chat media it starts.
    @State private var mediaOwner = UUID()
    @State private var expandedDetails: Set<String> = []
    /// Open "Ran 6 commands" rows, by their `commands:` node IDs.
    @State private var expandedCommandRuns: Set<String> = []
    /// Not observed: it only remembers the last feed projection.
    @State private var feedPresenter = ConversationFeedPresenter()
    /// Advances when a pending question expires, so it joins the
    /// conversation's settled questions without waiting for a new read.
    @State private var questionExpiryTick = 0
    @State private var requestedScrollID: String?
    /// Where the bar's title and trailing button sit, so the title fits between
    /// the bar's buttons. Changes only when the window or the bar's items change.
    @State private var headerGeometry = ProjectConversationHeader.BarGeometry()
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let rootChat: ChatSummary?
    let readOnly: Bool
    @State private var showingProjectSubagents = false
    /// Settings → Pills: which composer pills show, in what order.
    @ObservedObject private var pillPreferences = ComposerPillPreferences.shared
    @State private var showingGoal = false
    /// The thread waits for the owner in the app on the Mac: the sidebar's
    /// row, or the thread's own read when no loaded row lists it.
    @State private var threadWaitsOnMac = false
    /// A loaded sidebar row lists this thread (else `followComputerWait` reads it).
    @State private var sidebarListsThread = true
    /// The running turn's computer use, prepared when the latest turn changes.
    @State private var computerActivityTime = Date()
    private var agentComputer: ComputerAgentActivity {
        (model.computerActivities[chat.id] ?? .idle).current(at: computerActivityTime)
    }
    /// A macOS dialog is open on the Mac during this thread's turn (from the Project thread list).
    @State private var waitingOnMacPermission = false
    @State private var implementingPlan = false
    /// Mirrors the project library's plan mode and host support for this thread,
    /// so a change re-renders this view without observing the whole library.
    @State private var planModeOn: Bool
    /// Only this thread's host notice should invalidate its composer.
    @State private var projectNotice: String?
    /// When the computer resumes this thread after a usage limit.
    @State private var resumesAt: String?
    @State private var confirmingTakeOver = false
    @AppStorage(TakeOverWarning.storageKey) private var warnBeforeTakeOver = true
    @AppStorage(FollowUpBehavior.storageKey) private var followUp = FollowUpBehavior.defaultChoice
    @State private var supportsModes: Bool
    @State private var importing = false
    @State private var selectingPhoto = false
    @State private var showingCamera = false
    /// The sheet carries the scope it opened for: with isPresented plus a separate
    /// scope, an iPad (menu dismissing first) rendered the sheet before the scope
    /// and showed it empty.
    @State private var sketchRequest: SketchRequest?
    @State private var cameraScope: String?
    @State private var importScope: String?
    @State private var showingAttention = false
    /// Set when a request opened its review by itself: Allow and Decline ignore taps until then.
    @State private var attentionTapGuardUntil: Date?
    /// Requests this chat already opened by itself: each opens once, so closing
    /// the review keeps it closed until the agent asks something new.
    @State private var autoOpenedRequests: Set<String> = []
    /// This Mac's chat detail per provider, mirrored from the project library like plan mode.
    @State private var chatDetail: ChatDetailSettings?
    /// The page covers the chat without changing its timeline or tool selection.
    @StateObject private var browser = BrowserPresentation()
    #if WONDER_DIAGNOSTICS
    @State private var choosingPreviewPage = false
    @AppStorage("diagnostics.voiceRailPrototype") private var diagnosticsVoiceRail = false
    #endif
    /// A tapped link to the computer's page, checking this chat's servers first.
    @State private var loopbackLinkTask: Task<Void, Never>?
    @State private var tools = ToolPresentation()
    /// Live computer and simulator sessions, carried between the pane and the cover.
    @State private var toolSessions = ToolSessionStore()
    /// The size class `tools.layout` was last settled for: a turn keeps a live
    /// computer or simulator view where it is, a fold or unfold moves it.
    @State private var layoutSizeClass: UserInterfaceSizeClass?
    @State private var restoredTools = false
    @State private var toolPresentationOwner = UUID()
    /// The system shows a vertical bar for toolbar items (iPhone Duo).
    @State private var verticalBar = false
    @Environment(\.shellWindowSize) private var toolWindowSize
    @Environment(\.shellBarEpoch) private var barEpoch
    @Environment(\.shellWindowBounds) private var toolWindowBounds
    @Environment(\.shellDisplayFold) private var hasDisplayFold
    /// The hinge runs down the page, so a centred title could straddle it. A
    /// horizontal fold (unfolded portrait) leaves the bar whole: one row.
    @State private var measuredFoldRunsVertically: Bool?
    /// Until the first geometry pass measures the fold, a wide window has a vertical hinge and a tall one a horizontal
    /// hinge, so the header is right on the first frame.
    private var foldRunsVertically: Bool {
        measuredFoldRunsVertically ?? ((toolWindowBounds ?? toolWindowSize).map { $0.width > $0.height } ?? false)
    }
    private var inlineConversationHeader: Bool { pillsInBar || (hasDisplayFold && foldRunsVertically) }
    @StateObject private var computerRail = ComputerRail()
    @Environment(\.shellChatsCoverConversation) private var shellChatsCoverConversation
    @Environment(\.horizontalSizeClass) private var conversationSizeClass
    @Environment(\.pairedComputerCount) private var pairedComputerCount
    /// Inline `/` and `@` lists. `@Observable` held in `@State`: owned here, and
    /// only the list observes it, so typing does not re-render the conversation.
    @State private var completion = ComposerCompletionModel()
    /// A thread name typed in Thread details, kept while its sheet or pane closes.
    /// Observable and only passed on, so typing does not redraw the conversation.
    @State private var threadTitle = ThreadTitleDraft()
    @State private var notice: String?
    @State private var forking = false
    /// Edit from here: the Mac is rewinding, or the owner is confirming one.
    @State private var rewinding = false
    /// Reading the chat again for an edit the Mac never confirmed.
    @State private var checkingRewind = false
    @State private var rewindRequest: RewindRequest?
    /// The header's context and usage panel.
    @State private var showingContextUsage = false
    /// Opens another conversation in the shell; nil where a chat cannot navigate.
    let openConversation: ((String) -> Void)?
    /// Archives this chat from its menu; nil where its computer can't archive it.
    /// The owner (the shell) keeps the chat on screen as archived, with Restore.
    let archiveConversation: (() async throws -> Void)?
    /// The shell returns to Chats after its saved copy has been deleted.
    let deletedConversation: (() -> Void)?
    @State private var archiving = false
    @State private var confirmingArchive = false
    @State private var confirmingDelete = false
    @State private var deleting = false
    @AppStorage(ActionConfirmation.archiveKey) private var confirmArchive = ActionConfirmation.archiveDefault
    @State private var composerPhoto: ComposerAttachment?
    @State private var annotationEdit: ComposerAttachment?
    @State private var imagePasteTask: Task<Void, Never>?
    @State private var fileImportTask: Task<Void, Never>?
    @State private var messagePhotoGallery: MessagePhotoGallery?
    @State private var resolvedAttachmentMetadata: AttachmentMetadataRequest?
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.wonderTheme) private var theme
    private struct AttachmentMetadataRequest: Hashable {
        let chatID: String
        let scope: String
        let ids: [String]
    }
    private var cameraContextToken: String {
        model.cameraContextID.uuidString + ":" + chat.id
    }
    private var conversationCovered: Bool {
        browser.browser != nil || shellChatsCoverConversation || (companion != nil && companionExpanded) || showingDetails || importing || selectingPhoto || showingCamera || composerPhoto != nil || messagePhotoGallery != nil || selectedProjectSubagent != nil || showingProjectSubagents || showingGoal || showingComputer || showingSimulators
    }
    /// The provider owns computer activity in this conversation.
    private var computerAgentName: String { (model.agentFamily(chat)?.title ?? "Agent") }

    /// Published values arrive before the library stores them; read on the next turn.
    private func noteMacPermissionWait() {
        Task { @MainActor in
            let waiting = model.projects.waitsOnMacPermission(chat.id)
            if waitingOnMacPermission != waiting { waitingOnMacPermission = waiting }
            let listed = model.sidebarWaitsOnMac(chat.id) != nil
            if sidebarListsThread != listed { sidebarListsThread = listed }
            let onMac = model.threadWaitsOnMac(chat.id)
            if threadWaitsOnMac != onMac { threadWaitsOnMac = onMac }
        }
    }

    /// A tapped notification for this chat opens its computer view while it
    /// waits on the computer; otherwise the chat opens as usual.
    private func openRequestedComputer() {
        guard !readOnly, model.computerRequest?.conversation == chat.id,
              model.waitsOnComputer(chat, waitsOnMac: threadWaitsOnMac), model.takeComputerRequest(chat.id) else { return }
        openTool(.computer)
    }

    private func messageAttachments(for row: ReadRow, metadata: [String: ConversationFile]) -> [MessageAttachmentPresentation] {
        return row.attachmentIds.map { id in
            MessageAttachmentPresentation(id: id, file: metadata[id])
        }
    }
    init(
        model: ConnectionModel,
        chat: ChatSummary,
        rootChat: ChatSummary? = nil,
        readOnly: Bool = false,
        openConversation: ((String) -> Void)? = nil,
        archiveConversation: (() async throws -> Void)? = nil,
        deletedConversation: (() -> Void)? = nil
    ) {
        self.model = model
        self.chat = chat
        self.rootChat = rootChat
        self.readOnly = readOnly
        self.openConversation = openConversation
        self.archiveConversation = archiveConversation
        self.deletedConversation = deletedConversation
        _planModeOn = State(initialValue: model.projects.details[chat.id]?.planMode == true)
        _projectNotice = State(initialValue: model.projects.details[chat.id]?.notice)
        _resumesAt = State(initialValue: model.projects.details[chat.id]?.resumesAt)
        _supportsModes = State(initialValue: model.projects.supportsModes)
        _chatDetail = State(initialValue: model.projects.chatDetail)
    }
    private func subagentThreadIDs(for row: ReadRow) -> [String] {
        guard let item = row.item,
              item.type == "subAgentActivity" || item.type == "collabAgentToolCall" else { return [] }
        if let threadID = item.payload?["agentThreadId"]?.string { return [threadID] }
        let receivers = item.payload?["receiverThreadIds"]?.array?.compactMap(\.string) ?? []
        var seen = Set<String>()
        return receivers.filter { seen.insert($0).inserted }
    }
    private func projectSubagents(for row: ReadRow) -> [ProjectSubagentSummary] {
        guard model.isProject(chat) else { return [] }
        return subagentThreadIDs(for: row).compactMap { threadID in
            model.projectSubagent(threadID: threadID, parentConversationID: chat.id)
        }
    }
    @ViewBuilder private func entryContent(
        _ entry: ChatFeedEntry,
        turns: [String: ReadTurn],
        latestActivityEntryIDs: Set<String>,
        latestActiveActivityEntryID: String?,
        expandedActivityIDs: Set<String>,
        attachmentMetadata: [String: ConversationFile],
        implementablePlanID: String?,
        providerNotes: [String: String] = [:]
    ) -> some View {
                                if entry.isActivity {
                                    ActivityGroupView(rows: entry.rows,
                                                      turn: entry.rows.first?.turnId.flatMap { turns[$0] },
                                                      isLatestSegmentForTurn: latestActivityEntryIDs.contains(entry.id),
                                                      isLatestActiveSegment: latestActiveActivityEntryID == entry.id,
                                                      expanded: expandedActivityIDs.contains(entry.id)) {
                                        toggleActivity(entry: entry, isExpanded: expandedActivityIDs.contains(entry.id))
                                    }
                                } else if let row = entry.rows.first, let checklist = row.taskChecklist {
                                    TaskChecklistView(checklist: checklist, rowID: row.id)
                                        .frame(maxWidth: 640, alignment: .leading)
                                } else if let row = entry.rows.first, row.isPlan {
                                    PlanCard(text: row.planText, isRunning: row.activitySummary?.isRunning == true,
                                             canImplement: row.id == implementablePlanID, isImplementing: implementingPlan,
                                             implement: implementPlan)
                                        .frame(maxWidth: 640, alignment: .leading)
                                } else if let row = entry.rows.first, readOnly || !(model.asyncQuestions[chat.id] ?? []).contains(where: { $0.rowId == row.id }) {
                                if let question = row.nativeQuestion {
                                    NativeQuestionHistoryRow(question: question, rowID: row.id, author: row.author)
                                        .frame(maxWidth: 640, alignment: .leading)
                                } else {
                                let pending = model.composers[chat.id]?.pending
                                let isPending = row.id == pending.map({ "user-" + $0.request.clientMessageId })
                                let messageAttachments = messageAttachments(for: row, metadata: attachmentMetadata)
                                let finishedTurn = row.turnId.flatMap { turns[$0] }
                                MessageRow(row: row, pending: isPending ? pending : nil,
                                    recoveredBeforeReconnect: model.composers[chat.id]?.recoveredPending?.contains(where: { row.id == "user-" + $0.request.clientMessageId }) == true,
                                    isSending: model.sending.contains(chat.id),
                                    acceptance: row.isUser && row.id.hasPrefix("user-") ? model.awaitingAcceptance[chat.id]?[String(row.id.dropFirst(5))]?.label : nil,
                                    canRetry: !readOnly && !model.accessEnded && !model.previewMode && model.macConnected != false,
                                    retry: { Task { await model.deliver(chat) } },
                                    retryUndelivered: readOnly ? nil : { [model, chat] id in try await model.retryUndelivered(chat, messageID: id) },
                                    attachments: messageAttachments,
                                    imagePreviews: model.imagePreviews,
                                    imagePreviewScope: model.imagePreviewScope,
                                    chatID: chat.id,
                                    loadRemoteData: { [model, chat] file in try await model.thumbnail(file, chat: chat, size: .chip) },
                                    openImage: { file in
                                        guard let id = file.file?.id else { return }
                                        // The viewer swipes through this message's pictures only.
                                        messagePhotoGallery = MessagePhotoGallery(
                                            files: messageAttachments.compactMap(\.file),
                                            initialFileID: id
                                        )
                                    },
                                    openDocument: { showFiles(WorkspaceBrowserRequest(attachmentIDs: row.attachmentIds)) },
                                    forkFromHere: forkTurnID(for: row).map { turn in { fork(from: turn) } },
                                    editFromHere: rewindTurnID(for: row, isPending: isPending).map { turn in { askToRewind(row, turnID: turn) } },
                                    draftNativeReply: readOnly || model.accessEnded ? nil : { text in
                                        switch model.fillEmptyDraft(text, chat: chat.id) {
                                        case .saved: return nil
                                        case .occupied: return "Your draft is already in use. Finish it before adding this reply."
                                        case .saveFailed(let explanation): return explanation
                                        }
                                    },
                                    finishedAt: !row.isUser && row.settledCommentary == false && finishedTurn?.status == "completed"
                                        ? finishedTurn?.completedAt ?? row.timestamp : nil,
                                    providerNote: providerNotes[row.id])
                                }
                                }
    }
    private func toggleActivity(entry: ChatFeedEntry, isExpanded: Bool) {
        guard let turnID = entry.rows.first?.turnId else { return }
        let activeTurnIDs = model.activeTurnIDs(chat.id)
        let lifecycle = ActivityDisclosurePolicy.lifecycle(for: model.turn(turnID, in: chat.id))
        let descriptor = ActivityDisclosurePolicy.Entry(
            conversationID: chat.id,
            turnID: turnID,
            entryID: entry.id,
            lifecycle: lifecycle,
            autoOpenWhileActive: activeTurnIDs.contains(turnID)
        )
        #if WONDER_DIAGNOSTICS
        Diagnostics.shared.interaction("activity.expand", count: isExpanded ? 0 : 1)
        #endif
        activityDisclosure = ActivityDisclosurePolicy.toggled(activityDisclosure, entry: descriptor, isExpanded: isExpanded)
        // The tapped row is already visible. A forced animated scroll on every
        // disclosure change repeatedly lays out long histories and moves the
        // reader away from the row they opened.
    }
    private func toggleDetail(_ id: String) {
        #if WONDER_DIAGNOSTICS
        Diagnostics.shared.interaction("detail.expand", count: expandedDetails.contains(id) ? 0 : 1)
        #endif
        if !expandedDetails.insert(id).inserted { expandedDetails.remove(id) }
        // Like a Working group, the tapped row is already visible; scrolling it
        // to the top moved the reader and stopped following new output.
    }
    /// When the next answerable question stops being answerable.
    private var nextQuestionExpiry: UInt64? {
        guard !readOnly else { return nil }
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        return (model.asyncQuestions[chat.id] ?? []).filter { $0.canAnswer(now: now) }.map(\.expiresAtMs).min()
    }
    private func firstExpandableActivity(in entries: [ChatFeedEntry]) -> ReadRow? {
        for entry in entries where entry.isActivity {
            if let row = entry.rows.first(where: { row in
                row.activitySummary != nil && row.item?.type != "reasoning" && !row.isPlan
            }) {
                return row
            }
        }
        return nil
    }
    @ViewBuilder private func activityContent(_ row: ReadRow) -> some View {
        if row.isCommentary {
            CommentaryBubble(text: row.text, author: row.author)
                .frame(maxWidth: 640, alignment: .leading)
        } else {
            ActivityItemView(
                row: row,
                expanded: expandedDetails.contains(row.id),
                conversationID: chat.id,
                outputScope: model.pairingFence,
                loadOutput: { [model] identity in try await model.toolOutput(identity) },
                projectSubagents: projectSubagents(for: row),
                projectSubagentsAvailable: model.macConnected == true && model.projectSubagentAvailability[chat.id] != false,
                projectSubagentFreshIDs: model.projectSubagentFreshIDs[chat.id] ?? [],
                openProjectSubagent: { openTool(.agentTask($0)) },
                openFile: { path in showFiles(WorkspaceBrowserRequest(initialFilePath: path)) },
                toggle: { toggleDetail(row.id) }
            )
        }
    }
    /// A plan-mode thread offers to implement its newest finished plan once the
    /// agent is idle and the composer has nothing in progress.
    private func planToImplement(in timeline: [ReadRow]) -> String? {
        guard !readOnly, supportsModes, model.isProject(chat), planModeOn, model.activeTurn(chat.id) == nil else { return nil }
        let intent = model.composers[chat.id]
        guard (intent?.draft ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              (intent?.attachmentCount ?? 0) == 0, intent?.pending == nil else { return nil }
        return ReadRow.implementablePlanID(in: timeline)
    }
    /// Leaves plan mode, then sends the plan for implementation. If the save
    /// fails the composer reports it and nothing is sent.
    private func implementPlan() {
        guard !implementingPlan else { return }
        implementingPlan = true
        let scope = model.assignmentScope
        Task {
            defer { implementingPlan = false }
            guard await setPlanMode(false, model: model, library: model.projects, chat: chat),
                  !Task.isCancelled, scope == model.assignmentScope, model.visibleChat?.id == chat.id,
                  model.activeTurn(chat.id) == nil else { return }
            let intent = model.composers[chat.id]
            guard (intent?.draft ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                  (intent?.attachmentCount ?? 0) == 0, intent?.pending == nil else { return }
            model.editDraft("Implement the plan.", chat: chat.id)
            await model.send(chat)
        }
    }
    /// Project history belongs to the provider, so it is re-read alongside the
    /// saved copy that is already on screen. Send waits for the re-read.
    private func refreshNativeHistory() async {
        if model.isProject(chat), !readOnly { await model.reloadNativeHistoryIfBehind(chat) }
    }
    var body: some View {
        #if WONDER_DIAGNOSTICS
        let timelineStart = ProcessInfo.processInfo.systemUptime
        let _ = RenderCounts.conversation += 1
        #endif
        let prepared = workspaceRequest == nil
            ? model.timeline(for: chat, focusedRowID: model.searchFocus?.conversationID == chat.id ? model.searchFocus?.rowID : nil)
            : ConversationTimeline(chatID: chat.id, rows: [], activeTurnIDs: [],
                                   activeTurnID: nil, focusedRowID: nil, turns: nil)
        let timeline = prepared.rows
        let attachmentMetadata = Dictionary((model.files[chat.id] ?? []).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let requestedAttachmentIDs = Array(Set(
            prepared.attachmentIDs
                + (model.queues[chat.id] ?? []).flatMap(\.attachmentIds)
                + (model.composers[chat.id]?.draftAttachmentIds ?? [])
        )).sorted()
        let attachmentMetadataRequest = AttachmentMetadataRequest(
            chatID: chat.id,
            scope: model.assignmentScope,
            ids: requestedAttachmentIDs
        )
        // Resolve image versus document sizing before revealing the initial
        // reading position. A failed read still opens the saved conversation.
        let attachmentLayoutReady = model.previewMode || model.macConnected == false
            || requestedAttachmentIDs.allSatisfy { attachmentMetadata[$0] != nil }
            || resolvedAttachmentMetadata == attachmentMetadataRequest
        let entries = prepared.entries
        let latestActivityEntryIDs = prepared.latestActivityEntryIDs
        let latestActiveActivityEntryID = prepared.latestActiveActivityEntryID
        let disclosureEntries = prepared.disclosureEntries
        // Placed per update (state, answer and expiry change without a new
        // timeline revision), one lookup per question in the prepared index.
        let _ = questionExpiryTick
        let questions: (byEntry: [String: [AsyncQuestion]], outside: [AsyncQuestion]) = readOnly ? ([:], [])
            : prepared.feedIndex.placing(model.asyncQuestions[chat.id] ?? [], now: UInt64(Date().timeIntervalSince1970 * 1000))
        let feed = feedPresenter.output(for: prepared, disclosure: activityDisclosure,
            searchFocusRowID: model.searchFocus?.conversationID == chat.id ? model.searchFocus?.rowID : nil,
            chosenDetail: chatDetail?.detail(for: model.agentFamily(chat), chat: chat.id) ?? .everything,
            expandedCommandRuns: expandedCommandRuns, openDetails: expandedDetails, questions: questions.byEntry)
        let expandedActivityIDs = feed.expandedActivityIDs
        let nodes = feed.nodes
        let lastNodeID = nodes.last?.id
        let replyChangeHosts = ResponseEditedFiles.hosts(nodes, entries: entries, footers: prepared.replyChanges)
        let visibleReceipt = model.readReceipt(for: chat.id)
        let answeredQuestions = questions.outside
        // An outcome or question row reads as the entry it follows on the
        // mini map and when restoring a reading position.
        let nodeOwners = ChatFeedNode.owners(nodes)
        let disclosureRevision = disclosureEntries
        let implementablePlanID = planToImplement(in: timeline)
        // Files skips the timeline projection, but the composer row keeps the
        // Git pill: read the reply edits from the model's cached timeline.
        let replyTimeline = workspaceRequest == nil ? prepared
            : model.timeline(for: chat, focusedRowID: model.searchFocus?.conversationID == chat.id ? model.searchFocus?.rowID : nil)
        #if WONDER_DIAGNOSTICS
        if workspaceRequest == nil {
            let _ = DiagnosticJournal.shared.record(DiagnosticEvent(operation: "timeline.prepare", durationMs: (ProcessInfo.processInfo.systemUptime - timelineStart) * 1000, count: UInt64(timeline.count)))
        }
        #endif
        let transcript = Group {
            if let request = workspaceRequest {
                WorkspaceBrowser(model: model, chat: chat, attachmentIDs: request.attachmentIDs,
                                 initialFilePath: request.initialFilePath, preferredRootID: request.preferredRootID,
                                 reanchor: request.reanchor, replacingAnnotationID: request.replacingAnnotationID)
                    // Files fills the page under the floating title pill: start its header below the pill.
                    .padding(.top, inlineConversationHeader ? 44 : 0)
                    // A read-only chat has no composer, so no Files pill to close it: the browser
                    // draws its own header row with Done.
                    .environment(\.workspacePaneHeader, readOnly ? WorkspacePaneHeaderContext(close: closeCompanion) : nil)
                    .id(request.id)
            } else if showingAttention {
                AttentionReview(model: model, chat: chat, headerClearance: inlineConversationHeader ? 44 : 0,
                                tapGuardUntil: attentionTapGuardUntil) { showingAttention = false }
            } else if model.snapshots[chat.id] != nil {
                ConversationScroller(model: model, projects: model.projects, chat: chat, entries: entries,
                    minimap: prepared.minimap,
                    nodeIDs: nodes.map(\.id),
                    nodeOwners: nodeOwners,
                    isCovered: conversationCovered,
                    composerMidX: composerMidX,
                    attachmentLayoutReady: attachmentLayoutReady,
                    headerClearance: inlineConversationHeader ? 44 : 0,
                    requestedScrollID: $requestedScrollID) {
                        Group {
                            if let cursor = model.snapshots[chat.id]?.thread.nextCursor {
                                OlderMessagesLoader(model: model, chat: chat, cursor: cursor)
                                    .modifier(ConversationColumn())
                            }
                            ForEach(readOnly ? [] : answeredQuestions) { q in
                                AsyncQuestionRow(model: model, chat: chat, question: q)
                                    .modifier(ConversationColumn())
                            }
                            if nodes.isEmpty, !readOnly, model.goalRunsWithoutMessages(chat.id), let goal = model.goals[chat.id] {
                                GoalRunningNotice(objective: goal.objective).modifier(ConversationColumn())
                            }
                            ForEach(nodes) { node in
                                VStack(alignment: .leading, spacing: 12) {
                                switch node.content {
                                case .activity(let row):
                                    activityContent(row)
                                case .thinking(let row):
                                    ThinkingActivityView(row: row, isRunning: row.activitySummary?.isRunning == true,
                                                         summary: row.thinkingSummary)
                                case .commands(let run):
                                    // Open output keeps its run open (`openDetails`);
                                    // closing the run closes that output too.
                                    let openOutput = Set(run.rows.map(\.id)).intersection(expandedDetails)
                                    CommandRunView(run: run, expanded: expandedCommandRuns.contains(node.id) || !openOutput.isEmpty) {
                                        if expandedCommandRuns.contains(node.id) || !openOutput.isEmpty {
                                            expandedCommandRuns.remove(node.id)
                                            expandedDetails.subtract(openOutput)
                                        } else {
                                            expandedCommandRuns.insert(node.id)
                                        }
                                    }
                                case .outcome(let outcome):
                                    TurnOutcomeMarker(outcome: outcome, isProjectConversation: model.isProject(chat))
                                case .question(let question):
                                    AsyncQuestionRow(model: model, chat: chat, question: question)
                                case .compaction(let row):
                                    if row.isTaskNotification { TaskNotificationRow(row: row) }
                                    else { ContextCompactionMarker(row: row, isProjectConversation: model.isProject(chat)) }
                                case .file(let file): ToolFilePreview(model: model, chat: chat, file: file)
                                case .gallery(let files): ToolImageGallery(model: model, chat: chat, files: files)
                                case .updatedPage(let file): UpdatedPageNotice(model: model, chat: chat, file: file)
                                case .entry(let entry):
                                entryContent(
                                    entry,
                                    turns: prepared.turns,
                                    latestActivityEntryIDs: latestActivityEntryIDs,
                                    latestActiveActivityEntryID: latestActiveActivityEntryID,
                                    expandedActivityIDs: expandedActivityIDs,
                                    attachmentMetadata: attachmentMetadata,
                                    implementablePlanID: implementablePlanID,
                                    providerNotes: prepared.providerNotes
                                )
                                }
                                // After a finished reply that edited files, in this node so
                                // it stays in the conversation's one lazy stack.
                                ForEach(replyChangeHosts[node.id] ?? [], id: \.turnID) { edits in
                                    ReplyChangesRow(edits: edits) { openTool(.replyChanges(turnID: edits.turnID)) }
                                }
                                }.modifier(ConversationColumn())
                                .modifier(ConversationTextColors())
                                #if WONDER_DIAGNOSTICS
                                .modifier(ConversationRestoreFixtureRow(id: node.id))
                                #endif
                                .id(node.id)
                                .accessibilityElement(children: .contain)
                                .background {
                                    ConversationRowMarker(id: node.id,
                                        receipt: node.id == lastNodeID ? visibleReceipt : nil)
                                }
                            }
                            VStack(alignment: .leading, spacing: 0) {
                            // Only a waiting message adds space above the end; an empty dock must not.
                            if !readOnly, !(model.composers[chat.id]?.outbox ?? []).isEmpty {
                                OutboxDock(model: model, chat: chat).modifier(ConversationColumn()).padding(.bottom, 12)
                            }
                            Color.clear.frame(height: 1).modifier(ConversationColumn())
                                .background { GeometryReader { geometry in
                                    Color.clear.preference(key: BottomFrameKey.self, value: geometry.frame(in: .global))
                                } }
                            }.id("conversation-bottom")
                            .listRowInsets(EdgeInsets(top: 0, leading: 16, bottom: 0, trailing: 16))
                        }
                    }
            } else if let failure = model.conversationLoadFailures[chat.id] {
                ContentUnavailableView {
                    Label("Couldn’t load this chat", systemImage: "exclamationmark.bubble")
                } description: {
                    Text(failure.message)
                } actions: {
                    if failure.canRetry {
                        Button("Try again") { Task { await model.retryConversation(chat) } }
                            .accessibilityIdentifier("conversation-load-retry")
                    }
                }
            } else if model.accessEnded || model.macConnected == false {
                ContentUnavailableView("Not saved on this device", systemImage: "wifi.slash",
                    description: Text("Connect to \(model.macName) to load this chat."))
            } else {
                // Connecting or loading: saved chats never reach this branch.
                ProgressView("Loading chat…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .accessibilityIdentifier("conversation-loading")
            }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        let content = CompanionPaneContainer(tool: companion, expanded: Binding(get: { companionExpanded }, set: { changeTools(.expand($0)) }),
                                             showsClose: companion?.pill == nil || readOnly,
                                             close: closeCompanion) {
        PreparedConversationLayout {
            transcript
            // `http://localhost:5173/…` in a reply is the computer's page, not the phone's.
            .modifier(LoopbackLinkRouting(open: model.previewEndpoint(chat) == nil ? nil : { openLoopbackLink($0) }))
            .task(id: phase == .active ? agentComputer.nextExpiration : nil) {
                guard phase == .active else { return }
                computerActivityTime = .now
                while let expiry = agentComputer.nextExpiration {
                    do { try await Task.sleep(for: .seconds(min(300, max(0, expiry.timeIntervalSinceNow)))) }
                    catch { return }
                    computerActivityTime = .now
                }
            }
        } bottom: {
            // Voice controls remain available even when the viewed chat is read-only.
            VoiceAwareComposer(controller: model.voice, chatID: chat.id) {
                if !readOnly {
                    composer(replyEdits: replyTimeline.latestReplyEdits, lastReplyChanged: replyTimeline.lastReplyChanged, suggestion: replyTimeline.promptSuggestion)
                } else if let reason = model.projectDetail(chat)?.readOnlyReason {
                    Text(reason)
                        .font(.footnote).foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 16).padding(.vertical, 10)
                        .accessibilityIdentifier("chat-read-only-reason")
                }
            }
            .modifier(ChatBottomUnit())
            .onGeometryChange(for: CGFloat.self) { $0.frame(in: .global).midX } action: { composerMidX = $0 }
        }
        // Half an iPad mini rests its composer close to the bottom edge: the whole layout (so
        // every control stays tappable) sinks into the home-indicator inset, but never under
        // the keyboard, whose inset is far larger. A zero reading (a sheet covering the window) is ignored:
        // flipping the padding then closed the model sheet.
        .onGeometryChange(for: CGFloat.self) { $0.safeAreaInsets.bottom } action: { if $0 > 0 { composerBottomInset = $0 } }
        .onGeometryChange(for: Bool.self) { proxy in
            DuoFold.region(in: proxy).map { $0.frame.height > $0.frame.width } ?? false
        } action: { measuredFoldRunsVertically = $0 }
        .padding(.bottom, inlineConversationHeader && composerBottomInset > 0 && composerBottomInset <= 60 ? -20 : 0)
        // The inline title floats over the timeline as a pill: no bar, no band of page color
        // above the messages, which scroll beneath it (the timeline leaves it room at the top).
        .overlay(alignment: .top) {
            if inlineConversationHeader {
                Group {
                    if readOnly { conversationHeader(room: headerGeometry.titleRoom) }
                    else { conversationTitleMenu(replyEdits: replyTimeline.latestReplyEdits, lastReplyChanged: replyTimeline.lastReplyChanged) }
                }
                // Same cap as the navigation bar's title: it never scales past xxxLarge.
                .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
                .frame(minHeight: 44)
                .modifier(ConversationHeaderFoldPlacement(composerMidX: composerMidX))
            }
        }
        } pane: {
            if let companion {
                companionContent(companion, replyEdits: replyTimeline.latestReplyEdits,
                                 lastReplyChanged: replyTimeline.lastReplyChanged,
                                 replyChanges: replyTimeline.replyChanges(turnID:))
                    .environment(\.computerRail, computerRail)
                    .id(companion.contentID)
            }
        }
        let presented = AnyView(content)
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if companion != nil, companionExpanded, !pillsInBar, presentedPillLayout.row.hasPills {
                    ComposerPillRowView(row: presentedPillLayout.row, selection: tools.tool?.pill) { pill in
                        composerPill(pill, replyEdits: replyTimeline.latestReplyEdits,
                                     lastReplyChanged: replyTimeline.lastReplyChanged)
                    }
                    .padding(.horizontal, 16)
                    .padding(.vertical, 8)
                    .background(theme.page)
                }
            }
        #if WONDER_DIAGNOSTICS
        .background { DiagnosticDriverSurface(surface: driverSurface(nodes: nodes)).frame(width: 0, height: 0).accessibilityHidden(true) }
        #endif
        .background { ThemeBackdrop() }
        .toolbar(.hidden, for: .tabBar)
        .navigationTitle(inlineConversationHeader ? "" : chat.title)
        // Let the title float over the page and the list's soft scroll edge.
        // A forced background cuts off the previous row at the bar's boundary.
        .toolbarBackground(.hidden, for: .navigationBar)
        // A finished turn may have edited, committed or switched branches.
        .onChange(of: model.conversationWorking(chat.id)) { wasWorking, working in
            guard wasWorking, !working, model.isProject(chat) else { return }
            Task { await model.loadGitSummary(chat) }
            // The agent may have started or stopped a dev server.
            Task { await model.loadDevServers(chat) }
        }
        .onChange(of: disclosureRevision, initial: true) { _, _ in
            guard workspaceRequest == nil else { return }
            activityDisclosure = ActivityDisclosurePolicy.reconciled(
                activityDisclosure,
                entries: disclosureEntries,
                retainedConversationID: chat.id,
                retainedTurnIDs: prepared.retainedTurnIDs
            )
        }
        #if WONDER_DIAGNOSTICS
        .onAppear {
            if let saved = model.connection, !model.isDemo { Diagnostics.shared.selectHost(saved) }
            Diagnostics.shared.interaction("chat.open")
            LatencyHops.mark("open.appear", chat.id)
            DiagnosticPerformanceRun.threadOpened(chat.id)
        }
        .task(id: chat.id) { await runDictationStreamFixture() }
        .onReceive(DiagnosticScenarioControl.shared.$command) { command in
            guard let command, command.chatID == chat.id else { return }
            var performed = false
            switch command.action {
            case .expand:
                if let entry = entries.first(where: { $0.isActivity }) {
                    toggleActivity(entry: entry, isExpanded: expandedActivityIDs.contains(entry.id))
                    performed = true
                }
            case .detail:
                if let row = firstExpandableActivity(in: entries) {
                    toggleDetail(row.id)
                    performed = true
                }
            case .bottom: requestedScrollID="conversation-bottom"; performed=true
            case .top: requestedScrollID=nodes.first?.id; performed = !nodes.isEmpty
            }
            DiagnosticScenarioControl.shared.acknowledge(command.id, performed: performed)
        }
        #endif
        // Only the width: the phone sidebar drag offsets the whole card every
        // frame, and window coordinates would rebuild this view on each one.
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { headerGeometry.bar = 0...max(0, $0) }
        .toolbar {
            if !shellChatsCoverConversation {
                conversationToolbar(replyEdits: replyTimeline.latestReplyEdits, lastReplyChanged: replyTimeline.lastReplyChanged)
            }
        }
        // A tool filling the window takes the conversation's header too; its own toolbar stays.
        .toolbar(companion != nil && companionExpanded && !pillsInBar ? .hidden : .automatic, for: .navigationBar)
        .environment(\.composerPillsInBar, pillsInBar)
        .readsVerticalBar($verticalBar)
        .onChange(of: model.rewindResolutions[chat.id], initial: true) { _, text in
            // History decided an edit the Mac never confirmed: say so, once.
            guard let text else { return }
            notice = text
            model.rewindResolutions[chat.id] = nil
        }
        .overlay(alignment: .top) {
            if rewinding {
                HStack(spacing: 8) { ProgressView(); Text("Rewinding…") }
                    .font(.footnote.weight(.medium))
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    .background(.regularMaterial, in: Capsule())
                    .padding(.top, 8)
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("rewind-progress")
            } else if forking {
                HStack(spacing: 8) { ProgressView(); Text("Forking…") }
                    .font(.footnote.weight(.medium))
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    .background(.regularMaterial, in: Capsule())
                    .padding(.top, 8)
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("fork-progress")
            } else if let notice {
                ChatNotice(text: notice) { self.notice = nil }
                    .id(notice)
            }
        }
        // Edit from here and the context panel open from just below the header:
        // presented from the whole view or the toolbar ring, they covered its title.
        .background(alignment: .top) {
            HeaderAnchor()
                .confirmationDialog(rewindRequest?.confirmation.title ?? "Edit from this message?",
                                    isPresented: Binding(get: { rewindRequest != nil }, set: { if !$0 { rewindRequest = nil } }),
                                    titleVisibility: .visible) {
                    // Read live, not as presented: the backup check finishes after it opens.
                    if let request = rewindRequest {
                        ForEach(request.confirmation.choices, id: \.self) { choice in
                            Button(ConversationRewind.label(choice), role: choice == .checkBackups ? nil : .destructive) { performRewind(request, choice: choice) }
                                .accessibilityIdentifier(rewindChoiceID(choice))
                        }
                    }
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text(rewindRequest?.confirmation.message ?? "")
                }
                .confirmationDialog("Delete “\(chat.title)”?", isPresented: $confirmingDelete, titleVisibility: .visible) {
                    Button("Delete chat", role: .destructive) { deleteChat() }
                        .accessibilityIdentifier("delete-conversation-confirm")
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text("Deletes this chat’s saved messages, activity, attachment records and scheduled tasks from Cupola. Provider history, project and downloaded files, other chats and unsent drafts are kept. This can’t be undone.")
                }
                .confirmationDialog("Archive “\(chat.title)”?", isPresented: $confirmingArchive, titleVisibility: .visible) {
                    Button("Archive", role: .destructive) { archive() }
                        .accessibilityIdentifier("archive-conversation-confirm")
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text(archiveConsequence)
                }
                // The dialog's presenting strip must fit the same usable
                // display as its title pill, keeping buttons off the fold.
                .modifier(ConversationHeaderFoldPlacement(composerMidX: composerMidX))
        }
        .sheet(isPresented: phonePresentation(.details)) {
            if model.isProject(chat) {
                ProjectConversationDetailsView(model: model, library: model.projects, chat: chat, title: threadTitle)
                    .environment(\.toolHasGestureDismissal, true)
                    .presentationDragIndicator(.hidden)
            }
        }
        .sheet(isPresented: $confirmingTakeOver) {
            let holder = model.nativeHolders[chat.id]
            TakeOverConfirmation(app: holder?.app ?? "another app", computer: model.macName, stops: holder?.stops) { dontShowAgain in
                if dontShowAgain { warnBeforeTakeOver = false }
                confirmingTakeOver = false
                Task { await model.send(chat, takeOver: true) }
            }
        }
        .fullScreenCover(isPresented: Binding(get: { browser.browser != nil }, set: { if !$0 { browser.close() } })) {
            if let page = browser.browser {
                FullScreenPreview(model: page, close: { browser.close() }, stageScreenshot: model.canAttach(chat) ? { png in
                    if (model.composers[chat.id]?.attachmentCount ?? 0) >= 4 { return .attachmentsFull }
                    switch await model.stageCameraPhoto(png, chat: chat, scope: model.assignmentScope, namePrefix: "Preview") {
                    case .attached: return .added
                    case .failed, .cancelled: return .notAdded(nil)
                    }
                } : nil).modifier(LinkOpeningHost())
            }
        }
        .fullScreenCover(isPresented: phonePresentation(.computer)) {
            NavigationStack { // theme-exempt: ComputerSessionView draws theme.computerBackground
                if model.isDemo {
                    DemoComputerView()
                } else {
                    ComputerSessionView(model: model, chat: chat, agent: ComputerAgentPresence(name: computerAgentName, isActive: agentComputer.isUsingComputer, isActing: agentComputer.isActing), pauseAgent: { Task { await model.stop(chat) } })
                }
            }
        }
        .fullScreenCover(isPresented: phonePresentation(.simulators)) {
            // A stack, so the folded Duo's bar can carry the tool's selected button.
            NavigationStack { // theme-exempt: SimulatorSessionView draws theme.computerBackground
                SimulatorSessionView(model: model, chat: chat, sessions: toolSessions)
            }
        }
        .modifier(PhotoAttachmentPicker(model: model, chat: chat, isPresented: $selectingPhoto, scope: importScope))
        .sheet(isPresented: $showingCamera, onDismiss: { cameraScope = nil }) {
            if let cameraScope {
                CameraCaptureView(
                    chatID: chat.id,
                    chatTitle: chat.title,
                    originatingScope: cameraScope,
                    currentContextToken: cameraContextToken,
                    attachVideo: { url in
                        guard cameraScope == model.assignmentScope else { return .cancelled }
                        await model.stageChosenMedia([{ try await MediaPreparation.video(url) }], chat: chat)
                        return model.controlErrors[chat.id] == nil ? .attached : .failed
                    },
                    attachPhoto: { data in await model.stageCameraPhoto(data, chat: chat, scope: cameraScope) }
                )
            }
        }
        .sheet(item: $sketchRequest) { request in
            SketchSheet(contextToken: model.cameraContextID.uuidString + ":" + model.assignmentScope + ":" + (model.visibleChat?.id ?? "")) { png in
                await model.stageSketch(png, chat: chat, scope: request.scope)
            }
        }
        .sheet(item: $annotationEdit) { attachment in
            ArtifactNoteEditor(model: model, chat: chat, attachment: attachment) { annotation in
                annotationEdit = nil
                Task { @MainActor in
                    await Task.yield()
                    showFiles(WorkspaceBrowserRequest(initialFilePath: annotation.path,
                        preferredRootID: annotation.rootId, reanchor: annotation,
                        replacingAnnotationID: attachment.id))
                }
            }
        }
        .fullScreenCover(item: $messagePhotoGallery) { gallery in
            PhotoViewer(model: model, chat: chat, galleryFiles: gallery.files, initialFileID: gallery.initialFileID)
        }
        .fullScreenCover(item: $composerPhoto) { attachment in
            PhotoViewer(
                model: model,
                chat: chat,
                name: attachment.name,
                mimeType: attachment.mimeType,
                sourceID: attachment.id,
                localData: attachment.data,
                file: attachment.remoteFile
            )
        }
        .fileImporter(isPresented: $importing, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            guard importScope == model.assignmentScope, !model.accessEnded else { return }
            if case .success(let urls) = result {
                fileImportTask?.cancel()
                fileImportTask = Task { await model.stage(urls, chat: chat) }
            } else if case .failure = result { model.controlErrors[chat.id] = "The file could not be opened. Try selecting it again." }
        }
        .pillSheet(item: subagentPresentation, wide: toolLayout == .regular) { child in
            ProjectSubagentTranscriptView(model: model, parent: chat, child: child)
                .modifier(PillSheet(wide: toolLayout == .regular))
        }
        // Opens at half height over the conversation and can be pulled up. Each
        // opening reattaches to the shell, which outlives the sheet on the computer.
        .pillSheet(isPresented: phonePresentation(.terminal), wide: toolLayout == .regular) {
            TerminalSheet(model: TerminalSessionModel(conversationID: chat.id,
                                                      folderName: model.projectDetail(chat)?.workingFolderName ?? "Project",
                                                      transport: model.terminalTransport()))
                .modifier(PillSheet(wide: toolLayout == .regular))
        }
        // Presented from here, not from the pill, so it stays live (and open)
        // whether the pill sits in the row or in a folded Duo's vertical bar.
        .pillSheet(isPresented: phonePresentation(.pullRequests), wide: toolLayout == .regular) {
            codeChangesSheet(replyEdits: replyTimeline.latestReplyEdits, lastReplyChanged: replyTimeline.lastReplyChanged)
        }
        .pillSheet(item: replyChangesPresentation, wide: toolLayout == .regular) { request in
            ReplyChangesView(edits: replyTimeline.replyChanges(turnID: request.turnID))
        }
        trackingToolPresentation(AnyView(presented))
        .navigationBarTitleDisplayMode(.inline)
        #if WONDER_DIAGNOSTICS
        .onChange(of: model.attention.count, initial: true) { _, count in
            if count > 0 && ProcessInfo.processInfo.arguments.contains("-diagnostics-vault-prompt") { showingAttention = true }
        }
        #endif
        .onChange(of: pendingRequestIDs, initial: true) { _, ids in
            openNewRequests(ids)
        }
        .onChange(of: model.attentionFocus, initial: true) { _, focus in
            // Opened from an in-app banner for a request: show the request.
            guard focus == chat.id else { return }
            model.attentionFocus = nil
            // The review replaces the timeline; the chat behind it is left at
            // the request's turn, ready when the review closes.
            if let target = model.attentionRequestFocus {
                let request = model.requests(for: chat).first { $0.id == target }
                let question = model.asyncQuestions[chat.id]?.first { $0.id == target }
                if let entry = prepared.feedIndex.entry(row: question?.rowId, turn: request?.params.turnId ?? question?.turnId) {
                    requestedScrollID = entry
                }
            }
            dismissTool(.files)
            attentionTapGuardUntil = nil
            showingAttention = true
        }
        .environment(\.inlineMediaOwner, mediaOwner)
        .onDisappear {
            if isCurrentToolOwner {
                voiceScene?.companionPaneOpen = false
                voiceScene?.companionPaneExpanded = false
            }
            model.dismissConversation(chat)
            loopbackLinkTask?.cancel()
            browser.close()
            imagePasteTask?.cancel()
            fileImportTask?.cancel()
            model.dictation.captureControlsHidden(conversationID: chat.id)
            InlineMediaPlayback.shared.stop(owner: mediaOwner)
            SpeechReader.shared.conversationClosed(chat.id)
            if model.searchFocus?.conversationID == chat.id { model.searchFocus = nil }
        }
        .onChange(of: conversationCovered) { _, covered in
            if covered {
                model.dictation.captureControlsHidden(conversationID: chat.id)
            }
        }
        .onChange(of: model.goals[chat.id] != nil) { _, exists in
            if !exists { showingGoal = false }
        }
        .onChange(of: model.assignmentScope) { _, next in
            composerModelPresentation = ComposerModelPresentation()
            loopbackLinkTask?.cancel()
            browser.close()
            if cameraScope != nil, cameraScope != next { showingCamera = false; cameraScope = nil }
            InlineMediaPlayback.shared.stop(owner: mediaOwner, forgetPositions: true)
        }
        // The browser shows this chat's pages through this pairing only.
        .onChange(of: chat.id) { _, _ in
            composerModelPresentation = ComposerModelPresentation()
            loopbackLinkTask?.cancel()
            browser.close()
            InlineMediaPlayback.shared.stop(owner: mediaOwner)
        }
        .onChange(of: model.accessEnded) { _, ended in
            if ended { browser.close(); InlineMediaPlayback.shared.stop(owner: mediaOwner, forgetPositions: true) }
        }
        // Chat audio and video started in this window stop once it leaves the
        // screen; another window's keep playing.
        .onChange(of: phase) { _, next in
            if next == .background { InlineMediaPlayback.shared.stop(owner: mediaOwner) }
        }
        .onReceive(model.projects.$details) { details in
            let on = details[chat.id]?.planMode == true
            if planModeOn != on { planModeOn = on }
            let notice = details[chat.id]?.notice
            if projectNotice != notice { projectNotice = notice }
            let resumes = details[chat.id]?.resumesAt
            if resumesAt != resumes { resumesAt = resumes }
        }
        .onReceive(model.projects.$threads) { _ in noteMacPermissionWait() }
        .onReceive(model.projects.$pinned) { _ in noteMacPermissionWait() }
        .onReceive(model.$computerWaitReads) { _ in noteMacPermissionWait() }
        // A chat opened without a loaded row (a notification for a Project the
        // sidebar hasn't expanded) reads whether it waits on the computer itself.
        .task(id: "\(chat.id):\(sidebarListsThread):\(String(describing: model.macConnected)):\(phase == .active)") {
            guard !readOnly, !sidebarListsThread, model.macConnected == true, phase == .active, model.isProject(chat) else { return }
            await model.followComputerWait(chat)
        }
        .onChange(of: model.computerRequest, initial: true) { _, _ in openRequestedComputer() }
        .onChange(of: threadWaitsOnMac) { _, _ in openRequestedComputer() }
        .onReceive(model.projects.$chatDetail) { next in
            if chatDetail != next { chatDetail = next }
        }
        // The Mac sends thinking text only at the Thinking level, so a chat that
        // starts showing it reads its history again.
        .onChange(of: chatDetail?.detail(for: model.agentFamily(chat), chat: chat.id) == .thinking) { _, shows in
            guard shows, !readOnly, model.isProject(chat) else { return }
            Task { await model.reloadNativeHistory(chat) }
        }
        .onReceive(model.projects.$supportsModes) { supported in
            if supportsModes != supported { supportsModes = supported }
        }
        .task(id: chat.id) {
            async let history: Void = refreshNativeHistory()
            await model.open(chat, root: rootChat, readOnly: readOnly)
            await history
            if model.previewMode, ProcessInfo.processInfo.arguments.contains("-preview-document") { showFiles(WorkspaceBrowserRequest()) }
            #if WONDER_DIAGNOSTICS
            // Opens a tool beside the chat at launch, so the layout can be captured
            // where input is unavailable (the unfolded Duo simulator, #387).
            let arguments = ProcessInfo.processInfo.arguments
            if arguments.contains("-diagnostics-lazy-output-expanded") {
                expandedDetails.insert(DiagnosticLazyOutputFixture.turn + "/" + DiagnosticLazyOutputFixture.item)
            }
            if arguments.contains("-diagnostics-companion-files") { openTool(.files(WorkspaceBrowserRequest())) }
            else if arguments.contains("-diagnostics-companion-terminal") { openTool(.terminal) }
            else if arguments.contains("-diagnostics-companion-computer") { openTool(.computer) }
            else if arguments.contains("-diagnostics-companion-simulators") { openTool(.simulators) }
            else if arguments.contains("-diagnostics-companion-pull-requests") { openTool(.pullRequests) }
            else if arguments.contains("-diagnostics-companion-details") { openTool(.details) }
            else if arguments.contains("-diagnostics-companion-agent-task") {
                await model.loadProjectSubagents(chat)
                if let child = model.projectSubagents[chat.id]?.first { openTool(.agentTask(child)) }
            }
            else if arguments.contains("-diagnostics-companion-reply-changes"),
                    let edits = model.timeline(for: chat, focusedRowID: nil).latestReplyEdits {
                openTool(.replyChanges(turnID: edits.turnID))
            }
            if arguments.contains("-diagnostics-companion-expanded") { changeTools(.expand(true)) }
            #endif
        }
        .modifier(RunningElsewhereRefresh(model: model, chat: chat, readOnly: readOnly))
        .task(id: chat.id + ":" + (model.agentFamily(chat)?.rawValue ?? "") + ":" + String(phase == .active)) {
            guard phase == .active, !readOnly, model.isProject(chat), model.capabilities(chat)?.goals == true else { return }
            while !Task.isCancelled {
                await model.loadGoal(chat)
                // A running goal moves on by itself; one set on the Mac shows within 15 s.
                try? await Task.sleep(for: .seconds(model.goals[chat.id]?.status == "active" ? 5 : 15))
            }
        }
        .task(id: chat.id + String(phase == .active)) {
            guard phase == .active, !readOnly, model.isProject(chat) else { return }
            await model.loadChatNotifications([chat.id], refreshing: true)
        }
        .modifier(ComposerUsageMonitor(model: model, chat: chat, readOnly: readOnly))
        .task(id: nextQuestionExpiry) {
            guard let expiry = nextQuestionExpiry else { return }
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            if expiry > now {
                do { try await Task.sleep(for: .milliseconds(expiry - now + 1)) } catch { return }
            }
            questionExpiryTick &+= 1
        }
        .task(id: attachmentMetadataRequest) {
            let request = attachmentMetadataRequest
            guard !request.ids.isEmpty else { return }
            await model.loadFilesIfNeeded(chat, attachmentIDs: request.ids)
            guard !Task.isCancelled, request.scope == model.assignmentScope else { return }
            resolvedAttachmentMetadata = request
        }
    }
    private var voiceAvailable: Bool {
        !model.accessEnded && (model.macConnected == true || model.previewMode)
    }

    private func startVoice() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        model.dictation.captureControlsHidden(conversationID: chat.id)
        if model.voice.isActive { model.voice.showTarget(in: voiceScene) }
        else { model.voice.start(chat: chat, scene: voiceScene) }
    }

    private var composerInputControls: some View {
        HStack(spacing: 0) {
            DictationButton(controller: model.dictation, chat: chat,
                unavailable: model.previewMode || model.accessEnded || model.preparingSends.contains(chat.id) || model.uploading.contains(chat.id))
            ComposerVoiceButton(unavailable: !voiceAvailable, start: startVoice)
        }
    }

    #if WONDER_DIAGNOSTICS
    /// `-diagnostics-dictation-stream`: `wonderctl signal dictation-stream-N` shows the Nth
    /// cumulative transcript through the real editor and controller, as speech results would.
    /// No microphone or network; the first signal begins the capture.
    private func runDictationStreamFixture() async {
        guard ProcessInfo.processInfo.arguments.contains("-diagnostics-dictation-stream") else { return }
        let words = (1...60).map { "word\($0)" }
        var pending: DictationIntent?
        for step in 1...6 {
            while !DiagnosticSubagentFixture.signalled("dictation-stream-\(step)") {
                do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
            }
            if pending == nil { pending = model.dictation.beginNativeFixture(chat: chat) }
            guard let pending else { continue }
            // 10 more words per step: each step adds a wrapped line or more.
            model.dictation.receiveNativeFixture(words.prefix(step * 10).joined(separator: " "), pending: pending)
        }
    }

    private var voiceRailPrototypeEnabled: Bool {
        diagnosticsVoiceRail && pillsInBar && !readOnly &&
            (model.showsContextButton(chat) || composerShowsPills || computerRail.session != nil)
    }

    private var voiceRailPrototypeControls: [DiagnosticTestDriver.Control] {
        guard voiceRailPrototypeEnabled else { return [] }
        return [.init(id: "voice-rail-entry", label: model.voice.isActive ? model.voice.targetLabel : "Talk with voice",
                      enabled: model.voice.isActive || voiceAvailable, tap: {
            startVoice()
        })]
    }

    private func driverSurface(nodes: [ChatFeedNode]) -> DiagnosticTestDriver.Surface {
        .init(priority: 10, active: { model.visibleChat?.id == chat.id || isCurrentToolOwner }, state: {
            let visible = DiagnosticTestDriver.shared.visibleTimeline[chat.id] ?? []
            let items: [[String: Any]] = nodes.filter { visible.contains($0.id) }.prefix(200).map { node in
                let rows: [ReadRow]
                switch node.content {
                case .entry(let entry):
                    if entry.isActivity {
                        return ["id": node.id, "type": "activity", "summary": ChatFeedEntry.actionSummary(rows: entry.rows) ?? "Working"]
                    }
                    rows = entry.rows
                case .activity(let row), .thinking(let row):
                    return ["id": node.id, "type": "activity", "summary": row.activitySummary?.title ?? "Working"]
                case .commands(let run): return ["id": node.id, "type": "activity", "summary": run.label]
                case .compaction(let row): rows = [row]
                case .file(let file): return ["id": node.id, "type": "file", "name": file.name]
                case .gallery(let files): return ["id": node.id, "type": "gallery", "names": files.map(\.name)]
                case .updatedPage(let file): return ["id": node.id, "type": "updatedPage", "name": file.name]
                case .outcome(let outcome):
                    return ["id": node.id, "type": "outcome", "label": outcome.label(isProjectConversation: model.isProject(chat)),
                            "reason": outcome.reason ?? ""]
                case .question(let question): return ["id": node.id, "type": "question", "state": question.state]
                }
                return ["id": node.id, "rows": rows.filter { $0.item?.type != "reasoning" }.prefix(20).map {
                    ["id": $0.id, "text": String($0.text.prefix(4096)), "isUser": $0.isUser] as [String: Any]
                }]
            }
            let sheets = [(showingCamera, "camera"),
                          (selectingPhoto, "photos"), (importing, "files"), (showingComputer, "computer"), (showingSimulators, "simulators"),
                          (showingTerminal, "terminal"), (showingChanges, "changes"), (replyChangesRequest != nil, "reply-changes"), (showingDetails, "details"),
                          (showingContextUsage, "context-usage"), (showingGoal, "goal"), (selectedProjectSubagent != nil, "subagent"),
                          (composerPhoto != nil || messagePhotoGallery != nil, "photo-viewer"),
                          (showingProjectSubagents, "agent-tasks"),
                          (annotationEdit != nil, "annotation")].filter { $0.0 }.map { $0.1 }
            let intent = model.composers[chat.id]
            let outbox: [[String: Any]] = (intent?.outbox ?? []).map { item in
                ["text": item.body, "attachmentHashes": (item.stagedFiles ?? []).map(\.sha256)]
            }
            let composerState: [String: Any] = [
                "text": intent?.draft ?? "", "canSend": !readOnly && primarySendAvailable,
                "placeholder": FollowUpBehavior.placeholder(queues: placeholderQueues), "steers": sendGuides,
                "attachmentCount": intent?.attachmentCount ?? 0,
                "attachmentHashes": (intent?.stagedFiles ?? []).map(\.sha256),
                "recovery": model.controlErrors[chat.id] ?? "", "outbox": outbox]
            return ["route": "conversation", "thread": ["id": chat.id, "title": chat.title], "timeline": items,
                    "voice": model.voice.diagnosticState,
                    "voiceRail": ["enabled": voiceRailPrototypeEnabled, "panelOpen": false],
                    "composer": composerState,
                    "sheets": sheets, "alerts": rewindRequest == nil ? [] : ["rewind"],
                    // Compatibility keys for scenario assertions; no browser panel state remains.
                    "panels": [],
                    "toolLayout": tools.layout == .regular ? "regular" : "compact",
                    "toolWindow": toolWindowBounds.map { ["width": $0.width, "height": $0.height] } as Any? ?? NSNull(),
                    "browser": NSNull(),
                    "semanticMenus": choosingPreviewPage ? ["preview-pages-menu"] : [],
                    "pane": companion.map { ["tool": $0.kind.rawValue, "expanded": companionExpanded] as [String: Any] } ?? NSNull()]
        }, controls: {
            let contextControl = DiagnosticTestDriver.Control(id: "context-usage-ring", label: "Context window",
                value: showingContextUsage ? "Expanded" : "Collapsed", tap: { showingContextUsage.toggle() })
            let goalControl = DiagnosticTestDriver.Control(id: "goal-status-pill", label: "Goal",
                value: showingGoal ? "Expanded" : "Collapsed", tap: { showingGoal.toggle() })
            // Anchored rail tools retain their own toggle while presented.
            if pillsInBar, showingContextUsage { return [contextControl] }
            if pillsInBar, showingGoal { return [goalControl] }
            // A task popover leaves its own visible rail button available to close it.
            if pillsInBar, showingProjectSubagents {
                return [.init(id: "project-subagent-status-pill", label: "Agent tasks", value: "Expanded",
                              tap: { showingProjectSubagents.toggle() })]
            }
            // Expanded panes retain their visible closing toggle even though
            // the conversation and its composer are off screen.
            if companionExpanded, let tool = companion, let pill = tool.pill {
                let id: String
                switch tool {
                case .computer: id = "computer-status-pill"
                case .simulators: id = "conversation-simulators-pill"
                case .files: id = "conversation-files-pill"
                case .terminal: id = "conversation-terminal-pill"
                case .agentTask: id = "project-subagent-status-pill"
                case .pullRequests, .details, .replyChanges: return []
                }
                return [.init(id: id, label: pill.title, value: "Open", tap: closeCompanion)]
            }
            // A covered conversation must never satisfy a selector or receive input.
            guard !conversationCovered, !showingTerminal, !showingChanges, replyChangesRequest == nil else { return [] }
            let voiceControls = model.voice.diagnosticControls(scene: voiceScene, compact: model.voice.chatID != chat.id)
                + (voiceScene.map { scene in
                    [DiagnosticTestDriver.Control(id: "open-sidebar", label: "Show Chats", tap: { scene.showChats() })]
                } ?? []) + voiceRailPrototypeControls
            guard !model.voice.isActive || model.voice.chatID != chat.id else { return voiceControls }
            // Send's slot holds Stop while a reply runs and there is nothing to
            // send, and Stop beside Send while there is.
            let trailing = ComposerTrailingButton.resolve(composer: model.composers[chat.id],
                hearsWords: model.dictation.hasUncommittedWords(conversationID: chat.id),
                preparing: preparingSend || model.sending.contains(chat.id), offersStop: model.offersStop(chat))
            var controls = [DiagnosticTestDriver.Control(id: "message-draft", label: "Message \(chat.title)",
                                value: model.composers[chat.id]?.draft ?? "", enabled: !readOnly)] + voiceControls
            if let notice = composerHostNotice { controls.append(.init(id: notice.id, label: notice.text)) }
            if offlineNoticeShown != nil {
                controls.append(.init(id: "composer-reconnect", label: "Reconnect", enabled: !model.checkingConnection,
                                      tap: { Task { await model.check(renew: true, userInitiated: true) } }))
            }
            if let error = model.controlErrors[chat.id] { controls.append(.init(id: "composer-action-issue", label: error)) }
            if !model.voice.isActive {
                controls.append(.init(id: "voice-start", label: "Talk with voice",
                                      enabled: voiceAvailable, tap: { startVoice() }))
            }
            if model.showsContextButton(chat) { controls.append(contextControl) }
            if presentedPillLayout.visible.contains(.goal), offersGoal, model.goals[chat.id] != nil { controls.append(goalControl) }
            if trailing.showsStop {
                controls.append(.init(id: "stop-response", label: "Stop reply",
                                      enabled: !readOnly && !model.stopping.contains(chat.id) && !model.accessEnded,
                                      tap: { Task { await model.stop(chat) } }))
            }
            if trailing.showsSend {
                controls.append(.init(id: "send-message", label: sendQueues ? "Queue message" : "Send message", enabled: !readOnly && primarySendAvailable,
                                      tap: { primarySend() }))
            }
            if presentedPillLayout.visible.contains(.computer) {
                let notice = PushNotifications.shared.macDialogs[model.connection?.credential.hostInstallationId ?? ""]
                let attention = notice.map {
                    ", " + MacDialogNotice.countLabel($0.count) + ($0.dialogs == nil ? "" : ". " + $0.summary)
                } ?? ""
                controls.append(.init(id: "computer-status-pill", label: "View computer",
                                      value: (toolOpen(.computer) ? "Open" : "Closed") + attention,
                                      tap: { openTool(.computer, toggle: true) }))
            }
            if presentedPillLayout.visible.contains(.simulators), offersSimulators || toolOpen(.simulators) {
                controls.append(.init(id: "conversation-simulators-pill", label: "Simulators",
                                      value: toolOpen(.simulators) ? "Open" : "Closed",
                                      tap: { openTool(.simulators, toggle: true) }))
            }
            if presentedPillLayout.visible.contains(.files) {
                controls.append(.init(id: "conversation-files-pill", label: "Files",
                                      value: toolOpen(.files) ? "Open" : "Closed", tap: { toggleFiles() }))
            }
            if presentedPillLayout.visible.contains(.terminal), offersTerminal || toolOpen(.terminal) {
                controls.append(.init(id: "conversation-terminal-pill", label: "Terminal",
                                      value: toolOpen(.terminal) ? "Open" : "Closed",
                                      tap: { openTool(.terminal, toggle: true) }))
            }
            if presentedPillLayout.visible.contains(.agentTasks), offersAgentTasks || toolOpen(.agentTask) {
                controls.append(.init(id: "project-subagent-status-pill", label: "Agent tasks",
                                      value: companion?.kind == .agentTask ? "Open" : showingProjectSubagents ? "Expanded" : "Collapsed",
                                      tap: {
                                          if companion?.kind == .agentTask { closeCompanion() }
                                          else { showingProjectSubagents.toggle() }
                                      }))
            }
            if let presence = model.devServerPresence(chat) {
                controls.append(.init(id: "preview-pages-menu", label: "Preview, \(presence.servers.count) pages running",
                                      tap: { choosingPreviewPage = true }))
                if choosingPreviewPage {
                    controls += presence.servers.map { server in
                        .init(id: "preview-page-\(server.port)", label: DevServerMenu.title(server), tap: {
                            choosingPreviewPage = false
                            openPreview(server)
                        })
                    }
                    controls.append(.init(id: "preview-check-pages", label: "Check for pages", tap: {
                        choosingPreviewPage = false
                        Task { await model.loadDevServers(chat) }
                    }))
                }
            }
            // These fixture controls invoke the same route as a reply's native link.
            // Native hit targets remain covered separately on iPhone/iPad;
            // Duo uses these semantic controls while native input is limited by #387.
            if ProcessInfo.processInfo.arguments.contains("-dev-server-preview") {
                for (id, text) in [("localhost", "http://localhost:5173/?view=plan"),
                                   ("stopped", "http://localhost:3000/docs"),
                                   ("web", "https://example.com")] {
                    controls.append(.init(id: "fixture-link-" + id, label: text, tap: {
                        guard let url = URL(string: text) else { return }
                        LoopbackLinkRouting.route(url, open: model.previewEndpoint(chat) == nil ? nil : { openLoopbackLink($0) },
                                                  openWebLink: { openWebLink($0) })
                    }))
                }
            }
            return controls
        }, actions: [
            "dismiss": { _ in
                guard companion != nil || conversationCovered || showingTerminal || showingChanges || replyChangesRequest != nil || rewindRequest != nil || annotationEdit != nil else {
                    throw DiagnosticTestDriver.DriverError("No presentation to dismiss")
                }
                closeToolNow()
                importing = false; selectingPhoto = false; showingCamera = false
                composerPhoto = nil; messagePhotoGallery = nil
                showingProjectSubagents = false; showingGoal = false
                rewindRequest = nil; annotationEdit = nil
            },
            "type": { arguments in
                guard !readOnly, !conversationCovered, !showingTerminal, !showingChanges, replyChangesRequest == nil,
                      !model.voice.isActive || model.voice.chatID != chat.id,
                      !model.preparingSends.contains(chat.id), !model.uploading.contains(chat.id) else {
                    throw DiagnosticTestDriver.DriverError("Composer is unavailable")
                }
                model.editDraft((model.composers[chat.id]?.draft ?? "") + (arguments["text"] ?? ""), chat: chat.id)
            },
            "send": { _ in
                guard !readOnly, !conversationCovered, !showingTerminal, !showingChanges, replyChangesRequest == nil,
                      !model.voice.isActive || model.voice.chatID != chat.id, primarySendAvailable else { throw DiagnosticTestDriver.DriverError("Send is unavailable") }
                // The same action as the Send button, so the owner's queue/steer choice applies.
                primarySend()
            },
            "open-thread": { arguments in
                guard let id = arguments["id"], !id.isEmpty else { throw DiagnosticTestDriver.DriverError("Missing thread ID") }
                // External navigation belongs to the shell even while a tool covers
                // its chat, as a notification or chat link does in the product.
                if let voiceScene, let host = model.connection?.credential.hostInstallationId {
                    voiceScene.open(host: arguments["host"] ?? host, conversation: id)
                    return
                }
                guard !conversationCovered, !showingTerminal, !showingChanges, replyChangesRequest == nil else { throw DiagnosticTestDriver.DriverError("Conversation is covered") }
                if id != chat.id {
                    guard let openConversation else { throw DiagnosticTestDriver.DriverError("This fixture has no navigation owner") }
                    openConversation(id)
                }
            }
        ])
    }
    #endif

    @ToolbarContentBuilder
    private func conversationToolbar(replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool) -> some ToolbarContent {
            if !inlineConversationHeader {
                ToolbarItem(placement: .principal) {
                    Group {
                        if readOnly { conversationHeader(room: headerGeometry.titleRoom) }
                        else { conversationTitleMenu(replyEdits: replyEdits, lastReplyChanged: lastReplyChanged) }
                    }
                    // Navigation-bar titles stop scaling at large sizes, as the system's do;
                    // at the largest accessibility size the name would truncate to a few letters.
                    .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
                }
                // The title pill draws its own glass (TitleSurface); the system's shared
                // background would sit behind it as a second layer, and behind a read-only title.
                .sharedBackgroundVisibility(.hidden)
            }
            // Horizontal headers keep their native context item; the Duo's
            // rail groups independent buttons with explicit spacing below.
            if !pillsInBar, !readOnly, model.showsContextButton(chat) {
                ToolbarItem(placement: .topBarTrailing) {
                    ContextUsageButton(usage: model.nativeContextUsage[chat.id], showing: $showingContextUsage)
                        { contextUsagePanel }
                        .modifier(NavigationBarItemExtent { headerGeometry.trailingItem = $0 })
                        .onDisappear { headerGeometry.trailingItem = nil }
                }
            }
            if pillsInBar, !readOnly, model.showsContextButton(chat) || composerShowsPills || computerRail.session != nil {
                ToolbarItem(id: "conversation-rail-\(barEpoch)", placement: .topBarTrailing) {
                    DuoRailControls(selection: railSelection, spacing: railSpacing) {
                        VStack(spacing: railSpacing) {
                            #if WONDER_DIAGNOSTICS
                            if voiceRailPrototypeEnabled {
                                DiagnosticVoiceRailPrototype(controller: model.voice, scene: voiceScene,
                                    available: voiceAvailable, start: startVoice)
                                    .id("rail-voice-prototype")
                            }
                            #endif
                            if model.showsContextButton(chat) {
                                ContextUsageButton(usage: model.nativeContextUsage[chat.id], showing: $showingContextUsage, inRail: true)
                                    { contextUsagePanel }
                                    .id("rail-context")
                            }
                            if composerShowsPills {
                                composerPills(replyEdits: replyEdits, lastReplyChanged: lastReplyChanged, inBar: true)
                            }
                            if let session = computerRail.session {
                                ComputerRailButtons(model: session, agentActive: computerRail.agentActive)
                            }
                        }
                        .labelStyle(.iconOnly)
                        .environment(\.composerPillsInBar, true)
                    }
                }
                .duoRail(true)
            }
    }

    private var railSpacing: CGFloat { DuoRailLayout.spacing }

    private var contextUsagePanel: some View {
        ContextUsagePanel(model: model, chat: chat)
    }

    private var railSelection: String? {
        if showingProjectSubagents { return ComposerPill.agentTasks.id }
        if showingGoal { return ComposerPill.goal.id }
        if showingContextUsage { return "rail-context" }
        if showingAttention { return ComposerPill.attention.id }
        switch tools.tool?.kind {
        case .computer: return ComposerPill.computer.id
        case .simulators: return ComposerPill.simulators.id
        case .files: return ComposerPill.files.id
        case .terminal: return ComposerPill.terminal.id
        case .pullRequests: return ComposerPill.pullRequests.id
        case .agentTask: return ComposerPill.agentTasks.id
        default: return nil
        }
    }

    /// `room` is the width the bar leaves between its buttons. The bar sizes
    /// this item (and a menu's label) from its own content and centres it
    /// without shrinking it, so the limit must be applied here, inside.
    @ViewBuilder private func conversationHeader(room: CGFloat) -> some View {
        if let detail = model.projectDetail(chat) {
            ProjectConversationHeader(detail: detail, currentStep: waitingOnMacPermission
                                      ? ProjectThreadSummary.macPermissionWait : model.currentStep(chat.id),
                                      computer: model.macName, pairedComputers: pairedComputerCount,
                                      isDemo: model.isDemo, notification: model.chatNotifications.level(chat.id),
                                      notificationSaving: model.chatNotifications.isSaving(chat.id),
                                      room: room)
        } else {
            HStack(spacing: 7) {
                if let family = model.agentFamily(chat) {
                    ProviderIcon(family: family, size: 24).accessibilityHidden(true)
                }
                Text(chat.title).font(.headline).lineLimit(1)
            }
            .frame(maxWidth: room)
        }
    }
    /// Copies what is loaded on this device and says so when earlier history is not.
    private func copyAsMarkdown() {
        let output = ConversationMarkdown.render(
            title: chat.title,
            agentName: (model.agentFamily(chat)?.title ?? "Agent"),
            rows: model.timeline(for: chat, focusedRowID: nil).rows,
            isPartial: model.snapshots[chat.id]?.thread.nextCursor != nil
        )
        guard output.messageCount > 0 else { notice = "Nothing to copy yet"; return }
        UIPasteboard.general.string = output.markdown
        notice = output.confirmation
    }
    /// Fork is offered on a started, idle Project thread the owner can write to.
    private var canFork: Bool {
        guard !readOnly, openConversation != nil, let detail = model.projectDetail(chat) else { return false }
        return ConversationFork.canForkLatest(nativeFork: model.capabilities(chat)?.nativeFork == true,
                                              hasNativeSession: detail.hasNativeSession,
                                              hasActiveTurn: model.conversationWorking(chat.id),
                                              isArchived: detail.isArchived == true)
    }
    /// The turn a finished agent reply can be forked at, if this reply has one.
    private func forkTurnID(for row: ReadRow) -> String? {
        guard canFork else { return nil }
        let finished = row.turnId.flatMap { model.turn($0, in: chat.id) }?.isInProgress == false
        return ConversationFork.turnID(of: row, turnFinished: finished)
    }
    /// The reply whose changed files the phone-width sheet lists.
    struct ReplyChangesRequest: Identifiable {
        let turnID: String
        var id: String { turnID }
    }
    /// The owner's message, as Edit from here puts it back in the draft.
    struct RewindRequest: Identifiable {
        let id = UUID()
        let turnID: String
        let body: String
        let attachmentIDs: [String]
        let agent: String
        let family: AgentFamily?
        /// The provider restores files on rewind, so the backup setting matters.
        let canRestoreFiles: Bool
        var backups: FileBackups
        var confirmation: ConversationRewind.Confirmation {
            ConversationRewind.confirmation(agent: agent, canRestoreFiles: canRestoreFiles, backups: backups)
        }
    }
    /// Edit from here is offered on the owner's sent messages of a started,
    /// idle Project thread whose provider rewinds natively.
    private func rewindTurnID(for row: ReadRow, isPending: Bool) -> String? {
        guard !readOnly, !rewinding, !forking, !model.accessEnded, !model.previewMode, row.isUser,
              let detail = model.projectDetail(chat),
              ConversationRewind.canRewind(rewind: model.capabilities(chat)?.rewind == true,
                                           hasNativeSession: detail.hasNativeSession,
                                           hasActiveTurn: model.conversationWorking(chat.id),
                                           isArchived: detail.isArchived == true) else { return nil }
        return ConversationRewind.turnID(of: row, isPending: isPending)
    }
    /// Asks before rewinding, saying exactly what changes. File restore is
    /// offered only where the provider restores files and its backups are on;
    /// until the Mac says, the confirmation says it is checking (or that it
    /// could not), never that backups are off.
    private func askToRewind(_ row: ReadRow, turnID: String) {
        guard !rewinding else { return }
        if model.unconfirmedRewind(chat.id) != nil {
            notice = "Check the earlier edit first. Your message is kept in this chat."
            return
        }
        guard model.canRestoreRewound(chat.id) else {
            notice = "Wait for your draft to finish sending, then try again."
            return
        }
        let family = model.agentFamily(chat)
        let restores = model.capabilities(chat)?.restoresFilesOnRewind == true && family != nil
        let request = RewindRequest(turnID: turnID, body: row.text, attachmentIDs: row.attachmentIds,
                                    agent: family?.title ?? "Agent", family: family, canRestoreFiles: restores,
                                    backups: restores ? .loading : .disabled)
        guard restores else { rewindRequest = request; return }
        let scope = model.assignmentScope
        Task { await presentRewind(request, scope: scope) }
    }
    /// What the backup read has said so far; written and read on the main actor.
    private final class BackupRead { var result: FileBackups? }
    /// Shows the confirmation with the backup setting. A read that answers within
    /// a moment is shown as it is; a slower one opens as "checking" and is shown
    /// again with its answer (an open confirmation keeps what it was shown with).
    private func presentRewind(_ base: RewindRequest, scope: String) async {
        guard let family = base.family else { rewindRequest = base; return }
        let read = BackupRead()
        let reading = Task {
            do { read.result = FileBackups(settings: try await model.projects.fileRestoreSettings(), family: family) }
            catch is CancellationError {}
            catch { read.result = .failed }
        }
        try? await Task.sleep(for: .milliseconds(450))
        guard scope == model.assignmentScope, !model.accessEnded else { reading.cancel(); return }
        var request = base
        request.backups = read.result ?? .loading
        rewindRequest = request
        guard read.result == nil else { return }
        await reading.value
        guard scope == model.assignmentScope, var updated = rewindRequest, updated.id == request.id, let result = read.result else { return }
        updated.backups = result
        rewindRequest = nil
        try? await Task.sleep(for: .milliseconds(600))
        guard rewindRequest == nil, !rewinding, scope == model.assignmentScope, !model.accessEnded else { return }
        rewindRequest = updated
    }
    private func rewindChoiceID(_ choice: ConversationRewind.Choice) -> String {
        switch choice {
        case .restoreFiles: "rewind-restore-files"
        case .checkBackups: "rewind-check-backups"
        case .conversationOnly: "rewind-conversation"
        }
    }
    /// Rewinds on the Mac, then puts the message back in the draft. The message
    /// is saved first; when the Mac's answer is lost the chat is read again and
    /// history decides, so nothing is lost and nothing is sent twice.
    private func performRewind(_ request: RewindRequest, choice: ConversationRewind.Choice) {
        guard !rewinding else { return }
        if choice == .checkBackups {
            // The dialog closes with the tap; show it again, checking.
            let scope = model.assignmentScope
            var again = request
            again.backups = .loading
            Task { await presentRewind(again, scope: scope) }
            return
        }
        rewinding = true; notice = nil
        let scope = model.assignmentScope
        Task {
            defer { rewinding = false }
            let recovery: RewindRecovery
            do {
                recovery = try model.beginRewind(chat, turnID: request.turnID, body: request.body,
                                                 attachmentIDs: request.attachmentIDs, restoreFiles: choice == .restoreFiles)
            } catch ConnectionModel.RewindStartFailure.unresolved {
                notice = "Check the earlier edit first. Your message is kept in this chat."; return
            } catch ConnectionModel.RewindStartFailure.draftBusy {
                notice = "Wait for your draft to finish sending, then try again."; return
            } catch {
                notice = "Your message couldn’t be saved first, so the chat wasn’t changed. Free some storage and try again."; return
            }
            do {
                let response = try await model.projects.rewind(chat.id, turnID: request.turnID, restoreFiles: choice == .restoreFiles)
                guard scope == model.assignmentScope else { return }
                await model.finishRewind(chat, recovery: recovery)
                notice = ConversationRewind.outcome(response)
            } catch is CancellationError {
                // Left saved: the next read of this chat decides what happened.
            } catch {
                var status: Int?, detail: String?
                if case PairingFailure.response(let code) = error { status = code }
                if case PairingFailure.hostMessage(let code, let message) = error { status = code; detail = message }
                guard scope == model.assignmentScope else { return }
                switch ConversationRewind.disposition(status: status) {
                case .refused:
                    model.abandonRewind(chat, recovery: recovery)
                    notice = ConversationRewind.failureMessage(status: status, detail: detail, computer: model.macName)
                case .unconfirmed:
                    if status != nil { model.rewindAnswered(chat, recovery: recovery) }
                    notice = ConversationRewind.unconfirmedMessage(computer: model.macName)
                    await model.checkUnconfirmedRewind(chat)
                }
            }
        }
    }
    /// Reads the chat again for an edit the Mac never confirmed; the notice says where that stands.
    private func checkUnconfirmedRewind() async {
        guard !rewinding, !checkingRewind else { return }
        checkingRewind = true; defer { checkingRewind = false }
        await model.checkUnconfirmedRewind(chat)
        // Unresolved stays in the composer notice; a decided edit announces itself.
    }
    /// Creates the fork on the Mac, then opens it. The source is never changed.
    private func fork(from turnID: String?) {
        guard !forking, let openConversation else { return }
        forking = true; notice = nil
        let scope = model.assignmentScope
        Task {
            defer { forking = false }
            do {
                let id = try await model.projects.fork(chat.id, lastTurnID: turnID)
                guard scope == model.assignmentScope else { return }
                openConversation(id)
            } catch is CancellationError {
            } catch {
                var status: Int?
                if case PairingFailure.response(let code) = error { status = code }
                notice = ConversationFork.failureMessage(status: status, computer: model.macName)
            }
        }
    }
    /// What archiving changes, named for the provider when it archives there too.
    private var archiveConsequence: String {
        let restore = " You can restore it right away from this screen, or later from Archived chats."
        if let family = model.projectDetail(chat)?.family, let alsoIn = model.projects.archiveAlsoIn(family) {
            return "Cupola will also ask the \(alsoIn) app to archive it when it uses your computer’s account." + restore
        }
        return "It will be archived in Cupola only." + restore
    }
    private func archive() {
        guard !archiving, let archiveConversation else { return }
        // The thread may have started working while the confirmation was open.
        guard !model.archiveWaitsForWork(chat.id) else {
            notice = "The chat is working. Archive it when it finishes."
            return
        }
        archiving = true; notice = nil
        let scope = model.assignmentScope
        Task {
            defer { archiving = false }
            do { try await archiveConversation() }
            catch is CancellationError {}
            catch {
                guard scope == model.assignmentScope else { return }
                notice = "The chat couldn’t be archived. " + managementError(error)
            }
        }
    }
    private func deleteChat() {
        guard !deleting, !archiving else { return }
        guard !model.archiveWaitsForWork(chat.id) else {
            notice = "The chat is working. Stop it or wait for it to finish before deleting."
            return
        }
        deleting = true; notice = nil
        let scope = model.assignmentScope
        Task {
            defer { deleting = false }
            do {
                try await model.projects.delete(conversation: chat.id)
                deletedConversation?()
            }
            catch is CancellationError {}
            catch {
                guard scope == model.assignmentScope else { return }
                notice = "The chat couldn’t be deleted. " + managementError(error)
            }
        }
    }
    /// Shows `level` in this chat only; nil follows the provider again.
    private func setChatDetail(_ level: ChatDetailLevel?) {
        let scope = model.assignmentScope
        notice = nil
        Task {
            do { try await model.projects.setChatDetail(chat: chat.id, to: level) }
            catch is CancellationError {}
            catch {
                guard scope == model.assignmentScope else { return }
                notice = "What this chat shows couldn’t be saved. Check \(model.macName) and try again."
            }
        }
    }
    private var conversationTitleAccessibilityLabel: String {
        if let detail = model.projectDetail(chat) {
            let worktree = detail.worktreeLabel.map { ", worktree " + $0 } ?? ""
            return "\(detail.title), \(detail.projectName) Project, \(detail.family.title)\(worktree), chat options"
        }
        return "\(chat.title), \((model.agentFamily(chat)?.title ?? "Agent")), chat options"
    }
    /// Where the thread runs. VoiceOver names the computer even when the
    /// header's second line leaves it out.
    private var conversationPlaceValue: String {
        model.projectDetail(chat) == nil ? "" : "On " + model.macName
    }
    /// Native menu choices keep selection beside the action, without a second dialog.
    private var notificationsSection: some View {
        let level = model.chatNotifications.level(chat.id)
        return Section {
            Menu {
                Picker("Notifications", selection: Binding(get: {
                    model.chatNotifications.level(chat.id)
                }, set: { level in
                    Task {
                        if let message = await model.setChatNotification(chat, to: level) { notice = message }
                    }
                })) {
                    ForEach(ChatNotificationLevel.allCases, id: \.self) { choice in
                        Text(choice.title).tag(choice)
                            .accessibilityIdentifier("chat-notifications-" + choice.rawValue)
                    }
                }
                .pickerStyle(.inline)
            } label: {
                Label("Notifications: " + level.title, systemImage: level.symbolName)
            }
            .disabled(model.chatNotifications.isSaving(chat.id) || model.macConnected != true || model.accessEnded)
            .accessibilityIdentifier("chat-notifications")
        }
    }
    /// Pull requests with its open count, and a note when a check failed or
    /// a review asks for changes, since the chat no longer shows a pill.
    private var pullRequestMenuTitle: String {
        var title = ComposerPill.pullRequests.title
        if let count = pullRequestState.badgeCount { title += " (\(count.formatted())\(pullRequestState.badgeIsLowerBound ? "+" : ""))" }
        if pullRequestState.needsAttention { title += ": needs attention" }
        return title
    }
    /// The header's chat-actions pill. Its menu items and title pill are erased
    /// into a small struct so the deep generic types (a Menu, Picker, ViewThatFits
    /// and conditionals) are not walked together on the physical phone's main stack.
    private func conversationTitleMenu(replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool) -> some View {
        ConversationTitleMenuView(items: AnyView(conversationTitleMenuItems), pill: AnyView(conversationTitlePill),
                                  accessibilityLabel: conversationTitleAccessibilityLabel,
                                  accessibilityValue: model.currentStep(chat.id) ?? conversationPlaceValue,
                                  besideRail: pillsInBar)
    }

    @ViewBuilder private var conversationTitleMenuItems: some View {
            // The header pill truncates; the menu's first row reads the whole title, wrapped,
            // and tapping it copies the title.
            Section {
                Button { UIPasteboard.general.string = chat.title; notice = "Title copied" } label: { Text(chat.title) }
                    .accessibilityHint("Copies the title")
                    .accessibilityIdentifier("conversation-title-full")
            }
            Section {
            Button("Chat details", systemImage: "info.circle") { openTool(.details) }
            if model.isProject(chat), !model.accessEnded {
                ChatDetailMenu(override: chatDetail?.chats[chat.id]) { setChatDetail($0) }
            }
            // Pull requests opens from here whenever the chat has any, and when
            // the computer couldn't check or GitHub is signed out there, so the
            // reason and Try again stay reachable.
            if pullRequestState.opensFromMenu {
                Button {
                    openTool(.pullRequests, toggle: true)
                } label: {
                    Label(pullRequestMenuTitle, systemImage: pullRequestState.needsAttention ? "exclamationmark.triangle.fill" : ComposerPill.pullRequests.systemImage)
                }
                .accessibilityIdentifier("open-pull-requests")
            }
            }
            Section {
            Button("Copy as Markdown", systemImage: "doc.on.doc") { copyAsMarkdown() }
                .accessibilityIdentifier("copy-conversation-markdown")
            if canFork {
                Button("Fork", systemImage: "arrow.triangle.branch") { fork(from: nil) }
                    .disabled(forking || model.accessEnded || model.previewMode)
                    .accessibilityIdentifier("fork-conversation")
            }
            }
            if model.isProject(chat), !chat.isArchived {
                notificationsSection
            }
            // The same gates as the Chats list: the provider can archive it, it
            // isn't working, and its computer is connected.
            if archiveConversation != nil, !chat.isArchived {
                Button("Archive", systemImage: "archivebox") {
                    if confirmArchive { confirmingArchive = true } else { archive() }
                }
                .disabled(archiving || deleting || model.archiveWaitsForWork(chat.id) || model.macConnected != true
                          || model.accessEnded || model.previewMode)
                .accessibilityIdentifier("archive-conversation")
            }
            if model.isProject(chat), !chat.id.hasPrefix("history-"), model.projects.supportsDelete {
                Button("Delete", systemImage: "trash", role: .destructive) { confirmingDelete = true }
                    .disabled(deleting || archiving || model.archiveWaitsForWork(chat.id)
                              || model.macConnected != true || model.accessEnded || model.previewMode)
                    .accessibilityIdentifier("delete-conversation")
            }
    }

    private var conversationTitlePill: some View {
        HStack(spacing: 5) {
            // Keep the menu's tap area and chevron inside the measured room.
            conversationHeader(room: max(0, headerGeometry.titleRoom - ProjectConversationHeader.BarGeometry.menuChrome - 24))
            Image(systemName: "chevron.down")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                // UIKit can expose this image despite hiding it; keep its spoken name useful.
                .accessibilityLabel("Chat options")
                .accessibilityHidden(true)
        }
    }
    /// A plain tap steers the running reply when the owner chose "Sends it
    /// into the reply" in Settings; taking a thread over always goes through
    /// Take over in the hold menu.
    private var sendGuides: Bool {
        FollowUpBehavior.plainSendSteers(choice: followUp, offersTakeOver: model.offersTakeOver(chat), canGuide: model.canSteerReply(chat))
    }
    private var sendWouldWait: Bool { model.sendWaits(chat) || model.offersT3TakeOver(chat) }
    /// The placeholder follows `sendQueues` once it holds for a moment: the Mac's
    /// activity readings (T3 holder, background commands) can blip while nothing
    /// changes on screen, and Message/Queue must not flicker. Our own running
    /// reply and the first reading apply at once.
    @State private var settledQueues: Bool?
    private var placeholderQueues: Bool { settledQueues ?? sendQueues }
    private func settlePlaceholder(_ queues: Bool) async {
        if settledQueues == nil || settledQueues == queues || model.activeTurn(chat.id) != nil {
            settledQueues = queues
            return
        }
        try? await Task.sleep(for: .seconds(1))
        if !Task.isCancelled { settledQueues = queues }
    }
    private var sendQueues: Bool {
        FollowUpBehavior.queuesMessage(choice: followUp, waits: sendWouldWait,
                                       offersTakeOver: model.offersTakeOver(chat), canSteer: model.canSteerReply(chat))
    }
    /// Steer is chosen but this running reply's provider has no steer (OpenCode,
    /// Cursor): Send is the Queue glyph. Queue mode and waits for T3 or another
    /// device keep the arrow; their placeholder and label already say Queue.
    private var sendQueuesBecauseUnsteerable: Bool {
        followUp == FollowUpBehavior.steer && model.sendWaits(chat) && model.activeTurn(chat.id) != nil && !model.canSteerReply(chat)
    }
    private var primarySendAvailable: Bool { sendGuides ? model.canGuide(chat) : model.canSend(chat) }
    private func primarySend() {
        guard primarySendAvailable else { return }
        if sendGuides { Task { await model.guide(chat) } } else { Task { await model.send(chat) } }
    }
    /// Send's hold menu, in order, before the plain Send or Queue.
    private var sendHoldActions: [SendHoldAction] {
        SendHoldAction.offered(takesOver: model.offersTakeOver(chat), restartsT3Code: model.offersT3TakeOver(chat))
            .filter { $0 != .sendNow || !sendGuides }
    }
    private func canPerformHold(_ action: SendHoldAction) -> Bool {
        action == .sendNow ? model.canGuide(chat) : model.canSend(chat)
    }
    private func performHold(_ action: SendHoldAction) {
        guard canPerformHold(action) else { return }
        switch action {
        // Send now: steer the running reply.
        case .sendNow: Task { await model.guide(chat) }
        // Take over from the app on the Mac that holds the thread, after the
        // warning unless turned off.
        case .takeOver:
            if warnBeforeTakeOver { confirmingTakeOver = true } else { Task { await model.send(chat, takeOver: true) } }
        // T3 Code stops its reply there and starts on this message.
        case .restartInT3Code: Task { await model.send(chat, takeOver: true) }
        }
    }
    /// What sending ends on the Mac, for VoiceOver: Claude's reply there (the
    /// last sender wins) or the agent tasks its process still runs. Sighted
    /// users see that work in the timeline and the agent-task pill.
    private var sendStopsMacWork: String? {
        if model.turnRunsElsewhere(chat.id) {
            return model.nativeStopsMacReply.contains(chat.id) ? "Stops Claude’s reply on your computer." : nil
        }
        return model.nativeTasksOnMac.contains(chat.id) && !model.nativeOpenElsewhere.contains(chat.id)
            ? "Stops Claude’s agent tasks on your computer." : nil
    }
    /// Photo loading, uploads and send preparation show on Send itself rather
    /// than as status lines above the composer.
    private var preparingSend: Bool {
        model.loadingPhotos.contains(chat.id) || model.uploading.contains(chat.id) ||
            model.preparingSends.contains(chat.id) || model.finishingDictation.contains(chat.id)
    }
    /// Send with its hold menu (`SendHoldAction`) responds when it can send,
    /// and in a preview, which has no computer, so the menu can be shown.
    private var menuSendResponds: Bool { primarySendAvailable || model.previewMode }
    private func sendSymbol(preparing: Bool) -> some View {
        PreparedSendSymbol(preparing: preparing, queues: sendQueuesBecauseUnsteerable, progressTint: theme.page)
    }
    /// The composer's controls and pills show for live conversations.
    private var composerShowsPills: Bool {
        !chat.isArchived && model.isProject(chat)
    }
    /// An iPhone Duo keeps its toolbar in a vertical bar beside the content,
    /// folded or not; the pills join it there, under the sidebar button.
    private var pillsInBar: Bool { verticalBar }
    /// An open pane retains its toggle at the owner's chosen position,
    /// including panes restored or opened from the title menu.
    private var presentedPillLayout: ComposerPillLayout {
        pillPreferences.layout.showing(open: tools.tool?.pill)
    }

    /// What the Git pill and Code changes show: the last reply's edits, and a
    /// Project folder's branch and pull requests.
    private func codeChanges(replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool) -> CodeChanges {
        CodeChanges(git: model.isProject(chat) ? model.gitSummaries[chat.id] : nil,
                    pullRequests: model.isProject(chat) ? model.pullRequests[chat.id] : nil,
                    replyEdits: replyEdits.map(CodeChanges.ReplyEdits.init), lastReplyChanged: lastReplyChanged,
                    repositories: model.isProject(chat) ? model.threadRepositories[chat.id] : nil,
                    thread: model.isProject(chat) ? model.threadChangeCounts[chat.id] : nil)
    }

    /// The composer's status pills, in the order Settings → Pills keeps on
    /// this device, without the ones turned off there, as items in a folded
    /// iPhone Duo's vertical bar (`inBar`); above the composer they scroll in
    /// `ComposerPillRowView`. A pill only shows when its feature has something to show.
    @ViewBuilder private func composerPills(replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool, inBar: Bool) -> some View {
        ForEach(presentedPillLayout.visible) { pill in
            composerPill(pill, replyEdits: replyEdits, lastReplyChanged: lastReplyChanged)
        }
    }

    @ViewBuilder private func composerPill(_ pill: ComposerPill, replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool) -> some View {
        switch pill {
        case .computer:
            // The computer entry owns dialog attention; there is no separate pill.
            // The demo computer shows a still picture of its screen (`DemoComputerView`).
            MacDialogComputerEntry(host: model.connection?.credential.hostInstallationId ?? "", name: model.macName,
                                   dock: true, isPresented: toolOpen(.computer)) {
                openTool(.computer, toggle: true)
            }
        case .simulators:
            if offersSimulators || toolOpen(.simulators) {
                SimulatorsPill(isPresented: toolOpen(.simulators)) { openTool(.simulators, toggle: true) }
            }
        case .files:
            FilesDock(isPresented: toolOpen(.files)) { toggleFiles() }
        case .terminal:
            if offersTerminal || toolOpen(.terminal) {
                TerminalPill(isPresented: toolOpen(.terminal)) { openTool(.terminal, toggle: true) }
            }
        case .pullRequests:
            // Not a pill: the chat's title menu opens Pull requests.
            EmptyView()
        case .preview:
            if let presence = model.devServerPresence(chat), presence.showsPill(at: Date()), model.previewEndpoint(chat) != nil {
                let menu = DevServerMenu(servers: presence.servers, refresh: { await model.loadDevServers(chat) }, open: openPreview)
                if pillsInBar { menu.controlSize(.large) } else { menu }
            }
        case .attention:
            // An empty TimelineView still reserves stack spacing in a toolbar.
            if !pillsInBar || PendingAttention(model: model, chat: chat, now: Date()).count > 0 {
                AttentionDock(model: model, chat: chat, isPresented: showingAttention) {
                    if !showingAttention {
                        dismissTool(.files)
                    }
                    attentionTapGuardUntil = nil
                    showingAttention.toggle()
                }
            }
        case .goal:
            goalDock
        case .agentTasks:
            projectSubagentDock
        }
    }

    /// Simulators run on a Mac with Xcode; the pill opens the list of every paired computer's.
    private var offersSimulators: Bool { model.isMacHost && !model.accessEnded && !readOnly }
    private var offersTerminal: Bool { model.isProject(chat) && model.projects.supportsTerminal && !model.accessEnded }
    private var offersGoal: Bool { model.goals[chat.id] != nil && model.isProject(chat) && !readOnly }
    private var offersAgentTasks: Bool {
        model.isProject(chat) && (!(model.projectSubagents[chat.id] ?? []).isEmpty || model.hasOlderProjectSubagents(chat.id))
    }


    /// Code changes, as a pill sheet or in the companion pane.
    private func codeChangesSheet(replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool) -> some View {
            CodeChangesSheet(changes: codeChanges(replyEdits: replyEdits, lastReplyChanged: lastReplyChanged),
                latestEdits: replyEdits,
                pullRequestError: model.pullRequestErrors[chat.id],
                refreshingPullRequests: model.refreshingPullRequests.contains(chat.id),
                pullRequestStatus: pullRequestState,
                commentContext: model.projects.details[chat.id].map {
                    DiffCommentContext(projectID: $0.projectId, conversationID: chat.id, assignmentScope: model.assignmentScope)
                },
                stageComment: { [model, chat] annotation, scope in
                    try model.stageAnnotation(annotation, chat: chat, expectedScope: scope)
                },
                loadChanges: { [model, chat] scope, location in try await model.loadGitChanges(chat, scope: scope, location: location) },
                loadThreadChanges: { [model, chat] location in try await model.loadThreadChanges(chat, location: location) },
                loadDiff: { [model, chat] file, scope, location in
                    try await model.loadGitChangeDiff(chat, file: file, scope: scope, location: location)
                },
                refreshSummary: { [model, chat] in await model.loadGitSummary(chat) },
                refreshPullRequests: { [model, chat] in await model.loadPullRequests(chat, refresh: true) },
                refreshRepositories: { [model, chat] in await model.loadThreadRepositories(chat) },
                runGitAction: { [model, chat] request in try await model.runGitAction(chat, request) },
                loadMergeState: { [model, chat] repository, url in
                    try await model.loadMergeState(chat, repository: repository, url: url)
                },
                stagePullRequestTask: { [model, chat] prompt in model.stagePullRequestTask(prompt, chat: chat) })
    }

    /// Tall windows open a native sheet instead of dividing a short chat above
    /// or below the tool. The whole window, not the keyboard's remaining area,
    /// decides; landscape retains the split at the physical fold.
    private var toolLayout: ToolPresentation.Layout {
        guard conversationSizeClass == .regular else { return .compact }
        // A live computer or simulator view stays where it opened while the window
        // turns: moving it between the pane and the full-screen cover would rebuild
        // it, ending control and sending the simulator view back to its list.
        if let open = tools.tool?.kind, open == .computer || open == .simulators, layoutSizeClass == .regular { return tools.layout }
        guard let window = toolWindowBounds ?? toolWindowSize else { return .regular }
        return CompanionPaneLayout.direction(window: window) == .sideBySide ? .regular : .compact
    }

    private var paneAnimation: Animation? { reduceMotion ? nil : .snappy(duration: 0.25) }

    private var companion: CompanionTool? { tools.pane }
    private var companionExpanded: Bool { tools.expanded && !hasDisplayFold }
    private var showingComputer: Bool { tools.phone?.kind == .computer }
    private var showingSimulators: Bool { tools.phone?.kind == .simulators }
    private var showingTerminal: Bool { tools.phone?.kind == .terminal }
    private var showingChanges: Bool { tools.phone?.kind == .pullRequests }
    private var showingDetails: Bool { tools.phone?.kind == .details }
    private var workspaceRequest: WorkspaceBrowserRequest? {
        if case .files(let request) = tools.phone { return request }
        return nil
    }
    private var selectedProjectSubagent: ProjectSubagentSummary? {
        if case .agentTask(let child) = tools.phone { return child }
        return nil
    }
    private var replyChangesRequest: ReplyChangesRequest? {
        if case .replyChanges(let turnID) = tools.phone { return ReplyChangesRequest(turnID: turnID) }
        return nil
    }

    /// Binding dismissals carry the identity of the sheet they came from, so
    /// an outgoing sheet cannot close the tool after it moved to the pane.
    private func phonePresentation(_ kind: CompanionTool.Kind) -> Binding<Bool> {
        let identity = tools.phone?.kind == kind ? tools.identity : nil
        return Binding(get: { tools.phone?.kind == kind }, set: { showing in
            if !showing, let identity { changeTools(.dismiss(presentation: identity)) }
        })
    }

    private func phoneItem<Item>(_ extract: @escaping (CompanionTool) -> Item?) -> Binding<Item?> {
        let identity = tools.phone.flatMap(extract) == nil ? nil : tools.identity
        return Binding(get: { tools.phone.flatMap(extract) }, set: { item in
            if case nil = item, let identity { changeTools(.dismiss(presentation: identity)) }
        })
    }

    private var subagentPresentation: Binding<ProjectSubagentSummary?> {
        phoneItem { if case .agentTask(let child) = $0 { return child }; return nil }
    }

    private var replyChangesPresentation: Binding<ReplyChangesRequest?> {
        phoneItem { if case .replyChanges(let turnID) = $0 { return ReplyChangesRequest(turnID: turnID) }; return nil }
    }

    /// A pill toggles the shared selection, whichever layout presents it.
    private func openTool(_ tool: CompanionTool, toggle: Bool = false) {
        if tool.kind == .files { showingAttention = false }
        let show = {
            withAnimation(paneAnimation) {
                changeTools(.open(tool, toggle: toggle))
            }
        }
        if tool.kind == .details, !toggle || tools.tool?.kind != .details { show() } else { leavingDetails(show) }
    }

    /// Leaving Thread details saves a changed name first. A refused save keeps
    /// details open with the typed name and the reason.
    private func leavingDetails(_ leave: @escaping () -> Void) {
        guard tools.tool == .details, threadTitle.hasUnsavedChange else { leave(); return }
        let identity = tools.identity
        Task { await threadTitle.leave(model.projects, showing: { tools.identity == identity && tools.tool == .details }, then: leave) }
    }

    private func closeCompanion() {
        leavingDetails(closeToolNow)
    }

    private func closeToolNow() {
        guard tools.tool != nil else { return }
        withAnimation(paneAnimation) { changeTools(.close) }
    }

    /// All exits, including native sheet binding writes, share these effects.
    /// UIKit's later onDismiss must not restore the conversation a second time.
    private func changeTools(_ action: ToolPresentation.Action) {
        // An outgoing phone sheet may write its dismissal binding after the shell
        // has already opened B. It must not erase A's remembered pane.
        if case .dismiss = action, voiceScene != nil, !isCurrentToolOwner { return }
        let previous = tools.tool
        let saved = tools.saved
        tools.reduce(action)
        if tools.saved != saved, isCurrentToolOwner, let host = model.connection?.credential.hostInstallationId {
            voiceScene?.viewStates.save(tools.saved, host: host, conversation: chat.id)
        }
        if previous?.kind == .agentTask, tools.tool?.kind != .agentTask {
            model.presentConversation(chat, root: rootChat)
        }
    }

    /// Keep presentation lifecycle inference separate from the timeline's modifier chain.
    private func trackingToolPresentation<Content: View>(_ content: Content) -> some View {
        content
        .onAppear {
            restoreTools()
            model.presentConversation(chat, root: rootChat)
        }
        .onReceive(voiceScene?.$paneRequest.compactMap { $0 }.receive(on: DispatchQueue.main).eraseToAnyPublisher()
                    ?? Empty<ShellState.PaneRequest, Never>().eraseToAnyPublisher()) { request in
            // Voice asked for a tool; only the conversation on screen answers.
            guard isCurrentToolOwner else { return }
            voiceScene?.paneRequest = nil
            if let tool = request.tool { openTool(tool) } else { closeCompanion() }
        }
        .onChange(of: companion != nil, initial: true) { _, _ in syncShellPane() }
        .onChange(of: companion != nil && companionExpanded, initial: true) { _, _ in syncShellPane() }
        // Fold and unfold move the same selection between phone and pane.
        .onChange(of: toolLayout, initial: true) { _, layout in
            withAnimation(paneAnimation) {
                changeTools(.layoutChanged(layout))
            }
        }
        .onChange(of: conversationSizeClass, initial: true) { _, sizeClass in layoutSizeClass = sizeClass }
        .onChange(of: tools.tool?.kind) { _, kind in
            toolSessions.release(computer: kind != .computer, simulators: kind != .simulators)
        }
        .environment(\.toolSessions, toolSessions)
        .onChange(of: chat.id) { previous, _ in
            SpeechReader.shared.conversationClosed(previous)
            restoredTools = false
            restoreTools()
        }
        .onChange(of: model.projectSubagents[chat.id]?.map(\.id)) { _, _ in
            // A cold launch can display the chat before its child-task roster arrives.
            if isCurrentToolOwner, tools.tool == nil,
               let host = model.connection?.credential.hostInstallationId,
               voiceScene?.viewStates.pane(host: host, conversation: chat.id)?.kind == .agentTask {
                restoredTools = false
                restoreTools()
            }
        }
    }

    private var isCurrentToolOwner: Bool {
        voiceScene?.toolPresentationOwner == toolPresentationOwner
            && voiceScene?.selectedConversation?.host == model.connection?.credential.hostInstallationId
            && voiceScene?.selectedConversation?.id == chat.id
    }

    private func restoreTools() {
        if voiceScene?.selectedConversation?.host == model.connection?.credential.hostInstallationId,
           voiceScene?.selectedConversation?.id == chat.id {
            voiceScene?.toolPresentationOwner = toolPresentationOwner
        }
        guard !restoredTools else { syncShellPane(); return }
        restoredTools = true
        let saved = model.connection.flatMap {
            voiceScene?.viewStates.pane(host: $0.credential.hostInstallationId, conversation: chat.id)
        }
        tools.restore(saved, layout: toolLayout) { id in
            model.projectSubagents[chat.id]?.first { $0.id == id }
        }
        // A restored computer creates a fresh view-only session. Control is
        // transient @State and only Take control can acquire a new lease.
        syncShellPane()
    }

    private func syncShellPane() {
        guard isCurrentToolOwner else { return }
        if voiceScene?.companionPaneOpen != (companion != nil) { voiceScene?.companionPaneOpen = companion != nil }
        let expanded = companion != nil && companionExpanded
        if voiceScene?.companionPaneExpanded != expanded { voiceScene?.companionPaneExpanded = expanded }
    }

    /// Secret, question and permission requests in this chat, in review order.
    private var pendingRequestIDs: [String] {
        let pending = PendingAttention(model: model, chat: chat, now: Date())
        return pending.requests.map(\.id) + pending.questions.map(\.question.id)
    }

    /// A request the agent just made opens its review in front, as a tapped
    /// banner does, instead of waiting as a pill on the rail. Each request opens
    /// once: while the review is already open the pill's count and the review's
    /// own navigation show the new one, so a form being filled never jumps.
    private func openNewRequests(_ ids: [String]) {
        let fresh = ids.filter { !autoOpenedRequests.contains($0) }
        guard let first = fresh.first else { return }
        autoOpenedRequests.formUnion(fresh)
        guard !showingAttention, !readOnly else { return }
        #if WONDER_DIAGNOSTICS
        // Scenarios that measure the transcript or the pill row with a request pending.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-requests-stay-closed") { return }
        #endif
        // A banner or the inbox already named the request to show: keep its choice.
        if model.attentionRequestFocus == nil { model.attentionRequestFocus = first }
        dismissTool(.files)
        var dwell = 0.6
        #if WONDER_DIAGNOSTICS
        // A scenario tap takes over a second, so it asks for a longer dwell to land inside it.
        let args = ProcessInfo.processInfo.arguments
        if let i = args.firstIndex(of: "-diagnostics-request-dwell"), args.indices.contains(i + 1), let value = Double(args[i + 1]) { dwell = value }
        #endif
        attentionTapGuardUntil = Date().addingTimeInterval(dwell)
        showingAttention = true
    }

    private func dismissTool(_ kind: CompanionTool.Kind) {
        if tools.tool?.kind == kind { closeToolNow() }
    }

    private func toolOpen(_ kind: CompanionTool.Kind) -> Bool { tools.tool?.kind == kind }

    /// The chat's pull requests as the pill and list show them.
    private var pullRequestState: ConversationPullRequests {
        ConversationPullRequests(isProject: model.isProject(chat), list: model.pullRequests[chat.id],
                                 error: model.pullRequestErrors[chat.id])
    }

    /// A document or folder from the chat: in the pane beside it, or replacing it on a phone.
    private func showFiles(_ request: WorkspaceBrowserRequest) {
        openTool(.files(request))
    }

    private func toggleFiles() {
        openTool(.files(WorkspaceBrowserRequest()), toggle: true)
    }

    /// What the companion pane shows: the same view as the tool's sheet. Its
    /// Selected pills close tools; panes without a pill retain Close.
    @ViewBuilder private func companionContent(_ tool: CompanionTool, replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool,
                                               replyChanges: (String) -> ResponseEditedFiles?) -> some View {
        switch tool {
        case .computer:
            NavigationStack { // theme-exempt: ComputerSessionView draws theme.computerBackground
                if model.isDemo {
                    DemoComputerView()
                } else {
                    ComputerSessionView(model: model, chat: chat, agent: ComputerAgentPresence(name: computerAgentName, isActive: agentComputer.isUsingComputer, isActing: agentComputer.isActing), pauseAgent: { Task { await model.stop(chat) } })
                }
            }
        case .simulators:
            NavigationStack { // theme-exempt: SimulatorSessionView draws theme.computerBackground
                SimulatorSessionView(model: model, chat: chat, sessions: toolSessions)
            }
        case .terminal:
            TerminalSheet(model: TerminalSessionModel(conversationID: chat.id,
                                                      folderName: model.projectDetail(chat)?.workingFolderName ?? "Project",
                                                      transport: model.terminalTransport()))
        case .pullRequests:
            codeChangesSheet(replyEdits: replyEdits, lastReplyChanged: lastReplyChanged)
        case .files(let request):
            WorkspaceSheet(close: closeCompanion) {
                WorkspaceBrowser(model: model, chat: chat, attachmentIDs: request.attachmentIDs,
                                 initialFilePath: request.initialFilePath, preferredRootID: request.preferredRootID,
                                 reanchor: request.reanchor, replacingAnnotationID: request.replacingAnnotationID)
            }
        case .agentTask(let child):
            ProjectSubagentTranscriptView(model: model, parent: chat, child: child)
        case .details:
            if model.isProject(chat) { ProjectConversationDetailsView(model: model, library: model.projects, chat: chat, title: threadTitle) }
        case .replyChanges(let turnID):
            ReplyChangesView(edits: replyChanges(turnID))
        }
    }

    private func openPreview(_ server: DevServer) {
        guard let url = URL(string: "http://localhost:\(server.port)/"), let link = LoopbackLink(url) else { return }
        openPreview(at: link)
    }

    private func openPreview(at link: LoopbackLink) {
        guard let endpoint = model.previewEndpoint(chat) else { return }
        leavingDetails {
            browser.open(endpoint, at: link)
            Task { await model.loadDevServers(chat) }
        }
    }

    /// A reply's link to a page on the computer: opens it in the browser when
    /// this chat serves that port (reading the list again if it isn't there
    /// yet, as when the agent just started the server), otherwise says where
    /// the page runs.
    private func openLoopbackLink(_ link: LoopbackLink) {
        loopbackLinkTask?.cancel()
        let scope = model.assignmentScope
        let conversationID = chat.id
        loopbackLinkTask = Task {
            let running = link.isServed(by: model.devServerPresence(chat)?.servers ?? [])
                ? true : await model.devServerRunning(chat, port: link.port)
            guard !Task.isCancelled, model.assignmentScope == scope, chat.id == conversationID,
                  model.previewEndpoint(chat) != nil else { return }
            if running == true {
                openPreview(at: link)
            } else {
                notice = LoopbackLink.notServedMessage(computer: model.macName, previewAvailable: running != nil)
            }
        }
    }

    @ViewBuilder private var goalDock: some View {
        if let goal = model.goals[chat.id], offersGoal {
            GoalDock(goal: goal, error: model.goalErrors[chat.id], isPresented: $showingGoal,
                save: { objective, budget, timeBudget in
                    await model.updateGoal(chat, objective: objective,
                                           tokenBudget: budget, timeBudgetSeconds: timeBudget)
                },
                pause: { await model.pauseGoal(chat) },
                resume: { await model.resumeGoal(chat) },
                clear: { await model.clearGoal(chat) }, contentTypeSize: typeSize)
        }
    }

    @ViewBuilder private var projectSubagentDock: some View {
        if offersAgentTasks || toolOpen(.agentTask) {
            ProjectSubagentDock(agents: model.projectSubagents[chat.id] ?? [],
                sections: model.projectSubagentSections[chat.id],
                available: model.macConnected == true && model.projectSubagentAvailability[chat.id] != false,
                freshIDs: model.projectSubagentFreshIDs[chat.id] ?? [],
                detail: model.projectSubagentErrors[chat.id],
                stoppingIDs: model.stoppingProjectSubagents[chat.id] ?? [],
                stopError: model.projectSubagentStopErrors[chat.id],
                provider: model.agentFamily(chat)?.title,
                hasOlder: model.hasOlderProjectSubagents(chat.id),
                loadingOlder: model.loadingOlderProjectSubagents.contains(chat.id),
                isPresented: $showingProjectSubagents,
                paneIsPresented: companion?.kind == .agentTask, closePane: closeCompanion,
                loadOlder: { Task { await model.loadOlderProjectSubagents(chat) } },
                stop: { child in Task { await model.stopProjectSubagent(parent: chat, child: child) } }) { child in
                showingProjectSubagents = false
                Task { @MainActor in
                    await Task.yield()
                    openTool(.agentTask(child))
                }
            }
        }
    }

    /// One chat, one status line. A current event stream disproves blanket
    /// offline wording; the connection owner still decides recovery and writes.
    private var composerHostNotice: (text: String, id: String)? {
        if let holder = model.nativeHolders[chat.id], holder.t3Code != nil {
            if let text = OpenOnMacNotice.text(holder: holder, computer: model.macName, t3CodeLimit: model.t3CodeLimit(chat.id)) {
                return (text, "open-elsewhere")
            }
        } else if model.nativeOpenElsewhere.contains(chat.id), !model.turnRunsElsewhere(chat.id) {
            if let text = OpenOnMacNotice.text(holder: model.nativeHolders[chat.id], computer: model.macName),
               model.snapshots[chat.id]?.hasUnassignedPreTurnWork == true {
                return (text, "open-elsewhere")
            }
        } else if let limit = model.nativeT3CodeCantSend[chat.id] {
            return (T3CodeNote.cantSend(limit: limit, computer: model.macName), "t3-code-cant-send")
        } else if model.nativeRunsWithoutT3Code.contains(chat.id) {
            return (T3CodeNote.runsWithout(computer: model.macName), "t3-code-runs-without")
        } else if let holder = model.nativeHolders[chat.id], holder.kind == "desktop", !holder.waits,
                  model.projects.details[chat.id]?.family == .codex, // provider identity
                  let text = OpenOnMacNotice.text(holder: holder, computer: model.macName, relayed: true),
                  model.snapshots[chat.id]?.hasUnassignedPreTurnWork == true {
            return (text, "open-elsewhere")
        }
        return nil
    }

    /// Waiting messages already say so on their own rows, so the line steps aside.
    private var offlineNoticeShown: String? {
        readOnly || !(model.composers[chat.id]?.outbox ?? []).isEmpty ? nil : model.composerOfflineNotice
    }

    @ViewBuilder private var composerStatus: some View {
        if let error = model.controlErrors[chat.id] {
            ComposerIssue(message: error, id: "composer-action-issue") { model.dismissControlError(chat.id) }
        } else if let error = model.composerErrors[chat.id] {
            ComposerIssue(message: error, id: "composer-draft-issue", action: startOverAction) { model.dismissComposerNotice(chat.id) }
        } else if !rewinding, !readOnly, model.unconfirmedRewind(chat.id) != nil {
            UnconfirmedRewindNotice(computer: model.macName, checking: checkingRewind) { Task { await checkUnconfirmedRewind() } }
        } else if (model.composers[chat.id]?.draft.utf8.count ?? 0) > 65536 {
            Text("Message too long").font(.caption).foregroundStyle(.secondary)
        } else if model.nativeHistoryFailures.contains(chat.id) {
            HistoryRetryNotice { await model.reloadNativeHistory(chat) }
        } else if !readOnly, let kept = model.orphanedDrafts.first {
            OrphanedDraftNotice(draft: kept, more: model.orphanedDrafts.count - 1, error: model.orphanedDraftError,
                move: { _ = model.moveOrphanedDraft(kept.id, to: chat) }, discard: { model.discardOrphanedDraft(kept.id) })
        } else if !readOnly, let error = model.orphanedDraftError {
            FailureDetails(message: error)
        } else if let unreachable = offlineNoticeShown {
            if let rescue = model.rescue {
                // Cupola's supervisor answers: Restart replaces Reconnect on this one line.
                RescueNoticeView(model: model, notice: rescue, place: "composer")
            } else {
            (typeSize.isAccessibilitySize
                ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4))
                : AnyLayout(HStackLayout(alignment: .firstTextBaseline))) {
                Label(unreachable, systemImage: "desktopcomputer.trianglebadge.exclamationmark")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    .accessibilityHint("Messages you send now stay on this device and go out in order when your computer is back.")
                    .accessibilityIdentifier("composer-mac-unreachable")
                Button("Reconnect") { Task { await model.check(renew: true, userInitiated: true) } }
                    .font(.caption).foregroundStyle(Color(uiColor: .systemBlue)).frame(minHeight: 44)
                    .disabled(model.checkingConnection)
                    .accessibilityIdentifier("composer-reconnect")
            }
            }
        } else if !readOnly, model.waitsOnComputer(chat, waitsOnMac: threadWaitsOnMac) {
            ComputerWaitingNotice(agent: computerAgentName, computer: model.macName,
                approvalInT3Code: model.projects.details[chat.id]?.t3CodePermissions != nil || model.nativeHolders[chat.id]?.app == "T3 Code",
                checkIdentity: model.assignmentScope + ":" + String(describing: model.macConnected),
                check: { [model] in await model.computerViewUnavailable() }) { openTool(.computer) }
        } else if let note = composerHostNotice {
            Text(note.text).font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityIdentifier(note.id)
        } else if let at = UsageResume.date(resumesAt), !model.conversationWorking(chat.id) {
            UsageResumeNotice(library: model.projects, conversationID: chat.id, at: at, limitShown: false)
        } else if let message = model.usageLimitMessage(chat) {
            Text(message).font(.footnote).foregroundStyle(.secondary)
                .accessibilityIdentifier("composer-usage-limit")
        } else if let notice = model.snapshots[chat.id]?.pendingDeliveryNotice(projectNotice) {
            Text(notice).font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true).accessibilityIdentifier("last-request-issue")
        } else if let detail = model.projects.details[chat.id],
                  let notice = ConversationSnapshot.startingNotice(projectNotice, hasNativeSession: detail.hasNativeSession,
                                                                    snapshot: model.snapshots[chat.id]) {
            Label(notice, systemImage: "hourglass").font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true).accessibilityIdentifier("conversation-starting-notice")
        } else if !model.conversationWorking(chat.id), let issue = model.snapshots[chat.id]?.latestRequestIssue {
            TemporaryNotice(key: issue) {
                Text(issue).font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true).accessibilityIdentifier("last-request-issue")
            }
        }
    }

    private func composer(replyEdits: ResponseEditedFiles?, lastReplyChanged: Bool, suggestion: PromptSuggestion?) -> some View {
            let attachmentMetadata = Dictionary((model.files[chat.id] ?? []).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
            let attachments = composerAttachments
            let completionContext = model.composerCompletionContext(chat)
            return VStack(alignment: .leading, spacing: 4) {
                DictationControls(controller: model.dictation, model: model, chat: chat)
                if composerShowsPills {
                    composerStatus
                    if let failure = model.uploadFailures[chat.id], !model.uploading.contains(chat.id) {
                        HStack {
                            Text(failure).font(.caption).foregroundStyle(.secondary)
                            Button("Retry") { Task { await model.retryUpload(chat) } }
                                .frame(minWidth: 44, minHeight: 44)
                                .accessibilityLabel("Retry upload")
                                .accessibilityIdentifier("composer-upload-retry")
                        }
                    }
                    if let progress = model.uploadProgress[chat.id] {
                        HStack {
                            ComposerUploadProgress(progress: progress)
                            Button("Cancel") { model.cancelUpload(chat) }
                                .frame(minWidth: 44, minHeight: 44)
                                .accessibilityLabel("Cancel upload")
                                .accessibilityIdentifier("composer-upload-cancel")
                        }
                    }
                    VStack(spacing: 4) {
                        if !pillsInBar, presentedPillLayout.row.hasPills {
                            ComposerPillRowView(row: presentedPillLayout.row, selection: tools.tool?.pill) { pill in
                                composerPill(pill, replyEdits: replyEdits, lastReplyChanged: lastReplyChanged)
                            }
                        }
                    PreparedComposerSurface(background: theme.surface) {
                    VStack(spacing: 0) {
                    if !attachments.isEmpty {
                        ComposerAttachmentStrip(
                            attachments: attachments,
                            imagePreviews: model.imagePreviews,
                            imagePreviewScope: model.imagePreviewScope,
                            chatID: chat.id,
                            staleAnnotationIDs: Set(attachments.filter { model.isAnnotationStale($0.id, chat: chat.id) }.map(\.id)),
                            removalDisabled: model.preparingSends.contains(chat.id) || model.uploading.contains(chat.id) || model.sending.contains(chat.id) || model.composers[chat.id]?.pending != nil,
                            loadRemoteData: { [model, chat] file in try await model.thumbnail(file, chat: chat, size: .chip) },
                            openPhoto: { composerPhoto = $0 },
                            remove: { model.removeStaged($0, chat: chat.id) },
                            editAnnotation: { annotationEdit = $0 }
                        )
                    }
                    // A provider whose Plan is a permission mode shows it in the shield menu.
                    if model.isProject(chat), supportsModes, planModeOn, model.capabilities(chat)?.plan == .toggle {
                        HStack(spacing: 0) {
                            PlanModeChip(isDisabled: model.savingComposerSettings.contains(chat.id) || model.accessEnded) {
                                Task { await setPlanMode(false, model: model, library: model.projects, chat: chat) }
                            }
                            Spacer(minLength: 0)
                        }.padding(.horizontal, 8).padding(.top, 4)
                    }
                    SelectedCodexSkills(skills: model.composers[chat.id]?.codexSkills ?? [], takesSkills: model.nextReplyTakesSkills(chat),
                        disabled: model.preparingSends.contains(chat.id) || model.uploading.contains(chat.id) || model.sending.contains(chat.id),
                        remove: { model.removeSkill($0, chat: chat.id) })
                    if completionContext != nil {
                        ComposerSuggestionList(completion: completion)
                    }
                    messageEditor(suggestion: suggestion, completes: completionContext != nil).padding(.horizontal, 8)
                        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { composerControlWidth = $0 }
                    if model.isProject(chat), separatesComposerChoices {
                        ProjectComposerSettings(model: model, library: model.projects, chat: chat, placement: .choices, presentation: $composerModelPresentation) { EmptyView() }
                            .padding(.horizontal, 8)
                    }
                    // Adjacent 44 pt targets: attachments, then Permissions,
                    // Dictate and conversational voice. The settings row measures
                    // the whole input group before fitting the model and effort.
                    PreparedComposerControls {
                        Group {
                            Menu {
                        #if WONDER_DIAGNOSTICS
                                if ProcessInfo.processInfo.arguments.contains("-diagnostics-video") {
                                    Button("Attach fixture video") {
                                        Task {
                                            guard let url = Bundle.main.url(forResource: "attachment-video", withExtension: "mp4") else { return }
                                            await model.stageChosenMedia([{ try await MediaPreparation.video(url) }], chat: chat)
                                        }
                                    }.accessibilityIdentifier("fixture-attach-video")
                                    if ProcessInfo.processInfo.arguments.contains("-diagnostics-video-upload") {
                                        Button("Upload fixture video") { Task { await model.retryUpload(chat) } }
                                            .accessibilityIdentifier("fixture-upload-video")
                                    }
                                    Button("Reload saved draft") { model.reloadCameraFixtureDraft(chat.id) }
                                        .accessibilityIdentifier("fixture-reload-video-draft")
                                }
                                #endif
                                Button("Camera", systemImage: "camera") {
                                    UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                                    cameraScope = model.assignmentScope; showingCamera = true
                                }.disabled(!model.canAttach(chat) || model.loadingPhotos.contains(chat.id))
                                Button("Add sketch", systemImage: "pencil.tip.crop.circle.badge.plus") {
                                    UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                                    sketchRequest = SketchRequest(scope: model.assignmentScope)
                                }.disabled(!model.canAttach(chat) || model.loadingPhotos.contains(chat.id))
                                    .accessibilityIdentifier("composer-add-sketch")
                                Button("Add media", systemImage: "photo.on.rectangle") {
                                    importScope = model.assignmentScope; selectingPhoto = true
                                }.disabled(!model.canAttach(chat) || model.loadingPhotos.contains(chat.id))
                                Button(model.attachmentsSupported(chat) ? "Attach file" : "Attachments unavailable for this agent", systemImage: "paperclip") {
                                    importScope = model.assignmentScope; importing = true
                                }.disabled(!model.canAttach(chat) || model.loadingPhotos.contains(chat.id))
                            } label: { Image(systemName: "plus").font(.system(size: 22)).frame(width: 44, height: 44) }
                                .accessibilityLabel("Add attachment")
                                .accessibilityIdentifier("composer-actions")
                        }
                        if model.isProject(chat) {
                            ProjectComposerSettings(model: model, library: model.projects, chat: chat, placement: separatesComposerChoices ? .inputs : .inline, presentation: $composerModelPresentation) { composerInputControls }
                                .environment(\.composerCompactChips, composerControlWidth < 420 && !typeSize.isAccessibilitySize)
                        } else {
                            composerInputControls
                            Spacer(minLength: 0)
                        }
                        ComposerTrailingSlot(dictation: model.dictation, conversationID: chat.id,
                                             composer: model.composers[chat.id],
                                             preparing: preparingSend || model.sending.contains(chat.id),
                                             offersStop: model.offersStop(chat)) {
                        DictationSendControls(controller: model.dictation, conversationID: chat.id) {
                        if !model.offersGuide(chat) && !model.offersTakeOver(chat) {
                            Button { Task { await model.send(chat) } } label: { sendSymbol(preparing: preparingSend) }
                            .sendCircle(responds: model.canSend(chat))
                            .accessibilityLabel(sendQueues ? "Queue message" : "Send message").accessibilityIdentifier("send-message")
                            .accessibilityValue(preparingSend ? "Preparing" : "")
                            .accessibilityHint(sendStopsMacWork ?? "")
                            .keyboardShortcut(.return, modifiers: .command)
                            .disabled(!model.canSend(chat))
                        } else {
                        Menu {
                            // Touch and hold Send, then release on an action.
                            ForEach(sendHoldActions, id: \.accessibilityID) { action in
                                Button(action.title, systemImage: action.systemImage) { performHold(action) }
                                    .disabled(!canPerformHold(action))
                                    .accessibilityIdentifier(action.accessibilityID)
                            }
                            Button(sendWouldWait ? "Queue" : "Send",
                                   systemImage: sendWouldWait ? "text.line.last.and.arrowtriangle.forward" : "arrow.up") { Task { await model.send(chat) } }
                                .accessibilityIdentifier("send-queue")
                                .disabled(!model.canSend(chat))
                        } label: {
                            sendSymbol(preparing: preparingSend)
                        } primaryAction: {
                            primarySend()
                        }
                        .menuOrder(.fixed)
                        .menuStyle(.borderlessButton)
                        // Filled exactly when it responds. A preview has no
                        // computer to send to (canSend is false) but keeps the
                        // hold menu, so it looks as a paired thread's does.
                        .sendCircle(responds: menuSendResponds)
                        .accessibilityLabel(sendQueues ? "Queue message" : "Send message").accessibilityIdentifier("send-message")
                        .accessibilityValue(preparingSend ? "Preparing" : "")
                        .accessibilityHint([sendStopsMacWork, "Touch and hold for message actions."].compactMap { $0 }.joined(separator: " "))
                        .accessibilityActions {
                            ForEach(sendHoldActions.filter { canPerformHold($0) }, id: \.accessibilityID) { action in
                                Button(action.title) { performHold(action) }
                            }
                        }
                        .keyboardShortcut(.return, modifiers: .command)
                        .disabled(!menuSendResponds)
                        }
                        }
                        } stop: { besideSend in
                            // Send's place while a reply runs and there is nothing to send,
                            // filled like Send; beside Send, a quieter button of its own that
                            // works whatever blocks Send. Progress while the stop is confirmed;
                            // the draft stays.
                            let stopping = model.stopping.contains(chat.id)
                            let tint = besideSend ? theme.text : theme.page
                            Button { Task { await model.stop(chat) } } label: {
                                if stopping { ProgressView().tint(tint).frame(width: 44, height: 44) }
                                else { Image(systemName: "stop.fill").font(.system(size: besideSend ? 14 : 16, weight: .semibold)).frame(width: 44, height: 44) }
                            }
                            .foregroundStyle(tint)
                            .background(besideSend ? Color.secondary.opacity(0.18) : model.accessEnded ? Color.secondary.opacity(0.35) : theme.text, in: Circle())
                            .accessibilityLabel("Stop reply").accessibilityIdentifier("stop-response")
                            .accessibilityValue(stopping ? "Stopping" : "")
                            .disabled(stopping || model.accessEnded)
                        }
                    }
                    }
                    }
                    }
                            if !readOnly { QueueDock(
                                model: model,
                                chat: chat,
                                attachmentMetadata: attachmentMetadata,
                                imagePreviews: model.imagePreviews,
                                imagePreviewScope: model.imagePreviewScope,
                                loadRemoteData: { [model, chat] file in try await model.thumbnail(file, chat: chat, size: .chip) },
                                openImage: { attachment in
                                    guard let id = attachment.file?.id else { return }
                                    // The queued message's own pictures.
                                    let ids = (model.queues[chat.id] ?? []).first { $0.attachmentIds.contains(id) }?.attachmentIds ?? [id]
                                    messagePhotoGallery = MessagePhotoGallery(
                                        files: ids.compactMap { attachmentMetadata[$0] },
                                        initialFileID: id
                                    )
                                },
                                openDocument: { ids in
                                    showFiles(WorkspaceBrowserRequest(attachmentIDs: ids))
                                }
                            ) }

                }
                if chat.isArchived {
                    Text("Archived chat")
                        .font(.caption).foregroundStyle(.secondary)
                }

                    }
                    .modifier(ConversationComposerDock())
    }
    /// Offered while the saved draft can't be read; the files are set aside first.
    private var startOverAction: (title: String, hint: String, run: () -> Void)? {
        guard model.draftUnreadable(chat.id), !readOnly else { return nil }
        let model = model, id = chat.id
        return (title: "Start over", hint: "Keeps a copy of the saved message and starts an empty draft.",
                run: { _ = model.startOverUnreadableDraft(id) })
    }
    private var composerAttachments: [ComposerAttachment] {
        let intent = model.composers[chat.id]
        var result: [ComposerAttachment] = []
        var seen = Set<String>()

        for file in intent?.stagedFiles ?? [] where seen.insert(file.id).inserted {
            let metadata = file.uploaded
            result.append(ComposerAttachment(
                id: file.id,
                name: file.name,
                mimeType: file.mimeType,
                data: file.data,
                remoteFile: metadata,
                sha256: file.sha256,
                byteSize: metadata?.byteSize ?? file.data.count,
                state: metadata?.state ?? "local",
                updatedAt: metadata?.updatedAt ?? "composer",
                source: file.source
            ))
        }

        var metadataByID: [String: ConversationFile] = [:]
        for file in model.files[chat.id] ?? [] { metadataByID[file.id] = file }
        for id in intent?.draftAttachmentIds ?? [] where seen.insert(id).inserted {
            if let file = metadataByID[id] {
                result.append(ComposerAttachment(
                    id: id,
                    name: file.name,
                    mimeType: file.mimeType ?? "application/octet-stream",
                    data: nil,
                    remoteFile: file,
                    sha256: file.sha256,
                    byteSize: file.byteSize,
                    state: file.state,
                    updatedAt: file.updatedAt
                ))
            } else {
                result.append(ComposerAttachment(
                    id: id,
                    name: ComposerAttachment.fallbackName,
                    mimeType: "application/octet-stream",
                    data: nil,
                    remoteFile: nil,
                    sha256: nil,
                    byteSize: nil,
                    state: "missing",
                    updatedAt: "composer"
                ))
            }
        }
        return result
    }
    private func messageEditor(suggestion: PromptSuggestion?, completes: Bool) -> some View {
        let hint = model.eligiblePromptSuggestion(suggestion, chat: chat)
        let scope = model.assignmentScope
        return ComposerMessageEditor(draft: model.draftText(for: chat.id),
            onType: { [model, chat] in model.typeDraft($0, chat: chat.id) },
            // Keep Send reachable beside the voice bar and keyboard at large
            // text sizes. The draft keeps its font and scrolls inside the editor.
            maximumLines: typeSize.isAccessibilitySize ? (compactVoiceBarVisible ? 1 : 2) : 6,
            label: "Message \(chat.title)", editable: !model.preparingSends.contains(chat.id) && !model.uploading.contains(chat.id) && !model.draftUnreadable(chat.id),
            placeholder: hint?.text ?? FollowUpBehavior.placeholder(queues: placeholderQueues),
            acceptSuggestion: hint.map { suggestion in { model.acceptPromptSuggestion(suggestion, chat: chat, scope: scope) } },
            dictation: model.dictation, conversationID: chat.id,
            canPasteImages: model.canAttach(chat) && !model.loadingPhotos.contains(chat.id),
            pasteImages: { providers in
                let scope = model.assignmentScope
                imagePasteTask?.cancel()
                imagePasteTask = Task { await model.stagePastedImages(providers, chat: chat, scope: scope) }
            },
            completion: completes ? completion : nil)
            .frame(maxWidth: .infinity)
            .accessibilityLabel("Message \(chat.title)")
            .accessibilityIdentifier("message-draft")
            .task(id: "\(chat.id)/\(sendQueues)") { await settlePlaceholder(sendQueues) }
            // Stop is reachable from the message too, keeping the draft.
            .accessibilityActions {
                if model.offersStop(chat), !model.stopping.contains(chat.id), !model.accessEnded {
                    Button("Stop reply") { Task { await model.stop(chat) } }
                }
            }
            .onChange(of: model.composerCompletionContext(chat), initial: true) { _, context in
                completion.configure(context,
                    loadCommands: { [model] in try await model.loadComposerCommands($0, family: $1, inT3Code: $2) },
                    searchFiles: { [model] in try await model.searchComposerFiles($0, query: $1, family: $2) },
                    searchThreads: { [model] in try await model.searchComposerThreads($0, query: $1) })
                #if WONDER_DIAGNOSTICS
                if let text = DiagnosticSubagentFixture.composerPreviewText {
                    Task { @MainActor in
                        try? await Task.sleep(for: .milliseconds(400))
                        model.typeDraft(text, chat: chat.id)
                        completion.editorChanged(text: text, selection: NSRange(location: (text as NSString).length, length: 0))
                    }
                }
                #endif
            }
            .onDisappear { completion.reset() }
    }
}

/// A message kept from a chat that is no longer on the computer. It is never
/// sent on its own; the user copies its text, moves it here or discards it.
private struct OrphanedDraftNotice: View {
    let draft: OrphanedDraft
    let more: Int
    let error: String?
    let move: () -> Void
    let discard: () -> Void
    @State private var confirmingDiscard = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Label {
                    Text("Unsent message kept from “\(draft.title)”, a chat that’s no longer on your computer.")
                } icon: { Image(systemName: "tray.and.arrow.down") }
                .font(.caption).foregroundStyle(.secondary)
                Spacer(minLength: 0)
                Menu("Recover") {
                    Button("Copy text", systemImage: "doc.on.doc") { UIPasteboard.general.string = draft.text }
                    Button("Move to this chat", systemImage: "arrow.down.doc", action: move)
                    Button("Discard…", systemImage: "trash", role: .destructive) { confirmingDiscard = true }
                }
                .font(.caption.weight(.semibold))
                .accessibilityIdentifier("orphaned-draft-recover")
            }
            if draft.computerOnlyFileCount > 0 {
                Text(draft.computerOnlyFileCount == 1 ? "Its file was only on your computer and can’t be recovered."
                     : "\(draft.computerOnlyFileCount) of its files were only on your computer and can’t be recovered.")
                    .font(.caption2).foregroundStyle(.secondary)
            }
            if more > 0 {
                Text(more == 1 ? "1 more kept message after this one." : "\(more) more kept messages after this one.")
                    .font(.caption2).foregroundStyle(.secondary)
            }
            if let error { FailureDetails(message: error) }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("orphaned-draft-notice")
        .confirmationDialog("Discard this unsent message?", isPresented: $confirmingDiscard, titleVisibility: .visible) {
            Button("Discard", role: .destructive, action: discard)
        } message: {
            Text("It can’t be recovered afterward.")
        }
    }
}

/// How a turn that did not finish ended, with the reason its agent gave.
/// Outside the Working disclosure, so every detail level keeps it.
private struct TurnOutcomeMarker: View {
    let outcome: TurnOutcome
    let isProjectConversation: Bool

    var body: some View {
        let label = outcome.label(isProjectConversation: isProjectConversation)
        VStack(alignment: .leading, spacing: 2) {
            Label(label, systemImage: outcome.symbol)
                .font(.subheadline.weight(.medium))
                .foregroundStyle(outcome.kind == .failed ? Color.orange : Color.secondary)
            if let reason = outcome.reason {
                Text(reason)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
            }
        }
        .frame(maxWidth: 640, alignment: .leading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(outcome.reason.map { label + ". " + $0 } ?? label)
        .accessibilityIdentifier("turn-outcome:" + outcome.turnID)
    }
}

/// Keep the composer close to its display edge, including beside a terminal.
/// A leaf modifier keeps this spacing change out of the timeline's type depth.
private struct ConversationComposerDock: ViewModifier {
    func body(content: Content) -> some View {
        content.frame(maxWidth: 768).padding(.horizontal, 8).padding(.top, 4).padding(.bottom, 2)
            .frame(maxWidth: .infinity)
    }
}

/// The title pill's fill matches the buttons beside it: the system's glass, in the
/// navigation bar and in a Duo header whose sidebar button is glass too; the theme
/// surface where the Duo's side rail holds surface-filled buttons.
private struct TitleSurface: ViewModifier {
    let beside: Bool
    @Environment(\.wonderTheme) private var theme
    func body(content: Content) -> some View {
        if beside { content.background(theme.surface, in: Capsule()) }
        else { content.glassEffect(.regular.interactive(), in: .capsule) }
    }
}

/// The header's title pill with its chat-actions menu. Takes the menu items and
/// the pill as erased views, so this body's type stays shallow.
private struct ConversationTitleMenuView: View {
    let items: AnyView
    let pill: AnyView
    let accessibilityLabel: String
    let accessibilityValue: String
    /// The Duo's side rail is showing, with its surface-filled buttons.
    let besideRail: Bool

    var body: some View {
        Menu {
            items
        } label: {
            ViewThatFits(in: .horizontal) {
                pill.fixedSize(horizontal: true, vertical: false)
                pill
            }
            .padding(.horizontal, 12).padding(.vertical, 4)
            .frame(minHeight: 44)
            .modifier(TitleSurface(beside: besideRail))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityValue(accessibilityValue)
        .accessibilityHint("Show chat actions")
        .accessibilityIdentifier("conversation-title-menu")
    }
}
