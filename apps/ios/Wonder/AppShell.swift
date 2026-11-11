import SwiftUI
import Combine
import WonderPairing
import WonderComputerView

/// What the main surface shows. Every conversation address carries its Mac.
enum ShellRoute: Hashable, Codable {
    case newChat
    case conversation(host: String, id: String)
}

/// A new-chat request from the sidebar or header, applied by the draft view.
struct NewChatRequest: Equatable {
    let id = UUID()
    let host: String?
    let destination: ChatDestination?
    var exactProject = false
    /// Opens that Mac's screen viewer, as the widget's Computer tile does.
    var showComputer = false
}

/// Widget and external links carry only bounded, nonsecret addresses. The
/// channel-specific scheme is registered in BuildInfo.plist.
enum WonderDeepLink: Equatable {
    case attention
    case chat(host: String, id: String)
    case newProjectChat(host: String, project: String)
    case computer(host: String)
    /// A Project's provider thread, attached on open: the widget's thread tiles.
    case projectThread(host: String, project: String, family: AgentFamily, thread: String)
    /// A Live Activity's thread: a random route this phone resolves with the
    /// Mac, named by the Mac's ID, or by the push registration for an activity
    /// the Mac started.
    case liveActivityRoute(host: String?, registration: String?, route: String)

    static func parse(_ url: URL, scheme: String) -> Self? {
        guard let parts = URLComponents(url: url, resolvingAgainstBaseURL: false),
              url.absoluteString.utf8.count <= 512,
              parts.scheme == scheme, parts.host == "v1", parts.user == nil,
              parts.password == nil, parts.port == nil, parts.query == nil,
              parts.fragment == nil else { return nil }
        let path = parts.percentEncodedPath.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        if path == ["", "attention"] { return .attention }
        if path.count == 4, path[0].isEmpty, path[1] == "hosts", validID(path[2]), path[3] == "computer" {
            return .computer(host: path[2])
        }
        if path.count == 8, path[0].isEmpty, path[1] == "hosts", validID(path[2]), path[3] == "projects",
           validID(path[4]), path[5] == "threads", let family = AgentFamily(rawValue: path[6]), validID(path[7]) {
            return .projectThread(host: path[2], project: path[4], family: family, thread: path[7])
        }
        if path.count == 5, path[0].isEmpty, path[3] == "routes", validID(path[2]), UUID(uuidString: path[4]) != nil {
            if path[1] == "hosts" { return .liveActivityRoute(host: path[2], registration: nil, route: path[4]) }
            if path[1] == "registrations", UUID(uuidString: path[2]) != nil {
                return .liveActivityRoute(host: nil, registration: path[2], route: path[4])
            }
        }
        guard (path.count == 5 || path.count == 6), path[0].isEmpty,
              path[1] == "hosts", validID(path[2]), validID(path[4]) else { return nil }
        if path.count == 5, path[3] == "chats" { return .chat(host: path[2], id: path[4]) }
        if path.count == 6, path[3] == "projects", path[5] == "new" {
            return .newProjectChat(host: path[2], project: path[4])
        }
        return nil
    }

    var host: String {
        switch self {
        case .attention: ""
        case .chat(let host, _), .newProjectChat(let host, _), .computer(let host), .projectThread(let host, _, _, _): host
        case .liveActivityRoute(let host, _, _): host ?? ""
        }
    }

    func url(scheme: String) -> URL? {
        let hostless: Bool = self == .attention || { if case .liveActivityRoute(nil, _, _) = self { return true }; return false }()
        guard !scheme.isEmpty, hostless || Self.validID(host) else { return nil }
        var parts = URLComponents()
        parts.scheme = scheme
        parts.host = "v1"
        switch self {
        case .attention:
            parts.path = "/attention"
        case .chat(_, let id):
            guard Self.validID(id) else { return nil }
            parts.path = "/hosts/\(host)/chats/\(id)"
        case .newProjectChat(_, let project):
            guard Self.validID(project) else { return nil }
            parts.path = "/hosts/\(host)/projects/\(project)/new"
        case .computer:
            parts.path = "/hosts/\(host)/computer"
        case .projectThread(_, let project, let family, let thread):
            guard Self.validID(project), Self.validID(thread) else { return nil }
            parts.path = "/hosts/\(host)/projects/\(project)/threads/\(family.rawValue)/\(thread)"
        case .liveActivityRoute(let host, let registration, let route):
            guard UUID(uuidString: route) != nil else { return nil }
            if host != nil { parts.path = "/hosts/\(self.host)/routes/\(route)" }
            else if let registration, UUID(uuidString: registration) != nil { parts.path = "/registrations/\(registration)/routes/\(route)" }
            else { return nil }
        }
        return parts.url
    }

    static func validID(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 128 && value.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95
        }
    }
}

/// A management screen. It opens full screen in the chat column, pushed onto
/// the main navigation stack; sheets are kept for what pops up from a chat.
enum ShellPage: Hashable {
    case settings
    case usage
    /// Scheduled tasks of every computer, in one list.
    case automations
    case addProject(host: String)
    case projectSettings(host: String, project: String)
    case archivedThreads(host: String, project: String)
    /// Every computer's Projects, hidden ones included, to show them again.
    case manageProjects
    /// What the Project's agents produced, across its chats.
}

/// The one-step Undo Chats offers after archiving a thread or hiding a Project
/// from the sidebar. It says what Undo also changes outside Cupola.
struct SidebarUndo: Identifiable, Equatable {
    enum Action: Equatable {
        case restoreThread(conversation: String, reference: String)
        case showProject(id: String)
    }
    let id = UUID()
    let host: String
    let action: Action
    let message: String
    let detail: String?

    /// `alsoIn` names the provider app that archived it too (Codex).
    static func archived(host: String, title: String, conversation: String, reference: String, alsoIn: String?) -> SidebarUndo {
        SidebarUndo(host: host, action: .restoreThread(conversation: conversation, reference: reference),
                    message: "Archived “\(title)”",
                    detail: alsoIn.map { "If the \($0) app archived it too, Undo restores it there as well." })
    }
    static func hidden(host: String, project: ProjectSummary) -> SidebarUndo {
        SidebarUndo(host: host, action: .showProject(id: project.id), message: "Hid “\(project.name)” from the sidebar",
                    detail: "Its chats stay on your computer.")
    }
    /// What the owner is told when Undo fails; the notice stays so they can retry.
    var failureMessage: String {
        switch action {
        case .restoreThread: "The chat couldn’t be restored. "
        case .showProject: "The Project couldn’t be shown again. "
        }
    }
    /// How long the notice stays; with VoiceOver on it stays until dismissed.
    static let visibleSeconds = 10
}

/// Chats' one Undo notice and its progress. A failed Undo stays, without the
/// timer, until it is dismissed or a retry succeeds.
@MainActor final class SidebarUndoState: ObservableObject {
    @Published private(set) var notice: SidebarUndo?
    @Published private(set) var undoing = false
    @Published private(set) var failed = false

    func show(_ notice: SidebarUndo) {
        self.notice = notice; failed = false
    }
    func dismiss() {
        guard !undoing else { return }
        notice = nil; failed = false
    }
    /// The timer's end clears only a notice that is idle and hasn't failed.
    func expire(_ id: UUID) {
        if notice?.id == id, !undoing, !failed { notice = nil }
    }

    /// Hides a Project from the sidebar, then offers Undo.
    func hide(_ project: ProjectSummary, host: String, in library: ProjectLibrary) async throws {
        _ = try await library.update(project.id, fields: ["isIncluded": false])
        show(.hidden(host: host, project: project))
    }

    /// Reverses the notice's archive or hide. Returns what to tell the owner
    /// when it fails; the notice then stays so they can try again.
    func undo(in library: ProjectLibrary) async -> String? {
        guard !undoing, let notice else { return nil }
        undoing = true
        defer { undoing = false }
        do {
            switch notice.action {
            case .restoreThread(let conversation, let reference):
                try await library.unarchive(conversation: conversation, reference: reference)
            case .showProject(let id):
                _ = try await library.update(id, fields: ["isIncluded": true])
            }
            if self.notice?.id == notice.id { self.notice = nil; failed = false }
            return nil
        } catch is CancellationError {
            return nil
        } catch {
            if self.notice?.id == notice.id { failed = true }
            return notice.failureMessage + managementError(error)
        }
    }
}

/// This device's durable viewing choices, shared by its shells. Only a tool's
/// presentation is saved: computer sessions, leases and input never cross this boundary.
@MainActor final class ConversationViewStateStore {
    static let phone = ConversationViewStateStore(defaults: .standard)
    #if WONDER_DIAGNOSTICS
    static let fixturePhone: ConversationViewStateStore = {
        let name = "wonder.diagnostics.conversation-view-state"
        let defaults = UserDefaults(suiteName: name)!
        let arguments = ProcessInfo.processInfo.arguments
        if !arguments.contains("-diagnostics-durable-store") || arguments.contains("-diagnostics-durable-store-reset") {
            defaults.removePersistentDomain(forName: name)
        }
        return ConversationViewStateStore(defaults: defaults)
    }()
    #endif

    private struct Address: Hashable {
        let host: String
        let conversation: String
        var key: String {
            // Base64 contains no '.', so the two address parts cannot collide.
            let encoded = Data(host.utf8).base64EncodedString() + "." + Data(conversation.utf8).base64EncodedString()
            return "wonder.conversation.view-state.pane.v1." + ConversationFile.digest(Data(encoded.utf8))
        }
    }
    private let defaults: UserDefaults?
    private let routeKey = "wonder.conversation.view-state.route.v1"
    /// Only isolated, in-memory shells need a dictionary. The phone writes one
    /// small preference per address, without evicting older viewing choices.
    private var panes: [Address: ToolPresentation.Saved] = [:]
    private(set) var lastRoute: ShellRoute

    /// No defaults makes an isolated, in-memory shell (previews and unit tests).
    init(defaults: UserDefaults? = nil) {
        self.defaults = defaults
        lastRoute = defaults?.data(forKey: routeKey).flatMap { try? JSONDecoder().decode(ShellRoute.self, from: $0) } ?? .newChat
    }
    func select(_ route: ShellRoute) {
        guard lastRoute != route else { return }
        lastRoute = route
        if let data = try? JSONEncoder().encode(route) { defaults?.set(data, forKey: routeKey) }
    }
    func pane(host: String, conversation: String) -> ToolPresentation.Saved? {
        let address = Address(host: host, conversation: conversation)
        guard let defaults else { return panes[address] }
        return defaults.data(forKey: address.key).flatMap { try? JSONDecoder().decode(ToolPresentation.Saved.self, from: $0) }
    }
    func save(_ pane: ToolPresentation.Saved?, host: String, conversation: String) {
        guard self.pane(host: host, conversation: conversation) != pane else { return }
        let address = Address(host: host, conversation: conversation)
        guard let defaults else { panes[address] = pane; return }
        if let pane, let data = try? JSONEncoder().encode(pane) {
            defaults.set(data, forKey: address.key)
        } else if pane == nil {
            defaults.removeObject(forKey: address.key)
        }
    }
}

/// Per-window navigation state. Another iPad window keeps its own selection;
/// shells share this phone's restoration store, while transports stay in the models.
@MainActor final class ShellState: ObservableObject {
    let viewStates: ConversationViewStateStore
    /// The mounted conversation's token rejects late sheet writes after A → B → A.
    var toolPresentationOwner: UUID?
    @Published var route: ShellRoute {
        didSet { viewStates.select(route) }
    }
    init(viewStates: ConversationViewStateStore = ConversationViewStateStore()) {
        self.viewStates = viewStates
        route = viewStates.lastRoute
    }
    @Published var sidebarOpen = false
    @Published var attentionPresented = false
    @Published var newChatRequest: NewChatRequest?
    /// A tool pane asked for by voice: the open conversation applies it, then clears it.
    @Published var paneRequest: PaneRequest?
    /// A Settings page asked for by voice, by its search row; Settings applies it, then clears it.
    @Published var settingsRowRequest: String?
    struct PaneRequest: Equatable {
        let id = UUID()
        /// Nil closes the pane.
        let tool: CompanionTool?
    }
    /// The management screen over the chat, if any.
    @Published var page: ShellPage?
    var settingsOpen: Bool {
        get { page == .settings }
        set { if newValue { page = .settings } else if page == .settings { page = nil } }
    }
    /// The open conversation shows a tool beside the chat, and whether that tool
    /// fills the window. The sidebar steps aside when the chat would be too narrow.
    @Published var companionPaneOpen = false {
        didSet { if !companionPaneOpen { sidebarOverPane = false } }
    }
    @Published var companionPaneExpanded = false
    /// Show sidebar was chosen while a companion pane had tucked it away.
    @Published var sidebarOverPane = false

    /// Whether a regular-width sidebar is tucked away for the companion pane.
    func sidebarTucked(window: CGSize, sidebarWidth: CGFloat, hasFold: Bool) -> Bool {
        guard companionPaneOpen else { return false }
        if companionPaneExpanded { return true }
        return !sidebarOverPane && CompanionPaneLayout.tucksSidebar(window: window, sidebarWidth: sidebarWidth,
                                                                    hasFold: hasFold, expanded: false)
    }

    /// Show/Hide sidebar beside the conversation. While a pane has tucked the
    /// sidebar away, Show brings it back over the pane's room instead.
    func toggleWideSidebar(hidden: Binding<Bool>, tucked: Bool) {
        if tucked {
            sidebarOverPane = true
            hidden.wrappedValue = false
        } else if sidebarOverPane, !hidden.wrappedValue {
            sidebarOverPane = false
        } else {
            hidden.wrappedValue.toggle()
        }
    }

    /// Brings Chats into view in whichever layout shows: the drawer on a phone
    /// or a narrow window, and beside the conversation on a wide one, where the
    /// remembered Hide choice is cleared and a companion pane yields its room.
    func showChats(defaults: UserDefaults = .standard) {
        sidebarOpen = true
        defaults.set(false, forKey: SidebarPresentation.hiddenKey)
        // Only while a pane could tuck it away; a stale override would turn
        // the next Hide Chats into a no-op.
        if companionPaneOpen { sidebarOverPane = true }
    }

    /// Counts ⌘F presses; Chats focuses its search field on each.
    @Published private(set) var searchRequest = 0
    /// ⌘F: brings Chats into view, then asks it to focus its search field.
    func searchChats(defaults: UserDefaults = .standard) {
        showChats(defaults: defaults)
        searchRequest += 1
    }

    /// The demo just started: its first thread, with Chats open beside or over it.
    func showDemo(defaults: UserDefaults = .standard) {
        open(host: DemoMode.hostID, conversation: DemoContent.signOut.id)
        showChats(defaults: defaults)
    }
    func open(host: String, conversation: String) {
        #if WONDER_DIAGNOSTICS
        DiagnosticPerformanceRun.threadRequested(conversation)
        LatencyHops.mark("open.tap", conversation)
        LatencyHops.sendArmed.insert(conversation)
        #endif
        route = .conversation(host: host, id: conversation)
        sidebarOpen = false
        page = nil
    }
    /// A fresh draft; in a project the draft stays in that project.
    func newChat(host: String? = nil, destination: ChatDestination? = nil, exactProject: Bool = false, showComputer: Bool = false) {
        newChatRequest = NewChatRequest(host: host, destination: destination, exactProject: exactProject, showComputer: showComputer)
        route = .newChat
        sidebarOpen = false
        page = nil
    }
    /// Voice and incoming links use the same bell dropdown as a tap in Chats.
    func showAttention() {
        page = nil
        showChats()
        attentionPresented = true
    }
    /// Opens a management screen in the chat column; a phone's drawer closes over it.
    func show(_ page: ShellPage) {
        self.page = page
        sidebarOpen = false
    }
    /// Whether a conversation is what the window shows right now.
    var showsConversation: Bool { selectedConversation != nil && page == nil && !sidebarOpen }
    var selectedConversation: (host: String, id: String)? {
        if case .conversation(let host, let id) = route { return (host, id) }
        return nil
    }
}

private struct VoiceSceneKey: EnvironmentKey {
    static let defaultValue: ShellState? = nil
}
extension EnvironmentValues {
    var voiceScene: ShellState? {
        get { self[VoiceSceneKey.self] }
        set { self[VoiceSceneKey.self] = newValue }
    }
}

