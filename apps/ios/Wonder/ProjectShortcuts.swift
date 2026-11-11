import AppIntents
import Foundation
import WonderPairing

/// Saved shortcuts keep the pairing and Project, not just a thread ID. Re-pairing
/// must ask the owner to choose again instead of silently using another identity.
struct ProjectShortcutAddress: Equatable, Sendable {
    let host: String
    let device: String
    let project: String
    var family: AgentFamily?
    var reference: String?

    var id: String {
        var parts = [host, device, project]
        if let family, let reference { parts += [family.rawValue, reference] }
        return parts.joined(separator: "/")
    }

    init(host: String, device: String, project: String, family: AgentFamily? = nil, reference: String? = nil) {
        self.host = host; self.device = device; self.project = project
        self.family = family; self.reference = reference
    }

    var conversationID: String? {
        guard let reference, reference.hasPrefix("wonder:") else { return nil }
        return String(reference.dropFirst("wonder:".count))
    }

    func matches(_ detail: ProjectConversationDetail, conversation: String) -> Bool {
        detail.conversationId == conversation && detail.projectId == project &&
            detail.family == family && detail.isArchived != true
    }

    init?(id: String) {
        guard id.utf8.count <= 640 else { return nil }
        let parts = id.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        guard (parts.count == 3 || parts.count == 5), parts.prefix(3).allSatisfy(WonderDeepLink.validID) else { return nil }
        host = parts[0]; device = parts[1]; project = parts[2]
        if parts.count == 5 {
            guard let family = AgentFamily(rawValue: parts[3]) else { return nil }
            let reference = parts[4].split(separator: ":", omittingEmptySubsequences: false).map(String.init)
            guard reference.count == 2, [family.rawValue, "wonder"].contains(reference[0]),
                  WonderDeepLink.validID(reference[1]) else { return nil }
            if reference[0] != "wonder" {
                guard reference[1].utf8.allSatisfy({
                    (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) || $0 == 45
                }) else { return nil }
            }
            self.family = family; self.reference = parts[4]
        }
    }
}

struct ShortcutNavigation: Equatable {
    let id = UUID()
    let address: ProjectShortcutAddress
    let target: WonderDeepLink
}

enum ProjectShortcutFailure: LocalizedError {
    case locked, pairing, project, thread, status, projectStatus
    case message, agent, delivery
    case connection(String), timedOut
    var errorDescription: String? {
        switch self {
        case .locked: "Unlock your device and open Cupola to load your computers."
        case .pairing: "Open Cupola and pair this computer, then choose it in the shortcut again."
        case .project: "Open Cupola and choose an available Project for this shortcut."
        case .thread: "Open this chat in Cupola, then choose it in the shortcut again."
        case .status: "This chat’s current status is unavailable. Open it in Cupola to check."
        case .projectStatus: "This Project’s current status is unavailable. Open it in Cupola to check."
        case .message: "Enter a message of up to 64 KB."
        case .agent: "This agent is unavailable. Open Cupola and check its setup on your computer."
        case .delivery: "Open Cupola to check your message’s delivery before trying again."
        case .connection(let name): "Open Cupola and check the connection to \(name), then try again."
        case .timedOut: "Open Cupola and check your computer’s connection, then try again."
        }
    }
}

struct WonderProjectEntity: AppEntity {
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Project"
    static let defaultQuery = WonderProjectQuery()
    let id: String
    let name: String
    let hostName: String
    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)", subtitle: "\(hostName)", image: .init(systemName: "folder"))
    }
}

struct WonderThreadEntity: AppEntity {
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Chat"
    static let defaultQuery = WonderThreadQuery()
    let id: String
    let title: String
    let subtitle: String
    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(title)", subtitle: "\(subtitle)", image: .init(systemName: ConceptSymbol.thread))
    }
}

struct WonderProjectQuery: EntityStringQuery {
    @Dependency var library: ConnectionLibrary
    @MainActor func entities(for identifiers: [String]) async throws -> [WonderProjectEntity] {
        let wanted = Set(identifiers)
        return try library.shortcutProjects().filter { wanted.contains($0.id) }
    }
    @MainActor func suggestedEntities() async throws -> [WonderProjectEntity] { try library.shortcutProjects() }
    @MainActor func entities(matching string: String) async throws -> [WonderProjectEntity] {
        try library.shortcutProjects().filter { $0.name.localizedStandardContains(string) || $0.hostName.localizedStandardContains(string) }
    }
}

