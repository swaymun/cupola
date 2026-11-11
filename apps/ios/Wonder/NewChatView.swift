import SwiftUI
import Combine
import WonderPairing
import PhotosUI
import UniformTypeIdentifiers

/// Where people get Cupola for their computer: the public site's setup steps. The
/// source repository's releases page is private, so it would 404 for most people.
let wonderMacDownloadURL = URL(string: "https://getcupola.app/beta")!

/// First launch, before any computer is paired: what Wonder needs and how to start.
struct GetStartedView: View {
    let addComputer: () -> Void
    /// Shows the demo computer (`ConnectionLibrary.startDemo()`).
    var tryDemo: (() -> Void)? = nil
    var body: some View {
        ScrollView {
            VStack(spacing: 14) {
                Image(systemName: "laptopcomputer.and.iphone")
                    .font(.system(size: 44)).foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Text("Chat with agents on your computer")
                    .font(.title3.weight(.semibold)).multilineTextAlignment(.center)
                    .accessibilityAddTraits(.isHeader)
                Text("Cupola connects to Claude and Codex running on your own computer through the cupola command there. Commands and files stay on your computer.")
                    .multilineTextAlignment(.center).foregroundStyle(.secondary)
                Button(action: addComputer) {
                    Label("Add computer", systemImage: "plus").frame(maxWidth: 280)
                }
                .buttonStyle(.wonderProminent).controlSize(.large)
                .accessibilityIdentifier("welcome-add-computer")
                if let tryDemo {
                    Button(action: tryDemo) {
                        Label("Try the demo", systemImage: "play.circle").frame(maxWidth: 280)
                    }
                    .buttonStyle(.bordered).controlSize(.large)
                    .accessibilityHint("Shows sample chats on a demo computer. Nothing is sent anywhere.")
                    .accessibilityIdentifier("welcome-try-demo")
                }
                Link(destination: wonderMacDownloadURL) {
                    Label("Get Cupola for your computer", systemImage: "arrow.down.circle")
                }
                .accessibilityIdentifier("welcome-mac-download")
                Text("Pair while this device and your computer are on the same Wi-Fi.")
                    .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
                    .padding(.top, 6)
            }
            .frame(maxWidth: 420)
            .padding(.horizontal, 24).padding(.vertical, 32)
            .frame(maxWidth: .infinity)
        }
        .defaultScrollAnchor(.center)
        .accessibilityIdentifier("welcome")
    }
}

/// Shown instead of first-run setup while the saved computers can't be opened.
/// Each failure keeps its own advice: unlock, restart, or remove and pair again
/// (in Settings, which confirms first).
struct SavedComputersRecoveryView: View {
    let failure: SavedComputersFailure
    let retry: () -> Void
    let openSettings: () -> Void
    var body: some View {
        ContentUnavailableView {
            Label(failure == .locked ? "Unlock to see your computers" : "Couldn’t open your computers",
                  systemImage: failure == .locked ? "lock" : "exclamationmark.triangle")
        } description: {
            Text(failure.message)
        } actions: {
            if failure == .unreadable {
                Button("Open Settings", action: openSettings)
                    .buttonStyle(.wonderProminent)
                    .accessibilityIdentifier("saved-computers-open-settings")
            } else {
                Button("Try again", action: retry)
                    .buttonStyle(.wonderProminent)
                    .accessibilityIdentifier("saved-computers-retry")
            }
        }
        .accessibilityIdentifier("saved-computers-recovery")
    }
}