/// Root of the signed-in app: one chat surface with a navigation tree.
struct ChatShell: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject private var push = PushNotifications.shared
    @StateObject private var shell: ShellState
    @State private var sceneID = UUID()
    @State private var windowBounds: CGSize?
    @Environment(\.scenePhase) private var phase
    @AppStorage(SidebarPresentation.hiddenKey) private var wideSidebarHidden = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dynamicTypeSize) private var typeSize
    @State private var choosingProjectsHost: String?
    /// Diagnostics `-show-connections` opens Settings as a sheet. First launch
    /// instead shows GetStartedView, which explains what pairing needs.
    @State private var pairing = false
    @State private var dialogHost: String?
    @State private var linkIssue: String?
    @State private var linkRetry: (WonderDeepLink, ProjectShortcutAddress?)?
    @State private var linkTask: Task<Void, Never>?
    @State private var voiceSheetHost: String?
    @State private var compactVoiceBarVisible = false

    init(library: ConnectionLibrary) {
        self.library = library
        #if WONDER_DIAGNOSTICS
        _shell = StateObject(wrappedValue: ShellState(viewStates: library.isPreview || ProcessInfo.processInfo.arguments.contains("-diagnostics-unpaired")
            ? .fixturePhone : .phone))   // a phone's own last chat must not cover a first-launch fixture
        #else
        _shell = StateObject(wrappedValue: ShellState(viewStates: .phone))
        #endif
    }

    var body: some View {
        VStack(spacing: 0) {
            // Keep the presentation owner stable when an iPhone rotates. A
            // size-class switch used to replace this branch and dismiss the
            // full-screen computer viewer with it.
            if UIDevice.current.userInterfaceIdiom == .pad {
                GeometryReader { window in
                    let compact = window.size.width < SidebarCard.padCompactWidth
                    let sidebarWidth = compact ? SidebarCard.padDrawerWidth(for: window.size)
                        : SidebarCard.regularWidth(for: window.size, accessibilityText: typeSize.isAccessibilitySize, pad: true)
                    let tucked = !compact && shell.sidebarTucked(window: window.size, sidebarWidth: sidebarWidth, hasFold: false)
                    let wideHidden = wideSidebarHidden || tucked
                    let sidebarVisible = compact ? shell.sidebarOpen : !wideHidden
                    HStack(spacing: 0) {
                        Color.clear.frame(width: !compact && !wideHidden ? sidebarWidth : 0)
                            .accessibilityHidden(true)
                        VStack(spacing: 0) {
                            if compact {
                                // iPad window controls overlay the upper leading
                                // corner instead of contributing to safe area.
                                Color.clear.frame(height: 52).accessibilityHidden(true)
                            }
                            NavigationStack { // theme-exempt: a root stack; WonderThemeHost and its screens theme it
                                ShellMain(library: library, shell: shell,
                                          compactIPadWindow: compact,
                                          showSidebarButton: true,
                                          sidebarButtonLabel: compact || wideHidden ? "Show Chats" : "Hide Chats",
                                          sidebarButtonIcon: "sidebar.left") {
                                    toggleIPadSidebar(compact: compact, tucked: tucked)
                                }
                            }
                            .environment(\.shellWindowSize, window.size)
                        }
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                    .overlay(alignment: .leading) {
                        // Hidden buttons can survive accessibilityHidden in
                        // SwiftUI's AX tree and cover the composer to clients.
                        // Only an open compact drawer owns a dismissing scrim.
                        if compact && shell.sidebarOpen {
                            Button { shell.sidebarOpen = false } label: {
                                Color.black.opacity(0.35).ignoresSafeArea()
                            }
                            .buttonStyle(.plain)
                            .accessibilityLabel("Hide Chats")
                            .accessibilityIdentifier("ipad-sidebar-scrim")
                        }
                    }
                    .overlay(alignment: .leading) {
                        VStack(spacing: 0) {
                            if compact { Color.clear.frame(height: 52).accessibilityHidden(true) }
                            SidebarView(library: library, shell: shell, isVisible: sidebarVisible)
                        }
                        .frame(width: sidebarWidth)
                        // No divider: on light photo themes it drew a pale seam
                        // between the sidebar and the chat.
                        .background(theme.sidebar)
                        .offset(x: sidebarVisible ? 0 : -sidebarWidth)
                        .allowsHitTesting(sidebarVisible)
                        .accessibilityHidden(!sidebarVisible)
                    }
                    .background {
                        ShellShortcuts(shell: shell, canStartChat: !library.saved.connections.isEmpty,
                                       animation: SidebarPresentation.animation(reduceMotion: reduceMotion)) {
                            toggleIPadSidebar(compact: compact, tucked: tucked)
                        }
                    }
                    .animation(SidebarPresentation.animation(reduceMotion: reduceMotion), value: shell.sidebarOpen)
                    .animation(SidebarPresentation.animation(reduceMotion: reduceMotion), value: wideHidden)
                    .onChange(of: compact) { _, isCompact in
                        if !isCompact { shell.sidebarOpen = false }
                    }
                }
            } else {
                PhoneDrawerLayout(library: library, shell: shell)
            }
            // Reserve real layout space: the chat's composer intentionally
            // ignores the container's bottom safe area.
            ForEach(library.saved.connections, id: \.credential.hostInstallationId) { saved in
                VoiceSessionBar(controller: library.model(for: saved).voice, scene: shell)
            }
        }
        #if WONDER_DIAGNOSTICS
        .modifier(DiagnosticWindowWidth())
        .background { DiagnosticDriverSurface(surface: driverSurface).frame(width: 0, height: 0).accessibilityHidden(true) }
        #endif
        .background {
            GeometryReader { geometry in
                ShellWindowBoundsReader(layoutSize: geometry.size, size: $windowBounds)
                    .frame(width: 0, height: 0).accessibilityHidden(true)
            }
            .allowsHitTesting(false)
        }
        .environment(\.shellWindowBounds, windowBounds)
        .environment(\.voiceScene, shell)
        .environment(\.compactVoiceBarVisible, compactVoiceBarVisible)
        .onPreferenceChange(VoiceBarVisibilityPreference.self) { compactVoiceBarVisible = $0 }
        .overlay(alignment: .top) {
            InAppBannerOverlay(library: library, current: shell.selectedConversation.map { InAppAlertAddress(host: $0.host, conversation: $0.id) }) { alert in
                // A request opens with its review in front.
                if alert.kind.isRequest { model(alert.address.host)?.attentionFocus = alert.address.conversation }
                shell.open(host: alert.address.host, conversation: alert.address.conversation)
            }
        }
        .onChange(of: shell.route, initial: true) { _, _ in
            InAppBannerCenter.shared.setScreen(sceneID, shell.selectedConversation.map { InAppAlertAddress(host: $0.host, conversation: $0.id) })
        }
        .sheet(isPresented: $pairing) { ConnectionsView(library: library, isSheet: true) }
        .alert("Couldn’t open link", isPresented: Binding(get: { linkIssue != nil }, set: { if !$0 { linkIssue = nil } })) {
            if let retry = linkRetry {
                Button("Try again") { openTarget(retry.0, shortcut: retry.1) }
            }
            Button("OK", role: .cancel) { linkIssue = nil }
        } message: { Text(linkIssue ?? "") }
        .sheet(item: Binding(get: { choosingProjectsHost.map(HostSheet.init) }, set: { choosingProjectsHost = $0?.id })) { sheet in
            if let model = model(sheet.id) {
                AddProjectView(model: model, library: model.projects).onAppear { ProjectOnboardingObserver.acknowledge(sheet.id) }
            }
        }
        .alert("Notification", isPresented: Binding(get: { push.routingError != nil }, set: { if !$0 { push.hideRoutingError() } })) {
            if push.canRetryAction { Button("Retry reply") { push.retryActions() } }
            if push.canRetryOpening { Button(push.openingComputerNotification ? "View computer" : "Open chat") { push.retryOpening() } }
            Button("OK", role: .cancel) { push.dismissRoutingError() }
                .accessibilityIdentifier("notification-error-dismiss")
        } message: { Text(push.routingError ?? "") }
        .onChange(of: push.destination, initial: true) { _, value in
            guard let value else { return }
            if value.macDialog { dialogHost = value.host }
            else { shell.open(host: value.host, conversation: value.conversation) }
        }
        .fullScreenCover(item: Binding(get: { dialogHost.map(HostSheet.init) }, set: { dialogHost = $0?.id })) { host in
            if let model = model(host.id) {
                NavigationStack { // theme-exempt: ComputerSessionView draws theme.computerBackground
                    ComputerSessionView(model: model, chat: ChatSummary(conversationId: "wonder-host-view", title: "Computer",
                        lastMessagePreview: nil, lastMessageAt: nil, messageCount: 0, deliveryState: nil,
                        hasUnread: false, isArchived: false, isPinned: false),
                        onDialogGone: { Task { await push.refreshMacDialogs() } })
                }
            }
        }
        .onOpenURL { openLink($0) }
        .onChange(of: library.shortcutNavigation, initial: true) { _, _ in openShortcut() }
        .onChange(of: library.navigationScene) { _, _ in openShortcut() }
        .onReceive(VoiceLaunch.shared.$requested) { requested in
            // The window in front joins its own chat; another iPad window leaves the request to it.
            guard requested, library.navigationScene.map({ $0 == sceneID }) ?? true else { return }
            VoiceLaunch.shared.requested = false
            startVoiceFromShortcut()
        }
        .sheet(item: Binding(get: { voiceSheetHost.map(HostSheet.init) }, set: { voiceSheetHost = $0?.id })) { sheet in
            if let model = model(sheet.id) { VoiceSheet(controller: model.voice, macName: model.macName, scene: shell) }
        }
        .onChange(of: phase, initial: true) { _, next in
            library.setScene(sceneID, phase: next)
            #if WONDER_DIAGNOSTICS
            Diagnostics.shared.setActive(next == .active)
            #endif
            // Another iPad window may still be in the foreground. Inactive (locking,
            // Notification Center, the app switcher) already counts as away, so the
            // Mac stops keeping the open chat quiet before the app is suspended.
            push.setForeground(next == .active || UIApplication.shared.applicationState == .active)
            if next == .active { push.refresh() }
            AgentLiveActivities.shared.setForeground(next == .active || UIApplication.shared.applicationState == .active)
        }
        .onChange(of: shell.route, initial: true) { previous, route in
            if previous != route { linkTask?.cancel(); linkTask = nil }
            if case .conversation(let host, let id) = route {
                push.setOpenConversation(host: host, id: id)
            } else {
                push.setOpenConversation(host: nil, id: nil)
            }
        }
        .onDisappear {
            linkTask?.cancel(); linkTask = nil
            library.setScene(sceneID, phase: nil)
            InAppBannerCenter.shared.setScreen(sceneID, nil)
        }
        #if WONDER_DIAGNOSTICS
        .onChange(of: library.saved.connections.map { $0.credential.sessionToken }, initial: true) { _, _ in Diagnostics.shared.configure(library.paired) }
        #endif
        .task {
            library.load(); PushNotifications.shared.attach(library); VoiceKeyChoice.shared.attach(library)
            AgentLiveActivities.shared.attach(library)
            #if WONDER_DIAGNOSTICS
            let arguments = ProcessInfo.processInfo.arguments
            if arguments.contains("-diagnostics-notification-question-failure") {
                await push.receiveAction([:], decision: "respond", message: " ")
            }
            if arguments.contains("-diagnostics-notification-instruction-failure") {
                await push.receiveAction([:], decision: "decline", message: " ")
            }
            if arguments.contains("-diagnostics-notification-action-failure") {
                // An unavailable encrypted notification follows the real failure path without a host request.
                await push.receiveAction(["registrationId": "00000000-0000-4000-8000-000000000099",
                    "routeId": "00000000-0000-4000-8000-000000000098"], decision: "accept")
            }
            #endif
            #if DEBUG || WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-show-connections") { pairing = true }
            #endif
        }
        #if WONDER_DIAGNOSTICS
        .task {
            // The layout fixture opens its synthetic conversation as a notification would.
            if DiagnosticSubagentFixture.performanceFixture { shell.sidebarOpen = true }
            else if DiagnosticSubagentFixture.chatLayoutFixture && (!DiagnosticSubagentFixture.projectReadFixture || DiagnosticSubagentFixture.openProjectThreadFixture) {
                shell.open(host: DiagnosticSubagentFixture.hostID, conversation: DiagnosticSubagentFixture.parentID)
            }
            // Select the real shell's list state without relying on Duo HID
            // delivery. Native AX/captures still verify the rendered layout.
            if DiagnosticSubagentFixture.chatLayoutFixture && ProcessInfo.processInfo.arguments.contains("-diagnostics-sidebar-open") {
                shell.sidebarOpen = true
                shell.sidebarOverPane = true
            }
            if DiagnosticSubagentFixture.chatLayoutFixture && (ProcessInfo.processInfo.arguments.contains("-diagnostics-settings-open") || ProcessInfo.processInfo.arguments.contains("-diagnostics-vault-settings")) {
                shell.show(.settings)
            }
            if DiagnosticSubagentFixture.chatLayoutFixture && ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-open") {
                shell.show(.usage)
            }
            if DiagnosticSubagentFixture.chatLayoutFixture && ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-open") {
                shell.show(.automations)
            }
            if DiagnosticSubagentFixture.chatLayoutFixture && ProcessInfo.processInfo.arguments.contains("-diagnostics-manage-projects-open") {
                shell.show(.manageProjects)
            }
            // Edit Project, for the Duo captures that cannot tap through Manage Projects.
            if DiagnosticSubagentFixture.chatLayoutFixture && ProcessInfo.processInfo.arguments.contains("-diagnostics-project-settings-open") {
                shell.show(.projectSettings(host: DiagnosticSubagentFixture.hostID, project: "read-project"))
            }
            // Establish the fixture's default route before applying its cold link.
            // Separate tasks raced: the default A could overwrite a requested B.
            let arguments = ProcessInfo.processInfo.arguments
            if arguments.contains("-diagnostics-needs-attention-open") { shell.showAttention() }
            if let index = arguments.firstIndex(of: "-diagnostics-deep-link"), arguments.indices.contains(index + 1),
               let url = URL(string: arguments[index + 1]) { openLink(url) }
        }
        #endif
        .onChange(of: library.saved.connections.map { $0.credential.deviceId }) { _, _ in PushNotifications.shared.refresh() }
        // Whoever starts the demo (Try a demo, the welcome pages) lands in it;
        // leaving it, or pairing a real Mac, returns to a fresh start.
        .onChange(of: library.isDemo) { _, isDemo in
            if isDemo { shell.showDemo() } else { shell.newChat() }
        }
        .background {
            ForEach(library.saved.connections, id: \.credential.hostInstallationId) { saved in
                let model = library.model(for: saved)
                if library.foregroundOwner == sceneID {
                    ConnectionLifecycle(model: model).id(ObjectIdentifier(model))
                }
                ProjectOnboardingObserver(model: model, library: model.projects) { choosingProjectsHost = $0 }
                Color.clear.frame(width: 0, height: 0)
                    .onChange(of: shell.route, initial: true) { _, _ in
                        let selected = shell.selectedConversation
                        model.setHistoryVisible(selected?.host == saved.credential.hostInstallationId ? selected?.id : nil, window: sceneID)
                    }
                    .onDisappear { model.setHistoryVisible(nil, window: sceneID) }
                    .id(ObjectIdentifier(model))
            }
        }
    }
    /// A narrow iPad window opens the sidebar over the conversation; a wide one
    /// hides or shows it beside the conversation and remembers the choice.
    private func toggleIPadSidebar(compact: Bool, tucked: Bool) {
        withAnimation(SidebarPresentation.animation(reduceMotion: reduceMotion)) {
            if compact { shell.sidebarOpen.toggle() } else { shell.toggleWideSidebar(hidden: $wideSidebarHidden, tucked: tucked) }
        }
    }
    #if WONDER_DIAGNOSTICS
    private var driverSurface: DiagnosticTestDriver.Surface {
        let covered = shell.settingsOpen || shell.sidebarOpen || pairing || choosingProjectsHost != nil || dialogHost != nil || linkIssue != nil || push.routingError != nil
        return .init(priority: covered ? 30 : 0, state: {
            var state: [String: Any] = ["sidebarOpen": shell.sidebarOpen, "settingsOpen": shell.settingsOpen,
                "selection": shell.selectedConversation.map { ["host": $0.host, "id": $0.id] } ?? NSNull()]
            if shell.selectedConversation == nil { state["route"] = "new-chat" }
            if covered {
                state["route"] = shell.settingsOpen ? "settings" : "chats"
                state["thread"] = NSNull(); state["timeline"] = []; state["composer"] = NSNull()
                state["sheets"] = [(pairing, "connections"), (choosingProjectsHost != nil, "add-project"), (dialogHost != nil, "computer")].filter { $0.0 }.map { $0.1 }
                state["alerts"] = [linkIssue, push.routingError].compactMap { $0 }
            }
            return state
        }, controls: {
            if push.routingError != nil {
                return [.init(id: "notification-error-dismiss", label: "OK", tap: { push.dismissRoutingError(); push.hideRoutingError() })]
            }
            let voiceControls = library.saved.connections.flatMap { saved -> [DiagnosticTestDriver.Control] in
                let voice = library.model(for: saved).voice
                guard shell.sidebarOpen, voice.showsCompactControls(in: shell) else { return [] }
                return voice.diagnosticControls(scene: shell, compact: true)
            }
            return [.init(id: "open-sidebar", label: "Show Chats", tap: { shell.showChats() }),
                    .init(id: "sidebar-new-chat", label: "New chat", tap: { shell.newChat() })] + voiceControls
        }, actions: [
            "open-thread": { arguments in
                guard !covered, let id = arguments["id"] else { throw DiagnosticTestDriver.DriverError("Navigation is covered") }
                let host = arguments["host"] ?? ""
                let candidates = library.saved.connections.filter { host.isEmpty || $0.credential.hostInstallationId == host }
                    .filter { saved in
                        // With an explicit host, ShellConversation resolves the ID
                        // through the same authenticated Project-detail path as a link.
                        // Project threads need not appear in the legacy chat list.
                        if !host.isEmpty { return true }
                        let model = library.model(for: saved)
                        return model.projects.details[id] != nil || model.chats.contains { $0.id == id }
                            || model.projects.threads.values.contains { $0.threads.contains { $0.conversationId == id } }
                    }
                guard candidates.count == 1, let saved = candidates.first else { throw DiagnosticTestDriver.DriverError("Thread unavailable or host ambiguous") }
                shell.open(host: saved.credential.hostInstallationId, conversation: id)
            },
            "dismiss": { _ in
                guard covered else { throw DiagnosticTestDriver.DriverError("No presentation to dismiss") }
                shell.sidebarOpen = false; shell.settingsOpen = false; pairing = false
                choosingProjectsHost = nil; dialogHost = nil; linkIssue = nil
                if push.routingError != nil { push.dismissRoutingError() }
            }
        ])
    }
    #endif

    private func model(_ host: String) -> ConnectionModel? {
        library.saved.connections.first { $0.credential.hostInstallationId == host }.map { library.model(for: $0) }
    }

    /// The Action button or Shortcuts: voice in this window's open conversation,
    /// or on its own over whatever is on screen.
    private func startVoiceFromShortcut() {
        library.load()
        let models = library.saved.connections.map { library.model(for: $0) }
        switch VoiceShortcutTarget.resolve(models, shell: shell, lastHost: NewChatDraftStore.lastHost) {
        case .resume(let model):
            if model.voice.chatID == nil { voiceSheetHost = model.connection?.credential.hostInstallationId }
        case .chat(let model, let chat):
            model.voice.start(chat: chat, scene: shell)
        case .standalone(let model):
            voiceSheetHost = model.connection?.credential.hostInstallationId
            model.voice.start(chat: nil, scene: shell)
        case .unpaired:
            linkIssue = "Pair a computer with Cupola to use voice."
        }
    }

    private func openLink(_ url: URL) {
        linkRetry = nil
        // Keychain loading can complete after the URL arrives on a cold launch.
        library.load()
        guard library.loaded else {
            linkIssue = "Saved computers are unavailable. Unlock your device and try again."
            return
        }
        let scheme = Bundle.main.object(forInfoDictionaryKey: "WonderDeepLinkScheme") as? String ?? ""
        guard let target = WonderDeepLink.parse(url, scheme: scheme) else {
            linkIssue = "This Cupola link isn’t valid. Ask for a new link."
            return
        }
        if case .liveActivityRoute(let host, let registration, let route) = target {
            // Resolved and opened like a notification tap.
            PushNotifications.shared.openLiveActivityRoute(host: host, registration: registration, route: route)
            return
        }
        openTarget(target)
    }

    private func openShortcut() {
        do {
            guard let request = try library.takeShortcut(scene: sceneID) else { return }
            openTarget(request.target, shortcut: request.address)
        } catch { linkIssue = error.localizedDescription }
    }

    private func openTarget(_ target: WonderDeepLink, shortcut: ProjectShortcutAddress? = nil) {
        linkRetry = nil
        linkTask?.cancel(); linkTask = nil
        if target == .attention { shell.showAttention(); linkIssue = nil; return }
        guard let linkedModel = model(target.host),
              shortcut == nil || linkedModel.connection?.credential.deviceId == shortcut?.device else {
            linkIssue = "The linked computer is not paired on this device. Pair that computer to open it."
            return
        }
        #if WONDER_DIAGNOSTICS
        if library.isPreview && ProcessInfo.processInfo.arguments.contains("-diagnostics-deep-link-offline") {
            linkedModel.macConnected = false
        }
        #endif
        switch target {
        case .attention: shell.showAttention()
        case .chat(let host, let id):
            if let shortcut {
                let scope = linkedModel.assignmentScope
                linkTask = Task {
                    do { try await finishOpening(id, model: linkedModel, scope: scope, shortcut: shortcut) }
                    catch {
                        guard !Task.isCancelled, linkedModel.assignmentScope == scope else { return }
                        let issue = ChatOpenFailure(error, computer: linkedModel.macName)
                        linkIssue = issue.message
                        linkRetry = issue.canRetry ? (target, shortcut) : nil
                    }
                }
            } else { shell.open(host: host, conversation: id) }
        case .newProjectChat(let host, let project):
            shell.newChat(host: host, destination: .project(id: project), exactProject: true)
        case .computer(let host):
            shell.newChat(host: host, showComputer: true)
        case .projectThread(_, let project, let family, let thread):
            let summary = ProjectThreadSummary(reference: family.rawValue + ":" + thread, conversationId: nil,
                                               title: "", family: family, updatedAt: 0)
            let scope = linkedModel.assignmentScope
            linkTask = Task {
                do {
                    let conversation = try await linkedModel.projects.attach(project, thread: summary)
                    try await finishOpening(conversation, model: linkedModel, scope: scope, shortcut: shortcut)
                } catch is CancellationError {
                } catch {
                    guard !Task.isCancelled, linkedModel.assignmentScope == scope else { return }
                    let issue = ChatOpenFailure(error, computer: linkedModel.macName)
                    linkIssue = issue.message
                    linkRetry = issue.canRetry ? (target, shortcut) : nil
                }
            }
        case .liveActivityRoute(let host, let registration, let route):
            PushNotifications.shared.openLiveActivityRoute(host: host, registration: registration, route: route)
        }
        linkIssue = nil
    }

    private func finishOpening(_ conversation: String, model linkedModel: ConnectionModel,
                               scope: String, shortcut: ProjectShortcutAddress?) async throws {
        if let shortcut {
            let detail = try await linkedModel.projects.loadDetail(conversation)
            guard shortcut.matches(detail, conversation: conversation) else { throw ProjectShortcutFailure.thread }
        }
        guard !Task.isCancelled, let host = linkedModel.connection?.credential.hostInstallationId,
              model(host) === linkedModel, !linkedModel.accessEnded, linkedModel.assignmentScope == scope else {
            throw CancellationError()
        }
        linkedModel.projects.noteOpened(conversation)
        shell.open(host: host, conversation: conversation)
    }
}

