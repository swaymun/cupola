import Foundation
import Combine
import SwiftUI
import CryptoKit
import UserNotifications
import WonderPairing

private struct PushConfiguration: Decodable, Sendable {
    let endpoint: String?
    let topic: String
    let environment: String
    let previewVersion: Int?
}
struct PushRegistration: Codable {
    let host: String
    let device: String
    let endpoint: String
    let key: Data
    var enabled = true
    var token = ""
    var pendingToken = ""
    var enrollmentSecret = ""
    var nonce = ""
    var senderSecret = ""
    var id: String?
    var previousIDs: [String] = []
    var registeredAt: Date?
    var lastAttempt: Date?
    var macRegistered = false
    var previewVersion: Int?
    var revocationCompleted: Bool?
    /// The Notifications choice the service last confirmed for this registration; nil until it has.
    var alertsSynced: Bool?
    func belongs(to credential: Credential) -> Bool {
        host == credential.hostInstallationId && device == credential.deviceId
    }
}
struct PushDestination: Equatable {
    let host: String
    let conversation: String
    let receipt: String
    /// Routes host attention to the whole-screen viewer; never selects a window.
    var macDialog = false
}
/// The Mac owns the count. Opening the notification never acknowledges a dialog.
struct MacDialogNotice: Decodable, Equatable, Sendable {
    struct Dialog: Decodable, Equatable, Sendable {
        let owner: String
        var title: String? = nil
        var kind: String? = nil
        let count: Int
        var identity: [String] { [owner, title ?? "", kind ?? ""] }
        var label: String {
            let name: String
            if let title {
                name = title
            } else {
                switch kind {
                case "permission": name = "Permission request on your Mac"
                case "accessibility": name = "Accessibility request"
                case "password": name = "Password request"
                case "openConfirmation": name = "Confirm opening a file"
                default: name = "Dialog on your Mac"
                }
            }
            return name + (count > 1 ? " ×\(count)" : "")
        }
    }
    let count: Int
    let dialogs: [Dialog]?
    let summary: String
    let identities: Set<[String]>
    var isValid: Bool {
        (0...256).contains(count) && (dialogs.map { groups in
            groups.count <= 256 && groups.allSatisfy {
                (1...256).contains($0.count) && !$0.owner.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    && $0.owner.utf8.count <= 150 && ($0.title?.utf8.count ?? 0) <= 240 && ($0.kind?.utf8.count ?? 0) <= 80
            } && groups.reduce(0) { $0 + $1.count } == count
        } ?? true)
    }
    init(count: Int, dialogs: [Dialog]? = nil) {
        self.count = count; self.dialogs = dialogs
        summary = dialogs?.map(\.label).joined(separator: "; ") ?? Self.countLabel(count)
        // Count-only older hosts cannot distinguish dialog descriptions.
        identities = Set(dialogs?.map(\.identity) ?? [["legacy-count-only"]])
    }
    private enum CodingKeys: String, CodingKey { case count, dialogs }
    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        self.init(count: try values.decode(Int.self, forKey: .count),
                  dialogs: try values.decodeIfPresent([Dialog].self, forKey: .dialogs))
    }
    static func countLabel(_ count: Int) -> String {
        count == 0 ? "" : count == 1 ? "1 dialog waiting" : "\(count) dialogs waiting"
    }
    static func identifier(host: String) -> String { "mac-dialogs:" + host }
    static func content(host: String, name: String, notice: Self) -> UNMutableNotificationContent {
        let content = UNMutableNotificationContent()
        content.title = name
        content.body = notice.summary
        content.threadIdentifier = identifier(host: host)
        content.userInfo = ["wonderMacDialogs": ["host": host]]
        return content
    }
}
/// Only the computer entry observes this host's changing count. Unrelated
/// push setup, destinations and other hosts cannot invalidate a conversation.
@MainActor final class MacDialogCount: ObservableObject {
    @Published private(set) var notice: MacDialogNotice?
    var count: Int { notice?.count ?? 0 }
    var accessibilityValue: String {
        guard let notice else { return "" }
        let count = MacDialogNotice.countLabel(notice.count)
        return notice.dialogs == nil ? count : count + ". " + notice.summary
    }
    private var observation: AnyCancellable?
    init(host: String, notifications: PushNotifications = .shared) {
        notice = notifications.macDialogs[host]
        observation = notifications.$macDialogs
            .map { $0[host] }.removeDuplicates()
            .sink { [weak self] value in
                guard self?.notice != value else { return }
                self?.notice = value
            }
    }
}

private struct PushActionReceipt: Codable {
    let payload: [String: String]
    let decision: String
    let message: String?
    var optionIndex: Int? = nil
}
private enum PushActionFailure: LocalizedError, Equatable {
    case unavailable, rejected, unconfirmed, storage, queueFull, invalidInstruction, invalidAnswer
    var errorDescription: String? {
        switch self {
        case .invalidInstruction: "Enter a short instruction, or choose Deny without one."
        case .invalidAnswer: "Enter a short answer, or open the conversation to reply."
        case .unavailable: "This request is no longer available. Open the conversation to check it."
        case .rejected: "Your computer did not accept this reply. Open the conversation to review the request."
        case .storage: "Reply could not be saved. Unlock this device and try again."
        case .queueFull: "Too many replies are waiting. Open Cupola and retry them before answering this request."
        case .unconfirmed: "Reply not confirmed. Connect to your computer, then retry the saved reply."
        }
    }
}
private struct PushReceipt: Codable, Equatable {
    let registrationId: String
    let routeId: String
}
private enum PushReceiptIntent {
    case open(PushReceipt), dismiss
}
struct PushSettingsAlert: Identifiable {
    let id = UUID()
    let host: String
    let title: String
    let message: String
    var opensSettings = false
}
/// Whether iOS lets Cupola show notifications right now. It is separate from
/// the saved preference: turning notifications off in iOS Settings keeps the
/// preference, and Settings says why nothing arrives.
enum SystemNotificationPermission: Equatable {
    case unknown, allowed, notAsked, off
    init(_ status: UNAuthorizationStatus) {
        switch status {
        // Quiet (provisional) and App Clip (ephemeral) delivery still arrive.
        case .authorized, .provisional, .ephemeral: self = .allowed
        case .denied: self = .off
        case .notDetermined: self = .notAsked
        @unknown default: self = .unknown
        }
    }
}
enum PushSetupState: Equatable {
    case settingUp
    case needsRetry(String)
}

struct PushRetry {
    private(set) var failures = 0
    private(set) var nextAttempt = Date.distantPast
    mutating func postpone(now: Date = Date()) {
        failures = min(failures + 1, 6)
        nextAttempt = now.addingTimeInterval(min(15 * pow(2, Double(failures - 1)), 300))
    }
}
enum PushFailure: LocalizedError, Hashable {
    case unavailable, identityMismatch, denied
    /// Cupola's notification service: unreachable, over its setup limit, down,
    /// or refusing this device; or Apple refusing the device's push token.
    case serviceUnreachable, rateLimited, serviceDown, serviceRefused, appleRejectedDevice, setupExpired
    /// The computer: it no longer recognizes this device, or could not finish.
    case computerAccess, computerFailed
    var errorDescription: String? {
        switch self {
        case .unavailable: "Notifications could not connect. Make sure your computer is on and Cupola is running, then try again."
        case .identityMismatch: "This iOS build needs a push service configured for its Apple app identity and environment."
        case .denied: "Notifications are off in Settings."
        case .serviceUnreachable: "Couldn’t reach Cupola’s notification service. Check this device’s connection and retry."
        case .rateLimited: "Cupola’s notification service has had too many setup attempts from this device. Wait an hour, then retry."
        case .serviceDown: "Cupola’s notification service isn’t answering right now. Retry in a few minutes."
        case .serviceRefused: "Cupola’s notification service refused this setup. Update Cupola and retry."
        case .appleRejectedDevice: "Apple didn’t accept this device for notifications. Restart it, then retry."
        case .setupExpired: "Notification setup timed out before it was confirmed. Retry."
        case .computerAccess: "Your computer no longer recognizes this device. Pair again from your computer, then retry."
        case .computerFailed: "Your computer couldn’t finish notification setup. Update Cupola on it and retry."
        }
    }

    /// Which side of setup answered, so one status code names the right cause.
    enum Stage { case challenge, proof, computer }
    /// Gives a transport error or an HTTP status from `stage` its own cause.
    /// Anything else (cancellation, sign-in, storage) keeps its own meaning.
    static func classify(_ error: Error, _ stage: Stage) -> Error {
        if error is URLError { return stage == .computer ? PushFailure.unavailable : .serviceUnreachable }
        guard case PairingFailure.response(let status) = error else { return error }
        switch (stage, status) {
        case (.computer, 401), (.computer, 403), (.computer, 404): return PushFailure.computerAccess
        case (.computer, _): return PushFailure.computerFailed
        case (_, 429): return PushFailure.rateLimited
        case (.challenge, 400), (.challenge, 410): return PushFailure.appleRejectedDevice
        case (.proof, 403), (.proof, 409), (.proof, 410): return PushFailure.setupExpired
        case (_, 500...): return PushFailure.serviceDown
        default: return PushFailure.serviceRefused
        }
    }
    /// Apple's own reason a device token was not issued.
    static func registrationMessage(_ error: Error) -> String {
        let nsError = error as NSError
        if nsError.domain == NSCocoaErrorDomain {
            switch nsError.code {
            case 3010: return "This device can’t receive notifications: the iOS Simulator doesn’t support them. Use a physical device."
            case 3000: return "This build of Cupola isn’t allowed to receive notifications. Install it from TestFlight or the App Store."
            default: break
            }
        }
        return "This device couldn’t register with Apple’s notification service. Check your connection and retry."
    }
}