/// Durable new-chat drafts: one per Mac, plus the last explicitly chosen Mac.
enum NewChatDraftStore {
    struct RecoveryRecord: Identifiable {
        let id: String
        let draft: NewChatDraft?
    }
    static let changed = Notification.Name("wonder.newchat.draftsChanged")
    private static func key(_ host: String) -> String { "wonder.newchat.draft." + Data(host.utf8).base64EncodedString() }
    private static let hostKey = "wonder.newchat.host"
    private static var draftDirectory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("NewChatDrafts", isDirectory: true)
    }
    private static func url(_ key: String) -> URL { draftDirectory.appendingPathComponent(ConversationFile.digest(Data(key.utf8))) }
    private static func bytes(_ key: String) throws -> Data? {
        // Never replace an unreadable file with an older preferences copy.
        if FileManager.default.fileExists(atPath: url(key).path) { return try Data(contentsOf: url(key)) }
        return UserDefaults.standard.data(forKey: key)
    }
    private static func read(_ key: String) -> NewChatDraft? {
        guard let data = try? bytes(key) else { return nil }
        return try? JSONDecoder().decode(NewChatDraft.self, from: data)
    }
    private static func keys(host: String) -> [String] {
        let prefix = key(host)
        return UserDefaults.standard.dictionaryRepresentation().keys
            .filter { $0 == prefix || $0.hasPrefix(prefix + ".") }.sorted()
    }
    /// Existing unconfirmed sends become the open draft or this temporary list.
    /// Unreadable indexed records remain visible, including missing files.
    /// Queued requests belong exclusively to the still-active offline outbox.
    static func recoveryRecords(host: String) -> [RecoveryRecord] {
        keys(host: host).compactMap { stored in
            let draft = read(stored)
            guard draft == nil || (stored.hasPrefix(key(host) + ".pending.") && draft?.isQueued == false) else { return nil }
            return RecoveryRecord(id: stored, draft: draft)
        }
    }
    /// Explicit owner discard removes aliases of this request, never another
    /// destination's newer draft. Keep the index if deleting bytes fails.
    static func discard(_ record: RecoveryRecord, host: String) -> Bool {
        let owned = keys(host: host)
        guard owned.contains(record.id), read(record.id) == record.draft, record.draft?.isQueued != true else { return false }
        let targets = owned.filter { stored in
            stored == record.id || (record.draft != nil && read(stored)?.requestID == record.draft?.requestID)
        }
        do {
            for target in targets {
                if FileManager.default.fileExists(atPath: url(target).path) { try FileManager.default.removeItem(at: url(target)) }
                UserDefaults.standard.removeObject(forKey: target)
            }
            NotificationCenter.default.post(name: changed, object: nil)
            return true
        } catch { return false }
    }
    static func load(host: String) -> NewChatDraft? { read(key(host)) }
    private static func pendingKey(_ host: String, _ request: String) -> String { key(host) + ".pending." + request }
    @discardableResult static func save(_ draft: NewChatDraft, host: String, separately: Bool = false, makeCurrent: Bool = true) -> Bool {
        var draft = draft
        // A launch that may have started stays marked until it starts or the
        // draft is retired: a save from a view that hasn't seen the mark yet
        // (debounce, disappear) must not erase it and allow a duplicate launch.
        if draft.t3LaunchUnconfirmed == nil, draft.startsInT3Code == true {
            draft.t3LaunchUnconfirmed = [key(host), destinationKey(host, draft.destination), pendingKey(host, draft.requestID)].lazy
                .compactMap { read($0) }
                .first { $0.requestID == draft.requestID && $0.t3LaunchUnconfirmed != nil }?
                .t3LaunchUnconfirmed
        }
        do {
            try FileManager.default.createDirectory(at: draftDirectory, withIntermediateDirectories: true)
            let data = try JSONEncoder().encode(draft)
            // A pending request is separate from the editable destination draft.
            // Reviewing it must not overwrite newer words for that project.
            let archived = draft.isSubmitted || separately || UserDefaults.standard.bool(forKey: pendingKey(host, draft.requestID))
            var targets = archived
                ? [pendingKey(host, draft.requestID), key(host)]
                : [destinationKey(host, draft.destination), key(host)]
            if archived, read(destinationKey(host, draft.destination))?.requestID == draft.requestID {
                targets.insert(destinationKey(host, draft.destination), at: 0)
            }
            // A Shortcut's pending message must not replace the open composer.
            if !makeCurrent { targets.removeAll { $0 == key(host) } }
            // A default draft restored after a failed read must not erase the
            // unreadable original. Stage an indexed copy before touching it.
            var recoveredUnreadable = false
            for target in targets where UserDefaults.standard.object(forKey: target) != nil && read(target) == nil {
                let original = try bytes(target) ?? Data()
                let recovery = key(host) + ".unreadable." + UUID().uuidString
                try original.write(to: url(recovery), options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
                UserDefaults.standard.set(true, forKey: recovery)
                recoveredUnreadable = true
            }
            for target in targets {
                try data.write(to: url(target), options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
                // Small ownership index supports scoped unpair cleanup.
                UserDefaults.standard.set(true, forKey: target)
            }
            if archived || recoveredUnreadable { NotificationCenter.default.post(name: changed, object: nil) }
            return true
        } catch { return false }
    }
    static func load(host: String, destination: ChatDestination) -> NewChatDraft? { read(destinationKey(host, destination)) }
    static func savedMessages(host: String) -> [NewChatDraft] {
        let prefix = key(host) + ".pending."
        return UserDefaults.standard.dictionaryRepresentation().keys.sorted()
            .filter { $0.hasPrefix(prefix) }.compactMap { read($0) }
    }
    /// Requests waiting for the Mac (the New chat outbox), oldest first.
    static func queued(host: String) -> [NewChatDraft] {
        savedMessages(host: host).filter(\.isQueued).sorted { ($0.queuedAt ?? "") < ($1.queuedAt ?? "") }
    }
    static func removePending(host: String, requestID: String) {
        let target = pendingKey(host, requestID)
        try? FileManager.default.removeItem(at: url(target))
        UserDefaults.standard.removeObject(forKey: target)
    }
    /// Escape an unconfirmed send without changing or losing its retry identity.
    static func startNew(from submitted: NewChatDraft, host: String) -> NewChatDraft? {
        guard submitted.isSubmitted, save(submitted, host: host) else { return nil }
        let next = editableDraft(after: submitted, host: host)
        return save(next, host: host) ? next : nil
    }
    /// A confirmed handoff retires only this request. An independent draft made
    /// while it was unconfirmed keeps its words and attachments.
    static func complete(_ submitted: NewChatDraft, host: String) -> NewChatDraft? {
        let next = editableDraft(after: submitted, host: host)
        guard save(next, host: host) else { return nil }
        removePending(host: host, requestID: submitted.requestID)
        return next
    }
    static func reject(_ submitted: NewChatDraft, host: String) -> NewChatDraft? {
        var next = submitted
        next.rejectSubmission()
        let saved = submitted.destination.flatMap { load(host: host, destination: $0) }
        let independent = saved.map { !$0.isSubmitted && $0.requestID != submitted.requestID } ?? false
        guard save(next, host: host, separately: independent) else { return nil }
        removePending(host: host, requestID: submitted.requestID)
        return next
    }
    private static func editableDraft(after submitted: NewChatDraft, host: String) -> NewChatDraft {
        var next = submitted
        next.completeSubmission()
        if let destination = submitted.destination,
           let saved = load(host: host, destination: destination),
           !saved.isSubmitted, saved.requestID != submitted.requestID {
            next = read(pendingKey(host, saved.requestID)) ?? saved
        }
        return next
    }
    private static func destinationKey(_ host: String, _ destination: ChatDestination?) -> String {
        let encoded = (try? JSONEncoder().encode(destination)) ?? Data()
        return key(host) + "." + encoded.base64EncodedString()
    }
    static func remove(host: String) {
        let prefix = key(host)
        for stored in UserDefaults.standard.dictionaryRepresentation().keys where stored == prefix || stored.hasPrefix(prefix + ".") {
            try? FileManager.default.removeItem(at: url(stored))
            UserDefaults.standard.removeObject(forKey: stored)
        }
        if lastHost == host { lastHost = nil }
    }
    /// An empty project draft starts with the owner's words and attachments,
    /// and a new Project draft starts from that Project's last provider; a
    /// saved one keeps its own choices. Both destinations remain recoverable, including when a write fails.
    static func selecting(_ destination: ChatDestination, from draft: NewChatDraft, host: String, project: ProjectSummary? = nil,
                          capabilities: (AgentFamily) -> ProviderCapabilities,
                          startable: (AgentFamily) -> Bool = { _ in true }) -> NewChatDraft {
        guard !draft.isSubmitted, draft.destination != destination else { return draft }
        var next = load(host: host, destination: destination) ?? NewChatDraft()
        if case .project = destination, !next.isSubmitted, next.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
           (next.attachments ?? []).isEmpty, (next.annotations ?? []).isEmpty {
            next.text = draft.text; next.attachments = draft.attachments
        }
        guard save(draft, host: host) else { return draft }
        next.arrive(at: destination, project: project, capabilities: capabilities, startable: startable)
        guard save(next, host: host) else {
            save(draft, host: host)
            return draft
        }
        return next
    }
    private static var attachmentDirectory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("NewChatAttachments", isDirectory: true)
    }
    nonisolated static func stage(_ file: StagedFile) throws -> NewChatAttachment {
        let descriptor = NewChatAttachment(file)
        try FileManager.default.createDirectory(at: attachmentDirectory, withIntermediateDirectories: true)
        guard UUID(uuidString: file.id) != nil else { throw FileFailure.integrity }
        try file.data.write(to: attachmentDirectory.appendingPathComponent(file.id), options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        return descriptor
    }
    nonisolated static func files(_ descriptors: [NewChatAttachment]) throws -> [StagedFile] {
        try descriptors.map { descriptor in
            guard UUID(uuidString: descriptor.id) != nil else { throw FileFailure.integrity }
            let path = attachmentDirectory.appendingPathComponent(descriptor.id)
            let size = try path.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
            guard size == descriptor.byteSize else { throw FileFailure.integrity }
            try AttachmentLimits.validate(byteSize: size, mimeType: descriptor.mimeType)
            let data = try Data(contentsOf: path, options: .alwaysMapped)
            guard ConversationFile.digest(data) == descriptor.sha256 else { throw FileFailure.integrity }
            return try StagedFile(id: descriptor.id, name: descriptor.name, mimeType: descriptor.mimeType, data: data, source: AttachmentFileSource(url: path))
        }
    }
    /// Export the full draft (including file notes) and verified attachment
    /// bytes. A partial export is cleaned up and never presented as complete.
    nonisolated static func export(_ draft: NewChatDraft) throws -> (directory: URL, urls: [URL]) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        do {
            let staged = try files(draft.attachments ?? [])
            let record = root.appendingPathComponent("message.json")
            try JSONEncoder().encode(draft).write(to: record, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
            let urls = try staged.map { file in
                let url = root.appendingPathComponent(file.id + "-" + URL(fileURLWithPath: file.name).lastPathComponent)
                try file.data.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
                return url
            }
            return (root, [record] + urls)
        } catch { try? FileManager.default.removeItem(at: root); throw error }
    }
    nonisolated static func pruneAttachments() {
        let indexed = UserDefaults.standard.dictionaryRepresentation().keys.filter { $0.hasPrefix("wonder.newchat.draft.") }
        let drafts = indexed.compactMap { read($0) }
        // An unreadable record may still own these bytes. Only an explicit
        // discard allows cleanup to decide that they are unreferenced.
        guard drafts.count == indexed.count else { return }
        let references = Set(drafts.flatMap { ($0.attachments ?? []).map(\.id) })
        guard let files = try? FileManager.default.contentsOfDirectory(at: attachmentDirectory,
            includingPropertiesForKeys: [.contentModificationDateKey], options: [.skipsHiddenFiles]) else { return }
        for file in files where UUID(uuidString: file.lastPathComponent) != nil && !references.contains(file.lastPathComponent) {
            // Leave in-flight imports time to publish their metadata.
            if let date = try? file.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
               Date().timeIntervalSince(date) > 3600 { try? FileManager.default.removeItem(at: file) }
        }
    }
    static let lastHostChanged = Notification.Name("wonder.newchat.lastHostChanged")
    static var lastHost: String? {
        get { UserDefaults.standard.string(forKey: hostKey) }
        set {
            guard newValue != lastHost else { return }
            UserDefaults.standard.set(newValue, forKey: hostKey)
            NotificationCenter.default.post(name: lastHostChanged, object: nil)
        }
    }
}

/// Start in T3 Code's send: the draft is saved first, the launch goes once,
/// and a failure keeps the draft (and the choice) as typed. T3 Code has no
/// retry key, so nothing here sends it again.
@MainActor enum T3CodeSubmission {
    enum Outcome: Equatable {
        case cancelled
        /// Not started, in words for the owner; the draft is kept. A launch
        /// T3 Code turned away re-reads its connection (`refreshed`), so an
        /// expired authorization stops offering Start in T3 Code.
        case failed(String, refreshed: T3CodeStatus? = nil)
        /// The answer was lost and T3 Code may have started it; the draft is
        /// kept, marked (`t3LaunchUnconfirmed`), so sending it again needs
        /// the owner's say-so, also after New chat reopens.
        case mayHaveStarted(String, refreshed: T3CodeStatus)
        /// Started: the next draft, and the thread's conversation to open
        /// (nil when the Mac answered none).
        case started(next: NewChatDraft, conversationID: String?)
    }

    static func launch(_ draft: NewChatDraft, host: String, projectID: String, project: ProjectSummary?,
                       planToggle: Bool, model: ConnectionModel, catalog: T3CodeCatalog? = nil) async -> Outcome {
        guard NewChatDraftStore.save(draft, host: host) else {
            return .failed("Your message could not be saved. Free some storage and try again.")
        }
        let request = T3CodeLaunch.request(for: draft, body: draft.text, project: project, planToggle: planToggle,
                                           deviceID: model.connection?.credential.deviceId, catalog: catalog)
        let launched: T3CodeLaunched
        do {
            launched = try await model.launchInT3Code(projectID: projectID, request: request)
        } catch is CancellationError {
            return .cancelled
        } catch {
            let message = T3CodeLaunch.failureMessage(error, macName: model.macName)
            guard T3CodeLaunch.mayHaveStarted(error) else {
                return .failed(message, refreshed: await readStatus(model))
            }
            // Record "may have started" before any further await, so a quit or
            // crash during the status read can't restore an unguarded Send.
            // The owner may have reopened New chat and edited while the launch waited.
            let current = NewChatDraftStore.load(host: host)
            let marked = T3CodeLaunch.markingUnconfirmed(draft, message: message, current: current,
                saved: draft.destination.flatMap { NewChatDraftStore.load(host: host, destination: $0) })
            NewChatDraftStore.save(marked, host: host, makeCurrent: current == nil || current?.requestID == marked.requestID)
            return .mayHaveStarted(message, refreshed: await readStatus(model))
        }
        // Started: retire the draft even if New chat moved on, so it can't start twice.
        guard let next = NewChatDraftStore.complete(draft, host: host) else {
            return .failed("The thread started in T3 Code, but a new draft could not be saved. Free some storage.")
        }
        return .started(next: next, conversationID: launched.conversationId)
    }

    /// The "may have started" mark the saved draft still carries for this
    /// request (NewChatDraftStore.save keeps it until the launch starts).
    static func storedUnconfirmed(host: String, requestID: String) -> String? {
        NewChatDraftStore.load(host: host).flatMap { $0.requestID == requestID ? $0.t3LaunchUnconfirmed : nil }
    }

    /// T3 Code's connection now; unreadable reads as unavailable, so Send
    /// says so instead of checking forever.
    static func readStatus(_ model: ConnectionModel, refresh: Bool = false) async -> T3CodeStatus {
        await model.t3CodeStatus(refresh: refresh, catalog: true) ?? T3CodeStatus(state: "unavailable")
    }
}

/// Coalesces new-chat edits, but flushes durable intent synchronously before
/// suspension. Backgrounding does not make a SwiftUI view disappear, and its
/// delayed task may never run before the process is killed.
@MainActor final class NewChatDraftPersistence: ObservableObject {
    @Published private(set) var failure: String?
    private var pending: (host: String, draft: NewChatDraft)?
    private var task: Task<Void, Never>?
    private var lifecycle: AnyCancellable?

    init(notifications: NotificationCenter = .default) {
        lifecycle = notifications.publisher(for: UIApplication.willResignActiveNotification)
            .merge(with: notifications.publisher(for: UIApplication.didEnterBackgroundNotification),
                   notifications.publisher(for: UIApplication.willTerminateNotification))
            .sink { [weak self] _ in
                // UIKit delivers its application lifecycle notifications on MainActor.
                MainActor.assumeIsolated { self?.flush() }
            }
    }
    func update(host: String?, draft: NewChatDraft) {
        task?.cancel()
        pending = host.map { ($0, draft) }
        guard pending != nil else { return }
        task = Task { [weak self] in
            do { try await Task.sleep(for: .milliseconds(300)) } catch { return }
            guard !Task.isCancelled else { return }
            self?.flush()
        }
    }
    func flush() {
        task?.cancel()
        task = nil
        guard let pending else { return }
        if NewChatDraftStore.save(pending.draft, host: pending.host) {
            self.pending = nil
            failure = nil
        } else {
            failure = "Your draft could not be saved. Free some storage before leaving this chat."
        }
    }
    deinit { task?.cancel() }
}

/// A draft chat addressed to a Mac and one of its projects. Choosing either
/// never starts work; the shell remembers whether this or a chat was last open.
struct NewChatView: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    /// The shell already knows about a rail or display fold, independently of size classes.
    var keepsComposerAtBottom = false
    @State private var composerBottomInset: CGFloat = 0
    @State private var hostID: String?
    @State private var showingComputer = false
    @State private var draft = NewChatDraft()
    @StateObject private var draftPersistence = NewChatDraftPersistence()
    @State private var sending = false
    @State private var sendTask = NewChatSendTask()
    @State private var failure: String?
    @State private var addingProject = false
    @State private var pairing = false
    @State private var linkedProjectID: String?
    @State private var linkedProjectChecked = false
    @State private var linkRetry = 0
    @State private var pendingLinkedRequest: NewChatRequest?

    private var model: ConnectionModel? {
        guard let hostID, let saved = library.saved.connections.first(where: { $0.credential.hostInstallationId == hostID }) else { return nil }
        return library.model(for: saved)
    }

    var body: some View {
        Group {
            if let model, model.isDemo {
                DemoNewChatNotice(showChats: { shell.showChats() }, addComputer: { pairing = true })
            } else if let model {
                ProjectRouteGate(projects: model.projects) {
                    // A project this device already knows opens at once, even
                    // while the Mac reconnects; the check below still catches
                    // one that has since been removed.
                    if let linkedProjectID,
                       (pendingLinkedRequest != nil ||
                        model.projects.project(linkedProjectID)?.isIncluded != true ||
                        draft.destination != .project(id: linkedProjectID)) {
                        linkedProjectRecovery(model)
                    } else {
                        NewChatContent(library: library, shell: shell, model: model, projects: model.projects,
                                       hostID: $hostID, draft: persistedDraft, sending: $sending, sendTask: $sendTask, failure: $failure,
                                       addingProject: $addingProject, pairing: $pairing,
                                       linkedProjectID: $linkedProjectID, showingComputer: $showingComputer,
                                       keepsComposerAtBottom: keepsComposerAtBottom)
                        // Same as an open chat: sink into the home-indicator inset, never under the keyboard.
                        .onGeometryChange(for: CGFloat.self) { $0.safeAreaInsets.bottom } action: { if $0 > 0 { composerBottomInset = $0 } }
                        .padding(.bottom, keepsComposerAtBottom && composerBottomInset > 0 && composerBottomInset <= 60 ? -14 : 0)
                    }
                }
            } else {
                noConnection
            }
        }
        // Drawn like a conversation: the theme's backdrop and navigation bar.
        .background { ThemeBackdrop() }
        .toolbarBackground(theme.chrome, for: .navigationBar)
        .toolbarBackground(keepsComposerAtBottom ? .hidden : .visible, for: .navigationBar)
        // No "New chat" heading: the composer is the page (owner feedback 3, 40).
        .navigationTitle("")
        .navigationBarTitleDisplayMode(.inline)
        .onAppear(perform: restore)
        .onChange(of: model.map(ObjectIdentifier.init)) { previous, current in
            guard previous != current else { return }
            sendTask.cancel()
            sending = false
            failure = nil
        }
        .onChange(of: shell.newChatRequest) { _, request in apply(request) }
        .onChange(of: library.saved.connections.map(\.credential.hostInstallationId)) { _, hosts in
            if let hostID, !hosts.contains(hostID), linkedProjectID == nil {
                self.hostID = nil; draft = NewChatDraft()
            }
            if hostID == nil, linkedProjectID == nil { restore() }
        }
        .task(id: "\(hostID ?? ""):\(linkedProjectID ?? ""):\(model?.macConnected == true):\(linkRetry)") {
            guard linkedProjectID != nil, pendingLinkedRequest == nil, let model else { return }
            linkedProjectChecked = false
            guard model.macConnected == true else { linkedProjectChecked = true; return }
            await model.projects.refresh()
            if !Task.isCancelled { linkedProjectChecked = true }
        }
        .onChange(of: PersistenceKey(host: hostID, draft: draft), initial: true) { _, _ in
            draftPersistence.update(host: hostID, draft: draft)
        }
        .onChange(of: draftPersistence.failure) { _, issue in
            if let issue { failure = issue }
        }
        .onDisappear {
            draftPersistence.update(host: hostID, draft: draft)
            draftPersistence.flush()
            sendTask.cancel()
            sending = false
        }
        .task { await Task.detached { NewChatDraftStore.pruneAttachments() }.value }
        .sheet(isPresented: $pairing) { PairComputerView(model: library.pairingModel()) }
    }

    private struct PersistenceKey: Hashable { let host: String?; let draft: NewChatDraft }
    private var persistedDraft: Binding<NewChatDraft> {
        Binding(get: { draft }, set: { next in
            draft = next
            // Stage directly from the editor binding, before a lifecycle notification
            // can arrive ahead of SwiftUI's next onChange/render pass.
            draftPersistence.update(host: hostID, draft: next)
        })
    }

    /// Only shown while no Mac is paired; otherwise a Mac is always selected.
    @ViewBuilder private var noConnection: some View {
        if let failure = library.savedComputersFailure {
            // Computers may be saved; they just couldn't be opened. Pairing
            // needs them first, so first-run setup would only fail.
            SavedComputersRecoveryView(failure: failure, retry: { library.retrySavedComputers() },
                                       openSettings: { shell.show(.settings) })
        } else if linkedProjectID == nil {
            GetStartedView(addComputer: { pairing = true }, tryDemo: { library.startDemo() })
        } else {
            VStack(spacing: 16) {
                Spacer()
                Text("Linked computer is not paired").font(.title3.weight(.semibold))
                Text("Pair the computer from this link to open its Project.").foregroundStyle(.secondary)
                Button("Add computer", systemImage: "plus") { pairing = true }.buttonStyle(.wonderProminent)
                Spacer()
            }.frame(maxWidth: .infinity).padding()
        }
    }

    private func linkedProjectRecovery(_ model: ConnectionModel) -> some View {
        Group {
            if let pendingLinkedRequest {
                ContentUnavailableView {
                    Label("Couldn’t open Project", systemImage: "folder.badge.questionmark")
                } description: {
                    Text("Your current draft could not be saved. Free some storage.")
                } actions: {
                    Button("Try again") { apply(pendingLinkedRequest) }
                }
            } else if let linkedProjectID, model.projects.project(linkedProjectID)?.isIncluded == true,
               draft.destination != .project(id: linkedProjectID) {
                ContentUnavailableView {
                    Label("Couldn’t open Project", systemImage: "folder.badge.questionmark")
                } description: {
                    Text("Your draft could not be saved. Free some storage.")
                } actions: {
                    Button("Try again") {
                        if let hostID {
                            let destination = ChatDestination.project(id: linkedProjectID)
                            if NewChatDraftStore.save(draft, host: hostID) {
                                draft = NewChatDraftStore.load(host: hostID, destination: destination)
                                    ?? NewChatDraft(destination: destination)
                            }
                        }
                    }
                }
            } else if model.macConnected == nil {
                ProgressView("Opening Project…")
            } else if model.macConnected == false {
                ContentUnavailableView {
                    Label("Computer offline", systemImage: "wifi.slash")
                } description: {
                    Text("Opens when \(model.macName) is back.")
                }
            } else if !linkedProjectChecked || model.projects.loadingProjects {
                ProgressView("Checking Project…")
            } else {
                ContentUnavailableView {
                    Label(model.projects.failure == nil ? "Project unavailable" : "Couldn’t check project",
                          systemImage: "folder.badge.questionmark")
                } description: {
                    Text(model.projects.failure == nil
                         ? "This Project is no longer available on \(model.macName). Check the link or choose another Project."
                         : "Check the connection to \(model.macName).")
                } actions: {
                    Button("Try again") { linkRetry += 1 }
                    Button("Choose a Project") { linkedProjectID = nil; draft.destination = nil }
                }
            }
        }
        .accessibilityIdentifier("linked-project-recovery")
    }

    private func restore() {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-reset-new-chat-annotations-preview") {
            NewChatDraftStore.remove(host: "studio")
        }
        if ProcessInfo.processInfo.arguments.contains("-reset-new-chat-pending-preview") {
            // Owned offline fixture; no real pairing, messages or model work.
            NewChatDraftStore.remove(host: "studio")
            let image = UIGraphicsImageRenderer(size: CGSize(width: 80, height: 60)).pngData { context in
                UIColor.systemOrange.setFill(); context.fill(CGRect(x: 0, y: 0, width: 80, height: 60))
            }
            if let file = try? StagedFile(name: "saved-image.png", mimeType: "image/png", data: image),
               let attachment = try? NewChatDraftStore.stage(file) {
                var pending = NewChatDraft(destination: .project(id: "preview-project"), text: "Saved unconfirmed message",
                    family: .codex, model: "preview-model")
                pending.attachments = [attachment]; pending.freeze()
                NewChatDraftStore.save(pending, host: "studio")
            }
            NewChatDraftStore.lastHost = "studio"
        }
        if ProcessInfo.processInfo.arguments.contains("-reset-new-chat-recovery-preview") {
            NewChatDraftStore.remove(host: "studio")
            for (id, text) in [("first", "Check the release notes before sending."), ("second", "Keep these project ideas for later.")] {
                var pending = NewChatDraft(destination: .project(id: "preview-project"), text: text,
                    family: .codex, model: "preview-model", requestID: id)
                pending.freeze()
                NewChatDraftStore.save(pending, host: "studio", makeCurrent: false)
            }
            let prefix = "wonder.newchat.draft." + Data("studio".utf8).base64EncodedString()
            UserDefaults.standard.set(Data("{unreadable fixture".utf8), forKey: prefix + ".pending.corrupt")
            NewChatDraftStore.save(NewChatDraft(destination: .project(id: "preview-project")), host: "studio")
            NewChatDraftStore.lastHost = "studio"
        }
        #endif
        guard hostID == nil else { apply(shell.newChatRequest); return }
        let saved = library.saved.connections
        let hosts = saved.map(\.credential.hostInstallationId)
        // One paired Mac needs no choice. With several, the last one used wins,
        // then one that answers, then the first, so this surface is never a dead end.
        var chosen = NewChatDraftStore.lastHost.flatMap { hosts.contains($0) ? $0 : nil }
        if chosen == nil, saved.count > 1 {
            chosen = saved.first { library.model(for: $0).macConnected == true }?.credential.hostInstallationId
        }
        if chosen == nil { chosen = hosts.first }
        if let chosen {
            hostID = chosen
            draft = NewChatDraftStore.load(host: chosen) ?? NewChatDraft()
        }
        apply(shell.newChatRequest)
    }

    private func apply(_ request: NewChatRequest?) {
        guard let request else { return }
        if request.exactProject, let host = request.host,
           case .project(let id)? = request.destination {
            // A link opens this project's own draft. Never carry words or
            // attachments from another Mac or project into the linked target.
            let destination = ChatDestination.project(id: id)
            if host != hostID || draft.destination != destination {
                if !sendTask.cancelAfterSaving(draft, hostID: hostID) {
                    linkedProjectID = id
                    pendingLinkedRequest = request
                    failure = "Your draft could not be saved. Free some storage and try again."
                    shell.newChatRequest = nil
                    return
                }
                sending = false
                failure = nil
                hostID = host
                draft = NewChatDraftStore.load(host: host, destination: destination)
                    ?? NewChatDraft(destination: destination)
                NewChatDraftStore.lastHost = host
            }
            linkedProjectID = id
            pendingLinkedRequest = nil
            linkedProjectChecked = false
            shell.newChatRequest = nil
            return
        }
        if let host = request.host, host != hostID, !switchHost(host) {
            shell.newChatRequest = nil
            return
        }
        linkedProjectID = nil
        pendingLinkedRequest = nil
        linkedProjectChecked = false
        if let destination = request.destination, !draft.isSubmitted, let hostID {
            draft = NewChatDraftStore.selecting(destination, from: draft, host: hostID, project: projectSummary(destination),
                capabilities: { [model] in model?.projects.capabilities($0) ?? .unoffered },
                startable: { [model] in model?.projects.isStartable($0) ?? true })
        }
        if request.showComputer, request.host == hostID { showingComputer = true }
        shell.newChatRequest = nil
    }

    private func projectSummary(_ destination: ChatDestination) -> ProjectSummary? {
        if case .project(let id) = destination { return model?.projects.project(id) }
        return nil
    }

    private func switchHost(_ host: String) -> Bool {
        guard host != hostID else { return true }
        guard sendTask.cancelAfterSaving(draft, hostID: hostID) else {
            failure = "Your draft could not be saved. Free some storage and try again."
            return false
        }
        sending = false
        draft = NewChatDraft.switching(from: draft, toSaved: NewChatDraftStore.load(host: host))
        hostID = host
        NewChatDraftStore.lastHost = host
        failure = nil
        return true
    }
}

/// The exact linked destination must respond to catalog changes even when the
/// connection library itself has not changed.
private struct ProjectRouteGate<Content: View>: View {
    @ObservedObject var projects: ProjectLibrary
    let content: () -> Content
    init(projects: ProjectLibrary, @ViewBuilder content: @escaping () -> Content) {
        self.projects = projects
        self.content = content
    }
    var body: some View { content() }
}

/// The one path that turns a frozen New chat request into its Project thread
/// and first message. New chat's Send and the outbox drain both use it; the
/// request ID makes a repeat continue the same creation.
@MainActor enum NewChatSubmission {
    enum Failure: Error { case attachmentUnavailable }

    /// nil once `stillCurrent` says the caller no longer owns the request.
    static func create(_ draft: NewChatDraft, projectID: String, body: String, device: String, model: ConnectionModel,
                       fromOutbox: Bool = false, stillCurrent: () -> Bool,
                       prepared: (NewChatDraft) -> Void) async throws -> ChatSummary? {
        let noteDraftID = draft.annotations?.first?.draftID ?? draft.requestID
        let descriptors = draft.attachments ?? []
        var files: [StagedFile]
        do { files = try await Task.detached { try NewChatDraftStore.files(descriptors) }.value }
        catch { throw Failure.attachmentUnavailable }
        guard stillCurrent() else { return nil }
        var draft = draft
        let conversation: String
        if let existing = draft.preparedConversationID { conversation = existing }
        else {
            let response = try await model.projects.createThread(projectID: projectID, draft: draft, body: body, deviceID: device)
            guard stillCurrent() else { return nil }
            guard let created = response.conversation.conversationId else { throw PairingFailure.response(500) }
            conversation = created
            draft.preparedConversationID = created
            prepared(draft)
        }
        for pending in draft.annotations ?? [] {
            files.append(try pending.file(conversationID: conversation, projectID: projectID,
                                          folderID: draft.folderId ?? "", draftID: noteDraftID,
                                          rootsRevision: draft.rootsRevision ?? -1))
        }
        let detail = try await model.projects.loadDetail(conversation)
        guard stillCurrent() else { return nil }
        let chat = model.projectChat(detail)
        try await model.prepareCreation(chat, body: body, requestID: draft.requestID, files: files,
                                        skills: draft.codexSkills, fromOutbox: fromOutbox)
        guard stillCurrent() else { return nil }
        return chat
    }
}

