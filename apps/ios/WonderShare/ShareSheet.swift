#if WONDER_SHARE_EXTENSION || WONDER_DIAGNOSTICS
import SwiftUI
import UIKit
import ImageIO
import CryptoKit
import UniformTypeIdentifiers
import WonderPairing

/// Shared text, kept apart from the note so the sheet can preview links.
enum SharedText: Hashable, Sendable {
    case link(URL)
    case text(String)
    var body: String {
        switch self {
        case .link(let url): url.absoluteString
        case .text(let text): text
        }
    }
}

enum SharedItem: Sendable {
    case text(SharedText)
    case file(StagedFile, CGImage?)
}

/// Where Send goes: an existing thread or chat, or a new Project thread.
enum ShareChoice: Hashable, Sendable {
    case existing(String)
    case newThread
}

struct NewThreadChoice: Equatable, Sendable {
    var projectID: String?
    var family: AgentFamily = .codex
    var modelID: String?
}

enum SendPhase: Equatable, Sendable {
    case idle, preparing, uploading(done: Int, total: Int), sending, sent(queued: Bool)
    var isBusy: Bool {
        switch self {
        case .preparing, .uploading, .sending: true
        case .idle, .sent: false
        }
    }
}

/// The request a send was making when it failed, so the error names it.
enum ShareStep: Sendable { case startThread, openThread, upload, send }

struct ShareStepFailure: Error {
    let step: ShareStep
    let error: Error
}

enum ShareFailure: Error {
    /// No saved route reached the computer.
    case unreachable
    /// The share couldn't be saved for a safe retry, so nothing was sent.
    case notSaved
}

/// The app's theme, read from the value the app shares through the Keychain.
/// The default Wonder theme keeps the system's grouped colours.
struct ShareTheme {
    let spec: WonderThemeSpec
    let palette: ThemePalette
    var isDefault: Bool { spec.id == WonderThemeCatalog.defaultID }

    init(id: String?, systemIsDark: Bool) {
        spec = WonderThemeCatalog.theme(id: id)
        palette = spec.palette(systemIsDark: systemIsDark)
    }

    var forcedScheme: ColorScheme? { spec.forcedDark.map { $0 ? .dark : .light } }
    var page: Color { isDefault ? Color(uiColor: .systemGroupedBackground) : Self.color(palette.background) }
    var row: Color { isDefault ? Color(uiColor: .secondarySystemGroupedBackground) : Self.color(palette.agentBubble) }
    var text: Color { isDefault ? .primary : Self.color(palette.primaryText) }
    var secondary: Color { isDefault ? .secondary : Self.color(palette.secondaryText) }
    var accent: Color { Self.color(palette.accent) }
    /// Controls read as plain text, like the app; the accent marks the primary action.
    var tint: Color { isDefault ? .primary : Self.color(palette.primaryText) }

    static func color(_ hex: UInt32) -> Color {
        Color(.sRGB, red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255, blue: Double(hex & 0xFF) / 255)
    }
}

@MainActor final class ShareModel: ObservableObject {
    @Published private(set) var connections: [SavedConnection] = []
    @Published private(set) var hostID: String?
    @Published private(set) var projects: [ProjectSummary] = []
    @Published private(set) var availableFamilies: [AgentFamily] = AgentFamily.allCases
    @Published private(set) var models: [AgentOptions.Model]?
    @Published private(set) var threads: [ShareTarget] = []
    @Published private(set) var loadingDestinations = false
    @Published private(set) var loadingThreads = 0
    @Published private(set) var someThreadsMissing = false
    @Published private(set) var setupMessage: String?
    @Published private(set) var destinationError: String?
    @Published private(set) var choice: ShareChoice?
    @Published var newThread = NewThreadChoice()
    @Published var note = ""
    @Published var query = ""
    @Published private(set) var sharedTexts: [SharedText] = []
    @Published private(set) var files: [StagedFile] = []
    @Published private(set) var videoDurations: [String: String] = [:]
    @Published private(set) var uploadBytes = 0
    @Published private(set) var uploadTotalBytes = 0
    private var activeUpload: Task<ConversationFile, Error>?
    @Published private(set) var canCancelUpload = false
    @Published private(set) var thumbnails: [String: UIImage] = [:]
    @Published private(set) var loadingItems = true
    @Published private(set) var itemError: String?
    @Published private(set) var phase: SendPhase = .idle
    @Published private(set) var sendError: String?
    /// An earlier share the computer never confirmed, offered to finish or discard.
    @Published private(set) var recovery: PendingShare?
    @Published private(set) var recovering = false
    @Published private(set) var recoveryError: String?
    @Published private(set) var recoveryNotice: String?