/// Where the Action button or a Shortcut starts voice. A running session keeps
/// going. Otherwise voice joins the chat the invoking window shows (an agent task
/// open over it included); from New chat or a management screen it starts on its
/// own, on the computer used last when that one answers.
enum VoiceShortcutTarget {
    case resume(ConnectionModel)
    case chat(ConnectionModel, ChatSummary)
    case standalone(ConnectionModel)
    case unpaired

    @MainActor static func resolve(_ models: [ConnectionModel], shell: ShellState, lastHost: String?) -> Self {
        if let model = models.first(where: { $0.voice.isActive }) { return .resume(model) }
        func host(_ model: ConnectionModel) -> String? { model.connection?.credential.hostInstallationId }
        if shell.page == nil, let selected = shell.selectedConversation,
           let model = models.first(where: { host($0) == selected.host }) {
            // Still loading: stay on the computer this window shows.
            return model.shownChat(in: selected.id).map { .chat(model, $0) } ?? .standalone(model)
        }
        let last = lastHost.flatMap { wanted in models.first { host($0) == wanted } }
        if let last, last.macConnected == true { return .standalone(last) }
        guard let model = models.first(where: { $0.macConnected == true }) ?? last ?? models.first else { return .unpaired }
        return .standalone(model)
    }
}

private struct HostSheet: Identifiable { let id: String }

/// Shared by the iPhone and iPad layouts: whether a regular-width sidebar is
/// hidden (remembered across launches), and how the sidebar moves.
enum SidebarPresentation {
    static let hiddenKey = "wonder.sidebar.hidden"
    static func animation(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .snappy(duration: 0.3)
    }
}

/// The window's hardware-keyboard shortcuts: ⌘⇧S shows or hides the sidebar,
/// ⌘N starts a new chat, ⌘F searches Chats and ⌘, opens Settings. Zero-size
/// buttons keep them registered (and listed in the iPad shortcut overlay)
/// whichever layout shows. A focused editor or terminal is earlier in the
/// responder chain, so its own key commands still come first.
private struct ShellShortcuts: View {
    let shell: ShellState
    /// New chat needs a computer, as the sidebar's button does.
    let canStartChat: Bool
    /// How the sidebar moves in this layout.
    let animation: Animation?
    let toggleSidebar: () -> Void
    var body: some View {
        ZStack {
            Button("Toggle Sidebar", action: toggleSidebar)
                .keyboardShortcut("s", modifiers: [.command, .shift])
            Button("New Chat") { shell.newChat() }
                .keyboardShortcut("n", modifiers: .command)
                .disabled(!canStartChat)
            Button("Search Chats") { withAnimation(animation) { shell.searchChats() } }
                .keyboardShortcut("f", modifiers: .command)
            Button("Settings") { shell.show(.settings) }
                .keyboardShortcut(",", modifiers: .command)
        }
        .frame(width: 0, height: 0).opacity(0)
        .accessibilityHidden(true)
    }
}

/// How the compact iPhone sidebar moves. The sidebar stays put underneath and the
/// conversation slides aside over it like a card. Values follow ChatGPT's iOS side
/// menu (`ModernSideMenuContainerViewController`, build 2026.244), read from its
/// binary: a 300 pt menu (320 pt when the shorter screen side is 428 pt or more),
/// the menu scaling from 0.97 and fading in over the first 200 pt, a light-only
/// leading shadow (opacity 0.15, radius 14, 2 pt to the leading side) and a
/// critically damped spring (stiffness 986.96, damping 62.83: response 0.2 s).
/// ChatGPT rounds the moved conversation with the display's corner radius, which has
/// no public API; Wonder uses a fixed continuous radius close to it.
enum SidebarCard {
    /// At accessibility text sizes the sidebar takes all but a 56 pt strip of the
    /// conversation, so names have room to wrap.
    static func sidebarWidth(for size: CGSize, accessibilityText: Bool = false) -> CGFloat {
        let standard = min(size.width, min(size.width, size.height) < 428 ? 300 : 320)
        return accessibilityText ? max(standard, size.width - 56) : standard
    }
    /// Beside the conversation, the one owner for iPad, a regular-width iPhone and
    /// an unfolded Duo: 27% of the window between 240 and 320 pt (an iPad's
    /// windows: 27% between 344 and 380 pt; an unfolded Duo at least 344 pt so the
    /// selection toolbar and computer pills fit), or 42% up to 420 pt at accessibility
    /// text sizes. Every wide window keeps at least 360 pt for the conversation.
    static func regularWidth(for size: CGSize, accessibilityText: Bool = false, hasDisplayFold: Bool = false,
                             pad: Bool = false) -> CGFloat {
        let standard = accessibilityText ? min(420, max(320, size.width * 0.42))
            : pad ? min(380, max(344, size.width * 0.27))
            : min(320, max(240, size.width * 0.27))
        // The inner display needs room for titles and computer filters. Preserve
        // a readable chat column when the unfolded window is upright.
        let wider = max(344, standard)
        return hasDisplayFold && size.width - wider >= 360 ? wider : standard
    }
    /// An iPad window narrower than this opens Chats over the conversation.
    static let padCompactWidth: CGFloat = 760
    /// Chats over a narrow iPad window, leaving a strip of the conversation.
    static func padDrawerWidth(for size: CGSize) -> CGFloat { min(size.width * 0.85, 420) }
    static let revealDistance: CGFloat = 200
    static let sidebarStartScale: CGFloat = 0.97
    static let cornerRadius: CGFloat = 55
    static let shadowOpacity = 0.15
    static let shadowRadius: CGFloat = 14
    static let shadowOffset: CGFloat = -2
    static let stiffness = 986.96
    static let damping = 62.832

    /// The settle after a drag or a button: ChatGPT's spring, carrying the finger's
    /// speed (`velocity` in points per second over `distance` points still to go).
    /// Reduce Motion replaces the slide with a quick fade.
    static func animation(reduceMotion: Bool, velocity: CGFloat = 0, distance: CGFloat = 0) -> Animation {
        if reduceMotion { return .easeOut(duration: 0.15) }
        let relative = abs(distance) > 1 ? Double(velocity / distance) : 0
        return .interpolatingSpring(mass: 1, stiffness: stiffness, damping: damping, initialVelocity: relative)
    }
}

/// The iPhone layout. In compact width the sidebar lies underneath and opening
/// it slides the conversation aside like a card, rounded and shadowed, leaving a
/// strip of it that closes the sidebar when tapped or swiped. In regular width (a
/// large iPhone in landscape, an unfolded Duo) the sidebar sits beside the
/// conversation and can be hidden. The navigation stack remains mounted as the
/// size class changes. The sidebar and the conversation are built here, where
/// nothing changes per frame; the drag lives in `DrawerCard`, so a finger moving
/// the card changes only its offsets, corners and opacity and never rebuilds the
/// conversation behind it.
struct PhoneDrawerLayout: View {
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @Environment(\.dynamicTypeSize) private var typeSize
    @AppStorage(SidebarPresentation.hiddenKey) private var wideSidebarHidden = false
    /// Held, not observed: only `DrawerCard` redraws as the finger moves.
    @State private var drag = DrawerDrag()
    @State private var verticalBar = false
    @State private var barEpoch = 0
    @AccessibilityFocusState private var sidebarButtonFocused: Bool

    private var wideAnimation: Animation? { SidebarPresentation.animation(reduceMotion: reduceMotion) }
    private var cardAnimation: Animation { SidebarCard.animation(reduceMotion: reduceMotion) }

    var body: some View {
        NavigationStack { // theme-exempt: stable shell navigation owns the Duo rail
            GeometryReader { geometry in
                let wide = horizontalSizeClass == .regular
                let displayPanels = CompanionPaneLayout.displayPanels(size: geometry.size, fold: DuoFold.region(in: geometry))
                let hasFold = displayPanels != nil
                let coverDrawer = verticalBar && !wide
                let width = wide ? SidebarCard.regularWidth(for: geometry.size, accessibilityText: typeSize.isAccessibilitySize, hasDisplayFold: hasFold)
                    : coverDrawer ? geometry.size.width
                    : SidebarCard.sidebarWidth(for: geometry.size, accessibilityText: typeSize.isAccessibilitySize)
                let drawerOpen = !wide && shell.sidebarOpen
                // A tool beside the chat may need the sidebar's room, or the whole
                // window to split on an unfolded Duo's fold.
                let tucked = wide && shell.sidebarTucked(window: geometry.size, sidebarWidth: width,
                                                        hasFold: hasFold)
                let wideHidden = wideSidebarHidden || tucked
                let sidebarShown = wide ? !wideHidden : drawerOpen
                ZStack(alignment: .topLeading) {
                    ShellShortcuts(shell: shell, canStartChat: !library.saved.connections.isEmpty,
                                   animation: wide ? wideAnimation : Optional(cardAnimation)) { toggle(wide: wide, tucked: tucked) }
                    DrawerCard(drag: drag, shell: shell, sidebarOpen: shell.sidebarOpen,
                               edgeOpens: shell.page == nil, wide: wide, coverDrawer: coverDrawer,
                               width: width, wideHidden: wideHidden, sidebarShown: sidebarShown,
                               window: geometry.size, insets: geometry.safeAreaInsets, displayPanels: displayPanels) {
                        SidebarView(library: library, shell: shell, isVisible: sidebarShown,
                                    avoidsDisplayCorners: verticalBar || hasFold)
                    } conversation: {
                        ShellMain(library: library, shell: shell, compactIPadWindow: false,
                                  showSidebarButton: false,
                                  sidebarButtonLabel: sidebarShown ? "Hide Chats" : "Show Chats",
                                  adaptsToDisplayBars: verticalBar || hasFold) {
                            toggle(wide: wide, tucked: tucked)
                        }
                        .environment(\.shellWindowSize, geometry.size)
                        .environment(\.shellBarEpoch, barEpoch)
                        .environment(\.shellChatsReplaceTool, hasFold && sidebarShown)
                        .environment(\.shellDisplayFold, hasFold)
                        .environment(\.shellChatsCoverConversation, coverDrawer && drawerOpen)
                    }
                }
                #if WONDER_DIAGNOSTICS
                .background { DuoGeometryEvidence(geometry: geometry) }
                .onChange(of: "wide=\(wide) bar=\(verticalBar) open=\(shell.sidebarOpen) hidden=\(wideSidebarHidden) tucked=\(tucked) pane=\(shell.companionPaneOpen) size=\(Int(geometry.size.width))x\(Int(geometry.size.height)) toolbarHidden=\(drawerOpen && !coverDrawer)", initial: true) { _, line in
                    DuoShellTrail.record(line)
                }
                #endif
                .animation(wideAnimation, value: wideHidden)
                .onChange(of: geometry.size.height < 1) { collapsed, nowCollapsed in
                    // Unfolding with the keyboard up takes the shell's height to 0 for a few frames, and the
                    // vertical bar can return from it with its items' hosts empty (measured, about half the
                    // runs). Items under a new id are hosted afresh.
                    if collapsed, !nowCollapsed { barEpoch += 1 }
                }
                .toolbar {
                    // The one site that adds the Chats button on a phone, each form under its own id
                    // (the same id from two views, swapped on a `verticalBar` change, could drop it).
                    if verticalBar {
                        ToolbarItem(id: "shell-sidebar-rail-\(barEpoch)", placement: .topBarLeading) {
                            PhoneSidebarButton(inBar: true, shown: sidebarShown, wide: wide,
                                               focused: $sidebarButtonFocused) { toggle(wide: wide, tucked: tucked) }
                        }
                        .duoRail(true)
                    } else {
                        ToolbarItem(id: "shell-sidebar-\(barEpoch)", placement: .topBarLeading) {
                            PhoneSidebarButton(inBar: false, shown: sidebarShown, wide: wide,
                                               focused: $sidebarButtonFocused) { toggle(wide: wide, tucked: tucked) }
                        }
                    }
                }
                .navigationTitle("")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar(drawerOpen && !coverDrawer ? .hidden : .automatic, for: .navigationBar)
            }
        }
        .readsVerticalBar($verticalBar)
        // Pushed destinations inherit from the owning stack, not from the
        // Settings root that initiated the push.
        .environment(\.settingsInlineNavigation, verticalBar)
        .onChange(of: shell.sidebarOpen) { _, open in
            if !open, shell.page == nil { sidebarButtonFocused = true }
        }
    }

    private func toggle(wide: Bool, tucked: Bool) {
        if wide {
            withAnimation(wideAnimation) { shell.toggleWideSidebar(hidden: $wideSidebarHidden, tucked: tucked) }
        } else {
            withAnimation(cardAnimation) { shell.sidebarOpen.toggle(); drag.offset = 0 }
        }
    }
}

#if WONDER_DIAGNOSTICS
/// Diagnostics builds only: `duo-shell-state.log` in the app's tmp trails the shell's posture state per change,
/// and 1.5 s after the last change the navigation bar's views (class, frame, hidden, alpha), so a rail that went
/// missing can be read back from the simulator's container. Bounded to 64 KB (the older half is dropped).
@MainActor enum DuoShellTrail {
    private static let url = FileManager.default.temporaryDirectory.appendingPathComponent("duo-shell-state.log")
    private static var pending: Task<Void, Never>?

    static func record(_ line: String) {
        append("\(Date().timeIntervalSince1970) \(line)\n")
        pending?.cancel()
        pending = Task { @MainActor in
            do { try await Task.sleep(for: .milliseconds(1500)) } catch { return }
            append("\(Date().timeIntervalSince1970) bars:\n" + barViews().joined(separator: "\n") + "\n")
        }
    }

    private static func append(_ text: String) {
        if let size = (try? FileManager.default.attributesOfItem(atPath: url.path)[.size] as? Int), size > 64_000,
           let old = try? Data(contentsOf: url) {
            try? old.suffix(32_000).write(to: url)
        }
        if let handle = try? FileHandle(forWritingTo: url) { handle.seekToEndOfFile(); handle.write(Data(text.utf8)); try? handle.close() }
        else { try? text.write(to: url, atomically: true, encoding: .utf8) }
    }

    private static func barViews() -> [String] {
        var lines: [String] = []
        let windows = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.flatMap(\.windows)
        func walk(_ view: UIView, depth: Int) {
            let name = String(describing: type(of: view))
            let relevant = name.contains("Bar") || name.contains("Toolbar") || name.contains("Item")
            if relevant {
                lines.append("\(String(repeating: " ", count: depth))\(name) \(view.frame.integral) hidden=\(view.isHidden) alpha=\(view.alpha) subviews=\(view.subviews.count)")
            }
            guard depth < 14 else { return }
            for child in view.subviews { walk(child, depth: depth + (relevant ? 1 : 0)) }
        }
        for window in windows { walk(window, depth: 0) }
        return Array(lines.prefix(80))
    }
}
#endif

/// The phone shell's Chats button. In the Duo's vertical bar it is one of the
/// rail's 44 pt surfaces; elsewhere it is the system's own toolbar button.
private struct PhoneSidebarButton: View {
    let inBar: Bool
    let shown: Bool
    let wide: Bool
    var focused: AccessibilityFocusState<Bool>.Binding
    let action: () -> Void

    var body: some View {
        let icon = inBar || wide ? "sidebar.left" : "line.3.horizontal"
        Group {
            if inBar {
                Button(action: action) {
                    Label("Chats", systemImage: icon).labelStyle(.iconOnly)
                        .modifier(RailButtonSurface(inBar: true, selected: shown))
                }
                .buttonStyle(.plain)
                .environment(\.composerPillsInBar, true)
                .accessibilityValue(shown ? "Open" : "Closed")
                .accessibilityAddTraits(shown ? .isSelected : [])
            } else {
                // A native label keeps the item across the Duo's bar-axis changes.
                Button("Chats", systemImage: icon, action: action).labelStyle(.iconOnly)
            }
        }
        .accessibilityLabel(shown ? "Hide Chats" : "Show Chats")
        .accessibilityIdentifier("open-sidebar")
        .accessibilityFocused(focused)
    }
}

/// A sidebar drag in progress. Only `DrawerCard` observes it, so the views that
/// build the sidebar and the conversation are not asked to update per frame.
@MainActor final class DrawerDrag: ObservableObject {
    /// The finger's horizontal travel while a drag opens or closes the sidebar.
    @Published var offset: CGFloat = 0
    @Published var engaged = false
    /// The sidebar width of the last drag, for a settle that has no gesture geometry.
    var width: CGFloat = 0
}

/// Places the sidebar and the conversation and moves them under the finger. Both
/// arrive as finished values, so a drag frame redraws only this view's offsets,
/// corners and opacity; SwiftUI finds the stored content unchanged and leaves it.
struct DrawerCard<Sidebar: View, Conversation: View>: View {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject var drag: DrawerDrag
    /// Read and written when a drag starts or settles; the body draws `sidebarOpen`.
    let shell: ShellState
    let sidebarOpen: Bool
    /// The leading edge opens the sidebar only while no page covers the chat.
    let edgeOpens: Bool
    let wide: Bool
    let coverDrawer: Bool
    let width: CGFloat
    let wideHidden: Bool
    let sidebarShown: Bool
    let window: CGSize
    let insets: EdgeInsets
    let displayPanels: CompanionPaneLayout?
    let sidebar: Sidebar
    let conversation: Conversation
    /// True while a finger is down, so a cancelled drag still settles.
    @GestureState private var touching = false
    /// Compact: the closed sidebar waits off screen once the card has settled over it.
    @State private var sidebarParked = true

    private static var edgeWidth: CGFloat { 20 }
    private static var navigationBarHeight: CGFloat { 44 }
    private static var commitFraction: CGFloat { 0.4 }
    private static var commitVelocity: CGFloat { 500 }
    private var cardAnimation: Animation { SidebarCard.animation(reduceMotion: reduceMotion) }