/// A send can outlive the view's pairing, including when a replacement uses the
/// same Mac and the draft still has the same request ID.
@MainActor struct NewChatSendAttempt {
    let scope: String
    let hostID: String
    let requestID: String

    func applies(to model: ConnectionModel, hostID: String?, draft: NewChatDraft) -> Bool {
        scope == model.assignmentScope && !model.accessEnded && hostID == self.hostID &&
            draft.requestID == requestID && !Task.isCancelled
    }
}

/// The view releases its send control immediately when its paired model changes.
/// Finishing a cancelled request cannot release a newer send's control.
@MainActor struct NewChatSendTask {
    private(set) var generation = UUID()
    private(set) var task: Task<Void, Never>?
    var isActive: Bool { task != nil }

    mutating func start(_ task: Task<Void, Never>) {
        guard self.task == nil else { task.cancel(); return }
        self.task = task
    }
    mutating func cancel() {
        generation = UUID()
        task?.cancel()
        task = nil
    }
    mutating func cancelAfterSaving(_ draft: NewChatDraft, hostID: String?) -> Bool {
        if let hostID, !NewChatDraftStore.save(draft, host: hostID) { return false }
        cancel()
        return true
    }
    mutating func finish(_ expected: UUID) -> Bool {
        guard generation == expected else { return false }
        task = nil
        return true
    }
}

/// Where New chat's setup (computer, project and composer) sits.
enum NewChatLayout {
    /// A tall, wide window (an iPad) centres the setup in the empty page; a
    /// phone keeps it at the bottom, by the keyboard.
    static func centresSetup(horizontal: UserInterfaceSizeClass?, vertical: UserInterfaceSizeClass?,
                             keepsComposerAtBottom: Bool = false) -> Bool {
        horizontal == .regular && vertical == .regular && !keepsComposerAtBottom
    }
}