    private let finish: (Bool) -> Void
    private let api: PairingAPI
    private let trust: HomeNetworkTrust
    private let sign: SealedRPC.Sign?
    private let journal: ShareJournal
    private let readConnections: () throws -> Data?
    private var destinationRequest = UUID()
    private var supportsModes = false
    private var families: [ProjectFamilyAvailability] = []
    private var userChose = false
    private var saved: [String: String]?
    /// The route chosen to each computer for this sheet; a failed request forgets it.
    private var routes: [String: Task<SavedConnection, Error>] = [:]
    /// This sheet's share, saved in the journal before its first request. A
    /// retry of the same content reuses its created thread, uploads and
    /// message identity, so a lost response never creates a duplicate; after
    /// a restart it comes back as `recovery` instead.
    private var current: PendingShare?
    nonisolated static let maximumBytes = AttachmentImagePolicy.maximumBytes
    /// A bitmap this size is about 36 MB while it is re-encoded, which leaves
    /// room in the share extension's memory limit (about 120 MB).
    nonisolated static let maximumPixels = 3072
    nonisolated static var appName: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String ?? "Cupola" }
    nonisolated static let lastDestinationKey = "wonder.share.lastDestination.v2"

    /// `readConnections` returns the app's saved Macs; Diagnostics passes a
    /// fixture Mac and a synthetic transport. Requests use the app's routes:
    /// home addresses only with their pinned key, then Tailscale, then the
    /// relay's sealed connection signed with this device's key.
    init(api: PairingAPI? = nil, trust: HomeNetworkTrust = HomeNetworkTrust(),
         sign: SealedRPC.Sign? = PhoneIdentity.signing.sealedSign, journal: ShareJournal = .standard,
         readConnections: @escaping () throws -> Data? = ShareModel.savedConnections,
         finish: @escaping (Bool) -> Void) {
        self.api = api ?? PairingAPI(serverTrustDelegate: trust)
        self.trust = trust; self.sign = sign; self.journal = journal
        self.readConnections = readConnections; self.finish = finish
    }

    nonisolated static func savedConnections() throws -> Data? {
        let identity = PhoneIdentity()
        if let data = try identity.read("connections-v1") { return data }
        guard let legacy = try identity.read("connection") else { return nil }
        let connection = try JSONDecoder().decode(SavedConnection.self, from: legacy)
        return try JSONEncoder().encode(SavedConnections(legacy: connection))
    }

    // MARK: Derived state

    var selectedConnection: SavedConnection? { connections.first { $0.credential.hostInstallationId == hostID } }
    var selectedTarget: ShareTarget? {
        guard case .existing(let id) = choice else { return nil }
        return threads.first { $0.id == id }
    }
    var sections: [ShareTargetSection] { ShareCatalog.sections(threads: threads, projects: projects, query: query) }
    var includedProjects: [ProjectSummary] { projects.filter(\.isIncluded) }
    var newThreadProject: ProjectSummary? { includedProjects.first { $0.id == newThread.projectID } }
    var newThreadModels: [AgentOptions.Model] { ShareComposition.models(models ?? [], family: newThread.family) }
    var newThreadModel: AgentOptions.Model? { newThreadModels.first { $0.id == newThread.modelID } }
    var messageBody: String { ShareComposition.body(note: note, shared: sharedTexts.map(\.body)) }
    var macName: String { selectedConnection.map(Self.name) ?? "your computer" }

    /// Why Send is unavailable when the reason is not obvious from the sheet.
    var blockReason: String? {
        if messageBody.utf8.count > ShareComposition.maximumBodyBytes { return "This text is too long to send. Share a shorter selection." }
        return nil
    }

    var canSend: Bool {
        guard !loadingItems, !phase.isBusy, phase == .idle, !recovering, setupMessage == nil, selectedConnection != nil, blockReason == nil,
              !messageBody.isEmpty || !files.isEmpty else { return false }
        switch choice {
        case .existing: return selectedTarget != nil
        case .newThread: return newThreadProject != nil && newThreadModel != nil
        case nil: return false
        }
    }

    // MARK: Lifecycle

    func start(providers: [NSItemProvider]) {
        loadConnections()
        Task { await loadItems(providers) }
        Task { await loadDestinations() }
    }

    func cancelUpload() { activeUpload?.cancel() }
    func cancel() {
        close()
        finish(false)
    }

    /// Ends the relay's sealed connections; the extension opens nothing else that outlives it.
    private func close() {
        for task in routes.values { task.cancel() }
        routes = [:]
        let api = api
        Task { await api.closeSealed() }
    }

    func remove(_ fileID: String) {
        files.removeAll { $0.id == fileID }
        thumbnails[fileID] = nil
    }

    func choose(_ next: ShareChoice) {
        guard !phase.isBusy else { return }
        userChose = true
        choice = next
        sendError = nil
        if next == .newThread { settleNewThread() }
    }

    func chooseFamily(_ family: AgentFamily) {
        guard newThread.family != family else { return }
        newThread.family = family
        newThread.modelID = newThreadModels.first?.id
    }

    func chooseProject(_ id: String) {
        guard newThread.projectID != id else { return }
        newThread.projectID = id
        if let family = includedProjects.first(where: { $0.id == id })?.lastFamily, availableFamilies.contains(family) { chooseFamily(family) }
    }

    func selectHost(_ host: String?) {
        guard host != hostID, !phase.isBusy else { return }
        hostID = host
        choice = nil; userChose = false
        threads = []; projects = []; models = nil
        newThread = NewThreadChoice()
        sendError = nil
        Task { await loadDestinations() }
    }

    private func loadConnections() {
        do {
            var saved: [SavedConnection] = []
            if let data = try readConnections() {
                saved = try JSONDecoder().decode(SavedConnections.self, from: data).connections
            }
            connections = saved.filter { !$0.requiresPairing }
            // The app's trust for home addresses and its relay addresses, for every saved computer.
            trust.update(connections)
            api.markRelay(Set(connections.flatMap { $0.routes.filter { $0.kind == .relay }.map(\.origin) }))
            if connections.isEmpty {
                setupMessage = saved.isEmpty ? "Open \(Self.appName) and pair it with your computer first."
                    : "Open \(Self.appName) to reconnect to your computer, then share again."
            }
            self.saved = UserDefaults.standard.dictionary(forKey: Self.lastDestinationKey) as? [String: String]
            hostID = (connections.first { $0.credential.hostInstallationId == self.saved?["host"] } ?? connections.first)?.credential.hostInstallationId
            refreshRecovery()
        } catch {
            setupMessage = "\(Self.appName) couldn’t open your saved computers. Unlock this device and try again."
        }
    }

    /// The oldest unconfirmed share to a computer this iPhone still has, other than this sheet's own.
    private func refreshRecovery() {
        let hosts = Set(connections.map(\.credential.hostInstallationId))
        recovery = journal.load().first { hosts.contains($0.host) && $0.id != current?.id }
    }

    // MARK: Routes

    /// The route to `saved`, chosen once per sheet as the app chooses it.
    private func route(_ saved: SavedConnection) async throws -> SavedConnection {
        let host = saved.credential.hostInstallationId
        let task: Task<SavedConnection, Error>
        if let running = routes[host] { task = running } else {
            let api = api, trust = trust, sign = sign
            task = Task {
                let routed = try await Self.connect(saved, api: api, trust: trust, sign: sign)
                return try await Self.renewIfEnding(routed, api: api, sign: sign)
            }
            routes[host] = task
        }
        do { return try await task.value }
        catch {
            if routes[host] == task { routes[host] = nil }
            throw error
        }
    }

    /// After a request fails to arrive, the next one chooses the route again.
    private func forgetRoute(_ connection: SavedConnection, after error: Error) async {
        guard Self.isTransport(error) else { return }
        routes[connection.credential.hostInstallationId] = nil
        if api.isRelay(connection.origin) { await api.closeSealed(origin: connection.origin) }
    }

    /// Home addresses with their pinned key (on the local network they win
    /// over a faster Tailscale answer), then Tailscale, then the relay's
    /// sealed connection. Every probe proves the computer's key before a
    /// session is sent. The choice is not saved: the app keeps its own routes.
    nonisolated static func connect(_ saved: SavedConnection, api: PairingAPI, trust: HomeNetworkTrust?,
                                    sign: SealedRPC.Sign?) async throws -> SavedConnection {
        // One ordinary address and no key to prove: nothing to choose, as in the app.
        guard saved.routes.count > 1 || saved.hostKeyPin != nil
                || saved.routes.contains(where: { $0.kind != .tailscale || $0.learned }) else { return saved }
        let keyChanged = RouteKeyMismatch()
        let selector = ConnectionRouteSelector(clock: ContinuousClock()) { route in
            do { try await ConnectionRouteProber.check(route, saved: saved, api: api, keyMismatches: trust, sign: sign) }
            catch PairingFailure.wrongHost where saved.hostKeyPin != nil {
                keyChanged.note()
                throw PairingFailure.wrongHost
            }
        }
        guard let selection = try await selector.select(saved.routes, localNetworkAvailable: true) else {
            throw keyChanged.seen ? PairingFailure.wrongHost : ShareFailure.unreachable
        }
        return try saved.usingRoute(origin: selection.route.origin)
    }

    nonisolated static func isTransport(_ error: Error) -> Bool {
        if let failure = error as? ShareStepFailure { return isTransport(failure.error) }
        if case ShareFailure.unreachable = error { return true }
        return error is URLError
    }

    // MARK: Destinations

    func loadDestinations() async {
        guard let saved = selectedConnection else { return }
        let request = UUID()
        destinationRequest = request
        loadingDestinations = true
        destinationError = nil
        someThreadsMissing = false
        defer { if destinationRequest == request { loadingDestinations = false } }
        var connection = saved
        do {
            connection = try await route(saved)
            guard destinationRequest == request else { return }
            async let catalog: ProjectsResponse = api.request("/api/v1/projects", origin: connection.origin, credential: connection.credential)
            async let options = optional(AgentOptions.self, "/api/v1/agent-options", connection)
            let response = try await catalog
            let modelList = await options
            guard destinationRequest == request else { return }
            models = modelList?.models
            projects = response.projects
            supportsModes = (response.modesVersion ?? 0) >= 1
            families = response.families
            let available = response.families.filter(\.available).map(\.family)
            availableFamilies = available.isEmpty ? AgentFamily.allCases : available
            threads = ShareCatalog.threads(projects: projects, pinned: response.pinned ?? [], pages: [:])
            restoreChoice()
            await loadThreads(response, connection: connection, request: request)
        } catch {
            await forgetRoute(connection, after: error)
            guard destinationRequest == request else { return }
            destinationError = Self.message(for: error, connection: saved)
        }
    }

    /// The session ends within a minute (the computer ends each one an hour
    /// after it starts, and only the app renews it while it is in front).
    nonisolated static func expired(_ connection: SavedConnection) -> Bool {
        guard let expiry = connection.credential.expiresAtMs else { return false }
        return expiry <= UInt64(Date().timeIntervalSince1970 * 1000) + 60_000
    }

    /// Signs this device in again with its key, in memory only: the app keeps
    /// its own saved session (the computer keeps both valid), so a share works
    /// whenever the app is paired, not only within an hour of opening it.
    /// A pairing that itself ended still fails with 401.
    nonisolated static func renewIfEnding(_ connection: SavedConnection, api: PairingAPI,
                                          sign: SealedRPC.Sign?) async throws -> SavedConnection {
        guard expired(connection) else { return connection }
        guard let sign else { throw PairingFailure.response(401) }
        let credential: Credential
        func overRequests() async throws -> Credential {
            struct Refresh: Encodable { let deviceId: String }
            struct Signed: Encodable { let challengeId: String; let signature: String }
            let challenge: Challenge = try await api.request("/api/v1/pairing/session/refresh-challenge", origin: connection.origin,
                body: JSONEncoder().encode(Refresh(deviceId: connection.credential.deviceId)))
            try challenge.validate(origin: connection.origin, hostID: connection.credential.hostInstallationId, deviceID: connection.credential.deviceId)
            let signature = try await sign(challenge.transcript)
            return try await api.request("/api/v1/pairing/session", origin: connection.origin,
                body: JSONEncoder().encode(Signed(challengeId: challenge.challengeId, signature: signature)))
        }
        if api.isRelay(connection.origin), let pin = connection.hostKeyPin {
            do { credential = try await api.renewSealed(origin: connection.origin, credential: connection.credential, pin: pin, sign: sign) }
            catch SealedRPCFailure.renewalUnsupported { credential = try await overRequests() }
        } else {
            credential = try await overRequests()
        }
        guard credential.deviceId == connection.credential.deviceId,
              credential.hostInstallationId == connection.credential.hostInstallationId else { throw PairingFailure.wrongHost }
        let renewed = connection.renewed(credential: credential)
        // A time-limited pairing returns its own end: nothing more to renew.
        if expired(renewed) { throw PairingFailure.response(401) }
        return renewed
    }

    /// Existing threads remain usable when the model catalog cannot be read.
    private func optional<T: Decodable & Sendable>(_ type: T.Type, _ path: String, _ connection: SavedConnection) async -> T? {
        try? await api.request(path, origin: connection.origin, credential: connection.credential)
    }

    /// Each Project's first page, four at a time; rows appear as pages arrive.
    /// A Mac can take many seconds per Project, so one slow Project never
    /// holds back the others.
    private func loadThreads(_ response: ProjectsResponse, connection: SavedConnection, request: UUID) async {
        let ids = response.projects.filter(\.isIncluded).map(\.id)
        var pages: [String: [ProjectThreadSummary]] = [:]
        loadingThreads = ids.count
        defer { if destinationRequest == request { loadingThreads = 0 } }
        await withTaskGroup(of: (String, [ProjectThreadSummary]?).self) { group in
            var pending = ids[...]
            func next() {
                guard let id = pending.popFirst() else { return }
                let api = api
                group.addTask {
                    let page: ProjectThreadsPage? = try? await api.request("/api/v1/projects/\(Self.escape(id))/threads?limit=10",
                        origin: connection.origin, credential: connection.credential)
                    return (id, page?.threads)
                }
            }
            for _ in 0..<4 { next() }
            for await (id, rows) in group {
                guard destinationRequest == request else { group.cancelAll(); return }
                loadingThreads -= 1
                if let rows { pages[id] = rows } else { someThreadsMissing = true }
                threads = ShareCatalog.threads(projects: response.projects, pinned: response.pinned ?? [], pages: pages)
                restoreChoice()
                next()
            }
        }
    }

    /// The last destination comes back once it is listed again, unless the
    /// user already picked another. Nothing else is preselected.
    private func restoreChoice() {
        guard !userChose, choice == nil, let saved, saved["host"] == hostID else {
            if choice == .newThread || newThread.projectID == nil { settleNewThread() }
            return
        }
        if saved["choice"] == "new" {
            if let project = saved["project"], includedProjects.contains(where: { $0.id == project }) {
                newThread.projectID = project
                if let family = saved["family"].flatMap(AgentFamily.init(rawValue:)), availableFamilies.contains(family) { newThread.family = family }
                newThread.modelID = saved["model"]
                choice = .newThread
            }
        } else if let id = saved["choice"], threads.contains(where: { $0.id == id }) {
            choice = .existing(id)
        }
        settleNewThread()
    }

    /// Keeps the new-thread choice on a Project, provider and model the Mac offers.
    private func settleNewThread() {
        if newThreadProject == nil, let first = includedProjects.first {
            newThread.projectID = first.id
            if let family = first.lastFamily { newThread.family = family }
        }
        if !availableFamilies.contains(newThread.family), let family = availableFamilies.first { newThread.family = family }
        if newThreadModel == nil { newThread.modelID = newThreadModels.first?.id }
    }

    // MARK: Items

    private func loadItems(_ providers: [NSItemProvider]) async {
        defer { loadingItems = false }
        var texts: [SharedText] = []
        var staged: [StagedFile] = []
        var sizeErrors: [String] = []
        var tooMany = 0, unreadable = 0
        for provider in providers.prefix(12) {
            do {
                switch try await Self.load(provider) {
                case .text(let text):
                    if !texts.contains(text) { texts.append(text) }
                case .file(let file, let thumbnail):
                    guard staged.count < ShareComposition.maximumFiles else { tooMany += 1; continue }
                    staged.append(file)
                    if let thumbnail { thumbnails[file.id] = UIImage(cgImage: thumbnail) }
                    if file.mimeType.hasPrefix("video/"), let source = file.source,
                       let poster = try? await AttachmentVideoPosters.shared.poster(key: file.sha256, source: source, mimeType: file.mimeType) {
                        thumbnails[file.id] = UIImage(cgImage: poster.image)
                        videoDurations[file.id] = poster.duration
                    }
                case nil: unreadable += 1
                }
            } catch FileFailure.tooLarge {
                let video = provider.hasItemConformingToTypeIdentifier(UTType.movie.identifier)
                let name = provider.suggestedName ?? (video ? "This video" : "This file")
                let limit = video ? "1 GB (1,024 MB)" : "8 MB"
                sizeErrors.append("\(name) is too large. The \(video ? "video" : "file") limit is \(limit).")
            }
            catch { unreadable += 1 }
        }
        tooMany += max(0, providers.count - 12)
        sharedTexts = texts
        files = staged
        var notes = sizeErrors
        if tooMany > 0 { notes.append("Only the first four files are attached.") }
        if unreadable > 0 { notes.append(unreadable == 1 ? "One item couldn’t be read." : "\(unreadable) items couldn’t be read.") }
        itemError = notes.isEmpty ? nil : notes.joined(separator: " ")
    }

    // MARK: Sending

    func send() async {
        guard canSend, let saved = selectedConnection, let choice else { return }
        phase = .preparing
        sendError = nil
        let body = messageBody
        let host = saved.credential.hostInstallationId
        var connection = saved
        do {
            connection = try await route(saved)
            var remembered: [String: String] = ["host": host]
            let target: String, title: String
            let destination: (String) throws -> PendingShare.Destination
            switch choice {
            case .existing:
                guard let thread = selectedTarget else { throw SendFailure.pending }
                let attach = thread.needsAttach(families.capabilities(for: thread.family))
                guard attach || thread.conversationID != nil else { throw SendFailure.pending }
                target = thread.id
                title = thread.title
                destination = { _ in .existing(targetID: thread.id, projectID: thread.projectID, reference: thread.reference,
                                               conversationID: thread.conversationID, attach: attach) }
                remembered["choice"] = thread.id
            case .newThread:
                guard let project = newThreadProject, let model = newThreadModel else { throw SendFailure.pending }
                let family = newThread.family, capabilities = families.capabilities(for: family)
                let supportsModes = self.supportsModes, deviceID = connection.credential.deviceId
                target = ["new", project.id, family.rawValue, model.id].joined(separator: "\n")
                title = "New thread in \(project.name)"
                // The creation's identity is the share's: a retry creates nothing new.
                destination = { id in .newThread(projectID: project.id, request: try ShareComposition.newThreadBody(project: project,
                    family: family, capabilities: capabilities, model: model, supportsModes: supportsModes,
                    deviceID: deviceID, requestID: id, body: body)) }
                remembered.merge(["choice": "new", "project": project.id, "family": family.rawValue, "model": model.id]) { _, new in new }
            }
            // The same content retried on this sheet is the same share; anything else is a new one.
            var item: PendingShare
            if let previous = current, previous.isRetry(host: host, target: target, body: body, fileIDs: files.map(\.id)) {
                item = previous
            } else {
                let id = UUID().uuidString.lowercased()
                item = PendingShare(id: id, host: host, deviceID: connection.credential.deviceId, target: target,
                                    destination: try destination(id), body: body, files: files, title: title)
                // Saved before anything reaches the computer, so a restart can
                // finish it. The files' bytes are written off the main actor.
                let journal = journal, pending = item, staged = files
                do { try await Task.detached(priority: .userInitiated) { try journal.begin(pending, files: staged) }.value }
                catch { throw ShareFailure.notSaved }
                current = item
            }
            let receipt = try await deliver(&item, files: files, connection: connection) { self.current = $0 }
            UserDefaults.standard.set(remembered, forKey: Self.lastDestinationKey)
            current = nil
            phase = .sent(queued: receipt.queued == true)
            try? await Task.sleep(for: .milliseconds(receipt.queued == true ? 1600 : 900))
            close()
            finish(true)
        } catch {
            await forgetRoute(connection, after: error)
            if let item = current, Self.refused(error) {
                // The computer said no: nothing more of this share will arrive.
                journal.remove(item.id)
                current = nil
            }
            phase = .idle
            sendError = Self.message(for: error, connection: saved, step: .send)
        }
    }

    /// Finishes the earlier share with its saved bytes and identities.
    func finishRecovery() async {
        guard var item = recovery, !recovering, !phase.isBusy,
              let saved = connections.first(where: { $0.credential.hostInstallationId == item.host }) else { return }
        recovering = true
        recoveryError = nil
        defer { recovering = false }
        let files: [StagedFile]
        let journal = journal, pending = item
        do { files = try await Task.detached(priority: .userInitiated) { try journal.files(for: pending) }.value } catch {
            journal.remove(item.id)
            refreshRecovery()
            recoveryError = "The earlier share’s files couldn’t be read, so it can’t be finished. Share them again."
            return
        }
        var connection = saved
        do {
            connection = try await route(saved)
            _ = try await deliver(&item, files: files, connection: connection) { _ in }
            recoveryNotice = "Your earlier share to “\(item.title)” was sent."
            refreshRecovery()
        } catch {
            await forgetRoute(connection, after: error)
            if Self.refused(error) {
                journal.remove(item.id)
                refreshRecovery()
            }
            recoveryError = Self.message(for: error, connection: saved, step: .send)
        }
    }

    func discardRecovery() {
        guard let item = recovery, !recovering else { return }
        journal.remove(item.id)
        recoveryError = nil
        refreshRecovery()
    }

    /// Runs whatever the item still needs: its conversation, each upload and
    /// the message. Each answer is saved before the next request, and the item
    /// is retired only once the computer confirms the message.
    private func deliver(_ item: inout PendingShare, files: [StagedFile], connection: SavedConnection,
                         progress: (PendingShare) -> Void) async throws -> SendReceipt {
        var step = ShareStep.openThread
        do {
            let conversationID: String
            if let id = item.conversationID {
                conversationID = id
            } else {
                switch item.destination {
                case .newThread(let projectID, let request):
                    step = .startThread
                    let response: CreateProjectThreadResponse = try await api.request("/api/v1/projects/\(Self.escape(projectID))/threads",
                        origin: connection.origin, body: request, credential: connection.credential)
                    guard let id = response.conversation.conversationId else { throw SendFailure.receiptMismatch }
                    conversationID = id
                case .existing(_, let projectID, let reference, let known, _):
                    let thread: ProjectThreadSummary = try await api.request("/api/v1/projects/\(Self.escape(projectID))/threads/attach",
                        origin: connection.origin, body: JSONSerialization.data(withJSONObject: ["reference": reference]), credential: connection.credential)
                    guard let id = thread.conversationId, known == nil || known == id else { throw SendFailure.receiptMismatch }
                    conversationID = id
                }
                item.conversationID = conversationID
                try record(item, progress)
            }
            step = .upload
            var attachmentIDs: [String] = []
            for (index, file) in files.enumerated() {
                if let uploaded = item.uploads[file.id] { attachmentIDs.append(uploaded.id); continue }
                if !recovering { phase = .uploading(done: index, total: files.count) }
                var uploadingFile = file
                uploadingFile.uploadID = item.uploadIDs?[file.id]
                let immutableFile = uploadingFile
                uploadBytes = 0; uploadTotalBytes = file.data.count
                let path = "/api/v1/conversations/\(Self.escape(conversationID))"
                let api = api
                let task = Task {
                    try await api.uploadAttachment(immutableFile, conversationPath: path,
                        origin: connection.origin, credential: connection.credential) { [weak self] received, total in
                            await self?.noteUploadBytes(received, total)
                        }
                }
                activeUpload = task
                canCancelUpload = true
                defer { activeUpload = nil; canCancelUpload = false }
                let uploaded: ConversationFile
                do { uploaded = try await task.value }
                catch {
                    if task.isCancelled {
                        var ids = item.uploadIDs ?? [:]
                        ids[file.id] = UUID().uuidString
                        item.uploadIDs = ids
                        try record(item, progress)
                    }
                    throw error
                }
                item.uploads[file.id] = uploaded
                try record(item, progress)
                attachmentIDs.append(uploaded.id)
            }
            step = .send
            if !recovering { phase = .sending }
            let request: SendRequest
            if let saved = item.request {
                request = saved
            } else {
                // No expected turn: the Mac queues the message behind a reply
                // that is running, exactly as the app's composer sends it.
                var intent = ComposerIntent()
                intent.draft = item.body
                intent.draftAttachmentIds = attachmentIDs
                try intent.begin(device: item.deviceID, clientMessageID: item.id)
                guard let pending = intent.pending else { throw SendFailure.pending }
                request = pending.request
                item.request = request
                try record(item, progress)
            }
            let path = "/api/v1/conversations/\(Self.escape(conversationID))/messages"
            let receipt: SendReceipt = try await api.request(path, origin: connection.origin,
                body: JSONEncoder().encode(request), credential: connection.credential)
            guard receipt.clientMessageId == request.clientMessageId, receipt.conversationId == conversationID,
                  !receipt.wonderMessageId.isEmpty, receipt.bodySha256 == ConversationFile.digest(Data(item.body.utf8)) else { throw SendFailure.receiptMismatch }
            journal.remove(item.id)
            return receipt
        } catch let error as ShareFailure {
            throw error
        } catch {
            throw ShareStepFailure(step: step, error: error)
        }
    }

    /// Saves an answer before the next request depends on it.
    private func noteUploadBytes(_ received: Int, _ total: Int) {
        uploadBytes = received; uploadTotalBytes = total
    }

    private func record(_ item: PendingShare, _ progress: (PendingShare) -> Void) throws {
        do { try journal.update(item) } catch { throw ShareFailure.notSaved }
        progress(item)
    }

    /// The computer answered and turned the request down, so retrying the same
    /// share cannot deliver it. Lost answers, timeouts and a busy computer stay
    /// uncertain, as do expired access and a changed key (fixed in the app).
    nonisolated static func refused(_ error: Error) -> Bool {
        let error = (error as? ShareStepFailure)?.error ?? error
        switch error {
        case PairingFailure.response(let status): return (400..<500).contains(status) && ![401, 403, 408, 429].contains(status)
        case PairingFailure.skillRejected(_, _), PairingFailure.annotationRejected(_, _), PairingFailure.hostMessage(_, _): return true
        case FileFailure.tooLarge: return true
        default: return false
        }
    }

    // MARK: Item loading

    /// Images and files become attachments; web links and plain text join the message.
    /// Providers stay on the main actor; decoding and re-encoding run detached.
    static func load(_ provider: NSItemProvider) async throws -> SharedItem? {
        let types = provider.registeredTypeIdentifiers.compactMap { UTType($0) }
        let suggestedName = provider.suggestedName
        // Keep the movie when Photos also offers a still representation.
        if let type = types.first(where: { $0.conforms(to: .movie) && $0.preferredMIMEType != nil }) {
            let file: StagedFile = try await withCheckedThrowingContinuation { continuation in
                _ = provider.loadFileRepresentation(forTypeIdentifier: type.identifier) { url, error in
                    guard let url else { continuation.resume(throwing: error ?? FileFailure.integrity); return }
                    do {
                        continuation.resume(returning: try StagedFile.importingVideo(url,
                            name: fileName(suggestedName, type: type, fallback: "Video"),
                            mimeType: type.preferredMIMEType ?? "video/quicktime"))
                    } catch { continuation.resume(throwing: error) }
                }
            }
            return .file(file, nil)
        }
        if let image = types.first(where: { $0.conforms(to: .image) }) {
            let file = try await imageFile(provider, type: image, suggestedName: suggestedName)
            return try await Task.detached(priority: .userInitiated) { SharedItem.file(file, thumbnail(file.data)) }.value
        }
        let isFile = types.contains { $0.conforms(to: .fileURL) } || suggestedName?.contains(".") == true
        if !isFile, types.contains(where: { $0.conforms(to: .url) }), let url = try await object(provider, URL.self), !url.isFileURL {
            return .text(.link(url))
        }
        if !isFile, types.contains(where: { $0.conforms(to: .text) }), let text = try await object(provider, String.self) {
            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            if trimmed.isEmpty { return nil }
            if let url = URL(string: trimmed), ["http", "https"].contains(url.scheme?.lowercased() ?? ""), !trimmed.contains(where: \.isWhitespace) {
                return .text(.link(url))
            }
            return .text(.text(trimmed))
        }
        guard let type = types.first(where: { $0.conforms(to: .data) && !$0.conforms(to: .url) }) else { return nil }
        let data = try await fileData(provider, type: type)
        return try await Task.detached(priority: .userInitiated) {
            SharedItem.file(try StagedFile(name: fileName(suggestedName, type: type, fallback: "File"),
                                           mimeType: type.preferredMIMEType ?? "application/octet-stream", data: data), nil)
        }.value
    }

    /// Reads a photo from its file when the provider has one, so a large
    /// original is downsampled from disk instead of being loaded whole.
    private static func imageFile(_ provider: NSItemProvider, type: UTType, suggestedName: String?) async throws -> StagedFile {
        do {
            return try await withCheckedThrowingContinuation { continuation in
                _ = provider.loadFileRepresentation(forTypeIdentifier: type.identifier) { url, error in
                    // The file exists only for the duration of this callback.
                    guard let url else { continuation.resume(throwing: error ?? FileFailure.integrity); return }
                    do {
                        let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
                        if size <= maximumBytes, type.preferredMIMEType != nil {
                            continuation.resume(returning: try prepareImage(Data(contentsOf: url), type: type, suggestedName: suggestedName))
                        } else {
                            guard let source = CGImageSourceCreateWithURL(url as CFURL, nil) else { throw FileFailure.integrity }
                            guard size <= maximumBytes || AttachmentImagePolicy.mayShrink(source) else { throw FileFailure.tooLarge }
                            continuation.resume(returning: try reencode(source, suggestedName: suggestedName))
                        }
                    } catch { continuation.resume(throwing: error) }
                }
            }
        } catch FileFailure.tooLarge { throw FileFailure.tooLarge }
        catch {
            let data = try await data(provider, type: type)
            return try await Task.detached(priority: .userInitiated) { try prepareImage(data, type: type, suggestedName: suggestedName) }.value
        }
    }

    private static func data(_ provider: NSItemProvider, type: UTType) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            _ = provider.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, error in
                if let data { continuation.resume(returning: data) }
                else { continuation.resume(throwing: error ?? FileFailure.integrity) }
            }
        }
    }

    private static func fileData(_ provider: NSItemProvider, type: UTType) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            _ = provider.loadFileRepresentation(forTypeIdentifier: type.identifier) { url, error in
                // The file exists only for the duration of this callback.
                guard let url else { continuation.resume(throwing: error ?? FileFailure.integrity); return }
                do {
                    let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
                    try AttachmentLimits.validate(byteSize: size, mimeType: type.preferredMIMEType ?? "application/octet-stream")
                    continuation.resume(returning: try Data(contentsOf: url, options: .alwaysMapped))
                } catch { continuation.resume(throwing: error) }
            }
        }
    }

    private static func object<T: _ObjectiveCBridgeable>(_ provider: NSItemProvider, _ type: T.Type) async throws -> T?
    where T._ObjectiveCType: NSItemProviderReading, T: Sendable {
        try await withCheckedThrowingContinuation { continuation in
            _ = provider.loadObject(ofClass: type) { value, error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume(returning: value) }
            }
        }
    }

    /// A still photo larger than the attachment limit is re-encoded as a
    /// bounded JPEG; a larger PNG, transparent or animated image is refused
    /// (`AttachmentImagePolicy`), as in the app.
    nonisolated static func prepareImage(_ original: Data, type: UTType, suggestedName: String?) throws -> StagedFile {
        if original.count > maximumBytes || type.preferredMIMEType == nil {
            guard let source = CGImageSourceCreateWithData(original as CFData, nil) else { throw FileFailure.integrity }
            guard original.count <= maximumBytes || AttachmentImagePolicy.mayShrink(source) else { throw FileFailure.tooLarge }
            return try reencode(source, suggestedName: suggestedName)
        }
        return try StagedFile(name: fileName(suggestedName, type: type, fallback: "Photo-\(UUID().uuidString.prefix(8))"),
                              mimeType: type.preferredMIMEType ?? "image/jpeg", data: original)
    }

    nonisolated private static func reencode(_ source: CGImageSource, suggestedName: String?) throws -> StagedFile {
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maximumPixels,
        ] as CFDictionary) else { throw FileFailure.integrity }
        let output = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(output, UTType.jpeg.identifier as CFString, 1, nil) else { throw FileFailure.integrity }
        CGImageDestinationAddImage(destination, image, [kCGImageDestinationLossyCompressionQuality: 0.85] as CFDictionary)
        guard CGImageDestinationFinalize(destination) else { throw FileFailure.integrity }
        return try StagedFile(name: fileName(suggestedName, type: .jpeg, fallback: "Photo-\(UUID().uuidString.prefix(8))"),
                              mimeType: "image/jpeg", data: output as Data)
    }

    nonisolated private static func thumbnail(_ data: Data) -> CGImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        return CGImageSourceCreateThumbnailAtIndex(source, 0, [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: 240,
        ] as CFDictionary)
    }

    /// The host accepts one path component without control characters.
    nonisolated static func fileName(_ suggested: String?, type: UTType, fallback: String) -> String {
        let cleaned = (suggested ?? "").components(separatedBy: CharacterSet(charactersIn: "/\\").union(.controlCharacters))
            .joined(separator: "-").trimmingCharacters(in: .whitespacesAndNewlines)
        var base = (cleaned as NSString).deletingPathExtension
        if base.isEmpty || base == "." || base == ".." { base = fallback }
        let ext = String((type.preferredFilenameExtension ?? (cleaned as NSString).pathExtension).prefix(32))
        while (base + "." + ext).utf8.count > 255 { base.removeLast() }
        return ext.isEmpty ? base : base + "." + ext
    }

    nonisolated static func escape(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? ""
    }

    nonisolated static func name(_ connection: SavedConnection) -> String {
        let name = connection.hostName?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return name.isEmpty ? "your computer" : name
    }

    nonisolated static func message(for error: Error, connection: SavedConnection, step: ShareStep? = nil) -> String {
        let mac = name(connection)
        switch error {
        case let failure as ShareStepFailure:
            return message(for: failure.error, connection: connection, step: failure.step)
        case is CancellationError:
            return "Upload cancelled. Your media is saved; try again when ready."
        case PairingFailure.response(401), PairingFailure.response(403):
            return "This device no longer has access to \(mac). Open \(Self.appName) to pair again."
        case PairingFailure.wrongHost:
            return "\(mac) couldn’t prove its security key, so nothing was sent. Open \(Self.appName) to pair it again."
        case PairingFailure.response(412):
            return "This chat’s model or folders changed. Open it in \(Self.appName) to send."
        case PairingFailure.response(404):
            return step == .startThread ? "This Project is no longer on \(mac). Choose another and try again." : "This chat is no longer available."
        case PairingFailure.response(409), PairingFailure.response(422):
            switch step {
            case .startThread: return "\(mac) couldn’t start a thread with this Project and model. Choose another and try again."
            case .openThread: return "\(mac) couldn’t open this thread. Open it in \(Self.appName) to send."
            case .upload: return "\(mac) didn’t accept one of the files. Remove it and try again."
            case .send, nil: return "\(mac) didn’t accept this message. Open the chat in \(Self.appName) to send it."
            }
        case PairingFailure.response(503):
            return "\(mac) isn’t ready yet. Try again in a moment; nothing will be sent twice."
        case PairingFailure.response(413), FileFailure.tooLarge:
            return "Videos must be 1 GB (1,024 MB) or smaller; other files must be 8 MB or smaller."
        case FileFailure.integrity, SendFailure.receiptMismatch:
            return "\(mac) didn’t confirm this message. Try again; it won’t be sent twice."
        case ShareFailure.notSaved:
            return "\(Self.appName) couldn’t save this share to retry it safely, so nothing was sent. Free up some storage and try again."
        case let error as URLError where error.code == .timedOut:
            return "\(mac) took too long to answer. Try again on a stronger connection."
        case ShareFailure.unreachable, is URLError:
            // Names only the ways in this computer has: same Wi-Fi, Tailscale or neither.
            return UnreachableAdvice.status(routes: connection.routes)
        default:
            return step == nil ? "Couldn’t load threads from \(mac)." : "Couldn’t send to \(mac)."
        }
    }
}

