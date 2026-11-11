import SwiftUI
import WonderPairing

/// Projects on one paired Mac. Requests are fenced by the connection scope so a
/// late response from a replaced pairing or another Mac is never applied.
@MainActor final class ProjectLibrary: ObservableObject {
    private weak var model: ConnectionModel?
    /// Shared by every computer library; one phone-local document, never a host setting.
    let sidebarOrder = SidebarOrderStore.shared
    private func observeOrderMembership() {
        guard matchesCurrentConnection, let host = model?.connection?.credential.hostInstallationId else { return }
        sidebarOrder.observe([SidebarHostProjects(hostID: host, name: "", isOnline: true,
            supportsProjects: supportsProjects, projects: projects, threads: threads, pinned: pinned)])
    }
    /// nil until the Mac first answers, then true.
    @Published private(set) var supportsProjects: Bool?
    @Published private(set) var projects: [ProjectSummary] = [] { didSet { observeOrderMembership() } }
    @Published private(set) var includeWorktrees: Bool?
    @Published private(set) var savingWorktrees = false
    @Published private(set) var worktreesFailure: String?
    private var worktreesRevision = 0
    /// Advanced by each auto-archive save, so a read begun before it cannot
    /// replace the saved rule.
    private var autoArchiveRevision = 0
    /// The computer's auto-archive rule; nil until it answers or on hosts without it.
    @Published private(set) var autoArchive: AutoArchiveRule?
    @Published private(set) var savingAutoArchive = false
    @Published private(set) var autoArchiveFailure: String?
    /// Whether the computer continues work by itself; nil until it answers or
    /// on hosts without it.
    @Published private(set) var continuation: ContinuationSettings?
    @Published private(set) var savingContinuation = false
    @Published private(set) var continuationFailure: String?
    private var continuationRevision = 0
    /// Conversations whose scheduled resume is being cancelled.
    @Published private(set) var cancellingResume: Set<String> = []
    @Published private(set) var resumeFailures: [String: String] = [:]
    @Published private(set) var families: [ProjectFamilyAvailability] = []
    @Published private(set) var threads: [String: ProjectThreadsState] = [:] { didSet { noteMacPermissionWaits(); observeOrderMembership(); model?.armWorkWatchdog() } }
    @Published private(set) var details: [String: ProjectConversationDetail] = [:]
    /// This Mac's chat detail per provider. The Mac owns it; this is its last
    /// answer, kept in the offline cache so a saved conversation still shows it.
    @Published private(set) var chatDetail: ChatDetailSettings?
    @Published private(set) var loadingProjects = false
    /// The host accepts Claude approval modes and plan mode.
    /// The Mac can open a terminal in a Project conversation's folder.
    @Published private(set) var supportsTerminal = false
    @Published private(set) var supportsModes = false
    @Published private(set) var supportsArchive = false
    @Published private(set) var supportsDelete = false
    /// The Mac deletes a Project's Cupola copy: its chats go, its folders and agent history stay.
    @Published private(set) var supportsProjectDelete = false
    /// The Mac archives Claude Code threads in Wonder and lists archived threads.
    @Published private(set) var supportsArchiveList = false
    @Published private(set) var supportsArchiveBatch = false
    /// Pinned threads across this Mac's included projects.
    @Published private(set) var pinned: [PinnedProjectThread] = [] { didSet { noteMacPermissionWaits(); observeOrderMembership(); model?.armWorkWatchdog() } }
    /// Conversations last listed as waiting on a macOS dialog.
    private var macPermissionWaits: Set<String> = []
    /// Conversations the Mac reported gone while a saved copy was still open.
    @Published private(set) var unavailable: Set<String> = []
    // Detail and history load independently. A late detail response must not
    // erase a history 404; only a successful history read can do that.
    private var unavailableHistory: Set<String> = []
    private var unavailableDetails: Set<String> = []
    func historyNotFound(_ conversation: String) {
        unavailableHistory.insert(conversation)
        unavailable.insert(conversation)
    }
    func historyLoaded(_ conversation: String) {
        if unavailableHistory.remove(conversation) != nil, !unavailableDetails.contains(conversation) { unavailable.remove(conversation) }
    }
    /// An explicit unread action stays unread until the owner opens the thread
    /// again, even when its conversation is still visible beside an iPad sidebar.
    @Published private(set) var manuallyUnread: Set<String> = []
    /// Library working-status refresh only; each Project owns its thread-page failures.
    @Published private(set) var partial: [ProjectPartialFailure] = []
    @Published var failure: String?
    /// Model catalog for project composer settings, per connection scope.
    @Published var options: AgentOptions?
    var widgetSnapshotChanged: (() -> Void)?
    var owner: ConnectionModel? { model }
    /// A replaced pairing cannot render the previous connection’s catalog.
    var matchesCurrentConnection: Bool { scope == model?.assignmentScope }
    private var scope: String?
    private var threadTasks: [String: Task<Void, Never>] = [:]
    /// A first-page read may have started before a prepare-only conversation was created.
    private var createdDuringThreadRead: [String: Set<String>] = [:]
    /// Projects whose Show more waits for a refresh already reading them.
    private var pendingMore: Set<String> = []
    /// Consecutive answers per Project that came back while the Mac was
    /// still reading a provider; reset by a complete answer.
    private var stillReadingAnswers: [String: Int] = [:]
    /// The automatic re-read after a still-reading answer, per Project.
    private var continuations: [String: Task<Void, Never>] = [:]
    /// Still-reading answers read again by themselves before the list says the Mac is slow.
    static let stillReadingRetries = 3
    /// Wait before re-reading a still-reading answer, multiplied by the attempt.
    var stillReadingDelay: Duration = .seconds(1)
    private var refreshTask: Task<Void, Never>?
    private var refreshID: UUID?
    private var refreshPinRevision = 0
    /// Newest first; bounded so saved history stays small (see `retainedConversationIDs`).
    private var recentlyOpened = RecentlyOpenedConversations()
    /// Bumped by every local pin change so an older refresh cannot undo it.
    private var pinRevision = 0
    /// Bumped when a Project is saved, so a catalog read that began earlier
    /// cannot put the old hide/show or pin state back.
    private var projectSaveRevision = 0
    private var pendingPins: [String: Bool] = [:]
    /// A response begun before a successful archive cannot put that row back.
    private var archivedReferences: Set<String> = []
    private var detailRevisions: [String: Int] = [:]
    /// Bumped by each chat detail change so an older read cannot undo it.
    private var chatDetailRevision = 0
    /// Per-chat Show saves in flight: the newest request, and the level the Mac last confirmed.
    private struct ChatDetailPending { var confirmed: ChatDetailLevel?; var latest = 0; var inFlight = 0 }
    private var chatDetailPending: [String: ChatDetailPending] = [:]
    private var updatingDetails: Set<String> = []
    /// Fences older catalog/detail reads and automatic acknowledgements against
    /// an explicit read-status change, independently of pin changes.
    private var readRevisions: [String: Int] = [:]
    /// In-memory detail cache bound; open, recent, pinned and first-page threads always stay.
    private static let detailLimit = 120
    #if WONDER_DIAGNOSTICS
    private var previewFilesDetail: ProjectConversationDetail?
    private var previewFilesScope: String?
    func installPreviewFilesConversation(_ detail: ProjectConversationDetail) {
        previewFilesDetail = detail
        previewFilesScope = model?.assignmentScope
        details[detail.conversationId] = detail
    }
    #endif
    #if DEBUG || WONDER_DIAGNOSTICS
    /// The provider records a synthetic preview's Mac sends; previews never refresh Projects.
    func installPreviewFamilies(_ records: [ProjectFamilyAvailability]) {
        families = records
    }
    #endif

    init(model: ConnectionModel) {
        self.model = model
        scope = model.assignmentScope
        restoreCache()
        installPreviewFilesProject()
    }

    private func installPreviewFilesProject() {
        #if WONDER_DIAGNOSTICS
        if model?.previewMode == true && (ProcessInfo.processInfo.arguments.contains("-project-files-preview") ||
            ProcessInfo.processInfo.arguments.contains("-project-files-conversation-preview")) {
            // A read-only new-chat destination for the Files UI fixture.
            supportsProjects = true
            var folders = [ProjectFolder(id: "preview-folder", path: "/preview", name: "Preview",
                                         isPrimary: true, isAvailable: true)]
            if ProcessInfo.processInfo.arguments.contains("-project-files-multiple-folders-preview") {
                folders.append(ProjectFolder(id: "second-folder", path: "/second", name: "Second",
                                             isPrimary: false, isAvailable: true))
            }
            projects = [ProjectSummary(id: "preview-project", name: "Preview project", folders: folders)]
        }
        if model?.previewMode == true, DiagnosticTerminalTransport.enabled { supportsTerminal = true }
        if previewFilesScope == model?.assignmentScope, let previewFilesDetail {
            details[previewFilesDetail.conversationId] = previewFilesDetail
        }
        #endif
    }

    /// Refreshes write through this: @Published emits on every assignment, so
    /// an unchanged catalog, flag or page must leave the value alone.
    private func setIfChanged<Value: Equatable>(_ path: ReferenceWritableKeyPath<ProjectLibrary, Value>, _ value: Value) {
        if self[keyPath: path] != value { self[keyPath: path] = value }
    }

    var includedProjects: [ProjectSummary] { SidebarProjection.sorted(projects.filter(\.isIncluded)) }

    func isAvailable(_ family: AgentFamily) -> Bool {
        families.first { $0.family == family }?.available ?? false
    }

    /// A new chat may start on `family` by default: offered, listed and not signed out.
    func isStartable(_ family: AgentFamily) -> Bool {
        families.lists(family) && isAvailable(family) && families.first { $0.family == family }?.signedOut != true
    }

    /// What `family` offers on this Mac; views gate features on this record.
    func capabilities(_ family: AgentFamily) -> ProviderCapabilities {
        families.capabilities(for: family)
    }