private struct NewChatContent: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    @ObservedObject var model: ConnectionModel
    @ObservedObject var projects: ProjectLibrary
    @Binding var hostID: String?
    @Binding var draft: NewChatDraft
    @Binding var sending: Bool
    @Binding var sendTask: NewChatSendTask
    @Binding var failure: String?
    @Binding var addingProject: Bool
    @Binding var pairing: Bool
    @Binding var linkedProjectID: String?
    @Binding var showingComputer: Bool
    var keepsComposerAtBottom = false
    @Environment(\.dynamicTypeSize) private var typeSize
    @State private var composerControlWidth: CGFloat = 0
    @State private var setupFold: CGRect?
    private var separatesComposerChoices: Bool { typeSize.isAccessibilitySize || composerControlWidth < 300 }

    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @Environment(\.verticalSizeClass) private var verticalSizeClass

    @State private var importing = false
    @State private var selectingPhoto = false
    @State private var selectedPhotos: [PhotosPickerItem] = []
    @State private var importingFor: String?
    @State private var attachmentTask: Task<Void, Never>?
    @State private var loadingAttachment = false
    @State private var attachmentPresentation: [ComposerAttachment] = []
    @State private var previewAttachment: ComposerAttachment?
    @State private var showingCamera = false
    @State private var showingSketch = false
    @State private var sketchScope: String?
    @State private var sketchContext: String?
    @State private var cameraScope: String?
    @State private var showingModel = false
    @State private var hasDisplayFold = false
    @State private var modelSearch = ""
    @State private var showingConnectionPicker = false
    @State private var showingProjectPicker = false
    @State private var addProjectAfterPicker = false
    @State private var pairAfterConnectionPicker = false
    @State private var recoveryRecords: [NewChatDraftStore.RecoveryRecord] = []
    @State private var discardingRecovery: NewChatDraftStore.RecoveryRecord?
    @State private var recoveryShare: NewChatRecoveryShare?
    @State private var recoveryExportDirectory: URL?
    @State private var recoveryShareTask: Task<Void, Never>?
    @State private var preparingRecoveryShare = false
    @State private var pendingFolderSelection: ProjectFolder?
    @State private var showingFolderChangeConfirmation = false
    @State private var editingAnnotation: PendingNewChatAnnotation?
    /// The inline `@` file list; see `ConversationView.completion`.
    @State private var completion = ComposerCompletionModel()
    /// The T3 Code connection of the computer it was read from, for Start in
    /// T3 Code; another computer's, or none, hides it.
    @State private var t3Read: (scope: String, status: T3CodeStatus)?
    /// The latest T3 Code read; an older one that answers late is dropped.
    @State private var t3ReadGeneration = 0
    @State private var t3CatalogSearchError: String?
    @State private var t3DefaultsRead: (scope: String, enabled: Bool?)?
    @State private var providerProblem: ConnectionModel.SendRecoveryReason?
    @State private var recoveryRefresh = 0
    @State private var checkingRecovery = false
    @State private var recoveryCheckID = UUID()
    @State private var showingT3Connection = false
    @StateObject private var t3ComputerControl = ComputerUseSettingsModel()
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.voiceScene) private var voiceScene
    @State private var showingVoice = false
    /// A thread just started in T3 Code, until it shows in Chats.
    @State private var t3Notice: String?
    @State private var selectionNote: String?
    @State private var destinationNote: String?
    private var selectionChangeMessage: String? { destinationNote ?? selectionNote }
    @State private var switchedComputer = false
    @State private var destinationChanged = false
    /// The owner flipped the destination switch; the next destination change may explain what it adjusted.
    @State private var ownerToggledDestination = false
    @State private var pendingComputerSelection: NewChatDraft?
    #if WONDER_DIAGNOSTICS
    @State private var driverMenu: String?
    #endif
    @State private var confirmingT3Resend = false

    private var draftChat: ChatSummary {
        ChatSummary(conversationId: "new-chat:" + draft.requestID, title: "New chat",
            lastMessagePreview: nil, lastMessageAt: nil, messageCount: 0, deliveryState: nil,
            hasUnread: false, isArchived: false, isPinned: false)
    }

    /// The host-level computer view has no conversation; the host reserves this ID for it.
    private var hostViewChat: ChatSummary {
        ChatSummary(conversationId: "wonder-host-view", title: "Computer", lastMessagePreview: nil,
            lastMessageAt: nil, messageCount: 0, deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)
    }

    private var canAttach: Bool {
        !sending && !draft.isSubmitted && !loadingAttachment && draft.attachmentCount < 4
    }

    /// This computer's T3 Code connection; nil until read from it.
    private var t3Status: T3CodeStatus? { t3Read?.scope == model.assignmentScope ? t3Read?.status : nil }
    /// Send starts this draft's thread in T3 Code on the Mac, and only there.
    private var savedT3Default: Bool? {
        t3DefaultsRead?.scope == model.assignmentScope ? t3DefaultsRead?.enabled : nil
    }
    private var startsInT3Code: Bool {
        draft.usesT3Code(defaultEnabled: savedT3Default == true, status: t3Status, project: project)
    }
    private var recoveryNeedsModel: Bool { providerProblem?.needsModel == true }
    private var t3DefaultFallback: String? {
        guard !draft.isSubmitted, savedT3Default == true, draft.startsInT3Code == nil, !startsInT3Code else { return nil }
        if let problem = T3CodeLaunch.connectionProblem(t3Status, computer: model.macName) { return problem }
        if project?.primaryFolder == nil { return "This Project has no folder for T3 Code." }
        if let issue = t3Status?.launchCatalogError { return issue }
        return "T3 Code doesn’t support this provider in Cupola yet."
    }
    /// Start in T3 Code is offered for this Project on this Mac, or stays
    /// shown once chosen so it can be turned off while T3 Code is unavailable.
    private var showsT3CodeToggle: Bool {
        startsInT3Code || (!draft.isSubmitted && t3Status?.state == "connected" && project?.primaryFolder != nil
            && !(t3Status?.launchCatalog?.families.isEmpty ?? t3Status?.launchProviders?.isEmpty ?? true))
    }
    /// Why a draft that chose T3 Code can't be sent now; nil when it can.
    private var t3Blocked: String? {
        if startsInT3Code, t3Status?.state == "connected" {
            guard let catalog = t3Status?.launchCatalog else {
                return t3Status?.launchCatalogError ?? "Update Cupola on this computer to read T3 Code’s models."
            }
            guard selectedModel != nil, let provider = catalog.provider(for: draft.model) else {
                return "T3 Code has no available model for this chat. Choose another destination."
            }
            guard let mode = draft.t3RuntimeMode, provider.runtimeModes.contains(mode) else {
                return "T3 Code hasn’t offered permission options for this model. Update T3 Code or start in Cupola."
            }
        }
        var resolved = draft
        resolved.startsInT3Code = startsInT3Code
        return T3CodeLaunch.blockedReason(resolved, status: t3Status, project: project,
                                   connected: model.macConnected == true, macName: model.macName)
    }

    private var project: ProjectSummary? {
        if case .project(let id) = draft.destination { return projects.project(id) }
        return nil
    }
    private var projectFilesChat: ChatSummary? {
        guard let project, project.isIncluded,
              let folder = project.folders.first(where: { $0.id == (draft.folderId ?? project.primaryFolder?.id) }) else { return nil }
        return ChatSummary(conversationId: "project-files:\(project.id):\(folder.id)",
            title: project.name, lastMessagePreview: nil, lastMessageAt: nil, messageCount: 0,
            deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)
    }
    /// `@` searches the chosen Project folder when the provider takes file
    /// mentions. A new chat has no session yet, so it has no command list.
    private var completionContext: ComposerCompletionModel.Context? {
        guard let family = draft.family, let chat = projectFilesChat else { return nil }
        return .init(conversationID: chat.id, family: family, commands: true, files: capabilities.fileMentions, threads: capabilities.threadTools, inT3Code: startsInT3Code)
    }
    private var draftAnnotationActions: WorkspaceDraftAnnotationActions? {
        guard projectFilesChat != nil, let project,
              let folderID = project.folders.first(where: { $0.id == (draft.folderId ?? project.primaryFolder?.id) })?.id,
              let hostID else { return nil }
        let draftID = draft.annotations?.first?.draftID ?? draft.requestID
        let scope = model.assignmentScope
        let sketchContext = sketchContextToken
        let request = draft.requestID
        let reason: String? = sending || draft.isSubmitted ? "Wait for this message before adding a comment."
            : loadingAttachment ? "Wait for the file to finish attaching."
            : draft.attachmentCount >= 4 ? "Remove an attachment to add a comment."
            : model.macConnected != true ? "Reconnect to your computer to add a comment."
            : nil
        return WorkspaceDraftAnnotationActions(projectID: project.id, draftID: draftID,
            unavailableReason: reason, sketchUnavailableReason: sketchUnavailableReason, stage: { annotation in
                guard scope == model.assignmentScope, self.hostID == hostID,
                      !sending, !draft.isSubmitted, !loadingAttachment, draft.attachmentCount < 4,
                      draft.destination == .project(id: project.id),
                      annotation.projectId == project.id,
                      annotation.conversationId == "new-chat:" + draftID else { throw FileFailure.integrity }
                let pending = try PendingNewChatAnnotation(annotation: annotation, draftID: draftID,
                                                           folderID: folderID, rootsRevision: project.rootsRevision)
                var next = draft
                try next.addAnnotation(pending)
                guard NewChatDraftStore.save(next, host: hostID) else { throw FileFailure.integrity }
                draft = next
                return pending.id
            }, stageSketch: { png in await attachSketch(png, scope: scope, requestID: request, expectedContext: sketchContext) },
            contains: { id in draft.annotations?.contains(where: { $0.id == id }) == true },
            noteRevision: { rootID, path, sha256 in
                guard scope == model.assignmentScope, self.hostID == hostID,
                      draft.destination == .project(id: project.id),
                      (draft.folderId ?? project.primaryFolder?.id) == folderID,
                      (draft.annotations?.first?.draftID ?? draft.requestID) == draftID else { return }
                var next = draft
                next.noteWorkspaceRevision(folderID: folderID, draftID: draftID,
                                           rootID: rootID, path: path, sha256: sha256)
                if next != draft, NewChatDraftStore.save(next, host: hostID) { draft = next }
            })
    }
    private var sketchContextToken: String {
        model.cameraContextID.uuidString + ":" + model.assignmentScope + ":" + draft.requestID
            + ":" + String(describing: draft.destination) + ":" + (draft.folderId ?? "")
    }

    private var sketchUnavailableReason: String? {
        if sending || draft.isSubmitted { return "Wait for this message before attaching a sketch." }
        if loadingAttachment { return "Wait for the current file to finish attaching." }
        if draft.attachmentCount >= 4 { return "Remove an attachment to add a sketch. You can attach up to four files." }
        return nil
    }

    private func attachSketch(_ data: Data, scope: String, requestID: String, expectedContext: String) async -> SketchAttachmentResult {
        guard !Task.isCancelled, expectedContext == sketchContextToken, scope == model.assignmentScope,
              requestID == draft.requestID else { return .cancelled }
        if let reason = sketchUnavailableReason { return .failed(reason) }
        loadingAttachment = true
        defer { loadingAttachment = false }
        do {
            let file = try await SketchExport.attachment(data)
            let item = try await Task.detached { try NewChatDraftStore.stage(file) }.value
            guard !Task.isCancelled, expectedContext == sketchContextToken, scope == model.assignmentScope,
                  requestID == draft.requestID else { return .cancelled }
            if sending || draft.isSubmitted { return .failed("Wait for this message before attaching a sketch.") }
            if draft.attachmentCount >= 4 { return .failed("Remove an attachment to add a sketch. You can attach up to four files.") }
            var next = draft
            try next.addAttachments([item])
            guard let hostID, NewChatDraftStore.save(next, host: hostID) else { return .failed("The sketch could not be saved. Try attaching it again.") }
            draft = next
            return .attached
        } catch is CancellationError { return .cancelled }
        catch { return .failed(error.localizedDescription) }
    }

    private var displayedAttachments: [ComposerAttachment] {
        attachmentPresentation + (draft.annotations ?? []).map { pending in
            ComposerAttachment(id: pending.id, name: String(pending.annotation.path.split(separator: "/").last ?? "File"),
                mimeType: ArtifactAnnotation.mimeType, data: nil, remoteFile: nil,
                sha256: nil, byteSize: nil, state: "available", updatedAt: "")
        }
    }
    private var familyModels: [AgentOptions.Model] {
        pickerModels.filter { !$0.hidden && $0.family == draft.family }
    }
    private var pickerModels: [AgentOptions.Model] { startsInT3Code ? t3Status?.launchCatalog?.models ?? [] : projects.options?.models ?? [] }
    private var pickerFamilies: [AgentFamily] { startsInT3Code ? t3Status?.launchCatalog?.families ?? [] : AgentFamily.allCases.filter(projects.families.lists) }
    private func pickerAvailable(_ family: AgentFamily) -> Bool { startsInT3Code ? pickerFamilies.contains(family) : projects.isAvailable(family) }
    private var selectedModel: AgentOptions.Model? { familyModels.first { $0.id == draft.model } }
    /// A short list has no search field, so a search typed for another provider does not apply.
    private var searchingModels: Bool {
        familyModels.count > ProjectModelPicker.searchThreshold
            || (startsInT3Code && t3Status?.launchCatalog?.providers.contains { $0.family == draft.family && ($0.omittedModels ?? 0) > 0 } == true)
    }
    private var t3CatalogMissing: Bool { startsInT3Code && t3Status?.launchCatalog == nil }
    /// What the chip shows: the model Send runs, and T3 Code when that is where it runs.
    private var chipTitle: String { startsInT3Code && !modelTitle.contains("T3 Code") ? modelTitle + " · T3 Code" : modelTitle }
    /// The model's name without its provider ("Opus 5.5", not "Claude Opus 5.5"): the provider icon already says it, and the chip must fit a folded Duo.
    private var modelTitle: String {
        let full = ProjectModelPicker.title(model: selectedModel, family: draft.family)
        guard selectedModel != nil, let provider = draft.family?.title else { return full }
        guard full.lowercased().hasPrefix(provider.lowercased()) else { return full }
        let afterPrefix = full.dropFirst(provider.count)
        // The prefix must end at a word boundary ("Claude Opus", not "Claudette").
        guard let boundary = afterPrefix.first, boundary == " " || boundary == ":" || boundary == "-" else { return full }
        let rest = afterPrefix.drop { $0 == " " || $0 == ":" || $0 == "-" }
        return rest.isEmpty ? full : String(rest)
    }
    /// A chosen speed other than standard, shown as a bolt beside the model.
    private var speed: AgentOptions.Choice? {
        selectedModel?.serviceTiers?.first { $0.id == draft.serviceTier && $0.id != "default" }
    }
    private var placeholder: String { "Message" }
    /// What the draft's provider offers. A draft whose provider is not chosen yet is shown as Codex.
    private var capabilities: ProviderCapabilities {
        var value = projects.capabilities(draft.family ?? .codex) // provider identity
        if startsInT3Code { value.plan = .toggle; value.skills = false; value.attachments = false }
        return value
    }
    private var canSend: Bool {
        guard (draft.codexSkills ?? []).isEmpty || (draft.family != nil && capabilities.skills) else { return false }
        if model.dictation.blocksSending(conversationID: draftChat.id) { return false }
        // Offline, Send puts the request in the New chat outbox.
        guard !sending, model.connection != nil, !model.accessEnded, model.macConnected != nil else { return false }
        let text = (draft.submittedBody ?? draft.text).trimmingCharacters(in: .whitespacesAndNewlines)
        guard (!text.isEmpty || draft.attachmentCount > 0 || model.dictation.hasUncommittedWords(conversationID: draftChat.id)),
              draft.attachmentCount <= 4,
              text.utf8.count <= 65536, !loadingAttachment else { return false }
        guard case .project = draft.destination else { return false }
        // Read the saved choice before a fresh draft can send. A failed read
        // offers Retry rather than silently ignoring an unknown default.
        if !draft.isSubmitted, draft.startsInT3Code == nil, model.macConnected == true, savedT3Default == nil { return false }
        // An explicit T3 choice waits until T3 Code can start it.
        if startsInT3Code { return t3Blocked == nil && selectedModel != nil && !text.isEmpty }
        return !checkingRecovery && providerProblem == nil && project != nil && selectedModel != nil && draft.family.map(projects.isAvailable) == true
    }

    /// Without a Project Send stays off, so say how to get one.
    private var noProjects: String? {
        NoProjectsNotice.text(supportsProjects: projects.supportsProjects, projects: projects.projects, macName: model.macName)
    }

    private func noProjectsState(_ notice: String) -> some View {
        VStack(spacing: 8) {
            Text("Add a Project to start chatting").font(.headline)
            Text(notice + ". A Project is a folder on your computer that the agent works in.")
                .font(.subheadline).foregroundStyle(.secondary)
            Button("Add Project", systemImage: "folder.badge.plus") { addingProject = true }
                .buttonStyle(.wonderProminent).padding(.top, 4)
                .disabled(sending || draft.isSubmitted)
                .accessibilityIdentifier("new-chat-add-project")
        }
        .multilineTextAlignment(.center)
        .frame(maxWidth: 420).padding(.horizontal)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("new-chat-no-projects")
    }

    private var optionsLoadKey: String { model.assignmentScope + (model.macConnected == true ? ":ready" : ":waiting") }

    private var baseContent: some View {
        VStack(spacing: 0) {
            ZStack(alignment: .bottom) {
                Color.clear
                    .contentShape(Rectangle())
                    .onTapGesture { ComposerKeyboard.dismiss() }
                if !model.queuedNewChats.isEmpty || !recoveryRecords.isEmpty {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 16) {
                            if !recoveryRecords.isEmpty {
                                NewChatRecoveryList(records: recoveryRecords,
                                    review: review, share: shareRecovery,
                                    discard: { discardingRecovery = $0 })
                                    .disabled(sending || preparingRecoveryShare)
                                if preparingRecoveryShare { ProgressView("Preparing to share…") }
                            }
                            queuedNewChats
                        }.padding(.horizontal).padding(.vertical, 8)
                    }
                        .defaultScrollAnchor(.bottom)
                        .scrollBounceBehavior(.basedOnSize)
                        .frame(maxWidth: 768)
                        .accessibilityIdentifier("new-chat-outbox")
                } else if let noProjects {
                    noProjectsState(noProjects).frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            if typeSize.isAccessibilitySize {
                ScrollView { composer }
                    // Keep typing and Send in reach when setup choices exceed
                    // a short pane at accessibility sizes; choices scroll above.
                    .defaultScrollAnchor(.bottom)
                    .scrollBounceBehavior(.basedOnSize)
                    .accessibilityIdentifier("new-chat-setup-scroll")
                    .layoutPriority(1)
            } else {
                composer
            }
            if centred && !typeSize.isAccessibilitySize {
                // As much room below the setup as above it.
                Color.clear.frame(maxWidth: .infinity, maxHeight: .infinity)
                    .contentShape(Rectangle())
                    .onTapGesture { ComposerKeyboard.dismiss() }
                    .accessibilityHidden(true)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .onGeometryChange(for: Bool.self) { DuoFold.region(in: $0) != nil } action: { hasDisplayFold = $0 }
        // Rerun once the connection is ready: a launch-time read can fail
        // before the session renews, and the model list must not stay empty.
        #if WONDER_DIAGNOSTICS
        .background { DiagnosticDriverSurface(surface: pickerDriverSurface).frame(width: 0, height: 0).accessibilityHidden(true) }
        #endif
        .task(id: optionsLoadKey) {
            settleDestination()
            await projects.loadOptions()
            if projects.supportsProjects == nil { await projects.refresh() }
            settleDestination()
            ensureModel()
        }
        .task(id: optionsLoadKey + ":t3") {
            // Reads are scoped to their computer, so another computer's answer is never used.
            // Keep the last answer while rereading: clearing it flips the destination for a moment.
            await refreshT3Status()
        }
        .task(id: optionsLoadKey + ":recovery:\(draft.family?.rawValue ?? ""):\(draft.model ?? ""):\(startsInT3Code):\(recoveryRefresh)") {
            let checkID = UUID(); recoveryCheckID = checkID
            checkingRecovery = !startsInT3Code
            defer { if recoveryCheckID == checkID { checkingRecovery = false } }
            providerProblem = nil
            guard !startsInT3Code else { return }
            let scope = model.assignmentScope, request = draft.requestID, family = draft.family, selected = draft.model
            let reason = await model.sendRecoveryReason(family: family, selectedModel: selected, refresh: recoveryRefresh > 0)
            guard !Task.isCancelled, scope == model.assignmentScope, request == draft.requestID,
                  family == draft.family, selected == draft.model else { return }
            providerProblem = reason
        }
        // T3 Code's authorization can end while New chat stays open; the
        // task above rereads it each time New chat appears.
        .onChange(of: scenePhase) { _, phase in if phase == .active { Task { await refreshT3Status() } } }
        .alert("Send again anyway?", isPresented: $confirmingT3Resend) {
            Button("Cancel", role: .cancel) {}
            Button("Send again anyway") { T3CodeLaunch.confirmResend(&draft); startSendConfirmed() }
                .accessibilityIdentifier("new-chat-t3-resend")
        } message: {
            Text(T3CodeLaunch.resendWarning(macName: model.macName))
        }
        .alert("Discard this message?", isPresented: Binding(
            get: { discardingRecovery != nil }, set: { if !$0 { discardingRecovery = nil } }), presenting: discardingRecovery) { record in
            Button("Cancel", role: .cancel) {}
            Button("Discard", role: .destructive) { discardRecovery(record) }
                .accessibilityIdentifier("new-chat-confirm-discard")
        } message: { _ in
            Text("This removes the saved copy and its attachments from this device. It can’t undo anything already started on your computer.")
        }
        .sheet(item: $recoveryShare, onDismiss: {
            if let recoveryExportDirectory { try? FileManager.default.removeItem(at: recoveryExportDirectory) }
            recoveryExportDirectory = nil
        }) { item in NewChatRecoveryActivity(items: item.items) }
        .onChange(of: projects.projects.filter(\.isIncluded).map(\.id)) { _, _ in settleDestination(); ensureModel() }
        .onChange(of: draft.family) { _, _ in ensureModel() }
        .onChange(of: projects.options?.models) { _, _ in ensureModel() }
        .onChange(of: startsInT3Code) { previous, current in
            destinationChanged = true
            let explicit = ownerToggledDestination
            ownerToggledDestination = false
            selectionNote = nil
            if current { destinationNote = nil }
            if previous, !current { adoptT3PermissionForNative() }
            ensureModel(explaining: explicit)
        }
        .onChange(of: t3Status?.launchCatalog) { _, _ in ensureModel() }
        .task(id: model.assignmentScope + ":" + draft.requestID) {
            reloadRecovery()
            model.reloadQueuedNewChats()
        }
        .onReceive(NotificationCenter.default.publisher(for: NewChatDraftStore.changed)) { _ in reloadRecovery() }
        .onDisappear {
            recoveryShareTask?.cancel()
            if let recoveryExportDirectory { try? FileManager.default.removeItem(at: recoveryExportDirectory) }
            recoveryExportDirectory = nil
        }
    }

    /// New chats waiting for the Mac (the New chat outbox), oldest first.
    private var queuedNewChats: some View {
        let device = model.connection?.credential.deviceId
        return VStack(alignment: .trailing, spacing: 8) {
            ForEach(model.queuedNewChats, id: \.requestID) { item in
                OutboxMessageRow(id: item.requestID, text: item.submittedBody ?? item.text,
                                 attachmentCount: item.attachmentCount, refusal: item.outboxRefusal,
                                 previousPairing: item.submittedDeviceID != device,
                                 sending: model.outboxInFlight.contains(item.requestID),
                                 destination: projectName(item.destination),
                                 canEdit: item.canEditQueued, canRetry: item.outboxRetryable == true,
                                 edit: { editQueued(item) },
                                 cancel: { model.cancelQueuedNewChat(item) },
                                 retry: { model.retryQueuedNewChat(item) })
            }
        }
        .frame(maxWidth: .infinity, alignment: .trailing)
    }

    private func projectName(_ destination: ChatDestination?) -> String? {
        if case .project(let id) = destination { return projects.project(id)?.name }
        return nil
    }

    /// File, photo and camera pickers; erased so the sheet chain below does not
    /// nest on this one's opaque type.
    private var pickerContent: some View {
        // Erase baseContent so its ~24 modifiers do not nest on its opaque type
        // during one metadata walk on the physical phone's main stack.
        AnyView(baseContent).fileImporter(isPresented: $importing, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            guard importingFor == draft.requestID, canAttach else { return }
            if case .success(let urls) = result, !urls.isEmpty {
                // Files are read one at a time and added together, so a
                // selection over the four-file limit leaves the draft unchanged.
                importAttachment {
                    guard urls.count <= 4 - draft.attachmentCount else { throw FileFailure.tooLarge }
                    return try await Task.detached {
                        try urls.map { url in
                            try Task.checkCancellation()
                            let access = url.startAccessingSecurityScopedResource()
                            defer { if access { url.stopAccessingSecurityScopedResource() } }
                            let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
                            try AttachmentLimits.validate(byteSize: size, mimeType: UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? "application/octet-stream")
                            return try StagedFile(name: url.lastPathComponent,
                                mimeType: UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? "application/octet-stream", data: Data(contentsOf: url, options: .alwaysMapped))
                        }
                    }.value
                }
            }
        }
        .photosPicker(isPresented: $selectingPhoto, selection: $selectedPhotos,
                      maxSelectionCount: max(1, 4 - draft.attachmentCount),
                      selectionBehavior: .ordered, matching: .any(of: [.images, .videos]), preferredItemEncoding: .current)
        .sheet(isPresented: $showingCamera, onDismiss: { cameraScope = nil }) {
            if let cameraScope {
                CameraCaptureView(chatID: draft.requestID, chatTitle: "New chat", originatingScope: cameraScope,
                    currentContextToken: model.cameraContextID.uuidString + ":" + draft.requestID,
                    attachVideo: { url in
                        guard cameraScope == model.assignmentScope, canAttach else { return .cancelled }
                        let request = draft.requestID
                        do {
                            let file = try await MediaPreparation.video(url)
                            let item = try await Task.detached { try NewChatDraftStore.stage(file) }.value
                            guard !Task.isCancelled, cameraScope == model.assignmentScope, request == draft.requestID, canAttach else { return .cancelled }
                            try draft.addAttachments([item])
                            return .attached
                        } catch {
                            failure = AttachmentUploadFailure.message(for: error)
                            return .failed
                        }
                    },
                    attachPhoto: { data in
                        guard cameraScope == model.assignmentScope, canAttach else { return .cancelled }
                        let request = draft.requestID
                        do {
                            let file = try await ConnectionModel.prepareCameraAttachment(data)
                            let item = try await Task.detached { try NewChatDraftStore.stage(file) }.value
                            guard !Task.isCancelled, cameraScope == model.assignmentScope, request == draft.requestID, canAttach else { return .cancelled }
                            try draft.addAttachments([item])
                            return .attached
                        } catch { return .failed }
                    })
            }
        }
        .sheet(isPresented: $showingSketch, onDismiss: { sketchScope = nil; sketchContext = nil }) {
            if let sketchScope, let sketchContext {
                let request = draft.requestID
                SketchSheet(contextToken: sketchContextToken) { png in
                    await attachSketch(png, scope: sketchScope, requestID: request, expectedContext: sketchContext)
                }
            }
        }
    }

    private var attachmentContent: some View {
        AnyView(pickerContent)
        .sheet(isPresented: Binding(get: { showingModel && !hasDisplayFold }, set: { showingModel = $0 })) { modelSheet }
        .sheet(isPresented: $showingVoice) { VoiceSheet(controller: model.voice, macName: model.macName, scene: shell) }
        .sheet(isPresented: $showingT3Connection, onDismiss: { Task { await refreshT3Status() } }) {
            NavigationStack {
                ConnectedAppsView(model: model, computerControl: t3ComputerControl)
            }
            .environment(\.settingsInlineNavigation, false)
            .presentationDragIndicator(.hidden)
        }
        .sheet(item: $editingAnnotation) { pending in
            NewChatAnnotationEditor(pending: pending) { note in
                guard let hostID, !sending, !draft.isSubmitted,
                      draft.annotations?.contains(where: { $0.id == pending.id }) == true else { throw FileFailure.integrity }
                var next = draft
                try next.updateAnnotation(id: pending.id, note: note)
                guard NewChatDraftStore.save(next, host: hostID) else { throw FileFailure.integrity }
                draft = next
            }
        }
        .fullScreenCover(isPresented: $showingComputer) {
            NavigationStack { ComputerSessionView(model: model, chat: hostViewChat) } // theme-exempt: ComputerSessionView draws theme.computerBackground
        }
        .onChange(of: showingComputer) { _, showing in
            if showing { model.dictation.captureControlsHidden(conversationID: draftChat.id) }
        }
        .task(id: selectedPhotos) {
            let photos = selectedPhotos
            guard !photos.isEmpty, importingFor == draft.requestID, canAttach else { return }
            let request = draft.requestID
            loadingAttachment = true
            defer { loadingAttachment = false; selectedPhotos = [] }
            // Each photo is read and saved in the order picked; one that can't
            // be read doesn't cost the others.
            var items: [NewChatAttachment] = []
            do {
                guard photos.count <= 4 - draft.attachmentCount else { throw FileFailure.tooLarge }
                for photo in photos {
                    guard let file = try? await MediaPreparation.chosen(photo) else { continue }
                    try Task.checkCancellation()
                    items.append(try await Task.detached { try NewChatDraftStore.stage(file) }.value)
                }
                guard request == draft.requestID, !Task.isCancelled else { return }
                if !items.isEmpty { try draft.addAttachments(items) }
                guard items.count < photos.count else { return }
            } catch is CancellationError { return }
            catch { items = [] }
            let limits = "Videos must be 1 GB (1,024 MB) or smaller. PNG, transparent and animated images must be 8 MB or smaller."
            failure = photos.count == 1 ? "Could not attach this media. " + limits
                : items.isEmpty ? "Could not attach these items. Use up to four attachments. " + limits
                : "\(photos.count - items.count) of \(photos.count) items could not be attached. " + limits
        }
        .task(id: draft.attachments) {
            let descriptors = draft.attachments ?? []
            do {
                let files = try await Task.detached { try NewChatDraftStore.files(descriptors) }.value
                guard !Task.isCancelled else { return }
                attachmentPresentation = files.map { ComposerAttachment(id: $0.id, name: $0.name, mimeType: $0.mimeType,
                    data: $0.data, remoteFile: nil, sha256: $0.sha256, byteSize: $0.data.count, state: "available", updatedAt: "", source: $0.source) }
            } catch {
                attachmentPresentation = descriptors.map { ComposerAttachment(id: $0.id, name: $0.name, mimeType: $0.mimeType,
                    data: nil, remoteFile: nil, sha256: $0.sha256, byteSize: $0.byteSize, state: "unavailable", updatedAt: "") }
                failure = "A saved attachment is unavailable. Remove it and attach the file again."
            }
        }
    }

    var body: some View {
        attachmentContent.fullScreenCover(item: $previewAttachment) { attachment in
            PhotoViewer(model: model, chat: ChatSummary(conversationId: draft.requestID, title: "New chat",
                lastMessagePreview: nil, lastMessageAt: nil, messageCount: 0, deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false),
                name: attachment.name, mimeType: attachment.mimeType, sourceID: attachment.id,
                localData: attachment.data, file: nil)
        }
        #if WONDER_DIAGNOSTICS
        .background(DiagnosticDriverSurface(surface: recoveryDriverSurface))
        #endif
        .onDisappear {
            attachmentTask?.cancel()
            model.dictation.captureControlsHidden(conversationID: draftChat.id)
        }
        .sheet(isPresented: $addingProject) {
            AddProjectView(model: model, library: projects) { saved in
                choose(destination: .project(id: saved.id), project: saved)
            }
        }
    }

    #if WONDER_DIAGNOSTICS
    /// Reads and drives the mounted composer; fixtures never substitute a recovery view.
    private var recoveryDriverSurface: DiagnosticTestDriver.Surface {
        .init(priority: showingModel ? 200 : 10, state: {
            ["route": "new-chat", "composer": ["text": draft.text, "attachmentCount": draft.attachmentCount,
                "attachmentHashes": (draft.attachments ?? []).map(\.sha256), "canSend": canSend,
                "outbox": model.queuedNewChats.map { ["text": $0.text, "attachmentHashes": ($0.attachments ?? []).map(\.sha256)] as [String: Any] }],
             "recovery": ["blocked": t3Blocked ?? "", "provider": providerProblem?.message ?? "", "failure": failure ?? ""],
             "sheets": showingModel ? ["model"] : []]
        }, controls: {
            if showingModel {
                return familyModels.map { option in
                    .init(id: "new-chat-model:" + option.id, label: option.displayName,
                          tap: { draft.chooseModel(option) })
                }
            }
            var controls: [DiagnosticTestDriver.Control] = [
                .init(id: "new-chat-draft", label: draft.text),
                .init(id: "new-chat-send", label: "Send", enabled: canSend, tap: { startSend() }),
                .init(id: "project-agent-picker", label: "Model", tap: { showingModel = true })]
            if model.macConnected == false, !startsInT3Code {
                controls.append(.init(id: "new-chat-offline", label: "Send queues this until the computer is back."))
            }
            if let providerProblem {
                controls.append(.init(id: "new-chat-provider-recovery", label: providerProblem.message))
                controls.append(.init(id: "new-chat-recovery-retry", label: recoveryNeedsModel ? "Choose model" : "Check again", tap: {
                    if recoveryNeedsModel { showingModel = true } else { recoveryRefresh += 1 }
                }))
            }
            if startsInT3Code, t3Status?.state == "expired" || t3Status?.state == "disconnected" {
                controls.append(.init(id: "new-chat-t3-reconnect", label: "Connect T3 Code", tap: { showingT3Connection = true }))
            }
            if startsInT3Code, model.macConnected == true && (t3Status?.running == false || t3Status?.reachable == false || t3Status?.state == "unavailable") {
                controls.append(.init(id: "new-chat-t3-retry", label: "Check again", tap: { Task { await model.check(renew: true, userInitiated: true); await refreshT3Status(refresh: true) } }))
            }
            if let t3Blocked { controls.append(.init(id: "new-chat-t3-note", label: t3Blocked)) }
            if showsT3CodeToggle, startsInT3Code {
                controls.append(.init(id: "composer-start-in-t3", label: "Start in T3 Code", tap: { toggleDestination() }))
            }
            return controls
        }, actions: ["type": { args in draft.text += args["text"] ?? "" },
                     "dismiss": { _ in showingModel = false; showingT3Connection = false },
                     "send": { _ in startSend() },
                     "open-thread": { args in
                         // wonderctl sends an empty host when none is named: use this draft's computer.
                         let named = args["host"].flatMap { $0.isEmpty ? nil : $0 }
                         guard let id = args["id"], let host = named ?? hostID,
                               library.saved.connections.contains(where: { $0.credential.hostInstallationId == host }) else {
                             throw DiagnosticTestDriver.DriverError("Thread and paired computer required")
                         }
                         shell.open(host: host, conversation: id)
                     }])
    }
    #endif

    private var composer: some View {
        // Each block is its own erased view so composer's generic type name stays
        // short on the physical phone's main stack.
        VStack(alignment: .leading, spacing: 8) {
            DictationControls(controller: model.dictation, model: model, chat: draftChat)
            AnyView(composerNotices)
            pickers
            AnyView(composerBox)
        }
        .frame(maxWidth: 768).padding(.horizontal, 8).padding(.top, 4).padding(.bottom, 2)
        .frame(maxWidth: .infinity)
    }

    private var composerNotices: some View {
        Group {
            if let id = draft.preparedConversationID, let progress = model.uploadProgress[id] {
                HStack {
                    ComposerUploadProgress(progress: progress)
                    Button("Cancel") { model.cancelUpload(id) }
                        .frame(minWidth: 44, minHeight: 44)
                                .accessibilityLabel("Cancel upload").accessibilityIdentifier("new-chat-upload-cancel")
                }
            }
            if draft.isSubmitted && !sending {
                VStack(alignment: .leading, spacing: 0) {
                    Text(draft.preparedConversationID.flatMap { model.uploadFailures[$0] }
                         ?? "Your computer hasn’t confirmed this message. The original stays saved.")
                        .font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("new-chat-pending-notice")
                    HStack {
                        DictationSendControls(controller: model.dictation, conversationID: draftChat.id) {
                            Button(action: startSend) { Text("Try again").frame(minHeight: 44) }
                                .disabled(!canSend).accessibilityIdentifier("new-chat-retry")
                        }
                        Button(action: startNewDraft) { Text("New draft").frame(minHeight: 44) }
                            .accessibilityIdentifier("new-chat-start-new-draft")
                    }.buttonStyle(.plain).font(.subheadline)
                }.padding(.horizontal, 4)
            }
            if let failure, startsInT3Code, t3UnconfirmedMessage == nil {
                // Not started: the draft stays as typed; the reason shows open.
                VStack(alignment: .leading, spacing: 4) {
                    Label("Didn’t start in T3 Code", systemImage: "exclamationmark.triangle")
                        .font(.caption.weight(.semibold))
                    Text(failure).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("new-chat-t3-failure-reason")
                    Button("Try again") { startSend() }
                        .font(.caption.weight(.semibold)).frame(minHeight: 44)
                        .disabled(sending || !canSend)
                        .accessibilityIdentifier("new-chat-t3-try-again")
                }
                .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                .accessibilityIdentifier("new-chat-t3-failure")
            } else if let failure, draft.preparedConversationID.flatMap({ model.uploadFailures[$0] }) == nil {
                FailureDetails(draft.isSubmitted ? "Not confirmed" : "Couldn’t send", message: failure)
            }
            if sending, startsInT3Code {
                Text("Starting in T3 Code on \(model.macName)…").font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                    .accessibilityIdentifier("new-chat-t3-starting")
            }
            if let t3UnconfirmedMessage {
                // Shown open, not in a disclosure: sending again could start a second thread.
                VStack(alignment: .leading, spacing: 2) {
                    Label(T3CodeLaunch.unconfirmedTitle, systemImage: "questionmark.circle")
                        .font(.caption.weight(.semibold))
                    Text(t3UnconfirmedMessage).font(.caption).foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("new-chat-t3-unconfirmed")
            }
            if let t3Notice {
                Text(t3Notice).font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                    .accessibilityIdentifier("new-chat-t3-started")
            }
            // Keep a destination adjustment in the initial reading order,
            // before large picker rows or a provider sign-in recovery notice.
            if let selectionChangeMessage {
                Text(selectionChangeMessage).font(.caption).foregroundStyle(.primary)
                    .lineLimit(nil).fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12).padding(.top, 4)
                    .accessibilityIdentifier("new-chat-selection-note")
            }
        }
    }

    private var composerBox: some View {
        VStack(spacing: 0) {
            AnyView(composerBoxStatus)
            AnyView(composerBoxInput)
        }
            .padding(5)
            .foregroundStyle(.primary)
            .background(theme.surface, in: RoundedRectangle(cornerRadius: 28))
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("new-chat-composer")
    }

    private var composerBoxStatus: some View {
        Group {
                if !displayedAttachments.isEmpty {
                    ComposerAttachmentStrip(attachments: displayedAttachments, imagePreviews: model.imagePreviews,
                        imagePreviewScope: model.imagePreviewScope, chatID: draft.requestID,
                        staleAnnotationIDs: Set((draft.annotations ?? []).filter(\.sourceChanged).map(\.id)),
                        removalDisabled: sending || draft.isSubmitted,
                        loadRemoteData: { _ in throw FileFailure.notUploaded }, openPhoto: { previewAttachment = $0 },
                        remove: { id in
                            if draft.annotations?.contains(where: { $0.id == id }) == true {
                                guard let hostID else { return }
                                var next = draft
                                next.removeAnnotation(id: id)
                                if NewChatDraftStore.save(next, host: hostID) { draft = next }
                                else { failure = "Could not save this change. Free some storage and try again." }
                            } else { draft.attachments?.removeAll { $0.id == id } }
                        }, editAnnotation: { item in
                            editingAnnotation = draft.annotations?.first(where: { $0.id == item.id })
                        })
                }
                let planChip = projects.supportsModes && draft.planMode == true && capabilities.plan == .toggle
                if planChip || startsInT3Code {
                    HStack(spacing: 6) {
                        if planChip { PlanModeChip(isDisabled: sending || draft.isSubmitted) { draft.planMode = nil } }
                        if startsInT3Code { t3CodeChip }
                        Spacer(minLength: 0)
                    }.padding(.horizontal, 8).padding(.top, 4)
                }
                if let reason = t3DefaultFallback {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(reason).font(.caption).foregroundStyle(.secondary)
                            .accessibilityIdentifier("new-chat-t3-default-fallback")
                        if t3Status?.state != "connected" {
                            Button { showingT3Connection = true } label: {
                                Text("Connect T3 Code").font(.caption)
                                    .frame(minHeight: 44).contentShape(Rectangle())
                            }
                                .accessibilityIdentifier("new-chat-t3-default-connect")
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12).padding(.top, 6)
                }
                if !draft.isSubmitted, draft.startsInT3Code == nil, t3DefaultsRead?.scope == model.assignmentScope, savedT3Default == nil {
                    HStack {
                        Text("Couldn’t read this computer’s new-chat default.").font(.caption).foregroundStyle(.secondary)
                        Button { Task { await refreshT3Status() } } label: {
                            Text("Retry").frame(minHeight: 44).contentShape(Rectangle())
                        }.accessibilityIdentifier("new-chat-t3-default-retry")
                    }.padding(.horizontal, 12)
                }
                if model.macConnected == false, !startsInT3Code {
                    (typeSize.isAccessibilitySize
                        ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4))
                        : AnyLayout(HStackLayout(alignment: .firstTextBaseline))) {
                        // The computer reconnects on its own; the red dot beside its name is the only status.
                        Text("Send queues this until the computer is back.")
                            .font(.caption).foregroundStyle(.primary).fixedSize(horizontal: false, vertical: true)
                            .accessibilityIdentifier("new-chat-offline")
                    }.frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                }
                if let providerProblem, !startsInT3Code {
                    (typeSize.isAccessibilitySize
                        ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4))
                        : AnyLayout(HStackLayout(alignment: .firstTextBaseline))) {
                        Text(providerProblem.message).font(.caption).foregroundStyle(.primary).fixedSize(horizontal: false, vertical: true)
                            .accessibilityIdentifier("new-chat-provider-recovery")
                        Button(recoveryNeedsModel ? "Choose model" : "Check again") {
                            if recoveryNeedsModel { showingModel = true } else { recoveryRefresh += 1 }
                        }
                            .font(.caption).foregroundStyle(Color(uiColor: .systemBlue)).frame(minHeight: 44)
                            .accessibilityIdentifier("new-chat-recovery-retry")
                    }.frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                }
                if startsInT3Code, let t3Blocked {
                    // Send waits; say why and what to do.
                    (typeSize.isAccessibilitySize
                        ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4))
                        : AnyLayout(HStackLayout(alignment: .firstTextBaseline))) {
                        Text(t3Blocked).font(.caption).foregroundStyle(.primary).fixedSize(horizontal: false, vertical: true)
                            .accessibilityIdentifier("new-chat-t3-note")
                        if t3Status?.state == "expired" || t3Status?.state == "disconnected" {
                            Button("Connect T3 Code") { showingT3Connection = true }
                                .font(.caption).foregroundStyle(Color(uiColor: .systemBlue)).frame(minHeight: 44)
                                .accessibilityIdentifier("new-chat-t3-reconnect")
                        } else if model.macConnected == true && (t3Status?.running == false || t3Status?.reachable == false || t3Status?.state == "unavailable") {
                            Button("Check again") { Task { await model.check(renew: true, userInitiated: true); await refreshT3Status(refresh: true) } }
                                .font(.caption).foregroundStyle(Color(uiColor: .systemBlue)).frame(minHeight: 44)
                                .accessibilityIdentifier("new-chat-t3-retry")
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12).padding(.top, 6)
                }
        }
    }

    private var composerBoxInput: some View {
        Group {
                SelectedCodexSkills(skills: draft.codexSkills ?? [], takesSkills: draft.family != nil && capabilities.skills, disabled: sending || draft.isSubmitted,
                    remove: { path in
                        var text = draft.text, skills = draft.codexSkills
                        CodexSkillSelection.remove(path: path, text: &text, skills: &skills)
                        draft.text = text; draft.codexSkills = skills
                    })
                if completionContext != nil {
                    ComposerSuggestionList(completion: completion)
                }
                BoundedComposerEditor(
                    text: Binding(get: { draft.submittedBody ?? draft.text }, set: { if !draft.isSubmitted { draft.text = $0 } }),
                    maximumLines: typeSize >= .accessibility3 ? 1 : typeSize.isAccessibilitySize ? 2 : 6,
                    label: placeholder, editable: !sending && !draft.isSubmitted, placeholder: placeholder,
                    dictation: model.dictation, conversationID: draftChat.id,
                    didCommitDictation: {
                        if let hostID, !NewChatDraftStore.save(draft, host: hostID) {
                            failure = "Your draft could not be saved. Free some storage before leaving this chat."
                        }
                    },
                    canPasteImages: canAttach, pasteImages: pasteImages,
                    completion: completionContext == nil ? nil : completion)
                    .frame(maxWidth: .infinity)
                    .accessibilityLabel(placeholder)
                    .accessibilityIdentifier("new-chat-draft")
                    .onChange(of: completionContext, initial: true) { _, context in
                        completion.configure(context,
                            loadCommands: { [model] in try await model.loadComposerCommands($0, family: $1, inT3Code: $2) },
                            searchFiles: { [model] in try await model.searchComposerFiles($0, query: $1, family: $2) },
                            searchThreads: { [model] in try await model.searchComposerThreads($0, query: $1) })
                    }
                    .onDisappear { completion.reset() }
                    .padding(.horizontal, 8)
                    .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { composerControlWidth = $0 }
                if project != nil, separatesComposerChoices {
                    projectSettings(placement: .choices).padding(.horizontal, 8)
                }
                HStack(alignment: .center, spacing: 4) {
                    Menu {
                        #if WONDER_DIAGNOSTICS
                        if ProcessInfo.processInfo.arguments.contains("-diagnostics-video") {
                            Button("Attach fixture video") {
                                importAttachment {
                                    guard let url = Bundle.main.url(forResource: "attachment-video", withExtension: "mp4") else { throw FileFailure.integrity }
                                    return [try await MediaPreparation.video(url)]
                                }
                            }.accessibilityIdentifier("fixture-new-chat-video")
                        }
                        #endif
                        Button("Camera", systemImage: "camera") {
                            UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                            cameraScope = model.assignmentScope; showingCamera = true
                        }.disabled(!canAttach)
                        Button("Add sketch", systemImage: "pencil.tip.crop.circle.badge.plus") {
                            UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                            sketchScope = model.assignmentScope; sketchContext = sketchContextToken; showingSketch = true
                        }.disabled(!canAttach).accessibilityIdentifier("new-chat-add-sketch")
                        Button("Add media", systemImage: "photo.on.rectangle") { importingFor = draft.requestID; selectingPhoto = true }.disabled(!canAttach)
                        Button("Attach file", systemImage: "paperclip") { importingFor = draft.requestID; importing = true }.disabled(!canAttach)
                        if showsT3CodeToggle, !startsInT3Code {
                            Button("Start in T3 Code", systemImage: "terminal") { toggleDestination() }
                                .accessibilityIdentifier("new-chat-add-t3")
                        }
                    } label: { Image(systemName: "plus").font(.system(size: 22)).frame(width: 44, height: 44) }
                    .disabled(sending || draft.isSubmitted).accessibilityLabel("Add").accessibilityHint("Attachments and Start in T3 Code").accessibilityIdentifier("new-chat-attach")
                    if project != nil {
                        projectSettings(placement: separatesComposerChoices ? .inputs : .inline)
                            .environment(\.composerCompactChips, composerControlWidth < 420 && !typeSize.isAccessibilitySize)
                    }
                    else {
                        composerInputControls
                        Spacer(minLength: 0)
                    }
                    DictationSendControls(controller: model.dictation, conversationID: draftChat.id) {
                    Button(action: startSend) {
                        if sending || checkingRecovery { ProgressView().frame(width: 44, height: 44) }
                        else { Image(systemName: "arrow.up").font(.system(size: 20, weight: .semibold)).frame(width: 44, height: 44) }
                    }
                    .sendCircle(responds: canSend)
                    .disabled(!canSend)
                    .keyboardShortcut(.return, modifiers: .command)
                    .accessibilityLabel(draft.isSubmitted ? "Send again" : "Send")
                    .accessibilityIdentifier("new-chat-send")
                    }
                }
        }
    }

    private var centred: Bool {
        NewChatLayout.centresSetup(horizontal: horizontalSizeClass, vertical: verticalSizeClass,
                                   keepsComposerAtBottom: keepsComposerAtBottom)
    }

    /// Natural pill widths use spare room before truncating a long computer or
    /// Project name. At ordinary text sizes, narrow windows use two rows.
    private var pickers: some View {
        SetupPillRows(stacked: typeSize.isAccessibilitySize, fold: setupFold) {
            // Bound generic metadata construction on the physical phone's main
            // stack; composing all four opaque menu types here overflows it.
            AnyView(computerMenu.disabled(sending || draft.isSubmitted).modifier(SetupPill()))
            AnyView(projectMenu.disabled(sending || draft.isSubmitted).modifier(SetupPill()))
        }
        // Flush with the composer's leading edge on every device.
        .frame(maxWidth: .infinity, alignment: .leading)
        .onGeometryChange(for: CGRect?.self) { proxy in
            guard let fold = DuoFold.region(in: proxy)?.frame,
                  fold.width < proxy.size.width / 2, fold.minX > 0, fold.maxX < proxy.size.width else { return nil }
            return fold
        } action: { setupFold = $0 }
    }

    /// Where Send runs this chat, in the draft's own controls rather than the system bar.
    /// An owner flip arms the note for the destination change it causes; a flip that changes nothing leaves nothing armed.
    private func toggleDestination() {
        let before = startsInT3Code
        ownerToggledDestination = true
        draft.startsInT3Code = !before
        if startsInT3Code == before { ownerToggledDestination = false }
    }

    /// Shows that Send starts this chat in T3 Code; tapping removes it. Added back from the + menu.
    private var t3CodeChip: some View {
        ComposerModeChip(title: "T3 Code", systemImage: "terminal", label: "Start in T3 Code",
                         hint: "Removes T3 Code; the chat starts in Cupola", id: "composer-start-in-t3",
                         isDisabled: sending || draft.isSubmitted) { toggleDestination() }
    }

    private var computerMenu: some View {
        let status = model.accessEnded ? "Access ended" : model.macConnected == true ? "Connected" : model.macConnected == false ? "Offline" : "Connecting"
        return Button { showingConnectionPicker = true } label: {
            PickerRow(systemImage: "laptopcomputer", title: model.macName, status: ComputerState(model).color)
        }
        .buttonStyle(.plain).tint(.primary)
        .accessibilityLabel("Computer")
        .accessibilityValue("\(model.macName), \(status)")
        .accessibilityIdentifier("connection-picker")
        #if WONDER_DIAGNOSTICS
        .onAppear {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-new-chat-computers-open") { showingConnectionPicker = true }
        }
        #endif
        .pillPopover(isPresented: $showingConnectionPicker) {
            let preferredHeight = CGFloat(library.saved.connections.count + 1) * 44 + 21
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(library.saved.connections, id: \.credential.hostInstallationId) { saved in
                        let candidate = library.model(for: saved)
                        let state = ComputerState(candidate)
                        let detail = candidate.accessEnded ? "Access ended" : state == .connected ? "Connected" : state == .checking ? "Connecting" : "Offline"
                        let selected = saved.credential.hostInstallationId == hostID
                        Button {
                            showingConnectionPicker = false
                            choose(host: saved.credential.hostInstallationId)
                        } label: {
                            PickerOptionRow(systemImage: "laptopcomputer", title: candidate.macName, status: state.color, selected: selected)
                        }
                        .accessibilityLabel(candidate.macName)
                        .accessibilityValue(selected ? "\(detail), Selected" : detail)
                        .accessibilityIdentifier("connection-option:" + saved.credential.hostInstallationId)
                    }
                    Divider().padding(.vertical, 4)
                    Button {
                        pairAfterConnectionPicker = true
                        showingConnectionPicker = false
                    } label: {
                        Label("Add computer", systemImage: "plus")
                            .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                            .contentShape(Rectangle())
                    }
                    .accessibilityIdentifier("connection-add-computer")
                }
                .font(.body).foregroundStyle(.primary).buttonStyle(.plain)
                .padding(.horizontal, 14).padding(.vertical, 6)
            }
            .scrollBounceBehavior(.basedOnSize)
            .accessibilityIdentifier("connection-options")
            .frame(idealWidth: 290, maxWidth: 320, idealHeight: preferredHeight, maxHeight: preferredHeight)
            .onDisappear {
                if pairAfterConnectionPicker {
                    pairAfterConnectionPicker = false
                    pairing = true
                }
            }
        }
    }

    /// The same row shape as the computer picker: icon, name, trailing check.
    private var projectMenu: some View {
        Button { showingProjectPicker = true } label: {
            PickerRow(systemImage: project?.isNoProject == true ? "square.dashed" : "folder", title: project?.name ?? "Choose a Project")
        }
        .buttonStyle(.plain).tint(.primary)
        .accessibilityLabel("Project")
        .accessibilityValue(project?.name ?? "None")
        .accessibilityIdentifier("destination-picker")
        #if WONDER_DIAGNOSTICS
        .onAppear {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-new-chat-projects-open") { showingProjectPicker = true }
        }
        #endif
        .pillPopover(isPresented: $showingProjectPicker) {
            let preferredHeight = CGFloat(projects.includedProjects.count + 1) * 44 + 21
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(projects.includedProjects) { listed in
                        let selected = listed.id == project?.id
                        Button {
                            showingProjectPicker = false
                            choose(destination: .project(id: listed.id), project: listed)
                        } label: {
                            PickerOptionRow(systemImage: listed.isNoProject ? "square.dashed" : listed.isPinned ? "pin" : "folder", title: listed.name, status: nil, selected: selected)
                        }
                        .accessibilityLabel(listed.name)
                        .accessibilityValue(selected ? "Selected" : "")
                        .accessibilityIdentifier("project-option:" + listed.id)
                    }
                    Divider().padding(.vertical, 4)
                    Button {
                        addProjectAfterPicker = true
                        showingProjectPicker = false
                    } label: {
                        Label("Add Project", systemImage: "folder.badge.plus")
                            .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                            .contentShape(Rectangle())
                    }
                    .accessibilityIdentifier("project-add")
                }
                .font(.body).foregroundStyle(.primary).buttonStyle(.plain)
                .padding(.horizontal, 14).padding(.vertical, 6)
            }
            .scrollBounceBehavior(.basedOnSize)
            .accessibilityIdentifier("project-options")
            .frame(idealWidth: 290, maxWidth: 320, idealHeight: preferredHeight, maxHeight: min(preferredHeight, 420))
            .onDisappear {
                if addProjectAfterPicker {
                    addProjectAfterPicker = false
                    addingProject = true
                }
            }
        }
    }

    private func choose(destination: ChatDestination, project: ProjectSummary? = nil) {
        guard let hostID else { return }
        linkedProjectID = nil
        model.dictation.captureControlsHidden(conversationID: draftChat.id)
        draft = NewChatDraftStore.selecting(destination, from: draft, host: hostID, project: project, capabilities: projects.capabilities, startable: projects.isStartable)
        ensureModel()
    }

    private func choose(host: String) {
        guard host != hostID else { return }
        model.dictation.captureControlsHidden(conversationID: draftChat.id)
        guard sendTask.cancelAfterSaving(draft, hostID: hostID) else {
            failure = "Your draft could not be saved. Free some storage and try again."
            return
        }
        sending = false
        linkedProjectID = nil
        let previous = draft
        let previousDestinationIsT3 = startsInT3Code
        let saved = NewChatDraftStore.load(host: host)
        var next = NewChatDraft.switching(from: draft, toSaved: saved)
        let preservesPendingChoices = next.isSubmitted || T3CodeLaunch.unconfirmedLaunch(next) != nil
        // Each computer retains its words and Project. The current picker
        // choices travel with the switch and are revalidated at the destination.
        pendingComputerSelection = preservesPendingChoices ? nil : previous
        if !preservesPendingChoices {
            next.family = previous.family; next.model = previous.model; next.effort = previous.effort
            next.serviceTier = previous.serviceTier; next.access = previous.access
            next.startsInT3Code = previousDestinationIsT3; next.t3RuntimeMode = previous.t3RuntimeMode
        }
        switchedComputer = true
        destinationChanged = true
        selectionNote = nil
        destinationNote = preservesPendingChoices ? "Restored this computer’s pending message and its saved choices." : nil
        // The binding stages immediately; select its host before staging the
        // replacement so this computer's draft cannot overwrite the old one.
        hostID = host
        draft = next
        NewChatDraftStore.lastHost = host
        failure = nil
    }

    private func projectSettings(placement: ComposerSettingsPlacement) -> some View {
        let family = draft.family ?? .codex // provider identity: default provider
        // Match the thread composer: readable choices above the action row
        // when narrow, and vertically stacked at accessibility sizes.
        let modelLabel = ComposerModelLabel(family: draft.family, title: modelTitle, fast: speed != nil)
        let efforts = selectedModel?.reasoningEfforts ?? []
        return ComposerSettingsRow(placement: placement) {
            HStack(spacing: 0) {
                permissionMenu(family: family)
                    .fixedSize(horizontal: true, vertical: false)
                composerInputControls
            }
        } model: {
            Button { showingModel = true } label: {
                modelLabel.pillPopover(isPresented: Binding(get: { showingModel && hasDisplayFold }, set: { showingModel = $0 })) {
                    modelSheet.frame(width: 320, height: 500)
                }
            }
                .disabled(sending || draft.isSubmitted)
                .accessibilityLabel("Model")
                .accessibilityValue([family.title, selectedModel == nil ? "Choose model" : chipTitle, speed?.label].compactMap { $0 }.joined(separator: ", "))
                .accessibilityIdentifier("project-agent-picker")
                #if WONDER_DIAGNOSTICS
                .onAppear {
                    if ProcessInfo.processInfo.arguments.contains("-diagnostics-new-chat-model-open") { showingModel = true }
                }
                #endif
        } effort: {
            if !efforts.isEmpty {
                ComposerEffortButton(levels: efforts, selected: draft.effort, isDisabled: sending || draft.isSubmitted,
                                     identifier: "new-chat-effort") { level in
                    draft.effort = level
                }
            }
        }
    }

    @ViewBuilder private func permissionMenu(family: AgentFamily) -> some View {
        if startsInT3Code {
            Menu {
                ForEach(T3CodeCatalog.permissionOrder(t3Status?.launchCatalog?.provider(for: draft.model)?.runtimeModes ?? []), id: \.self) { mode in
                    Button { draft.t3RuntimeMode = mode } label: {
                        if draft.t3RuntimeMode == mode { Label(T3CodeCatalog.permissionTitle(mode), systemImage: "checkmark") }
                        else { Text(T3CodeCatalog.permissionTitle(mode)) }
                    }.accessibilityIdentifier("new-chat-t3-permission:" + mode)
                }
            } label: {
                ComposerPermissionsLabel()
                    .foregroundStyle(draft.t3RuntimeMode == "full-access" ? Color.orange : Color.primary)
            }
            .menuOrder(.fixed)
            .disabled(sending || draft.isSubmitted || t3Status?.launchCatalog?.provider(for: draft.model)?.runtimeModes.isEmpty != false)
            .accessibilityLabel("Permissions")
            .accessibilityValue(draft.t3RuntimeMode.map(T3CodeCatalog.permissionTitle) ?? "Unavailable")
            .accessibilityIdentifier("new-chat-access")
        } else {
            ProjectAccessMenu(family: family, capabilities: capabilities, supportsModes: projects.supportsModes, access: draft.access,
                              isDisabled: sending || draft.isSubmitted, identifier: "new-chat-access") { choice in
                draft.access = choice.result(from: draft.access, capabilities: capabilities, supportsModes: projects.supportsModes)
            }
        }
    }

    private var composerInputControls: some View {
        HStack(spacing: 0) {
            DictationButton(controller: model.dictation, chat: draftChat,
                unavailable: sending || draft.isSubmitted || model.accessEnded,
                prepare: {
                    guard let hostID, NewChatDraftStore.save(draft, host: hostID) else {
                        failure = "Your draft could not be saved. Free some storage and try again."
                        return false
                    }
                    return true
                })
            ComposerVoiceButton(unavailable: sending || draft.isSubmitted || model.accessEnded || model.macConnected != true) {
                UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                model.dictation.captureControlsHidden(conversationID: draftChat.id)
                if model.voice.isActive, model.voice.chatID != nil { model.voice.showTarget(in: voiceScene) }
                else {
                    // New chat has no thread yet. Use the existing standalone
                    // voice session, as the Action button/Shortcut does here.
                    if !model.voice.isActive { model.voice.start(chat: nil, scene: voiceScene) }
                    showingVoice = true
                }
            }
        }
    }

    private var modelSheet: some View {
        NavigationStack {
            WonderForm {
                if t3CatalogMissing {
                    Section {
                        Text(t3Blocked ?? "Reading T3 Code’s models…").foregroundStyle(.secondary)
                        Button("Retry") { Task { await refreshT3Status() } }.accessibilityIdentifier("new-chat-t3-catalog-retry")
                    }
                } else {
                HarnessModelPicker(
                    family: draft.family, offered: pickerFamilies.contains, canChoose: pickerAvailable,
                    models: ProjectModelPicker.matching(familyModels, query: searchingModels ? modelSearch : ""),
                    hasModels: !familyModels.isEmpty, query: modelSearch, selectedModel: draft.model,
                    speeds: selectedModel?.serviceTiers ?? [], speed: draft.serviceTier ?? "default",
                    computerName: model.macName, identifier: "new-chat",
                    chooseFamily: choosePickerFamily,
                    chooseModel: choosePickerModel,
                    chooseSpeed: { draft.serviceTier = $0 },
                    middle: {
                        if let t3CatalogSearchError, startsInT3Code {
                            Section { Text(t3CatalogSearchError).foregroundStyle(.secondary) }
                        }
                    },
                    trailing: { workingFolderSection })
                }
            }
            .listSectionSpacing(hasDisplayFold ? .custom(8) : .default)
            .foregroundStyle(.primary)
            .disabled(sending || draft.isSubmitted)
            .pinnedSheetHeader("", search: searchingModels
                ? SheetSearch(text: $modelSearch, prompt: "Search models", identifier: "new-chat-model-search") : nil)
            .onDisappear { modelSearch = "" }
            .task(id: model.assignmentScope + ":" + String(startsInT3Code) + ":" + modelSearch) {
                t3CatalogSearchError = nil
                let query = modelSearch.trimmingCharacters(in: .whitespacesAndNewlines)
                guard startsInT3Code, !query.isEmpty else { return }
                let scope = model.assignmentScope
                do {
                    try await Task.sleep(for: .milliseconds(300))
                    let result: T3CodeStatus = try await model.manage("/api/v1/t3-code?search=" + ConnectionModel.escape(String(query.prefix(200))))
                    guard !Task.isCancelled, scope == model.assignmentScope, startsInT3Code,
                          query == modelSearch.trimmingCharacters(in: .whitespacesAndNewlines) else { return }
                    guard let catalog = result.launchCatalog, let current = t3Status?.launchCatalog else {
                        t3CatalogSearchError = result.launchCatalogError ?? "Couldn’t search T3 Code’s models. Try searching again."
                        return
                    }
                    t3Read?.status.launchCatalog = current.mergingSearch(catalog, selected: draft.model)
                } catch is CancellationError {
                } catch {
                    if !Task.isCancelled, scope == model.assignmentScope {
                        t3CatalogSearchError = "Couldn’t search T3 Code’s models. Try searching again."
                    }
                }
            }
        }
        .alert("Switch working folder?", isPresented: $showingFolderChangeConfirmation,
               presenting: pendingFolderSelection) { folder in
            Button("Keep current folder", role: .cancel) {}
            Button("Switch and remove comments", role: .destructive) { chooseFolder(folder) }
        } message: { _ in
            Text("Your comments belong to files in this folder. Switching removes them from this draft.")
        }
        .presentationDetents([.large]).presentationDragIndicator(.hidden)
    }

    private func choosePickerFamily(_ family: AgentFamily) {
        selectionNote = nil
        if family != draft.family { modelSearch = "" }
        var next = draft
        next.chooseFamily(family, capabilities: projects.capabilities(family))
        draft = next
        ensureModel(explaining: true, ownerPickedProvider: true)
    }

    private func choosePickerModel(_ option: AgentOptions.Model) {
        if option.id != draft.model { selectionNote = nil; draft.chooseModel(option); ensureModel(explaining: true) }
        // Choosing a model ends the sheet unless the model has speeds to choose next.
        if (selectedModel?.serviceTiers ?? []).count <= 1 { showingModel = false }
    }

    #if WONDER_DIAGNOSTICS
    /// Drives the mounted picker for explicit semantic layout probes.
    /// Native input runs separately through AXe or the simulator touch rig.
    private var pickerDriverSurface: DiagnosticTestDriver.Surface {
        .init(priority: DiagnosticSubagentFixture.t3CodePickerFixture ? 250 : 5, active: { !showingComputer && !showingT3Connection && !shell.settingsOpen }, state: {
            ["route": "new-chat", "newChat": ["host": hostID ?? "", "t3": startsInT3Code, "destination": startsInT3Code ? "T3 Code" : "Cupola", "chip": chipTitle,
                "model": draft.model ?? "", "effort": draft.effort ?? "", "speed": draft.serviceTier ?? "default",
                "permission": draft.t3RuntimeMode ?? ""],
             "semanticMenus": driverMenu.map { [$0] } ?? [], "sheets": showingModel ? ["model"] : []]
        }, controls: {
            let enabled = !sending && !draft.isSubmitted
            if showingModel {
                var controls: [DiagnosticTestDriver.Control] = [.init(id: "sheet-top", label: "Agent")]
                if t3CatalogMissing {
                    controls.append(.init(id: "new-chat-t3-catalog-retry", label: "Retry", tap: { Task { await refreshT3Status() } }))
                    return controls
                }
                if searchingModels {
                    controls.append(.init(id: "new-chat-model-search", label: "Search models", value: modelSearch, tap: {}))
                }
                for family in AgentFamily.allCases.filter({ $0 == draft.family || pickerFamilies.contains($0) }) {
                    controls.append(.init(id: "new-chat-agent:" + family.rawValue, label: family.title,
                        enabled: enabled && pickerAvailable(family), tap: { choosePickerFamily(family) }))
                }
                for option in ProjectModelPicker.matching(familyModels, query: searchingModels ? modelSearch : "") {
                    controls.append(.init(id: "new-chat-model:" + option.id, label: option.displayName, enabled: enabled,
                        tap: { choosePickerModel(option) }))
                }
                for tier in selectedModel?.serviceTiers ?? [] {
                    controls.append(.init(id: "new-chat-speed:" + tier.id, label: tier.label, enabled: enabled,
                        tap: { draft.serviceTier = tier.id }))
                }
                if startsInT3Code, t3Status?.launchCatalog == nil {
                    controls.append(.init(id: "new-chat-t3-catalog-retry", label: "Retry", tap: { Task { await refreshT3Status() } }))
                }
                return controls
            }
            if showingConnectionPicker {
                return library.saved.connections.map { saved in
                    .init(id: "connection-option:" + saved.credential.hostInstallationId, label: library.model(for: saved).macName,
                        enabled: enabled, tap: { showingConnectionPicker = false; choose(host: saved.credential.hostInstallationId) })
                }
            }
            if driverMenu == "attachments" {
                // Mirrors the + menu: Start in T3 Code is offered only while the chip is absent.
                return showsT3CodeToggle && !startsInT3Code
                    ? [.init(id: "new-chat-add-t3", label: "Start in T3 Code", enabled: enabled, tap: { toggleDestination(); driverMenu = nil })] : []
            }
            if driverMenu == "permissions", startsInT3Code {
                return T3CodeCatalog.permissionOrder(t3Status?.launchCatalog?.provider(for: draft.model)?.runtimeModes ?? []).map { mode in
                    .init(id: "new-chat-t3-permission:" + mode, label: T3CodeCatalog.permissionTitle(mode), enabled: enabled,
                          tap: { draft.t3RuntimeMode = mode; driverMenu = nil })
                }
            }
            var controls: [DiagnosticTestDriver.Control] = [
                .init(id: "new-chat-draft", label: draft.text),
                .init(id: "project-agent-picker", label: modelTitle, value: [draft.family?.title, speed?.label, startsInT3Code ? "T3 Code" : "Cupola"].compactMap { $0 }.joined(separator: ", "), enabled: enabled, tap: { showingModel = true }),
                .init(id: "connection-picker", label: model.macName, enabled: enabled, tap: { showingConnectionPicker = true }),
                .init(id: "new-chat-access", label: "Permissions", value: draft.t3RuntimeMode.map(T3CodeCatalog.permissionTitle) ?? "", enabled: enabled, tap: { driverMenu = "permissions" }),
                .init(id: "new-chat-attach", label: "Attach", enabled: enabled, tap: { driverMenu = "attachments" })]
            if showsT3CodeToggle, startsInT3Code {
                controls.append(.init(id: "composer-start-in-t3", label: "Start in T3 Code", enabled: enabled, tap: { toggleDestination() }))
            }
            if let effort = selectedModel?.reasoningEfforts.first(where: { $0.id == draft.effort }) {
                controls.append(.init(id: "new-chat-effort", label: "Effort", value: effort.label))
            }
            if let t3Blocked, startsInT3Code { controls.append(.init(id: "new-chat-t3-note", label: t3Blocked)) }
            if let selectionChangeMessage { controls.append(.init(id: "new-chat-selection-note", label: selectionChangeMessage)) }
            return controls
        }, actions: ["type": { arguments in
            guard showingModel, searchingModels else { throw DiagnosticTestDriver.DriverError("Open model search before typing") }
            modelSearch += arguments["text"] ?? ""
        }, "dismiss": { _ in
            guard showingModel || showingConnectionPicker || driverMenu != nil else { throw DiagnosticTestDriver.DriverError("No picker presentation to dismiss") }
            showingModel = false; showingConnectionPicker = false; driverMenu = nil
        }])
    }
    #endif

    @ViewBuilder private var workingFolderSection: some View {
        if let project, project.folders.count > 1 {
            Section(startsInT3Code ? "Worktree" : "Working folder") {
                ForEach(project.folders) { folder in
                    Button {
                        guard folder.id != (draft.folderId ?? project.primaryFolder?.id) else { return }
                        if (draft.annotations ?? []).isEmpty { chooseFolder(folder) }
                        else { pendingFolderSelection = folder; showingFolderChangeConfirmation = true }
                    } label: {
                        HStack {
                            Text(folder.name); Spacer()
                            if folder.id == (draft.folderId ?? project.primaryFolder?.id) { Image(systemName: "checkmark") }
                        }
                    }
                }
            }
        }
    }

    private func chooseFolder(_ folder: ProjectFolder) {
        guard !sending, !draft.isSubmitted, let hostID else { return }
        var next = draft
        next.chooseFolder(folder.id)
        if NewChatDraftStore.save(next, host: hostID) { draft = next }
        else { failure = "Could not save the working folder. Free some storage and try again." }
    }

    /// A draft with no destination or a project that is gone belongs in
    /// the first included project.
    private func settleDestination() {
        if linkedProjectID != nil { return }
        var next = draft
        let settled = next.settle(in: projects.includedProjects, capabilities: projects.capabilities, startable: projects.isStartable)
        // The new computer's Project defaults must not overwrite the choices
        // being revalidated for that computer.
        if let previous = pendingComputerSelection, next.destination != nil {
            next.family = previous.family; next.model = previous.model; next.effort = previous.effort
            next.serviceTier = previous.serviceTier; next.access = previous.access
            pendingComputerSelection = nil
        }
        guard settled || next != draft else { return }
        draft = next
        ensureModel()
    }

    /// Keep the draft on a model its provider offers, with an effort that model
    /// supports: the draft's own choice, else the Project's last model for this
    /// provider, else the computer's default.
    /// `explaining` is true only for the call an owner action triggers, so a
    /// change they did not choose never produces an "Adjusted …" note.
    private func ensureModel(explaining: Bool = false, ownerPickedProvider: Bool = false) {
        guard !draft.isSubmitted, let project else { return }
        // A draft saved before its provider was known starts from the project's.
        var next = draft
        if draft.family == nil {
            // Changing the provider reruns this to pick the model.
            next.adoptDefaults(of: project, capabilities: projects.capabilities, startable: projects.isStartable)
            if next != draft { draft = next }
            return
        }
        // Resolve the computer's destination default before using its native
        // catalog; otherwise a late T3 read can silently change the provider.
        if draft.startsInT3Code == nil, savedT3Default == nil, model.macConnected == true { return }
        if startsInT3Code, t3Status?.launchCatalog == nil { return }
        // Keep an explicit unavailable choice visible until the owner picks
        // another model, unless they changed destination (T3 Code or computer).
        if !startsInT3Code, !destinationChanged, pendingComputerSelection == nil,
           let selected = draft.model, !familyModels.contains(where: { $0.id == selected }) { return }
        let before = next
        let available = pickerFamilies.filter { family in
            pickerAvailable(family) && pickerModels.contains { !$0.hidden && $0.family == family }
        }
        if !available.contains(next.family ?? .codex), let first = available.first { // provider identity: default provider
            next.chooseFamily(first, capabilities: projects.capabilities(first))
        }
        var models = pickerModels.filter { !$0.hidden && $0.family == next.family }
        // The same model listed under another catalogue's identifier is not a change.
        var sameModelRemapped = false
        // Preserve an equivalent native/T3 model before using the destination's default.
        if !models.contains(where: { $0.id == next.model }), let old = next.model {
            let nativeID = t3Status?.launchCatalog?.nativeModel(old) ?? old
            // The old id may come from either catalogue (turning T3 Code on or off).
            let oldTitle = (projects.options?.models.first(where: { $0.id == old })
                ?? t3Status?.launchCatalog?.models.first(where: { $0.id == old }))?.displayName
            // Same name in another catalogue ("Claude Sonnet 5.5" vs "Sonnet 5.5") is the same model.
            func plainName(_ name: String?) -> String? {
                guard var text = name?.lowercased() else { return nil }
                if let provider = next.family?.title.lowercased(), text.hasPrefix(provider) { text.removeFirst(provider.count) }
                let plain = text.filter { $0.isLetter || $0.isNumber }
                return plain.isEmpty ? nil : plain
            }
            let oldPlain = plainName(oldTitle)
            let exact = models.first(where: {
                $0.id == nativeID || $0.id.hasSuffix(":" + nativeID) || $0.displayName == oldTitle
                    || (oldPlain != nil && plainName($0.displayName) == oldPlain)
            })
            // Native Claude uses aliases such as claude:opus; T3 advertises
            // the full model ID. Match the alias before picking a default.
            let tokens = Set(nativeID.lowercased().split { !$0.isLetter && !$0.isNumber }.map(String.init))
            let alias = nativeID.split(separator: ":").last.map(String.init) ?? nativeID
            let equivalent = exact ?? models.first(where: {
                let candidate = $0.id.split(separator: ":").last.map(String.init) ?? $0.id
                let candidateTokens = Set(candidate.lowercased().split { !$0.isLetter && !$0.isNumber }.map(String.init))
                return (alias.count >= 3 && candidateTokens.contains(alias.lowercased()))
                    || (candidate.count >= 3 && tokens.contains(candidate.lowercased()))
            })
            if let equivalent {
                next.model = equivalent.id
                // Only an exact or same-name match is the same model; a loose alias match may be another version.
                sameModelRemapped = exact != nil
            }
        }
        // A model T3 Code doesn't offer has no equivalent to keep: start from
        // Claude, which T3 Code runs by default, not the first provider listed.
        // A provider the owner just picked is kept (its model is still nil).
        if startsInT3Code, !ownerPickedProvider, next.family != .claude, available.contains(.claude), // provider identity: T3 default
           !models.contains(where: { $0.id == next.model }) {
            next.chooseFamily(.claude, capabilities: projects.capabilities(.claude))
            models = pickerModels.filter { !$0.hidden && $0.family == .claude } // provider identity: T3 default
        }
        next.ensureModel(among: models, remembered: project.lastModel(for: next.family))
        if let option = models.first(where: { $0.id == next.model }) {
            // A replacement model can still support the previous effort/speed.
            next.effort = ModelDefaults.effort(before.effort, for: option)
            let tiers = option.serviceTiers ?? []
            let tier = tiers.first { $0.id == before.serviceTier }
                ?? tiers.first { $0.id == next.serviceTier }
                ?? tiers.first { $0.id == option.defaultServiceTier }
                ?? tiers.first { $0.id == "default" } ?? tiers.first
            next.serviceTier = tier?.id
        }
        if startsInT3Code, let provider = t3Status?.launchCatalog?.provider(for: next.model) {
            let desired = next.t3RuntimeMode ?? nativeT3Permission
            if !provider.runtimeModes.contains(desired) {
                next.t3RuntimeMode = provider.runtimeModes.first(where: { $0 == "approval-required" }) ?? provider.runtimeModes.first
            } else { next.t3RuntimeMode = desired }
        }
        var changed: [String] = []
        if before.family != nil, before.family != next.family { changed.append("provider") }
        if before.model != nil, before.model != next.model, !sameModelRemapped { changed.append("model") }
        if before.effort != nil, before.effort != next.effort { changed.append("effort") }
        func tierName(_ id: String?, in option: AgentOptions.Model?) -> String {
            let tier = option?.serviceTiers?.first { $0.id == id }
            return (tier?.label ?? ((id == nil || id == "default") ? "standard" : id!)).lowercased()
        }
        let oldOption = (projects.options?.models.first(where: { $0.id == before.model })
            ?? t3Status?.launchCatalog?.models.first(where: { $0.id == before.model }))
        if before.serviceTier != nil,
           tierName(before.serviceTier, in: oldOption) != tierName(next.serviceTier, in: models.first { $0.id == next.model }) {
            changed.append("speed")
        }
        if startsInT3Code, (before.t3RuntimeMode ?? nativeT3Permission) != next.t3RuntimeMode
            || (startsInT3Code && before.t3RuntimeMode == nil && before.accessMode == .readOnly) {
            changed.append("permissions (\(next.t3RuntimeMode.map(T3CodeCatalog.permissionTitle) ?? "unavailable"))")
        }
        if !changed.isEmpty, explaining {
            let destination = startsInT3Code ? "T3 Code" : "Cupola"
            let oldName = oldOption.map { ProjectModelPicker.title(model: $0, family: before.family) }
            let newOption = models.first { $0.id == next.model }
            if changed.contains("model") || changed.contains("provider"), let oldName, let newOption {
                let newName = ProjectModelPicker.title(model: newOption, family: next.family)
                selectionNote = "\(destination) doesn’t offer \(oldName), so this chat uses \(newName)."
            } else {
                selectionNote = "Adjusted \(changed.joined(separator: ", ")) for \(destination)."
            }
        }
        if next != draft { draft = next }
    }

    private var nativeT3Permission: String {
        if draft.accessMode == .fullAccess { return "full-access" }
        switch draft.claudeApproval {
        case .auto: return "auto"
        case .acceptEdits: return "auto-accept-edits"
        default: return "approval-required"
        }
    }

    private func adoptT3PermissionForNative() {
        guard let mode = draft.t3RuntimeMode, !draft.isSubmitted else { return }
        draft.accessMode = mode == "full-access" ? .fullAccess : .workspace
        if draft.family == .claude { // provider identity: permission mapping
            draft.claudeApproval = mode == "auto" ? .auto : mode == "auto-accept-edits" ? .acceptEdits : .ask
        }
    }

    private func importAttachment(_ load: @escaping @MainActor () async throws -> [StagedFile]) {
        guard canAttach else { return }
        let request = draft.requestID, scope = model.assignmentScope
        let remaining = 4 - draft.attachmentCount
        loadingAttachment = true
        attachmentTask = Task {
            defer { loadingAttachment = false }
            do {
                let files = try await load()
                guard files.count <= remaining else { throw FileFailure.tooLarge }
                let worker = Task.detached {
                    try Task.checkCancellation()
                    return try files.map { try NewChatDraftStore.stage($0) }
                }
                let items = try await withTaskCancellationHandler(operation: { try await worker.value }, onCancel: { worker.cancel() })
                guard !Task.isCancelled, request == draft.requestID, scope == model.assignmentScope else { return }
                try draft.addAttachments(items)
            } catch is CancellationError {
            } catch { failure = "Could not attach these files. Use up to four files, each no larger than 8 MB." }
        }
    }

    private func pasteImages(_ providers: [NSItemProvider]) {
        importAttachment {
            var files: [StagedFile] = []
            for provider in providers {
                guard let type = provider.registeredTypeIdentifiers.first(where: { UTType($0)?.conforms(to: .image) == true }) else { throw FileFailure.integrity }
                let data: Data = try await withCheckedThrowingContinuation { continuation in
                    provider.loadDataRepresentation(forTypeIdentifier: type) { data, error in
                        if let error { continuation.resume(throwing: error) }
                        else if let data { continuation.resume(returning: data) }
                        else { continuation.resume(throwing: FileFailure.integrity) }
                    }
                }
                files.append(try await ConnectionModel.prepareImageAttachment(data))
            }
            return files
        }
    }

    /// Why this draft's last T3 Code launch may have started; nil otherwise.
    private var t3UnconfirmedMessage: String? { T3CodeLaunch.unconfirmedLaunch(draft) }

    private func startSend() {
        if t3UnconfirmedMessage != nil { confirmingT3Resend = true; return }
        startSendConfirmed()
    }

    private func startSendConfirmed() {
        guard !sendTask.isActive else { return }
        let generation = sendTask.generation
        sendTask.start(Task { await send(generation: generation) })
    }

    private func send(generation: UUID) async {
        defer { if sendTask.finish(generation) { sending = false } }
        // Send during dictation commits the finished transcript to the draft first.
        if model.dictation.ownsCapture(conversationID: draftChat.id) {
            guard canSend, await model.dictation.finishForSending(conversationID: draftChat.id) else { return }
        }
        guard generation == sendTask.generation, canSend,
              case .project(let projectID) = draft.destination, let hostID,
              let device = model.connection?.credential.deviceId else { return }
        sending = true; failure = nil; t3Notice = nil
        if startsInT3Code {
            // Freeze the resolved default for retries and uncertain outcomes.
            draft.startsInT3Code = true
            await launchInT3Code(projectID: projectID, hostID: hostID)
            return
        }
        let recoveryScope = model.assignmentScope, recoveryRequest = draft.requestID
        let reason = await model.sendRecoveryReason(family: draft.family, selectedModel: draft.model)
        guard !Task.isCancelled, recoveryScope == model.assignmentScope, recoveryRequest == draft.requestID else { return }
        if let reason { providerProblem = reason; return }
        guard !(draft.annotations ?? []).contains(where: \.sourceChanged) else {
            failure = "A file you commented on changed. Remove that comment and select the current content before sending."
            return
        }
        let noteDraftID = draft.annotations?.first?.draftID ?? draft.requestID
        let folderID = draft.folderId ?? project?.primaryFolder?.id ?? ""
        let rootsRevision = draft.preparedConversationID == nil ? project?.rootsRevision : draft.rootsRevision
        do {
            for pending in draft.annotations ?? [] {
                _ = try pending.file(conversationID: "pending-validation", projectID: projectID,
                                     folderID: folderID, draftID: noteDraftID,
                                     rootsRevision: rootsRevision ?? -1)
            }
        } catch {
            failure = "A comment no longer matches this Project folder. Remove it and select the current file before sending."
            return
        }
        let scope = model.assignmentScope
        guard draft.submittedDeviceID == nil || draft.submittedDeviceID == device else {
            failure = "This request belongs to the previous pairing. Its delivery must be checked on your computer before sending again."
            return
        }
        // The Mac is unreachable: the request waits in the New chat outbox
        // and the composer starts a fresh draft.
        if model.macConnected == false { queueForMac(device: device, hostID: hostID); return }
        if !draft.isSubmitted {
            if let project { draft.folderId = draft.folderId ?? project.primaryFolder?.id; draft.rootsRevision = project.rootsRevision }
            draft.submittedDeviceID = device
            draft.freeze()
        }
        guard NewChatDraftStore.save(draft, host: hostID) else {
            failure = "Your message could not be saved. Free some storage and try again."
            return
        }
        let body = draft.submittedBody ?? draft.text
        let request = draft.requestID
        let attempt = NewChatSendAttempt(scope: scope, hostID: hostID, requestID: request)
        do {
            guard let chat = try await NewChatSubmission.create(draft, projectID: projectID, body: body, device: device, model: model,
                stillCurrent: { attempt.applies(to: model, hostID: self.hostID, draft: draft) },
                prepared: { prepared in
                    draft.preparedConversationID = prepared.preparedConversationID
                    NewChatDraftStore.save(draft, host: hostID)
                }) else { return }
            guard let next = NewChatDraftStore.complete(draft, host: hostID) else {
                failure = "Your message is saved, but the new draft could not be saved. Free some storage and retry this message."
                return
            }
            draft = next
            // Delivery now belongs to the connection model's durable intent;
            // leaving New Chat must not cancel it or reopen this screen later.
            let deliveryModel = model
            Task { await deliveryModel.deliver(chat) }
            shell.open(host: hostID, conversation: chat.id)
        } catch NewChatSubmission.Failure.attachmentUnavailable {
            guard attempt.applies(to: model, hostID: self.hostID, draft: draft) else { return }
            if draft.preparedConversationID == nil { rejectSubmission(host: hostID) }
            failure = "A saved attachment is unavailable. Remove it and attach the file again."
        } catch PairingFailure.response(412) {
            guard attempt.applies(to: model, hostID: self.hostID, draft: draft) else { return }
            rejectSubmission(host: hostID)
            let visibleRequest = draft.requestID
            await projects.refresh()
            guard scope == model.assignmentScope, !model.accessEnded, self.hostID == hostID,
                  draft.requestID == visibleRequest, !Task.isCancelled else { return }
            failure = "The Project’s folders changed. Review the working folder and send again."
        } catch PairingFailure.response(409) {
            guard attempt.applies(to: model, hostID: self.hostID, draft: draft) else { return }
            failure = "This request conflicts with the saved thread or its folder. Check the Project on your computer, then try this same request again."
        } catch PairingFailure.response(422) {
            guard attempt.applies(to: model, hostID: self.hostID, draft: draft) else { return }
            rejectSubmission(host: hostID)
            failure = "This Project, model or access choice can’t be used. Check them and send again."
        } catch PairingFailure.response(503) {
            guard attempt.applies(to: model, hostID: self.hostID, draft: draft) else { return }
            failure = "\(model.macName) isn’t ready yet. Send again in a moment to check the same request."
        } catch {
            guard attempt.applies(to: model, hostID: self.hostID, draft: draft) else { return }
            failure = draft.preparedConversationID.flatMap { model.uploadFailures[$0] }
                ?? "Your computer didn’t confirm the new thread. Send again to retry the same request; it won’t start twice."
        }
    }

    /// Start in T3 Code (`T3CodeSubmission`): the Mac answers the thread's
    /// conversation, which opens at once with the message and "Starting in
    /// T3 Code…"; T3 Code's agent session joins that same conversation.
    private func launchInT3Code(projectID: String, hostID: String) async {
        if let t3Blocked { failure = t3Blocked; return }
        let scope = model.assignmentScope, requestID = draft.requestID
        let outcome = await T3CodeSubmission.launch(draft, host: hostID, projectID: projectID, project: project,
            planToggle: projects.supportsModes && capabilities.plan == .toggle, model: model, catalog: t3Status?.launchCatalog)
        guard scope == model.assignmentScope, self.hostID == hostID, draft.requestID == requestID else { return }
        switch outcome {
        case .cancelled: return
        case .failed(let message, let refreshed):
            failure = message
            // A confirmed retry that is refused leaves the earlier launch
            // still unconfirmed: the saved draft kept the mark, so ask again.
            draft.t3LaunchUnconfirmed = T3CodeSubmission.storedUnconfirmed(host: hostID, requestID: requestID)
            if let refreshed { t3ReadGeneration += 1; t3Read = (scope, refreshed) }
        case .mayHaveStarted(let message, let refreshed):
            draft.t3LaunchUnconfirmed = message
            t3ReadGeneration += 1; t3Read = (scope, refreshed)
        case .started(let next, let conversationID):
            draft = next
            projects.loadThreads(projectID)
            guard let conversationID else {
                t3Notice = T3CodeLaunch.startedNotice(macName: model.macName)
                return
            }
            // Opened like any new chat; the conversation shows the message
            // and that it is starting in T3 Code.
            do {
                let chat = model.projectChat(try await projects.loadDetail(conversationID))
                guard scope == model.assignmentScope, self.hostID == hostID else { return }
                shell.open(host: hostID, conversation: chat.id)
            } catch {
                t3Notice = T3CodeLaunch.startedNotice(macName: model.macName)
            }
        }
    }

    /// Reads this computer's T3 Code connection, keeping the last answer
    /// while a new read is in flight so the menu doesn't flicker.
    private func refreshT3Status(refresh: Bool = false) async {
        guard model.macConnected == true else { return }
        t3ReadGeneration += 1
        let generation = t3ReadGeneration, scope = model.assignmentScope
        async let defaults: DefaultModelPreferences? = try? model.manage("/api/v1/settings/default-models")
        // A read that fails outright says nothing about T3 Code: keep the last
        // answer unless the owner asked to check again.
        let read = await model.t3CodeStatus(refresh: refresh, catalog: true)
        if read == nil, !refresh, t3Status != nil { _ = await defaults; return }
        var status = read ?? T3CodeStatus(state: "unavailable")
        // T3's short list can omit a model chosen through search. Recheck it
        // by name before replacing it with a destination default.
        if let previous = t3Status?.launchCatalog, let rawModel = previous.nativeModel(draft.model),
           let current = status.launchCatalog, current.provider(for: draft.model) == nil,
           let searched: T3CodeStatus = try? await model.manage("/api/v1/t3-code?search=" + ConnectionModel.escape(rawModel)),
           let catalog = searched.launchCatalog {
            status.launchCatalog = current.mergingSearch(catalog, selected: draft.model)
        }
        let preferences = await defaults
        guard !Task.isCancelled, generation == t3ReadGeneration, scope == model.assignmentScope else { return }
        t3Read = (scope, status)
        if status.state == "connected" { destinationNote = nil }
        t3DefaultsRead = (scope, preferences.map { $0.startsInT3Code == true })
        if switchedComputer {
            switchedComputer = false
            if status.state != "connected", !draft.isSubmitted,
               T3CodeLaunch.unconfirmedLaunch(draft) == nil, draft.startsInT3Code == true {
                draft.startsInT3Code = false
                destinationNote = "T3 Code isn’t connected on \(model.macName). Starting in Cupola."
            }
        }
        ensureModel()
    }

    private func rejectSubmission(host: String) {
        if let next = NewChatDraftStore.reject(draft, host: host) { draft = next }
    }

    private func queueForMac(device: String, hostID: String) {
        if let refusal = Outbox.newChatRefusal(adding: draft, to: NewChatDraftStore.queued(host: hostID)) {
            failure = Outbox.explanation(refusal)
            return
        }
        var next = draft
        if !next.isSubmitted, let project {
            next.folderId = next.folderId ?? project.primaryFolder?.id; next.rootsRevision = project.rootsRevision
        }
        next.queue(device: device)
        guard let fresh = NewChatDraftStore.startNew(from: next, host: hostID) else {
            failure = "Your message could not be saved. Free some storage and try again."
            return
        }
        model.dictation.captureControlsHidden(conversationID: draftChat.id)
        draft = fresh
        model.reloadQueuedNewChats()
        settleDestination(); ensureModel()
    }

    /// Edit a waiting New chat: it becomes the draft again (under a new
    /// request ID), only while the composer is empty.
    private func editQueued(_ item: NewChatDraft) {
        guard !sending, let hostID, item.canEditQueued, !model.outboxInFlight.contains(item.requestID) else { return }
        guard !draft.isSubmitted, draft.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, draft.attachmentCount == 0 else {
            failure = Outbox.explanation(.draftInUse)
            return
        }
        model.dictation.captureControlsHidden(conversationID: draftChat.id)
        var next = item
        next.unqueue()
        guard NewChatDraftStore.save(next, host: hostID) else {
            failure = "Your draft could not be saved. Free some storage and try again."
            return
        }
        NewChatDraftStore.removePending(host: hostID, requestID: item.requestID)
        draft = next; failure = nil
        model.reloadQueuedNewChats()
    }

    private func startNewDraft() {
        guard !sending, let hostID else { return }
        model.dictation.captureControlsHidden(conversationID: draftChat.id)
        guard let next = NewChatDraftStore.startNew(from: draft, host: hostID) else {
            failure = "Your pending message could not be saved. Free some storage before starting a new draft."
            return
        }
        draft = next; failure = nil
        reloadRecovery()
        settleDestination(); ensureModel()
    }

    private func reloadRecovery() {
        recoveryRecords = hostID.map { host in
            NewChatDraftStore.recoveryRecords(host: host).filter { $0.draft?.requestID != draft.requestID }
        } ?? []
    }

    private func discardRecovery(_ record: NewChatDraftStore.RecoveryRecord) {
        guard !sending, let hostID, NewChatDraftStore.discard(record, host: hostID) else {
            failure = "This message couldn’t be discarded. Reopen New chat and try again."
            return
        }
        reloadRecovery()
    }

    private func shareRecovery(_ pending: NewChatDraft) {
        guard !preparingRecoveryShare else { return }
        preparingRecoveryShare = true
        let scope = hostID
        recoveryShareTask = Task {
            defer { preparingRecoveryShare = false; recoveryShareTask = nil }
            do {
                let export = try await Task.detached { try NewChatDraftStore.export(pending) }.value
                guard !Task.isCancelled, hostID == scope else { try? FileManager.default.removeItem(at: export.directory); return }
                recoveryExportDirectory = export.directory
                recoveryShare = NewChatRecoveryShare(items: [pending.submittedBody ?? pending.text] + export.urls.map { $0 as Any })
            } catch {
                guard !Task.isCancelled, hostID == scope else { return }
                failure = "An attachment couldn’t be read. The original message is still kept. Review it to recover the available content."
            }
        }
    }

    private func review(_ pending: NewChatDraft) {
        guard !sending, let hostID else { return }
        model.dictation.captureControlsHidden(conversationID: draftChat.id)
        guard NewChatDraftStore.save(draft, host: hostID) else {
            failure = "Your draft could not be saved. Free some storage before reviewing this message."
            return
        }
        if !draft.text.isEmpty || draft.attachmentCount > 0 {
            guard NewChatDraftStore.save(draft, host: hostID, separately: true) else {
                failure = "Your draft could not be saved. Free some storage before reviewing this message."
                return
            }
        }
        guard NewChatDraftStore.save(pending, host: hostID, separately: true) else {
            failure = "Your draft could not be saved. Free some storage before reviewing this message."
            return
        }
        draft = pending; failure = nil
    }
}