// MARK: - Views

/// The provider's mark, drawn from the app's `ProviderMark`; VoiceOver hears the name.
struct ShareProviderMark: View {
    let family: AgentFamily
    var size: CGFloat = 20
    @ScaledMetric(relativeTo: .body) private var scale: CGFloat = 1
    var body: some View {
        switch ProviderMark(family) {
        case .symbol(let name):
            Image(systemName: name)
                .font(.system(size: size * scale * 0.5, weight: .bold))
                .foregroundStyle(.white)
                .frame(width: size * scale, height: size * scale)
                .background(Color.black.opacity(0.85), in: RoundedRectangle(cornerRadius: size * scale * 0.22, style: .continuous))
                .accessibilityLabel(family.title)
        case .artwork(let name):
            Image(name)
                .resizable().interpolation(.high).scaledToFit()
                .frame(width: size * scale, height: size * scale)
                .accessibilityLabel(family.title)
        }
    }
}

struct ShareView: View {
    @ObservedObject var model: ShareModel
    let themeID: String?
    @Environment(\.colorScheme) private var scheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @State private var choosing = false
    @FocusState private var editingNote: Bool
    @ScaledMetric(relativeTo: .body) private var destinationIconWidth: CGFloat = 28

    var body: some View {
        let theme = ShareTheme(id: themeID, systemIsDark: scheme == .dark)
        GeometryReader { geometry in
            if let division = Self.displayDivision(in: geometry) {
                HStack(spacing: division.width) {
                    column(theme, content: true, destination: false)
                        .frame(width: division.minX)
                    column(theme, content: false, destination: true)
                        .frame(width: geometry.size.width - division.maxX)
                }
                .presentationSizing(.page)
            } else {
                column(theme, content: true, destination: true)
            }
        }
        .background(theme.page)
        .tint(theme.tint)
        .preferredColorScheme(theme.forcedScheme)
    }