    private func fenced() -> String? {
        guard let model, !model.accessEnded, model.connection != nil else { return nil }
        let current = model.assignmentScope
        if scope != current {
            // A different pairing or Mac: nothing from the previous scope applies.
            scope = current
            threadTasks.values.forEach { $0.cancel() }
            refreshTask?.cancel(); refreshTask = nil; refreshID = nil
            threadTasks = [:]
            pendingMore = []
            continuations.values.forEach { $0.cancel() }; continuations = [:]; stillReadingAnswers = [:]
            createdDuringThreadRead = [:]
            includeWorktrees = nil; savingWorktrees = false; worktreesFailure = nil
            autoArchive = nil; savingAutoArchive = false; autoArchiveFailure = nil
            continuation = nil; savingContinuation = false; continuationFailure = nil
            cancellingResume = []; resumeFailures = [:]
            projects = []; families = []; loadingProjects = false; failure = nil; partial = []
            threads = [:]; details = [:]; supportsProjects = nil; options = nil; chatDetail = nil
            supportsTerminal = false; supportsModes = false; supportsArchive = false; supportsArchiveList = false; pinned = []; unavailable = []; unavailableHistory = []; unavailableDetails = []; manuallyUnread = []; recentlyOpened = RecentlyOpenedConversations()
            supportsArchiveBatch = false; supportsDelete = false; supportsProjectDelete = false
            archivedReferences = []
            pendingPins = [:]; detailRevisions = [:]; updatingDetails = []; readRevisions = [:]
            restoreCache()
            installPreviewFilesProject()
        }
        return current
    }

    private func isCurrent(_ expected: String) -> Bool {
        scope == expected && model?.assignmentScope == expected && model?.accessEnded == false && !Task.isCancelled
    }

    func refresh(refreshVisibleDetail: Bool = true) async {
        guard let expected = fenced() else { return }
        if let refreshTask {
            let id = refreshID, revision = refreshPinRevision
            await refreshTask.value
            guard isCurrent(expected) else { return }
            if refreshID == id { self.refreshTask = nil; refreshID = nil }
            // A save that joined an older catalog read needs a fresh read.
            if revision != pinRevision { await refresh(refreshVisibleDetail: refreshVisibleDetail); return }
            if refreshVisibleDetail, let current = model?.selectedChat, model?.isProject(current) == true { await refreshDetail(current.id) }
            return
        }
        let id = UUID()
        refreshID = id; refreshPinRevision = pinRevision
        let task = Task { await performRefresh() }
        refreshTask = task
        await task.value
        if isCurrent(expected), refreshID == id { refreshTask = nil; refreshID = nil }
        if isCurrent(expected), refreshVisibleDetail, let current = model?.selectedChat, model?.isProject(current) == true { await refreshDetail(current.id) }
    }

    /// Event-driven list reads never reload an unrelated open chat's detail.
    /// Unknown/newly running chats and pins need the catalog for discovery;
    /// known rows need only their affected Project pages.
    func refreshEventRows(_ conversations: Set<String>, discover: Bool) async {
        guard let expected = fenced() else { return }
        var affected: Set<String> = []
        var needsCatalog = discover || conversations.isEmpty
        for id in conversations {
            if pinned.contains(where: { $0.thread.conversationId == id }) { needsCatalog = true }
            if let project = details[id]?.projectId ?? threads.first(where: {
                $0.value.threads.contains(where: { $0.conversationId == id })
            })?.key {
                affected.insert(project)
                if threads[project] == nil { needsCatalog = true }
            }
            else { needsCatalog = true }
        }
        // A direct foreground read may already own the catalog or pages.
        // Wait for those reads, then start this event's pass; joining an older
        // page alone could miss the update that requested this refresh.
        if let task = refreshTask {
            let id = refreshID
            await task.value
            guard isCurrent(expected) else { return }
            if refreshID == id { refreshTask = nil; refreshID = nil }
        }
        let previousPages = needsCatalog ? Array(threadTasks.values) : affected.compactMap { threadTasks[$0] }
        for task in previousPages { await task.value }
        guard isCurrent(expected) else { return }
        if needsCatalog { await refresh(refreshVisibleDetail: false) }
        guard isCurrent(expected) else { return }
        for project in affected where threads[project] != nil { loadThreads(project) }
        // Catalog reads start loaded pages too. Keep those reads in this pass,
        // so a notice during a page read receives a trailing refresh.
        let tasks = needsCatalog ? Array(threadTasks.values) : affected.compactMap { threadTasks[$0] }
        for task in tasks { await task.value }
    }

    private func performRefresh() async {
        guard let model, let scope = fenced(), !model.previewMode else { return }
        loadingProjects = true
        defer { if scope == self.scope { loadingProjects = false } }
        // Read beside the catalog; a failed read keeps the cached choice.
        let chatDetailRevisionAtStart = chatDetailRevision
        async let chatDetailRead: ChatDetailSettings? = try? model.manage(ChatDetailSettings.path)
        let pinRevisionAtStart = pinRevision
        let projectSavesAtStart = projectSaveRevision
        let readsAtStart = readRevisions
        do {
            let response: ProjectsResponse = try await model.manage("/api/v1/projects")
            guard isCurrent(scope) else { return }
            setIfChanged(\.supportsProjects, true)
            let previous = Dictionary(projects.map { ($0.id, $0.rootsRevision) }, uniquingKeysWith: { _, last in last })
            let removedProjects = Set(projects.map(\.id)).subtracting(response.projects.map(\.id))
            if let host = model.connection?.credential.hostInstallationId, (response.partial ?? []).isEmpty {
                sidebarOrder.removeProjects(host: host, ids: removedProjects)
            }
            if projectSaveRevision == projectSavesAtStart { setIfChanged(\.projects, response.projects) }
            setIfChanged(\.families, response.families)
            setIfChanged(\.partial, response.partial ?? [])
            setIfChanged(\.supportsTerminal, (response.terminalVersion ?? 0) >= 1)
            setIfChanged(\.supportsModes, (response.modesVersion ?? 0) >= 1)
            setIfChanged(\.supportsDelete, (response.deleteVersion ?? 0) >= 1)
            setIfChanged(\.supportsProjectDelete, (response.projectDeleteVersion ?? 0) >= 1)
            setIfChanged(\.supportsArchive, (response.archiveVersion ?? 0) >= 1)
            setIfChanged(\.supportsArchiveList, (response.archiveVersion ?? 0) >= 2)
            setIfChanged(\.supportsArchiveBatch, (response.archiveVersion ?? 0) >= 3)
            // A pin changed while this request was in flight; the next refresh reports it.
            if pinRevision == pinRevisionAtStart, pendingPins.isEmpty, updatingDetails.isEmpty {
                if let host = model.connection?.credential.hostInstallationId, let pins = response.pinned, (response.partial ?? []).isEmpty {
                    sidebarOrder.reconcilePinned(host: host, threads: pins.map(\.thread))
                }
                archivedReferences.subtract((response.pinned ?? []).map(\.thread.reference))
                setIfChanged(\.pinned, (response.pinned ?? []).map {
                    PinnedProjectThread(projectId: $0.projectId, thread: reconcilingRead($0.thread, startedAt: readsAtStart))
                })
            }
            setIfChanged(\.failure, nil)
            for project in projects where threads[project.id] != nil {
                if previous[project.id] != project.rootsRevision {
                    threadTasks[project.id]?.cancel(); threadTasks[project.id] = nil; pendingMore.remove(project.id)
                    createdDuringThreadRead[project.id] = nil
                    threads[project.id] = threads[project.id]?.restartingPaging()
                }
                loadThreads(project.id)
            }
            if let read = await chatDetailRead, isCurrent(scope), chatDetailRevision == chatDetailRevisionAtStart,
               chatDetail != read {
                chatDetail = read
            }
            saveCache(refreshed: true)
        } catch is CancellationError {
        } catch {
            guard isCurrent(scope) else { return }
            // Cached projects stay readable; the sidebar notice under this
            // computer explains the refresh and offers Retry.
            failure = model.macConnected == false ? nil : projects.isEmpty
                ? "Projects couldn’t be loaded."
                : "Projects couldn’t be updated. Showing saved ones."
        }
    }

    /// First page on expansion, then explicit Show more. Identical requests
    /// share one task; a replaced connection cancels them.
    func loadThreads(_ projectID: String, more: Bool = false) {
        guard let model, let scope = fenced(), !model.previewMode else { return }
        guard let rootsRevision = projects.first(where: { $0.id == projectID })?.rootsRevision else { return }
        let key = projectID
        continuations[key]?.cancel(); continuations[key] = nil
        guard threadTasks[key] == nil else {
            // Show more tapped during a refresh reads its page right after it.
            if more, threads[projectID]?.nextCursor != nil { pendingMore.insert(key); threads[projectID]?.isLoadingMore = true }
            return
        }
        var state = threads[projectID] ?? ProjectThreadsState()
        if more && state.nextCursor == nil { state.isLoadingMore = false; threads[projectID] = state; return }
        // A refresh re-reads the rows already shown, so Show more pages stay.
        let limit = more ? 10 : state.hasLoaded || !state.threads.isEmpty ? state.refreshLimit : SidebarProjection.initialThreads
        state.isLoading = true
        state.isLoadingMore = more
        threads[projectID] = state
        let cursor = more ? state.nextCursor : nil
        let pinRevisionAtStart = pinRevision
        let readsAtStart = readRevisions
        threadTasks[key] = Task { [weak self] in
            var continueAfter: Int?
            defer {
                if let self, self.scope == scope, self.projects.first(where: { $0.id == projectID })?.rootsRevision == rootsRevision {
                    self.threadTasks[key] = nil
                    self.createdDuringThreadRead[projectID] = nil
                    if self.threads[projectID]?.isLoadingMore == true { self.threads[projectID]?.isLoadingMore = false }
                    if self.pendingMore.remove(key) != nil { self.loadThreads(projectID, more: true) }
                    else if let continueAfter { self.continueReading(projectID, attempt: continueAfter, scope: scope) }
                }
            }
            var path = "/api/v1/projects/\(ConnectionModel.escape(projectID))/threads?limit=\(limit)"
            if let cursor { path += "&cursor=" + ConnectionModel.escape(cursor) }
            do {
                let page: ProjectThreadsPage = try await model.manage(path)
                guard let self, self.isCurrent(scope) && self.projects.first(where: { $0.id == projectID })?.rootsRevision == rootsRevision, !Task.isCancelled else { return }
                var next = self.threads[projectID] ?? ProjectThreadsState()
                let before = next.threads
                next.apply(self.reconcilingReads(self.reconcilingPins(page, startedAt: pinRevisionAtStart, continuing: more), startedAt: readsAtStart), replacing: !more)
                if !more {
                    self.preserveCreatedDuringRead(in: &next, previous: before, projectID: projectID)
                    continueAfter = self.noteStillReading(projectID, in: &next)
                }
                next.isLoading = false
                if let host = model.connection?.credential.hostInstallationId {
                    self.sidebarOrder.reconcileThreads(host: host, project: projectID, state: next)
                }
                self.setIfChanged(\.threads[projectID], next)
                if !more { self.saveCache(refreshed: true) }
            } catch PairingFailure.response(410) {
                // The catalog changed between pages: reload from the top.
                guard let self, self.isCurrent(scope) && self.projects.first(where: { $0.id == projectID })?.rootsRevision == rootsRevision else { return }
                self.threads[projectID]?.isLoading = false
                self.threads[projectID]?.nextCursor = nil
                // Retry inside this task so its cleanup cannot erase a newer
                // request's handle and allow overlapping pagination.
                do {
                    let revision = self.pinRevision
                    let reads = self.readRevisions
                    let page: ProjectThreadsPage = try await model.manage("/api/v1/projects/\(ConnectionModel.escape(projectID))/threads?limit=\(SidebarProjection.initialThreads)")
                    guard self.isCurrent(scope) && self.projects.first(where: { $0.id == projectID })?.rootsRevision == rootsRevision else { return }
                    var next = self.threads[projectID] ?? ProjectThreadsState()
                    let before = next.threads
                    next.apply(self.reconcilingReads(self.reconcilingPins(page, startedAt: revision), startedAt: reads), replacing: true); next.isLoading = false
                    self.preserveCreatedDuringRead(in: &next, previous: before, projectID: projectID)
                    continueAfter = self.noteStillReading(projectID, in: &next)
                    self.setIfChanged(\.threads[projectID], next)
                    self.saveCache(refreshed: true)
                } catch {
                    guard self.isCurrent(scope) && self.projects.first(where: { $0.id == projectID })?.rootsRevision == rootsRevision else { return }
                    self.threads[projectID]?.failure = "Threads could not be loaded."
                }
            } catch {
                guard let self, self.isCurrent(scope) && self.projects.first(where: { $0.id == projectID })?.rootsRevision == rootsRevision, !(error is CancellationError) else { return }
                self.threads[projectID]?.isLoading = false
                // An offline computer shows its red dot alone, not a failure row.
                self.threads[projectID]?.failure = model.macConnected == false ? nil : "Threads could not be loaded."
            }
        }
    }

