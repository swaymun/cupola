import SwiftUI
import WonderPairing

/// One local catalog for search, navigation, focus and the row coverage contract.
/// No connection model or network work is involved in matching a query.
enum SettingsSearch {
    enum Scope: String, Sendable { case phone, computer }
    enum Screen: Hashable, Sendable {
        case settings, theme, font, pills, voice, computer, connectedApps, routes, defaults, secrets
        case provider(AgentFamily), defaultModel(AgentFamily), instruction(AgentFamily)
    }
    enum Anchor: String, Sendable {
        case computers, chats, confirmations, about, pills, pillsReset, notifications, account, notificationPrivacy
        case computer, newChatDefault, remoteAccess, providers, connectedApps, connectedAppsAgents, projects, autoArchive, continuation, skills, advanced
        case messageFonts, codeFonts, voiceSource, voiceKey, voiceTalking, voiceAsk, voiceAnnounce, voiceLimits, voiceTranscripts
        case defaultProviders, defaultFields, hiddenModels, providerStatus, claudeApp, runtime, rewind, providerAdditions, chatDetail, instructionEnabled, instructionText, routes
        var id: String { "settings-section:" + rawValue }
    }
    static func themeSectionID(_ group: ThemeGroup) -> String { "settings-section:theme:" + (group.title ?? "system") }
    struct DestinationKey: Hashable, Sendable {
        let screen: Screen
        let hostID: String?
    }
    struct Row: Identifiable, Sendable {
        let id: String
        let title: String
        let synonyms: [String]
        let path: String
        let screen: Screen
        let scope: Scope
        /// Link results open their destination and focus its first relevant control.
        var focusID: String {
            switch id {
            case "settings-theme": "theme-option:" + WonderThemeCatalog.defaultID
            case "settings-font": "message-font:system"
            case "settings-pills": "pills-row:computer"
            case "settings-voice": "voice-talk-mode"
            case "settings-secrets": "secrets-list"
            case "settings-default-models": "provider-default-model:codex"
            case "connection-connected-apps": "t3code-status"
            case "connection-details": "connection-route:home"
            default: id.hasPrefix("settings-provider:") ? "provider:" + id.dropFirst("settings-provider:".count) : id.hasPrefix("custom-instruction:") ? "custom-instruction-text:" + id.dropFirst("custom-instruction:".count) : id
            }
        }
        /// The list knows its section identity before lazily creating the row.
        /// Reveal that section first, then the row's exact accessibility anchor.
        var scrollAnchor: String {
            switch screen {
            case .settings:
                if id == "settings-add-computer" { return Anchor.computers.id }
                if id.hasPrefix("settings-confirm-") { return Anchor.confirmations.id }
                if id.hasPrefix("account-") || ["settings-notifications", "settings-live-activities"].contains(id) { return Anchor.notifications.id }
                if ["settings-follow-up", "settings-take-over-warning", "settings-open-links"].contains(id) { return Anchor.chats.id }
                return Anchor.about.id
            case .theme:
                let themeID = String(focusID.dropFirst("theme-option:".count))
                return WonderThemeCatalog.groups.first { $0.themes.contains { $0.id == themeID } }.map(SettingsSearch.themeSectionID) ?? focusID
            case .font: return focusID.hasPrefix("code-font:") ? Anchor.codeFonts.id : Anchor.messageFonts.id
            case .pills: return id == "pills-reset" ? Anchor.pillsReset.id : Anchor.pills.id
            case .voice:
                if focusID == "voice-key-computer" { return Anchor.voiceSource.id }
                if focusID.hasPrefix("voice-key-") { return Anchor.voiceKey.id }
                if focusID == "voice-talk-mode" || id == "settings-voice" { return Anchor.voiceTalking.id }
                if focusID.hasPrefix("voice-ask-") { return Anchor.voiceAsk.id }
                if focusID == "voice-announce" { return Anchor.voiceAnnounce.id }
                if focusID.contains("transcripts") { return Anchor.voiceTranscripts.id }
                return Anchor.voiceLimits.id
            case .computer:
                if focusID == "computer-start-new-chats-in-t3" { return Anchor.newChatDefault.id }
                if focusID.hasPrefix("auto-archive-") { return Anchor.autoArchive.id }
                if focusID.hasPrefix("continue-") { return Anchor.continuation.id }
                if focusID.hasPrefix("wonder-skill:") { return Anchor.skills.id }
                if focusID == "connection-remote-access" { return Anchor.remoteAccess.id }
                if focusID == "include-worktrees-chats" || focusID == "connection-projects" { return Anchor.projects.id }
                return Anchor.computer.id
            case .connectedApps: return id == "connected-apps-agent-apps" ? Anchor.connectedAppsAgents.id : Anchor.connectedApps.id
            case .defaults: return Anchor.defaultProviders.id
            case .secrets: return "secrets-list"
            case .defaultModel: return id.hasPrefix("hidden-models-") ? Anchor.hiddenModels.id : Anchor.defaultFields.id
            case .provider:
                if focusID.hasPrefix("provider-chat-detail:") { return Anchor.chatDetail.id }
                if focusID.hasPrefix("provider-runtime:") { return Anchor.runtime.id }
                // Rewind and the Claude app setting sit under Runtime: the search lands on its row.
                if focusID.hasPrefix("file-restore:") { return Anchor.runtime.id }
                if focusID.hasPrefix("thread-tools:") { return Anchor.providerAdditions.id }
                if focusID == "claude-app-sync" { return Anchor.runtime.id }
                return Anchor.providerStatus.id
            case .instruction: return focusID.hasPrefix("custom-instruction-enabled:") ? Anchor.instructionEnabled.id : Anchor.instructionText.id
            case .routes: return Anchor.routes.id
            }
        }
        init(_ id: String, _ title: String, _ synonyms: [String] = [], _ path: String,
             _ screen: Screen = .settings, _ scope: Scope = .phone) {
            self.id = id; self.title = title; self.synonyms = synonyms
            self.path = path; self.screen = screen; self.scope = scope
        }
    }
    static let rows: [Row] = {
        var rows: [Row] = [
            Row("computer-start-new-chats-in-t3", "Start new chats in T3 Code", ["default", "new conversation"], "Computer › New chats", .computer, .computer),
            Row("settings-add-computer", "Add computer", ["pair", "connect", "qr"], "Computers"),
            Row("settings-notifications", "Notifications", ["push", "alerts", "banners", "sign out", "delete account"], "Notifications"),
            Row("settings-live-activities", "Live Activities", ["lock screen", "dynamic island", "progress"], "Notifications"),
            Row("settings-secrets", "Secrets", ["vault", "token", "password", "keychain", "api key"], "Secrets", .secrets, .computer),
            Row("settings-default-models", "Default models", ["reasoning", "speed", "model"], "Default models", .defaults, .computer),
            Row("settings-follow-up", "Default send behavior", ["queue", "steer", "follow up", "while a reply runs"], "Chats"),
            Row("settings-take-over-warning", "Warn before taking over", ["handoff", "confirmation"], "Chats"),
            Row("settings-open-links", "Open links", ["browser", "safari"], "Chats"),
            Row("settings-pills", "Pills", ["toolbar", "composer", "buttons"], "Chats", .pills),
            Row("settings-voice", "Voice", ["talk", "speech", "openai", "key"], "Voice", .voice),
            Row("pills-reset", "Restore default order", ["reset", "pills"], "Chats › Pills", .pills),
            Row("account-sign-in-apple", "Sign in with Apple", ["account", "notifications"], "Notifications"),
            Row("account-provider", "Cupola account", ["account", "apple", "sign in"], "Notifications"),
            Row("voice-key-remove", "Remove voice key", ["voice", "openai", "api key"], "Voice › OpenAI key", .voice, .computer),
            Row("voice-delete-transcripts", "Delete saved transcripts", ["voice", "privacy", "history"], "Voice › Transcripts", .voice, .computer),
            Row("settings-confirm-archive", "Confirm before archiving", ["archive", "confirmation"], "Confirmations"),
            Row("settings-confirm-delete", "Confirm before deleting automations", ["delete", "confirmation"], "Confirmations"),
            Row("settings-theme", "Theme", ["appearance", "colour", "color"], "Appearance", .theme),
            Row("settings-font", "Font", ["text size", "typeface", "typography"], "Appearance", .font),
            Row("settings-triage", "Something not working?", ["help", "triage", "diagnose", "problem", "troubleshoot", "support"], "Help"),
            Row("settings-version", "Version", ["build", "about"], "About"),
            Row("settings-privacy", "Privacy policy", ["data"], "About"),
            Row("settings-acknowledgements", "Acknowledgements", ["licenses", "credits"], "About"),
            Row("connection-rename", "Computer name", ["rename"], "Computer", .computer, .computer),
            Row("connection-remote-access", "Reach from anywhere", ["reach", "relay", "remote", "tailscale"], "Computer", .computer, .computer),
            Row("connection-connected-apps", "Connected apps", ["integrations"], "Computer", .connectedApps, .computer),
            Row("connection-projects", "Manage Projects", ["folders"], "Computer › Projects", .computer, .computer),
            Row("include-worktrees-chats", "Worktree chats", ["worktrees", "branches"], "Computer › Projects", .computer, .computer),
            Row("auto-archive-days", "Archive idle chats", ["archive", "auto archive", "days"], "Computer › Auto-archive", .computer, .computer),
            Row("auto-archive-on-merge", "Archive when the pull request merges", ["archive", "merge", "pr"], "Computer › Auto-archive", .computer, .computer),
            Row("continue-after-limit", "Resume after usage limits", ["quota", "continue"], "Computer › Continuing work", .computer, .computer),
            Row("continue-after-restart", "Continue after restarts", ["crash", "recovery"], "Computer › Continuing work", .computer, .computer),
            Row("connection-details", "Connection details", ["routes", "network", "address"], "Computer › Advanced", .routes, .computer),
            Row("connection-route:home", "Home network", ["wifi", "wi fi", "address", "routes"], "Computer › Connection details", .routes, .computer),
            Row("connection-route:tailscale", "Tailscale", ["tailnet", "routes", "address"], "Computer › Connection details", .routes, .computer),
            Row("connection-route:relay", "Cupola relay", ["relay", "routes"], "Computer › Connection details", .routes, .computer),
            Row("t3code-status", "T3 Code", ["t3", "connect t3"], "Computer › Connected apps", .connectedApps, .computer),
            Row("claude-app-sync", ClaudeAppSettings.title, ["sync", "claude", "history", "conversations"], "Computer › Claude › Runtime", .provider(.claude), .computer),
            Row("computer-use-cua-driver", "Control this computer", ["computer use", "remote control", "driver"], "Computer › Connected apps", .connectedApps, .computer),
            Row("connected-apps-agent-apps", "Apps agents can use", ["integrations", "tools"], "Computer › Connected apps", .connectedApps, .computer),
            Row("wonder-skill:wonder-share-to-phone", "Share pages to your phone", ["skills", "html", "artifacts"], "Computer › Cupola skills", .computer, .computer),
            Row("wonder-skill:wonder-phone-replies", "Phone-friendly replies", ["skills", "formatting", "concise"], "Computer › Cupola skills", .computer, .computer),
            Row("wonder-skill:wonder-phone-questions", "Questions you can tap to answer", ["skills", "questions"], "Computer › Cupola skills", .computer, .computer),
            Row("voice-key-computer", "Use the key on", ["voice", "openai", "key", "computer"], "Voice › OpenAI key", .voice),
            Row("voice-key-add", "Add a key", ["voice", "openai", "api key"], "Voice › OpenAI key", .voice, .computer),
            Row("voice-talk-mode", "Talking", ["voice", "hands free", "hold to talk"], "Voice", .voice),
            Row("voice-ask-send", "Sending messages", ["voice", "ask before", "confirm"], "Voice › Ask before", .voice),
            Row("voice-ask-start-thread", "Starting chats", ["voice", "ask before"], "Voice › Ask before", .voice),
            Row("voice-ask-stop", "Stopping replies", ["voice", "ask before"], "Voice › Ask before", .voice),
            Row("voice-ask-approval", "Answering approvals", ["voice", "ask before"], "Voice › Ask before", .voice),
            Row("voice-ask-create-project", "Creating Projects", ["voice", "ask before"], "Voice › Ask before", .voice),
            Row("voice-announce", "Announcements", ["voice", "announce", "tell me", "finished", "approval", "question"], "Voice › Announcements", .voice),
            Row("voice-session-limit", "Session length", ["voice", "minutes", "limit"], "Voice › Limits", .voice, .computer),
            Row("voice-daily-minutes", "Daily minutes", ["voice", "limit"], "Voice › Limits", .voice, .computer),
            Row("voice-daily-dollars", "Daily spending", ["voice", "budget", "cost"], "Voice › Limits", .voice, .computer),
            Row("voice-keep-transcripts", "Keep transcripts", ["voice", "history", "privacy"], "Voice › Transcripts", .voice, .computer),
        ]
        rows += WonderThemeCatalog.themes.map { theme in
            Row("theme-option:" + theme.id, theme.name,
                ["theme", "appearance"] + (theme.id == "cupola-dark" ? ["dark"] : theme.id == "cupola-light" ? ["light"] : []),
                "Appearance › Theme", .theme)
        }
        rows += MessageFont.allCases.map { Row("message-font:" + $0.rawValue, $0.name + " message font", ["font", "typeface"], "Appearance › Font", .font) }
        rows += CodeFont.allCases.map { Row("code-font:" + $0.rawValue, $0.name + " code font", ["font", "monospace"], "Appearance › Font", .font) }
        rows += ComposerPill.layoutCases.map { Row("pills-row:" + $0.rawValue, $0.title + " pill", ["pills", "toolbar"], "Chats › Pills", .pills) }
        for family in AgentFamily.allCases {
            rows += [
                Row("settings-provider:" + family.rawValue, family.title, ["provider", "sign in", "account"], "Computer › Providers", .provider(family), .computer),
                Row("provider:" + family.rawValue, family.title + " sign-in", ["provider", "account", "status"], "Computer › " + family.title, .provider(family), .computer),
                Row("provider-default-model:" + family.rawValue, family.title + " default models", ["model", "reasoning", "speed"], "Default models", .defaults, .computer),
                Row("default-model-" + family.rawValue, family.title + " model", ["default model"], "Default models › " + family.title, .defaultModel(family), .computer),
                Row("default-effort-" + family.rawValue, family.title + " reasoning effort", ["thinking", "default model"], "Default models › " + family.title, .defaultModel(family), .computer),
                Row("custom-instruction:" + family.rawValue, family.title + " custom instructions", ["prompt", "instructions"], "Computer › " + family.title, .instruction(family), .computer),
                Row("custom-instruction-enabled:" + family.rawValue, family.title + " instruction enabled", ["instructions", "prompt"], "Computer › " + family.title + " › Custom instruction", .instruction(family), .computer),
                Row("custom-instruction-text:" + family.rawValue, family.title + " instruction text", ["instructions", "prompt"], "Computer › " + family.title + " › Custom instruction", .instruction(family), .computer),
                Row("file-restore:" + family.rawValue, "Let " + family.title + " restore files when you rewind", ["undo", "restore", "file restore", "backups"], "Computer › " + family.title + " › Runtime", .provider(family), .computer),
                Row("provider-runtime:" + family.rawValue, family.title + " runtime", ["program", "binary", "path", "arguments", "environment", "variables", "home folder", "api key"], "Computer › " + family.title, .provider(family), .computer),
                Row("thread-tools:" + family.rawValue, family.title + " Cupola thread tools", ["tools"], "Computer › " + family.title, .provider(family), .computer),
                Row("hidden-models-" + family.rawValue, family.title + " hidden models", ["hide models", "models", "delegate"], "Default models › " + family.title, .defaultModel(family), .computer),
                Row("default-speed-" + family.rawValue, family.title + " speed", ["fast", "default model"], "Default models › " + family.title, .defaultModel(family), .computer),
            ]
            for level in ChatDetailLevel.allCases {
                rows.append(Row("provider-chat-detail:" + family.rawValue + ":" + level.rawValue, family.title + " " + level.title,
                    ["chat detail", "thinking", "commentary", "activity"], "Computer › " + family.title + " › Chat detail", .provider(family), .computer))
            }
        }
        return rows
    }()