    private static func displayDivision(in geometry: GeometryProxy) -> CGRect? {
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *),
           let region = geometry.reservedRegions(kind: .division, options: .includeInactive).first,
           region.frame.height > region.frame.width,
           region.frame.minX > 100, region.frame.maxX < geometry.size.width - 100 {
            return region.frame
        }
        #endif
        return nil
    }

    private func column(_ theme: ShareTheme, content: Bool, destination: Bool) -> some View {
        NavigationStack {
            List {
                if content {
                    recovery(theme)
                    Section { composer(theme) }
                        .listRowBackground(theme.row)
                }
                if destination {
                Section {
                    Button { choosing = true } label: {
                        HStack {
                            destinationLabel(theme)
                            Spacer(minLength: 8)
                            Image(systemName: "chevron.forward").font(.footnote.weight(.semibold))
                                .foregroundStyle(theme.secondary.opacity(0.6)).accessibilityHidden(true)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("share-destination")
                    if model.choice == .newThread { NewThreadOptions(model: model, theme: theme) }
                } header: { Text("Send to").foregroundStyle(theme.secondary) }
                    .listRowBackground(theme.row)
                status(theme)
                }
            }
            .listStyle(.insetGrouped)
            .listSectionSpacing(.compact)
            // On a phone the keyboard covers Send to; dragging the list puts it away.
            .scrollDismissesKeyboard(.interactively)
            .scrollContentBackground(.hidden)
            .background(theme.page)
            .foregroundStyle(theme.text)
            .disabled(model.phase.isBusy || model.phase != .idle)
            .navigationTitle(content ? ShareModel.appName : "")
            .navigationBarTitleDisplayMode(.inline)
            .navigationDestination(isPresented: Binding(get: { destination && choosing }, set: { choosing = $0 })) { ShareDestinationPicker(model: model, theme: theme) }
            .toolbar {
                if content {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { if model.canCancelUpload { model.cancelUpload() } else { model.cancel() } }
                        .disabled(model.phase.isBusy && !model.canCancelUpload)
                        .accessibilityLabel(model.canCancelUpload ? "Cancel upload" : "Cancel sharing")
                        .accessibilityIdentifier("share-cancel")
                }
                }
                if destination {
                ToolbarItem(placement: .confirmationAction) {
                    // Put the keyboard away so the progress or a refusal below shows.
                    Button(model.sendError == nil ? "Send" : "Try again") { editingNote = false; Task { await model.send() } }
                        .fontWeight(.semibold)
                        .tint(theme.accent)
                        .disabled(!model.canSend)
                        .accessibilityIdentifier(model.sendError == nil ? "share-send" : "share-retry")
                }
                }
            }
        }
    }

    // MARK: Recovery

    /// A share an earlier sheet started but the computer never confirmed.
    @ViewBuilder private func recovery(_ theme: ShareTheme) -> some View {
        if let item = model.recovery {
            let place = model.connections.first { $0.credential.hostInstallationId == item.host }.map(ShareModel.name) ?? "your computer"
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Label("Your earlier share to “\(item.title)” on \(place) may not have arrived.", systemImage: "arrow.clockwise.circle")
                    Text("Finishing sends it only if it didn’t arrive.")
                        .font(.footnote).foregroundStyle(theme.secondary)
                    if let error = model.recoveryError {
                        Text(error).font(.footnote).foregroundStyle(.red).accessibilityIdentifier("share-recovery-error")
                    }
                    HStack {
                        Button { Task { await model.finishRecovery() } } label: {
                            if model.recovering {
                                HStack(spacing: 6) { ProgressView(); Text("Sending…") }
                            } else { Text("Finish sending") }
                        }
                        .fontWeight(.semibold).foregroundStyle(theme.accent)
                        .disabled(model.recovering || model.phase.isBusy)
                        .accessibilityIdentifier("share-recovery-finish")
                        Spacer(minLength: 8)
                        Button("Discard", role: .destructive) { model.discardRecovery() }
                            .disabled(model.recovering)
                            .accessibilityIdentifier("share-recovery-discard")
                    }
                    .buttonStyle(.borderless)
                }
                .accessibilityElement(children: .contain)
                .accessibilityIdentifier("share-recovery")
            }
            .listRowBackground(theme.row)
        } else if let message = model.recoveryNotice ?? model.recoveryError {
            Section {
                Label(message, systemImage: model.recoveryNotice == nil ? "exclamationmark.circle" : "checkmark.circle.fill")
                    .font(.footnote)
                    .foregroundStyle(model.recoveryNotice == nil ? Color.red : theme.accent)
                    .accessibilityIdentifier("share-recovery-result")
            }
            .listRowBackground(theme.row)
        }
    }

    // MARK: Composer and preview

    @ViewBuilder private func composer(_ theme: ShareTheme) -> some View {
        TextField("Add a message", text: $model.note, axis: .vertical)
            .lineLimit(1...5)
            .focused($editingNote)
            .accessibilityIdentifier("share-note")
        if model.loadingItems {
            HStack(spacing: 8) { ProgressView(); Text("Preparing items…").foregroundStyle(theme.secondary) }
                .font(.subheadline)
        }
        if !model.files.isEmpty { attachmentStrip(theme) }
        ForEach(model.sharedTexts, id: \.self) { item in
            switch item {
            case .link(let url): LinkPreview(url: url, theme: theme)
            case .text(let text): QuotePreview(text: text, theme: theme)
            }
        }
        if let error = model.itemError {
            Label(error, systemImage: "exclamationmark.triangle")
                .font(.footnote).foregroundStyle(theme.text)
                .accessibilityIdentifier("share-item-error")
        }
    }

    private func attachmentStrip(_ theme: ShareTheme) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                ForEach(model.files) { file in
                    AttachmentTile(file: file, thumbnail: model.thumbnails[file.id], duration: model.videoDurations[file.id], theme: theme) { model.remove(file.id) }
                }
            }
            .padding(.vertical, 6)
        }
        .scrollClipDisabled()
    }

    // MARK: Destination

    @ViewBuilder private func destinationLabel(_ theme: ShareTheme) -> some View {
        let layout = dynamicTypeSize.isAccessibilitySize
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 12))
            : AnyLayout(HStackLayout(spacing: 12))
        layout {
            switch model.choice {
            case .newThread:
                Image(systemName: ConceptSymbol.newThread).font(.body.weight(.medium)).frame(width: destinationIconWidth).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text("New thread")
                    Text(model.newThreadProject.map { "\($0.name) · \(model.macName)" } ?? model.macName)
                        .font(.footnote).foregroundStyle(theme.secondary)
                }
            case .existing:
                if let target = model.selectedTarget {
                    TargetIcon(target: target).frame(width: destinationIconWidth)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(target.title).lineLimit(1)
                        Text([target.projectName, model.macName].joined(separator: " · "))
                            .font(.footnote).foregroundStyle(theme.secondary).lineLimit(1)
                        if target.isWorking {
                            Text("Replying now · your message will wait its turn")
                                .font(.footnote).foregroundStyle(theme.accent)
                        }
                    }
                } else { choosePrompt(theme) }
            case nil:
                choosePrompt(theme)
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
        .accessibilityHint("Choose where to send")
    }

    @ViewBuilder private func choosePrompt(_ theme: ShareTheme) -> some View {
        Image(systemName: ConceptSymbol.thread).frame(width: destinationIconWidth).accessibilityHidden(true)
        VStack(alignment: .leading, spacing: 2) {
            Text("Choose a thread").foregroundStyle(theme.accent)
            if let message = model.setupMessage ?? model.destinationError {
                Text(message).font(.footnote).foregroundStyle(theme.secondary)
            } else if model.loadingDestinations {
                Text("Loading from \(model.macName)…").font(.footnote).foregroundStyle(theme.secondary)
            }
        }
    }

    // MARK: Status

    @ViewBuilder private func status(_ theme: ShareTheme) -> some View {
        let reason = model.blockReason
        if model.phase != .idle || model.sendError != nil || reason != nil {
            Section {
                switch model.phase {
                case .preparing:
                    progress("Preparing…", value: nil, theme: theme)
                case .uploading(let done, let total):
                    progress(total == 1 ? "Uploading file…" : "Uploading \(done + 1) of \(total)…",
                             value: model.uploadTotalBytes > 0 ? Double(model.uploadBytes) / Double(model.uploadTotalBytes) : nil, theme: theme)
                    Text("\(ByteCountFormatter.string(fromByteCount: Int64(model.uploadBytes), countStyle: .file)) of \(ByteCountFormatter.string(fromByteCount: Int64(model.uploadTotalBytes), countStyle: .file))")
                        .font(.caption).foregroundStyle(theme.secondary)
                case .sending:
                    progress("Sending…", value: 1, theme: theme)
                case .sent(let queued):
                    Label(queued ? "Queued. It sends when the current reply finishes." : "Sent",
                          systemImage: queued ? "clock.badge.checkmark" : "checkmark.circle.fill")
                        .foregroundStyle(theme.accent)
                        .accessibilityIdentifier(queued ? "share-queued" : "share-sent")
                case .idle:
                    if let error = model.sendError {
                        Label(error, systemImage: "exclamationmark.circle").foregroundStyle(.red)
                            .accessibilityIdentifier("share-error")
                    } else if let reason {
                        Label(reason, systemImage: "info.circle").font(.footnote).foregroundStyle(theme.secondary)
                    }
                }
            }
            .listRowBackground(theme.row)
        }
    }

    private func progress(_ title: String, value: Double?, theme: ShareTheme) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.subheadline)
            if let value { ProgressView(value: value).tint(theme.accent) } else { ProgressView().frame(maxWidth: .infinity, alignment: .leading) }
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("share-progress")
    }
}

