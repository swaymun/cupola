import SwiftUI
import Combine
import WonderComputerView
import AppIntents
import WonderPairing
import VisionKit
import UniformTypeIdentifiers

@main struct WonderApp: App {
    @UIApplicationDelegateAdaptor(PushAppDelegate.self) private var pushDelegate
    @StateObject private var library: ConnectionLibrary
    @StateObject private var preview = ConnectionModel(saved: nil, persistConnection: { _ in })
    init() {
        #if WONDER_DIAGNOSTICS
        ComputerOpenTimeline.shared.enable()
        if DiagnosticSubagentFixture.performanceFixture { DiagnosticPerformanceRun.start() }
        if ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("-diagnostics") }) {
            // Settings → Pills, the voice key's computer and Hold to talk are this device's choices:
            // each fixture launch starts from the defaults, whatever an earlier run chose.
            UserDefaults.standard.removeObject(forKey: ComposerPillLayout.storageKey)
            UserDefaults.standard.removeObject(forKey: VoiceKeySource.storageKey)
            UserDefaults.standard.removeObject(forKey: VoiceTalkMode.defaultsKey)
        }
        // The companion pane's side and size too, for fixture and preview launches alike.
        if ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("-diagnostics") || $0.hasSuffix("-preview") }) {
            for key in [CompanionPanePreferences.flippedKey, CompanionPanePreferences.landscapeFractionKey,
                        CompanionPanePreferences.portraitFractionKey] {
                UserDefaults.standard.removeObject(forKey: key)
            }
        }
        #endif
        let library = ConnectionLibrary()
        _library = StateObject(wrappedValue: library)
        AppDependencyManager.shared.add(dependency: library)
        // No voice session survives a relaunch; drop its leftover Live Activity.
        VoiceLiveActivity.endOrphans()
        Task.detached(priority: .utility) { InlineMediaFiles.removeEarlierLaunches() }
    }

    var body: some Scene {
        // WindowGroup can invoke its lazy builder on SwiftUI.AsyncRenderer.
        // Read app-owned state here; let the View body own actor-isolated work.
        let root = WonderRoot(library: library, preview: preview)
        let makeContent: @Sendable () -> WonderRoot = { root }
        WindowGroup(makeContent: makeContent)
    }
}

private struct WonderRoot: View {
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject var preview: ConnectionModel
    #if WONDER_DIAGNOSTICS
    @Environment(\.scenePhase) private var driverPhase
    #endif

    var body: some View {
        content
            // Tools beside a conversation draw here, outside every navigation stack.
            .modifier(CompanionPaneOverlayHost())
            .environmentObject(library)
            // "Try a demo" only while nothing is paired, like the empty Add a computer screen.
            .onboarding(tryDemo: library.paired.isEmpty ? { library.startDemo() } : nil)
            // The theme owns the tint (plain text, not blue links), the forced
            // appearance and the accent; switches use `.systemSwitch()`.
            .modifier(WonderThemeHost())
            // Web links, onboarding's included, open where Settings > Open links says.
            .modifier(LinkOpeningHost())
            #if WONDER_DIAGNOSTICS
            .overlay(alignment: .topLeading) {
                if ProcessInfo.processInfo.arguments.contains("-diagnostics-live-activity-probe") { LiveActivityProbe() }
            }
            .task { await LiveActivityProbe.showRequestedFixture() }
            .task { DiagnosticTestDriver.shared.start() }
            .onChange(of: driverPhase) { _, phase in
                if phase == .active { DiagnosticTestDriver.shared.start() }
                else if phase == .background { DiagnosticTestDriver.shared.stop() }
            }
            #endif
    }

    @ViewBuilder private var content: some View {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux6-settings-fixture") { UX6SettingsFixtureView() }
        else if DiagnosticShareFixture.requested { DiagnosticShareFixtureView() }
        else if ProcessInfo.processInfo.arguments.contains("-diagnostics-connected-apps") { DiagnosticConnectedAppsFixtureView() }
        else if ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-fixture") { DiagnosticAutomationsFixtureView() }
        else if DiagnosticSubagentFixture.chatLayoutFixture { DiagnosticChatLayoutFixtureView() }
        else if ProcessInfo.processInfo.arguments.contains("-diagnostics-subagent-fixture") { DiagnosticSubagentFixtureView() }
        else if ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-session-fixture") { ComputerSessionDiagnosticFixtureView() }
        else if ProcessInfo.processInfo.arguments.contains("-diagnostics-fixtures") { DiagnosticFixtureView() }
        else if DiagnosticScenarioLaunch.requested { DiagnosticLaunchView(library: library) }
        else { normalRoot }
        #else
        normalRoot
        #endif
    }
    @ViewBuilder private var normalRoot: some View {
        #if DEBUG || WONDER_DIAGNOSTICS
        if preview.previewMode && !library.isPreview { PreviewConversationRoot(model: preview) }
        else { ChatShell(library: library) }
        #else
        ChatShell(library: library)
        #endif
    }
}

#if WONDER_DIAGNOSTICS
/// Offline acceptance states through the production Settings views. All values
/// are synthetic; an empty library never reads or changes the owner's pairings.
private struct UX6SettingsFixtureView: View {
    @StateObject private var library: ConnectionLibrary
    @State private var showingSettings = true
    private let routes: ConnectionModel

    init() {
        let sample = DiagnosticSubagentFixture.savedConnection()
        // `-diagnostics-ux6-routes-duplicate`: a second address of the home kind, as a re-paired computer keeps.
        let extraHome = ProcessInfo.processInfo.arguments.contains("-diagnostics-ux6-routes-duplicate")
            ? [ConnectionRoute(kind: .home, origin: "http://fixture-studio.local:8787")] : []
        let saved = try! SavedConnection(routes: [
            ConnectionRoute(kind: .home, origin: "http://192.168.50.20:8787"),
            ConnectionRoute(kind: .tailscale, origin: "https://fixture.tail000000.ts.net"),
            ConnectionRoute(kind: .relay, origin: "https://relay.example.invalid")
        ] + extraHome, credential: sample.credential, hostName: "Fixture computer",
           hostKeyPin: Data(repeating: 7, count: 32).base64URL)
        routes = ConnectionModel(saved: saved, persistConnection: { _ in })
        // `-diagnostics-ux6-routes-in-use`: the first route carries the connection, so its row shows In use.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux6-routes-in-use") { routes.macConnected = true }
        _library = StateObject(wrappedValue: ConnectionLibrary(diagnosticModels: []))
    }

    var body: some View {
        NavigationStack {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux6-routes") {
                ConnectionRoutesDetail(model: routes)
            } else if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux6-themes") {
                ThemePickerView()
            } else if showingSettings {
                ConnectionsView(library: library, backToChats: { showingSettings = false })
            } else {
                Text("Settings closed").accessibilityIdentifier("ux6-settings-closed")
            }
        }
        .wonderPage()
    }
}

/// Scenario evidence: what ActivityKit reports for Wonder's Live Activities,
/// as an accessibility element the Lock Screen itself cannot offer.
private struct LiveActivityProbe: View {
    var body: some View {
        TimelineView(.periodic(from: .now, by: 0.5)) { _ in
            Color.clear.frame(width: 2, height: 2)
                .accessibilityElement()
                .accessibilityLabel(AgentLiveActivities.probe())
                .accessibilityIdentifier("live-activity-status")
        }
    }

    /// `-diagnostics-live-activity-fixture <phase> [running] [stale]`: a local
    /// activity in that state for Lock Screen and Dynamic Island captures.
    @MainActor static func showRequestedFixture() async {
        let arguments = ProcessInfo.processInfo.arguments
        guard let index = arguments.firstIndex(of: "-diagnostics-live-activity-fixture"), index + 1 < arguments.count,
              let phase = AgentActivityAttributes.Phase(rawValue: arguments[index + 1]) else { return }
        let running = index + 2 < arguments.count ? Int(arguments[index + 2]) ?? 1 : 1
        await AgentLiveActivities.shared.showFixture(phase, running: running, stale: arguments.contains("stale"))
    }
}
#endif

extension View {
    /// The app's one switch: system blue when on, in every theme. The app-wide
    /// tint follows the theme's text colour, which would draw an on switch black.
    func systemSwitch() -> some View { toggleStyle(.switch).tint(Color(uiColor: .systemBlue)) }
}

#if DEBUG || WONDER_DIAGNOSTICS
/// The synthetic `-read-preview` fixture: its one conversation, shown directly
/// so the shared timeline and composer can be exercised without pairing.
private struct PreviewConversationRoot: View {
    @ObservedObject var model: ConnectionModel
    var body: some View {
        NavigationStack { // theme-exempt: Diagnostics host; ConversationView draws ThemeBackdrop
            if let chat = model.chats.first { ConversationView(model: model, chat: chat).id(chat.id) }
            else { ProgressView() }
        }
    }
}
#endif