/// One application-owned coordinator. Tokens, private keys and sender capabilities
/// are kept in the device Keychain; preview keys are shared only with the paired Mac and notification extension.
@MainActor final class PushNotifications: ObservableObject {
    static let shared: PushNotifications = {
        #if WONDER_DIAGNOSTICS && targetEnvironment(simulator)
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-rail-fixture"),
           ProcessInfo.processInfo.arguments.contains("-diagnostics-subagent-fixture") {
            return DiagnosticRailFixture.notifications()
        }
        #endif
        #if WONDER_DIAGNOSTICS
        if DiagnosticSubagentFixture.chatOpenFailureFixture || DiagnosticSubagentFixture.queueUXFixture {
            // These owned opening/composer scenarios do not enroll for notifications
            // or read saved Keychain state (including in unsigned simulators).
            return PushNotifications(signingIdentity: DiagnosticSubagentFixture.syntheticSigningIdentity(),
                read: { _ in nil }, write: { _, _ in }, registerForPush: {}, accountService: { nil })
        }
        #endif
        return PushNotifications()
    }()
    // The switch expresses durable user intent, independently of network setup.
    // `enabled` is the registration with the service: it is on for notifications
    // or Live Activities, whichever the owner wants, as long as they are signed
    // in. `alertsEnabled` is only the Notifications switch: the iOS permission
    // prompt and Cupola's own notices.
    static let preferenceKey = "push-enabled-v1"
    static let alertsKey = "push-alerts-v1"
    @Published private(set) var enabled = false
    @Published private(set) var alertsEnabled = false
    /// Whether Live Activities want this phone registered (they run without notifications).
    var liveActivitiesWanted: @MainActor () -> Bool = { AgentLiveActivities.shared.enabled }
    private func registrationWanted(alerts: Bool) -> Bool { alerts || (isSignedIn && liveActivitiesWanted()) }
    private var legacyRequestedDevices: [String: String]?
    // Enrollment follows the current pairing snapshot, never a saved host list.
    private var requestedDevices: [String: String] {
        guard enabled, let library, library.loaded else { return [:] }
        if let legacyRequestedDevices { return legacyRequestedDevices }
        return Dictionary(uniqueKeysWithValues: library.paired.filter { !$0.requiresPairing }.map {
            ($0.credential.hostInstallationId, $0.credential.deviceId)
        })
    }
    @Published private(set) var settingsAlert: PushSettingsAlert?
    @Published private(set) var setupStates: [String: PushSetupState] = [:]
    @Published private(set) var systemPermission = SystemNotificationPermission.unknown
    @Published private(set) var destination: PushDestination?
    @Published private(set) var routingError: String?
    @Published private(set) var macDialogs: [String: MacDialogNotice] = [:]
    @Published private(set) var dismissedMacDialogs: [String: [[String]]] = [:]
    private let dialogPreferences: UserDefaults
    private static let dialogDismissalKey = "mac-dialog-banner-dismissals-v1"
    #if WONDER_DIAGNOSTICS
    private var diagnosticDialogNoticeStarted = false
    #endif
    private var knownDialogHosts: Set<String> = []
    private var dialogDevices: [String: String] = [:]
    private var pendingMacDialogHost: String?
    private var dialogNotificationTasks: [String: Task<Void, Never>] = [:]
    private var dialogEventRevisions: [String: UInt64] = [:]

    /// Validated host events own live counts. Initial/reconnect and push-tap reads
    /// recover missed events; no phone timer starts or keeps the watcher alive.
    func receiveMacDialogEvents(_ events: [ReplayEvent], host: String, name: String) {
        for event in events where event.event.type == "activity"
            && event.event.category == "mac_dialogs" && event.event.state == "changed" {
            guard let detail = event.event.detail,
                  let notice = try? JSONDecoder().decode(MacDialogNotice.self, from: Data(detail.utf8)),
                  notice.isValid else { continue }
            dialogEventRevisions[host, default: 0] &+= 1
            updateMacDialogs(host: host, name: name, notice: notice)
        }
    }

    func refreshMacDialogs(host onlyHost: String? = nil) async {
        guard let library, library.loaded else { return }
        #if WONDER_DIAGNOSTICS
        if DiagnosticSubagentFixture.macPermissionFixture {
            let closed = DiagnosticSubagentFixture.signalled("mac-permission-closed")
            let count = !DiagnosticSubagentFixture.macWaiting || closed ? 0
                : DiagnosticSubagentFixture.signalled("mac-dialog-one") ? 1
                : DiagnosticSubagentFixture.signalled("mac-dialog-three") ? 3 : 2
            updateMacDialogs(host: DiagnosticSubagentFixture.hostID, name: "Fixture Mac",
                             notice: MacDialogNotice(count: count))
            return
        }
        #endif
        guard !library.isPreview else { return }
        let hosts = Set(library.paired.filter { !$0.requiresPairing }.map { $0.credential.hostInstallationId })
        if let host = pendingMacDialogHost, !hosts.contains(host) {
            pendingMacDialogHost = nil
            routingError = "This notification belongs to a computer that is no longer paired."
        }
        for host in Set(macDialogs.keys).subtracting(hosts) {
            updateMacDialogs(host: host, name: "Computer", notice: MacDialogNotice(count: 0))
        }
        for saved in library.paired where !saved.requiresPairing && (onlyHost == nil || saved.credential.hostInstallationId == onlyHost) {
            let host = saved.credential.hostInstallationId, device = saved.credential.deviceId
            let revision = dialogEventRevisions[host, default: 0]
            let model = library.model(for: saved)
            if let previous = dialogDevices[host], previous != device {
                updateMacDialogs(host: host, name: model.macName, notice: MacDialogNotice(count: 0))
            }
            dialogDevices[host] = device
            // A failed read retains attention; only a measured empty snapshot clears it.
            guard let notice: MacDialogNotice = try? await model.api.request("/api/v1/computer/sources/windows?macDialogs=true",
                    origin: saved.origin, credential: saved.credential), !Task.isCancelled,
                  library.paired.contains(where: { $0.credential.hostInstallationId == host && $0.credential.deviceId == device && !$0.requiresPairing }),
                  model.connection?.credential.deviceId == device else {
                if pendingMacDialogHost == host { routingError = "Connect to your computer to open this computer notification, then try again." }
                continue
            }
            guard dialogEventRevisions[host, default: 0] == revision else { continue }
            updateMacDialogs(host: host, name: model.macName, notice: notice)
        }
    }

    func updateMacDialogs(host: String, name: String, notice: MacDialogNotice) {
        guard notice.isValid else { return }
        var notice = notice
        #if WONDER_DIAGNOSTICS
        if DiagnosticSubagentFixture.macPermissionFixture && host == DiagnosticSubagentFixture.hostID {
            // Held count signals select real helper owner/kind pairs without
            // inheriting a dismissal or inventing a requesting app or title.
            if !diagnosticDialogNoticeStarted {
                diagnosticDialogNoticeStarted = true
                dismissedMacDialogs[host] = nil
            }
            let groups: [MacDialogNotice.Dialog] = notice.count == 0 ? [] : [
                .init(owner: notice.count == 1 ? "SecurityAgent" : "UserNotificationCenter",
                      kind: notice.count == 1 ? "password" : "permission", count: notice.count)
            ]
            notice = MacDialogNotice(count: notice.count, dialogs: groups)
        }
        #endif
        // The helper may only identify an owner. Once its dialogs close, a
        // later prompt from that owner (or a count-only host) needs attention.
        if notice.count == 0, dismissedMacDialogs[host] != nil {
            dismissedMacDialogs[host] = nil
            dialogPreferences.set(dismissedMacDialogs, forKey: Self.dialogDismissalKey)
        }
        let next = notice.count > 0 ? notice : nil
        if macDialogs[host] != next || !knownDialogHosts.contains(host) {
            knownDialogHosts.insert(host)
            macDialogs[host] = next
            syncDialogNotification(host: host, name: name)
        }
        // A cold-launch tap waits through locked pairing and an unknown first
        // snapshot. Only an authoritative read may consume this navigation.
        if pendingMacDialogHost == host {
            pendingMacDialogHost = nil
            routingError = nil
            openMacDialogs(host: host)
        }
    }

    func macDialogBannerIsVisible(host: String) -> Bool {
        guard let notice = macDialogs[host] else { return false }
        return !notice.identities.isSubset(of: Set(dismissedMacDialogs[host] ?? []))
    }

    func dismissMacDialogBanner(host: String) {
        guard let notice = macDialogs[host] else { return }
        dismissedMacDialogs[host] = Array(notice.identities)
        dialogPreferences.set(dismissedMacDialogs, forKey: Self.dialogDismissalKey)
    }

    private func syncDialogNotification(host: String, name: String) {
        dialogNotificationTasks[host]?.cancel()
        let notice = macDialogs[host]
        dialogNotificationTasks[host] = Task { [weak self] in
            let center = UNUserNotificationCenter.current(), id = MacDialogNotice.identifier(host: host)
            center.removePendingNotificationRequests(withIdentifiers: [id])
            let delivered = await center.deliveredNotifications().filter {
                $0.request.identifier == id || $0.request.content.threadIdentifier == id
            }.map(\.request.identifier)
            guard !Task.isCancelled else { return }
            center.removeDeliveredNotifications(withIdentifiers: delivered)
            guard let notice, self?.alertsEnabled == true else { return }
            let settings = await center.notificationSettings()
            guard !Task.isCancelled, self?.macDialogs[host] == notice,
                  [.authorized, .provisional, .ephemeral].contains(settings.authorizationStatus) else { return }
            try? await center.add(UNNotificationRequest(identifier: id,
                content: MacDialogNotice.content(host: host, name: name, notice: notice), trigger: nil))
        }
    }

    func openMacDialogs(host: String) {
        guard let notice = macDialogs[host], notice.count > 0 else { return }
        destination = PushDestination(host: host, conversation: "wonder-host-view", receipt: UUID().uuidString, macDialog: true)
        // The OS consumes a tapped notification. Restore its remaining count.
        if let saved = library?.paired.first(where: { $0.credential.hostInstallationId == host }) {
            syncDialogNotification(host: host, name: library?.model(for: saved).macName ?? "Computer")
        }
    }

