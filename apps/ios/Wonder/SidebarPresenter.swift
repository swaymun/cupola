import Foundation
import Combine
import SwiftUI
import WonderPairing

@MainActor final class SidebarPresenter: ObservableObject {
    struct Inputs: Equatable {
        var models: [ObjectIdentifier]
        /// Empty shows every Mac; otherwise the single selected Mac.
        var visibleHosts: Set<String>
        var search: String
        var filter: SidebarSessionFilter = .all
        var layout: SidebarLayout = .byProject
        var isVisible = true
        var reduceMotion = false
        var expanded: Set<String>
        var selection: ShellRoute
        var expansionInitialized: Bool
    }
    struct Source {
        let hostID: String
        let model: ConnectionModel
    }
    /// A computer's dot: green connected, orange connecting, red disconnected.
    enum Link: Equatable {
        case connected, connecting, disconnected
        @MainActor init(_ model: ConnectionModel) {
            if model.accessEnded { self = .disconnected }
            else { self = model.macConnected.map { $0 ? .connected : .disconnected } ?? .connecting }
        }
        var color: Color { switch self { case .connected: .green; case .connecting: .orange; case .disconnected: .red } }
        var spoken: String { switch self { case .connected: "connected"; case .connecting: "connecting"; case .disconnected: "disconnected" } }
    }
    struct Pill: Identifiable, Equatable {
        let id: String
        let name: String
        let link: Link
    }

    @Published private(set) var rows: [SidebarRow] = []
    @Published private(set) var pills: [Pill] = []
    /// The shown computers Add Project can add to, in sidebar order.
    @Published private(set) var projectHosts: [Pill] = []
    /// Computers with no Project to show, by host: one line each under the list.
    @Published private(set) var noProjects: [String: String] = [:]
    /// Each computer's name by host, only while several are paired: rows name
    /// their computer then, and never with one.
    @Published private(set) var computerNames: [String: String] = [:]
    @Published private(set) var hasResults = false
    @Published private(set) var hasUnresolvedThreads = false
    /// Threads waiting on the owner (a permission request or a question), keyed
    /// by `needsInputKey`; the same requests the conversation's pill offers,
    /// plus threads whose agent waits on the Mac itself.
    @Published private(set) var needsInput: Set<String> = []
    /// The subset of `needsInput` answered on the Mac, not in Wonder: a desktop
    /// app's permission prompt or question.
    @Published private(set) var waitingOnMac: Set<String> = []
    /// First run: the project to open so the sidebar is not a list of closed folders.
    @Published private(set) var expansionSuggestion: String?
    private var inputs: Inputs?
    /// The Macs as last projected, for All chats paging.
    private var hosts: [String: SidebarHostProjects] = [:]
    private var sources: [Source] = []
    private var subscriptions: [AnyCancellable] = []
    private var rebuildScheduled = false
    private var expiryTask: Task<Void, Never>?
    deinit { expiryTask?.cancel() }

    func configure(_ inputs: Inputs, sources: [Source]) {
        let rebind = self.inputs?.models != inputs.models
        self.inputs = inputs
        self.sources = sources
        if rebind { subscribe() }
        rebuild()
    }

    private func subscribe() {
        subscriptions = sources.flatMap { source -> [AnyCancellable] in
            let model = source.model
            let changes: [AnyPublisher<Void, Never>] = [
                model.projects.objectWillChange.map { _ in }.eraseToAnyPublisher(),
                model.projects.sidebarOrder.objectWillChange.map { _ in }.eraseToAnyPublisher(),
                model.$macConnected.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$accessEnded.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$connectionLoss.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$rescue.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$connection.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$storageFailure.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$attention.dropFirst().map { _ in }.eraseToAnyPublisher(),
                model.$asyncQuestions.dropFirst().map { _ in }.eraseToAnyPublisher(),
            ]
            return changes.map { $0.sink { [weak self] in self?.scheduleRebuild() } }
        }
    }

    /// The observed change has not been applied yet when it is announced.
    private func scheduleRebuild() {
        guard !rebuildScheduled else { return }
        rebuildScheduled = true
        Task { @MainActor [weak self] in
            self?.rebuildScheduled = false
            self?.rebuild()
        }
    }

