import ActivityKit
import Combine
import Foundation
import UIKit
import WonderPairing

/// Starts the phone's one Live Activity when a paired Mac's agents work, and
/// hands its push token to the push service through that Mac. From then on the
/// service owns what the card shows: every computer reports its own chats, the
/// service merges them into one state and pushes it, including the end
/// (`services/push/src/activity.ts`), so the card stays complete and true with
/// Wonder suspended. The phone never builds rows itself, and never ends an
/// activity from what it last read of a chat: the first version did, and showed
/// stopped threads (#93, removed in #101).
///
/// The phone ends an activity itself only when the service can no longer reach
/// it: Live Activities or notifications were turned off, the Mac was unpaired, or
/// the activity's token was never accepted.
///
/// An activity the owner swipes off the Lock Screen while it still shows work
/// stays gone: the phone tells the service (`dismissed`), which stops updates and
/// push-to-start, and starts no card itself, until the owner opens Cupola
/// again (`setForeground`). A card the system ended (its 8 hour limit) counts the
/// same. The phone's own ends are not dismissals.
@MainActor final class AgentLiveActivities: ObservableObject {
    static let shared = AgentLiveActivities()
    typealias Content = AgentActivityAttributes.ContentState

    static let preferenceKey = "live-activities-enabled-v1"
    @Published private(set) var enabled: Bool
    private var preferenceLoaded: Bool
    /// Legacy migration input only; enrollment no longer stores host preferences.
    static let disabledKey = "live-activities-disabled-hosts-v1"
    /// Hosts whose Mac still holds a push-to-start token the phone must forget.
    static let pendingRemovalKey = "live-activities-pending-removal-v1"
    /// Whether the owner dismissed the card, and whether the service knows.
    static let dismissalKey = "live-activity-dismissal-v1"
    /// Matches the service's `STALE` (`activity.ts`): a missed push reads "Open Cupola for the latest".
    nonisolated static let staleAfter: TimeInterval = 10 * 60
    /// A finished card stays this long (the service's end push says the same: a finished chat stays 15 minutes).
    nonisolated static let finishedLinger: TimeInterval = 15 * 60
    /// An activity whose token the Mac has not accepted by then cannot be kept true.
    static let registrationDeadline: TimeInterval = 60

    /// Value snapshots keep ActivityKit objects out of the coordinator's state.
    struct OwnedActivity {
        let id: String
        let attributes: AgentActivityAttributes
        var isActive = true
        /// Gone from the Lock Screen while it still showed work: the owner
        /// swiped it away, or the system ended it (its 8 hour limit). Ends the
        /// phone or the service asked for show a finished state first.
        var dismissedWhileWorking = false
        /// The phase on screen, so a finished state can be checked against the
        /// work this phone can see (owner feedback 33).
        var phase: AgentActivityAttributes.Phase?
        var content: Content?
    }
    struct ActivityClient {
        var activities: () -> [OwnedActivity]
        var request: (AgentActivityAttributes, Content, Date) throws -> String
        var end: (String, ActivityUIDismissalPolicy) -> Void
        var apply: (String, Content) async -> Void = { await AgentLiveActivities.apply($1, to: $0) }
        var allows: () -> Bool = { ActivityAuthorizationInfo().areActivitiesEnabled }
        var observe = true
        static var live: Self {
            Self(activities: {
                Activity<AgentActivityAttributes>.activities.map {
                    OwnedActivity(id: $0.id, attributes: $0.attributes, isActive: $0.activityState == .active || $0.activityState == .stale,
                                  dismissedWhileWorking: ($0.activityState == .dismissed || $0.activityState == .ended) && !$0.content.state.phase.isFinished,
                                  phase: $0.content.state.phase, content: $0.content.state)
                }
            }, request: { attributes, content, date in
                try Activity.request(attributes: attributes, content: ActivityContent(state: content, staleDate: date), pushType: .token).id
            }, end: { id, dismissal in Task { await AgentLiveActivities.end(id, content: nil, dismissal: dismissal) } })
        }
    }