    /// A first page the Mac sent while still reading a provider is read
    /// again by itself, a little later each time, so the list fills in
    /// without Show more. After a few, the list says why chats may be
    /// missing and offers Retry; the regular refresh keeps reading.
    /// Returns the attempt to continue with, if any.
    private func noteStillReading(_ projectID: String, in state: inout ProjectThreadsState) -> Int? {
        guard state.isStillReading else { stillReadingAnswers[projectID] = nil; return nil }
        let attempt = stillReadingAnswers[projectID, default: 0] + 1
        stillReadingAnswers[projectID] = attempt
        guard attempt < Self.stillReadingRetries else {
            state.failure = "\(model?.macName ?? "Your computer") is slow to respond, so some chats may be missing."
            return nil
        }
        return attempt
    }

    private func continueReading(_ projectID: String, attempt: Int, scope: String) {
        continuations[projectID]?.cancel()
        let delay = stillReadingDelay * attempt
        continuations[projectID] = Task { [weak self] in
            do { try await Task.sleep(for: delay) } catch { return }
            guard let self, self.isCurrent(scope) else { return }
            self.continuations[projectID] = nil
            self.loadThreads(projectID)
        }
    }

    private func preserveCreatedDuringRead(in state: inout ProjectThreadsState, previous: [ProjectThreadSummary], projectID: String) {
        guard let created = createdDuringThreadRead[projectID] else { return }
        // The page may contain an earlier title, pin or archive state for a row
        // created during this request. The current local row wins; if it was
        // archived, its absence removes a stale server copy too.
        state.threads.removeAll { row in
            guard let id = row.conversationId else { return false }
            return created.contains(id)
        }
        for row in previous.reversed() {
            guard let id = row.conversationId, created.contains(id) else { continue }
            state.threads.insert(row, at: 0)
        }
    }

    func project(_ id: String) -> ProjectSummary? { projects.first { $0.id == id } }

    func upsert(_ project: ProjectSummary) {
        projectSaveRevision += 1
        if let index = projects.firstIndex(where: { $0.id == project.id }) { projects[index] = project }
        else { projects.insert(project, at: 0) }
        saveCache()
    }

    func createProject(requestID: String, name: String, folders: [String], primaryIndex: Int) async throws -> ProjectSummary {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let body = try JSONSerialization.data(withJSONObject: ["requestId": requestID, "name": name, "folders": folders, "primaryIndex": primaryIndex])
        let created: ProjectSummary = try await model.manage("/api/v1/projects", method: "POST", body: body)
        guard isCurrent(scope) else { throw CancellationError() }
        upsert(created)
        return created
    }

    /// Creates an empty folder named `name` in `parent` (the Mac's home folder
    /// when nil) and a project for it. A retry with the same request reuses it.
    func createProject(requestID: String, name: String, newFolderParent parent: String?) async throws -> ProjectSummary {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let body = try JSONSerialization.data(withJSONObject: ["requestId": requestID, "name": name,
                                                               "newFolder": parent.map { ["parent": $0] } ?? [:]])
        let created: ProjectSummary = try await model.manage("/api/v1/projects", method: "POST", body: body)
        guard isCurrent(scope) else { throw CancellationError() }
        upsert(created)
        return created
    }

    func update(_ projectID: String, fields: [String: Any]) async throws -> ProjectSummary {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let updated: ProjectSummary = try await model.manage("/api/v1/projects/\(ConnectionModel.escape(projectID))",
            method: "PATCH", body: JSONSerialization.data(withJSONObject: fields))
        guard isCurrent(scope) else { throw CancellationError() }
        upsert(updated)
        return updated
    }

    /// `conversationIds`: every Cupola chat the computer deleted with the Project, so chats this
    /// device cached in earlier sessions (outside the loaded pages) are forgotten too.
    private struct ProjectDeleteResponse: Decodable { let projectId: String; let deleted: Bool; let conversationIds: [String]? }

    /// Deletes the Project and its Cupola chats on the computer, then forgets them here. The
    /// computer's folders and the agents' own history are untouched. A refused delete (a chat
    /// still working) leaves everything as it was.
    func delete(project: ProjectSummary) async throws {
        guard supportsProjectDelete, let model, let scope = fenced() else { throw PairingFailure.response(422) }
        let response: ProjectDeleteResponse = try await model.manage(
            "/api/v1/projects/\(ConnectionModel.escape(project.id))", method: "DELETE")
        guard isCurrent(scope) else { throw CancellationError() }
        guard response.deleted, response.projectId == project.id else { throw ReadFailure.wrongConversation }
        let chats = Set((threads[project.id]?.threads.compactMap(\.conversationId) ?? [])
            + pinned.filter { $0.projectId == project.id }.compactMap(\.thread.conversationId)
            + details.values.filter { $0.projectId == project.id }.map(\.conversationId)
            + (response.conversationIds ?? []))
        let references = Set((threads[project.id]?.threads.map(\.reference) ?? [])
            + pinned.filter { $0.projectId == project.id }.map(\.thread.reference))
        projectSaveRevision += 1
        threadTasks[project.id]?.cancel(); threadTasks[project.id] = nil; pendingMore.remove(project.id)
        projects.removeAll { $0.id == project.id }
        threads[project.id] = nil
        pinned.removeAll { $0.projectId == project.id }
        for chat in chats {
            removeArchivedRows(chat)
            details.removeValue(forKey: chat)
            manuallyUnread.remove(chat)
            historyNotFound(chat)
            // The computer already deleted it; a local cleanup error only closes the chat.
            do { try model.removeDeletedConversation(chat) } catch { model.leaveDeletedConversation(chat) }
        }
        #if WONDER_DIAGNOSTICS
        DiagnosticSubagentFixture.projectDeleteReceipt = [
            "projectId": project.id, "purged": chats.sorted(),
            "stillCached": chats.intersection(model.cachedConversationIds).sorted(),
            "stillListed": chats.intersection(Set(model.chats.map(\.id))).sorted()]
        #endif
        if let host = model.connection?.credential.hostInstallationId {
            sidebarOrder.removeProjects(host: host, ids: [project.id])
            sidebarOrder.removeThreads(host: host, references: references)
        }
        saveCache()
    }

    func loadOptions() async {
        guard let model, let scope = fenced(), options == nil, !model.previewMode else { return }
        // A default or hidden model chosen while this computer was out of reach lands now.
        await LocalModelSettings.flush(model, refreshOptions: false)
        guard isCurrent(scope) else { return }
        let fetched: AgentOptions? = try? await model.manage("/api/v1/agent-options")
        guard isCurrent(scope) else { return }
        options = fetched
    }

    /// Reads the model list again after a default or hidden model changed.
    func refreshOptions() async {
        guard let model, let scope = fenced(), !model.previewMode else { return }
        guard let fetched: AgentOptions = try? await model.manage("/api/v1/agent-options"), isCurrent(scope) else { return }
        options = fetched
    }

    func candidates() async throws -> ProjectCandidatesResponse {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let response: ProjectCandidatesResponse = try await model.manage("/api/v1/projects/candidates")
        guard isCurrent(scope) else { throw CancellationError() }
        return response
    }

    /// Opening a catalog thread attaches it; this starts no model work.
    func attach(_ projectID: String, thread: ProjectThreadSummary) async throws -> String {
        // Reattach native sessions of providers that sync desktop projects (Codex) so
        // a missing desktop project assignment is repaired on open. Drafts have no
        // native metadata to synchronize.
        if let id = thread.conversationId, !capabilities(thread.family).desktopProjectSync || thread.reference.hasPrefix("wonder:") {
            return id
        }
        return try await attach(projectID, reference: thread.reference, expected: thread.conversationId)
    }

    /// Attaches a native thread by its catalog reference (a list row or a
    /// search result) and returns its conversation; this starts no model work.
    func attach(_ projectID: String, reference: String, expected: String? = nil) async throws -> String {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let attached: ProjectThreadSummary = try await model.manage(
            "/api/v1/projects/\(ConnectionModel.escape(projectID))/threads/attach", method: "POST",
            body: JSONSerialization.data(withJSONObject: ["reference": reference]))
        guard isCurrent(scope), let id = attached.conversationId else { throw CancellationError() }
        if let expected, expected != id { throw ReadFailure.wrongConversation }
        if var state = threads[projectID], let index = state.threads.firstIndex(where: { $0.reference == reference }) {
            // Reattachment can still report the pin from before the local save.
            // Keep the optimistic move until PATCH confirms it or rolls it back.
            state.threads[index] = pendingPins[reference].map { attached.settingPinned($0) } ?? attached
            threads[projectID] = state
        }
        return id
    }