    private func rebuild() {
        guard let inputs else { return }
        let visible = sources.filter { inputs.visibleHosts.isEmpty || inputs.visibleHosts.contains($0.hostID) }
        let allHosts = sources.map { source -> SidebarHostProjects in
            let model = source.model, catalog = model.projects
            return SidebarHostProjects(hostID: source.hostID, name: model.macName, isOnline: model.macConnected == true && !model.accessEnded,
                                       supportsProjects: catalog.matchesCurrentConnection ? catalog.supportsProjects : nil,
                                       projects: catalog.matchesCurrentConnection ? catalog.projects : [],
                                       threads: catalog.matchesCurrentConnection ? catalog.threads : [:],
                                       pinned: catalog.matchesCurrentConnection ? catalog.pinned : [],
                                       notice: Self.notice(model),
                                       unavailableFamilies: Set(catalog.families.filter { !$0.available }.map(\.family)),
                                       partial: catalog.matchesCurrentConnection ? catalog.partial : [])
        }
        let orderStore = sources.first?.model.projects.sidebarOrder
        let hosts = allHosts.filter { inputs.visibleHosts.isEmpty || inputs.visibleHosts.contains($0.hostID) }
        var selectedHost: String?, selectedConversation: String?
        if case .conversation(let host, let id) = inputs.selection { selectedHost = host; selectedConversation = id }
        let (waiting, onMac) = Self.waitingThreads(visible)
        // An unreachable computer shows its red dot and no failure or Retry row; it reconnects on its
        // own. Plain statements ("No threads yet", "No matches in loaded chats") still show, so Chats
        // is never blank for want of an explanation.
        let unreachable = Set(hosts.filter { !$0.isOnline && $0.notice != .pairAgain }.map(\.hostID))
        let next = SidebarProjection.rows(hosts: hosts, expanded: inputs.expanded,
                                          selectedHost: selectedHost, selectedConversation: selectedConversation, search: inputs.search,
                                          filter: inputs.filter, needsInput: waiting, layout: inputs.layout, order: orderStore?.value ?? SidebarOrder())
            .filter { row in
                switch row {
                case .threadsNotice(let host, _, _, let canRetry, _), .chatsNotice(let host, _, _, let canRetry, _):
                    !(canRetry && unreachable.contains(host))
                default: true
                }
            }
        self.hosts = Dictionary(hosts.map { ($0.hostID, $0) }, uniquingKeysWith: { $1 })
        if next != rows {
            let changedPins = Self.pinIDs(rows).symmetricDifference(Self.pinIDs(next))
            let continuing = Set(rows.map(\.id)).intersection(next.map(\.id))
            if inputs.isVisible && !changedPins.isDisjoint(with: continuing) {
                // The source changes are coalesced asynchronously, so the
                // action's transaction does not reach this publication.
                var transaction = Transaction(animation: inputs.reduceMotion ? nil : .easeInOut(duration: 0.25))
                transaction.disablesAnimations = inputs.reduceMotion
                withTransaction(transaction) { rows = next }
            } else {
                rows = next
            }
        }
        let results = next.contains { row in
            if inputs.filter == .all { return row.isResult }
            switch row {
            case .pinned, .thread, .threadsLoading, .threadsNotice, .moreThreads,
                 .chat, .chatsLoading, .chatsNotice, .moreChats: return true
            default: return false
            }
        }
        if results != hasResults { hasResults = results }
        let unresolved = visible.contains { source in
            let catalog = source.model.projects
            if !catalog.matchesCurrentConnection || source.model.macConnected != true || source.model.accessEnded || catalog.supportsProjects != true || catalog.loadingProjects || catalog.failure != nil || !catalog.partial.isEmpty { return true }
            return catalog.projects.filter(\.isIncluded).contains { project in
                guard let state = catalog.threads[project.id] else { return true }
                return !state.hasLoaded || state.isLoading || state.nextCursor != nil || state.failure != nil || !state.partial.isEmpty
            }
        }
        if unresolved != hasUnresolvedThreads { hasUnresolvedThreads = unresolved }
        if waiting != needsInput { needsInput = waiting }
        if onMac != waitingOnMac { waitingOnMac = onMac }
        let nextPills = sources.map { Pill(id: $0.hostID, name: $0.model.macName, link: Link($0.model)) }
        if nextPills != pills { pills = nextPills }
        let nextNoProjects = inputs.search.isEmpty && inputs.filter == .all ? Self.noProjectsNotices(hosts) : [:]
        if nextNoProjects != noProjects { noProjects = nextNoProjects }
        let nextNames = sources.count > 1 ? Dictionary(sources.map { ($0.hostID, $0.model.macName) }, uniquingKeysWith: { $1 }) : [:]
        if nextNames != computerNames { computerNames = nextNames }
        let nextProjectHosts = Self.projectHosts(hosts, links: Dictionary(nextPills.map { ($0.id, $0.link) }, uniquingKeysWith: { $1 }))
        if nextProjectHosts != projectHosts { projectHosts = nextProjectHosts }
        let suggestion = inputs.expansionInitialized ? nil : firstRunExpansion(visible)
        if suggestion != expansionSuggestion { expansionSuggestion = suggestion }
        scheduleExpiry(visible, enabled: inputs.isVisible)
        if inputs.isVisible && inputs.layout == .allChats { loadAllChats(hosts, inputs) }
        else if inputs.isVisible && inputs.filter == .all { loadExpandedThreads(visible, inputs) }
    }