struct WonderThreadQuery: EntityStringQuery {
    @Dependency var library: ConnectionLibrary
    @MainActor func entities(for identifiers: [String]) async throws -> [WonderThreadEntity] {
        try await library.resolveShortcutThreads(identifiers)
    }
    @MainActor func suggestedEntities() async throws -> [WonderThreadEntity] { try library.shortcutThreads() }
    @MainActor func entities(matching string: String) async throws -> [WonderThreadEntity] {
        try library.shortcutThreads().filter { $0.title.localizedStandardContains(string) || $0.subtitle.localizedStandardContains(string) }
    }
}

struct NewProjectThreadIntent: AppIntent {
    static let title: LocalizedStringResource = "New chat in Project"
    static let description = IntentDescription("Opens the Project’s composer in Cupola. Your saved draft stays available; nothing is sent.")
    static let openAppWhenRun = true
    @Parameter(title: "Project") var project: WonderProjectEntity
    @Dependency var library: ConnectionLibrary
    static var parameterSummary: some ParameterSummary { Summary("New chat in \(\.$project)") }

    @MainActor func perform() async throws -> some IntentResult {
        guard let address = ProjectShortcutAddress(id: project.id), address.family == nil else { throw ProjectShortcutFailure.project }
        try library.requestShortcut(address)
        return .result()
    }
}

/// Every agent the app knows; whether one can take the message is the Mac's
/// answer when the shortcut runs (`ProjectShortcutFailure.agent` otherwise).
/// App Intents needs literal display names, so a new provider is added here.
enum ShortcutAgent: String, AppEnum {
    case codex, claude, opencode, cursor
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Agent"
    static let caseDisplayRepresentations: [Self: DisplayRepresentation] = [
        .codex: "Codex", .claude: "Claude", .opencode: "OpenCode", .cursor: "Cursor"
    ]
    var family: AgentFamily {
        switch self { case .codex: .codex; case .claude: .claude; case .opencode: .opencode; case .cursor: .cursor } // provider identity
    }
}

struct AskProjectAgentIntent: AppIntent {
    static let title: LocalizedStringResource = "Ask an agent in Project"
    static let description = IntentDescription("Sends a message in a new chat using the agent’s default model. Your saved composer draft stays unchanged.")
    static let openAppWhenRun = true
    static let authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication
    @Parameter(title: "Project") var project: WonderProjectEntity
    @Parameter(title: "Agent") var agent: ShortcutAgent
    @Parameter(title: "Message") var message: String
    @Dependency var library: ConnectionLibrary
    static var parameterSummary: some ParameterSummary { Summary("Ask \(\.$agent) \(\.$message) in \(\.$project)") }

    @MainActor func perform() async throws -> some IntentResult & ReturnsValue<String> & ProvidesDialog {
        guard let address = ProjectShortcutAddress(id: project.id), address.family == nil else { throw ProjectShortcutFailure.project }
        let result = try await library.askShortcut(address, family: agent.family, message: message)
        return .result(value: result, dialog: "\(result)")
    }
}

struct OpenProjectThreadIntent: OpenIntent {
    static let title: LocalizedStringResource = "Open chat"
    @Parameter(title: "Chat") var target: WonderThreadEntity
    @Dependency var library: ConnectionLibrary
    static var parameterSummary: some ParameterSummary { Summary("Open \(\.$target)") }

    @MainActor func perform() async throws -> some IntentResult {
        guard let address = ProjectShortcutAddress(id: target.id), address.family != nil else { throw ProjectShortcutFailure.thread }
        try library.requestShortcut(address)
        return .result()
    }
}

/// A status check reads the selected chat without opening it or marking it read.
struct CheckProjectThreadIntent: AppIntent {
    static let title: LocalizedStringResource = "Check chat"
    static let description = IntentDescription("Checks whether a saved chat is working or needs input. Open a desktop-only chat in Cupola before choosing it.")
    static let authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication
    @Parameter(title: "Chat", optionsProvider: StatusThreadOptions()) var thread: WonderThreadEntity
    @Dependency var library: ConnectionLibrary
    static var parameterSummary: some ParameterSummary { Summary("Check \(\.$thread)") }