    init(drag: DrawerDrag, shell: ShellState, sidebarOpen: Bool, edgeOpens: Bool, wide: Bool,
         coverDrawer: Bool, width: CGFloat, wideHidden: Bool, sidebarShown: Bool, window: CGSize,
         insets: EdgeInsets, displayPanels: CompanionPaneLayout? = nil, @ViewBuilder sidebar: () -> Sidebar, @ViewBuilder conversation: () -> Conversation) {
        _drag = ObservedObject(wrappedValue: drag); self.shell = shell; self.sidebarOpen = sidebarOpen; self.edgeOpens = edgeOpens
        self.wide = wide; self.coverDrawer = coverDrawer; self.width = width; self.wideHidden = wideHidden
        self.sidebarShown = sidebarShown; self.window = window; self.insets = insets
        self.displayPanels = displayPanels
        // Built once per update of the layout above, never per drag frame.
        self.sidebar = sidebar(); self.conversation = conversation()
    }

    var body: some View {
        let drawerOpen = !wide && sidebarOpen
        // How far the conversation has moved aside (compact) or the sidebar's
        // visible width (regular).
        let reveal = wide ? (wideHidden ? 0 : width)
            : min(width, max(0, (drawerOpen ? width : 0) + drag.offset))
        // The card treatment reaches full strength over the first 200 pt.
        let emphasis = wide ? 0 : min(1, reveal / min(width, SidebarCard.revealDistance))
        // With Chats hidden the conversation spans both displays, or an open tool
        // divides the whole container itself; panels apply only while Chats shows.
        // On an unfolded Duo with a vertical hinge the conversation (alone or beside
        // a tool) sits on the trailing display with Chats on the leading one, so nothing of it
        // crosses the hinge; a pure function of the layout, so closing a tool
        // restores it. Other pages (Search, Settings) keep Chats at its content
        // width: reserving a display left an artificial band beside Search.
        // A tool temporarily replaced by Chats still retains its fold slot.
        let panels = wide && !wideHidden && (shell.companionPaneOpen || (shell.page == nil && displayPanels?.direction == .sideBySide)) ? displayPanels : nil
        let parked = !wide && sidebarParked && !drawerOpen && !drag.engaged
        ZStack(alignment: .topLeading) {
            // Compact: the sidebar's colour fills the screen under the card, so the
            // sidebar can settle from slightly smaller without showing an edge.
            if !wide { theme.sidebar.ignoresSafeArea() }
            // Each display fills right up to the hinge: its own colour reaches the middle of
            // the reserved region, so only the physical gap shows as empty.
            if let panels {
                let middle = (panels.chat.maxX + panels.pane.minX) / 2
                theme.sidebar.frame(width: max(0, middle - panels.chat.maxX), height: panels.chat.height + insets.top + insets.bottom)
                    .offset(x: panels.chat.maxX, y: panels.chat.minY - insets.top)
                    .accessibilityHidden(true).allowsHitTesting(false)
                theme.page.frame(width: max(0, panels.pane.minX - middle), height: panels.pane.height + insets.top + insets.bottom)
                    .offset(x: middle, y: panels.pane.minY - insets.top)
                    .accessibilityHidden(true).allowsHitTesting(false)
            }
            sidebar
                .frame(width: panels?.chat.width ?? width, height: panels?.chat.height)
                .frame(maxHeight: panels == nil ? .infinity : nil)
                .background(theme.sidebar)
                // A horizontal dismissal must cancel a pressed row rather than open it.
                .disabled(drag.engaged)
                .accessibilityElement(children: .contain)
                .accessibilityIdentifier(coverDrawer ? "cover-chats" : "chats-sidebar")
                .accessibilityAction(.escape) { if !wide { close() } }
                // Compact and closed: parked off screen, so its rows are not on screen
                // under the conversation; it returns, unanimated, as the card starts to move.
                .offset(x: panels?.chat.minX ?? (wide ? reveal - width : (parked ? -(width + 40) : 0)),
                        y: panels?.chat.minY ?? 0)
                .transaction(value: parked) { $0.animation = nil }
                // Compact: fixed underneath, settling from slightly smaller and faded.
                .scaleEffect(wide || reduceMotion ? 1
                             : SidebarCard.sidebarStartScale + (1 - SidebarCard.sidebarStartScale) * emphasis)
                // Faded in under a cover of its own colour, as ChatGPT does, so the list
                // itself stays opaque and keeps its accessibility elements.
                .overlay {
                    if !wide {
                        theme.sidebar.opacity(1 - emphasis).ignoresSafeArea()
                            .allowsHitTesting(false).accessibilityHidden(true)
                    }
                }
                .allowsHitTesting(sidebarShown)
                .accessibilityHidden(!sidebarShown)
                .simultaneousGesture(closeGesture(enabled: !wide))
            conversation
                // Regular width shares the window; compact keeps the full width and moves aside.
                .frame(width: panels?.pane.width ?? (wide ? max(0, window.width - reveal) : window.width), height: panels?.pane.height)
                .frame(maxHeight: panels == nil ? .infinity : nil)
                .clipShape(CardShape(cornerRadius: wide ? 0 : SidebarCard.cornerRadius * emphasis,
                                     insets: wide ? EdgeInsets(top: 1000, leading: 1000, bottom: 1000, trailing: 1000) : insets))
                // The card: an opaque page under the conversation (so the sidebar never
                // shows through), rounded and shadowed only while it is moved aside.
                .background {
                    if !wide {
                        CardShape(cornerRadius: SidebarCard.cornerRadius * emphasis, insets: insets)
                            .fill(theme.page)
                            .shadow(color: .black.opacity(theme.isDark ? 0 : SidebarCard.shadowOpacity * emphasis),
                                    radius: SidebarCard.shadowRadius, x: SidebarCard.shadowOffset)
                    }
                }
                // UIKit text inputs can remain in AX despite an ancestor's
                // opacity/accessibilityHidden. Park the whole page beyond the
                // window, including the system bar's trailing safe area.
                .offset(x: panels?.pane.minX ?? (coverDrawer && drawerOpen ? window.width + insets.leading + insets.trailing : reveal),
                        y: panels?.pane.minY ?? 0)
                // Reduce Motion: the conversation steps aside without sliding; the sidebar fades.
                .transaction(value: sidebarOpen) { transaction in
                    if reduceMotion && !wide { transaction.animation = nil }
                }
                // Background controls are inert while the sidebar is open.
                .accessibilityHidden(drawerOpen)
                .allowsHitTesting(!drawerOpen)
                .overlay(alignment: .leading) {
                    // Only the leading edge below the navigation bar opens the sidebar, so
                    // horizontal scrolling in the conversation and the header buttons keep working.
                    if !wide && !drawerOpen && edgeOpens {
                        Color.clear.frame(width: Self.edgeWidth).contentShape(Rectangle())
                            .padding(.top, Self.navigationBarHeight)
                            .gesture(openGesture())
                            .accessibilityHidden(true)
                    }
                }
            // The strip of the moved conversation: tapping or swiping it closes the sidebar.
            if !wide && !coverDrawer && (drawerOpen || drag.engaged) {
                Color.clear
                    .frame(width: max(0, window.width - reveal))
                    .frame(maxHeight: .infinity)
                    .ignoresSafeArea(edges: .vertical)
                    .contentShape(Rectangle())
                    .offset(x: reveal)
                    .onTapGesture { close() }
                    .simultaneousGesture(closeGesture())
                    .accessibilityElement()
                    .accessibilityLabel("Hide Chats")
                    .accessibilityAddTraits(.isButton)
                    .accessibilityAction { close() }
                    .accessibilityIdentifier("sidebar-scrim")
                    .allowsHitTesting(drawerOpen)
            }
        }
        .frame(width: window.width, height: window.height, alignment: .topLeading)
        // Opening or closing from elsewhere (choosing a thread, the Chats button)
        // settles like a button press; a drag's settle keeps its own spring.
        .transaction(value: sidebarOpen) { transaction in
            guard !wide else { return }
            if transaction.animation == nil && !transaction.disablesAnimations {
                transaction.animation = cardAnimation
            }
            transaction.addAnimationCompletion(criteria: .removed) { parkIfClosed() }
        }
        // Opening a page (a menu choice, a row) ends any drag the card still remembers.
        .onChange(of: shell.page) { _, _ in
            if !shell.sidebarOpen, drag.offset != 0 || drag.engaged { drag.offset = 0; drag.engaged = false }
        }
        .onChange(of: wide) { _, isWide in
            drag.offset = 0
            drag.engaged = false
            if isWide { shell.sidebarOpen = false }
        }
        .onChange(of: sidebarOpen) { _, open in
            if open { sidebarParked = false; dismissKeyboard() }
        }
        .onChange(of: drag.engaged) { _, engaged in
            guard engaged else { return }
            sidebarParked = false
            // The keyboard does not travel with the conversation; put it away as a drag starts.
            if !shell.sidebarOpen { dismissKeyboard() }
        }
        .onChange(of: touching) { _, isTouching in
            // A system-cancelled drag delivers no end; put the sidebar where it belongs.
            if !isTouching, drag.engaged { settle(width: drag.width, translation: drag.offset, velocity: 0) }
        }
    }

    private func parkIfClosed() {
        if !shell.sidebarOpen && !drag.engaged && drag.offset == 0 { sidebarParked = true }
    }

    private func dismissKeyboard() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
    }

    private func close() {
        withAnimation(cardAnimation) { shell.sidebarOpen = false; drag.offset = 0 }
    }

    /// Follows the finger. Writes only what changed: each write asks this view to redraw.
    private func track(_ travel: CGFloat) {
        if !drag.engaged { drag.engaged = true }
        drag.width = width
        if drag.offset != travel { drag.offset = travel }
    }

    // The sidebar, the strip and the conversation move under the finger, so
    // translations are measured in the window rather than in a moving view.
    private func openGesture() -> some Gesture {
        DragGesture(minimumDistance: 6, coordinateSpace: .global)
            .updating($touching) { _, state, _ in state = true }
            .onChanged { value in
                guard !shell.sidebarOpen else { return }
                let travel = value.translation
                if drag.engaged || abs(travel.width) > abs(travel.height) {
                    track(max(0, min(travel.width, width)))
                }
            }
            .onEnded { value in settle(width: width, translation: value.translation.width, velocity: value.velocity.width) }
    }

    private func closeGesture(enabled: Bool = true) -> some Gesture {
        DragGesture(minimumDistance: 10, coordinateSpace: .global)
            .updating($touching) { _, state, _ in state = true }
            .onChanged { value in
                guard enabled, shell.sidebarOpen else { return }
                let travel = value.translation
                if drag.engaged || (travel.width < 0 && abs(travel.width) > abs(travel.height) * 1.5) {
                    track(max(-width, min(0, travel.width)))
                }
            }
            .onEnded { value in settle(width: width, translation: value.translation.width, velocity: value.velocity.width) }
    }

    /// Commits on a quick flick or once the conversation moved past 40% of its travel,
    /// then springs on from the finger's speed.
    private func settle(width: CGFloat, translation: CGFloat, velocity: CGFloat) {
        guard drag.engaged else { return }
        drag.engaged = false
        let wasOpen = shell.sidebarOpen
        let open: Bool
        if velocity > Self.commitVelocity { open = true }
        else if velocity < -Self.commitVelocity { open = false }
        else if wasOpen { open = -translation <= width * Self.commitFraction }
        else { open = translation > width * Self.commitFraction }
        let current = min(width, max(0, (wasOpen ? width : 0) + drag.offset))
        let animation = SidebarCard.animation(reduceMotion: reduceMotion, velocity: velocity,
                                              distance: (open ? width : 0) - current)
        withAnimation(animation, completionCriteria: .removed) { drag.offset = 0; shell.sidebarOpen = open } completion: {
            parkIfClosed()
        }
    }
}

/// A rounded rectangle grown by the safe area, so clipping the conversation keeps
/// its backgrounds under the status bar and home indicator.
private struct CardShape: Shape {
    var cornerRadius: CGFloat
    var insets: EdgeInsets
    var animatableData: CGFloat {
        get { cornerRadius }
        set { cornerRadius = newValue }
    }
    func path(in rect: CGRect) -> Path {
        let grown = CGRect(x: rect.minX - insets.leading, y: rect.minY - insets.top,
                           width: rect.width + insets.leading + insets.trailing,
                           height: rect.height + insets.top + insets.bottom)
        return RoundedRectangle(cornerRadius: cornerRadius, style: .continuous).path(in: grown)
    }
}

/// Keeps a connection's checks and project catalog fresh while the app is in
/// the foreground; one owner per connection, cancelled with the scene.
struct ConnectionLifecycle: View {
    @ObservedObject var model: ConnectionModel
    @Environment(\.scenePhase) private var phase
    var body: some View {
        Color.clear.frame(width: 0, height: 0)
            .onReceive(NotificationCenter.default.publisher(for: UIApplication.willTerminateNotification)) { _ in
                model.flushTypedDrafts()
            }
            .onReceive(NotificationCenter.default.publisher(for: UIApplication.didReceiveMemoryWarningNotification)) { _ in
                model.releaseUnderMemoryPressure()
                model.imagePreviews.removeCachedImages()
                GrowingReplies.removeAll()
                Task { await NativeReplyCache.shared.removeAll(); await HTMLVisualCache.shared.removeAll() }
            }
            // Inactive (Control Center, Notification Center, the app switcher)
            // keeps this running; only the background stops it. A return from
            // the background checks the connection and its route (a computer
            // whose key changed meanwhile says so); the model renews the
            // session when its expiry is near (it renews ahead of it while the
            // connection runs).
            .task(id: phase != .background) {
                guard phase != .background else { return }
                await model.check(verifyRoute: true)
                guard !Task.isCancelled else { return }
                model.setForeground(true)
                await model.projects.refresh()
                // The sidebar marks threads waiting on the owner, with no chat open.
                await model.loadAttention()
                // After renewal: a composer that loaded before it may have no models.
                await model.projects.loadOptions()
                // A safety refresh: every 30 s while the Mac's event stream is
                // down, every 2 min while it is open (it carries the changes).
                // While the computer is out of reach, the connection's recovery
                // owns trying again with backoff; this loop does not add attempts.
                var lastRefresh = ContinuousClock.now
                while !Task.isCancelled {
                    do { try await Task.sleep(for: .seconds(30)) } catch { return }
                    if model.awaitingReconnect { continue }
                    if model.eventStreamLive, ContinuousClock.now - lastRefresh < .seconds(120) { continue }
                    lastRefresh = ContinuousClock.now
                    // Reads the Project catalog too (once: its list refresh owns it).
                    await model.loadChats(force: true)
                    guard !Task.isCancelled else { return }
                    await model.loadAttention()
                }
            }
    }
}

/// Offers Choose projects once per Mac after it is paired and supports them.
struct ProjectOnboardingObserver: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ProjectLibrary
    let offer: (String) -> Void
    static func offeredKey(_ host: String) -> String { "wonder.projects.offered." + Data(host.utf8).base64EncodedString() }
    /// The offer was seen; it is not made again for this Mac.
    static func acknowledge(_ host: String, defaults: UserDefaults = .standard) { defaults.set(true, forKey: offeredKey(host)) }
    var body: some View {
        Color.clear.frame(width: 0, height: 0)
            .onChange(of: library.supportsProjects) { _, supported in
                guard supported == true, library.projects.isEmpty, !model.previewMode,
                      let host = model.connection?.credential.hostInstallationId else { return }
                let key = Self.offeredKey(host)
                guard !UserDefaults.standard.bool(forKey: key) else { return }
                // Marked once its sheet appears: another sheet can hold the offer back.
                offer(host)
            }
    }
}

/// The main surface: a chat or its draft, with the shared header controls.
private struct ShellMain: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    let compactIPadWindow: Bool
    let showSidebarButton: Bool
    let sidebarButtonLabel: String
    var sidebarButtonIcon = "line.3.horizontal"
    var adaptsToDisplayBars = false
    let sidebarAction: () -> Void
    @AccessibilityFocusState private var sidebarButtonFocused: Bool
    @AccessibilityFocusState private var compactChatsFocused: Bool
    /// Pair again from a chat this device can no longer open.
    @State private var pairing = false
    private func model(_ host: String) -> ConnectionModel? {
        library.saved.connections.first { $0.credential.hostInstallationId == host }.map { library.model(for: $0) }
    }
    var body: some View {
        Group {
            switch shell.route {
            case .newChat:
                NewChatView(library: library, shell: shell, keepsComposerAtBottom: adaptsToDisplayBars)
            case .conversation(let host, let id):
                if let saved = library.saved.connections.first(where: { $0.credential.hostInstallationId == host }) {
                    let model = library.model(for: saved)
                    ShellConversation(model: model, projects: model.projects, chatID: id,
                                      openConversation: { shell.open(host: host, conversation: $0) },
                                      pairAgain: { pairing = true },
                                      showArchivedThreads: { shell.show(.archivedThreads(host: host, project: $0)) }, backToChats: { shell.route = .newChat; shell.showChats() })
                        .environment(\.pairedComputerCount, library.saved.connections.count)
                        .id(shell.route)
                } else {
                    ContentUnavailableView {
                        Label("Chat unavailable", systemImage: "bubble.left")
                    } description: {
                        Text("This computer is no longer paired. Pair it again to open its chats.")
                    } actions: {
                        Button("Pair again") { pairing = true }
                            .accessibilityIdentifier("chat-unavailable-pair-again")
                    }
                }
            }
        }
        .sheet(isPresented: $pairing) { PairComputerView(model: library.pairingModel()) }
        .safeAreaInset(edge: .top, spacing: 0) {
            VStack(spacing: 0) {
                if compactIPadWindow {
                    HStack {
                        Button("Chats", systemImage: "line.3.horizontal") { shell.sidebarOpen = true }
                            .accessibilityIdentifier("compact-ipad-chats")
                            .accessibilityFocused($compactChatsFocused)
                        Spacer()
                    }
                    .padding(.horizontal, 12)
                    .frame(minHeight: 44)
                    .background(theme.chrome)
                }
                MacDialogNotificationBanner(library: library)
            }
        }
        .toolbar {
            if !compactIPadWindow && showSidebarButton {
                ToolbarItem(id: "shell-sidebar", placement: .topBarLeading) {
                    if adaptsToDisplayBars {
                        // A native label keeps the item across the Duo's bar-axis changes.
                        Button("Chats", systemImage: sidebarButtonIcon, action: sidebarAction)
                            .labelStyle(.iconOnly)
                            .accessibilityLabel(sidebarButtonLabel)
                            .accessibilityIdentifier("open-sidebar")
                            .accessibilityFocused($sidebarButtonFocused)
                    } else {
                        Button(action: sidebarAction) { Image(systemName: sidebarButtonIcon) }
                            .accessibilityLabel(sidebarButtonLabel)
                            .accessibilityIdentifier("open-sidebar")
                            .accessibilityFocused($sidebarButtonFocused)
                    }
                }
            }
        }
        // The bar keeps one inline layout while the route swaps its content.
        // Left to each screen, a chat still loading (or the frame between two
        // chats) fell back to the large-title layout, so the menu button,
        // title and trailing button drew oversized until the chat appeared.
        .navigationBarTitleDisplayMode(.inline)
        // Each page is its own screen: another computer's Automations or another
        // Project's archive replaces the one shown instead of inheriting its state.
        .navigationDestination(item: $shell.page) { page in pageView(page).id(page) }
        .onChange(of: shell.sidebarOpen) { _, open in
            guard !open, shell.page == nil else { return }
            if compactIPadWindow { compactChatsFocused = true }
            else if showSidebarButton { sidebarButtonFocused = true }
        }
    }

    /// Management screens fill the chat column, with Back to the chat.
    @ViewBuilder private func pageView(_ page: ShellPage) -> some View {
        switch page {
        case .usage:
            UsageView(library: library)
        case .settings:
            ConnectionsView(library: library, backToChats: adaptsToDisplayBars ? { shell.page = nil } : nil)
        case .automations:
            AutomationsView(models: library.saved.connections.map { library.model(for: $0) }.filter { $0.projects.supportsProjects != false }) { shell.open(host: $0, conversation: $1) }
        case .addProject(let host) where DemoMode.isDemo(host: host), .projectSettings(let host, _) where DemoMode.isDemo(host: host):
            DemoPageNotice(library: library)
        case .addProject(let host):
            if let model = model(host) {
                // A suggestion adds in place; a new or chosen folder starts a chat in its project.
                AddProjectView(model: model, library: model.projects, embedded: true, onFinished: { created in
                    shell.newChat(host: host, destination: .project(id: created.id))
                })
            } else { pageUnavailable }
        case .projectSettings(let host, let project):
            if let model = model(host), let summary = model.projects.project(project) {
                ProjectEditorView(model: model, library: model.projects, project: summary, embedded: true) { _ in
                    if shell.page == page { shell.page = nil }
                }
            } else { pageUnavailable }
        case .archivedThreads(let host, let project):
            if let model = model(host), let summary = model.projects.project(project) {
                ArchivedThreadsView(model: model, library: model.projects, projectID: project, projectName: summary.name, embedded: true)
            } else { pageUnavailable }
        case .manageProjects:
            ManageProjectsView(models: library.saved.connections.map { library.model(for: $0) }, embedded: true)
        }
    }

    private var pageUnavailable: some View {
        ContentUnavailableView("Not available", systemImage: "exclamationmark.circle",
                               description: Text("This computer is no longer paired, or the Project was removed."))
    }
}