    /// Native List reordering commits only on drop; never while the pointer moves.
    @discardableResult func move(from indices: IndexSet, to destination: Int) -> Bool {
        #if WONDER_DIAGNOSTICS
        if DiagnosticSubagentFixture.sidebarReorderFixture {
            NSLog("sidebar-reorder: native offsets %@ -> %ld", indices.map { String($0) }.joined(separator: ","), destination)
        }
        #endif
        guard indices.count == 1, let source = indices.first,
              let placement = SidebarProjection.moveTarget(rows: rows, source: source, destination: destination) else { return false }
        let changed = move(placement.source, to: placement.target, after: placement.after)
        #if WONDER_DIAGNOSTICS
        if changed && DiagnosticSubagentFixture.sidebarReorderFixture { NSLog("sidebar-reorder: move committed") }
        #endif
        return changed
    }

    /// A drop and VoiceOver use the same visible-section move contract.
    @discardableResult func move(_ source: SidebarOrderID, to target: SidebarRow, after: Bool) -> Bool {
        guard let section = target.orderSection, let targetID = target.orderItem,
              let group = target.orderGroup,
              let sourceRow = rows.first(where: { $0.orderItem == source && $0.orderSection == section }),
              sourceRow.orderGroup == group,
              let store = sources.first?.model.projects.sidebarOrder else { return false }
        let visible = rows.filter { $0.orderGroup == group }.compactMap(\.orderItem)
        guard store.move(source, relativeTo: targetID, after: after, visible: visible, section: section, catalogs: Array(hosts.values)) else { return false }
        return true
    }

    func neighbor(of row: SidebarRow, down: Bool) -> SidebarRow? {
        guard let group = row.orderGroup else { return nil }
        let peers = rows.filter { $0.orderGroup == group }
        guard let index = peers.firstIndex(where: { $0.id == row.id }) else { return nil }
        let next = index + (down ? 1 : -1)
        return peers.indices.contains(next) ? peers[next] : nil
    }

    /// The one line under a computer's name: lost access first, then what this
    /// phone could not keep, then why its rows may be out of date.
    static func notice(_ model: ConnectionModel) -> SidebarHostNotice? {
        if model.accessEnded || model.connectionLoss != nil { return .pairAgain }
        if model.macConnected == false, let rescue = model.rescue { return .rescue(rescue) }
        if let failure = model.storageFailure { return .storageFailed(message: failure.message) }
        let catalog = model.projects
        // Unreachable is the red dot, not a notice with Retry.
        if model.macConnected != false, catalog.matchesCurrentConnection, let failure = catalog.failure {
            return .projectsFailed(message: failure, isRetrying: catalog.loadingProjects)
        }
        return nil
    }

    /// A computer takes a new Project once it reports Projects and while it is
    /// still paired, whichever list layout shows.
    static func projectHosts(_ hosts: [SidebarHostProjects], links: [String: Link] = [:]) -> [Pill] {
        hosts.filter { $0.notice != .pairAgain && ($0.supportsProjects == true || !$0.projects.isEmpty) }
            .map { Pill(id: $0.hostID, name: $0.name, link: links[$0.hostID] ?? ($0.isOnline ? .connected : .disconnected)) }
    }

    /// Still-paired computers whose Projects are known and none shown.
    static func noProjectsNotices(_ hosts: [SidebarHostProjects]) -> [String: String] {
        var notices: [String: String] = [:]
        for host in hosts where host.notice != .pairAgain {
            if let text = NoProjectsNotice.text(supportsProjects: host.supportsProjects, projects: host.projects, macName: host.name) {
                notices[host.hostID] = text
            }
        }
        return notices
    }

    static func needsInputKey(host: String, thread: String) -> String {
        SidebarSessionFilter.attentionKey(host: host, thread: thread)
    }

    private static func pinIDs(_ rows: [SidebarRow]) -> Set<String> {
        Set(rows.compactMap { row in
            if case .pinned = row { return row.id }
            return nil
        })
    }

    /// Threads waiting on the owner, and those of them that wait on the Mac
    /// with no request Wonder can answer.
    private static func waitingThreads(_ visible: [Source]) -> (Set<String>, Set<String>) {
        var result = Set<String>(), onMac = Set<String>()
        let now = Date()
        for source in visible {
            let model = source.model, catalog = model.projects
            guard catalog.matchesCurrentConnection, !model.accessEnded else { continue }
            let requests = !model.attention.isEmpty || !model.asyncQuestions.isEmpty
            let threads = catalog.threads.values.flatMap(\.threads) + catalog.pinned.map(\.thread)
            for thread in threads {
                let key = needsInputKey(host: source.hostID, thread: thread.reference)
                if requests && waits(thread, model: model, now: now) { result.insert(key) }
                else if thread.waitsOnMac { result.insert(key); onMac.insert(key) }
            }
        }
        return (result, onMac)
    }