/// Settings. It is pushed onto the main navigation stack with a Back button;
/// the Diagnostics `-show-connections` flag presents it as a sheet, which needs its own Done.
struct ConnectionsView: View {
    @ObservedObject var library: ConnectionLibrary
    var isSheet = false
    var searchEnabled = true
    @Environment(\.voiceScene) private var voiceScene
    var backToChats: (() -> Void)? = nil
    @Environment(\.wonderTheme) private var theme
    @State private var searchQuery = ""
    @FocusState private var searchFocused: Bool
    #if WONDER_DIAGNOSTICS
    @State private var seededSearch = false
    @State private var showingVoice = false
    #endif
    @State private var searchTarget: SettingsSearchTarget?
    #if WONDER_DIAGNOSTICS
    private var vaultPreview: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-vault-settings") }
    #endif
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dismiss) private var dismiss
    @AppStorage(WonderTheme.storageKey) private var themeID = WonderThemeCatalog.defaultID
    @AppStorage(TakeOverWarning.storageKey) private var warnBeforeTakeOver = true
    @AppStorage(FollowUpBehavior.storageKey) private var followUp = FollowUpBehavior.defaultChoice
    @AppStorage(ActionConfirmation.archiveKey) private var confirmArchive = ActionConfirmation.archiveDefault
    @AppStorage(ActionConfirmation.deleteKey) private var confirmDelete = ActionConfirmation.deleteDefault
    @AppStorage(LinkOpening.storageKey) private var linkOpening = LinkOpening.standard
    @Environment(\.wonderTypography) private var typography
    private var fontSummary: String {
        typography.code == .system ? typography.message.name : "\(typography.message.name), \(typography.code.name)"
    }
    @State private var adding = false
    @State private var removingUnreadable = false
    var body: some View {
        if isSheet {
            NavigationStack {
                content()
                    .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() }.accessibilityIdentifier("settings-done") } }
            }
        } else {
            // One full-width page on every device: subsections push, with no list + detail split.
            content()
            .environment(\.settingsInlineNavigation, backToChats != nil)
            #if WONDER_DIAGNOSTICS
            .onAppear {
                if ProcessInfo.processInfo.arguments.contains("-diagnostics-settings-voice") {
                    showingVoice = true
                }
            }
            #endif
        }
    }
    @ViewBuilder private func content() -> some View {
        settingsList(backAction: backToChats)
        #if WONDER_DIAGNOSTICS
        .onAppear {
            guard searchEnabled, !seededSearch else { return }
            seededSearch = true
            let args = ProcessInfo.processInfo.arguments
            if vaultPreview { searchTarget = SettingsSearchTarget(rowID: "settings-secrets", hostID: library.paired.first?.credential.hostInstallationId) }
            if let flag = args.firstIndex(of: "-diagnostics-settings-search-query"), args.indices.contains(flag + 1) { searchQuery = args[flag + 1] }
            if let flag = args.firstIndex(of: "-diagnostics-settings-search-row"), args.indices.contains(flag + 1),
               let row = SettingsSearch.rows.first(where: { $0.id == args[flag + 1] }) {
                searchTarget = SettingsSearchTarget(rowID: row.id, hostID: row.scope == .computer ? library.paired.first?.credential.hostInstallationId : nil)
            }
        }
        #endif
        .onChange(of: searchQuery) { _, query in
            if query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { searchTarget = nil }
        }
        .onReceive(voiceScene?.$settingsRowRequest.compactMap { $0 }.receive(on: DispatchQueue.main).eraseToAnyPublisher()
                    ?? Empty<String, Never>().eraseToAnyPublisher()) { request in
            // Voice named a page: the same destination a search result opens.
            guard let row = SettingsSearch.rows.first(where: { $0.id == request }) else { return }
            voiceScene?.settingsRowRequest = nil
            searchTarget = SettingsSearchTarget(rowID: row.id, hostID: row.scope == .computer ? library.paired.first?.credential.hostInstallationId : nil)
        }
        .navigationDestination(item: Binding(
            get: { searchTarget },
            set: { searchTarget = $0 }
        )) { target in SettingsSearchDestination(library: library, target: target) }
        #if WONDER_DIAGNOSTICS
        .navigationDestination(isPresented: $showingVoice) { VoiceSettingsView(library: library) }
        #endif
    }
    private func settingsList(backAction: (() -> Void)?) -> some View {
        WonderList {
            if searchEnabled {
                Section {
                    HStack {
                        Image(systemName: "magnifyingglass").foregroundStyle(theme.secondaryText).accessibilityHidden(true)
                        TextField("Search settings", text: $searchQuery,
                                  prompt: Text("Search settings").foregroundStyle(theme.secondaryText))
                            .focused($searchFocused)
                            .textInputAutocapitalization(.never).autocorrectionDisabled()
                            .accessibilityIdentifier("settings-search-field")
                        if !searchQuery.isEmpty {
                            Button { searchQuery = ""; searchTarget = nil } label: { Image(systemName: "xmark.circle.fill") }
                                .foregroundStyle(.secondary).accessibilityLabel("Clear search")
                                .accessibilityIdentifier("settings-search-clear")
                        }
                    }
                }
            }
            if searchEnabled && !searchQuery.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                searchResults
            } else {
                settingsSections
            }
        }
        .contentMargins(.top, 8, for: .scrollContent)
        .accessibilityIdentifier("settings-root-list")
        .simultaneousGesture(DragGesture(minimumDistance: 24).onEnded { drag in
            guard !isSheet, drag.startLocation.x < 28, drag.translation.width > 70,
                  abs(drag.translation.height) < drag.translation.width / 2 else { return }
            // This recognizer also participates in the compact navigation stack.
            // Give that path its real dismiss action instead of consuming a
            // left-edge drag with a nil pane callback.
            if let backAction { backAction() } else { dismiss() }
        })
        .navigationTitle(backAction == nil ? "Settings" : "")
        .navigationBarBackButtonHidden(backAction != nil)
        // A bar, not a plain inset: rows scrolled under the title fade out behind it.
        .safeAreaBar(edge: .top, spacing: 0) {
            if let backAction {
                // The whole title row goes back, not only the arrow, and a swipe from
                // the left edge does too (the hidden back button takes the system's away).
                Button(action: backAction) {
                    HStack(spacing: 8) {
                        Image(systemName: "chevron.left").font(.body.weight(.semibold))
                            .frame(width: 44, height: 52)
                        Text("Settings").font(.headline)
                            .accessibilityAddTraits(.isHeader)
                            .accessibilityIdentifier("settings-root-title")
                        Spacer(minLength: 0)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Back to chats")
                .accessibilityIdentifier("settings-back-to-chats")
                .padding(.horizontal, 8)
            }
        }
        .navigationBarTitleDisplayMode(.inline)
        .sheet(isPresented: $adding) { PairComputerView(model: library.pairingModel()) }
        .alert("Remove saved computers?", isPresented: $removingUnreadable) {
            Button("Cancel", role: .cancel) { }
            Button("Remove", role: .destructive) { library.removeUnreadableComputers() }
        } message: {
            Text("This device forgets the computers it couldn’t read. Chats stay on your computers; pair each one again to see them here.")
        }
    }
    @ViewBuilder private var searchResults: some View {
        let matches = SettingsSearch.matches(searchQuery)
        let phone = matches.filter { $0.scope == .phone }
        let computers = matches.filter { $0.scope == .computer }
        if phone.isEmpty && (computers.isEmpty || library.paired.isEmpty) {
            Text("No settings match").foregroundStyle(.secondary)
                .listRowBackground(Color.clear).accessibilityIdentifier("settings-search-empty")
        }
        if !phone.isEmpty {
            Section(SettingsSearch.scopeLabel(.phone)) {
                ForEach(phone) { row in searchResult(row, host: nil) }
            }
        }
        if !computers.isEmpty {
            ForEach(library.paired, id: \.credential.hostInstallationId) { saved in
                Section(SettingsSearch.scopeLabel(.computer, computer: library.model(for: saved).macName)) {
                    ForEach(computers) { row in searchResult(row, host: saved.credential.hostInstallationId) }
                }
            }
        }
    }
    private func searchResult(_ row: SettingsSearch.Row, host: String?) -> some View {
        Button {
            searchFocused = false
            let target = SettingsSearchTarget(rowID: row.id, hostID: host)
            if reduceMotion { var transaction = Transaction(); transaction.disablesAnimations = true; withTransaction(transaction) { searchTarget = target } }
            else { searchTarget = target }
        } label: {
            VStack(alignment: .leading, spacing: 3) {
                Text(row.title).foregroundStyle(.primary)
                Text(row.path).font(.caption).foregroundStyle(.secondary)
            }.frame(maxWidth: .infinity, alignment: .leading).frame(minHeight: 44)
                .contentShape(Rectangle())
        }
        .accessibilityIdentifier("settings-search-result:" + row.id + (host.map { ":" + $0 } ?? ""))
    }
    @ViewBuilder private var settingsSections: some View {
        if let error = library.error {
            FailureDetails("Connection problem", message: error)
            if library.savedComputersFailure == .unreadable {
                Button("Remove saved computers", role: .destructive) { removingUnreadable = true }
                    .accessibilityIdentifier("settings-remove-unreadable-computers")
            } else if !library.loaded { Button("Try again") { library.load() } }
        }
        // Computer-owned settings live under that computer. Only app-wide
        // preferences belong alongside the Computers list.
        Section {
            ForEach(library.paired, id: \.credential.hostInstallationId) { saved in
                SettingsDestinationLink {
                    ConnectionDetail(model: library.model(for: saved), library: library)
                } label: {
                    ComputerRow(model: library.model(for: saved), savedName: library.saved.name(for: saved))
                }
                .accessibilityIdentifier("settings-computer:" + saved.credential.hostInstallationId)
            }
            if let demo = library.demo {
                // The demo computer is not paired: its page offers its providers and the ways out.
                SettingsDestinationLink {
                    DemoComputerSettings(model: demo, addComputer: { adding = true }, leave: { Task { await library.leaveDemo() } })
                } label: {
                    HStack(spacing: 12) {
                        Image(systemName: ConceptSymbol.computer).font(.title3).foregroundStyle(.secondary).frame(width: 28)
                            .accessibilityHidden(true)
                        Text(DemoMode.computerName).lineLimit(1)
                        DemoBadge()
                    }
                    .frame(minHeight: 36)
                }
                .accessibilityIdentifier("settings-computer:" + DemoMode.hostID)
            }
            Button("Add computer", systemImage: "plus") { adding = true }.disabled(!library.loaded).accessibilityIdentifier("settings-add-computer").settingsRow("settings-add-computer")
        } header: { Text("Computers") }.id(SettingsSearch.Anchor.computers.id)
        NotificationsSettingsSection(library: library)
        if !library.paired.isEmpty {
            Section {
                SettingsDestinationLink("Secrets") { SecretsSettingsView(models: library.paired.map { library.model(for: $0) }) }
                    .accessibilityIdentifier("settings-secrets").settingsRow("settings-secrets")
                SettingsDestinationLink("Default models") { AppDefaultModelProvidersView(library: library) }
                    .accessibilityIdentifier("settings-default-models").settingsRow("settings-default-models")
            } header: {
                Text(SettingsSearch.computersScopeLabel(library.paired.map { library.model(for: $0).macName }))
            }
        }
        Section {
            Picker("Default send behavior", selection: $followUp) {
                Text("Queue").tag(FollowUpBehavior.queue)
                Text("Steer").tag(FollowUpBehavior.steer)
            }
            .accessibilityIdentifier("settings-follow-up").settingsRow("settings-follow-up")
            Toggle("Warn before taking over", isOn: $warnBeforeTakeOver)
                .systemSwitch()
                .accessibilityIdentifier("settings-take-over-warning").settingsRow("settings-take-over-warning")
            Picker("Open links", selection: $linkOpening) {
                ForEach(LinkOpening.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            .accessibilityIdentifier("settings-open-links").settingsRow("settings-open-links")
            SettingsDestinationLink("Pills") { PillsSettingsView() }
                .accessibilityIdentifier("settings-pills").settingsRow("settings-pills")
        } header: {
            Text("Chats")
        } footer: {
            Text("Touch and hold Send for the other choice. Taking over a chat open in another app on your computer stops it there and continues it here.")
        }.id(SettingsSearch.Anchor.chats.id)
        Section {
            SettingsDestinationLink("Voice") { VoiceSettingsView(library: library) }
                .accessibilityIdentifier("settings-voice").settingsRow("settings-voice")
        }
        Section {
            Toggle("Confirm before archiving", isOn: $confirmArchive)
                .systemSwitch()
                .accessibilityIdentifier("settings-confirm-archive").settingsRow("settings-confirm-archive")
            Toggle("Confirm before deleting automations", isOn: $confirmDelete)
                .systemSwitch()
                .accessibilityIdentifier("settings-confirm-delete").settingsRow("settings-confirm-delete")
        } header: {
            Text("Confirmations")
        } footer: {
            Text("Archiving several chats always asks first.")
        }.id(SettingsSearch.Anchor.confirmations.id)
        Section {
            SettingsDestinationLink { ThemePickerView() } label: {
                LabeledContent("Theme", value: WonderThemeCatalog.theme(id: ProcessInfo.processInfo.diagnosticThemeOverride ?? themeID).name)
            }
            .accessibilityIdentifier("settings-theme").settingsRow("settings-theme")
            SettingsDestinationLink { FontPickerView() } label: {
                LabeledContent("Font", value: fontSummary)
            }
            .accessibilityIdentifier("settings-font").settingsRow("settings-font")
        } header: { Text("Appearance") }
        Section {
            Link("Something not working?", destination: AppInfo.triageURL)
                .accessibilityIdentifier("settings-triage").settingsRow("settings-triage")
        } footer: {
            Text("On your computer, run `cupola triage`, then give the result to a coding agent. TRIAGE.md has the steps.")
        }
        // Testers report the version and build shown here.
        Section {
            LabeledContent("Version", value: AppInfo.versionLabel)
                .accessibilityIdentifier("settings-version").settingsRow("settings-version")
            Link("Privacy policy", destination: AppInfo.privacyURL)
                .accessibilityIdentifier("settings-privacy").settingsRow("settings-privacy")
            SettingsDestinationLink("Acknowledgements") { AcknowledgementsView() }
                .accessibilityIdentifier("settings-acknowledgements").settingsRow("settings-acknowledgements")
        } header: { Text("About") }.id(SettingsSearch.Anchor.about.id)
        #if WONDER_DIAGNOSTICS
        Section { SettingsDestinationLink("Diagnostics") { DiagnosticsView(library: library) }.accessibilityIdentifier("diagnostics-settings") }
            header: { Text("Developer details") }
        #endif
    }
}

/// Settings links push onto the page's navigation stack on every device.
struct SettingsDestinationLink<Destination: View, Label: View>: View {
    let destination: Destination
    let label: Label
    init(@ViewBuilder destination: () -> Destination, @ViewBuilder label: () -> Label) {
        self.destination = destination(); self.label = label()
    }
    init(_ title: String, @ViewBuilder destination: () -> Destination) where Label == Text {
        self.destination = destination(); self.label = Text(title)
    }
    var body: some View {
        NavigationLink { destination } label: { label }
    }
}

extension EnvironmentValues {
    @Entry var settingsInlineNavigation = false
}

/// The Duo puts native navigation controls in its side rail. Settings keeps
/// subsection Back beside the title, while other devices keep native navigation.
private struct SettingsNavigationTitle: ViewModifier {
    let title: String
    /// A page action (Save). In the Duo's title row it shares the Back row; elsewhere it is the toolbar's.
    var trailing: AnyView?
    @Environment(\.settingsInlineNavigation) private var inline
    @Environment(\.isPresented) private var isPresented
    @Environment(\.dismiss) private var dismiss

    func body(content: Content) -> some View {
        content
            .navigationTitle(inline ? "" : title)
            .navigationBarBackButtonHidden(inline)
            .toolbar {
                if !inline, let trailing { ToolbarItem(placement: .confirmationAction) { trailing } }
            }
            .simultaneousGesture(DragGesture(minimumDistance: 24).onEnded { drag in
                guard inline, isPresented, drag.startLocation.x < 28, drag.translation.width > 70,
                      abs(drag.translation.height) < drag.translation.width / 2 else { return }
                dismiss()
            })
            .safeAreaBar(edge: .top, spacing: 0) {
                if inline {
                    // The whole title row is the Back target, and the left-edge swipe
                    // (which hiding the system back button removes) goes back too.
                    let row = HStack(spacing: 8) {
                        if isPresented {
                            Image(systemName: "chevron.left").font(.body.weight(.semibold))
                                .frame(width: 44, height: 52)
                                .accessibilityHidden(true)
                        }
                        Text(title).font(.headline)
                            .accessibilityAddTraits(.isHeader)
                            .accessibilityIdentifier("settings-subsection-title")
                        Spacer(minLength: 0)
                    }
                    .contentShape(Rectangle())
                    HStack(spacing: 8) {
                        Group {
                            if isPresented {
                                Button { dismiss() } label: { row }
                                    .buttonStyle(.plain)
                                    .accessibilityLabel("Back")
                                    .accessibilityIdentifier("settings-subsection-back")
                            } else { row }
                        }
                        if let trailing { trailing }
                    }
                    .frame(minHeight: 44)
                    .padding(.horizontal, 8)
                    .background { ThemeBackdrop() }
                }
            }
    }
}

extension View {
    func settingsNavigationTitle(_ title: String) -> some View {
        modifier(SettingsNavigationTitle(title: title))
    }
    /// With a page action that sits in the title row on the Duo and in the toolbar elsewhere.
    func settingsNavigationTitle<Trailing: View>(_ title: String, @ViewBuilder trailing: () -> Trailing) -> some View {
        modifier(SettingsNavigationTitle(title: title, trailing: AnyView(trailing())))
    }
}

/// Settings → Pills: which composer pills this device shows, in what order.
/// Read once from UserDefaults and kept parsed; views read `layout`.
@MainActor final class ComposerPillPreferences: ObservableObject {
    static let shared = ComposerPillPreferences()
    @Published private(set) var layout: ComposerPillLayout
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let stored = defaults.string(forKey: ComposerPillLayout.storageKey)
        layout = ComposerPillLayout(stored: stored)
        if let stored, stored.split(separator: ",").contains(where: { $0 == "git" || $0 == "-git" }) {
            defaults.set(layout.stored, forKey: ComposerPillLayout.storageKey)
        }
    }

    func update(_ change: (inout ComposerPillLayout) -> Void) {
        var next = layout
        change(&next)
        guard next != layout else { return }
        layout = next
        defaults.set(next.stored, forKey: ComposerPillLayout.storageKey)
    }

    func reset() {
        layout = .standard
        defaults.removeObject(forKey: ComposerPillLayout.storageKey)
    }
}

struct PillsSettingsView: View {
    @ObservedObject private var preferences = ComposerPillPreferences.shared

    var body: some View {
        WonderList {
            Section {
                ForEach(preferences.layout.order, id: \.settingsSearchID) { pill in
                    row(pill)
                }
                .onMove { offsets, destination in
                    preferences.update { $0.move(fromOffsets: offsets, toOffset: destination) }
                }
            } footer: {
                Text(SettingsSearch.scopeLabel(.phone) + ". Drag to change the order above the message box. What a turned-off pill opens stays in the menu under the chat’s title.")
            }.id(SettingsSearch.Anchor.pills.id)
            if preferences.layout != .standard {
                Section {
                    Button("Restore default order") { preferences.reset() }
                        .accessibilityIdentifier("pills-reset").settingsRow("pills-reset")
                } header: { Text(SettingsSearch.scopeLabel(.phone)) }.id(SettingsSearch.Anchor.pillsReset.id)
            }
        }
        // Handles always show: reordering is this page's purpose.
        .environment(\.editMode, .constant(.active))
        .settingsNavigationTitle("Pills").navigationBarTitleDisplayMode(.inline)
    }

    @ViewBuilder private func row(_ pill: ComposerPill) -> some View {
        let label = Label { Text(pill.title) } icon: { pill.menuIcon }
        Group {
            if pill.alwaysShown {
                LabeledContent { Text("Always shown").foregroundStyle(.secondary) } label: { label }
            } else {
                Toggle(isOn: Binding(get: { preferences.layout.isShown(pill) },
                                     set: { shown in preferences.update { $0.setShown(pill, shown) } })) { label }
                    .systemSwitch()
            }
        }
        .accessibilityIdentifier("pills-row:" + pill.rawValue).settingsRow("pills-row:" + pill.rawValue)
        .contextMenu {
            // A drag needs a precise hold; these do the same in one tap.
            let index = preferences.layout.order.firstIndex(of: pill) ?? 0
            Button("Move up", systemImage: "arrow.up") {
                preferences.update { $0.move(fromOffsets: IndexSet(integer: index), toOffset: index - 1) }
            }
            .disabled(index == 0)
            .accessibilityIdentifier("pills-move-up")
            Button("Move down", systemImage: "arrow.down") {
                preferences.update { $0.move(fromOffsets: IndexSet(integer: index), toOffset: index + 2) }
            }
            .disabled(index == preferences.layout.order.count - 1)
            .accessibilityIdentifier("pills-move-down")
        }
    }
}

/// Offline notices for bundled components and the connected computer's sharing
/// helper, alongside the fonts, artwork and theme palettes derived from others.
struct AcknowledgementsView: View {
    struct Entry: Identifiable {
        let title: String
        let detail: String
        let text: () -> String?
        var id: String { title }
    }
    private static func resource(_ name: String, _ ext: String) -> String? {
        Bundle.main.url(forResource: name, withExtension: ext).flatMap { try? String(contentsOf: $0, encoding: .utf8) }
    }
    /// The framework ships only WebRTC's own license; the libraries compiled into
    /// it (libvpx, Opus, BoringSSL and others) follow, from
    /// fastlane/app-store/collect-webrtc-notices.py.
    private static func webRTCNotice() -> String? {
        guard let license = resource("WebRTC-LICENSE", "md"), let bundled = resource("WebRTC-NOTICES", "txt") else { return nil }
        return license + "\n\n" + bundled
    }
    static var sections: [(String, [Entry])] {
        [
            ("Fonts", BundledFonts.licenses.map { license in
                Entry(title: license.family, detail: "SIL Open Font License 1.1") { BundledFonts.licenseText(license.file) }
            }),
            ("Software", [
                Entry(title: "WebRTC", detail: "Computer view · BSD 3-Clause, with libvpx, Opus and more") { Self.webRTCNotice() },
                Entry(title: "Linux computer sharing", detail: "WebRTC, LiveKit and Rust dependencies") { Self.resource("ComputerSharing-Linux", "md") },
                Entry(title: "Windows computer sharing", detail: "WebRTC, LiveKit and Rust dependencies") { Self.resource("ComputerSharing-Windows", "md") },
                Entry(title: "SwiftTerm", detail: "Terminal · MIT") { Self.resource("SwiftTerm-LICENSE", "md") },
                Entry(title: "highlight.js", detail: "Code colours · BSD 3-Clause") { HighlightJS.licenseText },
            ]),
            ("Artwork", [
                Entry(title: "Git logo", detail: "Code changes pill · Jason Long · CC BY 3.0") { Self.gitLogoNotice },
            ]),
            ("Themes", ThemeCredits.all.map { credit in
                Entry(title: credit.title, detail: credit.detail) { credit.license }
            }),
        ]
    }
    /// CC BY 3.0 asks for the author, the license's address and any changes.
    private static let gitLogoNotice = """
        The Git logo by Jason Long (git-scm.com/downloads/logos) is licensed under \
        the Creative Commons Attribution 3.0 Unported License \
        (creativecommons.org/licenses/by/3.0/).

        Cupola shows it, unaltered in shape, as a single-colour mark tinted to the \
        current theme on the composer's Code changes pill.
        """
    var body: some View {
        WonderList {
            ForEach(Self.sections.filter { !$0.1.isEmpty }, id: \.0) { section in
                Section(section.0) {
                    ForEach(section.1) { entry in
                        NavigationLink { LicenseTextView(title: entry.title, text: entry.text()) } label: {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(entry.title)
                                Text(entry.detail).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        .accessibilityIdentifier("acknowledgement:" + entry.title)
                    }
                }
            }
        }
        .settingsNavigationTitle("Acknowledgements")
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("acknowledgements")
    }
}

struct ConnectionDetail: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var library: ConnectionLibrary
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var removing = false
    @State private var pairing = false
    @State private var settingsFailed = false
    @State private var renaming = false
    @State private var connectionName = ""
    @State private var renameError: String?
    /// Control this computer, on Connected apps: kept here so leaving that page
    /// mid-save neither loses the result nor reloads a stale value over it.
    @StateObject private var computerControl = ComputerUseSettingsModel()
    @AppStorage(ComputerLandscapePreference.padLayoutKey) private var experimentalPadLayout = false
    /// Cupola on the computer, from the providers read; nil from a computer older than this app needs.
    @State private var cupola: MacHostCupola?
    @State private var cupolaLoaded = false
    @State private var noCupolaList = false
    private func loadCupola() async {
        guard !model.previewMode, model.connection != nil else { return }
        do {
            let response: MacProvidersResponse = try await model.manage("/api/v1/providers")
            cupola = response.host; noCupolaList = false; cupolaLoaded = true
        } catch is CancellationError {
            return
        } catch PairingFailure.response(404) {
            noCupolaList = true; cupolaLoaded = true
        } catch {
            // Unread: show nothing rather than a wrong "older than needed".
        }
    }

    var body: some View {
        WonderForm {
            Section {
                ComputerHeader(model: model)
                if model.connection != nil {
                    Button {
                        connectionName = model.connection?.hostName ?? ""
                        renaming = true
                    } label: {
                        LabeledContent("Name", value: model.macName).lineLimit(1)
                    }
                    .accessibilityIdentifier("connection-rename").settingsRow("connection-rename")
                }
                // Recovery stays here; the routes themselves are under Advanced.
                if model.homeNetworkDenied, model.connection?.routes.contains(where: { $0.kind == .home }) == true {
                    Text("Allow Local Network for Cupola in iOS Settings to connect on this Wi-Fi.")
                        .font(.callout).foregroundStyle(.secondary)
                        .accessibilityIdentifier("connection-local-network-denied")
                    Button("Open Settings") {
                        guard let url = URL(string: UIApplication.openSettingsURLString) else { settingsFailed = true; return }
                        openURL(url) { accepted in settingsFailed = !accepted }
                    }
                    .accessibilityIdentifier("connection-open-settings")
                }
                Button("Check connection") { Task { await model.check(renew: true, userInitiated: true) } }.disabled(model.busy)
                if model.accessEnded || model.macConnected == false {
                    Button("Pair again") { pairing = true }
                        .disabled(model.busy || model.checkingConnection)
                        .accessibilityIdentifier("connection-pair-again")
                }
            } header: { Text("Computer") } footer: {
                if model.macConnected == false && !model.accessEnded {
                    Text("If your computer’s address changed, pair again using a fresh link or code from cupola pair on your computer.")
                } else if model.connection != nil {
                    Text("Your computer and your other devices show this name too.")
                }
            }.id(SettingsSearch.Anchor.computer.id)
            if UIDevice.current.userInterfaceIdiom == .pad {
                Section {
                    Toggle("Experimental iPad layout", isOn: $experimentalPadLayout)
                        .systemSwitch()
                        .accessibilityIdentifier("computer-experimental-ipad-layout")
                } footer: {
                    Text("In landscape, the computer view puts a trackpad on one side and a one-hand keyboard on the other. Portrait is unchanged.")
                }
            }
            if model.connection != nil, !model.isDemo, !model.previewMode {
                RemoteAccessSection(computer: model.macName, usingRelay: model.usingRelay, origin: model.connection?.origin,
                                    keyPinned: model.connection?.hostKeyPin != nil,
                                    pairAgain: { pairing = true },
                                    status: { [model] in try await model.relayStatus() },
                                    enable: { [model] account in try await model.enableRelay(account: account) },
                                    failureText: { [model] error in model.relayFailure(error) })
            }
            ComputerNewChatDefaultSection(model: model).id(model.assignmentScope)
                .id(SettingsSearch.Anchor.newChatDefault.id)
            // Once, on the computer's page, above its providers.
            if model.connection != nil, !model.isDemo { ComputerCupolaSection(macName: model.macName, host: cupola, loaded: cupolaLoaded, noProviderList: noCupolaList) }
            Section {
                ForEach(AgentFamily.allCases, id: \.settingsProviderID) { family in
                    // Not installed: greyed, and its page says how to install it.
                    let missing = model.projects.families.isNotInstalled(family)
                    NavigationLink { ProviderSettingsView(model: model, family: family) } label: {
                        HStack(spacing: 12) {
                            ProviderIcon(family: family, size: 22).frame(width: 28).accessibilityHidden(true)
                                .opacity(missing ? 0.5 : 1)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(family.title)
                                if missing { Text("Not installed").font(.footnote) }
                            }
                            .foregroundStyle(missing ? .secondary : .primary)
                        }
                        .frame(minHeight: 36)
                    }
                    .accessibilityIdentifier("settings-provider:" + family.rawValue).settingsRow("settings-provider:" + family.rawValue)
                }
            } header: {
                Text("Providers")
            }
            Section {
                // T3 Code, Control this computer and the apps each agent can use.
                NavigationLink("Connected apps") { ConnectedAppsView(model: model, computerControl: computerControl) }
                    .accessibilityIdentifier("connection-connected-apps").settingsRow("connection-connected-apps")
            }.id(SettingsSearch.Anchor.connectedApps.id)
            Section {
                NavigationLink("Manage Projects") { ManageProjectsView(models: library.saved.connections.map { library.model(for: $0) }, embedded: true) }
                    .accessibilityIdentifier("connection-projects").settingsRow("connection-projects")
                ProjectWorktreeSetting(library: model.projects, accessEnded: model.accessEnded)
            } header: {
                Text("Projects")
            }.id(SettingsSearch.Anchor.projects.id)
            // Saved values stay readable after access ends; changes would not reach the computer.
            AutoArchiveSection(library: model.projects, computer: model.macName).disabled(model.accessEnded).id(SettingsSearch.Anchor.autoArchive.id)
            ContinuationSection(library: model.projects, computer: model.macName).disabled(model.accessEnded).id(SettingsSearch.Anchor.continuation.id)
            WonderSkillsSection(model: model)
            if model.connection != nil {
                // Routes, relay and the paired key: transport detail, kept out of the page above.
                Section {
                    NavigationLink("Connection details") { ConnectionRoutesDetail(model: model) }
                        .accessibilityIdentifier("connection-details").settingsRow("connection-details")
                } header: {
                    Text("Advanced")
                }
            }
            Section {
                Button("Remove connection", role: .destructive) { removing = true }.disabled(model.busy)
            } header: { Text(SettingsSearch.scopeLabel(.phone)) }
            if let error = model.error { FailureDetails("Couldn’t connect", message: error) }
        }
        .accessibilityIdentifier("connection-settings")
        .settingsNavigationTitle(model.macName).navigationBarTitleDisplayMode(.inline)
        .task(id: model.assignmentScope) { await loadCupola() }
        .task {
            await model.check()
        }
        .alert("Rename computer", isPresented: $renaming) {
            TextField(model.macName, text: $connectionName)
                .accessibilityIdentifier("connection-name")
            Button("Cancel", role: .cancel) { }
            Button("Save") {
                let name = connectionName
                Task { renameError = await model.renameHost(name) }
            }
        } message: { Text("Leave empty to use the computer’s own name.") }
        .alert("Couldn’t open Settings", isPresented: $settingsFailed) {
            Button("OK", role: .cancel) { }
        } message: { Text("Open Settings, choose Cupola, then allow Local Network.") }
        .alert("Couldn’t rename computer", isPresented: Binding(
            get: { renameError != nil }, set: { if !$0 { renameError = nil } }
        )) { Button("OK", role: .cancel) { renameError = nil } } message: {
            Text(renameError ?? "")
        }
        .onChange(of: model.connection == nil) { _, removed in if removed { dismiss() } }
        .onChange(of: model.assignmentScope) { _, _ in computerControl.invalidate() }
        .sheet(isPresented: $pairing, onDismiss: {
            if let host = model.connection?.credential.hostInstallationId,
               let latest = library.saved.connections.first(where: { $0.credential.hostInstallationId == host }),
               library.model(for: latest) !== model { dismiss() }
        }) { PairComputerView(model: library.pairingModel()) }
        .alert("Remove \(model.macName)?", isPresented: $removing) {
            Button("Cancel", role: .cancel) { }
            Button("Remove connection", role: .destructive) { Task { await model.forget() } }
        } message: {
            Text("This removes this computer’s saved chats, drafts and credentials from this device, and asks the computer to disconnect it. Other connections stay saved. If the computer can’t be reached or can’t prove its security key, remove this device there with cupola devices.")
        }
    }

}