/// Resolves an address to a Project thread on its Mac, including notifications.
/// A thread with saved detail renders at once; the
/// detail refreshes behind it.
private struct ShellConversation: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var projects: ProjectLibrary
    let chatID: String
    let openConversation: (String) -> Void
    /// Opens pairing for a computer that stopped accepting this device.
    let pairAgain: () -> Void
    /// Opens a Project's Archived threads.
    var showArchivedThreads: ((String) -> Void)? = nil
    var backToChats: () -> Void
    @State private var unavailable = false
    @State private var archived = false
    @State private var loadFailure: ChatOpenFailure?
    @State private var restoring = false
    @State private var restoreFailure: String?
    /// Set once this chat has shown, so a brief network blip keeps it on screen (the dot still turns red).
    @State private var shownLive = false
    var body: some View {
        Group {
            if !model.accessEnded, model.macConnected == false, !shownLive {
                // Nothing saved shows while the computer is away: one plain screen.
                if let rescue = model.rescue {
                    RescueOfflineView(model: model, notice: rescue) { Task { await model.check(renew: true); await resolve() } }
                } else {
                    // The computer reconnects on its own; this opens the chat once it answers.
                    ContentUnavailableView {
                        Label("\(model.macName) is offline", systemImage: "wifi.slash")
                    }
                    .accessibilityIdentifier("chat-offline")
                }
            } else if !archived, !model.accessEnded, !projects.unavailable.contains(chatID),
               let detail = projects.details[chatID], detail.isArchived != true {
                ConversationView(model: model, chat: model.projectChat(detail), readOnly: detail.readOnlyReason != nil, openConversation: openConversation,
                                 archiveConversation: archiveAction(detail), deletedConversation: backToChats)
                    .onAppear {
                        shownLive = true
                        #if WONDER_DIAGNOSTICS
                        model.diagnosticChatShown()
                        #endif
                    }
            } else if !model.accessEnded, !projects.unavailable.contains(chatID),
                      archived || projects.details[chatID]?.isArchived == true {
                archivedView
            } else if model.accessEnded {
                // The access fence stays: nothing of this chat shows until pairing succeeds.
                ContentUnavailableView {
                    Label("Chat unavailable", systemImage: "bubble.left")
                } description: {
                    Text("\(model.macName) no longer accepts this device. Pair again to open this chat.")
                } actions: {
                    Button("Pair again", action: pairAgain)
                        .accessibilityIdentifier("chat-unavailable-pair-again")
                }
            } else if unavailable || projects.unavailable.contains(chatID) {
                ContentUnavailableView {
                    Label("Chat unavailable", systemImage: "bubble.left")
                } description: {
                    Text(model.conversationLoadFailures[chatID]?.message ?? ConnectionModel.removedChatRefusal)
                } actions: {
                    Button("Back to chats", action: backToChats)
                        .accessibilityIdentifier("chat-unavailable-back")
                }
            } else if model.macConnected != true {
                // Still checking (nothing saved to show): a connecting spinner, not a dead end.
                ProgressView("Connecting to \(model.macName)…").accessibilityIdentifier("conversation-loading")
            } else if let loadFailure {
                ContentUnavailableView {
                    Label("Couldn’t load chat", systemImage: "exclamationmark.arrow.circlepath")
                } description: {
                    Text(loadFailure.message)
                } actions: {
                    if loadFailure.canRetry { Button("Try again") { Task { await resolve() } } }
                }
            } else {
                ProgressView("Loading chat…").accessibilityIdentifier("conversation-loading")
            }
        }
        .task(id: chatID + ":" + String(model.macConnected == true)) { await resolve() }
        .onChange(of: chatID) { _, _ in shownLive = false }
        .onChange(of: projects.details[chatID]?.isArchived) { _, isArchived in
            if isArchived == true {
                archived = true
            } else if isArchived == false, archived {
                // Restored here or elsewhere (Archived threads, another window):
                // the chat opens again instead of keeping a stale archived screen.
                archived = false; restoreFailure = nil
            }
        }
    }

    /// Archived history remains readable. Restore is the only chat-changing
    /// action here; ConversationView's read-only mode omits its composer/tools.
    private var archivedView: some View {
        let detail = projects.details[chatID]
        let alsoIn = detail.flatMap { projects.archiveAlsoIn($0.family) }
        return Group {
            if let detail {
                ConversationView(model: model, chat: model.projectChat(detail), readOnly: true,
                                 openConversation: openConversation)
            } else {
                ContentUnavailableView("Chat archived", systemImage: "archivebox")
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Label("Chat archived", systemImage: "archivebox").font(.headline)
                Text(alsoIn.map { "Restore shows this chat in Cupola again and asks the \($0) app to restore it when it uses your computer’s account." }
                     ?? "Restore shows this chat in your chats again.")
                    .font(.subheadline).foregroundStyle(.secondary)
                if model.macConnected != true {
                    Text("Connect to \(model.macName) to restore it.").font(.subheadline)
                }
                if let restoreFailure {
                    Text(restoreFailure).foregroundStyle(.red).accessibilityIdentifier("archived-chat-restore-failure")
                }
                HStack {
                    if restoring {
                        ProgressView("Restoring…").accessibilityIdentifier("archived-chat-restoring")
                    } else {
                        Button("Restore") { Task { await restore() } }
                            .buttonStyle(.wonderProminent)
                            .disabled(model.macConnected != true || model.accessEnded)
                            .accessibilityIdentifier("archived-chat-restore")
                    }
                    if let project = detail?.projectId, let showArchivedThreads, projects.supportsArchiveList {
                        Button("Archived chats") { showArchivedThreads(project) }
                            .accessibilityIdentifier("archived-chat-list")
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding().background(.bar)
        }
    }

    /// Archive from the chat's own menu, where its computer can archive it;
    /// the menu asks first when the owner chose "Confirm before archiving".
    private func archiveAction(_ detail: ProjectConversationDetail) -> (() async throws -> Void)? {
        guard projects.canArchive(detail) else { return nil }
        return { try await archiveHere() }
    }

    private func archiveHere() async throws {
        restoreFailure = nil
        try await projects.archive(conversation: chatID)
        archived = true
    }

    private func restore() async {
        guard !restoring else { return }
        restoring = true; restoreFailure = nil
        let scope = model.assignmentScope
        defer { restoring = false }
        do {
            try await projects.unarchive(conversation: chatID)
            guard scope == model.assignmentScope else { return }
            guard projects.details[chatID]?.isArchived != true else {
                restoreFailure = "\(model.macName) still has this chat archived. Try again from Archived chats."
                return
            }
            archived = false
            projects.noteOpened(chatID)
        } catch is CancellationError {
        } catch {
            guard scope == model.assignmentScope else { return }
            restoreFailure = "The chat couldn’t be restored. " + managementError(error)
        }
    }

    private func resolve() async {
        if projects.details[chatID] != nil {
            // A saved archived copy still refreshes: a chat restored since then
            // reopens (see onChange) instead of keeping "Chat archived".
            if projects.details[chatID]?.isArchived == true { archived = true }
            else { projects.noteOpened(chatID) }
            guard model.macConnected == true else { return }
            await projects.refreshDetail(chatID)
            if !Task.isCancelled, projects.details[chatID]?.isArchived == true {
                archived = true
            }
            return
        }
        // A notification or restored route can arrive before anything is saved.
        // No in-flight flag: a cancelled earlier resolve may still be finishing its read, and
        // the restarted task must not bail on it (that left "Loading chat…" forever). The
        // refresh coordinator already single-flights the read.
        guard model.macConnected == true else { return }
        loadFailure = nil
        do {
            let detail = try await projects.loadDetail(chatID)
            guard !Task.isCancelled else { return }
            if detail.isArchived == true {
                archived = true
                return
            }
            projects.noteOpened(chatID)
            return
        } catch PairingFailure.response(404) {
        } catch {
            if !Task.isCancelled { loadFailure = ChatOpenFailure(error, computer: model.macName) }
            return
        }
        guard !Task.isCancelled, model.macConnected == true else { return }
        unavailable = true
    }
}

// MARK: - Sidebar presentation

/// Prepares the whole sidebar as one flat row list whenever an input or a Mac's
/// catalog changes, so the view only renders rows. Change notifications from the
/// Macs are coalesced to one rebuild per main-actor turn and unchanged results
/// are not published.
// MARK: - Sidebar

/// A custom header has no navigation bar to avoid the display's rounded
/// corners. Ask the platform for that space on the Duo's two displays.
struct DisplayHeaderCorners: ViewModifier {
    let enabled: Bool
    @ViewBuilder func body(content: Content) -> some View {
        if #available(iOS 26, *), enabled {
            content.containerCornerOffset(.horizontal, sizeToFit: true)
        } else {
            content
        }
    }
}

struct SidebarView: View {
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var shell: ShellState
    var isVisible = true
    var avoidsDisplayCorners = false
    #if WONDER_DIAGNOSTICS
    /// The hosted pin scenario selects its own layout/expansion instead of
    /// inheriting a previous simulator scene's saved sidebar preferences.
    var diagnosticPinFixture: (layout: SidebarLayout, expanded: String, reduceMotion: Bool)? = nil
    var diagnosticPinObserver: ((SidebarPresenter) -> Void)? = nil
    #endif
    @StateObject private var presenter = SidebarPresenter()
    @StateObject private var messageSearch = SidebarMessageSearch()
    /// The message search result being opened; its row shows a spinner.
    @State private var openingResult: String?
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.accessibilityReduceMotion) private var reduceSidebarMotion
    @AccessibilityFocusState private var headingFocused: Bool
    @FocusState private var searchFocused: Bool
    @SceneStorage("sidebar.search") private var search = ""
    /// By Project or All chats, remembered across launches.
    @AppStorage("sidebar.layout") private var layoutStorage = SidebarLayout.byProject.rawValue
    private var layout: SidebarLayout {
        #if WONDER_DIAGNOSTICS
        if let fixture = diagnosticPinFixture { return fixture.layout }
        #endif
        return SidebarLayout(rawValue: layoutStorage) ?? .byProject
    }
    @SceneStorage("sidebar.expanded") private var expandedStorage = ""
    @SceneStorage("sidebar.expansionInitialized") private var expansionInitialized = false
    /// The thread being attached or copied; its row shows a spinner.
    @State private var busyThread: String?
    @State private var busyOpenFeedback: String?
    @State private var failure: String?
    @State private var addingComputer = false
    @State private var renaming: ThreadTarget?
    @StateObject private var archiveSelection = ThreadArchiveSelection()
    @State private var confirmingArchive = false
    /// One thread waiting for "Confirm before archiving".
    @State private var confirmingThreadArchive: ThreadTarget?
    @AppStorage(ActionConfirmation.archiveKey) private var confirmArchive = ActionConfirmation.archiveDefault
    /// Undo for the last archive or hide, shown where Settings sits.
    @StateObject private var undoState = SidebarUndoState()
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver

    @State private var renameText = ""

    private struct ThreadTarget: Identifiable {
        let host: String
        let project: String
        let thread: ProjectThreadSummary
        var id: String { host + "/" + thread.reference }
    }

    private var expanded: Set<String> {
        #if WONDER_DIAGNOSTICS
        if let fixture = diagnosticPinFixture { return [fixture.expanded] }
        #endif
        return Set(expandedStorage.split(separator: "\n").map(String.init))
    }
    private var hasInitializedExpansion: Bool {
        #if WONDER_DIAGNOSTICS
        if diagnosticPinFixture != nil { return true }
        #endif
        return expansionInitialized
    }
    private var sidebarReduceMotion: Bool {
        #if WONDER_DIAGNOSTICS
        if let fixture = diagnosticPinFixture { return fixture.reduceMotion }
        #endif
        return reduceSidebarMotion
    }
    private func setExpanded(_ key: String, _ value: Bool) {
        var next = expanded
        if value { next.insert(key) } else { next.remove(key) }
        expandedStorage = next.sorted().joined(separator: "\n")
    }
    private var trimmedSearch: String { search.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var inputs: SidebarPresenter.Inputs {
        SidebarPresenter.Inputs(models: library.saved.connections.map { ObjectIdentifier(library.model(for: $0)) },
                                visibleHosts: library.saved.selectedHostIDs, search: search, layout: layout,
                                isVisible: isVisible && scenePhase == .active, reduceMotion: sidebarReduceMotion, expanded: expanded,
                                selection: shell.route, expansionInitialized: hasInitializedExpansion)
    }
    private func model(_ host: String) -> ConnectionModel? {
        library.saved.connections.first { $0.credential.hostInstallationId == host }.map { library.model(for: $0) }
    }

    private func headerIcon(_ systemName: String) -> some View {
        Image(systemName: systemName).font(.body.weight(.medium)).frame(minWidth: 44, minHeight: 44)
    }

    /// Add Project for the one computer that takes Projects, or a choice of computer.
    @ViewBuilder private var addProjectButton: some View {
        let hosts = presenter.projectHosts
        Group {
            if hosts.count > 1 {
                Menu {
                    Section("Choose a computer") {
                        ForEach(hosts) { host in
                            Button(host.name, systemImage: "laptopcomputer") { shell.show(.addProject(host: host.id)) }
                                .accessibilityIdentifier("sidebar-add-project:" + host.id)
                        }
                    }
                } label: { headerIcon("folder.badge.plus") }
            } else {
                Button { if let host = hosts.first { shell.show(.addProject(host: host.id)) } } label: { headerIcon("folder.badge.plus") }
                    .disabled(hosts.isEmpty)
            }
        }
        .disabled(archiveSelection.isSelecting)
        .accessibilityLabel("Add Project")
        .accessibilityIdentifier("sidebar-add-project")
    }

    var body: some View {
        VStack(spacing: 0) {
            if archiveSelection.isSelecting {
                selectionToolbar
                    .padding(.horizontal, 20).padding(.top, 6)
                    .modifier(DisplayHeaderCorners(enabled: avoidsDisplayCorners))
            } else {
                HStack(spacing: 4) {
                    // The same in By Project and All Chats: Cupola's icon, then New chat and Add Project.
                    HStack(spacing: 4) {
                        CupolaMark()
                            .accessibilityFocused($headingFocused)
                            .padding(.trailing, 4)
                        if library.isDemo { DemoBadge().accessibilityLabel("Demo computer") }
                        Button { shell.newChat() } label: { headerIcon("square.and.pencil") }
                            .disabled(library.saved.connections.isEmpty)
                            .accessibilityLabel("New chat")
                            .accessibilityIdentifier("sidebar-new-chat")
                        addProjectButton
                    }
                    // One menu arranges the list, like T3 Code's thread list options.
                    Menu {
                        Button("Select chats", systemImage: "checkmark.circle") { archiveSelection.begin() }
                            .disabled(busyThread != nil || !presenter.rows.contains { archiveTarget($0) != nil })
                            .accessibilityIdentifier("threads-select")
                        Section("View") {
                            Picker("View", selection: $layoutStorage) {
                                ForEach(SidebarLayout.allCases) { option in
                                    Text(option.title).tag(option.rawValue)
                                        .accessibilityIdentifier("sidebar-layout:" + option.rawValue)
                                }
                            }
                            .pickerStyle(.inline).labelsHidden()
                        }
                        // Hidden Projects come back from here, not only from Settings.
                        if !presenter.projectHosts.isEmpty {
                            Section {
                                Button("Manage Projects", systemImage: "folder") { shell.show(.manageProjects) }
                                    .accessibilityIdentifier("sidebar-manage-projects")
                            }
                        }
                    } label: {
                        Image(systemName: "ellipsis.circle").font(.body).frame(minWidth: 44, minHeight: 44)
                    }
                    .accessibilityLabel("Chat list options")
                    .accessibilityValue(layout.title)
                    .accessibilityIdentifier("sidebar-session-filter")
                    AttentionInboxEntry(library: library, shell: shell)
                    // Icons sit at one even gap from the left edge; the slack goes to the right.
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 20).padding(.top, 6)
                .modifier(DisplayHeaderCorners(enabled: avoidsDisplayCorners))
            }
            // The waiting requests drop in under the bell, in the sidebar's own surface, above Search.
            if shell.attentionPresented && !archiveSelection.isSelecting {
                AttentionInboxView(library: library, shell: shell)
            }
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass").foregroundStyle(theme.isDefault ? Color.secondary : theme.secondaryText)
                TextField("Search", text: $search, prompt: Text("Search").foregroundStyle(theme.isDefault ? Color.secondary : theme.secondaryText))
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    .focused($searchFocused)
                    .accessibilityLabel("Search chats and Projects")
                    .accessibilityIdentifier("sidebar-search")
                if !search.isEmpty {
                    Button { search = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                        .accessibilityLabel("Clear search")
                }
            }
            .padding(.horizontal, 12).frame(minHeight: 40)
            .background(theme.field, in: RoundedRectangle(cornerRadius: 10))
            .padding(.horizontal, 16).padding(.vertical, 8)
            if !presenter.pills.isEmpty { filterPills }
            if let filterFailure = library.filterFailure {
                // Choosing a computer again retries; the next saved choice clears this.
                Text(filterFailure).font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 20)
                    .accessibilityIdentifier("sidebar-filter-failure")
            }
            List { // theme-exempt: the sidebar draws theme.sidebar
                if let error = library.error { FailureDetails("Connection problem", message: error).listRowSeparator(.hidden) }
                if library.isDemo {
                    DemoChatsNotice(addComputer: { addingComputer = true }, leave: {
                        Task { await library.leaveDemo() }
                    })
                    .listRowSeparator(.hidden).listRowBackground(Color.clear)
                    .listRowInsets(EdgeInsets(top: 0, leading: 20, bottom: 4, trailing: 20))
                }
                ForEach(presenter.rows) { row in
                    SidebarRowView(row: row) { AnyView(rowView($0)) }
                        .moveDisabled(row.orderItem == nil || archiveSelection.isSelecting)
                        .modifier(ClearRowBackground(row: row))
                        .listRowSeparator(.hidden)
                        .listRowInsets(EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12))
                }
                .onMove { source, destination in presenter.move(from: source, to: destination) }
                ForEach(presenter.noProjects.sorted { $0.key < $1.key }, id: \.key) { host, notice in
                    noProjectsRow(host: host, notice: notice)
                        .listRowBackground(Color.clear).listRowSeparator(.hidden)
                        .listRowInsets(EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12))
                }
                messageResults
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12))
                if library.saved.connections.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("No computer yet. Run cupola pair on your computer to see its chats here.")
                            .foregroundStyle(.secondary)
                        Button("Add computer", systemImage: "plus") { addingComputer = true }
                            .accessibilityIdentifier("sidebar-add-computer")
                    }
                    .padding(.vertical, 6).listRowSeparator(.hidden).listRowBackground(Color.clear)
                } else if !trimmedSearch.isEmpty && !presenter.hasResults
                    && !presenter.hasUnresolvedThreads && !messageSearch.hasContent {
                    Text("No matching chats")
                        .foregroundStyle(theme.secondaryText)
                        .listRowSeparator(.hidden)
                        .listRowBackground(Color.clear)
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .environment(\.defaultMinListRowHeight, 40)
            .refreshable {
                await withTaskGroup(of: Void.self) { group in
                    for saved in library.saved.connections where library.saved.includes(saved.credential.hostInstallationId) {
                        let model = library.model(for: saved)
                        // Pulling to refresh is asking to try an unreachable computer now.
                        model.reconnectNow()
                        group.addTask { await model.loadChats(force: true); await model.projects.refresh() }
                    }
                }
            }
            .confirmationDialog("Archive “\(confirmingThreadArchive?.thread.title ?? "chat")”?", isPresented: Binding(
                get: { confirmingThreadArchive != nil }, set: { if !$0 { confirmingThreadArchive = nil } }),
                titleVisibility: .visible, presenting: confirmingThreadArchive) { target in
                Button("Archive", role: .destructive) { archive(target.host, target.project, target.thread) }
                    .accessibilityIdentifier("archive-thread-confirm")
            } message: { target in
                Text(target.thread.family == .codex // provider identity: names the Codex app consequence
                     ? "It will also be archived in the Codex app when it uses your computer’s account. You can restore it from Archived chats."
                     : "It will be archived in Cupola only. You can restore it from Archived chats.")
            }
            .alert("Rename chat", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
                TextField("Title", text: $renameText)
                Button("Rename") { if let target = renaming { rename(target) } }
                Button("Cancel", role: .cancel) {}
            }
            if archiveSelection.isSelecting || archiveSelection.notice != nil {
                archiveSelectionBar
            } else if let undo = undoState.notice {
                undoBar(undo)
            } else {
                HStack(spacing: 0) {
                    Button { shell.show(.usage) } label: {
                        Image(systemName: "chart.bar").font(.body).frame(width: 44, height: 44)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Usage")
                    .accessibilityIdentifier("sidebar-usage")
                    Button { shell.show(.settings) } label: {
                        Image(systemName: "gearshape").font(.body).frame(width: 44, height: 44)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Settings")
                    .accessibilityIdentifier("sidebar-settings")
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 12).padding(.bottom, 4)
            }
        }
        .task(id: undoState.notice?.id) {
            guard let shown = undoState.notice else { return }
            AccessibilityNotification.Announcement(shown.message + ". Undo is available.").post()
            guard !voiceOver else { return }
            #if WONDER_DIAGNOSTICS
            // A loaded simulator can take longer than the notice lasts to look
            // at it; `-diagnostics-undo-held` keeps it until `signal undo-expire`.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-undo-held") {
                while !DiagnosticSubagentFixture.signalled("undo-expire") {
                    try? await Task.sleep(for: .milliseconds(100))
                    if Task.isCancelled { return }
                }
                undoState.expire(shown.id)
                return
            }
            #endif
            try? await Task.sleep(for: .seconds(SidebarUndo.visibleSeconds))
            if !Task.isCancelled { undoState.expire(shown.id) }
        }
        .task(id: archiveSelection.requestID) {
            await archiveSelection.run(resolve: model) { target in
                if shell.selectedConversation?.host == target.host,
                   shell.selectedConversation?.id == target.thread.conversationId { shell.route = .newChat }
            }
        }
        // Computers no longer collapse as groups; drop the retired saved choice.
        .task { UserDefaults.standard.removeObject(forKey: "sidebar.collapsedHosts") }
        #if WONDER_DIAGNOSTICS
        .task {
            // Preview captures of the header while choosing chats need no long press.
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-sidebar-selecting") { archiveSelection.begin() }
        }
        #endif
        .alert("Couldn’t complete that", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK") { failure = nil }
        } message: { Text(failure ?? "") }
        .sheet(isPresented: $addingComputer) { PairComputerView(model: library.pairingModel()) }
        .onChange(of: inputs, initial: true) {
            presenter.configure(inputs, sources: library.saved.connections.map {
                SidebarPresenter.Source(hostID: $0.credential.hostInstallationId, model: library.model(for: $0))
            })
            #if WONDER_DIAGNOSTICS
            diagnosticPinObserver?(presenter)
            #endif
        }
        .onChange(of: messageSearchKey, initial: true) {
            messageSearch.update(query: search, sources: messageSearchSources)
        }
        .onChange(of: presenter.expansionSuggestion, initial: true) { _, key in
            guard let key, !hasInitializedExpansion else { return }
            expansionInitialized = true
            setExpanded(key, true)
        }
        .onChange(of: shell.sidebarOpen) { _, open in headingFocused = open }
        // After this update: opening the phone drawer first puts the keyboard away.
        .onChange(of: shell.searchRequest) { Task { @MainActor in searchFocused = true } }
        #if WONDER_DIAGNOSTICS
        .onChange(of: isVisible && presenter.hasResults, initial: true) { _, ready in
            if ready { DiagnosticPerformanceRun.chatsReady() }
        }
        .onAppear {
            // The threads-notice scenario starts from its project expanded, whatever was remembered.
            let arguments = ProcessInfo.processInfo.arguments
            if let fixture = diagnosticPinFixture {
                layoutStorage = fixture.layout.rawValue
                return
            }
            // Scenarios start By Project unless they choose a view themselves, so
            // one that stopped in All Chats can't change the next one's list.
            if arguments.contains("-diagnostics-sidebar-all-chats") { layoutStorage = SidebarLayout.allChats.rawValue }
            else if arguments.contains(where: { $0.hasPrefix("-diagnostics-") }) { layoutStorage = SidebarLayout.byProject.rawValue }
            if DiagnosticSubagentFixture.chatOpenFailureFixture {
                search = ""
            }
            if DiagnosticSubagentFixture.bulkArchiveFixture {
                expandedStorage = SidebarProjection.expansionKey(host: DiagnosticSubagentFixture.hostID, project: "read-project")
                search = ""
            }
            if ["-diagnostics-project-threads-partial", "-diagnostics-project-threads-slow", "-diagnostics-project-threads-down"]
                .contains(where: arguments.contains) || DiagnosticSubagentFixture.sidebarStatusFixture {
                expandedStorage = SidebarProjection.expansionKey(host: DiagnosticSubagentFixture.hostID, project: "read-project")
                if DiagnosticSubagentFixture.sidebarStatusFixture {
                    search = ""
                }
            }
        }
        #endif
    }

    // MARK: Message search

    /// The connected Macs shown in the sidebar, searched for message text.
    private var messageSearchSources: [SidebarMessageSearch.Source] {
        library.saved.connections
            .filter { library.saved.includes($0.credential.hostInstallationId) }
            .map { library.model(for: $0) }
            .filter { $0.macConnected == true }
            .compactMap { model in
                model.connection.map { SidebarMessageSearch.Source(host: $0.credential.hostInstallationId, name: model.macName, model: model) }
            }
    }
    private var messageSearchKey: String {
        trimmedSearch + "\n" + messageSearchSources.map(\.host).joined(separator: ",")
    }

    @ViewBuilder private var messageResults: some View {
        let multipleMacs = messageSearch.hosts.count > 1
        ForEach(messageSearch.hosts) { host in
            if host.loading || host.failed || !host.page.results.isEmpty {
                Text(multipleMacs ? "Messages on \(host.name)" : "Messages")
                    .font(.footnote.weight(.semibold)).foregroundStyle(theme.secondaryText)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 12).padding(.horizontal, 8)
                    .accessibilityAddTraits(.isHeader)
                    .accessibilityIdentifier("sidebar-message-results")
            }
            if host.loading {
                HStack(spacing: 8) {
                    ProgressView()
                    Text("Searching messages…").font(.footnote).foregroundStyle(theme.secondaryText)
                }
                .padding(.horizontal, 8).padding(.vertical, 6)
                .accessibilityIdentifier("sidebar-message-search-loading")
            } else if host.failed {
                Button {
                    if let source = messageSearchSources.first(where: { $0.host == host.id }) { messageSearch.retry(source) }
                } label: {
                    Label("Couldn’t search messages on \(host.name). Try again", systemImage: "exclamationmark.arrow.circlepath")
                        .font(.footnote).frame(maxWidth: .infinity, minHeight: 40, alignment: .leading)
                }
                .buttonStyle(.plain).padding(.horizontal, 8)
                .accessibilityIdentifier("sidebar-message-search-retry")
            }
            ForEach(host.page.results) { result in
                messageResultRow(result, host: host.id)
            }
            if host.page.partial == true {
                Text("Some history on \(host.name) couldn’t be searched. Try again shortly.")
                    .font(.footnote).foregroundStyle(theme.secondaryText)
                    .padding(.horizontal, 8).padding(.vertical, 4)
                    .accessibilityIdentifier("sidebar-message-search-partial")
            }
            if host.page.nextCursor != nil {
                Button {
                    if let source = messageSearchSources.first(where: { $0.host == host.id }) { messageSearch.loadMore(source) }
                } label: {
                    HStack(spacing: 8) {
                        Text("More messages")
                        if host.loadingMore { ProgressView() }
                    }
                    .font(.footnote.weight(.semibold)).frame(maxWidth: .infinity, minHeight: 40, alignment: .leading)
                }
                .buttonStyle(.plain).padding(.horizontal, 8)
                .disabled(host.loadingMore)
                .accessibilityIdentifier("sidebar-message-search-more")
            }
        }
    }

    private func messageResultRow(_ result: MessageSearchResult, host: String) -> some View {
        Button { openResult(result, host: host) } label: {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(result.title).font(.subheadline.weight(.semibold)).lineLimit(1)
                    Text(snippetText(result)).font(.footnote).foregroundStyle(theme.secondaryText).lineLimit(2)
                }
                Spacer(minLength: 4)
                if openingResult == result.identity { ProgressView() }
                else if let date = result.updatedDate {
                    Text(date, format: .relative(presentation: .named)).font(.caption).foregroundStyle(theme.secondaryText)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 8).padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(openingResult != nil)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("search-result:" + result.identity)
    }

    private func snippetText(_ result: MessageSearchResult) -> AttributedString {
        var text = AttributedString()
        for run in result.snippetRuns {
            var part = AttributedString(run.text)
            if run.isMatch {
                part.font = .footnote.weight(.semibold)
                part.foregroundColor = .primary
            }
            text += part
        }
        return text
    }

    /// Opens the result's conversation, attaching a thread started on the Mac
    /// first, then scrolls to the matching message once it is loaded.
    private func openResult(_ result: MessageSearchResult, host: String) {
        guard openingResult == nil, let model = model(host) else { return }
        openingResult = result.identity
        // The chat opens for reading; the search keyboard would cover it.
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        let query = messageSearch.query
        Task {
            defer { openingResult = nil }
            do {
                let conversation: String
                if let id = result.conversationId { conversation = id }
                else if let project = result.projectId, let reference = result.reference {
                    conversation = try await model.projects.attach(project, reference: reference)
                } else { throw PairingFailure.response(404) }
                model.projects.noteOpened(conversation)
                shell.open(host: host, conversation: conversation)
                model.focusSearchResult(result, conversation: conversation, query: query)
            } catch PairingFailure.response(404) {
                failure = "That chat is no longer on \(model.macName)."
            } catch is CancellationError {
            } catch {
                failure = ChatOpenFailure(error, computer: model.macName).message
            }
        }
    }

    // MARK: Header

    private func undoBar(_ undo: SidebarUndo) -> some View {
        let layout = typeSize.isAccessibilitySize ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4))
            : AnyLayout(HStackLayout(spacing: 12))
        return layout {
            VStack(alignment: .leading, spacing: 2) {
                Text(undo.message).font(.subheadline).lineLimit(2)
                if let detail = undo.detail {
                    Text(detail).font(.caption).foregroundStyle(theme.secondaryText)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("sidebar-undo-message")
            HStack(spacing: 12) {
                if case .showProject = undo.action {
                    Button("Manage") { shell.show(.manageProjects) }
                        .frame(minHeight: 44)
                        .accessibilityLabel("Manage Projects")
                        .accessibilityIdentifier("sidebar-undo-manage")
                }
                if undoState.undoing {
                    ProgressView().frame(minWidth: 44, minHeight: 44).accessibilityLabel("Undoing")
                } else {
                    Button("Undo") { performUndo(undo) }
                        .fontWeight(.semibold).frame(minHeight: 44)
                        .disabled(model(undo.host)?.macConnected != true)
                        .accessibilityIdentifier("sidebar-undo")
                }
                Button { undoState.dismiss() } label: { Image(systemName: "xmark").frame(minWidth: 44, minHeight: 44) }
                    .disabled(undoState.undoing)
                    .accessibilityLabel("Dismiss")
                    .accessibilityIdentifier("sidebar-undo-dismiss")
            }
        }
        .padding(.horizontal, 20).padding(.top, 4).padding(.bottom, 4)
        .background(theme.sidebar)
    }

    /// Reverses the last archive or hide. A failure keeps the notice, says why,
    /// and leaves Undo there to try again.
    private func performUndo(_ undo: SidebarUndo) {
        guard let model = model(undo.host) else { return }
        let scope = model.assignmentScope
        Task {
            let message = await undoState.undo(in: model.projects)
            guard scope == model.assignmentScope, let message else { return }
            failure = message
        }
    }

    private var archiveSelectionBar: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let notice = archiveSelection.notice {
                Text(notice).font(.subheadline).accessibilityIdentifier("threads-archive-result")
            }
            if !archiveSelection.failures.isEmpty {
                DisclosureGroup {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 8) {
                            ForEach(archiveSelection.failures) { failure in
                                Text(failure.target.thread.title + ": " + failure.message)
                                    .font(.caption).frame(maxWidth: .infinity, alignment: .leading)
                            }
                        }
                    }.frame(maxHeight: 120)
                } label: {
                    Text("Details").frame(minHeight: 44)
                }.font(.subheadline).accessibilityIdentifier("threads-archive-failures")
            }
            // Cancel lives in the selection toolbar at the top; "Done" only closes a finished result.
            HStack {
                if !archiveSelection.isSelecting { archiveCancel }
                Spacer()
                archiveAction
            }
        }
        .padding(.horizontal, 20).padding(.top, 8).padding(.bottom, 4)
        .background(theme.sidebar)
    }

    private var archiveCountDescription: String {
        let count = archiveSelection.selected.count
        return "\(count) " + (count == 1 ? "chat" : "chats")
    }

    /// Choosing chats swaps the icon row for one line: Cancel, how many are chosen, Select All.
    private var selectionToolbar: some View {
        let count = archiveSelection.selected.count
        let cancel = Button("Cancel") { archiveSelection.cancel() }
            .lineLimit(1).fixedSize()
            .frame(minHeight: 44)
            .disabled(archiveSelection.isArchiving)
            .accessibilityIdentifier("threads-selection-cancel")
        let selectAll = Button("Select All") { archiveSelection.selectAll(selectableArchiveTargets()) }
            .lineLimit(1).fixedSize()
            .frame(minHeight: 44)
            .disabled(archiveSelection.isArchiving)
            .accessibilityHint("Selects the loaded chats in this list")
            .accessibilityIdentifier("threads-select-all")
        let label = Text("\(count) selected").font(.subheadline.weight(.semibold)).monospacedDigit()
            .accessibilityIdentifier("threads-selection-count")
        // At accessibility text sizes the three no longer fit one line: the count sits above the buttons.
        return ViewThatFits(in: .horizontal) {
            HStack(spacing: 8) {
                cancel
                label.lineLimit(1).minimumScaleFactor(0.7).frame(maxWidth: .infinity)
                selectAll
            }
            VStack(spacing: 0) {
                label
                HStack { cancel; Spacer(minLength: 8); selectAll }
            }
        }
    }

    private var archiveCancel: some View {
        Button(archiveSelection.isSelecting ? "Cancel" : "Done") { archiveSelection.cancel() }
            .disabled(archiveSelection.isArchiving)
            .frame(minWidth: 44, maxWidth: typeSize.isAccessibilitySize ? .infinity : nil, minHeight: 44, alignment: .leading)
            .accessibilityIdentifier("threads-selection-cancel")
    }

    @ViewBuilder private var archiveAction: some View {
        if archiveSelection.isArchiving {
            ProgressView("Archiving…").font(.subheadline)
                .accessibilityIdentifier("threads-archiving")
        } else if archiveSelection.isSelecting {
            Button(archiveSelection.failures.isEmpty ? "Archive \(archiveSelection.selected.count)" : "Try again") {
                confirmingArchive = true
            }
            .disabled(archiveSelection.selected.isEmpty)
            .frame(maxWidth: typeSize.isAccessibilitySize ? .infinity : nil, minHeight: 44, alignment: .leading)
            .fontWeight(.semibold)
            .accessibilityLabel(archiveSelection.failures.isEmpty ? "Archive \(archiveCountDescription)" : "Try archiving \(archiveCountDescription) again")
            .accessibilityIdentifier("threads-archive")
            .confirmationDialog("Archive \(archiveCountDescription)?", isPresented: $confirmingArchive, titleVisibility: .visible) {
                Button("Archive \(archiveSelection.selected.count)", role: .destructive) { archiveSelection.requestArchive() }
                    .accessibilityIdentifier("threads-archive-confirm")
                Button("Cancel", role: .cancel) {}
            } message: {
                Text(archiveSelection.selected.contains { $0.thread.family == .codex } // provider identity: names the Codex app consequence
                     ? "Codex chats will also be archived in the Codex app when they use your computer’s account. Other chats are archived in Cupola only. You can restore chats from Archived chats."
                     : "These chats will be archived in Cupola only. You can restore them from Archived chats.")
            }
        }
    }

    private func archiveTarget(_ host: String, _ project: String, _ thread: ProjectThreadSummary) -> ThreadArchiveSelection.Target? {
        guard let model = model(host), model.projects.canArchive(thread) else { return nil }
        return ThreadArchiveSelection.Target(host: host, scope: model.assignmentScope, project: project, thread: thread)
    }

    private func selectableArchiveTargets() -> [ThreadArchiveSelection.Target] {
        presenter.rows.compactMap { archiveTarget($0) }
    }

    private func archiveTarget(_ row: SidebarRow) -> ThreadArchiveSelection.Target? {
        switch row {
        case .pinned(let host, let project, _, _, let thread, _),
             .thread(let host, let project, let thread, _),
             .chat(let host, let project, _, let thread, _):
            return archiveTarget(host, project, thread)
        default: return nil
        }
    }

    private var filterPills: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                pill("All", link: nil, selected: library.saved.selectedHostIDs.isEmpty, id: "all") { library.filter(nil) }
                ForEach(presenter.pills) { item in
                    pill(item.name, link: item.link, selected: library.saved.selectedHostIDs.contains(item.id), id: item.id) { library.filter(item.id) }
                }
            }
            .padding(.horizontal, 16)
        }
        .padding(.bottom, 8)
    }

    private func pill(_ title: String, link: SidebarPresenter.Link?, selected: Bool, id: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(title).font(.subheadline.weight(selected ? .semibold : .regular)).lineLimit(1)
                if let link { Circle().fill(link.color).frame(width: 7, height: 7).accessibilityHidden(true) }
            }
            .foregroundStyle(theme.text)
            .padding(.horizontal, 12).frame(minHeight: 32)
            .background(selected ? theme.accent.opacity(0.16) : theme.text.opacity(0.08), in: Capsule())
            .frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityLabel(link.map { title + ", " + $0.spoken } ?? title)
        .accessibilityIdentifier("computer-filter:" + id)
    }

    // MARK: Rows

    @ViewBuilder private func rowView(_ row: SidebarRow) -> some View {
        switch row {
        case .pinnedHeader:
            sectionTitle("Pinned")
        case .projectsHeader:
            sectionTitle("Projects")
        case .pinned(let host, let projectID, _, _, let thread, let isSelected),
             .thread(let host, let projectID, let thread, let isSelected),
             .chat(let host, let projectID, _, let thread, let isSelected):
            // All placements share this branch as well as the model identity;
            // changing only the ID would still replace the row's inner view.
            let placement = threadPlacement(row)
            threadRow(host: host, projectID: projectID, thread: thread, isSelected: isSelected, reorderRow: row,
                      subtitle: placement.subtitle, leading: placement.leading, prefix: placement.prefix)
                // Keep the moving cell above the headers it crosses.
                .zIndex(1)
        case .hostNotice(let host, let name, let kind):
            switch kind {
            case .pairAgain:
                hostNotice("Pair \(name) again to open its chats.", action: "Pair again", id: "sidebar-pair-again:" + host) {
                    addingComputer = true
                }
            case .rescue(let notice):
                if let model = model(host) {
                    RescueNoticeView(model: model, notice: notice, place: "sidebar")
                        .padding(.horizontal, 4)
                }
            case .storageFailed(let message):
                hostNotice(message, action: "Try again", id: "sidebar-storage-retry:" + host) { model(host)?.retryStorage() }
            case .projectsFailed(let message, let isRetrying):
                hostNotice(message, action: "Retry", id: "sidebar-projects-retry:" + host, isRetrying: isRetrying) {
                    guard let model = model(host) else { return }
                    Task { await model.projects.refresh() }
                }
            }
        case .project(let host, let project, let isExpanded, let isSelected):
            projectRow(host: host, project: project, isExpanded: isExpanded, isSelected: isSelected)
                .task(id: inputs) {
                    await Task.yield()
                    guard !Task.isCancelled else { return }
                    presenter.appeared(host: host, project: project.id)
                }
        case .threadsLoading:
            ProgressView().frame(maxWidth: .infinity, alignment: .leading).padding(.leading, childIndent)
        case .threadsNotice(let host, let projectID, let message, let canRetry, let isRetrying):
            HStack(spacing: 8) {
                Text(message).font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                if isRetrying {
                    ProgressView().controlSize(.small).accessibilityLabel("Retrying")
                } else if canRetry {
                    Button("Try again") { model(host)?.projects.loadThreads(projectID) }
                        .font(.caption.weight(.semibold)).buttonStyle(.borderless)
                        .frame(minHeight: 44)
                        .accessibilityIdentifier("threads-retry:" + projectID)
                }
            }
            .padding(.leading, childIndent)
        case .chatsLoading(let host):
            ProgressView().frame(maxWidth: .infinity, alignment: .leading).padding(.leading, 4)
                .accessibilityLabel("Loading chats")
                .accessibilityIdentifier("chats-loading:" + host)
        case .chatsNotice(let host, let projectID, let message, let canRetry, let isRetrying):
            HStack(spacing: 8) {
                Text(message).font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                if isRetrying {
                    ProgressView().controlSize(.small).accessibilityLabel("Retrying")
                } else if canRetry {
                    Button("Try again") { model(host)?.projects.loadThreads(projectID) }
                        .font(.caption.weight(.semibold)).buttonStyle(.borderless)
                        .frame(minHeight: 44)
                        .accessibilityIdentifier("chats-retry:" + projectID)
                }
            }
            .padding(.leading, 4)
        case .moreChats(let host, let isLoading):
            Button { presenter.loadMoreChats() } label: {
                HStack { Text("Show more"); if isLoading { ProgressView().controlSize(.small) } }
                    .font(.subheadline).foregroundStyle(.secondary).padding(.leading, 4)
                    .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                    .contentShape(Rectangle())
            }.buttonStyle(.plain).disabled(isLoading)
                .accessibilityIdentifier("chats-more:" + host)
        case .moreThreads(let host, let projectID, let isLoading):
            Button { model(host)?.projects.loadThreads(projectID, more: true) } label: {
                HStack { Text("Show more"); if isLoading { ProgressView().controlSize(.small) } }
                    .font(.subheadline).foregroundStyle(.secondary).padding(.leading, childIndent)
                    .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                    .contentShape(Rectangle())
            }.buttonStyle(.plain).disabled(isLoading)
                .accessibilityIdentifier("threads-more:" + projectID)
        case .automations:
            Button { shell.show(.automations) } label: {
                iconRow(Text("Automations")) { Image(systemName: "clock.arrow.circlepath") }
                    .foregroundStyle(.primary)
            }
            .buttonStyle(.plain)
            .accessibilityHint("Scheduled tasks on your computers")
            .accessibilityIdentifier("sidebar-automations")
        }
    }

    /// A computer with no Project to show: the notice and its Add Project.
    private func noProjectsRow(host: String, notice: String) -> some View {
        HStack(spacing: 8) {
            Text(notice).font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            Button("Add Project") { shell.show(.addProject(host: host)) }
                .font(.caption.weight(.semibold)).buttonStyle(.borderless)
                .frame(minHeight: 44)
                .disabled(archiveSelection.isSelecting)
                .accessibilityLabel("Add Project to \(presenter.computerNames[host] ?? "this computer")")
                .accessibilityIdentifier("sidebar-no-projects-add:" + host)
        }
        .padding(.horizontal, 4)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("sidebar-no-projects:" + host)
    }

    /// A computer's notice with the one action that recovers from it.
    private func hostNotice(_ message: String, action: String, id: String, isRetrying: Bool = false,
                            perform: @escaping () -> Void) -> some View {
        HStack(spacing: 8) {
            Text(message).font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            if isRetrying {
                ProgressView().controlSize(.small).accessibilityLabel("Retrying")
            } else {
                Button(action, action: perform)
                    .font(.caption.weight(.semibold)).buttonStyle(.borderless)
                    .frame(minHeight: 44)
                    .accessibilityIdentifier(id)
            }
        }
        .padding(.horizontal, 4)
    }

    /// Automations and projects put their icons in one column, so
    /// the icons line up and project threads indent under the names. Every icon
    /// is a body-size symbol (or drawn to match one) centred in the column.
    /// Icons grow with the text up to the first accessibility size (AX1), so at the
    /// largest sizes the names keep the sidebar's width.
    @ScaledMetric(relativeTo: .body) private var scaledIconColumn: CGFloat = 24
    private var iconColumn: CGFloat { min(scaledIconColumn, 40) }
    private static let iconLeading: CGFloat = 4
    private static let iconGap: CGFloat = 8
    private var childIndent: CGFloat { Self.iconLeading + iconColumn + Self.iconGap }
    /// Names wrap at accessibility sizes instead of ending after a letter or two.
    private var nameLines: Int { typeSize.isAccessibilitySize ? 3 : 1 }

    private func iconRow(_ title: some View, @ViewBuilder icon: () -> some View) -> some View {
        HStack(spacing: Self.iconGap) {
            icon().font(.body).imageScale(.medium).dynamicTypeSize(...DynamicTypeSize.accessibility1)
                .frame(width: iconColumn).accessibilityHidden(true)
            title.font(.body).lineLimit(nameLines)
        }
        .padding(.leading, Self.iconLeading)
        .frame(maxWidth: .infinity, minHeight: 40, alignment: .leading).contentShape(Rectangle())
    }

    private func sectionTitle(_ title: String) -> some View {
        Text(title).font(.subheadline.weight(.semibold)).foregroundStyle(theme.isDefault ? Color.secondary : theme.secondaryText)
            .padding(.horizontal, 4).frame(maxWidth: .infinity, minHeight: 40, alignment: .leading)
            .accessibilityAddTraits(.isHeader)
    }

    /// Native List needs a provider to lift rows outside Edit mode. This token
    /// is readable only inside this process and contains no title or stable ID.
    private func sidebarDragProvider() -> NSItemProvider {
        let provider = NSItemProvider()
        provider.registerDataRepresentation(forTypeIdentifier: "com.swaymun.wonder.sidebar-row", visibility: .ownProcess) { completion in
            completion(Data([0]), nil)
            return nil
        }
        return provider
    }

    /// A paired computer's small glyph and name, shown only with several paired.
    private func computerLabel(_ name: String, host: String) -> some View {
        HStack(spacing: 3) {
            Image(systemName: "desktopcomputer").font(.caption2).accessibilityHidden(true)
            Text(name).font(.caption).lineLimit(1)
        }
        .foregroundStyle(.secondary)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("sidebar-computer-label:" + host)
    }

    private func projectRow(host: String, project: ProjectSummary, isExpanded: Bool, isSelected: Bool) -> some View {
        let key = SidebarProjection.expansionKey(host: host, project: project.id)
        return HStack(spacing: 0) {
            // The folder is the disclosure control: closed when collapsed, open when expanded.
            Button { setExpanded(key, !isExpanded) } label: {
                FolderGlyph(isOpen: isExpanded, dashed: project.isNoProject).foregroundStyle(.primary)
                    .dynamicTypeSize(...DynamicTypeSize.accessibility1)
                    .frame(width: iconColumn)
                    .padding(.leading, Self.iconLeading).padding(.trailing, Self.iconGap)
                    .frame(minHeight: 40).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(isExpanded ? "Collapse \(project.name)" : "Expand \(project.name)")
            .accessibilityValue(isExpanded ? "Expanded" : "Collapsed")
            .accessibilityIdentifier("project-disclosure:" + project.id)
            Button {
                setExpanded(key, true)
                shell.newChat(host: host, destination: .project(id: project.id))
            } label: {
                HStack(spacing: 8) {
                    // The name keeps its width; the computer label (the extra) shortens first.
                    Text(project.name).lineLimit(nameLines).layoutPriority(1)
                    Spacer(minLength: 4)
                    if project.isPinned { Image(systemName: "pin").font(.caption).foregroundStyle(.secondary).accessibilityLabel("Pinned") }
                    if let computer = presenter.computerNames[host] { computerLabel(computer, host: host) }
                }.contentShape(Rectangle()).frame(minHeight: 40)
            }
            .buttonStyle(.plain)
            .accessibilityHint("Starts a new chat in this Project")
            .accessibilityIdentifier("project-row:" + project.id)
            .modifier(SidebarMoveActions(row: .project(hostID: host, project: project, isExpanded: isExpanded, isSelected: isSelected), presenter: presenter, enabled: !archiveSelection.isSelecting))
            .contextMenu {
                Button(project.isPinned ? "Unpin" : "Pin", systemImage: project.isPinned ? "pin.slash" : "pin") {
                    Task { await update(host, project, ["isPinned": !project.isPinned]) }
                }
                Button("Edit Project", systemImage: "pencil") { shell.show(.projectSettings(host: host, project: project.id)) }
                if let folder = project.primaryFolder {
                    Button("Copy folder path", systemImage: "folder") {
                        UIPasteboard.general.string = folder.path
                        UINotificationFeedbackGenerator().notificationOccurred(.success)
                    }
                    .accessibilityIdentifier("copy-project-folder")
                }
                if model(host)?.projects.supportsArchiveList == true {
                    Button("Archived chats", systemImage: "archivebox") {
                        shell.show(.archivedThreads(host: host, project: project.id))
                    }
                    .accessibilityIdentifier("project-archived-threads")
                }
                Button("Hide from sidebar", systemImage: "eye.slash") {
                    hide(host, project)
                }
                .accessibilityIdentifier("hide-project")
            }
        }
        .itemProvider { archiveSelection.isSelecting ? nil : sidebarDragProvider() }
        .listRowBackground(selectionFill(isSelected, theme: theme))

    }

    /// A narrow phone leaves a wider gap before a thread's status than a
    /// regular-width sidebar does.
    private var statusGap: CGFloat { horizontalSizeClass == .compact ? 14 : 10 }
    /// The status grows with the text up to about AX1, so at the largest sizes the
    /// spinner stays inside its row and leaves the title its width.
    @ScaledMetric(relativeTo: .body) private var scaledStatusSlot: CGFloat = 20
    private var statusSlot: CGFloat { min(scaledStatusSlot, 32) }
    /// The unread and needs-you dots share one size, scaled with the text.
    @ScaledMetric(relativeTo: .body) private var scaledStatusDot: CGFloat = 9
    private var statusDot: CGFloat { min(scaledStatusDot, 14) }
    /// The small spinner is drawn for the default text size; this scales it with Dynamic Type.
    private var spinnerScale: CGFloat { statusSlot / 20 }

    private func threadPlacement(_ row: SidebarRow) -> (subtitle: String?, leading: CGFloat, prefix: String) {
        switch row {
        case .pinned(_, _, let project, _, _, _):
            return (project, 4, "pinned-thread:")
        case .chat(_, _, let project, _, _):
            return (project, 4, "chat-thread:")
        default:
            return (nil, childIndent, "project-thread:")
        }
    }

    private func threadRow(host: String, projectID: String, thread: ProjectThreadSummary, isSelected: Bool, reorderRow: SidebarRow,
                           subtitle: String?, leading: CGFloat, prefix: String) -> some View {
        let key = SidebarPresenter.needsInputKey(host: host, thread: thread.reference)
        let waiting = presenter.needsInput.contains(key)
        let waitingLabel = thread.waitsOnMacPermission ? ProjectThreadSummary.macPermissionWait
            : presenter.waitingOnMac.contains(key) ? "Waiting for you on your computer" : "Needs your answer"
        let currentStep = waiting || model(host)?.macConnected != true ? nil : thread.currentStep
        // One answer for the row: the thread's own history and the Mac's check
        // join the list's read (`ThreadActivity`), so a row can't outlast its thread.
        let activity = model(host)?.rowActivity(thread) ?? (thread.isWorking ? .working(.row, since: nil) : .idle)
        let runningLabel = activity.isBusy ? currentStep ?? "Working"
            : thread.backgroundWork == true ? ProjectThreadSummary.backgroundWorkLabel : nil
        // Only the person at the Mac can answer a macOS dialog, so the row says so.
        let detail = thread.rowDetail(project: subtitle, waitingLabel: waiting ? waitingLabel : nil,
                                      macConnected: model(host)?.macConnected == true)
        let spokenLocation = thread.branch.map { "Branch " + $0 + (thread.worktreeLabel == nil ? "" : " in a worktree") }
            ?? thread.worktreeLabel.map { "Worktree " + $0 }
        let target = archiveTarget(host, projectID, thread)
        let isChecked = archiveSelection.contains(host + "/" + thread.reference)
        return ChatNotificationReader(model: model(host), conversation: thread.conversationId) { notification in
        VStack(alignment: .leading, spacing: 6) {
            Button {
                if archiveSelection.isSelecting {
                    if let target { archiveSelection.toggle(target) }
                } else { open(host, projectID, thread) }
            } label: {
                PreparedChatRow(title: thread.title, isComputerSession: thread.isComputerSession == true,
                                automationSymbol: thread.automationRun?.symbolName, automationFailed: thread.automationRun?.failed == true,
                                notificationSymbol: notification.badgeSymbolName, notificationLabel: notification.badgeLabel,
                                leading: leading) {
                    if archiveSelection.isSelecting {
                        Image(systemName: isChecked ? "checkmark.circle.fill" : "circle")
                            .foregroundStyle(isChecked ? theme.accent : Color.secondary)
                            .accessibilityHidden(true)
                    }
                } provider: {
                    SidebarThreadDetails(family: thread.family, project: detail.project,
                                         computer: subtitle == nil ? nil : presenter.computerNames[host],
                                         location: detail.location, activity: detail.status)
                } status: {
                    // The status keeps a fixed slot after a clear gap, so a long
                    // title ends before it on the narrowest phone.
                    if busyThread == thread.reference || waiting || activity.isBusy || thread.backgroundWork == true || thread.hasUnread {
                        // Spinner and dots share one centred column. Waiting on the owner
                        // is a dot in the theme's accent, not an alarm: unread stays primary.
                        Group {
                            if busyThread == thread.reference { ProgressView().controlSize(.small).scaleEffect(spinnerScale) }
                            else if waiting {
                                Circle().fill(theme.accent).frame(width: statusDot, height: statusDot)
                                    .accessibilityLabel(waitingLabel)
                            }
                            else if let runningLabel { ProgressView().controlSize(.small).scaleEffect(spinnerScale).accessibilityLabel(runningLabel) }
                            else { Circle().fill(Color.primary).frame(width: statusDot, height: statusDot).accessibilityLabel("Unread") }
                        }
                        .frame(width: statusSlot, height: statusSlot)
                        .padding(.leading, statusGap)
                        .layoutPriority(1)
                        .accessibilityIdentifier("sidebar-thread-status")
                    }
                }
                .modifier(ConversationTextColors())
                .frame(minHeight: archiveSelection.isSelecting ? 44 : 0)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            .disabled(archiveSelection.isSelecting && (target == nil || archiveSelection.isArchiving))
            .accessibilityLabel(archiveSelection.isSelecting ? "Select " + thread.title : thread.title)
            .accessibilityValue([archiveSelection.isSelecting ? (isChecked ? "Selected" : "Not selected") : nil,
                                 thread.family.title, subtitle, spokenLocation, thread.computerSessionLabel, thread.automationRun?.label,
                                 notification.badgeLabel,
                                 busyThread == thread.reference ? "Updating" : waiting ? waitingLabel
                                    : runningLabel ?? (thread.hasUnread ? "Unread" : nil),
                                 waiting || runningLabel != nil ? nil : detail.status]
                .compactMap { $0 }.joined(separator: ", "))
            .accessibilityAddTraits((archiveSelection.isSelecting ? isChecked : isSelected) ? .isSelected : [])
            .accessibilityHint(archiveSelection.isSelecting && target == nil ? "Archiving this chat is unavailable on this computer." : "")
            .accessibilityIdentifier(prefix + thread.reference)
            .modifier(SidebarMoveActions(row: reorderRow, presenter: presenter, enabled: !archiveSelection.isSelecting))
            .contextMenu {
                if !archiveSelection.isSelecting {
                    if let target {
                        Button("Select", systemImage: "checkmark.circle") { archiveSelection.toggle(target) }
                            .disabled(busyThread != nil)
                            .accessibilityIdentifier("select-project-thread")
                    }
                    Button(thread.hasUnread ? "Mark as read" : "Mark as unread", systemImage: thread.hasUnread ? "envelope.open" : "envelope.badge") {
                        setUnread(host, projectID, thread, !thread.hasUnread)
                    }.disabled(busyThread != nil)
                    Button(thread.isPinned ? "Unpin" : "Pin", systemImage: thread.isPinned ? "pin.slash" : "pin") {
                        setPinned(host, projectID, thread, !thread.isPinned)
                    }
                    Button("Rename", systemImage: "pencil") {
                        renameText = thread.title
                        renaming = ThreadTarget(host: host, project: projectID, thread: thread)
                    }
                    if let projects = model(host)?.projects {
                        ChatDetailMenu(override: projects.chatDetailOverride(thread.conversationId)) {
                            setChatDetail(host, projectID, thread, $0)
                        }
                        .disabled(busyThread != nil)
                    }
                    Button("Copy resume command", systemImage: "terminal") { copyResumeCommand(host, projectID, thread) }
                    // The provider's own ID, as Codex and Claude Code show it; a draft
                    // with no first message has none yet.
                    if let native = thread.nativeID {
                        Button(thread.family == .claude ? "Copy session ID" : "Copy thread ID", systemImage: "number") { // provider identity
                            UIPasteboard.general.string = native
                            UINotificationFeedbackGenerator().notificationOccurred(.success)
                        }
                        .accessibilityIdentifier("copy-thread-id")
                    }
                    if model(host)?.projects.canArchive(thread) == true {
                        Button("Archive", systemImage: "archivebox") {
                            if confirmArchive { confirmingThreadArchive = ThreadTarget(host: host, project: projectID, thread: thread) }
                            else { archive(host, projectID, thread) }
                        }
                            .disabled(busyThread != nil || activity.isBusy || model(host)?.macConnected != true)
                            .accessibilityIdentifier("archive-project-thread")
                    }
                }
            }
            if busyOpenFeedback == key {
                Text(busyThread == thread.reference ? "Still opening this chat…" : "Another chat is opening…")
                    .font(.subheadline).foregroundStyle(.secondary)
                    .padding(.leading, leading)
                    .accessibilityIdentifier("chat-open-busy:" + prefix + thread.reference)
            }
        }
        }
        .itemProvider { archiveSelection.isSelecting ? nil : sidebarDragProvider() }
        .listRowBackground(ZStack {
            theme.sidebar
            selectionFill(archiveSelection.isSelecting ? isChecked : isSelected, theme: theme)
        })
    }

    // MARK: Actions

    private func archive(_ host: String, _ projectID: String, _ thread: ProjectThreadSummary) {
        guard busyThread == nil, let model = model(host) else { return }
        let scope = model.assignmentScope
        busyThread = thread.reference
        Task {
            defer { busyThread = nil }
            do {
                let conversation = try await model.projects.archive(projectID, thread: thread)
                guard scope == model.assignmentScope else { return }
                if shell.selectedConversation?.host == host, shell.selectedConversation?.id == conversation {
                    shell.route = .newChat
                }
                undoState.show(.archived(host: host, title: thread.title, conversation: conversation, reference: thread.reference,
                                 alsoIn: model.projects.archiveAlsoIn(thread.family)))
            } catch is CancellationError {
            } catch {
                guard scope == model.assignmentScope else { return }
                failure = "The chat couldn’t be archived. " + managementError(error)
            }
        }
    }

    private func setUnread(_ host: String, _ projectID: String, _ thread: ProjectThreadSummary, _ unread: Bool) {
        guard busyThread == nil, let model = model(host) else { return }
        busyThread = thread.reference
        Task {
            defer { busyThread = nil }
            do {
                let conversation: String
                if let id = thread.conversationId { conversation = id }
                else { conversation = try await model.projects.attach(projectID, thread: thread) }
                try await model.projects.updateConversation(conversation, fields: ["hasUnread": unread])
            } catch is CancellationError {
            } catch {
                failure = "The read status couldn’t be saved. Check \(model.macName) and try again."
            }
        }
    }

    /// A thread with a conversation opens at once from what is saved; only a
    /// provider-only thread waits for attach.
    private func open(_ host: String, _ projectID: String, _ thread: ProjectThreadSummary) {
        guard let model = model(host) else { return }
        if let conversation = thread.conversationId {
            model.projects.noteOpened(conversation)
            shell.open(host: host, conversation: conversation)
            // Saved history opens immediately, including offline. A native
            // session can still need its desktop project repaired (Codex).
            if model.projects.capabilities(thread.family).desktopProjectSync, !thread.reference.hasPrefix("wonder:"),
               model.macConnected == true {
                let scope = model.assignmentScope
                let repairFailure = "Your chat opened, but its Project on your computer couldn’t be updated. Try opening it again."
                Task {
                    do {
                        _ = try await model.projects.attach(projectID, thread: thread)
                        if scope == model.assignmentScope, model.controlErrors[conversation] == repairFailure {
                            model.controlErrors[conversation] = nil
                        }
                    }
                    catch is CancellationError {}
                    catch {
                        guard scope == model.assignmentScope, !model.accessEnded else { return }
                        model.controlErrors[conversation] = repairFailure
                    }
                }
            }
            return
        }
        let key = SidebarPresenter.needsInputKey(host: host, thread: thread.reference)
        guard busyThread == nil else {
            busyOpenFeedback = key
            UINotificationFeedbackGenerator().notificationOccurred(.warning)
            return
        }
        let scope = model.assignmentScope
        busyThread = thread.reference
        busyOpenFeedback = nil
        Task {
            defer { busyThread = nil; busyOpenFeedback = nil }
            do {
                let conversation = try await model.projects.attach(projectID, thread: thread)
                guard !Task.isCancelled, scope == model.assignmentScope, !model.accessEnded else { return }
                model.projects.noteOpened(conversation)
                shell.open(host: host, conversation: conversation)
            } catch is CancellationError {
            } catch {
                guard !Task.isCancelled, scope == model.assignmentScope, !model.accessEnded else { return }
                failure = model.macConnected == true ? ChatOpenFailure(error, computer: model.macName).message : "\(model.macName) is offline."
            }
        }
    }

    private func setPinned(_ host: String, _ projectID: String, _ thread: ProjectThreadSummary, _ value: Bool) {
        guard let model = model(host) else { return }
        Task {
            do { try await model.projects.setPinned(projectID, thread: thread, value) }
            catch is CancellationError {}
            catch { failure = "The pin couldn’t be saved. Try again." }
        }
    }

    private func rename(_ target: ThreadTarget) {
        let title = renameText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty, title != target.thread.title, let model = model(target.host) else { return }
        Task {
            do { try await model.projects.rename(target.project, thread: target.thread, to: title) }
            catch is CancellationError {}
            catch { failure = "The new name couldn’t be saved. Try again." }
        }
    }

    /// Shows `level` in this chat only; nil follows the provider again. A chat the
    /// Mac hasn't opened yet is attached first, like marking it unread.
    private func setChatDetail(_ host: String, _ projectID: String, _ thread: ProjectThreadSummary, _ level: ChatDetailLevel?) {
        guard busyThread == nil, let model = model(host) else { return }
        busyThread = thread.reference
        Task {
            defer { busyThread = nil }
            do {
                let conversation: String
                if let id = thread.conversationId { conversation = id }
                else { conversation = try await model.projects.attach(projectID, thread: thread) }
                try await model.projects.setChatDetail(chat: conversation, to: level)
            } catch is CancellationError {
            } catch {
                failure = "What this chat shows couldn’t be saved. Check \(model.macName) and try again."
            }
        }
    }

    private func copyResumeCommand(_ host: String, _ projectID: String, _ thread: ProjectThreadSummary) {
        guard busyThread == nil, let model = model(host) else { return }
        busyThread = thread.reference
        Task {
            defer { busyThread = nil }
            do {
                let conversation = try await model.projects.attach(projectID, thread: thread)
                let continuation = try await model.projects.continuation(conversation)
                guard let command = continuation.options.first?.command else {
                    failure = continuation.unavailableText
                    return
                }
                UIPasteboard.general.string = command
                UINotificationFeedbackGenerator().notificationOccurred(.success)
            } catch PairingFailure.response(409) {
                failure = "Send the first message before continuing on your computer."
            } catch is CancellationError {
            } catch {
                failure = "The resume command couldn’t be loaded. Check \(model.macName) and try again."
            }
        }
    }

    private func hide(_ host: String, _ project: ProjectSummary) {
        guard let model = model(host) else { return }
        Task {
            do { try await undoState.hide(project, host: host, in: model.projects) }
            catch is CancellationError {}
            catch { failure = managementError(error) }
        }
    }

    @discardableResult private func update(_ host: String, _ project: ProjectSummary, _ fields: [String: Any]) async -> Bool {
        guard let model = model(host) else { return false }
        do { _ = try await model.projects.update(project.id, fields: fields); return true }
        catch { failure = managementError(error); return false }
    }
}

/// Reads a chat's notification choice from its computer's model, which the
/// sidebar does not otherwise observe, and loads it when the row first shows.
private struct ChatNotificationReader<Content: View>: View {
    let model: ConnectionModel?
    let conversation: String?
    @ViewBuilder let content: (ChatNotificationLevel) -> Content
    var body: some View {
        if let model, let conversation {
            Observed(model: model, conversation: conversation, content: content)
        } else {
            content(.default)
        }
    }
    private struct Observed: View {
        @ObservedObject var model: ConnectionModel
        let conversation: String
        let content: (ChatNotificationLevel) -> Content
        var body: some View {
            content(model.chatNotifications.level(conversation))
                .task(id: conversation) { await model.loadChatNotifications([conversation]) }
        }
    }
}

/// One sidebar row, built in its own body. The closure builds the row's content
/// from SidebarView (type-erased), so the ~15-way row switch is not part of
/// SidebarView.body's generic type or stack frame on the physical phone.
private struct SidebarRowView: View {
    let row: SidebarRow
    let content: (SidebarRow) -> AnyView

    var body: some View { content(row) }
}

/// A restrained fill for the one selected row; other rows stay on the sidebar.
/// Headers, notices and actions take the sidebar's colour; the system's row fill
/// would draw them black under a themed sidebar. Thread and project rows set their own.
private struct ClearRowBackground: ViewModifier {
    let row: SidebarRow
    @ViewBuilder func body(content: Content) -> some View {
        switch row {
        case .pinned, .thread, .project, .chat: content
        default: content.listRowBackground(Color.clear)
        }
    }
}

/// Explicit palette colors keep the small metadata readable over a selected
/// tinted row; hierarchical secondary text otherwise attenuates it again.
private struct SidebarThreadDetails: View {
    let family: AgentFamily
    let project: String?
    let computer: String?
    let location: String?
    let activity: String?
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 4) {
                details
                if let activity, !activity.isEmpty {
                    if project != nil || computer != nil || location != nil { separator }
                    Text(activity).foregroundStyle(secondary).lineLimit(1)
                }
            }
            if typeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 4) { details }
                    if let activity { Text(activity).foregroundStyle(secondary).lineLimit(2) }
                }
            } else {
                HStack(spacing: 4) { details }
            }
        }.font(.caption)
    }

    private var secondary: Color { theme.isDefault ? .secondary : theme.secondaryText }
    private var separator: some View { Text("·").foregroundStyle(secondary).accessibilityHidden(true) }
    @ViewBuilder private var details: some View {
        ProviderIcon(family: family, size: 12)
            .padding(1).background(.white, in: RoundedRectangle(cornerRadius: 3))
        if let project { Text(project).foregroundStyle(secondary).lineLimit(typeSize.isAccessibilitySize ? 2 : 1) }
        if let computer {
            if project != nil { separator }
            HStack(spacing: 3) {
                Image(systemName: "desktopcomputer").accessibilityHidden(true)
                Text(computer).lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
            }.foregroundStyle(secondary)
        }
        if let location {
            if project != nil || computer != nil { separator }
            Text(location).foregroundStyle(theme.accent).lineLimit(typeSize.isAccessibilitySize ? 2 : 1).truncationMode(.middle)
        }
    }
}