private struct NewChatRecoveryList: View {
    @Environment(\.dynamicTypeSize) private var typeSize
    let records: [NewChatDraftStore.RecoveryRecord]
    let review: (NewChatDraft) -> Void
    let share: (NewChatDraft) -> Void
    let discard: (NewChatDraftStore.RecoveryRecord) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Not sent yet").font(.headline)
                .accessibilityIdentifier("new-chat-recovery-title")
            ForEach(records) { record in
                VStack(alignment: .leading, spacing: 4) {
                    if let pending = record.draft {
                        Text(pending.submittedBody ?? pending.text).textSelection(.enabled)
                        ForEach(pending.attachments ?? [], id: \.id) { attachment in
                            Label(attachment.name, systemImage: "paperclip").font(.caption).foregroundStyle(.secondary)
                        }
                        let layout = typeSize.isAccessibilitySize
                            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 8))
                            : AnyLayout(HStackLayout(spacing: 16))
                        layout {
                            Button("Review") { review(pending) }
                                .frame(minWidth: 44, minHeight: 44)
                                .accessibilityIdentifier("new-chat-recover:" + pending.requestID)
                            Button("Copy") { UIPasteboard.general.string = pending.submittedBody ?? pending.text }
                                .frame(minWidth: 44, minHeight: 44)
                                .accessibilityIdentifier("new-chat-copy:" + pending.requestID)
                            Button("Share") { share(pending) }
                                .frame(minWidth: 44, minHeight: 44)
                                .accessibilityIdentifier("new-chat-share:" + pending.requestID)
                            if !typeSize.isAccessibilitySize { Spacer(minLength: 0) }
                            discardButton(record)
                        }
                    } else {
                        HStack {
                            Text("Couldn’t read")
                            Spacer()
                            discardButton(record)
                        }
                    }
                }
                .font(.subheadline)
                Divider()
            }
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("new-chat-recovery")
    }

    private func discardButton(_ record: NewChatDraftStore.RecoveryRecord) -> some View {
        Button(role: .destructive) { discard(record) } label: {
            Label("Discard", systemImage: "trash").labelStyle(.iconOnly).frame(minWidth: 44, minHeight: 44)
        }
        .accessibilityIdentifier("new-chat-discard:" + (record.draft?.requestID ?? ConversationFile.digest(Data(record.id.utf8))))
    }
}

