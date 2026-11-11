import AuthenticationServices
import SwiftUI

/// Live Activities' row on Settings > Notifications: what stops the switch, if
/// anything. They need only the account and iOS's own switch, not notifications.
enum LiveActivityAvailability: Equatable {
    case offInIOSSettings, available

    init(systemAllows: Bool) {
        self = systemAllows ? .available : .offInIOSSettings
    }

    var canToggle: Bool { self == .available }
    /// Under the switch. iOS's own Settings app is named, so "Settings" can't be
    /// read as this page.
    var detail: String? {
        switch self {
        case .offInIOSSettings: "Off in iOS Settings"
        case .available: nil
        }
    }
}

/// The Notifications switch shows the saved preference; this says whether iOS
/// lets those notifications through right now. iOS's own Settings app is
/// named, as for Live Activities, so it can't be read as this page.
enum NotificationAvailability: Equatable {
    case shown, offInIOSSettings

    init(preference: Bool, system: SystemNotificationPermission) {
        self = preference && system == .off ? .offInIOSSettings : .shown
    }

    var detail: String? {
        switch self {
        case .offInIOSSettings: "Off in iOS Settings. Allow notifications for Cupola there to get them again."
        case .shown: nil
        }
    }
}

/// Settings > Notifications and Live Activities: two rows, each with its own
/// switch. Signed out, the Apple sign-in sits in the list; signed in, the
/// account's actions are one level down. The switches apply to every paired computer, including ones paired later.
struct NotificationsSettingsSection: View {
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject private var account = WonderAccount.shared
    @ObservedObject private var push = PushNotifications.shared
    @ObservedObject private var liveActivities = AgentLiveActivities.shared
    @State private var systemAllows = true
    @Environment(\.scenePhase) private var scenePhase

    private var hosts: [String] { library.paired.map(\.credential.hostInstallationId) }
    private var allEnabled: Bool { !hosts.isEmpty && push.alertsEnabled }
    private var settingUp: Bool {
        library.paired.contains { ComputerState(library.model(for: $0)) != .offline && push.setupState($0.credential.hostInstallationId) == .settingUp }
    }
    private var failures: [String] {
        // A computer that is offline can't finish setup; it retries when it
        // reconnects, so it isn't an account-level failure (owner feedback 34).
        library.paired.compactMap { saved in
            let host = saved.credential.hostInstallationId
            guard ComputerState(library.model(for: saved)) != .offline,
                  case .needsRetry? = push.setupState(host) else { return nil }
            return host
        }
    }
    private var availability: NotificationAvailability {
        NotificationAvailability(preference: allEnabled, system: push.systemPermission)
    }
    private var activities: LiveActivityAvailability {
        LiveActivityAvailability(systemAllows: systemAllows)
    }
    var body: some View {
        Group {
            if !account.isSignedIn {
                AccountSignInSection().id(SettingsSearch.Anchor.account.id)
            } else if hosts.isEmpty {
                Section {
                    Text("Add a computer to get notifications.").foregroundStyle(.secondary)
                    accountMenu
                    accountError
                }
            } else {
                Section {
                    Toggle(isOn: Binding(get: { allEnabled }, set: { push.setAll(enabled: $0, library: library) })) {
                        HStack {
                            Text("Notifications")
                            if settingUp { ProgressView().accessibilityLabel("Enabling notifications") }
                        }
                    }
                    .systemSwitch().disabled(settingUp).frame(minHeight: 44)
                    .accessibilityValue(allEnabled ? "On" : "Off")
                    .accessibilityIdentifier("settings-notifications").settingsRow("settings-notifications")
                    Toggle("Live Activities", isOn: Binding(
                        get: { activities.canToggle && liveActivities.enabled },
                        set: { value in
                            guard activities.canToggle else { return }
                            AgentLiveActivities.shared.setAll(enabled: value, library: library)
                        }
                    ))
                    .systemSwitch().disabled(!activities.canToggle).frame(minHeight: 44)
                    .accessibilityValue(activities.canToggle && liveActivities.enabled ? "On" : "Off")
                    .accessibilityIdentifier("settings-live-activities").settingsRow("settings-live-activities")
                    if !failures.isEmpty {
                        Button("Retry notification setup") { failures.forEach { push.retry($0) } }
                            .accessibilityIdentifier("notifications-retry")
                    }
                    if availability == .offInIOSSettings || activities == .offInIOSSettings {
                        Button("Open iOS Settings") {
                            if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
                        }
                        .accessibilityIdentifier("notifications-settings")
                    }
                    ForEach(notificationNotes, id: \.self) { note in
                        Text(note).font(.footnote).foregroundStyle(.secondary)
                            .accessibilityIdentifier("notifications-note")
                    }
                    accountMenu
                    accountError
                }.id(SettingsSearch.Anchor.notifications.id)
            }
        }
        .onAppear(perform: read)
        .onChange(of: scenePhase) { _, phase in if phase == .active { read() } }
        .alert(push.settingsAlert?.title ?? "Couldn’t change notifications", isPresented: Binding(
            get: { push.settingsAlert != nil },
            set: { if !$0 { push.dismissSettingsAlert() } }
        ), presenting: push.settingsAlert) { alert in
            if alert.opensSettings {
                Button("Open Settings") {
                    if let url = URL(string: UIApplication.openNotificationSettingsURLString) { UIApplication.shared.open(url) }
                }
            }
            Button("OK", role: .cancel) { push.dismissSettingsAlert() }
        } message: { alert in Text(alert.message) }
    }