func selectionFill(_ selected: Bool, theme: WonderTheme) -> some View {
    RoundedRectangle(cornerRadius: 8)
        .fill(selected ? SidebarSelection.fill(theme) : Color.clear)
        .padding(.horizontal, 6)
}

/// The selected row's fill. The System theme keeps iOS's own fill; any other
/// theme tints its sidebar toward the accent, so a navy sidebar selects in
/// navy-blue rather than a grey slab. Photo themes draw an opaque sidebar, so
/// the same blend holds over their photos.
enum SidebarSelection {
    static func fill(_ theme: WonderTheme) -> Color {
        theme.isDefault ? Color(uiColor: .tertiarySystemFill) : Color(hex: hex(theme.palette))
    }

    /// The accent mixed into the sidebar only while both title and metadata
    /// retain 4.5:1 contrast over the selection.
    static func hex(_ palette: ThemePalette) -> UInt32 {
        for amount in [0.22, 0.16, 0.1] {
            let fill = mix(palette.sidebar, palette.accent, amount)
            if ThemePalette.contrast(palette.primaryText, fill) >= 4.5,
               ThemePalette.contrast(palette.secondaryText, fill) >= 4.5 { return fill }
        }
        return mix(palette.sidebar, palette.primaryText, 0.08)
    }