    func receiveMacDialogTap(host: String) async {
        pendingMacDialogHost = host
        await refreshMacDialogs()
    }
    private var records: [String: PushRegistration] = [:]
    private var token: String?
    private var pendingReceipt: PushReceipt?
    private var pendingActions: [String: PushActionReceipt] = [:]
    private var actionTasks: [String: Task<Void, Never>] = [:]
    private let read: (String) throws -> Data?
    private var savedStateLoaded = false
    // A locked snapshot cannot authorize a write. The newest explicit app-wide
    // intent is applied only after every saved item becomes readable.
    private var preferenceIntent: Bool?
    private var accountEndPending = false
    private var receiptIntent: PushReceiptIntent?
    private let previewKey: (String) throws -> Data?
    private let reportActionFailure: ([String: String]) async -> Void
    private let selectionDirectory: URL?
    private let write: (Data, String) throws -> Void
    private let authorize: () async throws -> Bool
    private let notificationStatus: @MainActor () async -> UNAuthorizationStatus
    private let registerForPush: () -> Void
    /// Push belongs to a Wonder account: the service it signs in to and a
    /// current access token for enrolling (nil service when signed out).
    private let accountService: () -> String?
    private let accessToken: () async throws -> String
    private let api: PairingAPI
    private let homeNetworkTrust = HomeNetworkTrust()
    private let signingIdentity: SigningIdentity
    private weak var library: ConnectionLibrary?
    private var tasks: [String: Task<Void, Never>] = [:]
    private var taskIDs: [String: UUID] = [:]
    private var proofTasks: [String: Task<Void, Never>] = [:]
    private var challengeTasks: [String: Task<Void, Never>] = [:]
    private var routingTask: Task<Void, Never>?
    private var foreground = false
    /// The conversation open on screen, so its Mac keeps only that one quiet.
    private var openConversation: (host: String, id: String)?
    private var presenceTask: Task<Void, Never>?
    private var lastIssued: UInt64 = 0
    private var retries: [String: PushRetry] = [:]

    func setForeground(_ value: Bool) {
        guard foreground != value || presenceTask == nil else { return }
        let resumed = value && !foreground
        foreground = value
        if resumed {
            Task { [weak self] in await self?.refreshMacDialogs() }
            Task { [weak self] in await self?.refreshSystemPermission() }
        }
        if value { retries.removeAll() }
        presenceTask?.cancel()
        presenceTask = Task { [weak self] in
            // A bounded background task gives the final lease-clear request time
            // to finish. Failure still expires on the Mac after 45 seconds, and
            // anything for the open conversation in that window stays quiet, so
            // the clear is retried briefly.
            let background = value ? UIBackgroundTaskIdentifier.invalid : UIApplication.shared.beginBackgroundTask(withName: "Wonder notification presence")
            defer { if background != .invalid { UIApplication.shared.endBackgroundTask(background) } }
            var attempt = 0
            repeat {
                guard let self, !Task.isCancelled else { return }
                let delivered = await sendPresenceToPairedMacs(value)
                if !value {
                    attempt += 1
                    if delivered || attempt >= 3 { return }
                    do { try await Task.sleep(for: .seconds(2)) } catch { return }
                    continue
                }
                refresh()
                do { try await Task.sleep(for: .seconds(15)) } catch { return }
            } while !Task.isCancelled
        }
    }
    /// Report the conversation on screen (nil for none). Only that conversation
    /// stays quiet; notifications for other chats still arrive while Wonder is open.
    func setOpenConversation(host: String?, id: String?) {
        let next = host.flatMap { host in id.map { (host: host, id: $0) } }
        guard next?.host != openConversation?.host || next?.id != openConversation?.id else { return }
        openConversation = next
        guard foreground else { return }
        presenceTask?.cancel()
        presenceTask = nil
        setForeground(true)
    }
    /// Whether every paired Mac with notifications on accepted the update.
    private func sendPresenceToPairedMacs(_ value: Bool) async -> Bool {
        guard savedStateLoaded, let library, library.loaded, !library.isPreview else { return true }
        let connections = library.paired.filter { saved in
            !saved.requiresPairing && requestedDevices[saved.credential.hostInstallationId] == saved.credential.deviceId
        }
        return await withTaskGroup(of: Bool.self) { group in
            for saved in connections {
                group.addTask {
                    await self.sendPresence(value, saved: saved)
                }
            }
            var delivered = true
            for await result in group where !result { delivered = false }
            return delivered
        }
    }
    private func sendPresence(_ value: Bool, saved: SavedConnection) async -> Bool {
        homeNetworkTrust.update(library?.paired ?? [])
        guard !Task.isCancelled, foreground == value, records[saved.credential.hostInstallationId]?.belongs(to: saved.credential) == true else { return true }
        let state = value ? "foreground" : "background"
        let host = saved.credential.hostInstallationId
        let conversation = value ? (openConversation?.host == host ? openConversation?.id ?? "" : "") : nil
        struct Presence: Decodable, Sendable { let updated: Bool }
        func send(_ conversation: String?) async throws {
            var fields = ["state": state], values = [state]
            if let conversation { fields["conversationId"] = conversation; values.append(conversation) }
            let body = try await signedBody(action: "push.presence", path: "/api/v1/push/presence", values: values, fields: fields, saved: saved)
            try Task.checkCancellation()
            guard foreground == value else { throw CancellationError() }
            let _: Presence = try await api.request("/api/v1/push/presence", origin: saved.origin, body: body, credential: saved.credential)
        }
        do {
            try await send(conversation)
            return true
        } catch is CancellationError {
            return true
        } catch {
            // A Mac from before per-conversation quiet rejects the extra field;
            // it then keeps every chat quiet while Wonder is in front, as before.
            guard conversation != nil, !Task.isCancelled else { return false }
            do { try await send(nil); return true } catch { return false }
        }
    }