    @MainActor func perform() async throws -> some IntentResult & ReturnsValue<String> & ProvidesDialog {
        guard let address = ProjectShortcutAddress(id: thread.id), address.conversationID != nil else {
            throw ProjectShortcutFailure.thread
        }
        let status = try await library.shortcutStatus(address)
        return .result(value: status, dialog: "\(status)")
    }
}

struct StatusThreadOptions: DynamicOptionsProvider {
    @Dependency var library: ConnectionLibrary
    @MainActor func results() async throws -> [WonderThreadEntity] {
        try library.shortcutThreads().filter { ProjectShortcutAddress(id: $0.id)?.conversationID != nil }
    }
}

struct CheckProjectIntent: AppIntent {
    static let title: LocalizedStringResource = "Check Project"
    static let description = IntentDescription("Checks which chats are working or need input. Reports any status the computer cannot verify.")
    static let authenticationPolicy: IntentAuthenticationPolicy = .requiresLocalDeviceAuthentication
    @Parameter(title: "Project") var project: WonderProjectEntity
    @Dependency var library: ConnectionLibrary
    static var parameterSummary: some ParameterSummary { Summary("Check \(\.$project)") }

    @MainActor func perform() async throws -> some IntentResult & ReturnsValue<String> & ProvidesDialog {
        guard let address = ProjectShortcutAddress(id: project.id), address.family == nil else { throw ProjectShortcutFailure.project }
        let status = try await library.shortcutProjectStatus(address)
        return .result(value: status, dialog: "\(status)")
    }
}

enum ShortcutThreadState: String, Sendable {
    case needsInput = "Needs input", working = "Working", idle = "Idle", unavailable = "Status unavailable"
}

private struct ShortcutThreadResult: Sendable {
    let title: String
    let state: ShortcutThreadState
    var inputKnown = true
    var display: String {
        let suffix = inputKnown ? "" : "; input status unavailable"
        return "\(String(title.prefix(160))): \(state.rawValue)\(suffix)"
    }
}

/// Native process state and turn outcomes are different: a completed reply does
/// not prove that the session's background work has stopped.
struct ShortcutThreadActivity: Decodable, Sendable {
    let latestTurnStatus: String
    let runningElsewhere: Bool
    let tasksOnMac: Bool?
    let threadStatus: NativeThreadStatus?

    var awaitsInput: Bool {
        threadStatus?.type == "active" && (threadStatus?.activeFlags ?? []).contains {
            ["waitingOnApproval", "waitingOnUserInput"].contains($0)
        }
    }

    func status(needsInput: Bool) throws -> ShortcutThreadState {
        if needsInput || awaitsInput { return .needsInput }
        if threadStatus?.isWorking == true || latestTurnStatus == "inProgress" || runningElsewhere || tasksOnMac == true {
            return .working
        }
        guard threadStatus?.isWorking == false else { throw ProjectShortcutFailure.status }
        return .idle
    }
}

// Shortcut symbols are literals the build extracts; they match ConceptSymbol.
struct WonderShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: AskProjectAgentIntent(),
                    phrases: ["Ask an agent in \(.applicationName)", "Ask an agent in \(\.$project) with \(.applicationName)"],
                    shortTitle: "Ask an agent", systemImageName: "paperplane")
        AppShortcut(intent: NewProjectThreadIntent(),
                    phrases: ["New chat in \(.applicationName)", "New chat in \(\.$project) with \(.applicationName)"],
                    shortTitle: "New chat", systemImageName: "square.and.pencil")
        AppShortcut(intent: OpenProjectThreadIntent(),
                    phrases: ["Open a chat in \(.applicationName)", "Open \(\.$target) in \(.applicationName)"],
                    shortTitle: "Open chat", systemImageName: "bubble.left.and.text.bubble.right")
        AppShortcut(intent: CheckProjectThreadIntent(),
                    phrases: ["Check a chat in \(.applicationName)", "Check \(\.$thread) in \(.applicationName)"],
                    shortTitle: "Check chat", systemImageName: "ellipsis.bubble")
        AppShortcut(intent: CheckProjectIntent(),
                    phrases: ["Check a Project in \(.applicationName)", "Check \(\.$project) with \(.applicationName)"],
                    shortTitle: "Check Project", systemImageName: "folder")
        AppShortcut(intent: StartWonderVoiceIntent(),
                    phrases: ["Start \(.applicationName) Voice", "Talk to \(.applicationName)"],
                    shortTitle: "Cupola Voice", systemImageName: "waveform")
    }
}