private struct AttachmentTile: View {
    let file: StagedFile
    let thumbnail: UIImage?
    var duration: String? = nil
    let theme: ShareTheme
    let remove: () -> Void
    @ScaledMetric(relativeTo: .body) private var side: CGFloat = 72
    private var isVideo: Bool { file.mimeType.hasPrefix("video/") }
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    var body: some View {
        let layout = dynamicTypeSize.isAccessibilitySize
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 10))
            : AnyLayout(HStackLayout(spacing: 10))
        Group {
            if isVideo {
                layout {
                    Group {
                        if let thumbnail {
                            Image(uiImage: thumbnail).resizable().scaledToFill()
                        } else {
                            Image(systemName: "video").font(.title2).foregroundStyle(theme.secondary)
                        }
                    }
                    .frame(width: min(side, 96), height: min(side, 96)).clipped()
                    .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(file.name).font(.caption).lineLimit(2).truncationMode(.middle)
                        Text([duration, ByteCountFormatter.string(fromByteCount: Int64(file.data.count), countStyle: .file)]
                            .compactMap { $0 }.joined(separator: " · "))
                            .font(.caption2).foregroundStyle(theme.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(8)
                .containerRelativeFrame(.horizontal) { available, _ in min(available, side * 3 + 26) }
            } else if let thumbnail {
                VStack(spacing: 2) {
                    Image(uiImage: thumbnail).resizable().scaledToFill().frame(width: side, height: side).clipped()
                    if let duration {
                        Text("\(duration) · \(ByteCountFormatter.string(fromByteCount: Int64(file.data.count), countStyle: .file))").font(.caption2)
                    }
                }
            } else {
                VStack(spacing: 4) {
                    Image(systemName: "doc").font(.title3)
                    Text(file.name).font(.caption2).lineLimit(2).multilineTextAlignment(.center)
                    Text(ByteCountFormatter.string(fromByteCount: Int64(file.data.count), countStyle: .file))
                        .font(.caption2).foregroundStyle(theme.secondary)
                }
                .padding(6).frame(width: side * 1.4, height: side)
            }
        }
        .background(theme.page)
        .clipShape(RoundedRectangle(cornerRadius: 10))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel([file.name, duration, ByteCountFormatter.string(fromByteCount: Int64(file.data.count), countStyle: .file)].compactMap { $0 }.joined(separator: ", "))
        .overlay(alignment: .topTrailing) {
            Button(action: remove) {
                Image(systemName: "xmark.circle.fill")
                    .font(isVideo ? .system(size: 20) : .body)
                    .symbolRenderingMode(.palette)
                    .foregroundStyle(.white, .black.opacity(0.6))
                    .frame(width: isVideo ? 44 : 32, height: isVideo ? 44 : 32)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .offset(x: 8, y: -8)
            .accessibilityLabel("Remove \(file.name)")
            .accessibilityIdentifier("share-attachment-remove")
        }
    }
}

private struct LinkPreview: View {
    let url: URL
    let theme: ShareTheme
    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "link").font(.subheadline.weight(.semibold)).foregroundStyle(theme.secondary)
                .frame(width: 28, height: 28).background(theme.page, in: RoundedRectangle(cornerRadius: 7))
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 1) {
                Text(url.host(percentEncoded: false) ?? url.absoluteString).font(.subheadline.weight(.semibold)).lineLimit(1)
                Text(url.absoluteString).font(.footnote).foregroundStyle(theme.secondary).lineLimit(1).truncationMode(.middle)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("share-link-preview")
    }
}