/// Reach from anywhere: the owner allows the relay on the computer, then
/// this iPhone finishes turning it on with its Cupola account. Off and
/// unavailable states say what to do on the computer instead.
private struct RemoteAccessSection: View {
    let computer: String
    let usingRelay: Bool
    /// The route in use; the state is read again when it changes.
    let origin: String?
    /// False when paired with a code: no saved security key, so pairing again
    /// with the QR code or link is the way to turn it on.
    let keyPinned: Bool
    let pairAgain: @MainActor () -> Void
    let status: @MainActor () async throws -> RelayStatus
    let enable: @MainActor (WonderAccount) async throws -> RelayStatus
    let failureText: @MainActor (Error) -> String
    @ObservedObject private var account = WonderAccount.shared
    @State private var relay: RelayStatus?
    @State private var loaded = false
    @State private var working = false
    @State private var failure: String?
    /// The computer's Cupola predates the relay: nothing to show.
    @State private var unsupported = false

    var body: some View {
        if !unsupported { content }
    }

    @ViewBuilder private var content: some View {
        let shown = RemoteAccessPresentation(status: relay, signedIn: account.isSignedIn, computer: computer,
                                             usingRelay: usingRelay, keyPinned: keyPinned)
        Section {
            switch shown.control {
            case .on(let value):
                LabeledContent(RemoteAccessPresentation.name, value: value)
                    .accessibilityIdentifier("connection-remote-access").settingsRow("connection-remote-access")
            case .available, .unavailable:
                Toggle(RemoteAccessPresentation.name, isOn: Binding(get: { working }, set: { if $0 { turnOn() } }))
                    .systemSwitch()
                    .disabled(shown.control == .unavailable || working || !loaded)
                    .accessibilityIdentifier("connection-remote-access").settingsRow("connection-remote-access")
            }
            if loaded, shown.pairAgain {
                Button("Pair again") { pairAgain() }
                    .accessibilityIdentifier("connection-remote-access-pair-again")
            }
            if working {
                HStack(spacing: 8) { ProgressView(); Text("Turning on…").foregroundStyle(.secondary) }
                    .accessibilityIdentifier("connection-remote-access-working")
            }
            if let failure {
                Text(Self.inline(failure)).font(.callout).foregroundStyle(.red)
                    .accessibilityIdentifier("connection-remote-access-failure")
            }
        } footer: {
            Text(loaded ? Self.inline(shown.note) : AttributedString("Checking \(computer)…"))
        }.id(SettingsSearch.Anchor.remoteAccess.id)
        .task(id: origin) { await load() }
    }