extension ConnectionLibrary {
    /// The normal creation and delivery owners retain the request before either
    /// write. An uncertain result remains reviewable in New Chat's saved messages.
    func askShortcut(_ address: ProjectShortcutAddress, family: AgentFamily, message: String,
                     timeout: Duration = .seconds(45)) async throws -> String {
        guard address.family == nil, address.reference == nil else { throw ProjectShortcutFailure.project }
        guard !message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              message.utf8.count <= ShareComposition.maximumBodyBytes else { throw ProjectShortcutFailure.message }
        let model = try shortcutModel(address)
        do {
            return try await withThrowingTaskGroup(of: String.self) { group in
                defer { group.cancelAll() }
                group.addTask {
                    try await Task.sleep(for: timeout)
                    throw ProjectShortcutFailure.delivery
                }
                group.addTask { try await self.sendShortcut(address, model: model, family: family, message: message) }
                guard let result = try await group.next() else { throw ProjectShortcutFailure.delivery }
                return result
            }
        } catch let failure as ProjectShortcutFailure { throw failure }
        catch is CancellationError { throw CancellationError() }
        catch { throw ProjectShortcutFailure.connection(model.macName) }
    }

    private func sendShortcut(_ address: ProjectShortcutAddress, model: ConnectionModel,
                              family: AgentFamily, message: String) async throws -> String {
        let fence = try await readyShortcutFence(address, model: model)
        func validate() throws { try validateShortcut(address, model: model, fence: fence) }
        async let catalog: ProjectsResponse = model.manage("/api/v1/projects")
        async let options: AgentOptions = model.manage("/api/v1/agent-options")
        let (projects, choices) = try await (catalog, options)
        try validate()
        guard let project = projects.projects.first(where: { $0.id == address.project && $0.isIncluded }),
              project.primaryFolder?.isAvailable == true else { throw ProjectShortcutFailure.project }
        guard projects.families.contains(where: { $0.family == family && $0.available }) else { throw ProjectShortcutFailure.agent }
        var draft = NewChatDraft(destination: .project(id: project.id), text: message, family: family)
        draft.ensureModel(among: choices.models.filter { $0.family == family && !$0.hidden })
        guard draft.model != nil else { throw ProjectShortcutFailure.agent }
        draft.folderId = project.primaryFolder?.id
        draft.rootsRevision = project.rootsRevision
        draft.submittedDeviceID = address.device
        draft.freeze()
        guard NewChatDraftStore.save(draft, host: address.host, separately: true, makeCurrent: false) else {
            throw ProjectShortcutFailure.delivery
        }
        do {
            let created = try await model.projects.createThread(projectID: project.id, draft: draft, body: message, deviceID: address.device)
            try validate()
            guard let conversation = created.conversation.conversationId else { throw ProjectShortcutFailure.delivery }
            draft.preparedConversationID = conversation
            guard NewChatDraftStore.save(draft, host: address.host, separately: true, makeCurrent: false) else {
                throw ProjectShortcutFailure.delivery
            }
            let detail = try await model.projects.loadDetail(conversation)
            try validate()
            let target = ProjectShortcutAddress(host: address.host, device: address.device, project: address.project,
                                                family: family, reference: "wonder:" + conversation)
            guard target.matches(detail, conversation: conversation) else { throw ProjectShortcutFailure.thread }
            let chat = model.projectChat(detail)
            try await model.prepareCreation(chat, body: message, requestID: draft.requestID, files: [])
            try validate()
            // Delivery now owns the durable intent; retiring this saved creation
            // leaves every independent composer draft unchanged.
            NewChatDraftStore.removePending(host: address.host, requestID: draft.requestID)
            await model.deliver(chat)
            try validate()
            let accepted = model.composers[conversation]?.pending?.receipt?.clientMessageId == draft.requestID ||
                model.snapshots[conversation]?.messages.contains { $0.clientMessageId == draft.requestID } == true
            guard accepted else { throw ProjectShortcutFailure.delivery }
            try requestShortcut(target)
            return "Sent to \(family.title) in \(project.name)."
        } catch is CancellationError { throw CancellationError() }
        catch { throw ProjectShortcutFailure.delivery }
    }