    init(
        api: PairingAPI? = nil,
        signingIdentity: SigningIdentity = PhoneIdentity.signing,
        read: @escaping (String) throws -> Data? = { try PhoneIdentity().read($0) },
        write: @escaping (Data, String) throws -> Void = { try PhoneIdentity().save($0, account: $1) },
        authorize: @escaping () async throws -> Bool = { try await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) },
        notificationStatus: @escaping @MainActor () async -> UNAuthorizationStatus = { await UNUserNotificationCenter.current().notificationSettings().authorizationStatus },
        registerForPush: @escaping () -> Void = { UIApplication.shared.registerForRemoteNotifications() },
        accountService: @escaping () -> String? = { WonderAccount.shared.isSignedIn ? WonderAccount.shared.service : nil },
        accessToken: @escaping () async throws -> String = { try await WonderAccount.shared.accessToken() },
        previewKey: @escaping (String) throws -> Data? = PushPreviewKeys.read,
        selectionDirectory: URL? = nil,
        dialogPreferences: UserDefaults = .standard,
        reportActionFailure: @escaping ([String: String]) async -> Void = { payload in
            let content = UNMutableNotificationContent()
            content.title = "Reply not confirmed"
            content.body = "Open Cupola to check this request."
            content.userInfo = ["wonderPush": payload]
            try? await UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "reply-" + (payload["eventId"] ?? UUID().uuidString), content: content, trigger: nil))
        }
    ) {
        self.api = api ?? PairingAPI(serverTrustDelegate: homeNetworkTrust)
        self.signingIdentity = signingIdentity
        self.read = read; self.write = write; self.authorize = authorize; self.registerForPush = registerForPush
        self.notificationStatus = notificationStatus
        self.accountService = accountService; self.accessToken = accessToken
        self.previewKey = previewKey; self.reportActionFailure = reportActionFailure
        self.selectionDirectory = selectionDirectory
        self.dialogPreferences = dialogPreferences
        dismissedMacDialogs = dialogPreferences.object(forKey: Self.dialogDismissalKey) as? [String: [[String]]] ?? [:]
        loadSavedState()
    }
    /// A failed Keychain read is not an absent item. Publish the whole snapshot
    /// together, and never let a partial snapshot authorize writes or cleanup.
    /// Foreground refresh and notification callbacks retry until this succeeds.
    @discardableResult private func loadSavedState() -> Bool {
        guard !savedStateLoaded else { return migratePreference() }
        do {
            let records = try read("push-registrations-v1").map { try JSONDecoder().decode([String: PushRegistration].self, from: $0) } ?? [:]
            let preferenceData = try read(Self.preferenceKey)
            var preference: Bool
            var legacy: [String: String]?
            if let preferenceData {
                preference = try JSONDecoder().decode(Bool.self, from: preferenceData)
            } else if let data = try read("push-requested-devices-v1") {
                legacy = try JSONDecoder().decode([String: String].self, from: data)
                preference = !(legacy?.isEmpty ?? true)
            } else {
                preference = records.values.contains { $0.enabled }
                legacy = records.filter { $0.value.enabled }.mapValues(\.device)
            }
            var actions: [String: PushActionReceipt] = [:]
            if let data = try read("push-actions-v1"), !data.isEmpty {
                actions = try JSONDecoder().decode([String: PushActionReceipt].self, from: data)
            }
            var receipt: PushReceipt?
            if let data = try read("push-receipt-v1"), !data.isEmpty { receipt = try JSONDecoder().decode(PushReceipt.self, from: data) }
            // Builds before Live Activities ran without notifications registered
            // for both: their one switch becomes the alerts switch.
            var alerts = try read(Self.alertsKey).map { try JSONDecoder().decode(Bool.self, from: $0) } ?? preference
            if accountEndPending { alerts = false }
            if let preferenceIntent { alerts = preferenceIntent }
            let changedPreference = accountEndPending || preferenceIntent != nil
            if changedPreference {
                preference = registrationWanted(alerts: alerts) && !accountEndPending
                try write(JSONEncoder().encode(alerts), Self.alertsKey)
                try write(JSONEncoder().encode(preference), Self.preferenceKey)
                legacy = nil
            }
            if let receiptIntent {
                switch receiptIntent {
                case .open(let newer): receipt = newer
                case .dismiss: receipt = nil
                }
                try write(try receipt.map { try JSONEncoder().encode($0) } ?? Data(), "push-receipt-v1")
            }
            self.records = records; pendingActions = actions; pendingReceipt = receipt
            savedStateLoaded = true
            preferenceIntent = nil; accountEndPending = false; receiptIntent = nil
            legacyRequestedDevices = legacy
            enabled = preference
            alertsEnabled = alerts
            if changedPreference { settingsAlert = nil }
            routingError = actions.isEmpty ? nil : PushActionFailure.unconfirmed.localizedDescription
            return migratePreference()
        } catch {
            routingError = "Unlock this device to open saved notification settings."
            return false
        }
    }
    @discardableResult private func migratePreference() -> Bool {
        let changedPreference = accountEndPending || preferenceIntent != nil
        guard legacyRequestedDevices != nil || changedPreference else { return true }
        guard changedPreference || library?.loaded == true else { return true }
        // Only a current pairing counted as On in the old Settings. Stale or
        // replaced credentials cannot turn the app-wide preference on.
        var value = library?.paired.contains {
            !$0.requiresPairing && legacyRequestedDevices?[$0.credential.hostInstallationId] == $0.credential.deviceId
        } ?? enabled
        if accountEndPending { value = false }
        if let preferenceIntent { value = preferenceIntent }
        let registration = registrationWanted(alerts: value) && !accountEndPending
        do {
            try write(JSONEncoder().encode(value), Self.alertsKey)
            try write(JSONEncoder().encode(registration), Self.preferenceKey)
            self.legacyRequestedDevices = nil
            preferenceIntent = nil; accountEndPending = false
            enabled = registration
            alertsEnabled = value
            if changedPreference { settingsAlert = nil }
            return true
        } catch {
            routingError = "Unlock this device to open saved notification settings."
            return false
        }
    }
    func isEnabled(_ host: String) -> Bool {
        guard let saved = library?.paired.first(where: { $0.credential.hostInstallationId == host }) else { return false }
        // Notifications need an account; saved intent alone does not count.
        return isSignedIn && requestedDevices[host] == saved.credential.deviceId
    }
    func attach(_ library: ConnectionLibrary) {
        self.library = library
        Task { [weak self] in await self?.refreshMacDialogs() }
        guard !library.isPreview else { _ = loadSavedState(); return }
        registerForPush()
        setForeground(UIApplication.shared.applicationState != .background)
        refresh()
    }
    var isSignedIn: Bool { accountService() != nil }
    /// Reads iOS's current permission without asking for it: on foreground and
    /// whenever Settings shows the switch.
    func refreshSystemPermission() async {
        let next = SystemNotificationPermission(await notificationStatus())
        if systemPermission != next { systemPermission = next }
    }
    /// The account ended on this iPhone (sign-out, deletion, or the service
    /// forgetting the session): turn notifications off for every Mac. The
    /// Macs drop their capability through the usual revocation.
    func accountEnded() {
        guard loadSavedState() else {
            accountEndPending = true
            preferenceIntent = nil
            settingsAlert = nil
            return
        }
        guard setPreference(false) else {
            accountEndPending = true
            preferenceIntent = nil
            settingsAlert = nil
            return
        }
        disableAll()
        settingsAlert = nil
    }
    func refresh() {
        guard loadSavedState(), let library, library.loaded, !library.isPreview else { return }
        reconcileEnrollments()
        resolveReceipt()
    }
    private func reconcileEnrollments() {
        guard let library, library.loaded else { return }
        // Saved intent from before sign-out (or from a build without accounts)
        // cannot be honoured; Settings explains how to sign in.
        if !isSignedIn && enabled { accountEnded() }
        // Signed in with Live Activities on: register even with notifications off.
        if isSignedIn, !enabled, liveActivitiesWanted() { _ = setPreference(alertsEnabled) }
        // A replacement pairing must retire the old delivery capability even
        // when the old Mac credential is no longer in the connection library.
        for (host, record) in records where !library.paired.contains(where: { record.belongs(to: $0.credential) }) {
            if record.enabled { disable(host) }
            else { scheduleDisable(host) }
        }
        for saved in library.paired {
            let host = saved.credential.hostInstallationId
            if requestedDevices[host] == saved.credential.deviceId {
                // Runs every 15 s: removing an absent state would still re-render.
                if isRegistered(host) { if setupStates[host] != nil { setupStates.removeValue(forKey: host) } }
                else if setupStates[host] == nil { setupStates[host] = .settingUp }
                if let record = records[host], !record.nonce.isEmpty,
                   let lastAttempt = record.lastAttempt, Date().timeIntervalSince(lastAttempt) >= 45 {
                    challengeTimedOut(host, nonce: record.nonce)
                }
                schedule(host)
                syncAlerts(host)
            }
            else if records[host]?.enabled == true { disable(host) }
            else { scheduleDisable(host) }
        }
    }
    /// Tell the service whether this registration may show banners. Live
    /// Activities keep updating either way; a failure retries on the next pass.
    private func syncAlerts(_ host: String) {
        guard alertsSyncing.insert(host).inserted else { return }
        guard let record = records[host], record.enabled, record.macRegistered, let id = record.id, record.alertsSynced != alertsEnabled else {
            alertsSyncing.remove(host); return
        }
        let wanted = alertsEnabled
        Task { [weak self] in
            guard let self else { return }
            defer { alertsSyncing.remove(host) }
            struct Updated: Decodable, Sendable { let alerts: Bool }
            do {
                let body = try JSONSerialization.data(withJSONObject: ["enabled": wanted])
                let _: Updated = try await api.request("/v1/registrations/\(id)/alerts", origin: record.endpoint, body: body, bearerToken: record.senderSecret)
                guard records[host]?.id == id else { return }
                records[host]?.alertsSynced = wanted; try? persist()
            } catch { /* The next reconcile pass retries. */ }
        }
    }
    private var alertsSyncing = Set<String>()
    func receivedToken(_ data: Data) {
        let value = data.map { String(format: "%02x", $0) }.joined()
        if token != value { token = value; retries.removeAll() }
        for host in requestedDevices.keys where isRegistered(host) && setupStates[host] != nil { setupStates.removeValue(forKey: host) }
        refresh()
    }
    func registrationFailed(_ error: Error) {
        guard loadSavedState(), token == nil else { return }
        for host in requestedDevices.keys where isEnabled(host) {
            setupStates[host] = .needsRetry(PushFailure.registrationMessage(error))
        }
    }
    func setupState(_ host: String) -> PushSetupState? { isEnabled(host) ? setupStates[host] : nil }
    func retry(_ host: String) {
        guard loadSavedState(), isEnabled(host) else { return }
        if isRegistered(host) { setupStates.removeValue(forKey: host); return }
        challengeTasks.removeValue(forKey: host)?.cancel()
        tasks.removeValue(forKey: host)?.cancel()
        taskIDs.removeValue(forKey: host)
        if records[host]?.enabled == true {
            records[host]?.lastAttempt = nil
            records[host]?.nonce = ""
            do { try persist() }
            catch {
                setupStates[host] = .needsRetry("Unlock this device, then retry notification setup.")
                return
            }
        }
        retries.removeValue(forKey: host)
        setupStates[host] = .settingUp
        if token == nil { registerForPush() }
        schedule(host)
    }
    private func isRegistered(_ host: String) -> Bool {
        guard let record = records[host], isEnabled(host) else { return false }
        return record.enabled && record.macRegistered && record.id != nil && record.token == token
    }
    func enable(_ model: ConnectionModel) {
        // The demo computer cannot send notifications; it is never enrolled.
        guard let saved = model.connection, isSignedIn, !model.isDemo, !DemoMode.isDemo(saved) else { return }
        guard let library else { return }
        setAll(enabled: true, library: library)
    }
    /// Durable app-wide intent; existing and future pairings share this switch.
    func setAll(enabled: Bool, library: ConnectionLibrary) {
        guard !enabled || isSignedIn else { return }
        self.library = library
        guard setPreference(enabled) else { return }
        settingsAlert = nil
        retries.removeAll()
        // The registration stays while Live Activities still need it.
        if !self.enabled { disableAll() }
        reconcileEnrollments()
    }
    /// Live Activities were turned on or off: register this phone for them (or
    /// stop, unless notifications still need the registration).
    func liveActivitiesChanged(library: ConnectionLibrary?) {
        if let library { self.library = library }
        guard setPreference(alertsEnabled) else { return }
        retries.removeAll()
        if !enabled { disableAll() }
        reconcileEnrollments()
    }
    private func prepare(_ host: String) async throws {
        guard let device = requestedDevices[host], let model = model(host, device: device), let saved = model.connection else { return }
        // Live Activities alone need no notification permission.
        if alertsEnabled { guard try await authorize() else { throw PushFailure.denied } }
        try Task.checkCancellation()
        guard requestedDevices[host] == device, model.connection?.credential.deviceId == device else { return }
        // Already registered devices need no network work until renewal or rotation.
        if let record = records[host], record.enabled, record.belongs(to: saved.credential) {
            if token == nil { registerForPush() }
            try await enroll(host)
            return
        }
        let config: PushConfiguration
        do { config = try await model.api.request("/api/v1/push/config", origin: saved.origin, credential: saved.credential) }
        catch { throw PushFailure.classify(error, .computer) }
        let environment = Bundle.main.object(forInfoDictionaryKey: "WonderAPNsEnvironment") as? String ?? "production"
        guard config.topic == Bundle.main.bundleIdentifier, config.environment == environment,
              let endpoint = config.endpoint, let url = URL(string: endpoint), url.scheme == "https", url.host != nil,
              url.user == nil, url.password == nil, url.query == nil, url.fragment == nil else { throw PushFailure.identityMismatch }
        // The registration belongs to the account, so it must live on the account's service.
        guard let service = accountService() else { throw AccountFailure.signedOut }
        guard endpoint.trimmingCharacters(in: CharacterSet(charactersIn: "/")) == service else { throw AccountFailure.otherService }
        try Task.checkCancellation()
        guard requestedDevices[host] == device, model.connection?.credential.deviceId == device else { throw CancellationError() }
        let previous = records[host]
        if records[host]?.device != saved.credential.deviceId || records[host]?.endpoint != endpoint || records[host]?.enabled != true {
            if let previous { try await revokeAtService(previous) }
            try Task.checkCancellation()
            guard requestedDevices[host] == device, model.connection?.credential.deviceId == device else { throw CancellationError() }
            records[host] = PushRegistration(host: host, device: saved.credential.deviceId, endpoint: endpoint, key: P256.Signing.PrivateKey().rawRepresentation)
        }
        do { try persist() } catch { records[host] = previous; throw error }
        registerForPush()
        try await enroll(host)
    }
    private func schedule(_ host: String) {
        guard tasks[host] == nil, isEnabled(host), (retries[host]?.nextAttempt ?? .distantPast) <= Date() else { return }
        if let record = records[host], record.enabled, record.device == requestedDevices[host], record.macRegistered,
           record.previewVersion == 1, record.token == token, let date = record.registeredAt, Date().timeIntervalSince(date) < 30 * 86400 { return }
        let id = UUID(); taskIDs[host] = id
        tasks[host] = Task { [weak self] in
            guard let self else { return }
            do { try await prepare(host) }
            catch { if !Task.isCancelled { setupFailed(error, host: host) } }
            if taskIDs[host] == id {
                tasks.removeValue(forKey: host); taskIDs.removeValue(forKey: host)
                if isEnabled(host) { retries[host, default: PushRetry()].postpone() }
            }
        }
    }
    private func model(_ host: String, device: String) -> ConnectionModel? {
        guard let library, let saved = library.paired.first(where: { $0.credential.hostInstallationId == host && $0.credential.deviceId == device && !$0.requiresPairing }) else { return nil }
        return library.model(for: saved)
    }
    private func enroll(_ host: String) async throws {
        guard !Task.isCancelled, var record = records[host], record.enabled, let token, let model = model(host, device: record.device) else { return }
        if record.token == token, record.id != nil, let date = record.registeredAt, Date().timeIntervalSince(date) < 30 * 86400 {
            if !record.macRegistered || record.previewVersion != 1 { try await registerWithMac(record, model: model) }
            try Task.checkCancellation()
            guard records[host]?.enabled == true else { return }
            return
        }
        // A pending proof may still arrive for ten minutes. Do not consume the
        // service's three-per-hour challenge quota in a foreground retry loop.
        if !record.nonce.isEmpty && record.pendingToken == token { return }
        // The access token goes only to the account's own service.
        guard let service = accountService() else { throw AccountFailure.signedOut }
        guard record.endpoint.trimmingCharacters(in: CharacterSet(charactersIn: "/")) == service else { throw AccountFailure.otherService }
        record.pendingToken = token; record.nonce = try randomSecret(); record.enrollmentSecret = try randomSecret(); record.lastAttempt = Date(); record.macRegistered = false
        records[host] = record; try persist()
        setupStates[host] = .settingUp
        let key = try P256.Signing.PrivateKey(rawRepresentation: record.key)
        let body = try JSONSerialization.data(withJSONObject: ["token": token, "publicKey": key.publicKey.x963Representation.base64URL, "nonce": record.nonce])
        let access = try await accessToken()
        struct Challenge: Decodable, Sendable { let id: String }
        do { let _: Challenge = try await api.request("/v1/challenges", origin: record.endpoint, body: body, decodingStatuses: [202], bearerToken: access) }
        catch PairingFailure.response(401) { throw AccountFailure.signedOut }
        catch { throw PushFailure.classify(error, .challenge) }
        // The proof arrives via APNs, never in this HTTP response.
        try Task.checkCancellation()
        let nonce = record.nonce
        challengeTasks[host]?.cancel()
        challengeTasks[host] = Task { [weak self] in
            try? await Task.sleep(for: .seconds(45))
            guard let self, !Task.isCancelled else { return }
            self.challengeTimedOut(host, nonce: nonce)
        }
    }
    func challengeTimedOut(_ host: String, nonce: String) {
        guard proofTasks[host] == nil, records[host]?.nonce == nonce,
              records[host]?.macRegistered != true, isEnabled(host) else { return }
        setupStates[host] = .needsRetry("Apple didn’t confirm notification setup. Check your connection and retry.")
    }
    func receiveChallenge(id: String, challenge: String, nonce: String) async {
        guard loadSavedState(), UUID(uuidString: id) != nil, challenge.count == 43,
              let (host, record) = records.first(where: { $0.value.enabled && $0.value.nonce == nonce }),
              isEnabled(host),
              let model = model(host, device: record.device) else { return }
        guard proofTasks[host] == nil else { return }
        challengeTasks.removeValue(forKey: host)?.cancel()
        let task = Task { [weak self] in
            guard let self else { return }
            await acceptChallenge(id: id, challenge: challenge, nonce: nonce, host: host, record: record, model: model)
        }
        proofTasks[host] = task
        await task.value
        proofTasks.removeValue(forKey: host)
    }
    private func acceptChallenge(id: String, challenge: String, nonce: String, host: String, record: PushRegistration, model: ConnectionModel) async {
        do {
            try Task.checkCancellation()
            let key = try P256.Signing.PrivateKey(rawRepresentation: record.key)
            let signature = try key.signature(for: Data("register|\(id)|\(challenge)|\(record.enrollmentSecret)".utf8)).rawRepresentation.base64URL
            let data = try JSONSerialization.data(withJSONObject: ["id": id, "challenge": challenge, "senderSecret": record.enrollmentSecret, "signature": signature])
            struct Accepted: Decodable, Sendable { let id: String }
            let accepted: Accepted
            do { accepted = try await api.request("/v1/registrations", origin: record.endpoint, body: data) }
            catch { throw PushFailure.classify(error, .proof) }
            try Task.checkCancellation()
            guard accepted.id == id, records[host]?.nonce == nonce, records[host]?.enabled == true else { throw CancellationError() }
            var updated = record
            if record.previousIDs.count >= 16, let oldest = record.previousIDs.last { try PushPreviewKeys.remove(oldest) }
            if let old = record.id, old != id { updated.previousIDs = Array(([old] + record.previousIDs).prefix(16)) }
            updated.id = id; updated.alertsSynced = nil; updated.token = record.pendingToken; updated.senderSecret = record.enrollmentSecret; updated.registeredAt = Date(); updated.macRegistered = false
            updated.nonce = ""
            records[host] = updated; try persist()
            do { try await registerWithMac(updated, model: model) }
            catch { throw PushFailure.classify(error, .computer) }
            challengeTasks.removeValue(forKey: host)?.cancel()
            retries.removeValue(forKey: host)
        } catch {
            if !Task.isCancelled {
                setupFailed(error, host: host)
                if isEnabled(host) { retries[host, default: PushRetry()].postpone() }
            }
        }
    }
    private func registerWithMac(_ record: PushRegistration, model: ConnectionModel) async throws {
        try Task.checkCancellation()
        guard let id = record.id, records[record.host]?.enabled == true, let saved = model.connection, record.belongs(to: saved.credential) else { throw CancellationError() }
        let config: PushConfiguration = try await model.api.request("/api/v1/push/config", origin: saved.origin, credential: saved.credential)
        var values = [id, record.senderSecret], fields = ["id": id, "senderSecret": record.senderSecret]
        if config.previewVersion == 1 {
            let key = try PushPreviewKeys.getOrCreate(id).base64URL
            values.append(key); fields["previewKey"] = key
        }
        // Establish foreground presence before enabling delivery on this Mac.
        await sendPresence(foreground, saved: saved)
        try Task.checkCancellation()
        guard records[record.host]?.id == id, records[record.host]?.enabled == true else { throw CancellationError() }
        let body = try await signedBody(action: "push.register", path: "/api/v1/push/registration", values: values, fields: fields, saved: saved)
        struct Registered: Decodable, Sendable { let registered: Bool }
        let result: Registered = try await model.api.request("/api/v1/push/registration", origin: saved.origin, body: body, credential: saved.credential)
        guard result.registered, records[record.host]?.id == id, records[record.host]?.enabled == true else { throw CancellationError() }
        records[record.host]?.macRegistered = true; records[record.host]?.previewVersion = config.previewVersion; try persist()
        setupStates.removeValue(forKey: record.host)
    }
    func disable(_ model: ConnectionModel) {
        guard model.connection != nil, let library else { return }
        setAll(enabled: false, library: library)
    }
    func dismissSettingsAlert() { settingsAlert = nil }
    @discardableResult private func setPreference(_ value: Bool, host: String = "") -> Bool {
        guard loadSavedState() else {
            preferenceIntent = value
            settingsAlert = PushSettingsAlert(host: host, title: "Couldn’t change notifications", message: "Unlock this device and try again.")
            return false
        }
        do {
            let registration = registrationWanted(alerts: value)
            try write(JSONEncoder().encode(value), Self.alertsKey)
            try write(JSONEncoder().encode(registration), Self.preferenceKey)
            legacyRequestedDevices = nil
            alertsEnabled = value
            enabled = registration
            for host in macDialogs.keys {
                let saved = library?.paired.first { $0.credential.hostInstallationId == host }
                syncDialogNotification(host: host, name: saved.flatMap { library?.model(for: $0).macName } ?? "Computer")
            }
            return true
        } catch {
            settingsAlert = PushSettingsAlert(host: host, title: "Couldn’t change notifications", message: "Unlock this device and try again.")
            return false
        }
    }
    private func setupFailed(_ error: Error, host: String) {
        guard isEnabled(host) else { return }
        let message: String
        var opensSettings = false
        switch error {
        case PushFailure.denied:
            message = "Allow notifications for Cupola in Settings."
            opensSettings = true
        case PushFailure.identityMismatch:
            message = "This build needs a matching notification service. Check the push configuration on your computer."
        case let failure as AccountFailure where failure == .signedOut || failure == .otherService:
            message = failure.localizedDescription
        case PairingFailure.wrongHost, PairingFailure.missingIdentity, PairingFailure.expired:
            message = "Pair this device with your computer again, then retry notification setup."
        case is SigningIdentityFailure:
            message = "Unlock this device and try again."
        case let failure as PushFailure:
            message = failure.localizedDescription
        case let failure as AccountFailure where failure == .unavailable || failure == .rejected || failure == .storage:
            message = failure.localizedDescription
        default:
            // Preserve intent for automatic retries, but make a stalled setup visible.
            setupStates[host] = .needsRetry("Couldn’t finish notification setup. Check your connection and retry.")
            return
        }
        // Permission and account loss apply to the app. A single Mac's setup
        // failure must not switch off other computers or future enrollment.
        if opensSettings || (error as? AccountFailure) == .signedOut {
            guard setPreference(false, host: host) else { return }
            reconcileEnrollments()
        } else {
            setupStates[host] = .needsRetry(message)
        }
        // A computer that is not confirmed connected (offline, or state still
        // being read) retries on reconnect; its setup failure is not worth an
        // alert (owner feedback 34). Settings still shows the retry state.
        if !opensSettings, let library, let saved = library.paired.first(where: { $0.credential.hostInstallationId == host }),
           [.offline, .checking].contains(ComputerState(library.model(for: saved))) { return }
        settingsAlert = PushSettingsAlert(host: host, title: opensSettings ? "Couldn’t turn on notifications" : "Couldn’t finish notification setup", message: message, opensSettings: opensSettings)
    }
    private func disable(_ host: String) {
        setupStates.removeValue(forKey: host)
        challengeTasks.removeValue(forKey: host)?.cancel()
        retries.removeValue(forKey: host)
        if records[host] != nil {
            records[host]?.enabled = false
            records[host]?.nonce = ""
            // The saved preference already authorizes revocation after a restart.
            do { try persist() } catch { /* Retry cleanup while the desired state stays off. */ }
        }
        if let record = records[host] {
            for id in ([record.id].compactMap { $0 } + record.previousIDs) { try? PushPreviewKeys.remove(id) }
        }
        let enrollment = tasks[host], proof = proofTasks[host]
        enrollment?.cancel(); proof?.cancel()
        tasks.removeValue(forKey: host)
        scheduleDisable(host, after: enrollment, proof: proof)
    }
    private func disableAll() {
        let hosts = Set(records.keys).union(tasks.keys).union(proofTasks.keys).union(challengeTasks.keys)
        setupStates.removeAll()
        for host in hosts { disable(host) }
    }
    private func revokeAtService(_ record: PushRegistration) async throws {
        guard let id = record.id else { return }
        struct Revoked: Decodable, Sendable { let revoked: Bool }
        do {
            let _: Revoked = try await api.request("/v1/registrations/\(id)/revoke", origin: record.endpoint, body: Data("{}".utf8), bearerToken: record.senderSecret)
        } catch PairingFailure.response(410) { /* Already revoked or expired. */ }
    }
    private func scheduleDisable(_ host: String, after enrollment: Task<Void, Never>? = nil, proof: Task<Void, Never>? = nil) {
        guard tasks[host] == nil, let pending = records[host], pending.revocationCompleted != true else { return }
        guard (retries[host]?.nextAttempt ?? .distantPast) <= Date() else { return }
        let id = UUID(); taskIDs[host] = id
        tasks[host] = Task { [weak self] in
            guard let self else { return }
            defer {
                if taskIDs[host] == id {
                    tasks.removeValue(forKey: host); taskIDs.removeValue(forKey: host)
                    if isEnabled(host) { schedule(host) }
                }
            }
            await enrollment?.value; await proof?.value
            guard let record = records[host], !record.enabled else { return }
            do {
                var failure: Error?
                do { try await revokeAtService(record) } catch { failure = error }
                if let model = model(host, device: record.device), let saved = model.connection {
                    do {
                        let body = try await signedBody(action: "push.revoke", path: "/api/v1/push/revoke", values: [], fields: [:], saved: saved)
                        struct Revoked: Decodable, Sendable { let revoked: Bool }
                        let _: Revoked = try await model.api.request("/api/v1/push/revoke", origin: saved.origin, body: body, credential: saved.credential)
                    } catch { failure = error }
                }
                if let failure { throw failure }
                records[host]?.revocationCompleted = true
                try persist()
                retries.removeValue(forKey: host)
            } catch { retries[host, default: PushRetry()].postpone() }
        }
    }
    var canRetryAction: Bool { !pendingActions.isEmpty }
    func retryActions() {
        guard loadSavedState() else { return }
        for receipt in pendingActions.values {
            Task { await receiveAction(receipt.payload, decision: receipt.decision, message: receipt.message, optionIndex: receipt.optionIndex) }
        }
    }
    func receiveForwardedSelection(_ payload: [String: String]) async -> Bool {
        guard let index = try? PushQuestionSelection.read(payload, directory: selectionDirectory) else { return false }
        await receiveAction(payload, decision: "respond", optionIndex: index)
        return true
    }
    /// The OS callback has one deadline, including cold launch and renewal.
    /// Each receipt keeps its own answer; navigation's single receipt is separate.
    func receiveAction(_ payload: [String: String], decision: String, message suppliedMessage: String? = nil, optionIndex: Int? = nil, timeout: Duration = .seconds(20)) async {
        guard ["accept", "decline", "respond"].contains(decision) else { return }
        do {
            guard loadSavedState() else { throw PushActionFailure.storage }
            if optionIndex == nil {
                if decision == "respond", suppliedMessage == nil { throw PushActionFailure.invalidAnswer }
                if let suppliedMessage {
                    guard !suppliedMessage.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                          suppliedMessage.utf8.count <= 2000 else {
                        throw decision == "respond" ? PushActionFailure.invalidAnswer : PushActionFailure.invalidInstruction
                    }
                }
            }
            guard let registration = payload["registrationId"], let key = try previewKey(registration) else { throw PushActionFailure.unavailable }
            let preview = try PushPreview.decrypt(payload, key: key)
            guard let approval = preview.action else { throw PushActionFailure.unavailable }
            let message: String?
            if let optionIndex {
                guard decision == "respond", approval.questionId != nil,
                      let options = approval.questionOptions, options.indices.contains(optionIndex),
                      suppliedMessage == nil || suppliedMessage == options[optionIndex].label else { throw PushActionFailure.unavailable }
                message = options[optionIndex].label
            } else { message = suppliedMessage }
            if decision == "respond" {
                guard approval.questionId != nil else { throw PushActionFailure.unavailable }
            } else if approval.questionId != nil {
                throw PushActionFailure.unavailable
            } else if message != nil {
                guard decision == "decline", approval.supportsDenyMessage == true else { throw PushActionFailure.unavailable }
            }
            let id = registration + ":" + approval.approvalId + ":" + approval.actionNonce
            if let receipt = pendingActions[id], receipt.decision != decision || receipt.message != message || receipt.optionIndex != optionIndex { throw PushActionFailure.unconfirmed }
            if actionTasks[id] != nil { return }
            guard pendingActions[id] != nil || pendingActions.count < 32 else { throw PushActionFailure.queueFull }
            pendingActions[id] = PushActionReceipt(payload: payload, decision: decision, message: message, optionIndex: optionIndex)
            do { try persistActions() } catch { throw PushActionFailure.storage }
            // The app's private, durable receipt now owns this selection.
            try? PushQuestionSelection.remove(payload, directory: selectionDirectory)
            await withCheckedContinuation { continuation in
                var finished = false
                let task = Task { @MainActor in
                    do {
                        try await performAction(payload, preview: preview, decision: decision, message: message, optionIndex: optionIndex)
                        guard !Task.isCancelled else { return }
                        pendingActions.removeValue(forKey: id)
                        do {
                            try persistActions()
                            if pendingActions.isEmpty { routingError = nil }
                        } catch {
                            routingError = "Reply delivered, but its saved notification could not be cleared. Unlock this device and reopen Cupola."
                        }
                    } catch {
                        guard !Task.isCancelled else { return }
                        if let failure = error as? PushActionFailure, failure != .unconfirmed {
                            pendingActions.removeValue(forKey: id)
                            try? persistActions()
                        }
                        routingError = (error as? PushActionFailure ?? .unconfirmed).localizedDescription
                        if !foreground { Task { await reportActionFailure(payload) } }
                    }
                    guard !finished else { return }
                    finished = true; actionTasks.removeValue(forKey: id)
                    continuation.resume()
                }
                actionTasks[id] = task
                Task { @MainActor in
                    try? await Task.sleep(for: timeout)
                    guard !finished else { return }
                    finished = true; task.cancel(); actionTasks.removeValue(forKey: id)
                    routingError = PushActionFailure.unconfirmed.localizedDescription
                    continuation.resume()
                    if !foreground { Task { await reportActionFailure(payload) } }
                }
            }
        } catch {
            routingError = (error as? PushActionFailure ?? .unconfirmed).localizedDescription
            if !foreground { Task { await reportActionFailure(payload) } }
        }
    }
    private func persistActions() throws {
        try requireSavedState()
        try write(JSONEncoder().encode(pendingActions), "push-actions-v1")
    }
    private func performAction(_ payload: [String: String], preview: PushPreview, decision: String, message: String?, optionIndex: Int?) async throws {
        while library?.loaded != true {
            try Task.checkCancellation()
            try await Task.sleep(for: .milliseconds(50))
        }
        guard library != nil, let registration = payload["registrationId"], let routeID = payload["routeId"],
              let host = preview.hostInstallationId, let conversation = preview.conversationId, let approval = preview.action,
              let record = records[host], record.enabled, isEnabled(host), record.id == registration,
              let model = model(host, device: record.device) else { throw PushActionFailure.unavailable }
        await model.check(renew: true)
        try Task.checkCancellation()
        guard let saved = model.connection, record.belongs(to: saved.credential), !saved.requiresPairing else { throw PushActionFailure.unavailable }
        struct Route: Decodable, Sendable { let conversationId: String }
        let route: Route = try await model.api.request("/api/v1/push/routes/\(routeID)", origin: saved.origin, credential: saved.credential)
        guard route.conversationId == conversation else { throw PushActionFailure.unavailable }
        let requests: [AttentionRequest] = try await model.api.request("/api/v1/approvals", origin: saved.origin, credential: saved.credential)
        try Task.checkCancellation()
        guard records[host]?.id == registration, records[host]?.enabled == true,
              model.connection?.credential.hostInstallationId == host, model.connection?.credential.deviceId == record.device,
              let request = requests.first(where: { $0.id == approval.approvalId }), request.belongs(to: conversation),
              request.actionNonce == approval.actionNonce else { throw PushActionFailure.unavailable }
        let response: String?
        var answers: [String: String] = [:]
        if let questionID = approval.questionId {
            guard decision == "respond", let message, request.isQuestion,
                  let questions = request.params.questions, questions.count == 1,
                  questions[0].id == questionID, questions[0].isOther == true,
                  questions[0].isSecret != true, questions[0].multiSelect != true else { throw PushActionFailure.unavailable }
            if let offered = approval.questionOptions {
                // The encrypted route attests the complete form at creation.
                // Stored request parameters/nonce are immutable; verify that
                // same pending request and its current display before replying.
                let prompt = [questions[0].header ?? "", questions[0].question].filter { !$0.isEmpty }.joined(separator: "\n")
                let current = (questions[0].options ?? []).map {
                    PushPreview.QuestionOption(label: $0.label, description: $0.description?.isEmpty == false ? $0.description : nil)
                }
                guard prompt == preview.body, current == offered else { throw PushActionFailure.unavailable }
                if let optionIndex {
                    guard offered.indices.contains(optionIndex), message == offered[optionIndex].label else { throw PushActionFailure.unavailable }
                }
            } else {
                guard optionIndex == nil, let question = request.params.notificationQuestion,
                      question.id == questionID, question.body == preview.body else { throw PushActionFailure.unavailable }
            }
            answers[questionID] = message
            response = nil
        } else {
            guard request.method == "item/commandExecution/requestApproval", request.params.kind == "command",
                  let command = request.params.command, let cwd = request.params.cwd,
                  [request.params.reason ?? "", command, cwd].filter({ !$0.isEmpty }).joined(separator: "\n") == preview.body,
                  let choice = request.phoneChoices.first(where: { $0.decision == decision && $0.structuredDecision == nil }) else { throw PushActionFailure.unavailable }
            if let message {
                guard request.params.supportsDenyMessage == true else { throw PushActionFailure.unavailable }
                let data = try JSONSerialization.data(withJSONObject: ["decision": "decline", "message": message], options: [.sortedKeys, .withoutEscapingSlashes])
                response = String(decoding: data, as: UTF8.self)
            } else {
                response = try request.responseJSON(for: choice, answers: [:])
            }
        }
        switch await model.resolve(request, decision: decision, answers: answers, responseJSON: response, refreshAfterResolution: false, maySend: { [weak self] in
            self?.records[host]?.id == registration && self?.records[host]?.enabled == true
                && self?.records[host]?.device == record.device
        }) {
        case .confirmed: return
        case .rejected: throw PushActionFailure.rejected
        case .unconfirmed: throw PushActionFailure.unconfirmed
        }
    }
    func receiveTap(registration: String, route: String) {
        guard UUID(uuidString: registration) != nil, UUID(uuidString: route) != nil else { return }
        guard loadSavedState() else {
            receiptIntent = .open(PushReceipt(registrationId: registration, routeId: route))
            return
        }
        pendingReceipt = PushReceipt(registrationId: registration, routeId: route)
        do { try persistReceipt() } catch { routingError = error.localizedDescription }
        resolveReceipt()
    }
    func retryOpening() {
        if pendingMacDialogHost != nil { Task { await refreshMacDialogs() } }
        else { resolveReceipt() }
    }
    /// A notification that arrived while Wonder is open: find its chat (the
    /// route lookup does not consume the route) and offer it as an in-app
    /// banner, which skips chats the live stream already alerted.
    func receiveInForeground(registration: String, route: String, body: String?, kind: String?) {
        guard loadSavedState(), UUID(uuidString: registration) != nil, UUID(uuidString: route) != nil, let library, !library.isPreview,
              let (host, record) = records.first(where: { $0.value.id == registration || $0.value.previousIDs.contains(registration) }),
              let model = model(host, device: record.device), let saved = model.connection else { return }
        Task {
            struct Route: Decodable, Sendable { let conversationId: String }
            guard let target: Route = try? await model.api.request("/api/v1/push/routes/\(route)", origin: saved.origin, credential: saved.credential) else { return }
            InAppBannerCenter.shared.receiveNotification(InAppAlertAddress(host: host, conversation: target.conversationId),
                                                         body: body, completed: kind.map { $0 == "completed" })
        }
    }
    var openingComputerNotification: Bool { pendingMacDialogHost != nil }
    var canRetryOpening: Bool { pendingReceipt != nil || pendingMacDialogHost != nil }
    func hideRoutingError() { routingError = nil }
    func dismissRoutingError() {
        if pendingMacDialogHost != nil { pendingMacDialogHost = nil; routingError = nil; return }
        guard loadSavedState() else { receiptIntent = .dismiss; return }
        routingTask?.cancel()
        let previous = pendingReceipt
        pendingReceipt = nil
        do { try persistReceipt(); routingError = nil }
        catch { pendingReceipt = previous; routingError = "Unlock this device to dismiss this notification, then try again." }
    }
    private func unavailableReceipt(_ receipt: PushReceipt, message: String) {
        guard pendingReceipt == receipt else { return }
        pendingReceipt = nil
        do { try persistReceipt(); routingError = message }
        catch { pendingReceipt = receipt; routingError = "Unlock this device to update this notification, then try again." }
    }
    private func resolveReceipt() {
        guard loadSavedState(), routingTask == nil, let receipt = pendingReceipt, let library, library.loaded, !library.isPreview else { return }
        guard let (host, record) = records.first(where: { $0.value.id == receipt.registrationId || $0.value.previousIDs.contains(receipt.registrationId) }),
              let model = model(host, device: record.device) else {
            unavailableReceipt(receipt, message: "This notification belongs to a computer that is no longer paired.")
            return
        }
        routingTask = Task { [weak self] in
            guard let self else { return }; defer { routingTask = nil }
            await model.check(renew: true)
            guard let saved = model.connection, saved.credential.deviceId == record.device else { return }
            do {
                struct Route: Decodable, Sendable { let conversationId: String }
                let route: Route = try await model.api.request("/api/v1/push/routes/\(receipt.routeId)", origin: saved.origin, credential: saved.credential)
                let detail = try await model.projects.loadDetail(route.conversationId)
                guard pendingReceipt == receipt, model.connection?.credential.deviceId == record.device,
                      detail.conversationId == route.conversationId else { throw CancellationError() }
                // A chat that waits for the owner on the computer opens its
                // screen there; the chat decides, from whether it still waits.
                model.requestComputer(for: route.conversationId)
                destination = PushDestination(host: host, conversation: route.conversationId, receipt: receipt.routeId)
                pendingReceipt = nil; try persistReceipt(); routingError = nil
            } catch {
                guard !Task.isCancelled, pendingReceipt == receipt else { return }
                if case PairingFailure.response(let code) = error, code == 404 || code == 410 {
                    unavailableReceipt(receipt, message: "This notification is no longer available.")
                } else {
                    routingError = "Connect to your computer to open this notification, then try again."
                }
            }
        }
    }
    // MARK: Live Activities

    enum LiveActivityAuthority: Equatable {
        case restoring, available, revoked
    }

    /// Launch has not yet proved a saved registration invalid just because
    /// Keychain/pairing is locked or Apple has not returned this launch's token.
    /// This grace applies only to preserving an existing activity.
    func liveActivityAuthority(host: String?, registration: String? = nil) -> LiveActivityAuthority {
        if accountEndPending { return .revoked }
        if preferenceIntent == false { return .revoked }
        if let library, library.loaded {
            let paired = library.paired.filter { !$0.requiresPairing }
            if paired.isEmpty || (host != nil && !paired.contains(where: { $0.credential.hostInstallationId == host })) { return .revoked }
        }
        guard savedStateLoaded else { return .restoring }
        let owner = host ?? registration.flatMap { id in
            records.first { $0.value.id == id || $0.value.previousIDs.contains(id) }?.key
        }
        // A confirmed off preference or invalid saved registration remains a
        // revocation even while the separate pairing snapshot is unavailable.
        guard let owner, enabled, isSignedIn, let record = records[owner],
              record.enabled, record.macRegistered, let id = record.id, !record.token.isEmpty,
              record.endpoint.trimmingCharacters(in: CharacterSet(charactersIn: "/")) == accountService() else { return .revoked }
        if let legacyRequestedDevices, legacyRequestedDevices[owner] != record.device { return .revoked }
        if let registration, registration != id && !record.previousIDs.contains(registration) { return .revoked }
        if let token, record.token != token { return .revoked }
        guard let library, library.loaded else { return .restoring }
        guard let saved = library.paired.first(where: { $0.credential.hostInstallationId == owner && !$0.requiresPairing }),
              record.belongs(to: saved.credential) else { return .revoked }
        guard let token else { return .restoring }
        return record.token == token ? .available : .revoked
    }

    /// Whether this Mac can push to a Live Activity: notifications are on and
    /// registered with it. Without that nothing could end one while Wonder is
    /// suspended, so none is started (the first version's failure).
    func canFollowLiveActivity(_ host: String) -> Bool { liveActivityAuthority(host: host) == .available }
    func registrationID(host: String) -> String? { canFollowLiveActivity(host) ? records[host]?.id : nil }
    func host(registration: String) -> String? {
        records.first { $0.value.enabled && ($0.value.id == registration || $0.value.previousIDs.contains(registration)) }?.key
    }
    struct LiveActivityReply: Decodable, Sendable {
        let registered: Bool
        /// The service already holds a dismissal for the card this token belongs to.
        let dismissed: Bool?
        let state: AgentActivityAttributes.ContentState?
    }
    /// Register (`update`, `start`) or forget (`ended`, or an empty token) an
    /// ActivityKit token, or report the card's dismissal (`dismissed`) or that
    /// Cupola opened again (`foreground`, empty token), with the push service
    /// through this Mac over the pairing.
    func liveActivityToken(host: String, kind: String, token: String) async throws -> LiveActivityReply {
        homeNetworkTrust.update(library?.paired ?? [])
        guard loadSavedState(), isEnabled(host), let record = records[host], record.enabled, let model = model(host, device: record.device),
              let saved = model.connection, record.belongs(to: saved.credential), !saved.requiresPairing,
              token.isEmpty || canFollowLiveActivity(host) else { throw PushFailure.unavailable }
        func stillOwned() -> Bool {
            isEnabled(host) && records[host]?.enabled == true && records[host]?.id == record.id
                && records[host]?.device == record.device && records[host]?.key == record.key
                && self.model(host, device: record.device)?.connection?.credential.deviceId == saved.credential.deviceId
                && (token.isEmpty || canFollowLiveActivity(host))
        }
        let body = try await signedBody(action: "push.liveActivity", path: "/api/v1/push/live-activity", values: [kind, token],
                                        fields: ["kind": kind, "token": token], saved: saved)
        try Task.checkCancellation()
        guard stillOwned() else { throw CancellationError() }
        let reply: LiveActivityReply = try await api.request("/api/v1/push/live-activity", origin: saved.origin, body: body, credential: saved.credential)
        try Task.checkCancellation()
        guard stillOwned() else { throw CancellationError() }
        return reply
    }
    /// Open from a Live Activity: the same route handling as a notification tap.
    func openLiveActivityRoute(host: String?, registration: String?, route: String) {
        guard let id = registration ?? host.flatMap({ records[$0]?.id }) else {
            routingError = "This Live Activity belongs to a computer that is no longer paired."
            return
        }
        receiveTap(registration: id, route: route)
    }

    private func signedBody(action: String, path: String, values: [String], fields: [String: String], saved: SavedConnection) async throws -> Data {
        let issued = max(UInt64(Date().timeIntervalSince1970 * 1000), lastIssued + 1), nonce = UUID().uuidString
        lastIssued = issued
        let transcript = try SignedAction.transcript(action: action, path: path, values: values, nonce: nonce, connection: saved, issuedAtMs: issued)
        return try SignedAction.body(fields: fields, nonce: nonce, issuedAtMs: issued, signature: try await signingIdentity.sign(transcript))
    }
    private func randomSecret() throws -> String {
        var bytes = Data(count: 32)
        let result = bytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        guard result == errSecSuccess else { throw SigningIdentityFailure.keychain(result) }
        return bytes.base64URL
    }
    private func requireSavedState() throws {
        guard savedStateLoaded else { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
    }
    private func persist() throws {
        try requireSavedState()
        try write(JSONEncoder().encode(records), "push-registrations-v1")
    }
    private func persistReceipt() throws {
        try requireSavedState()
        try write(try pendingReceipt.map { try JSONEncoder().encode($0) } ?? Data(), "push-receipt-v1")
    }
}