    /// Commands read as code (`cupola relay on`).
    private static func inline(_ text: String) -> AttributedString {
        (try? AttributedString(markdown: text, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(text)
    }

    private func load() async {
        do { relay = try await status() }
        catch is CancellationError { return }
        catch PairingFailure.response(404) { unsupported = true }
        catch { relay = nil }
        loaded = true
    }

    private func turnOn() {
        guard !working else { return }
        working = true; failure = nil
        Task {
            defer { working = false }
            do { relay = try await enable(account) }
            catch is CancellationError {}
            catch { failure = failureText(error) }
        }
    }
}

/// Settings > computer > Advanced: the transport names, kept off the computer's
/// ordinary page. Local Network recovery is on that page, not here.
struct ConnectionRoutesDetail: View {
    @ObservedObject var model: ConnectionModel
    /// Addresses and the key stay hidden until tapped; leaving the page hides them again.
    @State private var revealed: Set<String> = []
    var body: some View {
        WonderForm {
            if let saved = model.connection {
                Section {
                    ForEach(kinds(of: saved), id: \.self) { kind in
                        let route = shown(kind, saved: saved)
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text(title(kind))
                                Spacer(minLength: 8)
                                badge(kind, route: route, saved: saved)
                            }
                            if kind == .relay {
                                // The relay's address is never shown.
                                Text("Used when home network and Tailscale aren’t available.").font(.caption).foregroundStyle(.secondary)
                            } else {
                                reveal(id: kind.rawValue, secret: route.origin, label: title(kind) + " address")
                            }
                        }
                        .accessibilityElement(children: .contain)
                        .accessibilityIdentifier("connection-route:" + kind.rawValue).settingsRow("connection-route:" + kind.rawValue)
                    }
                } header: { SettingsScopeHeader(title: "Routes") }.id(SettingsSearch.Anchor.routes.id)
                if let pin = saved.hostKeyPin {
                    Section {
                        reveal(id: "key", secret: pin, label: "Paired computer key", monospaced: true)
                    } header: { SettingsScopeHeader(title: "Paired computer key") }
                }
            }
        }
        .settingsNavigationTitle("Connection details").navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("connection-routes")
        #if WONDER_DIAGNOSTICS
        .onAppear {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux6-routes-revealed") {
                revealed = ["home", "tailscale", "key"]
            }
        }
        #endif
    }
    /// One row per kind, in the order the routes were saved; a computer can keep several addresses of one kind.
    private func kinds(of saved: SavedConnection) -> [ConnectionRoute.Kind] {
        var seen: [ConnectionRoute.Kind] = []
        for route in saved.routes where !seen.contains(route.kind) { seen.append(route.kind) }
        return seen
    }
    /// The address in use if this kind carries the connection, otherwise the first saved.
    private func shown(_ kind: ConnectionRoute.Kind, saved: SavedConnection) -> ConnectionRoute {
        let group = saved.routes.filter { $0.kind == kind }
        return group.first { $0.origin == saved.origin } ?? group[0]
    }
    @ViewBuilder private func badge(_ kind: ConnectionRoute.Kind, route: ConnectionRoute, saved: SavedConnection) -> some View {
        if kind == .home && model.homeNetworkDenied {
            Text("Unavailable").foregroundStyle(.secondary)
        } else if route.origin == saved.origin && model.macConnected == true {
            Text("In use").font(.footnote.weight(.semibold)).foregroundStyle(.green)
                .padding(.horizontal, 8).padding(.vertical, 2)
                .background(Color.green.opacity(0.15), in: Capsule())
                .accessibilityIdentifier("connection-in-use:" + kind.rawValue)
        }
    }
    private func reveal(id: String, secret: String, label: String, monospaced: Bool = false) -> some View {
        let shown = revealed.contains(id)
        return Button {
            if shown { revealed.remove(id) } else { revealed.insert(id) }
        } label: {
            HStack(spacing: 8) {
                Text(shown ? secret : "••••••••••••")
                    .font(monospaced ? .caption.monospaced() : .caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .multilineTextAlignment(.leading)
                Spacer(minLength: 0)
                Text(shown ? "Hide" : "Show").font(.caption).foregroundStyle(.tint)
            }
            .frame(minHeight: 44).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        // Touch and hold copies the value without showing it.
        .contextMenu {
            Button("Copy", systemImage: "doc.on.doc") {
                // Addresses and the key pin stay on this phone (no Universal Clipboard) and expire.
                UIPasteboard.general.setItems([[UTType.utf8PlainText.identifier: secret]],
                                              options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(120)])
            }
        }
        .accessibilityLabel(label)
        .accessibilityValue(shown ? secret : "Hidden")
        .accessibilityHint(shown ? "Hides it. Touch and hold to copy." : "Shows it. Touch and hold to copy.")
        .accessibilityIdentifier("connection-reveal:" + id)
    }
    private func title(_ kind: ConnectionRoute.Kind) -> String {
        switch kind { case .home: "Home network"; case .tailscale: "Tailscale"; case .relay: "Cupola relay" }
    }
}

/// Phone-wide choices to confirm before archiving one chat or deleting an
/// automation, as T3 Code offers. Archive asks nothing by default.
enum ActionConfirmation {
    static let archiveKey = "wonder.confirm.archive"
    static let archiveDefault = false
    static let deleteKey = "wonder.confirm.delete"
    static let deleteDefault = true
}

/// The computer's auto-archive rule, for every Project without its own. Hidden
/// on hosts that have no auto-archive.
private struct AutoArchiveSection: View {
    @ObservedObject var library: ProjectLibrary
    let computer: String
    var body: some View {
        if let rule = library.autoArchive {
            Section {
                AutoArchiveRuleFields(rule: rule, saving: library.savingAutoArchive) { changed in
                    Task { await library.setAutoArchive(changed) }
                }
                if let failure = library.autoArchiveFailure {
                    Text(failure).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("auto-archive-error")
                }
            } header: {
                Text("Auto-archive")
            } footer: {
                Text("Pinned chats and chats that are working or waiting for you stay. A Project can have its own rule.")
            }
        }
    }
}

/// Whether the computer continues work without the owner. Hidden on hosts
/// that cannot.
private struct ContinuationSection: View {
    @ObservedObject var library: ProjectLibrary
    let computer: String
    var body: some View {
        if let settings = library.continuation {
            Section {
                Toggle(isOn: Binding(get: { settings.resumeAfterLimit }, set: { on in
                    var next = settings; next.resumeAfterLimit = on
                    Task { await library.setContinuation(next) }
                })) {
                    HStack {
                        Text("Resume after usage limits")
                        if library.savingContinuation { ProgressView().controlSize(.small) }
                    }
                }
                .systemSwitch()
                .disabled(library.savingContinuation)
                .accessibilityIdentifier("continue-after-limit").settingsRow("continue-after-limit")
                Toggle("Continue after restarts", isOn: Binding(get: { settings.continueAfterRestart }, set: { on in
                    var next = settings; next.continueAfterRestart = on
                    Task { await library.setContinuation(next) }
                }))
                .systemSwitch()
                .disabled(library.savingContinuation)
                .accessibilityIdentifier("continue-after-restart").settingsRow("continue-after-restart")
                if let failure = library.continuationFailure {
                    Text(failure).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("continuation-error")
                }
            } header: {
                Text("Continuing work")
            } footer: {
                Text("A chat stopped by a usage limit, a crash or a restart continues where it stopped. You can cancel that in the chat.")
            }
        }
    }
}

/// The days picker and merge switch, shared by Settings and a Project's editor.
struct AutoArchiveRuleFields: View {
    let rule: AutoArchiveRule
    var saving = false
    var identifier = "auto-archive"
    let change: (AutoArchiveRule) -> Void
    private var choices: [Int] {
        AutoArchiveRule.dayChoices.contains(rule.idleDays)
            ? AutoArchiveRule.dayChoices : (AutoArchiveRule.dayChoices + [rule.idleDays]).sorted()
    }
    var body: some View {
        Picker(selection: Binding(get: { rule.idleDays }, set: { change(AutoArchiveRule(idleDays: $0, onMerge: rule.onMerge)) })) {
            ForEach(choices, id: \.self) { Text(AutoArchiveRule.daysLabel($0)).tag($0) }
        } label: {
            HStack {
                Text("Archive idle chats")
                if saving { ProgressView().controlSize(.small) }
            }
        }
        .disabled(saving)
        .accessibilityIdentifier(identifier + "-days").settingsRow(identifier + "-days")
        Toggle("Archive when the pull request merges", isOn: Binding(get: { rule.onMerge }, set: {
            change(AutoArchiveRule(idleDays: rule.idleDays, onMerge: $0))
        }))
        .systemSwitch()
        .disabled(saving)
        .accessibilityIdentifier(identifier + "-on-merge").settingsRow(identifier + "-on-merge")
    }
}

/// What the Worktree chats row shows. A computer that no longer accepts this
/// device never answers the read, so its row says how to recover instead of
/// waiting for it.
struct WorktreeSettingPresentation: Equatable {
    let isOn: Bool
    let isEnabled: Bool
    let isLoading: Bool
    let note: String?
    let canRetry: Bool

    init(includeWorktrees: Bool?, saving: Bool, failure: String?, accessEnded: Bool) {
        isOn = includeWorktrees ?? true
        if accessEnded {
            isEnabled = false; isLoading = false; note = "Pair again to change this."; canRetry = false
        } else {
            isEnabled = includeWorktrees != nil && !saving
            isLoading = saving || (includeWorktrees == nil && failure == nil)
            note = failure; canRetry = failure != nil
        }
    }
}

/// One choice for every Project/provider on the connected host.
private struct ProjectWorktreeSetting: View {
    @ObservedObject var library: ProjectLibrary
    let accessEnded: Bool
    var body: some View {
        let shown = WorktreeSettingPresentation(includeWorktrees: library.includeWorktrees, saving: library.savingWorktrees,
                                                failure: library.worktreesFailure, accessEnded: accessEnded)
        Toggle(isOn: Binding(get: { shown.isOn }, set: { value in
            Task { await library.setIncludeWorktrees(value) }
        })) {
            HStack {
                Text("Worktree chats")
                if shown.isLoading { ProgressView().controlSize(.small) }
            }
        }
        .systemSwitch()
        .disabled(!shown.isEnabled)
        .accessibilityIdentifier("include-worktrees-chats").settingsRow("include-worktrees-chats")
        .task(id: accessEnded) { if !accessEnded { await library.refreshWorktreeSettings() } }
        if let note = shown.note {
            Text(note).font(.caption).foregroundStyle(.secondary)
                .accessibilityIdentifier(accessEnded ? "worktrees-settings-pair-again" : "worktrees-settings-error")
        }
        if shown.canRetry {
            Button("Try again") { Task { await library.refreshWorktreeSettings() } }
                .accessibilityIdentifier("worktrees-settings-retry")
        }
    }
}

/// A computer's short connection state, shared by its Settings row and page.
enum ComputerState: Equatable {
    case connected, checking, offline, pairAgain
    @MainActor init(_ model: ConnectionModel) {
        if model.accessEnded || model.connection == nil { self = .pairAgain }
        else if model.macConnected == true { self = .connected }
        else if model.macConnected == false { self = .offline }
        else { self = .checking }
    }
    var title: String {
        switch self {
        case .connected: "Connected"
        case .checking: "Checking…"
        case .offline: "Not connected"
        case .pairAgain: "Pair again to connect"
        }
    }
    var color: Color {
        switch self {
        case .connected: .green
        case .checking: .orange
        case .offline, .pairAgain: .red
        }
    }
}

/// Settings > Computers: the name with its connection state beneath it.
private struct ComputerRow: View {
    @ObservedObject var model: ConnectionModel
    /// The library's copy can lag a rename; the model holds the host's
    /// confirmed name as soon as it saves it.
    let savedName: String
    var body: some View {
        let state = ComputerState(model)
        let name = model.connection?.hostName ?? savedName
        HStack(spacing: 12) {
            Image(systemName: ConceptSymbol.computer).font(.title3).foregroundStyle(.secondary).frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(name).lineLimit(1)
                HStack(spacing: 4) {
                    Circle().fill(state.color).frame(width: 8, height: 8).accessibilityHidden(true)
                    Text(state.title).font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .frame(minHeight: 44)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(name)
        .accessibilityValue(state.title)
    }
}

/// The computer page's lead: what it is and whether Wonder reaches it, with
/// the reason in the Mac's words when it doesn't.
private struct ComputerHeader: View {
    @ObservedObject var model: ConnectionModel
    var body: some View {
        let state = ComputerState(model)
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                if state == .checking || model.checkingConnection { ProgressView().controlSize(.mini) }
                else { Circle().fill(state.color).frame(width: 8, height: 8) }
                Text(state.title).font(.subheadline).foregroundStyle(.secondary)
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("connection-state")
            if state != .connected {
                Text(model.status).font(.footnote).foregroundStyle(.secondary)
                    .accessibilityIdentifier("connection-status")
            }
        }
        .padding(.vertical, 6)
    }
}

/// This setting is read and written on the selected computer, alongside its
/// model preferences; it is independent of the shared model picker.
private struct ComputerNewChatDefaultSection: View {
    @ObservedObject var model: ConnectionModel
    @State private var confirmed: Bool?
    @State private var pending: Bool?
    @State private var loading = false
    @State private var readID = UUID()
    @State private var failure: String?