    private struct SearchEntry: Sendable {
        let row: Row
        let title: String
        let aliases: [String]
        let terms: [String]
    }
    private static let searchEntries: [SearchEntry] = rows.map { row in
        let title = normalized(row.title)
        let aliases = row.synonyms.map(normalized)
        return SearchEntry(row: row, title: title, aliases: aliases, terms: [title, normalized(row.path)] + aliases)
    }
    static func matches(_ query: String) -> [Row] {
        let query = normalized(query)
        guard !query.isEmpty else { return [] }
        let words = query.split(separator: " ").map(String.init)
        let ranked = searchEntries.compactMap { entry -> (Row, Int)? in
            guard words.allSatisfy({ word in entry.terms.contains { $0.contains(word) } }) else { return nil }
            let score = entry.title == query ? 0 : entry.title.hasPrefix(query) ? 1 : entry.aliases.contains(query) ? 2 : entry.title.contains(query) ? 3 : 4
            return (entry.row, score)
        }.sorted { $0.1 == $1.1 ? $0.0.title.localizedStandardCompare($1.0.title) == .orderedAscending : $0.1 < $1.1 }.map(\.0)
        var destinations: Set<MatchDestination> = []
        return ranked.filter { destinations.insert(MatchDestination(screen: $0.screen, scope: $0.scope, focusID: $0.focusID)).inserted }
    }
    private struct MatchDestination: Hashable {
        let screen: Screen
        let scope: Scope
        let focusID: String
    }
    private static func normalized(_ text: String) -> String {
        let folded = text.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: Locale(identifier: "en_US_POSIX"))
        return folded.unicodeScalars.map { scalar in
            if CharacterSet.alphanumerics.contains(scalar) { return String(scalar) }
            return scalar.value == 0x27 || scalar.value == 0x2019 ? "" : " "
        }.joined().split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }
    /// The header over settings kept on every paired computer: their names while
    /// that stays short, "All computers" once there are more than two.
    static func computersScopeLabel(_ names: [String]) -> String {
        names.count > 2 ? "All computers" : scopeLabel(.computer, computer: names.formatted(.list(type: .and)))
    }
    static func scopeLabel(_ scope: Scope, computer: String? = nil) -> String {
        if scope == .phone { return "This device" }
        let name = computer?.trimmingCharacters(in: .whitespacesAndNewlines)
        return "On " + ((name?.isEmpty == false ? name : nil) ?? "your computer")
    }
}