    private static func mix(_ a: UInt32, _ b: UInt32, _ amount: Double) -> UInt32 {
        func channel(_ shift: UInt32) -> UInt32 {
            let x = Double((a >> shift) & 0xFF), y = Double((b >> shift) & 0xFF)
            return UInt32((x + (y - x) * amount).rounded()) << shift
        }
        return channel(16) | channel(8) | channel(0)
    }
}

/// A folder outline drawn to match the sidebar's other icons, open when its
/// project's threads show. SF Symbols has no open folder. Sized to the body
/// `folder` symbol (measured at 17 pt: 18.8 × 15 pt of ink, 1.3 pt strokes).
/// Cupola's app icon at the top of the sidebar, where the word used to be.
/// It names the list for VoiceOver as the heading did.
private struct CupolaMark: View {
    @ScaledMetric(relativeTo: .title3) private var scaledSize: CGFloat = 28
    private var size: CGFloat { min(scaledSize, 40) }
    var body: some View {
        Image("CupolaMark").resizable().interpolation(.high).scaledToFit()
            .frame(width: size, height: size)
            .clipShape(RoundedRectangle(cornerRadius: size * 0.225, style: .continuous))
            .accessibilityRemoveTraits(.isImage)
            .accessibilityAddTraits(.isHeader)
            .accessibilityLabel("Cupola")
            .accessibilityIdentifier("sidebar-cupola")
    }
}