    var body: some View {
        Section {
            VStack(alignment: .leading, spacing: 8) {
                Toggle(isOn: Binding(
                    get: { pending ?? confirmed ?? false },
                    set: { value in Task { await save(value) } }
                )) {
                    Text("Start new chats in T3 Code")
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                        .onTapGesture {
                            let next = !(pending ?? confirmed ?? false)
                            Task { await save(next) }
                        }
                }
                .disabled(confirmed == nil || loading || pending != nil || model.accessEnded)
                .systemSwitch()
                .accessibilityIdentifier("computer-start-new-chats-in-t3").settingsRow("computer-start-new-chats-in-t3")
                if loading || pending != nil { ProgressView().controlSize(.small).accessibilityIdentifier("computer-new-chat-default-loading") }
                if let failure {
                    Text(failure).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("computer-new-chat-default-error")
                    Button { Task { await load() } } label: {
                        Text("Try again").frame(minHeight: 44).contentShape(Rectangle())
                    }.accessibilityIdentifier("computer-new-chat-default-retry")
                }
            }
            .task(id: model.assignmentScope + (model.macConnected == true ? ":ready" : ":waiting")) { await load() }
        }
    }

    private func load() async {
        guard pending == nil else { return }
        let scope = model.assignmentScope, requestID = UUID()
        readID = requestID
        loading = true; failure = nil
        defer { if readID == requestID { loading = false } }
        do {
            let defaults: DefaultModelPreferences = try await model.manage("/api/v1/settings/default-models")
            guard !Task.isCancelled, scope == model.assignmentScope else { return }
            guard readID == requestID else { return }
            confirmed = defaults.startsInT3Code == true
        } catch {
            guard !Task.isCancelled, scope == model.assignmentScope, readID == requestID else { return }
            confirmed = nil; failure = "Couldn’t read this computer’s default. Try again."
        }
    }

    private func save(_ enabled: Bool) async {
        guard pending == nil, confirmed != nil, !loading, !model.accessEnded else { return }
        let scope = model.assignmentScope
        readID = UUID(); loading = false
        pending = enabled; failure = nil
        defer { pending = nil }
        do {
            let body = try JSONSerialization.data(withJSONObject: ["startsInT3Code": enabled])
            let defaults: DefaultModelPreferences = try await model.manage("/api/v1/settings/default-models", method: "PUT", body: body)
            guard !Task.isCancelled, scope == model.assignmentScope else { return }
            confirmed = defaults.startsInT3Code == true
        } catch {
            guard !Task.isCancelled, scope == model.assignmentScope else { return }
            failure = "Couldn’t save this computer’s default. Try again."
        }
    }
}

/// Shared chooser for defaults stored on each compatible paired computer.
struct AppDefaultModelProvidersView: View {
    @ObservedObject var library: ConnectionLibrary
    var body: some View {
        WonderList {
            Section {
                ForEach(AgentFamily.allCases, id: \.settingsDefaultProviderID) { family in
                    NavigationLink {
                        AppDefaultModelsView(library: library, family: family)
                    } label: {
                        Label { Text(family.title) } icon: { ProviderIcon(family: family).accessibilityHidden(true) }
                    }
                    .accessibilityIdentifier("provider-default-model:" + family.rawValue).settingsRow("provider-default-model:" + family.rawValue)
                }
            } header: {
                Text(SettingsSearch.computersScopeLabel(library.paired.map { library.model(for: $0).macName }))
            }.id(SettingsSearch.Anchor.defaultProviders.id)
        }
        .settingsNavigationTitle("Default models").navigationBarTitleDisplayMode(.inline)
    }
}

/// Settings > a named computer > a provider. Every read and action uses this
/// computer's model; the shared defaults chooser has a separate Settings path.
struct ProviderSettingsView: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    /// The computer did not answer the last chat detail read.
    @State private var chatDetailUnread = false
    var body: some View {
        WonderForm {
            MacProvidersSection(model: model, family: family, title: SettingsSearch.scopeLabel(.computer, computer: model.macName))
            if model.projects.capabilities(family).usageLimits {
                SettingsProviderUsageSection(model: model, family: family)
            }
            Section {
                ChatDetailRow(model: model, projects: model.projects, family: family, unread: chatDetailUnread)
            } header: {
                Text("Chat detail")
            }.id(SettingsSearch.Anchor.chatDetail.id)
        }
        .task(id: model.assignmentScope) { chatDetailUnread = !(await model.projects.refreshChatDetail()) }
        .settingsNavigationTitle(family.title).navigationBarTitleDisplayMode(.inline)
    }
}

/// One computer's chat detail for a provider: Conversation, Commentary &
/// tools or Thinking, each with its one-line description. That Mac stores the
/// choice, so this reads it there when the page opens and saves it there;
/// offline, the last answer shows and the choices wait for the connection.
private struct ChatDetailRow: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var projects: ProjectLibrary
    let family: AgentFamily
    let unread: Bool
    @State private var pending: ChatDetailLevel?
    @State private var failure: String?
    @Environment(\.wonderTheme) private var theme

    private struct Choice: Identifiable {
        let family: AgentFamily
        let level: ChatDetailLevel
        var id: String { "provider-chat-detail:" + family.rawValue + ":" + level.rawValue }
    }
    var body: some View {
        let current = projects.chatDetail?.detail(for: family)
        let selected = pending ?? current ?? .everything
        // The lazy list must know the search anchor before it creates the row.
        ForEach(ChatDetailLevel.allCases.map { Choice(family: family, level: $0) }) { choice in
            let level = choice.level
            HStack(spacing: 10) {
                Button {
                    Task { await save(level) }
                } label: {
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(level.title).foregroundStyle(.primary)
                            Text(level.summary).font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 8)
                        Image(systemName: "checkmark")
                            .font(.body.weight(.semibold)).foregroundStyle(theme.accent)
                            .opacity(level == selected ? 1 : 0)
                    }
                    .frame(minHeight: 44).contentShape(Rectangle())
                }
                .disabled(pending != nil || current == nil || model.accessEnded)
                .accessibilityValue(level == selected ? "Selected" : "")
                .accessibilityIdentifier("provider-chat-detail:" + family.rawValue + ":" + level.rawValue).settingsRow(choice.id)
                if pending == level {
                    ProgressView().accessibilityLabel("Saving")
                        .accessibilityIdentifier("provider-chat-detail-saving:" + family.rawValue)
                }
            }
        }
        if let failure {
            Text(failure).font(.caption).foregroundStyle(.red)
                .accessibilityIdentifier("provider-chat-detail-failure:" + family.rawValue)
        } else if current == nil, unread {
            Text("Connect to \(model.macName) to choose.").font(.caption).foregroundStyle(.secondary)
        }
    }

    private func save(_ detail: ChatDetailLevel) async {
        guard pending == nil, detail != projects.chatDetail?.detail(for: family) else { return }
        pending = detail; failure = nil
        defer { pending = nil }
        do {
            try await projects.setChatDetail(family, detail)
        } catch is CancellationError {
            return
        } catch PairingFailure.hostMessage(_, let message) {
            failure = message
        } catch {
            failure = "\(model.macName) didn’t save the change. Try again."
        }
    }
}

/// Usage refreshes on its own while visible: cached values first, then a fresh
/// check every minute until the section disappears.
enum UsageAutoRefresh {
    /// Values older than this are fetched again; a restarted view keeps the pace.
    static var maxAge: TimeInterval {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-fast-refresh") { return 2 }
        #endif
        return 60
    }
    @MainActor static func run(_ load: @MainActor (Bool) async -> Void) async {
        while !Task.isCancelled {
            await load(false)
            try? await Task.sleep(for: .seconds(maxAge))
        }
    }
}

/// A setting a Mac may not offer: nil when it has none or the read fails. A
/// cancelled read keeps what the page shows: the update check's task ends as
/// soon as its own read reports the finished update, mid-way through these.
enum OptionalHostSetting {
    @MainActor static func read<T>(keeping current: T?, _ read: @MainActor () async throws -> T) async -> T? {
        do { return try await read() } catch {
            let cancelled = Task.isCancelled || error is CancellationError || (error as? URLError)?.code == .cancelled
            return cancelled ? current : nil
        }
    }
}