private struct NewChatRecoveryShare: Identifiable {
    let id = UUID()
    let items: [Any]
}

private struct NewChatRecoveryActivity: UIViewControllerRepresentable {
    let items: [Any]
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}

private struct NewChatAnnotationEditor: View {
    @Environment(\.dismiss) private var dismiss
    let pending: PendingNewChatAnnotation
    let onSave: (String) throws -> Void
    @State private var note: String
    @State private var failure: String?

    init(pending: PendingNewChatAnnotation, onSave: @escaping (String) throws -> Void) {
        self.pending = pending; self.onSave = onSave
        _note = State(initialValue: pending.annotation.note)
    }

    var body: some View {
        NavigationStack {
            WonderForm {
                Section(pending.annotation.path) {
                    TextEditor(text: $note)
                        .frame(minHeight: 110)
                        .accessibilityIdentifier("annotation-edit-note")
                    if pending.sourceChanged {
                        Text("This file changed. Remove this comment and select the current text before sending.")
                            .foregroundStyle(.orange)
                    }
                    if note.utf8.count > 4096 {
                        Text("Keep the comment under 4,096 bytes.").foregroundStyle(.red)
                    }
                }
                if let failure { Text(failure).foregroundStyle(.red) }
            }
            .navigationTitle("Comment")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        do { try onSave(note); dismiss() }
                        catch { failure = "Could not save this note. Check its text and available storage." }
                    }
                    .disabled(note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || note.utf8.count > 4096)
                    .accessibilityIdentifier("annotation-edit-save")
                }
            }
        }
    }
}