private struct FolderGlyph: View {
    let isOpen: Bool
    /// The No project folder is outlined with dashes: a folder that isn't one.
    var dashed = false
    @ScaledMetric(relativeTo: .body) private var point: CGFloat = 17
    var body: some View {
        let k = point / 17
        FolderShape(isOpen: isOpen)
            .stroke(style: StrokeStyle(lineWidth: 1.3 * k, lineCap: .round, lineJoin: .round, dash: dashed ? [2.6 * k, 2.6 * k] : []))
            .frame(width: 21 * k, height: 18.3 * k)
    }
}

/// Drawn in a 24 × 20 box and scaled to fit.
private struct FolderShape: Shape {
    let isOpen: Bool
    func path(in rect: CGRect) -> Path {
        let sx = rect.width / 24, sy = rect.height / 20
        func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: rect.minX + x * sx, y: rect.minY + y * sy) }
        var path = Path()
        // Back: left side, tab and top edge.
        path.move(to: p(2, 15.5))
        path.addLine(to: p(2, 4.5))
        path.addQuadCurve(to: p(4, 2.5), control: p(2, 2.5))
        path.addLine(to: p(8.6, 2.5))
        path.addLine(to: p(10.6, 4.8))
        if isOpen {
            path.addLine(to: p(18, 4.8))
            path.addQuadCurve(to: p(20, 6.8), control: p(20, 4.8))
            path.addLine(to: p(20, 8.6))
            // The front panel tips forward.
            path.move(to: p(2, 15.5))
            path.addQuadCurve(to: p(4, 17.5), control: p(2, 17.5))
            path.addLine(to: p(18.6, 17.5))
            path.addLine(to: p(22.4, 8.6))
            path.addLine(to: p(6.4, 8.6))
            path.addLine(to: p(2.6, 16.4))
        } else {
            path.addLine(to: p(20, 4.8))
            path.addQuadCurve(to: p(22, 6.8), control: p(22, 4.8))
            path.addLine(to: p(22, 15.5))
            path.addQuadCurve(to: p(20, 17.5), control: p(22, 17.5))
            path.addLine(to: p(4, 17.5))
            path.addQuadCurve(to: p(2, 15.5), control: p(2, 17.5))
            path.move(to: p(2, 8))
            path.addLine(to: p(22, 8))
        }
        return path
    }
}

/// Message search across the paired Macs for the sidebar's search text.
/// Typing waits briefly before asking; a newer query cancels older requests.
@MainActor final class SidebarMessageSearch: ObservableObject {
    struct Source {
        let host: String
        let name: String
        let model: ConnectionModel
    }
    struct HostResults: Identifiable {
        let id: String
        let name: String
        var page = MessageSearchPage()
        var loading = true
        var loadingMore = false
        var failed = false
    }
    @Published private(set) var query = ""
    @Published private(set) var hosts: [HostResults] = []
    private var key = ""
    private var tasks: [String: Task<Void, Never>] = [:]

    var hasContent: Bool { hosts.contains { $0.loading || $0.failed || !$0.page.results.isEmpty } }

    func update(query raw: String, sources: [Source]) {
        let query = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        let key = query + "\n" + sources.map(\.host).joined(separator: ",")
        guard key != self.key else { return }
        self.key = key
        tasks.values.forEach { $0.cancel() }
        tasks = [:]
        self.query = query
        // One letter matches nearly everything; the Mac also needs a word.
        guard query.count >= 2, query.utf8.count <= 256, query.contains(where: { $0.isLetter || $0.isNumber }) else {
            hosts = []
            return
        }
        hosts = sources.map { HostResults(id: $0.host, name: $0.name) }
        for source in sources { start(source, more: false, delay: .milliseconds(350)) }
    }

    func retry(_ source: Source) {
        guard let index = hosts.firstIndex(where: { $0.id == source.host }) else { return }
        hosts[index].failed = false
        hosts[index].loading = true
        start(source, more: false, delay: .zero)
    }

    func loadMore(_ source: Source) {
        guard let index = hosts.firstIndex(where: { $0.id == source.host }), !hosts[index].loadingMore,
              hosts[index].page.nextCursor != nil else { return }
        hosts[index].loadingMore = true
        start(source, more: true, delay: .zero)
    }

    private func start(_ source: Source, more: Bool, delay: Duration) {
        let query = self.query
        tasks[source.host]?.cancel()
        tasks[source.host] = Task { [weak self] in
            if delay > .zero { try? await Task.sleep(for: delay) }
            guard let self, !Task.isCancelled else { return }
            let cursor = more ? self.hosts.first(where: { $0.id == source.host })?.page.nextCursor : nil
            let result: Result<MessageSearchPage, Error>
            do { result = .success(try await source.model.searchMessages(query, cursor: cursor)) }
            catch { result = .failure(error) }
            guard !Task.isCancelled, query == self.query,
                  let index = self.hosts.firstIndex(where: { $0.id == source.host }) else { return }
            switch result {
            case .success(let page):
                if more { self.hosts[index].page.append(page) } else { self.hosts[index].page = page }
            case .failure(let error):
                if error is CancellationError { return }
                // A failed later page keeps what is shown; only a first page fails the section.
                if !more { self.hosts[index].failed = true }
            }
            self.hosts[index].loading = false
            self.hosts[index].loadingMore = false
        }
    }
}