    @discardableResult func loadDetail(_ conversationID: String, afterCurrentRead: Bool = false) async throws -> ProjectConversationDetail {
        guard let model else { throw PairingFailure.response(401) }
        var result: ProjectConversationDetail?
        try await model.refreshCoordinator.read(.detail(conversationID), afterCurrent: afterCurrentRead) { [self] in
            result = try await performDetailRead(conversationID)
        }
        guard let detail = result ?? details[conversationID] else { throw ReadFailure.wrongConversation }
        return detail
    }

    private func performDetailRead(_ conversationID: String) async throws -> ProjectConversationDetail {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let revision = detailRevisions[conversationID, default: 0]
        let detail: ProjectConversationDetail = try await model.manage("/api/v1/project-conversations/\(ConnectionModel.escape(conversationID))")
        guard isCurrent(scope) else { throw CancellationError() }
        guard detail.conversationId == conversationID else { throw ReadFailure.wrongConversation }
        if revision != detailRevisions[conversationID, default: 0] || updatingDetails.contains(conversationID) {
            guard let current = details[conversationID] else { throw CancellationError() }
            return current
        }
        // Every refresh reloads the open thread's detail; most are unchanged.
        setIfChanged(\.details[conversationID], detail)
        if detail.isArchived == true { removeArchivedRows(conversationID) }
        unavailableDetails.remove(conversationID)
        if !unavailableHistory.contains(conversationID), unavailable.contains(conversationID) { unavailable.remove(conversationID) }
        trimDetails(keeping: conversationID)
        saveCache()
        return detail
    }

    /// Refreshes a conversation that is already on screen from its saved copy.
    /// Only a 404 replaces it; any other failure keeps the saved view.
    func refreshDetail(_ conversationID: String, afterCurrentRead: Bool = false) async {
        guard let scope = fenced() else { return }
        do { try await loadDetail(conversationID, afterCurrentRead: afterCurrentRead) }
        catch PairingFailure.response(404) {
            guard isCurrent(scope) else { return }
            unavailableDetails.insert(conversationID)
            unavailable.insert(conversationID)
            widgetSnapshotChanged?()
        } catch {}
    }

    func updateConversation(_ conversationID: String, fields: [String: Any]) async throws {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        guard updatingDetails.insert(conversationID).inserted else { throw PairingFailure.response(409) }
        let changesPin = fields["isPinned"] != nil
        let changesRead = fields["hasUnread"] != nil
        let changesArchive = fields["isArchived"] != nil
        detailRevisions[conversationID, default: 0] += 1
        if changesPin || changesArchive { pinRevision += 1 }
        if changesRead { readRevisions[conversationID, default: 0] += 1 }
        defer {
            if self.scope == scope, model.assignmentScope == scope {
                updatingDetails.remove(conversationID)
                detailRevisions[conversationID, default: 0] += 1
                if changesPin || changesArchive { pinRevision += 1 }
                if changesRead { readRevisions[conversationID, default: 0] += 1 }
            }
        }
        let detail: ProjectConversationDetail = try await model.manage(
            "/api/v1/project-conversations/\(ConnectionModel.escape(conversationID))", method: "PATCH",
            body: JSONSerialization.data(withJSONObject: fields))
        guard isCurrent(scope) else { throw CancellationError() }
        if let unread = fields["hasUnread"] as? Bool {
            if unread { manuallyUnread.insert(conversationID) }
            else { manuallyUnread.remove(conversationID) }
        }
        details[conversationID] = detail
        if detail.isArchived == true {
            removeArchivedRows(conversationID)
            saveCache()
            return
        }
        for (project, var state) in threads {
            guard let index = state.threads.firstIndex(where: { $0.conversationId == conversationID }) else { continue }
            let old = state.threads[index]
            state.threads[index] = old.updatingDetails(title: detail.title, isPinned: detail.isPinned, hasUnread: detail.hasUnread)
            threads[project] = state
        }
        if detail.isPinned, !pinned.contains(where: { $0.thread.conversationId == conversationID }),
           let row = threads[detail.projectId]?.threads.first(where: { $0.conversationId == conversationID }) {
            pinned.insert(PinnedProjectThread(projectId: detail.projectId, thread: row), at: 0)
        }
        for index in pinned.indices.reversed() where pinned[index].thread.conversationId == conversationID {
            if detail.isPinned {
                let old = pinned[index].thread
                pinned[index] = PinnedProjectThread(projectId: pinned[index].projectId,
                    thread: old.updatingDetails(title: detail.title, isPinned: true, hasUnread: detail.hasUnread))
            } else { pinned.remove(at: index) }
        }
        saveCache()
    }

    /// Reads started before a local pin save must not put the old pin back.
    /// Reads started during the optimistic sidebar change keep that state too.
    private func reconcilingPins(_ page: ProjectThreadsPage, startedAt revision: Int, continuing: Bool = false) -> ProjectThreadsPage {
        guard revision != pinRevision || !pendingPins.isEmpty || !updatingDetails.isEmpty else {
            // A continuation may have buffered rows before the archive even
            // when this request started later. Only a fresh list can restore
            // those identities; explicit Restore clears its exclusion itself.
            if continuing {
                return page.replacingThreads(page.threads.filter { !archivedReferences.contains($0.reference) })
            }
            archivedReferences.subtract(page.threads.map(\.reference))
            return page
        }
        let rows = page.threads.filter { !archivedReferences.contains($0.reference) }.map { row in
            if let value = pendingPins[row.reference] { return row.settingPinned(value) }
            if let id = row.conversationId, let detail = details[id] { return row.settingPinned(detail.isPinned) }
            if let current = threads.values.lazy.flatMap({ $0.threads }).first(where: { $0.reference == row.reference }) {
                return row.settingPinned(current.isPinned)
            }
            return row
        }
        return page.replacingThreads(rows)
    }

    // MARK: Opening, pinning and renaming threads

    /// Remembers that a thread was opened. The most recent ones keep their saved
    /// history and detail on this phone.
    func noteOpened(_ conversationID: String) {
        if manuallyUnread.contains(conversationID) { manuallyUnread.remove(conversationID) }
        guard recentlyOpened.ids.first != conversationID else { return }
        recentlyOpened.note(conversationID)
        if model?.previewMode == false, let recentKey, let data = try? JSONEncoder().encode(recentlyOpened) {
            UserDefaults.standard.set(data, forKey: recentKey)
        }
        widgetSnapshotChanged?()
    }

    /// Pin ownership is independent of recent-list and resident-cache limits.
    var pinnedConversationIDs: Set<String> {
        var ids = Set(pinned.compactMap(\.thread.conversationId))
        ids.formUnion(details.values.lazy.filter(\.isPinned).map(\.conversationId))
        for state in threads.values { ids.formUnion(state.threads.lazy.filter(\.isPinned).compactMap(\.conversationId)) }
        return ids
    }

    /// Detail records kept handy for pinned and recently opened threads.
    /// Body files have their own independent lifetime in the history cache.
    var retainedConversationIDs: Set<String> {
        var ids = Set(recentlyOpened.ids)
        ids.formUnion(pinnedConversationIDs)
        return ids
    }

    /// Pinning shows in the Pinned list at once; a failure puts everything back.
    func setPinned(_ projectID: String, thread: ProjectThreadSummary, _ value: Bool) async throws {
        guard let scope = fenced() else { throw PairingFailure.response(401) }
        let reference = thread.reference
        guard pendingPins[reference] == nil else { throw PairingFailure.response(409) }
        pendingPins[reference] = value
        defer { if self.scope == scope, model?.assignmentScope == scope { pendingPins.removeValue(forKey: reference) } }
        let previousEntry = pinned.first { $0.thread.reference == reference }
        let previousIndex = pinned.firstIndex { $0.thread.reference == reference }
        let previousThread = threads[projectID]?.threads.first { $0.reference == reference }
        pinRevision += 1
        pinned.removeAll { $0.thread.reference == reference }
        if value { pinned.insert(PinnedProjectThread(projectId: projectID, thread: thread.settingPinned(true)), at: 0) }
        replaceThread(projectID, reference: reference) { $0.settingPinned(value) }
        do {
            // Pinning a provider-only thread first attaches it, without work.
            let conversation = try await attach(projectID, thread: thread)
            try await updateConversation(conversation, fields: ["isPinned": value])
            if value, let index = pinned.firstIndex(where: { $0.thread.reference == reference }),
               let latest = threads[projectID]?.threads.first(where: { $0.reference == reference }) {
                pinned[index] = PinnedProjectThread(projectId: projectID, thread: latest)
            }
        } catch {
            guard self.scope == scope, model?.assignmentScope == scope else { throw error }
            pinRevision += 1
            pinned.removeAll { $0.thread.reference == reference }
            if let previousEntry { pinned.insert(previousEntry, at: min(previousIndex ?? 0, pinned.count)) }
            if let previousThread { replaceThread(projectID, reference: reference) { _ in previousThread } }
            saveCache()
            throw error
        }
        pendingPins.removeValue(forKey: reference)
        saveCache()
        await refresh()
    }

    func rename(_ projectID: String, thread: ProjectThreadSummary, to title: String) async throws {
        let conversation = try await attach(projectID, thread: thread)
        try await updateConversation(conversation, fields: ["title": title])
    }

    /// Whether a thread can be archived: in the provider (once it has a
    /// provider thread) or in Wonder, as the provider's record says.
    func canArchive(_ thread: ProjectThreadSummary) -> Bool {
        switch capabilities(thread.family).archive {
        case .provider: supportsArchive && thread.nativeID != nil
        case .wonder: supportsArchiveList
        }
    }

    /// The same rule for an open conversation: a provider archives it once it
    /// has a provider session.
    func canArchive(_ detail: ProjectConversationDetail) -> Bool {
        switch capabilities(detail.family).archive {
        case .provider: supportsArchive && detail.hasNativeSession
        case .wonder: supportsArchiveList
        }
    }

    /// Whether Chats lists this conversation as working. The open chat's
    /// Archive checks it too, so a thread Chats shows as running can't be
    /// archived from its header while the chat's own activity is still loading.
    /// The Chats rows that show work running, which the working watchdog keeps
    /// re-reading until the Mac's list says they ended. A native session not yet
    /// attached has no conversation, so it can only be found again by a catalog read.
    var workingRows: (conversations: Set<String>, unattached: Bool) {
        var ids = Set<String>(), unattached = false
        func note(_ thread: ProjectThreadSummary) {
            guard thread.isWorking else { return }
            if let id = thread.conversationId { ids.insert(id) } else { unattached = true }
        }
        for entry in pinned { note(entry.thread) }
        for state in threads.values { state.threads.forEach(note) }
        return (ids, unattached)
    }
    func listsWorking(_ conversationID: String) -> Bool {
        pinned.contains { $0.thread.conversationId == conversationID && $0.thread.isWorking }
            || threads.values.contains { $0.threads.contains { $0.conversationId == conversationID && $0.isWorking } }
    }