    private weak var library: ConnectionLibrary?
    private let defaults: UserDefaults
    private let push: PushNotifications
    private let activityClient: ActivityClient
    private let now: () -> Date
    private var scanTask: Task<Void, Never>?
    private var isForeground = false
    private var workScanTask: Task<Void, Never>?
    private var workWatches: [String: (model: ConnectionModel, observations: [AnyCancellable])] = [:]
    private var watchers: [String: Task<Void, Never>] = [:]
    /// Activities whose current token this phone's Mac accepted.
    private var registered: Set<String> = []
    /// When this launch began following each activity, and its last token.
    private var firstSeen: [String: Date] = [:]
    private var tokens: [String: String] = [:]
    /// The host of each followed activity, kept until its first observed end
    /// so a later duplicate end cannot be read as a dismissal.
    private var followedHosts: [String: String] = [:]
    private var dismissalSync: Task<Void, Never>?
    private var restoring: Set<String> = []
    private var registering: [String: UUID] = [:]
    private var ending: Set<String> = []
    /// Finished activities being corrected to running, one apply at a time.
    private var correcting: Set<String> = []
    /// One retry batch per token and actual connection/setup recovery,
    /// rather than per scan, healthy read or duplicate ActivityKit token.
    private var attemptedTokens: [String: String] = [:]
    private var tokenTasks: [String: (owner: UUID, task: Task<Void, Never>)] = [:]
    /// A timed-out activity ends once. Cached work cannot start its replacement
    /// until this Mac reconnects, setup recovers, or the owner re-enables it.
    private var waitingForConnection: Set<String> = []
    private var connectionWatches: [String: (model: ConnectionModel, observation: AnyCancellable)] = [:]
    private var startTokenTask: Task<Void, Never>?
    private var newActivityTask: Task<Void, Never>?
    private var startToken: String?
    private var startRegistrations: [String: (device: String, registration: String, token: String)] = [:]
    private var startTasks: [String: (owner: UUID, task: Task<Void, Never>)] = [:]
    private var removalTasks: [String: (owner: UUID, task: Task<Void, Never>)] = [:]

    init(defaults: UserDefaults = .standard, push: PushNotifications = .shared,
         activityClient: ActivityClient = .live, now: @escaping () -> Date = Date.init) {
        self.defaults = defaults; self.push = push; self.activityClient = activityClient; self.now = now
        preferenceLoaded = defaults.object(forKey: Self.preferenceKey) != nil
        enabled = preferenceLoaded ? defaults.bool(forKey: Self.preferenceKey) : true
    }

    deinit {
        scanTask?.cancel(); workScanTask?.cancel(); startTokenTask?.cancel(); newActivityTask?.cancel(); dismissalSync?.cancel()
        for watcher in watchers.values { watcher.cancel() }
        for entry in tokenTasks.values { entry.task.cancel() }
        for entry in startTasks.values { entry.task.cancel() }
        for entry in removalTasks.values { entry.task.cancel() }
    }

    // MARK: Settings

    func isEnabled(_ host: String) -> Bool {
        preferenceLoaded ? enabled : !(defaults.stringArray(forKey: Self.disabledKey) ?? []).contains(host)
    }
    var systemAllows: Bool { activityClient.allows() }

    func setEnabled(_ enabled: Bool, host: String) {
        setAll(enabled: enabled)
    }

    /// The library argument lets Settings use this before the lifecycle attach.
    func setAll(enabled: Bool, library: ConnectionLibrary? = nil) {
        if let library { self.library = library }
        let hosts = Set(self.library?.paired.map { $0.credential.hostInstallationId } ?? [])
            .union(activityClient.activities().compactMap { host(of: $0.attributes) })
            .union(defaults.stringArray(forKey: Self.pendingRemovalKey) ?? [])
            .union(startTasks.keys).union(startRegistrations.keys)
        defaults.set(enabled, forKey: Self.preferenceKey)
        defaults.removeObject(forKey: Self.disabledKey)
        preferenceLoaded = true
        self.enabled = enabled
        waitingForConnection.removeAll()
        // Live Activities register the phone themselves: no notifications needed.
        push.liveActivitiesChanged(library: self.library)
        if enabled {
            // Turning them back on is a fresh start: the service forgets any dismissal.
            if dismissal.dismissed { dismissal = Dismissal(dismissed: false, told: false) }
            for entry in removalTasks.values { entry.task.cancel() }
            scan()
        } else {
            for activity in activityClient.activities() where activity.isActive { end(activity.id, dismissal: .immediate) }
            startRegistrations.removeAll()
            for host in hosts {
                startTasks[host]?.task.cancel()
                markPendingRemoval(host, true)
                forgetStartToken(host)
            }
        }
    }

    private func migratePreference() {
        guard !preferenceLoaded, let library, library.loaded, !library.isPreview else { return }
        #if WONDER_DIAGNOSTICS
        // The capture fixture has no paired owner; preserve it just as the
        // foreground scan does until its explicit fixture lifecycle ends it.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-live-activity-fixture") { return }
        #endif
        let disabled = Set(defaults.stringArray(forKey: Self.disabledKey) ?? [])
        let hosts = library.paired.map { $0.credential.hostInstallationId }
        // Legacy Live Activities defaulted on for each host. Any current On
        // wins; an empty pairing library has no enabled computer to migrate.
        setAll(enabled: hosts.contains { !disabled.contains($0) })
    }

    // MARK: Lifecycle

    func attach(_ library: ConnectionLibrary) {
        self.library = library
        guard !library.isPreview else { return }
        migratePreference()
        for activity in activityClient.activities() { watch(activity.id) }
        guard activityClient.observe else { return }
        // Includes activities the Mac push-started while Wonder was suspended.
        newActivityTask = newActivityTask ?? Task { [weak self] in
            await Self.followNewActivities { [weak self] id in self?.watch(id) }
        }
        startTokenTask = startTokenTask ?? Task { [weak self] in
            await Self.followStartTokens { [weak self] token in self?.receivedStartToken(token) }
        }
        setForeground(UIApplication.shared.applicationState == .active)
    }