/// At most two rows for ordinary text; accessibility sizes keep every name
/// readable in a vertical list. Measure intrinsic widths before sharing space.
private struct SetupPillRows: Layout {
    var stacked: Bool
    var fold: CGRect?
    private let spacing: CGFloat = 6

    /// The pills are one group from the composer's leading edge. A hinge
    /// cutting across the row never splits it and never strands one pill
    /// beyond it: the whole group sits flush with the start of the side that
    /// holds it on one row (the leading side first), else the wider side.
    private func region(width: CGFloat, natural: CGFloat) -> (origin: CGFloat, room: CGFloat) {
        guard let fold else { return (0, width) }
        let leading = (origin: CGFloat(0), room: min(width, max(44, fold.minX)))
        let trailing = (origin: min(width, fold.maxX), room: max(44, width - fold.maxX))
        if natural <= leading.room { return leading }
        if natural <= trailing.room { return trailing }
        return leading.room >= trailing.room ? leading : trailing
    }

    private func natural(_ sizes: [CGSize]) -> CGFloat {
        sizes.map(\.width).reduce(0, +) + spacing * CGFloat(max(0, sizes.count - 1))
    }

    private func rows(width: CGFloat, subviews: Subviews) -> (origin: CGFloat, rows: [[CGSize]]) {
        let sizes = subviews.map { $0.sizeThatFits(.unspecified) }
        let (origin, room) = region(width: width, natural: natural(sizes))
        let groups: [[CGSize]]
        if stacked { groups = sizes.map { [$0] } }
        else if natural(sizes) <= room { groups = [sizes] }
        else { groups = [Array(sizes.prefix(2)), Array(sizes.dropFirst(2))].filter { !$0.isEmpty } }
        return (origin, groups.map { group in
            var widths = group.map(\.width)
            let budget = max(0, room - spacing * CGFloat(max(0, group.count - 1)))
            // Short names stay whole; share a shortage between longer controls.
            while widths.reduce(0, +) > budget + 0.5 {
                let largest = widths.max() ?? 0
                let indices = widths.indices.filter { widths[$0] >= largest - 0.5 }
                let next = widths.filter { $0 < largest - 0.5 }.max() ?? 0
                let reduction = min(largest - next, (widths.reduce(0, +) - budget) / CGFloat(indices.count))
                for index in indices { widths[index] -= reduction }
            }
            return zip(group, widths).map { CGSize(width: $0.1, height: $0.0.height) }
        })
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? subviews.reduce(0) { $0 + $1.sizeThatFits(.unspecified).width + spacing }
        return CGSize(width: width, height: rows(width: width, subviews: subviews).rows.reduce(0) { $0 + ($1.map(\.height).max() ?? 0) })
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var index = 0
        var y = bounds.minY
        let laidOut = rows(width: bounds.width, subviews: subviews)
        for row in laidOut.rows {
            let height = row.map(\.height).max() ?? 0
            var x = bounds.minX + laidOut.origin
            for size in row {
                subviews[index].place(at: CGPoint(x: x, y: y + (height - size.height) / 2),
                                      proposal: ProposedViewSize(size))
                x += size.width + spacing
                index += 1
            }
            y += height
        }
    }
}