    /// Queries reuse the full private Project cache already loaded by the app.
    /// They never scan providers, fetch history, or expose message contents.
    func shortcutProjects() throws -> [WonderProjectEntity] {
        var result: [WonderProjectEntity] = []
        for model in try shortcutModels() {
            guard let connection = model.connection else { continue }
            for project in model.projects.includedProjects {
                let address = ProjectShortcutAddress(host: connection.credential.hostInstallationId,
                    device: connection.credential.deviceId, project: project.id)
                guard ProjectShortcutAddress(id: address.id) != nil else { continue }
                result.append(WonderProjectEntity(id: address.id, name: project.name, hostName: model.macName))
            }
        }
        return result
    }

    func shortcutThreads() throws -> [WonderThreadEntity] {
        var result: [WonderThreadEntity] = []
        for model in try shortcutModels() {
            guard let connection = model.connection else { continue }
            for project in model.projects.includedProjects {
                let rows = (model.projects.threads[project.id]?.threads ?? []) +
                    model.projects.pinned.filter { $0.projectId == project.id }.map(\.thread)
                var seen = Set<String>()
                for thread in rows {
                    let address = ProjectShortcutAddress(host: connection.credential.hostInstallationId,
                        device: connection.credential.deviceId, project: project.id,
                        family: thread.family, reference: thread.conversationId.map { "wonder:" + $0 } ?? thread.reference)
                    guard seen.insert(address.id).inserted, ProjectShortcutAddress(id: address.id) != nil,
                          thread.conversationId != nil || model.projects.capabilities(thread.family).nativeSessions,
                          !threadIsUnavailable(thread, model: model) else { continue }
                    result.append(WonderThreadEntity(id: address.id, title: thread.title.isEmpty ? "Untitled chat" : thread.title,
                        subtitle: "\(thread.family.title) · \(project.name) · \(model.macName)"))
                }
            }
        }
        return result
    }