    /// Observe loaded work while Wonder is in front. The periodic scan remains
    /// the backstop for registration deadlines and setup recovery; the Mac
    /// keeps accepted activities current when Wonder leaves the foreground.
    func setForeground(_ active: Bool) {
        let wasForeground = isForeground
        scanTask?.cancel(); scanTask = nil
        workScanTask?.cancel(); workScanTask = nil
        workWatches.removeAll()
        isForeground = false
        guard active, library?.isPreview == false else { return }
        #if WONDER_DIAGNOSTICS
        // A capture fixture's activity belongs to no paired Mac; keep it.
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-live-activity-fixture") { return }
        #endif
        isForeground = true
        // Opening Cupola again lifts a dismissal. The service is told whether or not the
        // card was swiped: it may have found the card gone on its own while Wonder was
        // suspended. Not when Live Activities are off for every computer: there is no
        // card to lift anything from, and the service keeps nothing for that phone.
        if !wasForeground, connections().contains(where: { isEnabled($0.host) }) { reopened() }
        retryPendingRemovals()
        scanTask = Task { [weak self] in
            while !Task.isCancelled {
                self?.scan()
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    func receivedStartToken(_ token: String) {
        startToken = token
        for host in followableHosts() where isEnabled(host) { registerStartToken(token, host: host) }
    }

    private func connections() -> [(host: String, model: ConnectionModel)] {
        guard let library, library.loaded else { return [] }
        return library.paired.filter { !$0.requiresPairing }.map { ($0.credential.hostInstallationId, library.model(for: $0)) }
    }

    private func followableHosts() -> [String] {
        connections().map(\.host).filter { push.canFollowLiveActivity($0) }
    }

    /// Whether the phone sees agent work on this Mac: a reply it sent, a
    /// running thread or one an app on the Mac runs.
    static func hasWork(_ model: ConnectionModel) -> Bool {
        !work(model).isEmpty
    }

    func scan() {
        migratePreference()
        let now = now()
        let snapshots = activityClient.activities()
        let activeIDs = Set(snapshots.filter(\.isActive).map(\.id))
        let followedIDs = Set(firstSeen.keys).union(watchers.keys).union(tokenTasks.keys)
        for id in followedIDs.subtracting(activeIDs) {
            ended(id, dismissedByOwner: snapshots.first { $0.id == id }?.dismissedWhileWorking == true)
            firstSeen[id] = nil
            watchers.removeValue(forKey: id)?.cancel()
        }
        let connections = connections()
        followConnections(connections)
        syncDismissal()
        if let startToken {
            for host in followableHosts() where isEnabled(host) { registerStartToken(startToken, host: host) }
        }
        retryPendingRemovals()
        ending.formIntersection(Set(snapshots.map(\.id)))
        let dismissed = isDismissed
        // An ended activity still lingering as Done cannot be updated; take it
        // down so it never claims Done while this phone sees work running. The
        // start loop below then requests a Running activity in this same scan,
        // because an ended activity is not active for its computer.
        for activity in snapshots where !activity.isActive && !ending.contains(activity.id) && activity.phase?.isFinished == true {
            if let host = host(of: activity.attributes), !truthful(Content(phase: .done, running: 0, needsInput: 0), host: host).phase.isFinished {
                end(activity.id, dismissal: .immediate)
            }
        }
        for activity in snapshots where activity.isActive && !ending.contains(activity.id) {
            let host = host(of: activity.attributes)
            if activity.phase?.isFinished == true, let host, !correcting.contains(activity.id) {
                let shown = truthful(Content(phase: .done, running: 0, needsInput: 0), host: host)
                if !shown.phase.isFinished {
                    correcting.insert(activity.id)
                    let id = activity.id
                    Task { await activityClient.apply(id, shown); correcting.remove(id) }
                }
            }
            // A push-start that raced the dismissal cannot bring the card back.
            if let host, !isEnabled(host) || dismissed { end(activity.id, dismissal: .immediate); continue }
            switch push.liveActivityAuthority(host: activity.attributes.hostID, registration: activity.attributes.registrationId) {
            case .restoring:
                restoring.insert(activity.id)
                continue
            case .revoked:
                end(activity.id, dismissal: .immediate)
                continue
            case .available:
                // The registration window starts when setup is available,
                // not while the phone is waiting for Keychain or APNs.
                if restoring.remove(activity.id) != nil {
                    firstSeen[activity.id] = now
                    attemptedTokens[activity.id] = nil
                    if let token = tokens[activity.id] {
                        retryUpdateToken(token, activityID: activity.id)
                    }
                }
            }
            let unregistered = !registered.contains(activity.id)
                && now.timeIntervalSince(firstSeen[activity.id] ?? now) > Self.registrationDeadline
            if unregistered {
                if let host { waitingForConnection.insert(host) }
                end(activity.id, dismissal: .immediate)
            }
        }
        endDuplicateActivities(snapshots)
        guard systemAllows, canStartCard else { return }
        for (host, model) in connections where isEnabled(host) && Self.hasWork(model) && push.canFollowLiveActivity(host)
            && !waitingForConnection.contains(host) {
            // One activity covers every computer.
            guard !snapshots.contains(where: { $0.isActive && !ending.contains($0.id) }) else { continue }
            start(host: host, model: model, now: now)
        }
    }

    // MARK: One activity for every computer

    /// The phone shows one activity. A second one (another Mac's push-start, or
    /// a start that raced a push-start) goes away; the one the Mac accepted wins.
    private func endDuplicateActivities(_ snapshots: [OwnedActivity]) {
        let live = snapshots.filter { $0.isActive && !ending.contains($0.id) }
        guard live.count > 1 else { return }
        let keep = live.first { registered.contains($0.id) } ?? live[0]
        for extra in live where extra.id != keep.id { end(extra.id, dismissal: .immediate) }
    }

    // MARK: Dismissal

    private struct Dismissal: Codable, Equatable {
        /// The owner swiped the card away (or the system ended it) and has not
        /// opened Cupola since.
        var dismissed = false
        /// The service knows. Until it does, no card starts: it would refuse the token.
        var told = true
    }

    private var dismissal: Dismissal {
        get { defaults.data(forKey: Self.dismissalKey).flatMap { try? JSONDecoder().decode(Dismissal.self, from: $0) } ?? Dismissal() }
        set {
            if newValue == Dismissal() { defaults.removeObject(forKey: Self.dismissalKey) }
            else if let data = try? JSONEncoder().encode(newValue) { defaults.set(data, forKey: Self.dismissalKey) }
        }
    }

    var isDismissed: Bool { dismissal.dismissed }
    private var canStartCard: Bool { let state = dismissal; return !state.dismissed && state.told }

    /// A card is only ever swiped away while Cupola is not in front, but iOS
    /// reports it when the app resumes. Seen while in front, the owner is
    /// already back: the dismissal is lifted at once.
    private func markDismissed() {
        dismissal = Dismissal(dismissed: !isForeground, told: false)
        syncDismissal()
    }

    private func reopened() {
        dismissal = Dismissal(dismissed: false, told: false)
        syncDismissal()
    }

    /// Tells the service what the owner did with the card, through any paired
    /// computer that can reach it, until one accepts. The service holds the
    /// flag for the phone's every computer.
    private func syncDismissal() {
        guard dismissalSync == nil, !dismissal.told else { return }
        let wanted = dismissal.dismissed
        dismissalSync = Task { [weak self] in
            guard let self else { return }
            for host in self.followableHosts() where !Task.isCancelled {
                guard (try? await self.push.liveActivityToken(host: host, kind: wanted ? "dismissed" : "foreground", token: "")) != nil else { continue }
                if self.dismissal.dismissed == wanted { self.dismissal.told = true }
                break
            }
            self.dismissalSync = nil
            if !self.dismissal.told, self.dismissal.dismissed != wanted { self.syncDismissal() }
            else if self.dismissal.told { self.scheduleWorkScan() }
        }
    }

    /// One piece of work the phone can name on a Mac: a chat's running reply,
    /// or the chat itself while the phone has not read which reply runs.
    struct WorkItem: Hashable {
        let chat: String
        let turn: String?
    }

    /// Positive work evidence. Current, hydrated history can prove a chat idle;
    /// offline history and catalog rows cannot (saved rows deliberately clear
    /// isWorking). Absence is never idleness.
    static func work(_ model: ConnectionModel) -> Set<WorkItem> {
        var working = Set<WorkItem>(), idle = Set<String>()
        for (chat, snapshot) in model.snapshots {
            let cached = model.cachedConversationIds.contains(chat)
            if let turn = snapshot.activeTurnID {
                // Offline history can suggest work, but its reply identity
                // cannot prove replacement of the dismissed work on reconnect.
                working.insert(WorkItem(chat: chat, turn: cached ? nil : turn))
            } else if snapshot.hasUnassignedPreTurnWork {
                working.insert(WorkItem(chat: chat, turn: nil))
            } else if !cached && snapshot.thread.hydrated {
                idle.insert(chat)
            }
        }
        for (chat, status) in model.nativeThreadStatus {
            if status.isWorking == true { working.insert(WorkItem(chat: chat, turn: nil)) }
            else if status.isWorking == false { idle.insert(chat) }
        }
        for thread in model.projects.pinned.map(\.thread) + model.projects.threads.values.flatMap(\.threads) {
            let chat = thread.conversationId ?? thread.reference
            // A current completed history outvotes an older working catalog
            // row. A live native process above can still report pre-turn work.
            if thread.isWorking && !idle.contains(chat) { working.insert(WorkItem(chat: chat, turn: nil)) }
        }
        // A named reply stands for its chat; any source still working outvotes an idle one.
        let named = Set(working.compactMap { $0.turn == nil ? nil : $0.chat })
        working = working.filter { $0.turn != nil || !named.contains($0.chat) }
        return working
    }

    private func followConnections(_ connections: [(host: String, model: ConnectionModel)]) {
        followForegroundWork(connections)
        let hosts = Set(connections.map(\.host))
        for host in startRegistrations.keys where !hosts.contains(host) { startRegistrations[host] = nil }
        for host in startTasks.keys where !hosts.contains(host) { startTasks[host]?.task.cancel() }
        waitingForConnection.formIntersection(Set(connections.filter { isEnabled($0.host) && push.canFollowLiveActivity($0.host) }.map(\.host)))
        for host in connectionWatches.keys where !hosts.contains(host) { connectionWatches[host] = nil }
        for (host, model) in connections where connectionWatches[host]?.model !== model {
            // Ignore cached state and repeated healthy reads (published as
            // often as every 300 ms). Only a real reconnect opens another
            // registration window; reads cannot prove signed-token acceptance.
            let observation = model.$macConnected.removeDuplicates().dropFirst().sink { [weak self] connected in
                guard let self, connected == true else { return }
                self.waitingForConnection.remove(host)
                for activity in self.activities(for: host) where !self.registered.contains(activity.id) && self.registering[activity.id] == nil && !self.ending.contains(activity.id) {
                    self.attemptedTokens[activity.id] = nil
                    if let token = self.tokens[activity.id] { self.retryUpdateToken(token, activityID: activity.id) }
                }
            }
            connectionWatches[host] = (model, observation)
        }
    }

    private func followForegroundWork(_ connections: [(host: String, model: ConnectionModel)]) {
        guard isForeground else { return }
        let hosts = Set(connections.map(\.host))
        for host in workWatches.keys where !hosts.contains(host) { workWatches[host] = nil }
        for (host, model) in connections where workWatches[host]?.model !== model {
            let wake: (Bool) -> Void = { [weak self] _ in
                self?.scheduleWorkScan()
            }
            // Project to work presence before deduplication: streaming an
            // already-running reply must not rescan ActivityKit per delta. The
            // model keeps its history's work presence up to date per chat.
            let observations = [
                model.$hasSnapshotWork.removeDuplicates().dropFirst().sink(receiveValue: wake),
                model.$nativeThreadStatus.map { $0.values.contains { $0.isWorking == true } }
                    .removeDuplicates().dropFirst().sink(receiveValue: wake),
                model.projects.$pinned.map { $0.contains { $0.thread.isWorking } }
                    .removeDuplicates().dropFirst().sink(receiveValue: wake),
                model.projects.$threads.map { $0.values.contains { $0.threads.contains(where: \.isWorking) } }
                    .removeDuplicates().dropFirst().sink(receiveValue: wake),
            ]
            workWatches[host] = (model, observations)
        }
    }

    private func scheduleWorkScan() {
        guard isForeground, workScanTask == nil else { return }
        workScanTask = Task { [weak self] in
            // @Published emits before assignment. One actor turn also folds
            // simultaneous work signals into one scan of the current values.
            await Task.yield()
            guard !Task.isCancelled, let self, self.isForeground else { return }
            self.workScanTask = nil
            self.scan()
        }
    }

    private func start(host: String, model: ConnectionModel, now: Date) {
        let attributes = AgentActivityAttributes(hostID: host, registrationId: push.registrationID(host: host),
                                                 computer: AgentActivityAttributes.label(model.macName, limit: 40),
                                                 conversationID: model.visibleChat?.id)
        // Wait for authoritative counts from the Mac's first push or reply below.
        let content = Content(phase: .running, running: 1, needsInput: nil)
        do {
            let id = try activityClient.request(attributes, content, now.addingTimeInterval(Self.staleAfter))
            watch(id)
        } catch {
            // Live Activities can be off or at the system's limit. The chat
            // itself still shows the work, so there is nothing to recover here.
        }
    }

    private func host(of attributes: AgentActivityAttributes) -> String? {
        attributes.hostID ?? attributes.registrationId.flatMap { push.host(registration: $0) }
    }

    private func activities(for host: String) -> [OwnedActivity] {
        activityClient.activities().filter { $0.isActive && self.host(of: $0.attributes) == host }
    }

    /// Follow an activity's token (it can change) and its end.
    private func watch(_ id: String) {
        guard watchers[id] == nil else { return }
        firstSeen[id] = firstSeen[id] ?? now()
        if followedHosts[id] == nil, let activity = activityClient.activities().first(where: { $0.id == id && $0.isActive }) {
            followedHosts[id] = host(of: activity.attributes)
        }
        // One card only: a new one takes the place of an earlier card that lingers as Done.
        for old in activityClient.activities() where old.id != id && !old.isActive && !ending.contains(old.id) && old.phase?.isFinished == true {
            end(old.id, dismissal: .immediate)
        }
        guard activityClient.observe else { return }
        watchers[id] = Task { [weak self] in
            await Self.follow(id,
                              token: { [weak self] token in await self?.registerUpdateToken(token, activityID: id) },
                              ended: { [weak self] dismissed in self?.ended(id, dismissedByOwner: dismissed) })
            self?.watchers[id] = nil
            self?.firstSeen[id] = nil
            self?.followedHosts[id] = nil
        }
    }

    func registerUpdateToken(_ token: String, activityID id: String) async {
        guard !Task.isCancelled, !ending.contains(id), let activity = activityClient.activities().first(where: { $0.id == id && $0.isActive }) else { return }
        if tokens[id] != token {
            registered.remove(id)
            // A first token uses the activity's original window; only a
            // replacement token needs a fresh registration deadline.
            if tokens[id] != nil || firstSeen[id] == nil { firstSeen[id] = now() }
        }
        tokens[id] = token
        switch push.liveActivityAuthority(host: activity.attributes.hostID, registration: activity.attributes.registrationId) {
        case .restoring:
            restoring.insert(id)
            return
        case .revoked:
            end(id, dismissal: .immediate)
            return
        case .available: break
        }
        // A push-start woken in the background, before any scan, for a dismissed card ends here.
        guard let host = host(of: activity.attributes), isEnabled(host), !isDismissed else {
            end(id, dismissal: .immediate); return
        }
        // The phone shows one activity: a second one, such as another Mac's
        // push-start, goes away once the first has been accepted.
        if activityClient.activities().contains(where: { $0.id != id && $0.isActive && !ending.contains($0.id) && registered.contains($0.id) }) {
            end(id, dismissal: .immediate); return
        }
        guard !registered.contains(id), attemptedTokens[id] != token else { return }
        guard registering[id] == nil else { return }
        let owner = UUID()
        registering[id] = owner
        attemptedTokens[id] = token
        defer {
            if registering[id] == owner {
                registering[id] = nil
                if let latest = tokens[id], latest != token, !ending.contains(id) {
                    retryUpdateToken(latest, activityID: id)
                }
            }
        }
        // A background wake (push-to-start) gets a short window to finish this.
        let background = UIApplication.shared.beginBackgroundTask(withName: "Wonder Live Activity")
        defer { if background != .invalid { UIApplication.shared.endBackgroundTask(background) } }
        func stillCurrent() -> Bool {
            !Task.isCancelled && registering[id] == owner && !ending.contains(id) && tokens[id] == token && isEnabled(host)
                && activityClient.activities().contains(where: { $0.id == id && $0.isActive && $0.attributes.registrationId == activity.attributes.registrationId })
                && push.liveActivityAuthority(host: activity.attributes.hostID, registration: activity.attributes.registrationId) == .available
        }
        for attempt in 0..<3 {
            guard stillCurrent() else { return }
            do {
                let reply = try await push.liveActivityToken(host: host, kind: "update", token: token)
                guard stillCurrent() else { return }
                if reply.dismissed == true {
                    // The service already holds a dismissal the phone did not see: end
                    // this card. In front, the owner is back and the service is told so;
                    // otherwise nothing starts until they open Cupola.
                    if isForeground { reopened() } else { dismissal = Dismissal(dismissed: true, told: true) }
                    end(id, dismissal: .immediate)
                    return
                }
                guard reply.registered else { return }
                registered.insert(id)
                waitingForConnection.remove(host)
                if let state = reply.state { await activityClient.apply(id, truthful(state, host: host)) }
                return
            } catch {
                guard stillCurrent() else { return }
                if attempt < 2 { try? await Task.sleep(for: .seconds(2 << attempt)) }
            }
        }
    }

    /// A state of Done while this phone itself sees a chat working shows
    /// running instead, until the service's next push says the truth. Only work
    /// read from a connected Mac with a named reply counts; cached history can't
    /// override a genuine Done.
    func truthful(_ state: Content, host: String) -> Content {
        guard state.phase.isFinished, let model = connections().first(where: { $0.host == host })?.model,
              model.macConnected == true else { return state }
        let working = Set(Self.work(model).filter { $0.turn != nil }.map(\.chat)).count
        guard working > 0 else { return state }
        return Content(phase: .running, running: working, needsInput: 0)
    }

    private func retryUpdateToken(_ token: String, activityID id: String) {
        guard tokenTasks[id] == nil, !ending.contains(id) else { return }
        let owner = UUID()
        let task = Task { [weak self] in
            guard let self, !Task.isCancelled, self.tokenTasks[id]?.owner == owner else { return }
            if self.tokens[id] == token { await self.registerUpdateToken(token, activityID: id) }
            // Cancellation may have released this slot and started a successor
            // while the old transport/apply was suspended.
            guard self.tokenTasks[id]?.owner == owner else { return }
            self.tokenTasks[id] = nil
            if !Task.isCancelled, let latest = self.tokens[id], latest != token {
                self.retryUpdateToken(latest, activityID: id)
            }
        }
        tokenTasks[id] = (owner, task)
    }

    private func ended(_ id: String, dismissedByOwner: Bool = false) {
        // Only the first observed end counts, and never one the phone asked for.
        if followedHosts.removeValue(forKey: id) != nil, dismissedByOwner, !ending.contains(id) { markDismissed() }
        registered.remove(id)
        restoring.remove(id)
        attemptedTokens[id] = nil
        registering[id] = nil
        tokenTasks.removeValue(forKey: id)?.task.cancel()
        guard let token = tokens.removeValue(forKey: id), let activity = Self.find(id),
              let host = host(of: activity.attributes) else { return }
        // Best effort: the Mac also forgets a token APNs reports as gone.
        Task { _ = try? await push.liveActivityToken(host: host, kind: "ended", token: token) }
    }

    private func end(_ id: String, dismissal: ActivityUIDismissalPolicy) {
        guard ending.insert(id).inserted else { return }
        registered.remove(id)
        restoring.remove(id)
        registering[id] = nil
        tokenTasks.removeValue(forKey: id)?.task.cancel()
        activityClient.end(id, dismissal)
    }

    // MARK: ActivityKit by ID (activities are not Sendable)

    nonisolated private static func find(_ id: String) -> Activity<AgentActivityAttributes>? {
        Activity<AgentActivityAttributes>.activities.first { $0.id == id }
    }

    nonisolated private static func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }

    nonisolated private static func follow(_ id: String,
                                           token: @escaping @MainActor @Sendable (String) async -> Void,
                                           ended: @escaping @MainActor @Sendable (Bool) -> Void) async {
        await withTaskGroup(of: Bool.self) { group in
            group.addTask {
                guard let activity = find(id) else { return false }
                for await data in activity.pushTokenUpdates { await token(hex(data)) }
                return false
            }
            group.addTask {
                guard let activity = find(id) else { return false }
                for await state in activity.activityStateUpdates where state == .ended || state == .dismissed {
                    // Gone while still showing work: swiped away, or ended by the system.
                    await ended(!activity.content.state.phase.isFinished)
                    return true
                }
                return false
            }
            for await didEnd in group where didEnd { group.cancelAll(); break }
        }
    }

    nonisolated private static func followNewActivities(_ found: @escaping @MainActor @Sendable (String) -> Void) async {
        for await activity in Activity<AgentActivityAttributes>.activityUpdates {
            let id = activity.id
            await found(id)
        }
    }

    nonisolated private static func followStartTokens(_ received: @escaping @MainActor @Sendable (String) -> Void) async {
        for await data in Activity<AgentActivityAttributes>.pushToStartTokenUpdates {
            let token = hex(data)
            await received(token)
        }
    }

    nonisolated private static func update(_ id: String, _ state: Content, staleDate: Date? = nil) async {
        guard let activity = find(id) else { return }
        await activity.update(ActivityContent(state: state, staleDate: staleDate ?? Date().addingTimeInterval(staleAfter)))
    }

    nonisolated private static func end(_ id: String, content: Content?, dismissal: ActivityUIDismissalPolicy) async {
        guard let activity = find(id) else { return }
        await activity.end(content.map { ActivityContent(state: $0, staleDate: nil) }, dismissalPolicy: dismissal)
    }

    /// The Mac's state, applied by the phone itself (its registration reply).
    nonisolated private static func apply(_ state: Content, to id: String) async {
        guard let activity = find(id), activity.activityState == .active || activity.activityState == .stale else { return }
        if state.phase.isFinished {
            await end(id, content: state, dismissal: .after(Date().addingTimeInterval(finishedLinger)))
        } else {
            await update(id, state)
        }
    }

    private func registerStartToken(_ token: String, host: String) {
        guard isEnabled(host), startTasks[host] == nil,
              let device = library?.paired.first(where: { $0.credential.hostInstallationId == host })?.credential.deviceId,
              let registration = push.registrationID(host: host),
              startRegistrations[host]?.device != device || startRegistrations[host]?.registration != registration || startRegistrations[host]?.token != token else { return }
        let removal = removalTasks[host]?.task
        let owner = UUID()
        let task = Task { [weak self] in
            await removal?.value
            guard let self else { return }
            defer {
                if self.startTasks[host]?.owner == owner {
                    self.startTasks[host] = nil
                    if !Task.isCancelled, self.isEnabled(host), let latest = self.startToken, latest != token {
                        self.registerStartToken(latest, host: host)
                    }
                }
            }
            guard !Task.isCancelled, self.isEnabled(host), self.startToken == token else { return }
            do {
                let reply = try await self.push.liveActivityToken(host: host, kind: "start", token: token)
                guard reply.registered, !Task.isCancelled, self.isEnabled(host), self.startToken == token,
                      self.push.registrationID(host: host) == registration,
                      self.library?.paired.contains(where: { $0.credential.hostInstallationId == host && $0.credential.deviceId == device }) == true else { return }
                self.startRegistrations[host] = (device, registration, token)
                self.markPendingRemoval(host, false)
            } catch {}
        }
        startTasks[host] = (owner, task)
    }

    // MARK: Turning off

    private func markPendingRemoval(_ host: String, _ pending: Bool) {
        var hosts = Set(defaults.stringArray(forKey: Self.pendingRemovalKey) ?? [])
        if pending { hosts.insert(host) } else { hosts.remove(host) }
        defaults.set(hosts.sorted(), forKey: Self.pendingRemovalKey)
    }

    /// The service must forget the push-to-start token, or it could start an
    /// activity after the owner turned them off; retried until it confirms.
    private func forgetStartToken(_ host: String) {
        guard removalTasks[host] == nil else { return }
        let enrollment = startTasks[host]?.task
        let owner = UUID()
        let task = Task { [weak self] in
            await enrollment?.value
            guard let self else { return }
            defer { if self.removalTasks[host]?.owner == owner { self.removalTasks[host] = nil } }
            do {
                guard !Task.isCancelled, !self.isEnabled(host) else { return }
                let start = try await self.push.liveActivityToken(host: host, kind: "start", token: "")
                guard !start.registered, !Task.isCancelled, !self.isEnabled(host) else { return }
                let update = try await self.push.liveActivityToken(host: host, kind: "update", token: "")
                guard !update.registered, !Task.isCancelled, !self.isEnabled(host) else { return }
                self.markPendingRemoval(host, false)
            } catch {}
        }
        removalTasks[host] = (owner, task)
    }

    private func retryPendingRemovals() {
        for host in defaults.stringArray(forKey: Self.pendingRemovalKey) ?? [] where !isEnabled(host) {
            forgetStartToken(host)
        }
    }

    #if WONDER_DIAGNOSTICS
    /// Scenario evidence: the Live Activities ActivityKit itself reports.
    static func probe() -> String {
        let activities = Activity<AgentActivityAttributes>.activities
        if shared.isDismissed { return "Live Activity: dismissed while working; suppressed" }
        guard !activities.isEmpty else { return "Live Activity: none" }
        return "Live Activity: " + activities.map { activity in
            let state: String = switch activity.activityState {
            case .active: "active"
            case .ended: "ended"
            case .dismissed: "dismissed"
            case .stale: "stale"
            case .pending: "pending"
            @unknown default: "unknown"
            }
            let shown = AgentActivityPresentation(attributes: activity.attributes, state: activity.content.state, isStale: activity.activityState == .stale)
            return "\(activity.content.state.phase.rawValue) \(activity.content.state.running) (\(state)): \(shown.detail); compact \(shown.shortLabel.isEmpty ? "icon only" : shown.shortLabel)"
                + (shown.rows.isEmpty ? "" : ", rows: " + shown.rows.map { "\($0.title) is \($0.status.rawValue)" }.joined(separator: ", ") + (shown.more > 0 ? ", +\(shown.more) more" : ""))
        }.joined(separator: ", ")
    }

    /// A Lock Screen capture of each state from a local fixture: no Mac, no push.
    func showFixture(_ phase: AgentActivityAttributes.Phase, running: Int, stale: Bool) async {
        for activity in Activity<AgentActivityAttributes>.activities where activity.attributes.hostID == "fixture-mac" {
            await Self.end(activity.id, content: nil, dismissal: .immediate)
        }
        // Each capture owns this synthetic host. Exercise the production
        // ActivityKit observer, and leave no dismissal from an earlier case.
        dismissal = Dismissal()
        markPendingRemoval("fixture-mac", false)
        // A negative count is the capture runner's cleanup: end only this
        // app's synthetic activity without starting another one.
        guard running >= 0 else { return }
        let now = Date()
        let needsInput = phase == .needsApproval || phase == .needsInput ? min(1, running) : 0
        // One row per working thread (up to four), so a capture shows the list:
        // the first row takes the fixture's phase, the others are running.
        let samples = [("Fix login crash", "Cupola"), ("Refactor sync engine", "Cupola"), ("Write release notes", "Studio"),
                       ("Review pull request 214", "Studio"), ("Update onboarding", "Cupola")]
        let rows = phase.isFinished && running <= 0 ? [] : (0..<max(0, running)).prefix(4).map { index in
            AgentActivityAttributes.ThreadRow(title: samples[index % samples.count].0, project: samples[index % samples.count].1,
                                              status: index == 0 ? phase : .running,
                                              startedAt: now.addingTimeInterval(Double(-90 * (index + 1))).timeIntervalSince1970)
        }
        let content = Content(phase: phase, running: phase.isFinished ? 0 : running, needsInput: needsInput,
                              threads: rows.isEmpty ? nil : rows, more: running > 4 ? running - 4 : nil)
        let attributes = AgentActivityAttributes(hostID: "fixture-mac", registrationId: nil, computer: "Studio", conversationID: "fixture-chat")
        guard let activity = try? Activity.request(attributes: attributes,
                                                   content: ActivityContent(state: content, staleDate: now.addingTimeInterval(Self.staleAfter)),
                                                   pushType: nil) else { return }
        watch(activity.id)
        // As if the Mac's pushes stopped: the stale date has passed.
        if stale { await Self.update(activity.id, content, staleDate: now.addingTimeInterval(-60)) }
    }
    #endif
}