/// Something handled in Cupola that settles delivered system notifications: an
/// answered request, or a chat read up to a history position. Matching reads
/// each notification's own encrypted preview, so the computer, request and
/// chat are never guessed from a thread group or a title, and opening a chat
/// at an older reading position leaves later replies' notifications alone.
enum HandledNotification: Equatable, Sendable {
    case request(host: String, id: String, nonce: String)
    case read(host: String, conversation: String, hostEpoch: String, sequence: UInt64)

    func settles(_ preview: PushPreview) -> Bool {
        switch self {
        case let .request(host, id, nonce):
            guard let action = preview.action else { return false }
            return preview.hostInstallationId == host && action.approvalId == id && action.actionNonce == nonce
        case let .read(host, conversation, hostEpoch, sequence):
            // Only a finished reply carries its position; a request stays
            // until it is answered, however far the chat is read.
            guard preview.action == nil, let position = preview.readThrough else { return false }
            return preview.hostInstallationId == host && preview.conversationId == conversation
                && position.hostEpoch == hostEpoch && position.sequence <= sequence
        }
    }

    /// The delivered notifications this settles. One that cannot be read is kept.
    static func identifiers(settledBy handled: Self, in delivered: [(id: String, payload: [String: String])],
                            key: (String) throws -> Data?) -> [String] {
        var keys: [String: Data?] = [:]
        return delivered.compactMap { item in
            guard let registration = item.payload["registrationId"] else { return nil }
            if keys[registration] == nil { keys[registration] = .some(try? key(registration)) }
            guard let secret = keys[registration] ?? nil,
                  let preview = try? PushPreview.decrypt(item.payload, key: secret),
                  handled.settles(preview) else { return nil }
            return item.id
        }
    }

