import Foundation
import SwiftUI
import Combine
import CryptoKit
import Security
import WonderPairing

@MainActor final class ConnectionLibrary: ObservableObject {
    /// The computers the app shows: the paired ones, or only the demo computer
    /// while the demo runs. Only `commit` changes it, and it never stores the demo.
    @Published private(set) var saved = SavedConnections()
    /// The demo computer's model while the demo runs (`startDemo()`).
    @Published private(set) var demo: ConnectionModel?
    var isDemo: Bool { demo != nil }
    /// Paired computers only, never the demo. Notifications, presence, Live
    /// Activities, widgets and Diagnostics read these, so the demo cannot reach them.
    var paired: [SavedConnection] { saved.connections.filter { !DemoMode.isDemo($0) } }
    @Published var error: String?
    /// Why the saved computers couldn't be opened; nil once they load.
    @Published private(set) var savedComputersFailure: SavedComputersFailure?
    /// The last Chats filter choice could not be kept; cleared by the next
    /// one that is. Kept apart from `error` so it never reads as a lost pairing.
    @Published private(set) var filterFailure: String?
    @Published private(set) var loaded = false
    @Published private(set) var foregroundOwner: UUID?
    /// The window that takes navigation requests (Shortcuts, voice launches):
    /// an active one, preferring the transport owner. An inactive owner keeps
    /// transport but must not open what the window in front was asked for.
    @Published private(set) var navigationScene: UUID?
    @Published var shortcutNavigation: ShortcutNavigation?
    /// Scenes on screen (active or inactive) and, of those, the ones in front.
    private var visibleScenes: Set<UUID> = []
    private var activeScenes: Set<UUID> = []
    /// Where saved computers are kept: the pairing Keychain item.
    struct Storage {
        var read: (String) throws -> Data?
        var save: (Data, String) throws -> Void
        var forgetLegacy: () throws -> Void
        static var keychain: Storage { Storage(read: { try PhoneIdentity().read($0) },
                                      save: { try PhoneIdentity().save($0, account: $1) },
                                      forgetLegacy: { try PhoneIdentity().forgetConnection() }) }
    }
    private let identity: Storage
    private var models: [String: ConnectionModel] = [:]
    private var leases: [String: UUID] = [:]
    private var observations: [String: AnyCancellable] = [:]
    private var widgetPublishTask: Task<Void, Never>?
    private var widgetPublishNeedsRefresh = false
    private var widgetPublisher: (ConnectionLibrary, Bool) -> Bool = {
        ProjectWidgetSnapshotPublisher.publish(from: $0, refreshIfUnchanged: $1)
    }
    private(set) var isPreview = false
    /// Diagnostics tests only; nil gives each model its own client.
    private var modelAPI: PairingAPI?

    #if WONDER_DIAGNOSTICS
    /// A library over other storage than the Keychain, loaded at once. `api`
    /// routes every model it opens through a synthetic transport.
    init(storage: Storage, widgetPublisher: ((ConnectionLibrary, Bool) -> Bool)? = nil, api: PairingAPI? = nil) {
        identity = storage
        modelAPI = api
        if let widgetPublisher { self.widgetPublisher = widgetPublisher }
        load()
    }

    init(diagnosticModels: [ConnectionModel]) {
        identity = .keychain
        loaded = true
        isPreview = true
        for model in diagnosticModels {
            guard let connection = model.connection else { continue }
            saved.save(connection)
            let host = connection.credential.hostInstallationId
            models[host] = model
            observations[host] = model.$listRevision.dropFirst().sink { [weak self] _ in self?.objectWillChange.send() }
        }
    }
    #endif