    private static func waits(_ thread: ProjectThreadSummary, model: ConnectionModel, now: Date) -> Bool {
        SidebarSessionFilter.awaitsInput(conversation: thread.conversationId, requests: model.attention,
                                        questions: thread.conversationId.flatMap { model.asyncQuestions[$0] } ?? [],
                                        now: UInt64(max(0, now.timeIntervalSince1970 * 1000)))
    }

    /// Wake once when a request expires; no polling while the sidebar is hidden.
    private func scheduleExpiry(_ visible: [Source], enabled: Bool) {
        expiryTask?.cancel(); expiryTask = nil
        guard enabled else { return }
        let now = UInt64(max(0, Date().timeIntervalSince1970 * 1000))
        let deadlines = visible.flatMap { source in
            source.model.attention.compactMap(\.params.expiresAtMs)
                + source.model.asyncQuestions.values.flatMap { $0.filter { $0.state == "pending" }.map(\.expiresAtMs) }
        }
        guard let next = deadlines.filter({ $0 > now }).min() else { return }
        expiryTask = Task { [weak self] in
            do { try await Task.sleep(for: .milliseconds(next - now)) } catch { return }
            guard !Task.isCancelled else { return }
            self?.rebuild()
        }
    }

    /// Filtering expands Projects temporarily; fetch only rows that reach the
    /// visible list, and leave deeper pages behind their existing Show more action.
    func appeared(host: String, project: String) {
        guard let inputs, inputs.isVisible, inputs.filter != .all,
              inputs.visibleHosts.isEmpty || inputs.visibleHosts.contains(host),
              let source = sources.first(where: { $0.hostID == host }),
              source.model.macConnected != false, !source.model.accessEnded else { return }
        let catalog = source.model.projects
        if let state = catalog.threads[project], state.hasLoaded || state.isLoading || state.failure != nil { return }
        catalog.loadThreads(project)
    }

    /// All chats needs each included Project's first page to order the Mac's list.
    private func loadAllChats(_ hosts: [SidebarHostProjects], _ inputs: Inputs) {
        for host in hosts where host.isOnline {
            guard let catalog = sources.first(where: { $0.hostID == host.hostID })?.model.projects else { continue }
            SidebarProjection.chatsProjectsToLoad(host).forEach { catalog.loadThreads($0) }
        }
    }

    /// Show more in All chats reads the next page of the Project holding the
    /// Mac's older rows back.
    func loadMoreChats() {
        for (host, projected) in hosts {
            guard let project = SidebarProjection.nextChatsProject(projected),
                  let catalog = sources.first(where: { $0.hostID == host })?.model.projects else { continue }
            catalog.loadThreads(project, more: catalog.threads[project]?.hasLoaded == true)
        }
    }

    private func firstRunExpansion(_ visible: [Source]) -> String? {
        for source in visible where source.model.projects.matchesCurrentConnection {
            let included = source.model.projects.projects.filter(\.isIncluded)
            let recent = included.max { ($0.lastUsedAt ?? $0.createdAt) < ($1.lastUsedAt ?? $1.createdAt) } ?? included.first
            if let recent { return SidebarProjection.expansionKey(host: source.hostID, project: recent.id) }
        }
        return nil
    }

    /// First page for each expanded project that has not been asked for. A failed
    /// load keeps its retry row instead of looping.
    private func loadExpandedThreads(_ visible: [Source], _ inputs: Inputs) {
        for source in visible {
            let catalog = source.model.projects
            for project in catalog.projects where project.isIncluded
                && inputs.expanded.contains(SidebarProjection.expansionKey(host: source.hostID, project: project.id)) {
                if let state = catalog.threads[project.id], state.hasLoaded || state.isLoading || state.failure != nil { continue }
                catalog.loadThreads(project.id)
            }
        }
    }
}


/// What a computer with no Project to start a chat in says, in the sidebar and
/// New chat; nil while its Projects are unknown or once one is shown.
enum NoProjectsNotice {
    static func text(supportsProjects: Bool?, projects: [ProjectSummary], macName: String) -> String? {
        guard supportsProjects == true, !projects.contains(where: \.isIncluded) else { return nil }
        return projects.isEmpty ? "No Projects on \(macName)" : "Projects on \(macName) are hidden"
    }
}