    static func clear(_ handled: Self, key: @escaping @Sendable (String) throws -> Data? = { try PushPreviewKeys.read($0) }) {
        Task.detached(priority: .utility) {
            let center = UNUserNotificationCenter.current()
            let delivered = await center.deliveredNotifications().compactMap { notification -> (id: String, payload: [String: String])? in
                guard let payload = notification.request.content.userInfo["wonderPush"] as? [String: String] else { return nil }
                return (notification.request.identifier, payload)
            }
            let ids = identifiers(settledBy: handled, in: delivered, key: key)
            if !ids.isEmpty { center.removeDeliveredNotifications(withIdentifiers: ids) }
        }
    }
}

/// A "Waiting for a permission on your Mac" notification is only true while the
/// dialog is open. Once the Mac reports it closed, remove the delivered ones.
enum MacPermissionNotifications {
    static func clear(host: String, conversations: Set<String>) {
        let groups = Set(conversations.map {
            PushPreview(title: "", body: "", hostInstallationId: host, conversationId: $0).threadIdentifier
        })
        Task {
            let center = UNUserNotificationCenter.current()
            let ids = await center.deliveredNotifications().filter {
                groups.contains($0.request.content.threadIdentifier)
                    && $0.request.content.title.hasSuffix(ProjectThreadSummary.macPermissionWait)
            }.map(\.request.identifier)
            if !ids.isEmpty { center.removeDeliveredNotifications(withIdentifiers: ids) }
        }
    }
}