    init() {
        identity = .keychain
        #if WONDER_DIAGNOSTICS
        // First launch without touching the Keychain: no saved computers, whatever an earlier run paired.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-unpaired") {
            loaded = true
            return
        }
        #endif
        #if DEBUG || WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-read-preview") && !ProcessInfo.processInfo.arguments.contains("-connections-preview") {
            loaded = true
            return
        }
        if ProcessInfo.processInfo.arguments.contains("-connections-preview") {
            isPreview = true
            loaded = true
            for (host, name) in [("studio", "Studio"), ("macbook", "Laptop"), ("mini", "Home")] {
                let value: [String: Any] = ["origin": "https://\(host).invalid", "hostName": name,
                    "credential": ["sessionToken": "synthetic", "csrfToken": "synthetic", "deviceId": "preview-device", "hostInstallationId": host]]
                if let data = try? JSONSerialization.data(withJSONObject: value), var connection = try? JSONDecoder().decode(SavedConnection.self, from: data) {
                    #if WONDER_DIAGNOSTICS
                    let arguments = ProcessInfo.processInfo.arguments
                    let denied = arguments.contains("-diagnostics-home-network-denied")
                    if denied || arguments.contains("-diagnostics-home-network") {
                        let home = "https://192.168.1.20:3778"
                        connection = (try? SavedConnection(routes: [ConnectionRoute(kind: .home, origin: home),
                            ConnectionRoute(kind: .tailscale, origin: connection.origin)], origin: denied ? connection.origin : home,
                            credential: connection.credential, hostName: connection.hostName,
                            hostKeyPin: Data(repeating: 1, count: 32).base64URL)) ?? connection
                    }
                    #endif
                    saved.save(connection)
                }
            }
            return
        }
        #endif
        load()
        // The widget follows the Mac last used for a new chat.
        lastHostObservation = NotificationCenter.default.publisher(for: NewChatDraftStore.lastHostChanged)
            .sink { [weak self] _ in self?.scheduleWidgetSnapshot() }
    }
    private var lastHostObservation: AnyCancellable?

    deinit { widgetPublishTask?.cancel() }

    func load() {
        guard !loaded else { return }
        do {
            if let data = try identity.read("connections-v1") {
                saved = try JSONDecoder().decode(SavedConnections.self, from: data)
            } else {
                let legacy = try identity.read("connection").map { try JSONDecoder().decode(SavedConnection.self, from: $0) }
                let migrated = SavedConnections(legacy: legacy)
                try identity.save(JSONEncoder().encode(migrated), "connections-v1")
                saved = migrated
            }
            // The collection is authoritative before deleting the legacy value.
            try? identity.forgetLegacy()
            loaded = true
            error = nil
            savedComputersFailure = nil
            publishWidgetSnapshotNow()
        } catch {
            let failure = SavedComputersFailure(error)
            savedComputersFailure = failure
            self.error = failure.message
        }
    }

    /// Opens the saved computers again when retrying can help: on returning to
    /// the foreground after a locked launch, or after a passing storage fault.
    func retrySavedComputers() {
        guard !loaded, let failure = savedComputersFailure, failure != .unreadable else { return }
        load()
    }

    /// Replaces saved computers this version can't read with none, so the
    /// owner can pair again. Only offered when retrying cannot help.
    func removeUnreadableComputers() {
        guard !loaded, savedComputersFailure == .unreadable else { return }
        do {
            try identity.save(JSONEncoder().encode(SavedConnections()), "connections-v1")
            try? identity.forgetLegacy()
            savedComputersFailure = nil
            error = nil
            loaded = true
            publishWidgetSnapshotNow()
        } catch { self.error = SavedComputersFailure(error).message }
    }

    func model(for connection: SavedConnection) -> ConnectionModel {
        let host = connection.credential.hostInstallationId
        if let existing = models[host] { return existing }
        // Only `startDemo` creates the demo computer's model.
        if DemoMode.isDemo(connection), let demo { return demo }
        let device = connection.credential.deviceId
        let lease = UUID()
        leases[host] = lease
        let model = ConnectionModel(saved: connection, persistConnection: { [weak self] updated in
            guard let self, self.leases[host] == lease, self.saved.connections.contains(where: {
                $0.credential.hostInstallationId == host && $0.credential.deviceId == device
            }) else { throw PairingFailure.wrongHost }
            var next = self.saved
            if let updated {
                guard updated.credential.hostInstallationId == host, updated.credential.deviceId == device else { throw PairingFailure.wrongHost }
                guard !self.saved.connections.contains(where: { $0.credential.hostInstallationId == host && $0.requiresPairing && !updated.requiresPairing }) else {
                    throw SigningIdentityFailure.invalidated
                }
                next.save(updated)
            } else { next.remove(host) }
            try self.commit(next)
            if updated == nil { self.models.removeValue(forKey: host); self.leases.removeValue(forKey: host); self.observations.removeValue(forKey: host) }
        }, api: modelAPI)
        model.identityNeedsRepair = { [weak self] in
            guard let self else { throw CancellationError() }
            try self.requirePairing()
        }
        if isPreview { model.connection = connection; model.macConnected = true; model.status = "Connected to your computer." }
        models[host] = model
        // Views reading a model's own state observe it directly. The library
        // re-renders the list and root only for list-visible changes.
        observations[host] = model.$listRevision.dropFirst().sink { [weak self] _ in
            self?.objectWillChange.send()
            self?.scheduleWidgetSnapshot()
        }
        model.projects.widgetSnapshotChanged = { [weak self] in self?.scheduleWidgetSnapshot(refresh: true) }
        return model
    }

    func pairingModel() -> ConnectionModel {
        let model = ConnectionModel { [weak self] connection in
            guard let self, let connection else { throw PairingFailure.missingIdentity }
            try self.savePairing(connection)
        }
        model.identityNeedsRepair = { [weak self] in
            guard let self, self.loaded else { throw SigningIdentityFailure.missing }
            try self.requirePairing()
        }
        model.previousPairingConnection = { [weak self] host in
            self?.saved.connections.first { $0.credential.hostInstallationId == host }
        }
        return model
    }

    /// Saves a computer just paired. Pairing a real Mac ends the demo.
    func savePairing(_ connection: SavedConnection) throws {
        guard loaded, !DemoMode.isDemo(connection) else { throw PairingFailure.missingIdentity }
        var next = saved
        let host = connection.credential.hostInstallationId
        next.save(connection)
        try commit(next)
        models[host]?.retireAfterPairingReplacement()
        models.removeValue(forKey: host)
        leases.removeValue(forKey: host)
        observations.removeValue(forKey: host)
    }

    // MARK: Demo

    /// The entry point for "Try a demo" (the empty Add a computer screen, the
    /// welcome pages): shows a local demo computer with Projects and threads. It
    /// is offered only while no computer is paired, and returns whether the demo
    /// is running. Nothing is sent anywhere; see `DemoMode`.
    @discardableResult func startDemo() -> Bool {
        guard loaded, paired.isEmpty else { return false }
        if demo != nil { return true }
        DemoMode.removeLeftovers()
        let model = DemoMode.makeModel(storeRoot: DemoMode.newStoreRoot())
        let connection = DemoMode.connection()
        demo = model
        models[DemoMode.hostID] = model
        observations[DemoMode.hostID] = model.$listRevision.dropFirst().sink { [weak self] _ in self?.objectWillChange.send() }
        var next = saved
        next.save(connection)
        // Shown, never stored: the demo is not a pairing.
        saved = next
        return true
    }

    /// Leaves the demo: the demo computer, its chats, drafts and cached Projects
    /// (including its temporary folder) are removed and the app returns to Add a computer.
    func leaveDemo() async {
        guard let model = endDemo() else { return }
        await model.forget()
        DemoMode.removeLeftovers()
    }

    /// Takes the demo computer out of the app now; returns its model for cleanup.
    @discardableResult private func endDemo() -> ConnectionModel? {
        guard let model = demo else { return nil }
        model.setForeground(false)
        demo = nil
        models.removeValue(forKey: DemoMode.hostID)
        observations.removeValue(forKey: DemoMode.hostID)
        var next = saved
        next.remove(DemoMode.hostID)
        saved = next
        return model
    }

    private func requirePairing() throws {
        var next = saved
        next.requirePairing()
        // Immediately fence callbacks, even if durable storage is unavailable.
        for model in models.values { model.stopForIdentityRecovery() }
        try commit(next)
    }

    func filter(_ host: String?) {
        do {
            var next = saved
            if let host { try next.select(host) } else { next.showAll() }
            try commit(next)
            if filterFailure != nil { filterFailure = nil }
        } catch { filterFailure = "Your chat filter could not be saved. Try again." }
    }

    func suspendAll() {
        for model in models.values { model.setForeground(false) }
        ProjectCatalogWriter.shared.flushBeforeSuspension()
        // Flush the current value before suspension instead of leaving its
        // publication dependent on a sleeping foreground task.
        publishWidgetSnapshotNow()
    }

    /// Exactly one visible scene runs shared transport/catalog maintenance;
    /// `phase` is nil once the scene disappears. A brief system overlay
    /// (Control Center, Notification Center, the app switcher) leaves scenes
    /// inactive, not background: the owner keeps every connection, and with no
    /// scene in front the computers only save typed drafts and stop dictation.
    /// Connections suspend once no scene is visible. Closing another iPad
    /// window cannot suspend the surviving window.
    func setScene(_ scene: UUID, phase: ScenePhase?) {
        let wasInFront = !activeScenes.isEmpty
        if phase == .active { activeScenes.insert(scene); retrySavedComputers() } else { activeScenes.remove(scene) }
        if let phase, phase != .background { visibleScenes.insert(scene) } else { visibleScenes.remove(scene) }
        if wasInFront != !activeScenes.isEmpty {
            for model in models.values { model.setInFront(!activeScenes.isEmpty) }
        }
        defer {
            let active = foregroundOwner.flatMap { activeScenes.contains($0) ? $0 : nil }
                ?? activeScenes.sorted { $0.uuidString < $1.uuidString }.first ?? foregroundOwner
            if navigationScene != active { navigationScene = active }
        }
        if let foregroundOwner, visibleScenes.contains(foregroundOwner) { return }
        foregroundOwner = visibleScenes.sorted { $0.uuidString < $1.uuidString }.first
        if foregroundOwner == nil { suspendAll() }
    }

    private func commit(_ next: SavedConnections) throws {
        var stored = next
        stored.remove(DemoMode.hostID)
        // While the demo runs nothing real changes, so nothing is stored; a
        // real computer joining (a pairing) ends the demo first.
        let demoOnly = demo != nil && stored.connections.isEmpty
        if !demoOnly {
            if !isPreview { try identity.save(JSONEncoder().encode(stored), "connections-v1") }
            if let model = endDemo() {
                Task { await model.forget(); DemoMode.removeLeftovers() }
            }
        }
        saved = demoOnly ? next : stored
        publishWidgetSnapshotNow()
    }

    @discardableResult func publishWidgetSnapshotNow() -> Bool {
        widgetPublishTask?.cancel()
        widgetPublishTask = nil
        let refresh = widgetPublishNeedsRefresh
        widgetPublishNeedsRefresh = false
        return widgetPublisher(self, refresh)
    }

    private func scheduleWidgetSnapshot(refresh: Bool = false) {
        widgetPublishNeedsRefresh = widgetPublishNeedsRefresh || refresh
        // Keep the first deadline. Continuing list publications must not
        // starve a changed catalog snapshot until the stream becomes quiet.
        guard widgetPublishTask == nil else { return }
        widgetPublishTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            self?.publishWidgetSnapshotNow()
        }
    }
}