    /// Where archiving or restoring this kind of thread also takes effect, for
    /// the confirmation and Undo; nil when it changes Cupola only.
    func archiveAlsoIn(_ family: AgentFamily) -> String? {
        capabilities(family).archive == .provider ? family.title : nil
    }

    /// Returns the archived conversation, which Undo restores.
    @discardableResult func archive(_ projectID: String, thread: ProjectThreadSummary) async throws -> String {
        guard canArchive(thread) else { throw PairingFailure.response(422) }
        let conversation = try await attach(projectID, thread: thread)
        try await archive(conversation: conversation)
        return conversation
    }

    /// Archives an attached conversation, such as the one open on screen.
    func archive(conversation: String) async throws {
        try await updateConversation(conversation, fields: ["isArchived": true])
        guard details[conversation]?.isArchived == true else { throw PairingFailure.response(503) }
    }

    private struct DeleteResponse: Decodable { let conversationId: String; let deleted: Bool }

    /// The host commits before removing the cached chat. Draft preservation is
    /// shared with a history 404; a dropped response can safely retry DELETE.
    func delete(conversation: String) async throws {
        guard supportsDelete, let model, let scope = fenced() else { throw PairingFailure.response(422) }
        guard updatingDetails.insert(conversation).inserted else { throw PairingFailure.response(409) }
        pinRevision += 1; detailRevisions[conversation, default: 0] += 1
        defer {
            if isCurrent(scope) {
                updatingDetails.remove(conversation)
                pinRevision += 1; detailRevisions[conversation, default: 0] += 1
            }
        }
        let response: DeleteResponse = try await model.manage(
            "/api/v1/project-conversations/\(ConnectionModel.escape(conversation))", method: "DELETE")
        guard isCurrent(scope) else { throw CancellationError() }
        guard response.deleted, response.conversationId == conversation else { throw ReadFailure.wrongConversation }
        removeArchivedRows(conversation)
        details.removeValue(forKey: conversation)
        manuallyUnread.remove(conversation)
        historyNotFound(conversation)
        saveCache()
        // The delete already happened on the computer; a local cleanup error
        // must not report it as failed. The chat just closes.
        do {
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-delete-cleanup-fails") { throw CocoaError(.fileWriteUnknown) }
            #endif
            try model.removeDeletedConversation(conversation)
        }
        catch { model.leaveDeletedConversation(conversation) }
    }

    struct ArchiveOutcome: Decodable, Sendable {
        let reference: String
        let conversationId: String?
        let error: String?
    }
    private struct ArchiveResponse: Decodable, Sendable { let results: [ArchiveOutcome] }
    static let archiveBatchLimit = 50

    /// At most 50 writes, in list order. Older hosts use the same ordered
    /// single-item operations; neither path reloads the list after archiving.
    func archive(_ projectID: String, threads selected: [ProjectThreadSummary]) async throws -> [ArchiveOutcome] {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        guard !selected.isEmpty, selected.count <= Self.archiveBatchLimit,
              Set(selected.map(\.reference)).count == selected.count else { throw PairingFailure.response(422) }
        if !supportsArchiveBatch {
            var results: [ArchiveOutcome] = []
            for thread in selected {
                guard isCurrent(scope) else { throw CancellationError() }
                do {
                    try await archive(projectID, thread: thread)
                    results.append(ArchiveOutcome(reference: thread.reference, conversationId: thread.conversationId, error: nil))
                } catch is CancellationError { throw CancellationError() }
                catch { results.append(ArchiveOutcome(reference: thread.reference, conversationId: nil, error: managementError(error))) }
            }
            return results
        }
        // Fence catalog and detail requests already in flight. A successful
        // response removes every placement, retaining the remaining page cursor.
        let ids = selected.compactMap(\.conversationId)
        guard ids.allSatisfy({ !updatingDetails.contains($0) }) else { throw PairingFailure.response(409) }
        pinRevision += 1
        for id in ids { detailRevisions[id, default: 0] += 1; updatingDetails.insert(id) }
        defer {
            if self.scope == scope, model.assignmentScope == scope {
                pinRevision += 1
                for id in ids { detailRevisions[id, default: 0] += 1; updatingDetails.remove(id) }
            }
        }
        let items: [[String: Any]] = selected.map { thread in
            var item: [String: Any] = ["reference": thread.reference]
            if let id = thread.conversationId { item["conversationId"] = id }
            return item
        }
        let response: ArchiveResponse = try await model.manage(
            "/api/v1/projects/\(ConnectionModel.escape(projectID))/threads/archive", method: "POST",
            body: JSONSerialization.data(withJSONObject: ["threads": items]))
        guard isCurrent(scope) else { throw CancellationError() }
        guard response.results.count == selected.count else { throw ReadFailure.wrongConversation }
        for (thread, result) in zip(selected, response.results) {
            guard thread.reference == result.reference,
                  result.error != nil || (result.conversationId != nil
                    && (thread.conversationId == nil || thread.conversationId == result.conversationId)) else {
                throw ReadFailure.wrongConversation
            }
        }
        let successes = response.results.filter { $0.error == nil }
        let archivedIDs = Set(successes.compactMap(\.conversationId))
        removeArchivedRows(conversationIDs: archivedIDs, references: Set(successes.map(\.reference)))
        // A later read must confirm archived detail rather than keep a
        // previously cached, sendable conversation after a lost selection.
        if !archivedIDs.isEmpty { details = details.filter { !archivedIDs.contains($0.key) } }
        saveCache()
        widgetSnapshotChanged?()
        return response.results
    }

    /// A Project's archived threads, newest first.
    func archivedThreads(_ projectID: String) async throws -> ArchivedProjectThreadsPage {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let page: ArchivedProjectThreadsPage = try await model.manage(
            "/api/v1/projects/\(ConnectionModel.escape(projectID))/threads/archived")
        guard isCurrent(scope) else { throw CancellationError() }
        return page
    }

    /// Restores an archived thread: in Codex for a Codex thread, in Wonder for
    /// Claude Code. It returns to the sidebar on the next read.
    func unarchive(_ projectID: String, thread: ProjectThreadSummary) async throws {
        let conversation = try await attach(projectID, thread: thread)
        try await unarchive(conversation: conversation, reference: thread.reference)
    }

    /// Restores an attached conversation (Undo, or Restore on an archived
    /// chat's screen). The detail the computer returns is saved before this
    /// returns, so a screen showing "Chat archived" can reopen the chat.
    func unarchive(conversation: String, reference: String? = nil) async throws {
        try await updateConversation(conversation, fields: ["isArchived": false])
        if let reference { archivedReferences.remove(reference) }
        await refresh()
        if let projectID = details[conversation]?.projectId, threads[projectID] != nil { loadThreads(projectID) }
    }

    private func removeArchivedRows(_ conversationID: String) {
        removeArchivedRows(conversationIDs: [conversationID], references: [])
    }

    private func removeArchivedRows(conversationIDs: Set<String>, references: Set<String>) {
        func matches(_ row: ProjectThreadSummary) -> Bool {
            references.contains(row.reference) || row.conversationId.map { conversationIDs.contains($0) } == true
        }
        archivedReferences.formUnion(references)
        for (project, var state) in threads {
            let oldCount = state.threads.count
            for row in state.threads where matches(row) { archivedReferences.insert(row.reference) }
            state.threads.removeAll(where: matches)
            if state.threads.count != oldCount { threads[project] = state }
        }
        for row in pinned where matches(row.thread) { archivedReferences.insert(row.thread.reference) }
        if pinned.contains(where: { matches($0.thread) }) { pinned.removeAll { matches($0.thread) } }
        if let host = model?.connection?.credential.hostInstallationId {
            sidebarOrder.removeThreads(host: host, references: archivedReferences)
        }
    }

    func hasUnread(_ conversationID: String) -> Bool {
        details[conversationID]?.hasUnread == true
            || pinned.contains { $0.thread.conversationId == conversationID && $0.thread.hasUnread }
            || threads.values.contains { $0.threads.contains { $0.conversationId == conversationID && $0.hasUnread } }
    }

    private func noteMacPermissionWaits() {
        var waiting = Set<String>()
        for thread in pinned.map(\.thread) + threads.values.flatMap(\.threads) where thread.waitsOnMacPermission {
            if let id = thread.conversationId { waiting.insert(id) }
        }
        guard waiting != macPermissionWaits else { return }
        let ended = macPermissionWaits.subtracting(waiting)
        macPermissionWaits = waiting
        if !ended.isEmpty, let host = model?.connection?.credential.hostInstallationId {
            MacPermissionNotifications.clear(host: host, conversations: ended)
        }
    }

    /// A macOS dialog is open on the Mac while this conversation's turn runs.
    func waitsOnMacPermission(_ conversationID: String) -> Bool {
        pinned.contains { $0.thread.conversationId == conversationID && $0.thread.waitsOnMacPermission }
            || threads.values.contains { $0.threads.contains { $0.conversationId == conversationID && $0.waitsOnMacPermission } }
    }

    func readRevision(_ conversationID: String) -> Int { readRevisions[conversationID, default: 0] }

    private func reconcilingRead(_ row: ProjectThreadSummary, startedAt revisions: [String: Int]) -> ProjectThreadSummary {
        guard let id = row.conversationId, readRevision(id) != revisions[id, default: 0] else { return row }
        return row.settingUnread(hasUnread(id))
    }

    private func reconcilingReads(_ page: ProjectThreadsPage, startedAt revisions: [String: Int]) -> ProjectThreadsPage {
        page.replacingThreads(page.threads.map { reconcilingRead($0, startedAt: revisions) })
    }

    /// Applies a read acknowledgement the Mac has confirmed.
    func markRead(_ conversationID: String) {
        readRevisions[conversationID, default: 0] += 1
        detailRevisions[conversationID, default: 0] += 1
        for (project, var state) in threads where state.threads.contains(where: { $0.conversationId == conversationID && $0.hasUnread }) {
            state.threads = state.threads.map { $0.conversationId == conversationID ? $0.settingUnread(false) : $0 }
            threads[project] = state
        }
        if pinned.contains(where: { $0.thread.conversationId == conversationID && $0.thread.hasUnread }) {
            pinned = pinned.map { $0.thread.conversationId == conversationID ? PinnedProjectThread(projectId: $0.projectId, thread: $0.thread.settingUnread(false)) : $0 }
        }
        if let detail = details[conversationID], detail.hasUnread,
           var fields = (try? JSONEncoder().encode(detail)).flatMap({ try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }) {
            fields["hasUnread"] = false
            if let data = try? JSONSerialization.data(withJSONObject: fields),
               let read = try? JSONDecoder().decode(ProjectConversationDetail.self, from: data) { details[conversationID] = read }
        }
        saveCache()
    }