/// Codex and Claude on this Mac, as wonder providers reports them, with
/// the Mac's Reconnect when what Wonder runs is out of step with what is
/// installed. Sign-in uses the provider's own setup on this computer.
private struct MacProvidersSection: View {
    @ObservedObject var model: ConnectionModel
    /// The provider a provider page shows.
    let family: AgentFamily
    var title = "Status"
    @State private var providers: [MacProviderStatus] = []
    /// The computer answered the providers list with "not found": an earlier Cupola.
    @State private var noProviderList = false
    @State private var loading = false
    @State private var loaded = false
    @State private var failure: String?
    @State private var reconnecting: String?
    /// With `reconnecting`: the request starts an update rather than a reconnect.
    @State private var installing = false
    @State private var messages: [String: (text: String, failed: Bool)] = [:]
    /// File backups for Edit from here; nil until read, or on a Mac without them.
    @State private var fileRestore: FileRestoreSettings?
    /// Claude's Claude app setting; nil until read, or on a Mac without it.
    @State private var claudeApp: ClaudeAppSettings?
    /// What this Mac adds to the turns Cupola runs; nil until read, or on a Mac without them.
    @State private var customInstructions: CustomInstructionSettings?
    @State private var threadTools: ThreadToolSettings?
    /// The program, arguments and environment this Mac starts providers with; nil until read, or on a Mac without them.
    @State private var runtimeSettings: ProviderRuntimeSettings?
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        let providers = providers.filter { $0.family == family }
        Group {
            Section {
                if noProviderList {
                    Text("\(family.title)’s status can’t be read until Cupola is updated on \(model.macName).").foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-status-unavailable")
                } else if providers.isEmpty && loaded {
                    Text("\(family.title) isn’t set up on \(model.macName).").foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-missing:" + family.rawValue)
                } else if providers.isEmpty {
                    if let failure {
                        Text(failure).foregroundStyle(.secondary).accessibilityIdentifier("providers-failure")
                        Button("Try again") { Task { await load(refresh: true) } }.disabled(loading)
                    } else {
                        ProgressView("Checking providers…").accessibilityIdentifier("providers-loading")
                    }
                }
                ForEach(providers) { provider in
                    row(provider)
                }
            } header: {
                HStack {
                    Text(title).accessibilityIdentifier("provider-computer")
                    Spacer()
                    if loading && !providers.isEmpty { ProgressView().controlSize(.small).accessibilityLabel("Checking providers") }
                }
            }.id(SettingsSearch.Anchor.providerStatus.id)
            // Runtime holds the Claude app and Rewind settings too; a Mac without runtime
            // settings keeps them here.
            let runtimeHome = !providers.isEmpty && runtimeSettings?.entry(family) != nil
            if runtimeHome {
                Section { ProviderRuntimeRow(model: model, family: family, settings: $runtimeSettings,
                                             claudeApp: $claudeApp, fileRestore: $fileRestore) }
                    .id(SettingsSearch.Anchor.runtime.id)
            } else if !providers.isEmpty {
                ProviderRuntimeExtras(model: model, family: family, claudeApp: $claudeApp, fileRestore: $fileRestore)
            }
            if !providers.isEmpty, customInstructions?.provider(family) != nil || threadTools?.enabled(family) != nil {
                Section {
                    AgentAdditionRows(model: model, family: family, instructions: $customInstructions, threadTools: $threadTools)
                } header: {
                    Text("Added by Cupola")
                } footer: {
                    Text("Applies only to chats Cupola runs, not to sessions you start in \(family.title)’s own apps.")
                }.id(SettingsSearch.Anchor.providerAdditions.id)
            }
        }
        .task(id: model.assignmentScope) { await load(refresh: false) }
        // The installer runs on the Mac; check until it finishes while this page is open.
        .task(id: updating) {
            while updating, !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                if Task.isCancelled || !updating { break }
                await load(refresh: false)
            }
        }
    }

    private var updating: Bool {
        providers.contains { $0.family == family && $0.update?.status == "updating" }
    }

    @ViewBuilder private func row(_ provider: MacProviderStatus) -> some View {
        let presented = MacProviderRow(provider, macName: model.macName)
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            if let family = provider.family { ProviderIcon(family: family).accessibilityHidden(true) }
            VStack(alignment: .leading, spacing: 2) {
                Text(provider.name).font(.body.weight(.semibold))
                Text(reconnecting == provider.id ? (installing ? "Starting the update…" : "Reconnecting…") : presented.status)
                    .foregroundStyle(presented.needsAttention ? Color.orange : Color.secondary)
                // Runtime startup/update status must not hide this computer's sign-in.
                if provider.state == "signedIn", provider.updatePending || provider.starting == true {
                    Text(provider.account.isEmpty ? "Signed in on this computer" : "Signed in with \(provider.account)")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if let detail = presented.detail, reconnecting != provider.id {
                    Text(detail).font(.caption).foregroundStyle(.secondary)
                }
                if let versions = presented.versions {
                    Text(versions).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-versions:" + provider.id)
                }
                if let warning = presented.warning, reconnecting != provider.id {
                    Text(warning).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-warning:" + provider.id)
                }
                if let note = presented.updateNote, reconnecting != provider.id {
                    HStack(spacing: 6) {
                        if presented.updating { ProgressView().controlSize(.mini).accessibilityHidden(true) }
                        Text(note).font(.caption).foregroundStyle(presented.updateFailed ? Color.red : Color.secondary)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("provider-update:" + provider.id)
                }
                // Notes about the provider sit in its text column, under its name.
                if let blocked = presented.blocked, reconnecting == nil {
                    Text(blocked).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-blocked:" + provider.id)
                }
                if let message = messages[provider.id] {
                    Text(message.text).font(.caption).foregroundStyle(message.failed ? Color.red : Color.secondary)
                        .accessibilityIdentifier("provider-message:" + provider.id)
                }
                // Accessibility sizes: the action sits under the text, which keeps the
                // column's width instead of breaking every word.
                if typeSize.isAccessibilitySize { action(provider, presented).padding(.top, 4) }
            }
            if !typeSize.isAccessibilitySize {
                Spacer(minLength: 8)
                action(provider, presented)
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("provider:" + provider.id).settingsRow("provider:" + provider.id)
    }

    @ViewBuilder private func action(_ provider: MacProviderStatus, _ presented: MacProviderRow) -> some View {
        if reconnecting == provider.id {
            ProgressView()
        } else if let action = presented.action {
            Button(MacProviderRow.title(action)) {
                switch action {
                case .checkAgain: Task { await load(refresh: true) }
                case .install: Task { await install(provider) }
                case .reconnect, .update: Task { await reconnect(provider) }
                }
            }
            .buttonStyle(.bordered)
            .disabled(presented.blocked != nil || reconnecting != nil || loading || model.accessEnded || model.macConnected != true)
            .accessibilityIdentifier("provider-action:" + provider.id)
        }
    }

    private func load(refresh: Bool) async {
        guard !model.previewMode, !loading else { return }
        loading = true; defer { loading = false }
        do {
            let response: MacProvidersResponse = try await model.manage("/api/v1/providers" + (refresh ? "?refresh=true" : ""))
            providers = response.providers; noProviderList = false; failure = nil; loaded = true
            // A Mac without the setting (404) or a failed read shows no switch.
            fileRestore = await OptionalHostSetting.read(keeping: fileRestore) { try await model.projects.fileRestoreSettings() }
            if family == .claude { // provider identity: Claude app setting
                claudeApp = await OptionalHostSetting.read(keeping: claudeApp) { try await model.manage(ClaudeAppSettings.path) }
            }
            customInstructions = await OptionalHostSetting.read(keeping: customInstructions) { try await model.manage(CustomInstructionSettings.path) }
            threadTools = await OptionalHostSetting.read(keeping: threadTools) { try await model.manage(ThreadToolSettings.path) }
            runtimeSettings = await OptionalHostSetting.read(keeping: runtimeSettings) { try await model.manage(ProviderRuntimeSettings.path) }
        } catch is CancellationError {
            return
        } catch PairingFailure.response(404) {
            // An earlier Cupola has no providers list; say what to do, not "couldn't read".
            noProviderList = true; loaded = true; failure = nil
        } catch {
            failure = model.macConnected == true ? "Cupola couldn’t read providers on \(model.macName)." : "Connect to \(model.macName) to see its providers."
        }
    }

    /// Starts the provider's installer on the Mac; the row then shows its
    /// progress and result (`update.status`) from later reads.
    private func install(_ provider: MacProviderStatus) async {
        struct Started: Decodable, Sendable { let update: MacProviderUpdate? }
        reconnecting = provider.id; installing = true; messages[provider.id] = nil
        defer { reconnecting = nil; installing = false }
        do {
            let _: Started = try await model.manage(
                "/api/v1/providers/\(ConnectionModel.escape(provider.id))/update", method: "POST", body: Data("{}".utf8))
            await load(refresh: false)
        } catch PairingFailure.hostMessage(_, let detail) {
            messages[provider.id] = (detail, true)
            await load(refresh: false)
        } catch is CancellationError {
            return
        } catch {
            messages[provider.id] = ("\(provider.name) couldn’t start updating. \(managementError(error))", true)
        }
    }

    private func reconnect(_ provider: MacProviderStatus) async {
        reconnecting = provider.id; messages[provider.id] = nil
        defer { reconnecting = nil }
        do {
            let updated: MacProviderStatus = try await model.manage(
                "/api/v1/providers/\(ConnectionModel.escape(provider.id))/reconnect", method: "POST", body: Data("{}".utf8))
            if let index = providers.firstIndex(where: { $0.id == updated.id }) { providers[index] = updated }
            messages[provider.id] = ("\(provider.name) reconnected.", false)
        } catch PairingFailure.hostMessage(_, let detail) {
            messages[provider.id] = (detail, true)
            await load(refresh: true)
        } catch is CancellationError {
            return
        } catch let error as URLError where error.code == .timedOut {
            // The Mac may still finish; show what it reports now rather than a failure.
            messages[provider.id] = ("\(provider.name) is taking longer than usual to reconnect on your computer. Check again in a moment.", true)
            await load(refresh: false)
        } catch {
            messages[provider.id] = ("\(provider.name) could not reconnect. \(managementError(error))", true)
        }
    }
}

/// Settings > computer > provider: file backups for Edit from here, for providers that
/// can restore files. The Mac lists exactly those (its `rewindFiles`
/// capability); a Mac without the setting answers 404 and shows nothing. Off
/// by default; the Mac stores the choice and it applies to later replies.
struct FileRestoreRows: View {
    @ObservedObject var model: ConnectionModel
    @Binding var settings: FileRestoreSettings?
    let providers: [FileRestoreSettings.Provider]
    @State private var saving: AgentFamily?
    @State private var saveFailure: (family: AgentFamily, message: String)?

    var body: some View {
        ForEach(providers) { provider in row(provider.family, enabled: provider.enabled) }
    }

    @ViewBuilder private func row(_ family: AgentFamily, enabled: Bool) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .center) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Let \(family.title) restore files when you rewind")
                    Text("\(family.title) keeps backups of the files it edits. Applies to later replies.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                .accessibilityElement(children: .combine)
                Spacer(minLength: 8)
                if saving == family {
                    ProgressView().accessibilityLabel("Saving")
                        .accessibilityIdentifier("file-restore-saving:" + family.rawValue)
                } else {
                    Toggle("Let \(family.title) restore files when you rewind", isOn: Binding(get: { enabled }, set: { value in
                        Task { await save(family, enabled: value) }
                    }))
                    .labelsHidden()
                    .systemSwitch()
                    .disabled(saving != nil || model.accessEnded || model.previewMode)
                    .accessibilityIdentifier("file-restore:" + family.rawValue).settingsRow("file-restore:" + family.rawValue)
                }
            }
            if let saveFailure, saveFailure.family == family {
                Text(saveFailure.message).font(.caption).foregroundStyle(.red)
                    .accessibilityIdentifier("file-restore-save-failure:" + family.rawValue)
            }
        }
        .padding(.vertical, 2)
    }

    private func save(_ family: AgentFamily, enabled: Bool) async {
        guard saving == nil else { return }
        saving = family; saveFailure = nil
        defer { saving = nil }
        do {
            settings = try await model.projects.setFileRestore(family, enabled: enabled)
        } catch is CancellationError {
            return
        } catch PairingFailure.hostMessage(_, let detail) {
            saveFailure = (family, detail)
        } catch {
            saveFailure = (family, (enabled ? "Couldn’t turn this on. " : "Couldn’t turn this off. ") + "\(model.macName) didn’t save the change. Try again.")
        }
    }
}

/// Settings > computer > Claude: the Claude app setting (`ClaudeAppSettings`),
/// off by default. It saves at once; the Mac applies it from the next send.
struct ClaudeAppRow: View {
    @ObservedObject var model: ConnectionModel
    @Binding var settings: ClaudeAppSettings?
    @State private var saving = false
    @State private var failure: String?

    var body: some View {
        if let settings {
            VStack(alignment: .leading, spacing: 4) {
                HStack(alignment: .center) {
                    Text(ClaudeAppSettings.title)
                    Spacer(minLength: 8)
                    if saving {
                        ProgressView().accessibilityLabel("Saving").accessibilityIdentifier("claude-app-saving")
                    } else {
                        Toggle(ClaudeAppSettings.title, isOn: Binding(get: { settings.enabled }, set: { value in
                            Task { await save(value) }
                        }))
                        .labelsHidden()
                        .systemSwitch()
                        .disabled(!settings.appInstalled && !settings.enabled || model.accessEnded || model.previewMode
                                  || model.macConnected != true)
                        .accessibilityIdentifier("claude-app-sync").settingsRow("claude-app-sync")
                    }
                }
                Text(settings.explanation(computer: model.macName))
                    .font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("claude-app-explanation")
                if let failure {
                    Text(failure).font(.caption).foregroundStyle(.red).accessibilityIdentifier("claude-app-failure")
                }
            }
            .padding(.vertical, 2)
        }
    }

    private func save(_ enabled: Bool) async {
        guard !saving else { return }
        saving = true; failure = nil
        defer { saving = false }
        do {
            settings = try await model.manage(ClaudeAppSettings.path, method: "PUT", body: ClaudeAppSettings.body(enabled: enabled))
        } catch is CancellationError {
            return
        } catch {
            failure = (enabled ? "Couldn’t turn this on. " : "Couldn’t turn this off. ") + "\(model.macName) didn’t save the change. Try again."
        }
    }
}

/// Settings > computer > provider > Added by Cupola: the custom instruction
/// the Mac adds to each turn Cupola runs for this provider (a row that opens
/// its editor) and the Cupola thread tools switch. The Mac lists only the
/// providers that can take each; the switch saves at once.
private struct AgentAdditionRows: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @Binding var instructions: CustomInstructionSettings?
    @Binding var threadTools: ThreadToolSettings?
    @State private var saving = false
    @State private var failure: String?

    var body: some View {
        if let instruction = instructions?.provider(family) {
            NavigationLink {
                CustomInstructionEditor(model: model, family: family, settings: $instructions)
            } label: {
                HStack(spacing: 10) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Custom instruction")
                        Text(CustomInstructionSettings.preview(instruction.text) ?? "None")
                            .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer(minLength: 8)
                    Text(instruction.enabled ? "On" : "Off").foregroundStyle(.secondary)
                }
            }
            .accessibilityIdentifier("custom-instruction:" + family.rawValue).settingsRow("custom-instruction:" + family.rawValue)
        }
        if let enabled = threadTools?.enabled(family) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(alignment: .center) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Cupola thread tools")
                        Text("Lets \(family.title) read and message your other chats and hand work to another agent.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    .accessibilityElement(children: .combine)
                    Spacer(minLength: 8)
                    if saving {
                        ProgressView().accessibilityLabel("Saving")
                            .accessibilityIdentifier("thread-tools-saving:" + family.rawValue)
                    } else {
                        Toggle("Cupola thread tools", isOn: Binding(get: { enabled }, set: { value in
                            Task { await save(value) }
                        }))
                        .labelsHidden()
                        .systemSwitch()
                        .disabled(model.accessEnded || model.previewMode)
                        .accessibilityIdentifier("thread-tools:" + family.rawValue).settingsRow("thread-tools:" + family.rawValue)
                    }
                }
                if let failure {
                    Text(failure).font(.caption).foregroundStyle(.red)
                        .accessibilityIdentifier("thread-tools-failure:" + family.rawValue)
                }
            }
            .padding(.vertical, 2)
        }
    }

    private func save(_ enabled: Bool) async {
        guard !saving else { return }
        saving = true; failure = nil
        defer { saving = false }
        do {
            threadTools = try await model.manage(ThreadToolSettings.path(family), method: "PUT", body: ThreadToolSettings.request(enabled: enabled))
        } catch is CancellationError {
            return
        } catch PairingFailure.hostMessage(_, let message) {
            failure = message
        } catch {
            failure = (enabled ? "Couldn’t turn this on. " : "Couldn’t turn this off. ") + "\(model.macName) didn’t save the change. Try again."
        }
    }
}

/// Edits one provider's custom instruction on this Mac. Save writes the text
/// and its switch together; the Mac keeps the text while it is off. A refused
/// save stays open with the Mac's reason, and the saved instruction is unchanged.
struct CustomInstructionEditor: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @Binding var settings: CustomInstructionSettings?
    @State private var text: String
    @State private var enabled: Bool
    @State private var saving = false
    @State private var failure: String?
    @Environment(\.dismiss) private var dismiss

    init(model: ConnectionModel, family: AgentFamily, settings: Binding<CustomInstructionSettings?>) {
        self.model = model
        self.family = family
        _settings = settings
        let saved = settings.wrappedValue?.provider(family)
        _text = State(initialValue: saved?.text ?? "")
        _enabled = State(initialValue: saved?.enabled ?? false)
    }

    private var maxLength: Int { settings?.maxLength ?? CustomInstructionSettings.defaultMaxLength }
    private var length: Int { CustomInstructionSettings.length(text) }
    private var changed: Bool {
        let saved = settings?.provider(family)
        return text != (saved?.text ?? "") || enabled != (saved?.enabled ?? false)
    }
    private var canSave: Bool {
        changed && length <= maxLength && !saving && !model.accessEnded && !model.previewMode
    }

    var body: some View {
        WonderForm {
            Section {
                Toggle("On", isOn: $enabled)
                    .systemSwitch()
                    .disabled(saving)
                    .accessibilityIdentifier("custom-instruction-enabled:" + family.rawValue).settingsRow("custom-instruction-enabled:" + family.rawValue)
            } header: { SettingsScopeHeader(title: "Custom instruction", scope: .computer, computer: model.macName) } footer: {
                Text("\(family.title) gets this with every message Cupola sends it. Turning it off keeps the text.")
            }.id(SettingsSearch.Anchor.instructionEnabled.id)
            // The count and a refusal are rows of their own: in the section's
            // footer (several views) custom-instruction.wctl never found the count.
            Section {
                TextEditor(text: $text)
                    .frame(height: 180)
                    .disabled(saving)
                    .accessibilityLabel("Instruction")
                    .accessibilityIdentifier("custom-instruction-text:" + family.rawValue).settingsRow("custom-instruction-text:" + family.rawValue)
                Text("\(length.formatted()) of \(maxLength.formatted()) characters")
                    .font(.caption).monospacedDigit()
                    .foregroundStyle(length > maxLength ? Color.red : Color.secondary)
                    .accessibilityIdentifier("custom-instruction-count:" + family.rawValue)
                if let failure {
                    Text(failure).font(.caption).foregroundStyle(.red)
                        .accessibilityIdentifier("custom-instruction-failure:" + family.rawValue)
                }
            } header: { Text(SettingsSearch.scopeLabel(.computer, computer: model.macName)) }.id(SettingsSearch.Anchor.instructionText.id)
        }
        .settingsNavigationTitle("Custom instruction").navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .confirmationAction) {
                if saving {
                    ProgressView().accessibilityLabel("Saving")
                        .accessibilityIdentifier("custom-instruction-saving:" + family.rawValue)
                } else {
                    Button("Save") { Task { await save() } }
                        .disabled(!canSave)
                        .accessibilityIdentifier("custom-instruction-save:" + family.rawValue)
                }
            }
        }
    }

    private func save() async {
        guard canSave else { return }
        saving = true; failure = nil
        defer { saving = false }
        do {
            settings = try await model.manage(CustomInstructionSettings.path(family), method: "PUT",
                                              body: CustomInstructionSettings.request(enabled: enabled, text: text))
            dismiss()
        } catch is CancellationError {
            return
        } catch PairingFailure.hostMessage(_, let message) {
            failure = message
        } catch {
            failure = "\(model.macName) didn’t save the instruction. Try again."
        }
    }
}