struct SettingsSearchTarget: Identifiable, Hashable, Sendable {
    let rowID: String
    let hostID: String?
    var focusOverride: String? = nil
    var focusID: String { focusOverride ?? SettingsSearch.rows.first { $0.id == rowID }?.focusID ?? rowID }
    var destinationKey: SettingsSearch.DestinationKey? {
        guard let row = SettingsSearch.rows.first(where: { $0.id == rowID }) else { return nil }
        // Moving between fields preserves the form's owner and any unsaved text.
        return SettingsSearch.DestinationKey(screen: row.screen, hostID: hostID)
    }
    // Each selection owns its focus lifetime, including a repeated selection.
    let id = UUID()
}

private struct SettingsViewportKey: EnvironmentKey { static let defaultValue = CGRect.zero }
private struct SettingsFocusKey: EnvironmentKey { static let defaultValue: SettingsSearchTarget? = nil }
private struct SettingsFocusModelKey: EnvironmentKey { static let defaultValue: SettingsSearchFocusModel? = nil }
extension EnvironmentValues {
    var settingsSearchViewport: CGRect {
        get { self[SettingsViewportKey.self] }
        set { self[SettingsViewportKey.self] = newValue }
    }
    var settingsSearchFocusModel: SettingsSearchFocusModel? {
        get { self[SettingsFocusModelKey.self] }
        set { self[SettingsFocusModelKey.self] = newValue }
    }
    var settingsSearchTarget: SettingsSearchTarget? {
        get { self[SettingsFocusKey.self] }
        set { self[SettingsFocusKey.self] = newValue }
    }
}