    private func replaceThread(_ projectID: String, reference: String, _ transform: (ProjectThreadSummary) -> ProjectThreadSummary) {
        guard var state = threads[projectID], let index = state.threads.firstIndex(where: { $0.reference == reference }) else { return }
        state.threads[index] = transform(state.threads[index])
        threads[projectID] = state
    }

    /// Drops details nothing can show or reopen quickly once the cache grows.
    private func trimDetails(keeping loaded: String) {
        guard details.count > Self.detailLimit else { return }
        let keep = detailIDsToPersist().union([loaded])
        let trimmed = details.filter { keep.contains($0.key) }
        if trimmed.count != details.count { details = trimmed }
    }

    private func detailIDsToPersist() -> Set<String> {
        var ids = retainedConversationIDs
        for state in threads.values {
            ids.formUnion(state.threads.prefix(SidebarProjection.initialThreads + 10).compactMap(\.conversationId))
        }
        return ids
    }

    /// The first message creates the native thread; the request ID makes a
    /// lost response retry the same creation instead of starting another.
    func createThread(projectID: String, draft: NewChatDraft, body: String, deviceID: String) async throws -> CreateProjectThreadResponse {
        guard let model, let scope = fenced(), let family = draft.family, let modelID = draft.model else { throw PairingFailure.response(422) }
        var fields: [String: Any] = ["deviceId": deviceID, "clientMessageId": draft.requestID, "family": family.rawValue,
                                     "model": modelID, "body": body, "prepareOnly": true]
        let provider = capabilities(family)
        if provider.accessChoices { fields["accessMode"] = draft.accessMode.rawValue }
        if let effort = draft.effort { fields["effort"] = effort }
        if let serviceTier = draft.serviceTier { fields["serviceTier"] = serviceTier }
        if supportsModes {
            if provider.allowsApprovalChoice { fields["claudeApproval"] = (draft.claudeApproval ?? .standard).rawValue }
            if draft.planMode == true { fields["planMode"] = true }
        }
        if let folder = draft.folderId { fields["folderId"] = folder }
        if let revision = draft.rootsRevision { fields["rootsRevision"] = revision }
        let response: CreateProjectThreadResponse = try await model.manage(
            "/api/v1/projects/\(ConnectionModel.escape(projectID))/threads", method: "POST",
            body: JSONSerialization.data(withJSONObject: fields))
        guard isCurrent(scope) else { throw CancellationError() }
        var state = threads[projectID] ?? ProjectThreadsState()
        if !state.threads.contains(where: { $0.conversationId == response.conversation.conversationId }) {
            if let host = model.connection?.credential.hostInstallationId {
                sidebarOrder.insertedThread(host: host, thread: response.conversation)
            }
            state.threads.insert(response.conversation, at: 0)
            state.hasLoaded = true
        }
        threads[projectID] = state
        if threadTasks[projectID] != nil, let id = response.conversation.conversationId {
            createdDuringThreadRead[projectID, default: []].insert(id)
        }
        if let index = projects.firstIndex(where: { $0.id == projectID }) {
            projects[index] = projects[index].recording(family, model: modelID, effort: draft.effort, startsThread: true)
        }
        saveCache()
        return response
    }

    /// A model saved on a thread is its Project's last one for that provider,
    /// as the Mac records it; shown here until the next list arrives.
    func recordModelChoice(projectID: String, family: AgentFamily, model: String, effort: String?) {
        guard let index = projects.firstIndex(where: { $0.id == projectID }) else { return }
        projects[index] = projects[index].recording(family, model: model, effort: effort, startsThread: false)
        saveCache()
    }

    /// Copies a started thread, through `lastTurnID` when given, into a new
    /// thread in the same project. The Mac never changes the source. Returns
    /// the new conversation's ID.
    func fork(_ conversationID: String, lastTurnID: String?) async throws -> String {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let created: ProjectThreadSummary = try await model.manage(
            "/api/v1/project-conversations/\(ConnectionModel.escape(conversationID))/fork", method: "POST",
            body: ForkConversationRequest(lastTurnId: lastTurnID).body())
        guard isCurrent(scope), let id = created.conversationId else { throw CancellationError() }
        if let projectID = details[conversationID]?.projectId, var state = threads[projectID],
           !state.threads.contains(where: { $0.conversationId == id }) {
            if let host = model.connection?.credential.hostInstallationId {
                sidebarOrder.insertedThread(host: host, thread: created)
            }
            state.threads.insert(created, at: 0)
            threads[projectID] = state
            if threadTasks[projectID] != nil { createdDuringThreadRead[projectID, default: []].insert(id) }
            saveCache()
        }
        return id
    }

    /// Edit from here: the Mac cuts the provider's history back to before the
    /// owner's message that opened `turnID` (`ConversationRewind`).
    func rewind(_ conversationID: String, turnID: String, restoreFiles: Bool) async throws -> RewindConversationResponse {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let response: RewindConversationResponse = try await model.manage(
            "/api/v1/conversations/\(ConnectionModel.escape(conversationID))/rewind", method: "POST",
            body: RewindConversationRequest(turnId: turnID, restoreFiles: restoreFiles).body())
        guard isCurrent(scope) else { throw CancellationError() }
        return response
    }

    func refreshWorktreeSettings() async {
        guard let model, let scope = fenced() else { return }
        let revision = worktreesRevision, archiveRevision = autoArchiveRevision
        let continuing = continuationRevision
        do {
            let settings: ProjectDiscoverySettings = try await model.manage(ProjectDiscoverySettings.path)
            guard isCurrent(scope) else { return }
            if continuing == continuationRevision, !savingContinuation {
                continuation = settings.continuation; continuationFailure = nil
            }
            if archiveRevision == autoArchiveRevision, !savingAutoArchive {
                autoArchive = settings.autoArchive; autoArchiveFailure = nil
            }
            guard revision == worktreesRevision, !savingWorktrees else { return }
            includeWorktrees = settings.includeWorktrees
            worktreesFailure = nil
        } catch {
            guard isCurrent(scope), revision == worktreesRevision else { return }
            worktreesFailure = "Project settings could not be loaded. Try again."
        }
    }

    func setContinuation(_ settings: ContinuationSettings) async {
        guard let model, let scope = fenced(), !savingContinuation else { return }
        continuationRevision += 1
        savingContinuation = true; continuationFailure = nil
        defer { if isCurrent(scope) { savingContinuation = false } }
        do {
            let saved: ProjectDiscoverySettings = try await model.manage(ProjectDiscoverySettings.path,
                method: "PUT", body: JSONEncoder().encode(settings))
            guard isCurrent(scope) else { return }
            continuation = saved.continuation
        } catch {
            guard isCurrent(scope) else { return }
            continuationFailure = "This choice could not be saved. Try again."
        }
    }

    /// Cancels the computer's scheduled resume of a thread a usage limit
    /// stopped, then rereads the thread so its row and header drop "Resumes at".
    func cancelResume(_ conversationID: String) async {
        guard let model, let scope = fenced(), cancellingResume.insert(conversationID).inserted else { return }
        resumeFailures[conversationID] = nil
        defer { if isCurrent(scope) { cancellingResume.remove(conversationID) } }
        struct Empty: Decodable, Sendable {}
        do {
            let _: Empty = try await model.manage(
                "/api/v1/conversations/\(ConnectionModel.escape(conversationID))/usage-resume", method: "DELETE")
        } catch PairingFailure.response(404) {
            // Already resumed or cancelled elsewhere; the reread shows which.
        } catch {
            guard isCurrent(scope) else { return }
            resumeFailures[conversationID] = "The resume could not be cancelled. Try again."
            return
        }
        guard isCurrent(scope) else { return }
        await refreshDetail(conversationID, afterCurrentRead: true)
        if let projectID = details[conversationID]?.projectId { loadThreads(projectID) }
    }

    func setAutoArchive(_ rule: AutoArchiveRule) async {
        guard let model, let scope = fenced(), !savingAutoArchive else { return }
        autoArchiveRevision += 1
        savingAutoArchive = true; autoArchiveFailure = nil
        defer { if isCurrent(scope) { savingAutoArchive = false } }
        do {
            let settings: ProjectDiscoverySettings = try await model.manage(ProjectDiscoverySettings.path,
                method: "PUT", body: JSONEncoder().encode(AutoArchiveSettingsUpdate(rule)))
            guard isCurrent(scope) else { return }
            autoArchive = settings.autoArchive
        } catch {
            guard isCurrent(scope) else { return }
            autoArchiveFailure = "Auto-archive could not be saved. Try again."
        }
    }

    func setIncludeWorktrees(_ enabled: Bool) async {
        guard let model, let scope = fenced(), !savingWorktrees else { return }
        worktreesRevision += 1
        savingWorktrees = true; worktreesFailure = nil
        defer { if isCurrent(scope) { savingWorktrees = false } }
        do {
            let settings: ProjectDiscoverySettings = try await model.manage(ProjectDiscoverySettings.path,
                method: "PUT", body: JSONEncoder().encode(ProjectDiscoverySettings(includeWorktrees: enabled)))
            guard isCurrent(scope) else { return }
            includeWorktrees = settings.includeWorktrees
            // Cancel snapshots begun before the host saved the choice. The
            // refreshed catalog supplies both pins and rows; no history is deleted.
            refreshTask?.cancel(); refreshTask = nil; refreshID = nil
            threadTasks.values.forEach { $0.cancel() }; threadTasks = [:]
            continuations.values.forEach { $0.cancel() }; continuations = [:]; stillReadingAnswers = [:]
            pendingMore = []; createdDuringThreadRead = [:]
            let loadedProjects = Array(threads.keys)
            threads = Dictionary(uniqueKeysWithValues: loadedProjects.map { ($0, ProjectThreadsState()) })
            pinned = []
            await refresh()
        } catch {
            guard isCurrent(scope) else { return }
            worktreesFailure = "Project settings could not be saved. Try again."
        }
    }

    /// Whether each provider that can restore files on rewind keeps backups on this Mac.
    func fileRestoreSettings() async throws -> FileRestoreSettings {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let response: FileRestoreSettings = try await model.manage("/api/v1/settings/file-restore")
        guard isCurrent(scope) else { throw CancellationError() }
        return response
    }