struct PairComputerView: View {
    @StateObject var model: ConnectionModel
    @Environment(\.dismiss) private var dismiss
    @State private var link = ""
    @State private var address = ""
    @State private var code = ""
    @State private var enteringCode = false
    @State private var scanning = false
    var body: some View {
        let canScan = DataScannerViewController.isSupported && DataScannerViewController.isAvailable
        // Only the QR code and link carry the key a home address needs.
        let homeAddress = enteringCode && HomeNetworkTrust.isHomeOrigin((try? PairingLink.address(address)) ?? "")
        NavigationStack {
            WonderForm {
                if model.busy {
                    Section {
                        ProgressView(model.status)
                        if let verification = model.verification {
                            Text(verification).font(.title2.monospaced()).textSelection(.enabled)
                                .accessibilityLabel("Verification text \(verification.map(String.init).joined(separator: " "))")
                        }
                        Button("Stop pairing", role: .cancel) { model.cancel() }
                    }
                } else {
                    if enteringCode {
                        Section {
                            TextField("Computer address", text: $address, prompt: Text("Address, like studio.tail1234.ts.net"))
                                .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                                .accessibilityIdentifier("pairing-mac-address")
                            TextField("Pairing code", text: $code)
                                .textInputAutocapitalization(.characters).autocorrectionDisabled()
                                .accessibilityIdentifier("pairing-code")
                        } footer: {
                            Text("On your computer, run cupola pair.")
                        }
                    } else {
                        Section {
                            if canScan {
                                Button("Scan QR code", systemImage: "qrcode.viewfinder") { scanning = true }
                            }
                            TextField("Paste pairing link", text: $link).textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                                .accessibilityIdentifier("pairing-link")
                        } footer: {
                            VStack(alignment: .leading, spacing: 4) {
                                Text("On your computer, run cupola pair.")
                                if !canScan { Text("QR scanning is unavailable on this device.") }
                            }
                        }
                    }
                    Section {
                        if homeAddress {
                            Text(PairingFailure.homeNetworkCode.errorDescription ?? "")
                                .accessibilityIdentifier("pairing-home-code-refusal")
                        }
                        Button(enteringCode ? (canScan ? "Use QR or pairing link" : "Use pairing link") : "Use pairing code") {
                            enteringCode.toggle()
                        }
                        .accessibilityIdentifier("pairing-entry-mode")
                    }
                }
                if let error = model.error { FailureDetails("Couldn’t connect", message: error) }
            }
            .safeAreaInset(edge: .bottom) {
                if !model.busy {
                    Button(enteringCode ? "Connect with code" : "Connect to computer") {
                        model.pair(link: enteringCode ? "" : link,
                                   address: enteringCode ? address : "",
                                   code: enteringCode ? code : "")
                    }
                    .disabled(enteringCode
                        ? homeAddress || address.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ||
                          code.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                        : link.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .accessibilityIdentifier(enteringCode ? "pairing-connect-code" : "pairing-connect-link")
                    .buttonStyle(.wonderProminent).controlSize(.large)
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal)
                    .padding(.vertical, 8)
                    .background(.regularMaterial)
                }
            }
            .navigationTitle("Add computer").navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { model.cancel(); dismiss() } } }
            .onChange(of: model.connection != nil) { _, paired in if paired { dismiss() } }
            .onDisappear { model.cancel() }
            .sheet(isPresented: $scanning) {
                NavigationStack {
                    QRScanner(scanned: { value in scanning = false; link = value; model.pair(link: value, address: "", code: "") }, failed: { message in scanning = false; model.error = message + " Paste a pairing link to connect." })
                        .navigationTitle("Scan your computer’s code").navigationBarTitleDisplayMode(.inline)
                        .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { scanning = false } } }
                }
            }
        }
    }
}

struct QRScanner: UIViewControllerRepresentable {
    var scanned: (String) -> Void
    var failed: (String) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(scanned: scanned, failed: failed) }
    func makeUIViewController(context: Context) -> DataScannerViewController {
        let controller = DataScannerViewController(recognizedDataTypes: [.barcode(symbologies: [.qr])], qualityLevel: .balanced, recognizesMultipleItems: false, isGuidanceEnabled: true, isHighlightingEnabled: true)
        controller.delegate = context.coordinator
        do { try controller.startScanning() } catch {
            Task { @MainActor in context.coordinator.failed(error.localizedDescription) }
        }
        return controller
    }
    func updateUIViewController(_ uiViewController: DataScannerViewController, context: Context) {}
    static func dismantleUIViewController(_ uiViewController: DataScannerViewController, coordinator: Coordinator) { uiViewController.stopScanning() }
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let scanned: (String) -> Void
        var used = false
        let failed: (String) -> Void
        init(scanned: @escaping (String) -> Void, failed: @escaping (String) -> Void) { self.scanned = scanned; self.failed = failed }
        func dataScanner(_ dataScanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable) { failed("Camera scanning is unavailable.") }
        func dataScanner(_ dataScanner: DataScannerViewController, didAdd addedItems: [RecognizedItem], allItems: [RecognizedItem]) {
            guard !used else { return }
            for case .barcode(let barcode) in addedItems {
                if let value = barcode.payloadStringValue { used = true; dataScanner.stopScanning(); scanned(value); break }
            }
        }
    }
}

struct ConnectedApp: Decodable, Identifiable, Sendable {
    let id: String
    let name: String
    let description: String?
    let status: String
    let logoUrl: String?
    let logoUrlDark: String?
    /// The host's validated place to link the app's account (older hosts omit it).
    let setupUrl: String?
    /// Where Connect goes: only for an app that isn't usable yet, and only to
    /// the vendor pages the host allows (connected_apps.rs `safe_url`).
    var connectURL: URL? {
        guard status == "not_connected" || status == "disabled", let setupUrl,
              let components = URLComponents(string: setupUrl), components.scheme == "https",
              components.user == nil, components.password == nil, components.port == nil,
              components.query == nil, components.fragment == nil else { return nil }
        let allowed = (components.host == "chatgpt.com" && components.path.hasPrefix("/apps/"))
            || (components.host == "claude.ai" && components.path == "/settings/connectors")
        return allowed ? components.url : nil
    }
    var displayName: String {
        // Older Claude hosts included the account namespace in connector names. Rows read this
        // several times per render; only a name that carries the prefix pays for the regex.
        guard name.range(of: "claude.ai", options: [.caseInsensitive, .anchored]) != nil else { return name }
        let normalized = name.replacingOccurrences(of: #"(?i)^claude\.ai(?:\s*[:/·-]\s*|\s+)"#, with: "", options: .regularExpression)
        return normalized.isEmpty ? name : normalized
    }
    var bundledIcon: String? {
        switch displayName.lowercased() {
        case "gmail": "ConnectorGmail"
        case "google calendar": "ConnectorGoogleCalendar"
        case "google drive": "ConnectorGoogleDrive"
        case "claude docs": "ConnectorClaudeDocs"
        case "github": "ConnectorGitHub"
        case "openai platform": "ConnectorOpenAIPlatform"
        case "linear": "ConnectorLinear"
        case "sites": "ConnectorSites"
        case "flashloop": "ConnectorFlashloop"
        case "adobe acrobat": "ConnectorAdobeAcrobat"
        default: nil
        }
    }
    var label: String {
        switch status {
        case "available": "Available"
        case "disabled": "Disabled"
        case "not_connected": "Not connected"
        case "unavailable": "Unavailable"
        default: "Not verified"
        }
    }
}
struct ConnectedAppPage: Decodable, Sendable {
    let agentFamily: String?
    let hostInstallationId: String
    let conversationId: String?
    let apps: [ConnectedApp]
    let nextCursor: String?
    let warning: String?
}
struct ConnectedAppCacheEntry {
    let page: ConnectedAppPage
    let fetchedAt: Date
}

/// Settings > computer > Connected apps: whether a message to a thread open
/// in T3 Code on the Mac goes into it there instead of waiting or stopping it.
/// Connect is one tap: the Mac approves Cupola in T3 Code itself (and renews
/// that sign-in); only a T3 Code it can't do that with opens the approval
/// page in the Mac's browser ("waiting").
/// Rows of the page's computer section: the first row carries the loads, since
/// modifiers on several rows would run once per row.
private struct T3CodeConnectionRows: View {
    @ObservedObject var model: ConnectionModel
    @Environment(\.dismiss) private var dismiss
    @State private var status: T3CodeStatus?
    @State private var loading = false
    @State private var acting = false
    @State private var failure: String?
    @State private var confirmReconnect = false
    @State private var showPairingFailure = false

    var body: some View {
        if status?.installed != false || status?.state.map({ $0 != "disconnected" }) == true || status?.pairingFailure != nil {
            // One row whatever it shows: in a List each branch of `lead` is its
            // own row, so a load that changed the branch restarted this task,
            // and a failed check reloaded forever.
            VStack(alignment: .leading, spacing: 0) { lead }
                .task(id: model.assignmentScope + ":\(model.macConnected == true)") {
                    if model.macConnected != true { status = nil }
                    await load()
                }
                .sheet(isPresented: $showPairingFailure) { pairingFailureSheet }
                #if WONDER_DIAGNOSTICS
                .background(DiagnosticDriverSurface(surface: driverSurface))
                #endif
                // Approval happens on the Mac; check until it is decided while this page is open.
                .task(id: state == "waiting") {
                    while state == "waiting", !Task.isCancelled {
                        try? await Task.sleep(for: .seconds(2))
                        if Task.isCancelled || state != "waiting" { break }
                        await load()
                    }
                }
            actions
        }
    }

    private var state: String? { status?.state }
    private var busy: Bool { acting || loading || model.accessEnded || model.macConnected != true }