    private var notificationNotes: [String] {
        var notes: [String] = []
        for host in hosts { if case .needsRetry(let message)? = push.setupState(host), !notes.contains(message) { notes.append(message) } }
        if let detail = availability.detail { notes.append(detail) }
        if let detail = activities.detail, activities != .offInIOSSettings || availability != .offInIOSSettings { notes.append(detail) }
        return notes
    }

    /// The account's state at a glance; its actions are one level down.
    private var accountMenu: some View {
        SettingsDestinationLink { AccountSettingsView() } label: {
            LabeledContent("Cupola account", value: "Signed in")
                .frame(minHeight: 44)
        }
        .disabled(account.working)
        .accessibilityIdentifier("account-provider").settingsRow("account-provider")
    }

    @ViewBuilder private var accountError: some View {
        if let error = account.error {
            Text(error).foregroundStyle(.red).accessibilityIdentifier("account-error")
        }
    }

    private func read() {
        systemAllows = LiveActivitySettings.systemActivitiesAllowed
        Task { await push.refreshSystemPermission() }
    }
}

/// What Live Activities need before their switch works.
@MainActor enum LiveActivitySettings {
    static var systemActivitiesAllowed: Bool {
        #if WONDER_DIAGNOSTICS
        if settingsFixture {
            return !ProcessInfo.processInfo.arguments.contains("-diagnostics-live-activities-system-off")
        }
        #endif
        return AgentLiveActivities.shared.systemAllows
    }

    #if WONDER_DIAGNOSTICS
    private static var settingsFixture: Bool {
        WonderAccount.usesFixture && ProcessInfo.processInfo.arguments.contains("-diagnostics-live-activities-settings")
    }
    #endif
}

/// The consent line and Sign in with Apple. Shared by Settings and onboarding.
struct AccountSignInSection: View {
    @ObservedObject private var account = WonderAccount.shared
    var body: some View {
        Section {
            Text("Sign in to get notifications and Live Activities.")
                .accessibilityIdentifier("account-consent")
            AccountSignInButton()
                .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 8, trailing: 16))
            if let error = account.error {
                Text(error).foregroundStyle(.red).accessibilityIdentifier("account-error")
            }
            Link("Notification privacy", destination: account.privacyURL)
                .accessibilityIdentifier("account-privacy")
        }
    }
}

struct AccountSignInButton: View {
    @ObservedObject private var account = WonderAccount.shared
    @Environment(\.colorScheme) private var colorScheme
    var body: some View {
        Group {
            #if WONDER_DIAGNOSTICS
            if WonderAccount.usesFixture {
                // Scenarios cannot drive Apple's sheet; the button stays Apple's
                // and a transparent layer above it signs in to the local stand-in.
                appleButton.overlay {
                    Color.clear.contentShape(Rectangle())
                        .onTapGesture { Task { await account.signInWithFixture() } }
                }
            } else { appleButton }
            #else
            appleButton
            #endif
        }
        .frame(height: 50)
        .disabled(account.working)
        .accessibilityIdentifier("account-sign-in-apple").settingsRow("account-sign-in-apple")
    }
    private var appleButton: some View {
        SignInWithAppleButton(.signIn, onRequest: account.prepare, onCompletion: { result in Task { await account.complete(result) } })
            .signInWithAppleButtonStyle(colorScheme == .dark ? .white : .black)
    }
}


/// Settings > Cupola account: who is signed in, and Sign out. Notifications and
/// Live Activities stop on this device until the next sign-in.
struct AccountSettingsView: View {
    @ObservedObject private var account = WonderAccount.shared
    @State private var confirmingDelete = false
    @State private var confirmingSignOut = false
    var body: some View {
        WonderForm {
            if account.isSignedIn {
                Section {
                    LabeledContent("Status", value: "Signed in with Apple")
                    Link("Notification privacy", destination: account.privacyURL)
                        .accessibilityIdentifier("account-privacy")
                }
                Section {
                    Button("Sign out") { confirmingSignOut = true }
                        .accessibilityIdentifier("account-sign-out")
                    Button("Delete account", role: .destructive) { confirmingDelete = true }
                        .accessibilityIdentifier("account-delete")
                }
                .disabled(account.working)
                if let error = account.error {
                    Section { Text(error).foregroundStyle(.red).accessibilityIdentifier("account-error") }
                }
            } else {
                AccountSignInSection()
            }
        }
        .settingsNavigationTitle("Cupola account")
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("account-page")
        .alert("Sign out?", isPresented: $confirmingSignOut) {
            Button("Cancel", role: .cancel) { }
            Button("Sign out") { Task { await account.signOut() } }
        } message: { Text("Notifications and Live Activities stop on this device until you sign in again.") }
        .alert("Delete your Cupola account?", isPresented: $confirmingDelete) {
            Button("Cancel", role: .cancel) { }
            Button("Delete", role: .destructive) { Task { await account.deleteAccount() } }
        } message: { Text("This can’t be undone. Notifications and Live Activities stop on every iPhone and iPad signed in to this account.") }
    }
}