    func setFileRestore(_ family: AgentFamily, enabled: Bool) async throws -> FileRestoreSettings {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let response: FileRestoreSettings = try await model.manage(
            "/api/v1/settings/file-restore/\(ConnectionModel.escape(family.rawValue))", method: "PUT",
            body: JSONEncoder().encode(["enabled": enabled]))
        guard isCurrent(scope) else { throw CancellationError() }
        return response
    }

    /// What `family`'s conversations show on this Mac.
    func chatDetail(for family: AgentFamily?) -> ChatDetailLevel { chatDetail?.detail(for: family) ?? .everything }

    /// Reads this Mac's chat detail alone (Settings); false when the Mac didn't answer.
    @discardableResult func refreshChatDetail() async -> Bool {
        guard let model, let scope = fenced(), !model.previewMode else { return false }
        let revision = chatDetailRevision
        guard let read: ChatDetailSettings = try? await model.manage(ChatDetailSettings.path) else { return false }
        guard isCurrent(scope), chatDetailRevision == revision else { return true }
        if chatDetail != read { chatDetail = read; saveCache() }
        return true
    }

    /// Saves `detail` on this Mac, which answers with every provider's choice.
    func setChatDetail(_ family: AgentFamily, _ detail: ChatDetailLevel) async throws {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        chatDetailRevision += 1
        let response: ChatDetailSettings = try await model.manage(
            ChatDetailSettings.path(family), method: "PUT", body: ChatDetailSettings.request(detail))
        guard isCurrent(scope) else { throw CancellationError() }
        chatDetail = response
        saveCache()
    }

    /// The level `conversationID` overrides its provider with, if it does.
    func chatDetailOverride(_ conversationID: String?) -> ChatDetailLevel? {
        conversationID.flatMap { chatDetail?.chats[$0] }
    }

    /// Shows `level` in one chat (nil follows its provider again). The chat
    /// changes at once. A Mac that doesn't save it puts the chat back to what the
    /// Mac last confirmed, and with several taps in flight only the newest one
    /// decides what the chat shows.
    func setChatDetail(chat conversationID: String, to level: ChatDetailLevel?) async throws {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        chatDetailRevision += 1
        var pending = chatDetailPending[conversationID] ?? ChatDetailPending(confirmed: chatDetailOverride(conversationID))
        pending.latest += 1
        pending.inFlight += 1
        let ticket = pending.latest
        chatDetailPending[conversationID] = pending
        chatDetail = (chatDetail ?? ChatDetailSettings(providers: [])).setting(chat: conversationID, to: level)
        defer {
            chatDetailPending[conversationID]?.inFlight -= 1
            if chatDetailPending[conversationID]?.inFlight == 0 { chatDetailPending[conversationID] = nil }
        }
        do {
            let response: ChatDetailSettings = try await model.manage(
                ChatDetailSettings.path(chat: ConnectionModel.escape(conversationID)), method: "PUT",
                body: ChatDetailSettings.request(chat: level))
            guard isCurrent(scope) else { throw CancellationError() }
            chatDetailPending[conversationID]?.confirmed = response.chats[conversationID]
            guard chatDetailPending[conversationID]?.latest == ticket else { return }
            chatDetailRevision += 1
            chatDetail = response
            saveCache()
        } catch {
            if isCurrent(scope), let pending = chatDetailPending[conversationID], pending.latest == ticket {
                chatDetailRevision += 1
                chatDetail = chatDetail?.setting(chat: conversationID, to: pending.confirmed)
            }
            throw error
        }
    }

    func continuation(_ conversationID: String) async throws -> DesktopContinuation {
        guard let model, let scope = fenced() else { throw PairingFailure.response(401) }
        let response: DesktopContinuation = try await model.manage("/api/v1/conversations/\(ConnectionModel.escape(conversationID))/desktop-continuation")
        guard isCurrent(scope) else { throw CancellationError() }
        return response
    }

    // Bounded offline cache: the project list and each first thread page.
    private struct Cache: Codable, Equatable, Sendable {
        let projects: [ProjectSummary]
        let threads: [String: [ProjectThreadSummary]]
        let details: [String: ProjectConversationDetail]?
        let pinned: [PinnedProjectThread]?
        let supportsModes: Bool?
        let supportsProjects: Bool?
        let chatDetail: ChatDetailSettings?
        /// Each provider's capability record, so a cached launch keeps the
        /// controls the Mac offered before it answers again.
        let families: [ProjectFamilyAvailability]?
    }
    static func cacheKeySuffix(_ connection: SavedConnection) -> String {
        Data((connection.credential.hostInstallationId + ":" + connection.credential.deviceId).utf8).base64EncodedString()
    }
    private func hostKey() -> String? { model?.connection.map(Self.cacheKeySuffix) }
    private var cacheKey: String? { hostKey().map { "wonder.projects." + $0 } }
    private var recentKey: String? { hostKey().map { "wonder.projects.recent." + $0 } }
    /// The catalog last handed to the writer, so an unchanged one is
    /// compared in memory instead of encoded on the main thread.
    private var savedCache: (key: String, value: Cache)?
    /// A refresh saves after the catalog and after each loaded Project's page;
    /// an unchanged catalog (most refreshes) writes nothing. `refreshed`: a
    /// successful read from the Mac, which tells the widget its rows are
    /// current even when unchanged (the publisher renews its timestamp at
    /// most every 15 minutes, so the widget doesn't ask to open Cupola).
    private func saveCache(refreshed: Bool = false) {
        guard let cacheKey, model?.previewMode == false else { return }
        let firstPages = threads.mapValues { Array($0.threads.prefix(SidebarProjection.initialThreads + 10)) }
        let persisted = detailIDsToPersist()
        let cache = Cache(projects: projects, threads: firstPages,
            details: details.filter { persisted.contains($0.key) },
            pinned: pinned, supportsModes: supportsModes, supportsProjects: supportsProjects, chatDetail: chatDetail,
            families: families)
        if savedCache?.key != cacheKey || savedCache?.value != cache {
            savedCache = (cacheKey, cache)
            ProjectCatalogWriter.shared.schedule(cacheKey) {
                // Sorted keys: unsorted ones come out in a different order on
                // each encoding, so an unchanged catalog would never compare equal.
                let encoder = JSONEncoder(); encoder.outputFormatting = .sortedKeys
                return try? encoder.encode(cache)
            }
        } else if !refreshed { return }
        widgetSnapshotChanged?()
    }
    private func restoreCache() {
        if let recentKey, let data = UserDefaults.standard.data(forKey: recentKey),
           let recent = try? JSONDecoder().decode(RecentlyOpenedConversations.self, from: data) {
            recentlyOpened = RecentlyOpenedConversations(ids: recent.ids)
        }
        guard let cacheKey, let data = UserDefaults.standard.data(forKey: cacheKey),
              let cache = try? JSONDecoder().decode(Cache.self, from: data) else { return }
        projects = cache.projects
        details = cache.details ?? [:]
        pinned = (cache.pinned ?? []).map(\.saved)
        supportsModes = cache.supportsModes ?? false
        supportsProjects = cache.supportsProjects
        chatDetail = cache.chatDetail
        families = cache.families ?? []
        for (project, rows) in cache.threads where threads[project] == nil {
            #if WONDER_DIAGNOSTICS
            // The slow-Project fixture starts with that Project unanswered, so
            // rows an earlier run saved for it are not restored.
            if DiagnosticSubagentFixture.slowProjectFixture, project == "slow-project" { continue }
            #endif
            threads[project] = ProjectThreadsState(saved: rows)
        }
    }
    func forgetCache() {
        if let cacheKey { ProjectCatalogWriter.shared.forget(cacheKey) }
        savedCache = nil
        if let recentKey { UserDefaults.standard.removeObject(forKey: recentKey) }
        widgetSnapshotChanged?()
    }
}

/// One model-owned order store shared by the computer libraries. Writes happen
/// on catalog membership changes and completed moves, never while a finger moves.
@MainActor final class SidebarOrderStore: ObservableObject {
    static let shared = SidebarOrderStore()
    @Published private(set) var value: SidebarOrder
    private let defaults: UserDefaults
    private let key: String

    init(defaults: UserDefaults = .standard, key: String? = nil) {
        self.defaults = defaults
        #if WONDER_DIAGNOSTICS
        let arguments = ProcessInfo.processInfo.arguments
        let fixture = arguments.contains("-diagnostics-sidebar-reorder")
        let diagnostics = arguments.contains { $0.hasPrefix("-diagnostics-") }
        self.key = key ?? (fixture ? "cupola.sidebarOrder.reorderFixture" : diagnostics ? "cupola.sidebarOrder.diagnostics" : "cupola.sidebarOrder.v1")
        if key == nil && ((fixture && arguments.contains("-diagnostics-sidebar-reorder-reset")) || (diagnostics && !fixture)) {
            defaults.removeObject(forKey: self.key)
        }
        #else
        self.key = key ?? "cupola.sidebarOrder.v1"
        #endif
        value = defaults.data(forKey: self.key).flatMap { try? JSONDecoder().decode(SidebarOrder.self, from: $0) } ?? SidebarOrder()
    }

    private enum Member: Hashable {
        case project(String, Bool)
        case thread(String, String?, Bool)
        case pin(String, String?)
    }
    private var observedMembership: [String: Set<Member>] = [:]
    func observe(_ hosts: [SidebarHostProjects]) {
        // Status, title, activity and loading changes do not sort or persist order.
        let changed = hosts.filter { host in
            var members = Set<Member>()
            for project in host.projects { members.insert(.project(project.id, project.isIncluded)) }
            for state in host.threads.values {
                for thread in state.threads { members.insert(.thread(thread.reference, thread.conversationId, thread.isPinned)) }
            }
            for pin in host.pinned { members.insert(.pin(pin.thread.reference, pin.thread.conversationId)) }
            guard observedMembership[host.hostID] != members else { return false }
            observedMembership[host.hostID] = members
            return true
        }
        guard !changed.isEmpty, SidebarOrder.Section.allCases.contains(where: value.isManual) else { return }
        var next = value
        let projects = changed.flatMap { host in
            SidebarProjection.sorted(host.projects.filter(\.isIncluded)).map { SidebarOrderID(host: host.hostID, item: $0.id) }
        }
        typealias Entry = (host: String, thread: ProjectThreadSummary)
        var threads: [Entry] = [], pins: [Entry] = []
        for host in changed {
            for state in host.threads.values {
                for thread in state.threads { threads.append((host.hostID, thread)) }
            }
            for pin in SidebarProjection.pinnedThreads(host) { pins.append((host.hostID, pin.thread)) }
        }
        func newest(_ left: Entry, _ right: Entry) -> Bool {
            if left.thread.updatedAt != right.thread.updatedAt { return left.thread.updatedAt > right.thread.updatedAt }
            if left.thread.reference != right.thread.reference { return left.thread.reference < right.thread.reference }
            return left.host < right.host
        }
        threads.sort(by: newest)
        pins.sort(by: newest)
        for entry in threads + pins { next.recordThread(host: entry.host, thread: entry.thread) }
        if next.isManual(.projects) { next.observe(projects, in: .projects) }
        if next.isManual(.chats) { next.observe(threads.map { SidebarOrderID(host: $0.host, item: $0.thread.reference) }, in: .chats) }
        if next.isManual(.pinned) { next.observe(pins.map { SidebarOrderID(host: $0.host, item: $0.thread.reference) }, in: .pinned) }
        save(next)
    }