private struct QuotePreview: View {
    let text: String
    let theme: ShareTheme
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            RoundedRectangle(cornerRadius: 1.5).fill(theme.secondary.opacity(0.5)).frame(width: 3).accessibilityHidden(true)
            Text(text).font(.subheadline).foregroundStyle(theme.secondary).lineLimit(4)
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("share-text-preview")
    }
}

private struct TargetIcon: View {
    let target: ShareTarget
    var body: some View {
        ShareProviderMark(family: target.family, size: 22)
    }
}

private struct NewThreadOptions: View {
    @ObservedObject var model: ShareModel
    let theme: ShareTheme

    var body: some View {
        Picker("Project", selection: Binding(get: { model.newThread.projectID ?? "" }, set: { model.chooseProject($0) })) {
            ForEach(model.includedProjects) { Text($0.name).tag($0.id) }
        }
        .accessibilityIdentifier("share-new-project")
        if model.availableFamilies.count > 1 {
            Picker("Agent", selection: Binding(get: { model.newThread.family }, set: { model.chooseFamily($0) })) {
                ForEach(model.availableFamilies) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .accessibilityIdentifier("share-new-family")
        }
        if model.models == nil {
            Text(model.loadingDestinations ? "Loading models…" : "\(model.macName) didn’t list its models. Go back and try again.")
                .font(.footnote).foregroundStyle(theme.secondary)
        } else {
            Picker("Model", selection: Binding(get: { model.newThread.modelID ?? "" }, set: { model.newThread.modelID = $0 })) {
                ForEach(model.newThreadModels) { Text($0.displayName).tag($0.id) }
            }
            .accessibilityIdentifier("share-new-model")
        }
    }
}

struct ShareDestinationPicker: View {
    @ObservedObject var model: ShareModel
    let theme: ShareTheme
    @Environment(\.dismiss) private var dismiss
    private static let relative: RelativeDateTimeFormatter = {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter
    }()