    /// Resolve saved conversation choices even after their sidebar row ages out.
    /// At most 16 exact reads share one deadline. Native-only lookups never
    /// attach a session, and suggestions never perform these network reads.
    func resolveShortcutThreads(_ identifiers: [String], timeout: Duration = .seconds(20)) async throws -> [WonderThreadEntity] {
        guard identifiers.count <= 16 else { throw ProjectShortcutFailure.thread }
        let cached = try shortcutThreads()
        var found = Dictionary(cached.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        let missing = Set(identifiers).filter { found[$0] == nil }.compactMap(ProjectShortcutAddress.init(id:))
            .filter { $0.conversationID != nil }
        if !missing.isEmpty {
            try await withThrowingTaskGroup(of: WonderThreadEntity?.self) { group in
                defer { group.cancelAll() }
                group.addTask {
                    try await Task.sleep(for: timeout)
                    throw ProjectShortcutFailure.timedOut
                }
                for address in missing {
                    group.addTask { try await self.resolveShortcutThread(address) }
                }
                for _ in missing {
                    guard let result = try await group.next() else { break }
                    if let entity = result { found[entity.id] = entity }
                }
            }
        }
        return identifiers.compactMap { found[$0] }
    }

    private func resolveShortcutThread(_ address: ProjectShortcutAddress) async throws -> WonderThreadEntity? {
        let model = try shortcutModel(address)
        guard let conversation = address.conversationID else { return nil }
        let detail: ProjectConversationDetail
        do { detail = try await model.projects.loadDetail(conversation) }
        catch PairingFailure.response(404) { return nil }
        catch PairingFailure.hostMessage(404, _) { return nil }
        catch is CancellationError { throw CancellationError() }
        catch { throw ProjectShortcutFailure.connection(model.macName) }
        guard try shortcutModel(address) === model, address.matches(detail, conversation: conversation) else { return nil }
        return WonderThreadEntity(id: address.id, title: detail.title.isEmpty ? "Untitled chat" : detail.title,
                                  subtitle: "\(detail.family.title) · \(detail.projectName) · \(model.macName)")
    }

    /// One deadline covers validation plus three independent native reads. No
    /// catalog paging, session attachment, history hydration, or cached answer.
    func shortcutStatus(_ address: ProjectShortcutAddress, timeout: Duration = .seconds(20)) async throws -> String {
        guard let conversation = address.conversationID else { throw ProjectShortcutFailure.thread }
        let model = try shortcutModel(address)
        do {
            return try await withThrowingTaskGroup(of: String.self) { group in
                defer { group.cancelAll() }
                group.addTask {
                    try await Task.sleep(for: timeout)
                    throw ProjectShortcutFailure.timedOut
                }
                group.addTask {
                    let fence = try await self.readyShortcutFence(address, model: model)
                    return try await self.readShortcutStatus(address, model: model, conversation: conversation, fence: fence).display
                }
                guard let result = try await group.next() else { throw ProjectShortcutFailure.status }
                return result
            }
        } catch let failure as ProjectShortcutFailure { throw failure }
        catch is CancellationError { throw CancellationError() }
        catch { throw ProjectShortcutFailure.connection(model.macName) }
    }

    private func readShortcutStatus(_ address: ProjectShortcutAddress, model: ConnectionModel, conversation: String,
                                    fence: String, sharedRequests: [AttentionRequest]? = nil) async throws -> ShortcutThreadResult {
        let detail = try await model.projects.loadDetail(conversation)
        guard address.matches(detail, conversation: conversation) else { throw ProjectShortcutFailure.thread }
        async let activity: ShortcutThreadActivity = model.manage("/api/v1/conversations/\(ConnectionModel.escape(conversation))/history/activity")
        async let questions: [AsyncQuestion] = model.manage("/api/v1/conversations/\(ConnectionModel.escape(conversation))/questions")
        let requests: [AttentionRequest]
        if let sharedRequests { requests = sharedRequests }
        else { requests = try await model.manage("/api/v1/approvals") }
        let values = try await (activity, questions)
        try validateShortcut(address, model: model, fence: fence)
        let needsInput = SidebarSessionFilter.awaitsInput(conversation: conversation, requests: requests,
            questions: values.1, now: UInt64(Date().timeIntervalSince1970 * 1000))
        // The current questions API returns at most 100 rows. A full page may
        // omit an older unanswered question, so it cannot establish all-clear.
        guard needsInput || values.0.awaitsInput || values.1.count < 100 else { throw ProjectShortcutFailure.status }
        let label = try values.0.status(needsInput: needsInput)
        let title = detail.title.isEmpty ? "Untitled chat" : String(detail.title.prefix(160))
        return ShortcutThreadResult(title: title, state: label)
    }

    /// Read the whole current catalog, with one deadline and at most four saved
    /// chats in flight. A quiet desktop row alone never establishes all-clear.
    func shortcutProjectStatus(_ address: ProjectShortcutAddress, timeout: Duration = .seconds(30)) async throws -> String {
        guard address.family == nil, address.reference == nil else { throw ProjectShortcutFailure.project }
        let model = try shortcutModel(address)
        do {
            return try await withThrowingTaskGroup(of: String.self) { group in
                defer { group.cancelAll() }
                group.addTask {
                    try await Task.sleep(for: timeout)
                    throw ProjectShortcutFailure.timedOut
                }
                group.addTask {
                    let fence = try await self.readyShortcutFence(address, model: model)
                    return try await self.readProjectShortcutStatus(address, model: model, fence: fence)
                }
                guard let result = try await group.next() else { throw ProjectShortcutFailure.projectStatus }
                return result
            }
        } catch let failure as ProjectShortcutFailure { throw failure }
        catch is CancellationError { throw CancellationError() }
        catch { throw ProjectShortcutFailure.connection(model.macName) }
    }

    private func readProjectShortcutStatus(_ address: ProjectShortcutAddress, model: ConnectionModel,
                                           fence: String) async throws -> String {
        func validate() throws { try validateShortcut(address, model: model, fence: fence) }
        async let catalog: ProjectsResponse = model.manage("/api/v1/projects")
        async let approvals: [AttentionRequest] = model.manage("/api/v1/approvals")
        let (projects, requests) = try await (catalog, approvals)
        try validate()
        guard let project = projects.projects.first(where: { $0.id == address.project && $0.isIncluded }) else {
            throw ProjectShortcutFailure.project
        }
        var rows: [ProjectThreadSummary] = []
        var seen = Set<String>(), cursors = Set<String>()
        var cursor: String?
        for _ in 0..<10 {
            var path = "/api/v1/projects/\(ConnectionModel.escape(address.project))/threads?limit=30"
            if let cursor { path += "&cursor=" + ConnectionModel.escape(cursor) }
            let page: ProjectThreadsPage = try await model.manage(path)
            try validate()
            guard page.partial.isEmpty else { throw ProjectShortcutFailure.projectStatus }
            for row in page.threads {
                let key = row.conversationId.map { "wonder:" + $0 } ?? row.reference
                if seen.insert(key).inserted { rows.append(row) }
            }
            guard rows.count <= 300 else { throw ProjectShortcutFailure.projectStatus }
            cursor = page.nextCursor
            guard let cursor else { break }
            guard !cursor.isEmpty, cursor.utf8.count <= 4096, cursors.insert(cursor).inserted else {
                throw ProjectShortcutFailure.projectStatus
            }
        }
        guard cursor == nil else { throw ProjectShortcutFailure.projectStatus }
        var results: [ShortcutThreadResult] = []
        for start in stride(from: 0, to: rows.count, by: 4) {
            let batch = rows[start..<min(start + 4, rows.count)]
            let checked = try await withThrowingTaskGroup(of: (Int, ShortcutThreadResult).self) { group in
                for (index, row) in batch.enumerated() {
                    group.addTask {
                        let title = row.title.isEmpty ? "Untitled chat" : row.title
                        guard let conversation = row.conversationId else {
                            return (index, ShortcutThreadResult(title: title, state: row.isWorking ? .working : .unavailable,
                                                                inputKnown: !row.isWorking))
                        }
                        let target = ProjectShortcutAddress(host: address.host, device: address.device, project: address.project,
                                                            family: row.family, reference: "wonder:" + conversation)
                        do {
                            _ = try await self.shortcutModel(target)
                            return (index, try await self.readShortcutStatus(target, model: model, conversation: conversation,
                                                                             fence: fence, sharedRequests: requests))
                        } catch is CancellationError { throw CancellationError() }
                        catch { return (index, ShortcutThreadResult(title: title, state: .unavailable)) }
                    }
                }
                var batchResults: [(Int, ShortcutThreadResult)] = []
                for try await result in group { batchResults.append(result) }
                return batchResults.sorted { $0.0 < $1.0 }.map(\.1)
            }
            try validate()
            results += checked
        }
        try validate()
        let working = results.filter { $0.state == .working }.count
        let input = results.filter { $0.state == .needsInput }.count
        let idle = results.filter { $0.state == .idle }.count
        let unknown = results.filter { $0.state == .unavailable || !$0.inputKnown }.count
        let inputVerb = input == 1 ? "needs" : "need"
        var lines = ["\(String(project.name.prefix(160))): \(working) working, \(input) \(inputVerb) input, \(idle) idle."]
        if unknown > 0 {
            let noun = unknown == 1 ? "chat" : "chats"
            lines.append("Status incomplete for \(unknown) \(noun).")
        }
        let attention = results.filter { $0.state == .needsInput } + results.filter { $0.state == .working } +
            results.filter { $0.state == .unavailable }
        lines += attention.prefix(12).map(\.display)
        if attention.count > 12 { lines.append("\(attention.count - 12) more chats. Open the Project in Cupola to see them.") }
        return lines.joined(separator: "\n")
    }

    /// Each operation establishes the connection once: a cold launch or a moved
    /// network picks its route and renews the session here, before any read.
    /// The returned fence survives those changes and breaks only when the
    /// pairing is replaced, revoked or forgotten.
    private func readyShortcutFence(_ address: ProjectShortcutAddress, model: ConnectionModel) async throws -> String {
        let ready = await model.connectionReady()
        try Task.checkCancellation()
        guard try shortcutModel(address) === model else { throw ProjectShortcutFailure.pairing }
        guard ready else { throw ProjectShortcutFailure.connection(model.macName) }
        return model.pairingFence
    }

    private func validateShortcut(_ address: ProjectShortcutAddress, model: ConnectionModel, fence: String) throws {
        try Task.checkCancellation()
        guard try shortcutModel(address) === model, model.pairingFence == fence else { throw ProjectShortcutFailure.pairing }
    }

    private func shortcutModel(_ address: ProjectShortcutAddress) throws -> ConnectionModel {
        guard ProjectShortcutAddress(id: address.id) == address else { throw ProjectShortcutFailure.project }
        let models = try shortcutModels()
        guard let model = models.first(where: {
            $0.connection?.credential.hostInstallationId == address.host &&
            $0.connection?.credential.deviceId == address.device
        }) else { throw ProjectShortcutFailure.pairing }
        guard model.projects.includedProjects.contains(where: { $0.id == address.project }) else { throw ProjectShortcutFailure.project }
        return model
    }

    func shortcutTarget(_ address: ProjectShortcutAddress) throws -> WonderDeepLink {
        let model = try shortcutModel(address)
        if let conversation = address.conversationID {
            guard !model.projects.unavailable.contains(conversation),
                  model.projects.details[conversation].map({ address.matches($0, conversation: conversation) }) ?? true else {
                throw ProjectShortcutFailure.thread
            }
            return .chat(host: address.host, id: conversation)
        }
        guard let family = address.family, let reference = address.reference else {
            return .newProjectChat(host: address.host, project: address.project)
        }
        let rows = (model.projects.threads[address.project]?.threads ?? []) +
            model.projects.pinned.filter { $0.projectId == address.project }.map(\.thread)
        guard let thread = rows.first(where: { $0.reference == reference && $0.family == family }),
              !threadIsUnavailable(thread, model: model) else { throw ProjectShortcutFailure.thread }
        if let conversation = thread.conversationId, WonderDeepLink.validID(conversation) {
            return .chat(host: address.host, id: conversation)
        }
        // A provider without nativeSessions exposes Wonder-created
        // conversations only; its desktop sessions cannot be attached.
        guard model.projects.capabilities(family).nativeSessions, let native = thread.nativeID else { throw ProjectShortcutFailure.thread }
        return .projectThread(host: address.host, project: address.project, family: family, thread: native)
    }

    func requestShortcut(_ address: ProjectShortcutAddress) throws {
        shortcutNavigation = ShortcutNavigation(address: address, target: try shortcutTarget(address))
    }

    /// Only the active window consumes a launch request. Cold launches retain it
    /// until the shell is ready; another iPad window cannot open it a second time.
    func takeShortcut(scene: UUID) throws -> ShortcutNavigation? {
        guard navigationScene == scene, let request = shortcutNavigation else { return nil }
        shortcutNavigation = nil
        guard try shortcutTarget(request.address) == request.target else { throw ProjectShortcutFailure.thread }
        return request
    }

    private func shortcutModels() throws -> [ConnectionModel] {
        load()
        guard loaded else { throw ProjectShortcutFailure.locked }
        guard !saved.connections.isEmpty else { throw ProjectShortcutFailure.pairing }
        let models = saved.connections.filter { !$0.requiresPairing }.compactMap { connection -> ConnectionModel? in
            let model = model(for: connection)
            guard !model.accessEnded, model.connection?.credential.deviceId == connection.credential.deviceId else { return nil }
            return model
        }
        guard !models.isEmpty else { throw ProjectShortcutFailure.pairing }
        return models
    }

    private func threadIsUnavailable(_ thread: ProjectThreadSummary, model: ConnectionModel) -> Bool {
        guard let id = thread.conversationId else { return false }
        return model.projects.unavailable.contains(id) || model.projects.details[id]?.isArchived == true
    }
}