    func reconcilePinned(host: String, threads: [ProjectThreadSummary]) {
        guard value.isManual(.pinned) else { return }
        var next = value
        next.reconcilePinned(host: host, threads: threads)
        save(next)
    }

    func reconcileThreads(host: String, project: String, state: ProjectThreadsState) {
        guard value.isManual(.chats) || value.isManual(.pinned) else { return }
        var next = value
        next.reconcileThreads(host: host, project: project, threads: state.threads,
            complete: state.hasLoaded && state.nextCursor == nil && state.partial.isEmpty && !state.isStillReading && state.failure == nil)
        save(next)
    }

    /// Creation is known locally, independent of computer clock skew. A retry
    /// naming an already-known ID must preserve any intervening manual move.
    func insertedThread(host: String, thread: ProjectThreadSummary) {
        guard value.isManual(.chats) || value.isManual(.pinned) else { return }
        var next = value
        let id = SidebarOrderID(host: host, item: thread.reference)
        next.recordThread(host: host, thread: thread)
        if next.isManual(.chats) { next.observe([id], in: .chats) }
        if thread.isPinned && next.isManual(.pinned) { next.observe([id], in: .pinned) }
        save(next)
    }

    @discardableResult func move(_ source: SidebarOrderID, relativeTo target: SidebarOrderID, after: Bool,
                                 visible: [SidebarOrderID], section: SidebarOrder.Section, catalogs: [SidebarHostProjects]) -> Bool {
        var next = value
        // Seed every known slot once at the first manual move, including hidden
        // computers and collapsed Projects. Later observation only adds membership.
        if !next.isManual(section) {
            for host in catalogs {
                for (project, state) in host.threads {
                    next.reconcileThreads(host: host.hostID, project: project, threads: state.threads, complete: false)
                }
                let ids: [SidebarOrderID]
                switch section {
                case .projects:
                    ids = SidebarProjection.sorted(host.projects.filter(\.isIncluded)).map { SidebarOrderID(host: host.hostID, item: $0.id) }
                case .pinned:
                    ids = SidebarProjection.pinnedThreads(host).map { SidebarOrderID(host: host.hostID, item: $0.thread.reference) }
                case .chats:
                    ids = host.threads.values.flatMap(\.threads).sorted { $0.updatedAt > $1.updatedAt }.map { SidebarOrderID(host: host.hostID, item: $0.reference) }
                }
                next.observe(ids, in: section)
            }
        }
        guard next.move(source, relativeTo: target, after: after, visible: visible, in: section) else { return false }
        save(next)
        return true
    }

    func removeProjects(host: String, ids: Set<String>) {
        var next = value
        next.removeProjects(host: host, ids: ids)
        save(next)
    }

    func removeThreads(host: String, references: Set<String>) {
        var next = value
        let ids = Set(references.map { SidebarOrderID(host: host, item: $0) })
        next.remove(ids, from: .pinned)
        next.remove(ids, from: .chats)
        save(next)
    }

    private func save(_ next: SidebarOrder) {
        guard next != value, let data = try? JSONEncoder().encode(next) else { return }
        defaults.set(data, forKey: key)
        value = next
    }
}

/// Writes the saved Projects catalog off the main thread. A burst of saves
/// collapses to the newest snapshot per key, writes run in order on one queue
/// as soon as it is free (so suspension finds nothing waiting on a timer), and
/// `forget` fences every write scheduled before it, so a late write never
/// restores a forgotten pairing's catalog. Keys name the host and device, so
/// a write cannot land in another pairing's cache. Suspension holds a
/// background grant until the queue drains (`flushBeforeSuspension`).
final class ProjectCatalogWriter: @unchecked Sendable {
    static let shared = ProjectCatalogWriter()
    private let queue = DispatchQueue(label: "com.swaymun.wonder.project-catalog", qos: .utility)
    private let lock = NSLock()
    private var pending: [String: (generation: UInt64, encode: @Sendable () -> Data?)] = [:]
    private var generations: [String: UInt64] = [:]

    func schedule(_ key: String, encode: @escaping @Sendable () -> Data?) {
        let first = lock.withLock {
            defer { pending[key] = (generations[key, default: 0], encode) }
            return pending[key] == nil
        }
        if first { queue.async { self.write(key) } }
    }

    func forget(_ key: String) {
        lock.withLock {
            generations[key, default: 0] &+= 1
            pending[key] = nil
            UserDefaults.standard.removeObject(forKey: key)
        }
    }

    private func write(_ key: String) {
        guard let next = lock.withLock({ pending.removeValue(forKey: key) }), let data = next.encode() else { return }
        lock.withLock {
            // An unchanged catalog (most refreshes) is not written again.
            guard generations[key, default: 0] == next.generation, UserDefaults.standard.data(forKey: key) != data else { return }
            UserDefaults.standard.set(data, forKey: key)
        }
    }

    /// Runs `done` once every write scheduled before the call has finished.
    func flush(_ done: @escaping @Sendable () -> Void) { queue.async(execute: done) }

    /// Leaving the foreground: a background grant keeps the app running until
    /// the newest catalog is written, without blocking the main thread.
    @MainActor func flushBeforeSuspension(grant: CacheBackgroundTask = CacheBackgroundTask(
        begin: { UIApplication.shared.beginBackgroundTask(withName: "Save Projects", expirationHandler: $0) })) {
        flush { Task { @MainActor in grant.finish() } }
    }

    #if WONDER_DIAGNOSTICS
    /// Returns once every write scheduled before the call has finished.
    func settle() async {
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in flush { done.resume() } }
    }
    #endif
}

/// Selection is keyed independently of placement: moving a pinned row, changing
/// search or paging never selects a different conversation.
@MainActor final class ThreadArchiveSelection: ObservableObject {
    struct Target: Identifiable {
        let host: String
        let scope: String
        let project: String
        let thread: ProjectThreadSummary
        var id: String { host + "/" + thread.reference }
    }
    struct Failure: Identifiable {
        let target: Target
        let message: String
        var id: String { target.id }
    }
    @Published private(set) var isSelecting = false
    @Published private(set) var selected: [Target] = []
    @Published private(set) var isArchiving = false
    @Published private(set) var failures: [Failure] = []
    @Published private(set) var notice: String?
    @Published private(set) var requestID: UUID?
    private var selectedIDs: Set<String> = []

    func contains(_ id: String) -> Bool { selectedIDs.contains(id) }
    func begin() { isSelecting = true; notice = nil; failures = [] }
    func toggle(_ target: Target) {
        guard !isArchiving else { return }
        if !isSelecting { begin() }
        if contains(target.id) { selected.removeAll { $0.id == target.id }; selectedIDs.remove(target.id) }
        else { selected.append(target); selectedIDs.insert(target.id) }
        failures.removeAll { $0.id == target.id }
        notice = nil
    }
    func selectAll(_ targets: [Target]) {
        guard !isArchiving else { return }
        if !isSelecting { begin() }
        for target in targets where selectedIDs.insert(target.id).inserted { selected.append(target) }
        notice = nil; failures = []
    }
    func cancel() {
        guard !isArchiving else { return }
        selected = []; selectedIDs = []; failures = []; notice = nil; isSelecting = false; requestID = nil
    }
    func requestArchive() {
        guard !selected.isEmpty, !isArchiving else { return }
        requestID = UUID()
    }

    /// The view's task owns this work. Group only adjacent rows from the same
    /// computer/Project, preserving selection order across hosts and pages.
    func run(resolve: (String) -> ConnectionModel?, archived: (Target) -> Void) async {
        guard requestID != nil, !isArchiving else { return }
        isArchiving = true; failures = []; notice = nil
        defer { isArchiving = false; requestID = nil }
        let pending = selected
        var succeeded = 0
        var index = 0
        while index < pending.count {
            guard !Task.isCancelled else { break }
            let first = pending[index]
            var end = index + 1
            while end < pending.count, end - index < ProjectLibrary.archiveBatchLimit,
                  pending[end].host == first.host, pending[end].scope == first.scope,
                  pending[end].project == first.project { end += 1 }
            let batch = Array(pending[index..<end])
            do {
                guard let model = resolve(first.host), model.assignmentScope == first.scope else { throw CancellationError() }
                let results = try await model.projects.archive(first.project, threads: batch.map(\.thread))
                guard !Task.isCancelled, model.assignmentScope == first.scope else { break }
                for (target, result) in zip(batch, results) {
                    if let message = result.error { failures.append(Failure(target: target, message: message)) }
                    else {
                        selected.removeAll { $0.id == target.id }
                        selectedIDs.remove(target.id)
                        succeeded += 1
                        archived(target)
                    }
                }
            } catch is CancellationError { break }
            catch {
                failures += batch.map { Failure(target: $0, message: managementError(error)) }
            }
            index = end
        }
        if selected.isEmpty { isSelecting = false }
        if !failures.isEmpty { notice = "Archived \(succeeded); \(failures.count) could not be archived." }
        else if succeeded > 0 { notice = "Archived \(succeeded)" }
    }
}

extension ConnectionModel {
    /// Present a project thread through the shared conversation surface. It is
    /// identified by its Project and provider thread.
    func projectChat(_ detail: ProjectConversationDetail) -> ChatSummary {
        ChatSummary(conversationId: detail.conversationId, title: detail.title, lastMessagePreview: nil,
                    lastMessageAt: nil, messageCount: 0, deliveryState: nil, hasUnread: detail.hasUnread,
                    isArchived: detail.isArchived == true, isPinned: detail.isPinned)
    }
    func projectDetail(_ chat: ChatSummary) -> ProjectConversationDetail? { projects.details[chat.id] }
    /// Whether work holds back archiving this chat: its own activity says it is
    /// working, or Chats lists it as working (the chat's activity may still be
    /// loading, or have failed to load). The open chat's Archive and its
    /// confirmation both check this, like the Chats row.
    func archiveWaitsForWork(_ chatID: String) -> Bool {
        conversationWorking(chatID) || projects.listsWorking(chatID)
    }
}