    @ViewBuilder private var lead: some View {
        if let state {
            HStack(spacing: 12) {
                Image(systemName: "app.connected.to.app.below.fill")
                    .frame(width: 32, height: 32).foregroundStyle(.secondary).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text("T3 Code").font(.headline)
                    if let status {
                        Text(T3CodeSettingsText.label(status, computer: model.macName)).font(.subheadline)
                            .foregroundStyle(status.wasRemoved ? Color.orange : Color.secondary)
                        Text(T3CodeSettingsText.footer(status, computer: model.macName)).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                if state == "waiting" || loading { ProgressView().accessibilityHidden(true) }
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("t3code-status").settingsRow("t3code-status")
        } else if loading {
            ProgressView("Checking T3 Code…")
        } else {
            Text(failure ?? "Cupola couldn’t check T3 Code on \(model.macName).").foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var actions: some View {
        if let state {
            if status?.running == false || status?.reachable == false {
                Button("Check again") { Task { await load(refresh: true) } }
                    .disabled(busy).accessibilityIdentifier("t3code-retry")
            }
            switch state {
            case "connected":
                Button("Disconnect", role: .destructive) {
                    Task { await act("/api/v1/t3-code", method: "DELETE", failed: "T3 Code wasn’t disconnected.") }
                }.disabled(busy).accessibilityIdentifier("t3code-disconnect")
            case "waiting":
                Button("Cancel") {
                    Task { await act("/api/v1/t3-code", method: "DELETE", failed: "The sign-in wasn’t cancelled.") }
                }.disabled(busy).accessibilityIdentifier("t3code-cancel")
            default:
                Button(status.map(T3CodeSettingsText.connectTitle) ?? "Connect T3 Code") {
                    // The owner removed Cupola in T3 Code: ask before signing in there again.
                    if status?.wasRemoved == true && status?.pairingFailure == nil { confirmReconnect = true } else { connect() }
                }
                .disabled(busy || status?.running == false).accessibilityIdentifier("t3code-connect")
                .confirmationDialog(T3CodeSettingsText.reconnectQuestion, isPresented: $confirmReconnect, titleVisibility: .visible) {
                    Button("Reconnect") { connect() }.accessibilityIdentifier("t3code-reconnect-confirm")
                    Button("Not now", role: .cancel) {}
                } message: {
                    Text(T3CodeSettingsText.reconnectMessage(computer: model.macName))
                }
            }
            if status?.pairingFailure != nil {
                Button("Open approval on \(model.macName)") { openApproval() }
                    .disabled(busy).accessibilityIdentifier("t3code-open-approval")
            }
            if acting { ProgressView("Connecting T3 Code…") }
            if let message = failure ?? status?.error {
                Text(message).font(.caption).foregroundStyle(status?.pairingFailure == nil ? Color.red : Color.primary).accessibilityIdentifier("t3code-message")
            }
        } else if !loading {
            Button("Try again") { Task { await load() } }.disabled(busy).accessibilityIdentifier("t3code-retry")
        }
    }

    #if WONDER_DIAGNOSTICS
    /// Drives the mounted product rows and their actions on Duo's touchless rig.
    private var driverSurface: DiagnosticTestDriver.Surface {
        .init(priority: 300, state: {
            ["route": "settings", "sheets": showPairingFailure ? ["t3-code-failure"] : []]
        }, controls: {
            if showPairingFailure {
                return [.init(id: "t3code-failure-reason", label: status?.error ?? ""),
                        .init(id: "t3code-failure-retry", label: "Try again", enabled: !busy,
                              tap: { showPairingFailure = false; connect() }),
                        .init(id: "t3code-failure-open", label: "Open approval on \(model.macName)", enabled: !busy,
                              tap: { showPairingFailure = false; openApproval() })]
            }
            guard let status else { return [] }
            var controls = [DiagnosticTestDriver.Control(id: "t3code-status", label: T3CodeSettingsText.label(status, computer: model.macName)),
                            .init(id: "t3code-guidance", label: T3CodeSettingsText.footer(status, computer: model.macName))]
            if status.running == false || status.reachable == false {
                controls.append(.init(id: "t3code-retry", label: "Check again", enabled: !busy, tap: { Task { await load(refresh: true) } }))
            }
            switch status.state {
            case "connected":
                controls.append(.init(id: "t3code-disconnect", label: "Disconnect", enabled: !busy, tap: {
                    Task { await act("/api/v1/t3-code", method: "DELETE", failed: "T3 Code wasn’t disconnected.") }
                }))
            case "waiting":
                controls.append(.init(id: "t3code-cancel", label: "Cancel", enabled: !busy, tap: {
                    Task { await act("/api/v1/t3-code", method: "DELETE", failed: "The sign-in wasn’t cancelled.") }
                }))
            default:
                controls.append(.init(id: "t3code-connect", label: T3CodeSettingsText.connectTitle(status),
                                      enabled: !busy && status.running != false, tap: {
                    if status.wasRemoved && status.pairingFailure == nil { confirmReconnect = true } else { connect() }
                }))
            }
            if status.pairingFailure != nil {
                controls.append(.init(id: "t3code-open-approval", label: "Open approval on \(model.macName)", enabled: !busy, tap: { openApproval() }))
            }
            if let message = failure ?? status.error { controls.append(.init(id: "t3code-message", label: message)) }
            return controls
        }, actions: ["dismiss": { _ in
            if showPairingFailure { showPairingFailure = false } else { dismiss() }
        }])
    }
    #endif

    private var pairingFailureSheet: some View {
        NavigationStack {
            WonderList {
                Section {
                    Text(status?.error ?? "T3 Code couldn’t approve Cupola on this computer.")
                        .accessibilityIdentifier("t3code-failure-reason")
                }
                Section {
                    Button("Try again") { showPairingFailure = false; connect() }
                        .disabled(busy).accessibilityIdentifier("t3code-failure-retry")
                    Button("Open approval on \(model.macName)") { showPairingFailure = false; openApproval() }
                        .disabled(busy).accessibilityIdentifier("t3code-failure-open")
                } footer: {
                    Text("Browser approval opens on \(model.macName). You’ll need access to that computer to approve Cupola.")
                        .foregroundStyle(Color.primary)
                }
            }
            .navigationTitle("Couldn’t connect T3 Code")
            .navigationBarTitleDisplayMode(.inline)
        }
        .presentationDragIndicator(.hidden)
    }

    private func openApproval() {
        Task { await act("/api/v1/t3-code/connect?browser=true", method: "POST", failed: "Approval wasn’t opened on \(model.macName).") }
    }

    private func connect() {
        Task { await act("/api/v1/t3-code/connect", method: "POST", failed: "T3 Code wasn’t connected.") }
    }

    private func load(refresh: Bool = false) async {
        guard !loading, model.connection != nil, !model.accessEnded else { return }
        loading = true; defer { loading = false }
        do {
            let next: T3CodeStatus = try await model.manage("/api/v1/t3-code" + (refresh ? "?refresh=true" : ""), decodingStatuses: [503])
            if let detail = next.detail { failure = detail } else {
                if next.pairingFailure != nil && status?.pairingFailure != next.pairingFailure { showPairingFailure = true }
                status = next; if next.error == nil { failure = nil }
            }
        } catch is CancellationError {
        } catch {
            status = nil
            failure = model.macConnected == false
                ? "Computer offline. Reconnect to \(model.macName) to check T3 Code."
                : "Cupola couldn’t check T3 Code on \(model.macName). \(managementError(error))"
        }
    }

    private func act(_ path: String, method: String, failed: String) async {
        acting = true; failure = nil
        defer { acting = false }
        do {
            let next: T3CodeStatus = try await model.manage(path, method: method, decodingStatuses: [409, 502, 503])
            if let detail = next.detail { failure = detail } else {
                status = next
                showPairingFailure = next.pairingFailure != nil
            }
        } catch is CancellationError {
        } catch {
            failure = "\(failed) \(managementError(error))"
        }
    }
}

/// Settings > computer > Connected apps: the computer-wide apps (T3 Code and
/// control of the computer itself), with each agent's app list one level down.
struct ConnectedAppsView: View {
    @Environment(\.settingsInlineNavigation) private var inlineNavigation
    @ObservedObject var model: ConnectionModel
    @ObservedObject var computerControl: ComputerUseSettingsModel
    var body: some View {
        WonderList {
            Section {
                // A computer that is off is said once; its rows load when it is back.
                if computerIsOffline(model) {
                    ComputerOfflineRow(name: model.macName).frame(minHeight: 44)
                } else {
                    T3CodeConnectionRows(model: model)
                    ComputerControlRows(model: model, state: computerControl)
                }
            } header: {
                Text(SettingsSearch.scopeLabel(.computer, computer: model.macName))
            }.id(SettingsSearch.Anchor.connectedApps.id)
            if AgentFamily.allCases.contains(where: { model.projects.capabilities($0).connectedApps }) {
                Section {
                    NavigationLink("Apps agents can use") { AgentAppsView(model: model) }
                        .accessibilityIdentifier("connected-apps-agent-apps").settingsRow("connected-apps-agent-apps")
                }.id(SettingsSearch.Anchor.connectedAppsAgents.id)
            }
        }
        .settingsNavigationTitle("Connected apps")
        .toolbar {
            if !inlineNavigation {
                ToolbarItem(placement: .principal) {
                    Text("Connected apps").font(.headline).accessibilityIdentifier("connected-apps-title")
                }
            }
        }
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// Settings > computer > Connected apps > Apps agents can use: each provider's
/// apps on this Mac, account-wide. An inventory, so it sits behind its own row.
struct AgentAppsView: View {
    @ObservedObject var model: ConnectionModel
    @State private var apps: [ConnectedApp] = []
    @State private var cursor: String?
    @State private var loading = false
    @State private var failure: String?
    /// An update or a provider without an app list can't be fixed by trying again.
    @State private var retryable = true
    @State private var warning: String?
    @State private var visited: Set<String> = []
    @State private var selectedFamily: AgentFamily = .codex
    @State private var reportedFamily: AgentFamily?
    @State private var loadingScope: String?
    @State private var loadID = UUID()
    /// A Connect page was opened; the next return to the app refreshes past the cache.
    @State private var openedSetup = false
    @Environment(\.openURL) private var openURL
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.scenePhase) private var scenePhase
    var body: some View {
        WonderList {
            Picker("Agent", selection: $selectedFamily) {
                ForEach(AgentFamily.allCases.filter { model.projects.capabilities($0).connectedApps }) { family in
                    Text(family.title).tag(family)
                }
            }.pickerStyle(.segmented)
            if let failure {
                Section {
                    Text(failure).foregroundStyle(.secondary)
                    if retryable {
                        Button("Try again") { Task { await load(refresh: true) } }
                            .disabled(loading)
                            .accessibilityIdentifier("connected-apps-retry")
                    }
                }
            }
            if let warning { Text(warning).foregroundStyle(.secondary) }
            Section {
                ForEach(apps) { app in
                    HStack(alignment: .top, spacing: 12) {
                        Group {
                            if let icon = app.bundledIcon {
                                Image(icon).resizable().scaledToFit()
                            } else {
                                AsyncImage(url: URL(string: (colorScheme == .dark ? app.logoUrlDark ?? app.logoUrl : app.logoUrl) ?? "")) { image in
                                    image.resizable().scaledToFit()
                                } placeholder: { Image(systemName: "puzzlepiece.extension.fill").resizable().scaledToFit().foregroundStyle(.secondary) }
                            }
                        }
                        .frame(width: 32, height: 32).accessibilityHidden(true)
                        VStack(alignment: .leading, spacing: 4) {
                            if dynamicTypeSize.isAccessibilitySize {
                                Text(app.displayName).font(.headline)
                                Text(failure == nil ? app.label : "Not verified").font(.subheadline).foregroundStyle(.secondary)
                            } else {
                                HStack { Text(app.displayName).font(.headline); Spacer(); Text(failure == nil ? app.label : "Not verified").font(.subheadline).foregroundStyle(.secondary) }
                            }
                            if let description = app.description { Text(description).font(.subheadline).foregroundStyle(.secondary).lineLimit(2) }
                            if dynamicTypeSize.isAccessibilitySize, failure == nil, let url = app.connectURL { connectButton(app, url) }
                        }
                        if !dynamicTypeSize.isAccessibilitySize, failure == nil, let url = app.connectURL { connectButton(app, url) }
                    }.padding(.vertical, 4)
                }
                if loading { ProgressView("Checking apps…") }
                else if cursor != nil { Button("Load more") { Task { await load(more: true) } } }
                else if apps.isEmpty && failure == nil { Text("\(selectedFamily.title) has no apps on \(model.macName).").foregroundStyle(.secondary) }
                if (reportedFamily ?? selectedFamily) == .claude { // provider identity: link to Claude's own connector settings
                    Link(destination: URL(string: "https://claude.ai/settings/connectors")!) {
                        HStack(spacing: 12) {
                            Image(systemName: "slider.horizontal.3")
                                .frame(width: 32, height: 32)
                                .foregroundStyle(.secondary)
                                .accessibilityHidden(true)
                            Text("Manage Claude connections")
                            Spacer(minLength: 8)
                            Image(systemName: "arrow.up.right")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(.tertiary)
                                .accessibilityHidden(true)
                        }
                        .foregroundStyle(.primary)
                    }
                    .accessibilityIdentifier("connected-apps-manage-claude")
                    .accessibilityHint("Opens claude.ai in your browser")
                }
            } footer: {
                // Cupola's Claude turns load these connectors; each call asks
                // the owner per the chat's access (claude-runtime permissions.mjs).
                if (reportedFamily ?? selectedFamily) == .claude, !apps.isEmpty, failure == nil { // provider identity: describes Claude's own ask-before-use rule
                    Text("Claude asks you before each use, except in chats with Full access.")
                        .accessibilityIdentifier("connected-apps-claude-approval")
                } else if (reportedFamily ?? selectedFamily) == .codex, failure == nil, apps.contains(where: { $0.connectURL != nil }) { // provider identity: Codex apps link through its ChatGPT account
                    // Connect opens ChatGPT on this phone; the app only works
                    // if that is the account Codex signed in with on the Mac.
                    Text("Use the ChatGPT account Codex uses on \(model.macName).")
                        .accessibilityIdentifier("connected-apps-codex-account")
                }
            }
        }
        .settingsNavigationTitle("Apps agents can use")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { Button("Refresh", systemImage: "arrow.clockwise") { Task { await load(refresh: true) } }.disabled(loading) }
        .task(id: scope) { apps = []; cursor = nil; visited = []; reportedFamily = nil; await load() }
        .onChange(of: model.accessEnded) { _, ended in if ended { apps = []; cursor = nil; warning = nil; failure = "Access has ended. Reconnect in Settings." } }
        .onChange(of: scenePhase) { _, next in
            guard next == .active else { return }
            // Back from a setup page: ask the computer again, past the cache.
            // Opening the page never counts as connected; only the host says so.
            let refresh = openedSetup
            openedSetup = false
            Task { await load(refresh: refresh) }
        }
    }
    private func connectButton(_ app: ConnectedApp, _ url: URL) -> some View {
        Button("Connect") { openedSetup = true; openURL(url) }
            .buttonStyle(.bordered)
            .font(.subheadline.weight(.semibold))
            .accessibilityLabel("Connect \(app.displayName)")
            .accessibilityHint(url.host() == "claude.ai" ? "Opens claude.ai in your browser" : "Opens ChatGPT")
            .accessibilityIdentifier("connected-apps-connect:" + app.id)
    }
    private var scope: String { model.assignmentScope + ":host:" + selectedFamily.rawValue }
    @MainActor private func load(more: Bool = false, refresh: Bool = false) async {
        guard !loading || loadingScope != scope, let saved = model.connection, !model.accessEnded else { return }
        let requestID = UUID(); loadID = requestID; loadingScope = scope
        loading = true; failure = nil; retryable = true
        defer { if loadID == requestID { loading = false } }
        var components = URLComponents()
        components.path = "/api/v1/connected-apps"
        components.queryItems = [URLQueryItem(name: "refresh", value: refresh ? "true" : "false")]
        components.queryItems?.append(URLQueryItem(name: "agentFamily", value: selectedFamily.rawValue))
        if more, let cursor { components.queryItems?.append(URLQueryItem(name: "cursor", value: cursor)) }
        let savedScope = scope
        let cacheKey = savedScope + ":" + (more ? cursor ?? "" : "")
        if !more, apps.isEmpty, let cached = model.connectedAppsCache[cacheKey] {
            apps = cached.page.apps; cursor = cached.page.nextCursor; warning = cached.page.warning
        }
        do {
            let page: ConnectedAppPage
            if !refresh, let cached = model.connectedAppsCache[cacheKey], Date().timeIntervalSince(cached.fetchedAt) < 300 {
                page = cached.page
            } else {
                page = try await model.api.request(components.string!, origin: saved.origin, credential: saved.credential)
                guard savedScope == scope, page.hostInstallationId == saved.credential.hostInstallationId,
                    page.conversationId == nil, !model.accessEnded else { return }
                if refresh { model.connectedAppsCache = model.connectedAppsCache.filter { !$0.key.hasPrefix(savedScope + ":") } }
                model.connectedAppsCache[cacheKey] = ConnectedAppCacheEntry(page: page, fetchedAt: Date())
            }
            guard savedScope == scope,
                page.hostInstallationId == saved.credential.hostInstallationId, page.conversationId == nil,
                !model.accessEnded else { return }
            reportedFamily = AgentFamily(host: page.agentFamily)
            if reportedFamily != selectedFamily { throw ReadFailure.resync }
            if !more { apps = []; visited = [] }
            var ids = Set(apps.map(\.id))
            apps += page.apps.filter { ids.insert($0.id).inserted }
            warning = page.warning
            cursor = page.nextCursor
            if let next = cursor, !visited.insert(next).inserted { cursor = nil; warning = "More apps could not be loaded. Refresh to try again." }
        } catch {
            guard savedScope == scope, !model.accessEnded else { return }
            if case PairingFailure.response(501) = error {
                failure = "\(selectedFamily.title) doesn’t list its apps in Cupola yet."
                retryable = false
            } else {
                failure = "Apps could not be verified. Check Cupola on your computer."
            }
        }
    }
}