/// A quiet capsule behind a setup control. The touch target stays 44 pt tall;
/// only the capsule is shorter.
struct SetupPill: ViewModifier {
    @Environment(\.wonderTheme) private var theme
    func body(content: Content) -> some View {
        content
            .padding(.horizontal, 10)
            .frame(minWidth: 44, minHeight: 44)
            .contentShape(Rectangle())
            .background { Capsule().fill(theme.surface).padding(.vertical, 5) }
    }
}

/// One row of a pill popover: icon, name, an optional status dot after the name,
/// and a trailing check on the current choice. Computers and Projects share it.
struct PickerOptionRow: View {
    let systemImage: String
    let title: String
    /// A small dot after the name (green connected, red offline); nil for rows with no status.
    let status: Color?
    let selected: Bool
    @Environment(\.wonderTheme) private var theme
    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: systemImage).foregroundStyle(.secondary).frame(width: 24).accessibilityHidden(true)
            Text(title).lineLimit(2)
            if let status { Circle().fill(status).frame(width: 8, height: 8).accessibilityHidden(true) }
            Spacer(minLength: 8)
            Image(systemName: "checkmark").font(.body.weight(.semibold)).foregroundStyle(theme.accent)
                .opacity(selected ? 1 : 0).accessibilityHidden(true)
        }
        .frame(minHeight: 44).contentShape(Rectangle())
    }
}

/// A plain, left-aligned row that opens an anchored native menu or an action:
/// a secondary icon, primary text and, for menus, the up-down chevron.
struct PickerRow: View {
    let systemImage: String
    let title: String
    var showsChevron = true
    /// The status dot after the name, as in the computer picker's rows.
    var status: Color?
    @Environment(\.dynamicTypeSize) private var typeSize
    var body: some View {
        HStack(spacing: typeSize.isAccessibilitySize ? 12 : 8) {
            Image(systemName: systemImage)
                .font(.system(size: typeSize.isAccessibilitySize ? 28 : 17))
                .foregroundStyle(.secondary)
                .frame(width: typeSize.isAccessibilitySize ? 36 : 22)
            Text(title).foregroundStyle(.primary).lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
            if let status { Circle().fill(status).frame(width: 8, height: 8).accessibilityHidden(true) }
            if showsChevron {
                Image(systemName: "chevron.up.chevron.down")
                    .font(typeSize.isAccessibilitySize ? .system(size: 18, weight: .semibold) : .caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
        }
        .font(.callout)
        .frame(minHeight: 44)
        .contentShape(Rectangle())
    }
}

extension Notification.Name {
}