/// The destination owns expiry, so cell recycling or a scroll correction cannot
/// cancel a row's fade and leave a permanent highlight.
@MainActor final class SettingsSearchFocusModel: ObservableObject {
    @Published private(set) var locatedID: UUID?
    @Published private(set) var highlightedID: UUID?
    private var expiry: Task<Void, Never>?
    private let waitForExpiry: @Sendable () async throws -> Void
    init(waitForExpiry: @escaping @Sendable () async throws -> Void = { try await Task.sleep(for: .seconds(3)) }) {
        self.waitForExpiry = waitForExpiry
    }
    func reveal(_ request: UUID, reduceMotion: Bool) {
        guard locatedID != request else { return }
        expiry?.cancel()
        locatedID = request; highlightedID = request
        expiry = Task { @MainActor [weak self, waitForExpiry] in
            do { try await waitForExpiry() } catch { return }
            guard let self, self.highlightedID == request else { return }
            if reduceMotion { self.highlightedID = nil }
            else { withAnimation(.easeOut(duration: 0.2)) { self.highlightedID = nil } }
        }
    }
    func cancel() { expiry?.cancel(); expiry = nil; highlightedID = nil }
    deinit { expiry?.cancel() }
}

/// The existing accessibility ID also becomes a stable scroll/focus anchor.
private struct SettingsRowFocus: ViewModifier {
    let id: String
    @Environment(\.settingsSearchTarget) private var target
    @Environment(\.settingsSearchFocusModel) private var focus
    @Environment(\.settingsSearchViewport) private var viewport
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var visible = false
    private struct Request: Equatable { let id: UUID?; let visible: Bool }
    func body(content: Content) -> some View {
        content.id(id)
            .overlay {
                if let target, let focus, target.focusID == id, visible {
                    SettingsSearchPulse(focus: focus, requestID: target.id, rowID: id)
                }
            }
            .onGeometryChange(for: Bool.self) { geometry in
                guard target?.focusID == id, !viewport.isEmpty else { return false }
                let frame = geometry.frame(in: .global)
                return viewport.contains(CGPoint(x: frame.midX, y: frame.midY))
            } action: { visible = $0 }
            .task(id: Request(id: target?.id, visible: visible)) {
                guard let target, target.focusID == id, visible else { return }
                focus?.reveal(target.id, reduceMotion: reduceMotion)
            }
    }
}
private struct SettingsSearchPulse: View {
    @ObservedObject var focus: SettingsSearchFocusModel
    let requestID: UUID
    let rowID: String
    var body: some View {
        if focus.highlightedID == requestID {
            Rectangle().fill(Color.accentColor.opacity(0.16))
                .allowsHitTesting(false)
                .accessibilityElement().accessibilityLabel("Search match")
                .accessibilityIdentifier("settings-search-highlight:" + rowID)
        }
    }
}
extension View {
    func settingsRow(_ id: String) -> some View { modifier(SettingsRowFocus(id: id)) }
}