    var body: some View {
        List {
            if model.connections.count > 1 {
                Section {
                    Picker("Computer", selection: Binding(get: { model.hostID }, set: { model.selectHost($0) })) {
                        ForEach(model.connections, id: \.credential.hostInstallationId) { connection in
                            Text(ShareModel.name(connection)).tag(Optional(connection.credential.hostInstallationId))
                        }
                    }
                    .accessibilityIdentifier("share-computer")
                }
                .listRowBackground(theme.row)
            }
            if !model.includedProjects.isEmpty, model.query.isEmpty {
                Section {
                    Button { model.choose(.newThread); dismiss() } label: {
                        HStack(spacing: 12) {
                            Image(systemName: ConceptSymbol.newThread).font(.body.weight(.medium)).frame(width: 28).accessibilityHidden(true)
                            Text("New thread")
                            Spacer(minLength: 8)
                            if model.choice == .newThread { checkmark }
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(model.choice == .newThread ? .isSelected : [])
                    .accessibilityIdentifier("share-new-thread")
                }
                .listRowBackground(theme.row)
            }
            content
        }
        .accessibilityElement(children: .contain).accessibilityIdentifier("share-destination-list")
        .listStyle(.insetGrouped)
        .listSectionSpacing(.compact)
        .scrollContentBackground(.hidden)
        .background(theme.page)
        .searchable(text: $model.query, placement: .navigationBarDrawer(displayMode: .always), prompt: "Search threads")
        .navigationTitle("Send to")
        .navigationBarTitleDisplayMode(.inline)
    }

    @ViewBuilder private var content: some View {
        if let message = model.setupMessage {
            Text(message).foregroundStyle(theme.secondary).listRowBackground(theme.row)
        } else if let error = model.destinationError {
            Section {
                Text(error)
                Button("Try again") { Task { await model.loadDestinations() } }
                    .foregroundStyle(theme.accent).accessibilityIdentifier("share-destinations-retry")
            }
            .listRowBackground(theme.row)
        } else {
            let sections = model.sections
            ForEach(sections) { section in
                Section {
                    ForEach(section.targets) { target in row(target) }
                } header: { Text(section.title).foregroundStyle(theme.secondary) }
                    .listRowBackground(theme.row)
            }
            if model.loadingDestinations || model.loadingThreads > 0 {
                HStack(spacing: 8) { ProgressView(); Text("Loading threads…").foregroundStyle(theme.secondary) }
                    .font(.subheadline).listRowBackground(theme.row)
            } else if sections.isEmpty {
                Text(model.query.isEmpty ? "No threads on \(model.macName) yet." : "No matching threads.")
                    .foregroundStyle(theme.secondary).listRowBackground(theme.row)
            }
            if model.someThreadsMissing {
                Label("Some Projects’ threads couldn’t be loaded.", systemImage: "exclamationmark.triangle")
                    .font(.footnote).foregroundStyle(theme.secondary).listRowBackground(theme.row)
            }
        }
    }

    private var checkmark: some View {
        Image(systemName: "checkmark").fontWeight(.semibold).foregroundStyle(theme.accent).accessibilityHidden(true)
    }

    private func row(_ target: ShareTarget) -> some View {
        let selected = model.choice == .existing(target.id)
        let place = target.projectName
        let when = Self.relative.localizedString(for: target.updatedAt, relativeTo: Date())
        return Button { model.choose(.existing(target.id)); dismiss() } label: {
            HStack(spacing: 12) {
                TargetIcon(target: target).frame(width: 28)
                VStack(alignment: .leading, spacing: 2) {
                    Text(target.title).foregroundStyle(theme.text).lineLimit(1)
                    HStack(spacing: 4) {
                        Text([place, when].joined(separator: " · ")).lineLimit(1)
                        if target.isWorking { Text("· Replying").foregroundStyle(theme.accent) }
                    }
                    .font(.footnote).foregroundStyle(theme.secondary)
                }
                Spacer(minLength: 8)
                if selected { checkmark }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityIdentifier("share-target:" + target.id)
    }
}
#endif