final class PushAppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        UNUserNotificationCenter.current().setNotificationCategories(Self.approvalCategories)
        return true
    }
    static var approvalCategories: Set<UNNotificationCategory> {
        let actions = [
            UNNotificationAction(identifier: PushPreview.allowAction, title: "Allow once", options: [.authenticationRequired]),
            UNNotificationAction(identifier: PushPreview.denyAction, title: "Deny", options: [.authenticationRequired])
        ]
        let feedback = UNTextInputNotificationAction(identifier: PushPreview.denyMessageAction, title: "Deny with instruction",
            options: [.authenticationRequired], textInputButtonTitle: "Deny", textInputPlaceholder: "Tell Claude what to do instead")
        let reply = PushPreview.otherAnswerAction
        return [
            UNNotificationCategory(identifier: PushPreview.commandCategory, actions: actions, intentIdentifiers: []),
            UNNotificationCategory(identifier: PushPreview.commandFeedbackCategory, actions: actions + [feedback], intentIdentifiers: []),
            UNNotificationCategory(identifier: PushPreview.questionCategory, actions: [reply], intentIdentifiers: [])
        ]
    }
    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        Task { @MainActor in PushNotifications.shared.receivedToken(deviceToken) }
    }
    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        Task { @MainActor in PushNotifications.shared.registrationFailed(error) }
    }
    func application(_ application: UIApplication, didReceiveRemoteNotification userInfo: [AnyHashable: Any], fetchCompletionHandler completionHandler: @escaping (UIBackgroundFetchResult) -> Void) {
        guard let value = userInfo["wonderPush"] as? [String: String], let id = value["challengeId"], let challenge = value["challenge"], let nonce = value["nonce"] else { completionHandler(.noData); return }
        Task { @MainActor in
            await PushNotifications.shared.receiveChallenge(id: id, challenge: challenge, nonce: nonce)
            completionHandler(.newData)
        }
    }
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        if let value = response.notification.request.content.userInfo["wonderMacDialogs"] as? [String: String],
           let host = value["host"], response.actionIdentifier == UNNotificationDefaultActionIdentifier {
            await PushNotifications.shared.receiveMacDialogTap(host: host)
            return
        }
        guard let value = response.notification.request.content.userInfo["wonderPush"] as? [String: String], let registration = value["registrationId"], let route = value["routeId"] else { return }
        switch response.actionIdentifier {
        case PushPreview.allowAction: await PushNotifications.shared.receiveAction(value, decision: "accept")
        case PushPreview.denyAction: await PushNotifications.shared.receiveAction(value, decision: "decline")
        case PushPreview.denyMessageAction:
            guard let response = response as? UNTextInputNotificationResponse else { return }
            await PushNotifications.shared.receiveAction(value, decision: "decline", message: response.userText)
        case PushPreview.replyAction:
            guard let response = response as? UNTextInputNotificationResponse else { return }
            await PushNotifications.shared.receiveAction(value, decision: "respond", message: response.userText)
        case UNNotificationDefaultActionIdentifier:
            if !(await PushNotifications.shared.receiveForwardedSelection(value)) {
                await MainActor.run { PushNotifications.shared.receiveTap(registration: registration, route: route) }
            }
        default:
            if let index = PushPreview.optionIndex(response.actionIdentifier) {
                await PushNotifications.shared.receiveAction(value, decision: "respond", optionIndex: index)
            }
        }
    }
    /// While Wonder is open, notifications never show as system banners. One
    /// for a chat the live stream does not cover (a thread Claude Code or the
    /// Codex app runs on the Mac) shows as an in-app banner instead.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        let content = notification.request.content
        if content.userInfo["wonderMacDialogs"] != nil {
            await PushNotifications.shared.refreshMacDialogs()
            return [.list]
        }
        guard let value = content.userInfo["wonderPush"] as? [String: String], let registration = value["registrationId"], let route = value["routeId"] else { return [] }
        let body = content.body.isEmpty ? nil : content.body, kind = value["kind"]
        await MainActor.run { PushNotifications.shared.receiveInForeground(registration: registration, route: route, body: body, kind: kind) }
        return []
    }
}