struct SettingsScopeHeader: View {
    let title: String
    var scope: SettingsSearch.Scope = .phone
    var computer: String? = nil
    var body: some View {
        Text(scope == .phone ? title : title + " · " + SettingsSearch.scopeLabel(scope, computer: computer))
    }
}

/// Geometry may describe the destination before its push transition has ended.
/// Start reveal/expiry only after UIKit has presented its final viewport, as the
/// conversation reading-anchor owner does with ConversationPresentationReady.
private struct SettingsPresentationReady: UIViewControllerRepresentable {
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

/// Used in the existing shell's detail column on wide devices, or pushed on iPhone.
struct SettingsSearchDestination: View {
    @ObservedObject var library: ConnectionLibrary
    let target: SettingsSearchTarget
    @StateObject private var focus = SettingsSearchFocusModel()
    @State private var unavailable = false
    @State private var viewport = CGRect.zero
    @State private var presented = false
    private struct RevealRequest: Equatable { let id: UUID; let presented: Bool }
    @Environment(\.wonderTheme) private var theme
    private var row: SettingsSearch.Row? { SettingsSearch.rows.first { $0.id == target.rowID } }
    private var computer: ConnectionModel? {
        library.paired.first { $0.credential.hostInstallationId == target.hostID }.map { library.model(for: $0) }
    }
    private var focusedTarget: SettingsSearchTarget {
        var focused = target
        if target.rowID == "connection-details", let kind = computer?.connection?.routes.first?.kind {
            focused.focusOverride = "connection-route:" + kind.rawValue
        }
        return focused
    }
    var body: some View {
        ScrollViewReader { proxy in
            destination
                .id(target.destinationKey)
                .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { viewport = $0 }
                .environment(\.settingsSearchViewport, viewport)
                .environment(\.settingsSearchTarget, presented ? focusedTarget : nil)
                .background(SettingsPresentationReady { presented = true })
                .environment(\.settingsSearchFocusModel, focus)
                .onChange(of: focus.locatedID) { _, id in if id == target.id { unavailable = false } }
                .onDisappear { presented = false; focus.cancel() }
                .safeAreaInset(edge: .bottom) {
                    if unavailable {
                        Text(row?.scope == .computer ? "This setting isn’t available on this computer. Check its connection and supported settings." : "This setting isn’t available in the current configuration.")
                            .font(.callout).foregroundStyle(.secondary).padding()
                            .frame(maxWidth: .infinity).background(theme.page)
                            .accessibilityIdentifier("settings-search-unavailable")
                    }
                }
                .task(id: RevealRequest(id: target.id, presented: presented)) {
                    guard presented else { return }
                    unavailable = false
                    // Rows whose values arrive with the destination's normal read
                    // may not exist on its first layout. Retry only local scrolling.
                    for delay in [150, 350, 700, 1_400, 2_000] {
                        do { try await Task.sleep(for: .milliseconds(delay)) } catch { return }
                        guard !Task.isCancelled else { return }
                        let anchor = SettingsSearch.rows.first { $0.id == focusedTarget.focusID }?.scrollAnchor ?? row?.scrollAnchor ?? focusedTarget.focusID
                        proxy.scrollTo(anchor, anchor: .top)
                        do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
                        proxy.scrollTo(focusedTarget.focusID, anchor: .center)
                        if focus.locatedID == target.id { return }
                    }
                    unavailable = focus.locatedID != target.id
                }
        }
    }
    @ViewBuilder private var destination: some View {
        if let row {
            switch row.screen {
            case .settings: ConnectionsView(library: library, searchEnabled: false)
            case .theme: ThemePickerView()
            case .font: FontPickerView()
            case .pills: PillsSettingsView()
            case .voice: VoiceSettingsView(library: library, settingsHost: target.hostID)
            case .defaults: AppDefaultModelProvidersView(library: library)
            case .secrets: SecretsSettingsView(models: library.paired.map { library.model(for: $0) })
            case .defaultModel(let family): AppDefaultModelsView(library: library, family: family)
            case .computer:
                if let computer { ConnectionDetail(model: computer, library: library) }
            case .connectedApps:
                if let computer { SettingsConnectedAppsDestination(model: computer) }
            case .instruction(let family):
                if let computer { SettingsInstructionDestination(model: computer, family: family) }
            case .provider(let family):
                if let computer { ProviderSettingsView(model: computer, family: family) }
            case .routes:
                if let computer { ConnectionRoutesDetail(model: computer) }
            }
        }
    }
}

private struct SettingsConnectedAppsDestination: View {
    @ObservedObject var model: ConnectionModel
    @StateObject private var control = ComputerUseSettingsModel()
    var body: some View { ConnectedAppsView(model: model, computerControl: control) }
}

private struct SettingsInstructionDestination: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @State private var settings: CustomInstructionSettings?
    @State private var failed = false
    var body: some View {
        Group {
            if settings != nil { CustomInstructionEditor(model: model, family: family, settings: $settings) }
            else if failed { Text("Couldn’t read custom instructions. Open the provider’s settings to try again.").foregroundStyle(.secondary) }
            else { ProgressView("Loading instructions…") }
        }
        .task(id: model.assignmentScope) {
            let scope = model.assignmentScope
            settings = nil; failed = false
            do {
                let loaded: CustomInstructionSettings = try await model.manage(CustomInstructionSettings.path)
                guard !Task.isCancelled, model.assignmentScope == scope, !model.accessEnded else { return }
                settings = loaded
            } catch {
                guard !Task.isCancelled, model.assignmentScope == scope else { return }
                failed = true
            }
        }
    }
}

// Native List identities match the catalog even before an offscreen cell exists.
extension WonderThemeSpec { var settingsSearchID: String { "theme-option:" + id } }
extension MessageFont { var settingsSearchID: String { "message-font:" + rawValue } }
extension CodeFont { var settingsSearchID: String { "code-font:" + rawValue } }
extension ComposerPill { var settingsSearchID: String { "pills-row:" + rawValue } }
extension WonderSkillSettings.Skill { var settingsSearchID: String { "wonder-skill:" + id } }
extension AgentFamily {
    var settingsProviderID: String { "settings-provider:" + rawValue }
    var settingsDefaultProviderID: String { "provider-default-model:" + rawValue }
}
