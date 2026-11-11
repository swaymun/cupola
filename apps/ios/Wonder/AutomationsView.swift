import SwiftUI
import WonderPairing

struct AutomationItem: Decodable, Identifiable, Sendable {
    let id: String
    let revision: Int64
    let name: String
    let scopeType: String
    let scopeId: String
    let prompt: String
    let rrule: String
    let timezone: String
    let notificationPolicy: String
    let status: String
    let nextRunText: String?
    /// When a scheduled run last started, as the Mac recorded it.
    let lastRunAt: Date?
    /// Set once any run started, so the row asks for the latest one.
    let hasRuns: Bool

    private enum CodingKeys: String, CodingKey {
        case id, revision, name, scopeType, scopeId, prompt, rrule, timezone, notificationPolicy, status, nextRunAt, lastRunAt, lastAttemptAt
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        revision = try values.decodeIfPresent(Int64.self, forKey: .revision) ?? 0
        name = try values.decode(String.self, forKey: .name)
        scopeType = try values.decode(String.self, forKey: .scopeType)
        scopeId = try values.decode(String.self, forKey: .scopeId)
        prompt = try values.decode(String.self, forKey: .prompt)
        rrule = try values.decode(String.self, forKey: .rrule)
        timezone = try values.decode(String.self, forKey: .timezone)
        notificationPolicy = try values.decode(String.self, forKey: .notificationPolicy)
        status = try values.decode(String.self, forKey: .status)
        nextRunText = Self.displayTime(try values.decodeIfPresent(String.self, forKey: .nextRunAt))
        lastRunAt = AutomationDate.parse(try values.decodeIfPresent(String.self, forKey: .lastRunAt))
        hasRuns = try values.decodeIfPresent(String.self, forKey: .lastAttemptAt) != nil
    }

    static func displayTime(_ value: String?) -> String? {
        AutomationDate.parse(value)?.formatted(date: .abbreviated, time: .shortened)
    }

    var isPaused: Bool { status == "paused" }
    var canToggle: Bool { status == "active" || status == "paused" }
}

/// Reads the Mac's timestamps, with or without fractional seconds, once when a
/// response is decoded; rows only format the prepared dates. The two parsers
/// are made once and used one caller at a time.
enum AutomationDate {
    private final class Parsers: @unchecked Sendable {
        let lock = NSLock()
        let fractional: ISO8601DateFormatter = {
            let formatter = ISO8601DateFormatter()
            formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            return formatter
        }()
        let whole = ISO8601DateFormatter()
    }
    private static let parsers = Parsers()

    /// Nil for a missing or malformed value.
    static func parse(_ value: String?) -> Date? {
        guard let value else { return nil }
        return parsers.lock.withLock { parsers.fractional.date(from: value) ?? parsers.whole.date(from: value) }
    }
}

/// One run of an automation, newest first from `/runs`.
struct AutomationRun: Decodable, Sendable {
    let id: String
    let automationId: String
    /// `running`, `completed` or `failed`.
    let status: String
    let startedAt: String
    let error: String?
    /// `startedAt` read once at decode; nil when the Mac sent a malformed time.
    let startedDate: Date?
    let startedText: String?

    private enum CodingKeys: String, CodingKey { case id, automationId, status, startedAt, error }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        automationId = try values.decode(String.self, forKey: .automationId)
        status = try values.decode(String.self, forKey: .status)
        startedAt = try values.decode(String.self, forKey: .startedAt)
        error = try values.decodeIfPresent(String.self, forKey: .error)
        startedDate = AutomationDate.parse(startedAt)
        startedText = startedDate?.formatted(date: .abbreviated, time: .shortened)
    }
}

/// The newest run of each automation, kept in step with changes made on the
/// Automations screen. Every change (Run Now, pause, edit, delete) advances
/// `revision`, so a list load that began before it is discarded instead of
/// overwriting what the change showed.
struct AutomationRunBoard {
    private(set) var latest: [String: AutomationRun] = [:]
    private(set) var revision: UInt64 = 0

    mutating func changed() { revision &+= 1 }

    mutating func record(_ run: AutomationRun) {
        latest[run.automationId] = run
        changed()
    }

    /// Replaces a run's earlier report with a newer one for the same run; a
    /// different run that started since keeps its place.
    mutating func update(_ run: AutomationRun) {
        guard latest[run.automationId]?.id == run.id else { return }
        record(run)
    }

    mutating func remove(_ automationID: String) {
        latest[automationID] = nil
        pendingRunRequests[automationID] = nil
        changed()
    }

    /// Run Now's request ID per automation, kept until the Mac answers for
    /// certain. The Mac starts one run per request ID, so retrying after a
    /// lost response returns the run it already started instead of a second.
    private var pendingRunRequests: [String: String] = [:]

    mutating func runRequestID(for automationID: String) -> String {
        if let pending = pendingRunRequests[automationID] { return pending }
        let id = UUID().uuidString
        pendingRunRequests[automationID] = id
        return id
    }

    /// Settles Run Now's request: a returned run or a refusal ends it; a
    /// timeout, lost connection or server error keeps it for the retry.
    mutating func settleRunRequest(for automationID: String, failure: Error?) {
        if let failure, !Self.refused(failure) { return }
        pendingRunRequests[automationID] = nil
    }

    private static func refused(_ error: Error) -> Bool {
        switch error {
        case PairingFailure.response(let code), PairingFailure.hostMessage(let code, _): return (400..<500).contains(code)
        case PairingFailure.invalidLink: return true
        default: return false
        }
    }

    /// Takes a load's runs only if nothing changed on this screen since the
    /// load began at `revision`.
    @discardableResult mutating func applyLoad(_ runs: [String: AutomationRun], startedAt revision: UInt64) -> Bool {
        guard revision == self.revision else { return false }
        latest = runs
        return true
    }

    /// When the automation last ran: its newest run, manual or scheduled, or
    /// the Mac's record of its last scheduled run, whichever is later.
    func lastRun(_ item: AutomationItem) -> Date? {
        let started = latest[item.id]?.startedDate
        switch (started, item.lastRunAt) {
        case let (started?, recorded?): return max(started, recorded)
        case let (started, recorded): return started ?? recorded
        }
    }
}

/// What the list shows beside its automations once the list itself is up:
/// each Project thread's name and each automation's newest run. Every detail
/// is its own request; a few run at once instead of one after another.
struct AutomationListDetails: Sendable {
    enum Request: Hashable, Sendable {
        /// The name of a Project thread, by its conversation.
        case target(String)
        /// The newest run of an automation.
        case latestRun(String)
    }

    enum Answer: Sendable {
        case target(String)
        case latestRun(AutomationRun?)
    }

    /// Names are keyed as the list keys them (`project:<thread>`); runs by automation.
    private(set) var names: [String: String] = [:]
    private(set) var runs: [String: AutomationRun] = [:]
    private(set) var namesMissing = false
    private(set) var runsMissing = false

    static let concurrency = 4

    /// One request per distinct Project thread and per automation that has
    /// run, so a load costs 1 + threads + automations with runs.
    static func requests(for items: [AutomationItem]) -> [Request] {
        var seen = Set<String>()
        let targets = items.filter { $0.scopeType == "project_thread" && seen.insert($0.scopeId).inserted }
        return targets.map { .target($0.scopeId) } + items.filter(\.hasRuns).map { .latestRun($0.id) }
    }

    /// Runs `requests` at most `limit` at a time. A failed or cancelled
    /// request counts as missing; the caller checks cancellation before use.
    static func fetch(_ requests: [Request], limit: Int = concurrency,
                      _ perform: @escaping @Sendable (Request) async throws -> Answer) async -> AutomationListDetails {
        var details = AutomationListDetails()
        await withTaskGroup(of: (Request, Answer?).self) { group in
            var next = requests.startIndex
            while next < requests.endIndex, next - requests.startIndex < max(1, limit) {
                let request = requests[next]
                group.addTask { (request, try? await perform(request)) }
                next += 1
            }
            while let result = await group.next() {
                details.take(result.0, result.1)
                if next < requests.endIndex, !Task.isCancelled {
                    let request = requests[next]
                    group.addTask { (request, try? await perform(request)) }
                    next += 1
                }
            }
            // Requests never started after cancellation are missing too.
            for request in requests[next...] { details.take(request, nil) }
        }
        return details
    }

    private mutating func take(_ request: Request, _ answer: Answer?) {
        switch (request, answer) {
        case let (.target(id), .target(name)?): names["project:\(id)"] = name
        case (.target, _): namesMissing = true
        case let (.latestRun(id), .latestRun(run)?): runs[id] = run
        case (.latestRun, _): runsMissing = true
        }
    }

    /// Asks the Mac for one detail, off the main actor.
    static func ask(_ request: Request, api: PairingAPI, saved: SavedConnection) async throws -> Answer {
        switch request {
        case .target(let id):
            guard let escaped = id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { throw PairingFailure.invalidLink }
            let detail: AutomationProjectDetail = try await api.request(
                "/api/v1/project-conversations/\(escaped)", origin: saved.origin, credential: saved.credential)
            return .target("\(detail.projectName) · \(detail.title)")
        case .latestRun(let id):
            guard let pathID = id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { throw PairingFailure.invalidLink }
            let runs: [AutomationRun] = try await api.request(
                "/api/v1/automations/\(pathID)/runs", origin: saved.origin, credential: saved.credential)
            return .latestRun(runs.first)
        }
    }
}

private struct AutomationRunRequest: Encodable {
    let clientRequestId: String
}

private struct AutomationDeleted: Decodable, Sendable {}

private struct AutomationStatusChange: Encodable {
    let status: String
    let expectedRevision: Int64
}

private struct AutomationProjectDetail: Decodable, Sendable {
    let projectName: String
    let title: String
}

/// The form the page has open: one computer's automation, or a new one for it.
/// The page owns it (not the lazy rows), so scrolling cannot drop the form.
private struct AutomationEditorContext: Identifiable, Hashable {
    let id = UUID()
    let host: String
    let item: AutomationItem?
    let targetLabel: String?
    static func == (lhs: Self, rhs: Self) -> Bool { lhs.id == rhs.id }
    func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

/// What the form told the page, for the computer whose rows should apply it.
private struct AutomationEditorOutcome: Equatable {
    let id = UUID()
    let host: String
    let saved: AutomationItem?
    let targetLabel: String?
    static func == (lhs: Self, rhs: Self) -> Bool { lhs.id == rhs.id }
}

/// What one computer's rows tell the page that lists them all.
private struct AutomationHostSummary: Equatable {
    var count = 0
    var loaded = false
    var loading = false
}

/// Automations of every paired computer in one list, opened from the sidebar as
/// a full screen in the chat column. Each card names its computer when several
/// are paired; its actions go to that computer. New and Edit push their form.
struct AutomationsView: View {
    let models: [ConnectionModel]
    /// Opens the chat an automation posts into, on its computer (its host
    /// installation and the thread's conversation). Without it rows only name the chat.
    var openChat: ((_ host: String, _ conversation: String) -> Void)? = nil
    @State private var refreshTick = 0
    @State private var editor: AutomationEditorContext?
    @State private var outcome: AutomationEditorOutcome?
    @State private var summaries: [String: AutomationHostSummary] = [:]

    private func hostID(_ model: ConnectionModel) -> String { model.connection?.credential.hostInstallationId ?? "" }
    private var anyLoading: Bool { summaries.values.contains { $0.loading } }
    /// Only computers whose list was actually read count: an offline or failed
    /// one says so in its own rows and says nothing about being empty.
    private var everyLoaded: Bool { models.allSatisfy { summaries[hostID($0)]?.loaded == true } }
    private var isEmpty: Bool { models.allSatisfy { (summaries[hostID($0)]?.count ?? 0) == 0 } }
    private var canCreate: Bool { models.contains { $0.macConnected == true && !$0.accessEnded } }

    var body: some View {
        WonderForm {
            ForEach(models, id: \.assignmentScope) { model in
                AutomationsHostRows(model: model, labelled: models.count > 1, openChat: openChat,
                                    refreshTick: refreshTick, editor: $editor, outcome: outcome, summaries: $summaries)
            }
            if everyLoaded && isEmpty {
                Section {
                    Text("No automations yet.")
                        .foregroundStyle(.secondary)
                        .accessibilityIdentifier("automations-empty")
                }
            }
            Section {
                newAutomationControl
            } footer: {
                Text(canCreate ? "Runs a task in an existing Project thread."
                         : "New automations need a connected computer.")
            }
        }
        .settingsNavigationTitle("Automations") {
            Button("Refresh", systemImage: "arrow.clockwise") { refreshTick += 1 }
                .disabled(anyLoading)
                .accessibilityIdentifier("automations-refresh")
        }
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { refreshTick += 1 }
        .navigationDestination(item: $editor) { context in
            if let model = models.first(where: { hostID($0) == context.host }) {
                ProjectAutomationEditor(model: model, item: context.item, targetLabel: context.targetLabel) { saved, label in
                    outcome = AutomationEditorOutcome(host: context.host, saved: saved, targetLabel: label)
                    editor = nil
                } onReload: {
                    outcome = AutomationEditorOutcome(host: context.host, saved: nil, targetLabel: nil)
                    editor = nil
                }
            }
        }
    }

    private func startNew(on model: ConnectionModel) {
        editor = AutomationEditorContext(host: hostID(model), item: nil, targetLabel: nil)
    }

    /// One computer creates directly; several ask which one first.
    @ViewBuilder private var newAutomationControl: some View {
        if models.count > 1 {
            Menu {
                ForEach(models, id: \.assignmentScope) { model in
                    Button(model.macName) { startNew(on: model) }
                        .disabled(model.macConnected != true || model.accessEnded)
                }
            } label: {
                Label("New automation", systemImage: "plus")
            }
            .disabled(!canCreate)
            .accessibilityIdentifier("project-automation-new")
        } else {
            Button("New automation", systemImage: "plus") { if let model = models.first { startNew(on: model) } }
                .disabled(!canCreate)
                .accessibilityIdentifier("project-automation-new")
        }
    }
}

/// One computer's automations as rows of the shared list. Every request goes
/// to this computer.
private struct AutomationsHostRows: View {
    @ObservedObject var model: ConnectionModel
    let labelled: Bool
    var openChat: ((_ host: String, _ conversation: String) -> Void)?
    let refreshTick: Int
    @Binding var editor: AutomationEditorContext?
    let outcome: AutomationEditorOutcome?
    @Binding var summaries: [String: AutomationHostSummary]
    @State private var items: [AutomationItem] = []
    @State private var targetNames: [String: String] = [:]
    @State private var loadToken: UUID?
    @State private var loader: (scope: String, task: Task<Void, Never>)?
    @State private var loaded = false
    @State private var failure: String?
    @State private var changing: Set<String> = []
    /// What the busy row is doing (pausing, starting a run, deleting).
    @State private var changeProgress: [String: String] = [:]
    @State private var changeFailures: [String: String] = [:]
    /// The newest run of each automation, for its last-run line.
    @State private var runBoard = AutomationRunBoard()
    @State private var runNotices: [String: String] = [:]
    @State private var deleting: AutomationItem?
    @AppStorage(ActionConfirmation.deleteKey) private var confirmDelete = ActionConfirmation.deleteDefault

    private var loading: Bool { loadToken != nil }
    private var hostID: String { model.connection?.credential.hostInstallationId ?? "" }

    var body: some View {
        Section {
            // One message per state. Offline is the computer's name with a red dot and nothing to
            // press: it reconnects on its own and its automations load when it answers.
            if model.accessEnded {
                Label("Access ended on \(model.macName). Pair this device again to manage automations.", systemImage: "wifi.slash")
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("automations-offline")
            } else if model.macConnected != true {
                HStack(spacing: 8) {
                    Text(model.macName).foregroundStyle(.secondary)
                    Circle().fill(Color.red).frame(width: 8, height: 8).accessibilityHidden(true)
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("\(model.macName), offline")
                .accessibilityIdentifier("automations-offline")
            } else if let failure {
                VStack(alignment: .leading, spacing: 4) {
                    Text(labelled ? "\(model.macName): \(failure)" : failure).foregroundStyle(.secondary)
                        .accessibilityIdentifier("automations-failure")
                    Button("Try again") { Task { await load(recheck: true) } }
                        .disabled(loading || model.accessEnded)
                        .accessibilityIdentifier("automations-retry")
                }
            }
            // Only while nothing can show yet: rows appear as soon as
            // the list arrives, before their names and runs. Shown from the first
            // render, not from the load's start: a Section with no content never
            // mounts its `.task`, so an unloaded connected computer would stay blank.
            if model.macConnected == true, !model.accessEnded, failure == nil, !loaded, items.isEmpty {
                ProgressView("Loading automations…")
                    .accessibilityIdentifier("automations-loading")
            }
            ForEach(items) { item in
                automationRow(item)
            }
        }
        .onChange(of: summary, initial: true) { _, value in summaries[hostID] = value }
                .onChange(of: refreshTick) { Task { await load(recheck: true) } }
        .onChange(of: model.macConnected) { _, connected in
            if connected == true, !loaded { Task { await load() } }
        }
        .onChange(of: outcome) { _, outcome in
            // A row scrolled away misses this; it reloads when it returns.
            guard let outcome, outcome.host == hostID else { return }
            runBoard.changed()
            if let saved = outcome.saved {
                if let index = items.firstIndex(where: { $0.id == saved.id }) { items[index] = saved }
                else { items.append(saved) }
                if let label = outcome.targetLabel { targetNames["project:\(saved.scopeId)"] = label }
            } else {
                Task { await load(recheck: true) }
            }
        }
        // Modifiers on a Section reach each of its rows, so this body runs once per row
        // and again whenever rows come and go; resetting here would empty the rows it
        // just loaded and loop. One unstructured load per computer scope instead.
        .task(id: model.assignmentScope) {
            let scope = model.assignmentScope
            guard loader?.scope != scope else { return }
            loader?.task.cancel()
            items = []; targetNames = [:]; loaded = false; failure = nil; changeFailures = [:]; changing = []; loadToken = nil
            runBoard = AutomationRunBoard(); runNotices = [:]; deleting = nil
            loader = (scope, Task { await load() })
        }
        .onChange(of: model.macConnected) { _, connected in
            if connected == true && !loading { Task { await load() } }
        }
        .confirmationDialog("Delete \(deleting?.name ?? "automation")?", isPresented: Binding(
            get: { deleting != nil }, set: { if !$0 { deleting = nil } }), titleVisibility: .visible, presenting: deleting) { item in
            Button("Delete automation", role: .destructive) { Task { await delete(item) } }
                .accessibilityIdentifier("automation-delete-confirm")
        } message: { _ in
            Text("It stops running. Messages it already posted stay in their thread.")
        }
    }

    private var summary: AutomationHostSummary {
        AutomationHostSummary(count: items.count,
                              loaded: loaded,
                              loading: loading)
    }

    @ViewBuilder private func automationRow(_ item: AutomationItem) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(item.name).font(.headline)
                Spacer(minLength: 8)
                if labelled {
                    Label(model.macName, systemImage: "desktopcomputer")
                        .font(.caption).foregroundStyle(.secondary)
                        .lineLimit(1)
                        .accessibilityIdentifier("automation-computer:\(item.id)")
                }
                // Active and paused already show as "Next: …" and the Pause/Resume button.
                if !item.isPaused, item.status != "active" {
                    Text("Unavailable").font(.caption).foregroundStyle(.secondary)
                }
            }
            if let openChat, item.scopeType == "project_thread", !item.scopeId.isEmpty,
               let host = model.connection?.credential.hostInstallationId {
                // Saved history opens offline too, so this stays available.
                Button {
                    openChat(host, item.scopeId)
                } label: {
                    Label(targetName(for: item), systemImage: "bubble.left.and.bubble.right")
                        .font(.subheadline)
                        .frame(minHeight: 44, alignment: .leading)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.borderless)
                .accessibilityLabel("Open chat: \(targetName(for: item))")
                .accessibilityIdentifier("automation-open-chat:\(item.id)")
            } else {
                Text(targetName(for: item))
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            VStack(alignment: .leading, spacing: 3) {
                if let repeatText = AutomationSchedule.repeatText(rrule: item.rrule, timezone: item.timezone) {
                    Label(repeatText, systemImage: "repeat")
                        .accessibilityIdentifier("automation-repeat:\(item.id)")
                }
                Label(item.isPaused ? "Next: paused" : "Next: \(item.nextRunText ?? "not scheduled")", systemImage: "calendar")
                lastRunLabel(item)
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            if let notice = runNotices[item.id] {
                Text(notice).font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("automation-run-notice:\(item.id)")
            }
            if let error = changeFailures[item.id] {
                Text(error).font(.caption).foregroundStyle(.red)
                    .accessibilityIdentifier("automation-change-failure:\(item.id)")
            }
            HStack(spacing: 20) {
                if item.canToggle {
                    Button {
                        Task { await changeStatus(item) }
                    } label: {
                        Text(item.isPaused ? "Resume" : "Pause")
                            .frame(minWidth: 44, minHeight: 44, alignment: .leading)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    .disabled(changing.contains(item.id) || model.macConnected != true || model.accessEnded)
                    .accessibilityIdentifier("automation-toggle:\(item.id)")
                }
                if item.scopeType == "project_thread" {
                    Button {
                        editor = AutomationEditorContext(host: hostID, item: item, targetLabel: targetName(for: item))
                    } label: {
                        Text("Edit")
                            .frame(minWidth: 44, minHeight: 44, alignment: .leading)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    .disabled(model.macConnected != true || model.accessEnded)
                    .accessibilityIdentifier("automation-edit:\(item.id)")
                    Button {
                        Task { await runNow(item) }
                    } label: {
                        Text("Run now")
                            .frame(minWidth: 44, minHeight: 44, alignment: .leading)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    .disabled(changing.contains(item.id) || runBoard.latest[item.id]?.status == "running"
                              || model.macConnected != true || model.accessEnded)
                    .accessibilityIdentifier("automation-run:\(item.id)")
                }
                Spacer(minLength: 0)
                Button(role: .destructive) {
                    if confirmDelete { deleting = item } else { Task { await delete(item) } }
                } label: {
                    Text("Delete")
                        .frame(minWidth: 44, minHeight: 44, alignment: .trailing)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.borderless)
                .disabled(changing.contains(item.id) || model.macConnected != true || model.accessEnded)
                .accessibilityIdentifier("automation-delete:\(item.id)")
            }
            if let progress = changing.contains(item.id) ? changeProgress[item.id] : nil {
                ProgressView(progress)
                    .font(.caption)
                    .accessibilityIdentifier("automation-changing:\(item.id)")
            }
        }
        .padding(.vertical, 4)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("automation-row:\(item.id)")
    }

    /// The newest run: a failure in the Mac's words, a run in progress, or
    /// when it last ran.
    @ViewBuilder private func lastRunLabel(_ item: AutomationItem) -> some View {
        if let run = runBoard.latest[item.id], run.status == "failed" {
            Label("Last run failed\(run.startedText.map { " (\($0))" } ?? ""): \(run.error ?? "Open its thread for details.")",
                  systemImage: "clock.badge.exclamationmark")
                .foregroundStyle(.red)
                .accessibilityIdentifier("automation-last-failure:\(item.id)")
        } else if runBoard.latest[item.id]?.status == "running" {
            Label("Running now", systemImage: "clock.arrow.circlepath")
                .accessibilityIdentifier("automation-running:\(item.id)")
        } else {
            Label("Last: \(runBoard.lastRun(item)?.formatted(date: .abbreviated, time: .shortened) ?? "never")", systemImage: "clock")
        }
    }

    private func targetName(for item: AutomationItem) -> String {
        if item.scopeType == "project_thread" {
            return targetNames["project:\(item.scopeId)"] ?? "Project thread"
        }
        return "Unavailable target"
    }

    @MainActor private func load(recheck: Bool = false) async {
        guard !loading else { return }
        let token = UUID()
        loadToken = token
        defer { if loadToken == token { loadToken = nil } }
        failure = nil
        let scope = model.assignmentScope
        let revision = runBoard.revision
        if model.macConnected != true { await model.check(renew: recheck, userInitiated: recheck) }
        guard scope == model.assignmentScope, !Task.isCancelled else { return }
        guard let saved = model.connection, !model.accessEnded, model.macConnected == true else {
            failure = model.accessEnded ? "Pair this device again to manage automations." : "Connect to your computer, then try again."
            return
        }
        /// Every publication: still this load, computer and pairing, and still wanted.
        func current() -> Bool {
            loadToken == token && scope == model.assignmentScope && model.connection?.origin == saved.origin
                && !model.accessEnded && !Task.isCancelled
        }
        let shown: [AutomationItem]
        do {
            let response: [AutomationItem] = try await model.api.request("/api/v1/automations", origin: saved.origin, credential: saved.credential)
            // A change made here since the load began (Run Now, pause, edit,
            // delete) showed newer state than this list.
            guard current(), revision == runBoard.revision else { return }
            shown = response.filter { $0.scopeType == "project_thread" }
            // The list shows at once; names already read stay until replaced.
            let threads = Set(shown.map { "project:\($0.scopeId)" })
            targetNames = targetNames.filter { threads.contains($0.key) }
            items = shown
            loaded = true
            changeFailures = [:]
            runNotices = [:]
        } catch {
            guard current(), revision == runBoard.revision else { return }
            failure = loadFailure(error)
            return
        }
        let api = model.api
        let details = await AutomationListDetails.fetch(AutomationListDetails.requests(for: shown)) { request in
            try await AutomationListDetails.ask(request, api: api, saved: saved)
        }
        guard current() else { return }
        targetNames.merge(details.names) { _, new in new }
        // Runs from before a change made here are stale; the change showed its own.
        runBoard.applyLoad(details.runs, startedAt: revision)
        if details.namesMissing {
            failure = "Some target names couldn’t be loaded."
        } else if details.runsMissing {
            failure = "Some recent runs couldn’t be loaded."
        }
    }

    @MainActor private func changeStatus(_ item: AutomationItem) async {
        guard item.canToggle, !changing.contains(item.id), let saved = model.connection,
              !model.accessEnded, model.macConnected == true else { return }
        let scope = model.assignmentScope
        changing.insert(item.id)
        changeProgress[item.id] = item.isPaused ? "Resuming…" : "Pausing…"
        changeFailures[item.id] = nil
        defer { if scope == model.assignmentScope { changing.remove(item.id) } }
        do {
            let body = try JSONEncoder().encode(AutomationStatusChange(
                status: item.isPaused ? "active" : "paused", expectedRevision: item.revision))
            guard let pathID = item.id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { throw PairingFailure.invalidLink }
            let updated: AutomationItem = try await model.api.request(
                "/api/v1/automations/\(pathID)",
                origin: saved.origin, body: body, credential: saved.credential, method: "PATCH")
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            guard updated.id == item.id else { throw ReadFailure.resync }
            runBoard.changed()
            if let index = items.firstIndex(where: { $0.id == item.id }) { items[index] = updated }
        } catch {
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            let detail: String
            if case PairingFailure.response(404) = error { detail = "It is no longer available. Refresh the list." }
            else { detail = requestFailure(error) }
            changeFailures[item.id] = "Couldn’t \(item.isPaused ? "resume" : "pause") this automation. \(detail)"
        }
    }

    private func latestRun(_ id: String, _ saved: SavedConnection) async throws -> AutomationRun? {
        guard let pathID = id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { throw PairingFailure.invalidLink }
        let runs: [AutomationRun] = try await model.api.request(
            "/api/v1/automations/\(pathID)/runs", origin: saved.origin, credential: saved.credential)
        return runs.first
    }

    /// Starts a run now. The Mac posts the task into its thread; a run that
    /// fails right away (no paired phone, thread unavailable) shows here.
    @MainActor private func runNow(_ item: AutomationItem) async {
        guard !changing.contains(item.id), let saved = model.connection,
              !model.accessEnded, model.macConnected == true else { return }
        let scope = model.assignmentScope
        changing.insert(item.id)
        changeProgress[item.id] = "Starting…"
        changeFailures[item.id] = nil
        runNotices[item.id] = nil
        // A list load already under way predates this run; let it go.
        runBoard.changed()
        var busy = true
        defer { if busy && scope == model.assignmentScope { changing.remove(item.id) } }
        do {
            guard let pathID = item.id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { throw PairingFailure.invalidLink }
            let body = try JSONEncoder().encode(AutomationRunRequest(clientRequestId: runBoard.runRequestID(for: item.id)))
            let run: AutomationRun
            do {
                run = try await model.api.request(
                    "/api/v1/automations/\(pathID)/run", origin: saved.origin, body: body, credential: saved.credential)
            } catch {
                if scope == model.assignmentScope { runBoard.settleRunRequest(for: item.id, failure: error) }
                throw error
            }
            if scope == model.assignmentScope { runBoard.settleRunRequest(for: item.id, failure: nil) }
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            guard run.automationId == item.id else { throw ReadFailure.resync }
            runBoard.record(run)
            runNotices[item.id] = "The reply appears in its thread."
            changing.remove(item.id); busy = false
            // A run the Mac cannot start fails within moments; show why.
            try await Task.sleep(for: .seconds(2))
            let latest = try await latestRun(item.id, saved)
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            if let latest, latest.id == run.id {
                runBoard.update(latest)
                if latest.status == "failed" { runNotices[item.id] = nil }
            }
        } catch is CancellationError {
        } catch {
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            if !busy { return } // The run started; only the recheck failed.
            changeFailures[item.id] = "Couldn’t run this automation. \(requestFailure(error))"
        }
    }

    @MainActor private func delete(_ item: AutomationItem) async {
        deleting = nil
        guard !changing.contains(item.id), let saved = model.connection,
              !model.accessEnded, model.macConnected == true else { return }
        let scope = model.assignmentScope
        changing.insert(item.id)
        changeProgress[item.id] = "Deleting…"
        changeFailures[item.id] = nil
        defer { if scope == model.assignmentScope { changing.remove(item.id) } }
        do {
            guard let pathID = item.id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { throw PairingFailure.invalidLink }
            let _: AutomationDeleted = try await model.api.request(
                "/api/v1/automations/\(pathID)", origin: saved.origin, credential: saved.credential, method: "DELETE")
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            items.removeAll { $0.id == item.id }
            runBoard.remove(item.id); runNotices[item.id] = nil
        } catch {
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            if case PairingFailure.response(404) = error {
                // Already gone on the Mac.
                items.removeAll { $0.id == item.id }
                runBoard.remove(item.id)
                return
            }
            changeFailures[item.id] = "Couldn’t delete this automation. \(requestFailure(error))"
        }
    }

    private func loadFailure(_ error: Error) -> String {
        let detail = requestFailure(error)
        return detail == "Try again." ? "Automations couldn’t be loaded." : detail
    }

    private func requestFailure(_ error: Error) -> String {
        if case PairingFailure.response(409) = error { return "The automation or its target changed. Refresh and try again." }
        if case PairingFailure.hostMessage(_, let detail) = error { return detail }
        if error is URLError { return "Couldn’t reach your computer. Check the connection and try again." }
        return "Try again."
    }
}

private struct AutomationProjectsResponse: Decodable {
    let projects: [AutomationProjectChoice]
}

private struct AutomationProjectChoice: Decodable, Identifiable {
    let id: String
    let name: String
    let isIncluded: Bool
}

private struct AutomationThreadsPage: Decodable {
    let threads: [AutomationThreadChoice]
    let nextCursor: String?
}

private struct AutomationThreadChoice: Decodable, Identifiable {
    let reference: String
    let conversationId: String?
    let title: String
    let family: String
    var id: String { conversationId ?? reference }
    var isSchedulable: Bool { conversationId != nil && !reference.hasPrefix("wonder:") }
    var label: String { "\(title) · \(AgentFamily(host: family).title)" }
}

enum AutomationRecurrence: String, CaseIterable, Identifiable {
    case daily, weekdays, weekly
    var id: String { rawValue }
    var title: String {
        switch self {
        case .daily: "Every day"
        case .weekdays: "Weekdays"
        case .weekly: "Every week"
        }
    }
}

/// A repeat the editor can show and change: every day, weekdays or one day a
/// week, at an hour and minute in the schedule's own time zone.
struct AutomationSchedule: Equatable {
    var recurrence: AutomationRecurrence
    var weekday: String
    var hour: Int
    var minute: Int

    static let weekdays: [(String, String)] = [
        ("MO", "Monday"), ("TU", "Tuesday"), ("WE", "Wednesday"),
        ("TH", "Thursday"), ("FR", "Friday"), ("SA", "Saturday"), ("SU", "Sunday")
    ]

    init(recurrence: AutomationRecurrence, weekday: String, hour: Int, minute: Int) {
        self.recurrence = recurrence; self.weekday = weekday; self.hour = hour; self.minute = minute
    }

    /// Nil for a repeat this screen can't change.
    init?(rrule: String) {
        let fields = rrule.split(separator: ";").compactMap { part -> (String, String)? in
            let pair = part.split(separator: "=", maxSplits: 1).map(String.init)
            return pair.count == 2 ? (pair[0], pair[1]) : nil
        }
        let values = Dictionary(fields, uniquingKeysWith: { _, new in new })
        guard let hour = values["BYHOUR"].flatMap(Int.init), (0...23).contains(hour),
              let minute = values["BYMINUTE"].flatMap(Int.init), (0...59).contains(minute) else { return nil }
        if values["FREQ"] == "DAILY", values["BYDAY"] == nil {
            self.init(recurrence: .daily, weekday: "MO", hour: hour, minute: minute)
        } else if values["FREQ"] == "WEEKLY", values["BYDAY"] == "MO,TU,WE,TH,FR" {
            self.init(recurrence: .weekdays, weekday: "MO", hour: hour, minute: minute)
        } else if values["FREQ"] == "WEEKLY", let day = values["BYDAY"], Self.weekdays.contains(where: { $0.0 == day }) {
            self.init(recurrence: .weekly, weekday: day, hour: hour, minute: minute)
        } else {
            return nil
        }
    }

    var rrule: String {
        let start: String
        switch recurrence {
        case .daily: start = "FREQ=DAILY"
        case .weekdays: start = "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"
        case .weekly: start = "FREQ=WEEKLY;BYDAY=\(weekday)"
        }
        return "\(start);BYHOUR=\(hour);BYMINUTE=\(minute)"
    }

    /// "every day at 9:00 AM", with the time as a clock in `zone` reads it.
    func summary(in zone: TimeZone, locale: Locale) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = zone
        // A January day: no daylight-saving change falls on it to skip an hour.
        let date = calendar.date(from: DateComponents(year: 2026, month: 1, day: 5, hour: hour, minute: minute)) ?? Date()
        let time = date.formatted(Date.FormatStyle(date: .omitted, time: .shortened, locale: locale, calendar: calendar, timeZone: zone))
        switch recurrence {
        case .daily: return "every day at \(time)"
        case .weekdays: return "on weekdays at \(time)"
        case .weekly:
            let day = Self.weekdays.first(where: { $0.0 == weekday })?.1 ?? "week"
            return "every \(day) at \(time)"
        }
    }

    /// How a saved rule repeats, in words, for a row: "Every day at 9:00 AM",
    /// "Every 30 minutes", "Once". Nil for a rule this phone doesn't describe.
    static func repeatText(rrule: String, timezone: String, locale: Locale = .current) -> String? {
        if rrule.hasSuffix(";COUNT=1") { return "Once" }
        if let schedule = AutomationSchedule(rrule: rrule) {
            let zone = TimeZone(identifier: timezone) ?? .current
            // A time on another computer's clock names its zone, as Next is in this phone's.
            let named = zone.identifier == TimeZone.current.identifier ? "" : ", \(zoneName(zone, locale: locale))"
            let text = schedule.summary(in: zone, locale: locale) + named
            return text.prefix(1).uppercased() + text.dropFirst()
        }
        let values = Dictionary(rrule.split(separator: ";").compactMap { part -> (String, String)? in
            let pair = part.split(separator: "=", maxSplits: 1).map(String.init)
            return pair.count == 2 ? (pair[0], pair[1]) : nil
        }, uniquingKeysWith: { _, new in new })
        let interval = values["INTERVAL"].flatMap(Int.init) ?? 1
        switch values["FREQ"] {
        case "MINUTELY": return interval == 1 ? "Every minute" : "Every \(interval) minutes"
        case "HOURLY": return interval == 1 ? "Every hour" : "Every \(interval) hours"
        case "MONTHLY": return values["BYMONTHDAY"].map { "Monthly, on day \($0)" } ?? "Every month"
        default: return nil
        }
    }

    /// The time zone as people name it ("Central European Time"), not its ID.
    static func zoneName(_ zone: TimeZone, locale: Locale = .current) -> String {
        zone.localizedName(for: .generic, locale: locale) ?? zone.identifier
    }

    /// What the editor says about a saved schedule it keeps as it is, or nil
    /// when the schedule uses this device's time zone and can be changed in
    /// place. A schedule set in another time zone keeps that zone until the
    /// owner replaces it with Change schedule.
    static func savedNote(rrule: String, timezone: String, current: TimeZone = .current, locale: Locale = .current) -> String? {
        let schedule = AutomationSchedule(rrule: rrule)
        guard timezone != current.identifier || schedule == nil else { return nil }
        let zone = TimeZone(identifier: timezone)
        let zoneText = zone.map { zoneName($0, locale: locale) } ?? timezone
        guard let schedule else {
            let place = timezone == current.identifier ? "" : " It runs on \(zoneText)."
            return "This schedule repeats in a way this screen can’t change.\(place) Its timing stays the same when you save."
        }
        return "Runs \(schedule.summary(in: zone ?? current, locale: locale)), \(zoneText). Saving keeps that time zone; "
            + "Change schedule to use \(zoneName(current, locale: locale)) instead."
    }
}

private struct AutomationWrite: Encodable {
    let name: String
    let prompt: String
    let rrule: String
    let timezone: String
    let status: String
    let notificationPolicy: String
    let kind: String?
    let scopeType: String?
    let scopeId: String?
    let conversationId: String?
    let clientRequestId: String?
    let expectedRevision: Int64?
}

private struct ProjectAutomationEditor: View {
    @ObservedObject var model: ConnectionModel
    let item: AutomationItem?
    let targetLabel: String?
    let onSaved: (AutomationItem, String?) -> Void
    let onReload: () -> Void

    @State private var name: String
    @State private var prompt: String
    @State private var isActive: Bool
    @State private var notificationPolicy: String
    @State private var recurrence: AutomationRecurrence
    @State private var weekday: String
    @State private var time: Date
    @State private var scheduleEditable: Bool
    @State private var projects: [AutomationProjectChoice] = []
    @State private var projectID = ""
    @State private var threads: [AutomationThreadChoice] = []
    @State private var threadID = ""
    @State private var nextCursor: String?
    @State private var loadingProjects = false
    @State private var loadingThreads = false
    @State private var threadLoadToken = UUID()
    @State private var targetFailure: String?
    @State private var saveFailure: String?
    @State private var saveConflict = false
    @State private var createConflict = false
    @State private var saving = false
    @State private var requestID = UUID().uuidString
    /// Why a saved schedule is kept as it is (another time zone, or a repeat
    /// this screen can't change); nil when it can be changed in place.
    private let savedScheduleNote: String?
    private let localZoneName = AutomationSchedule.zoneName(.current)

    init(model: ConnectionModel, item: AutomationItem?, targetLabel: String?,
         onSaved: @escaping (AutomationItem, String?) -> Void, onReload: @escaping () -> Void) {
        self.model = model; self.item = item; self.targetLabel = targetLabel
        self.onSaved = onSaved; self.onReload = onReload
        _name = State(initialValue: item?.name ?? "")
        _prompt = State(initialValue: item?.prompt ?? "")
        _isActive = State(initialValue: item?.status != "paused")
        _notificationPolicy = State(initialValue: item?.notificationPolicy ?? "all_runs")
        // Change schedule starts from the saved repeat and clock time.
        let schedule = item.flatMap { AutomationSchedule(rrule: $0.rrule) }
            ?? AutomationSchedule(recurrence: .daily, weekday: "MO", hour: 9, minute: 0)
        _recurrence = State(initialValue: schedule.recurrence)
        _weekday = State(initialValue: schedule.weekday)
        _time = State(initialValue: Calendar.current.date(bySettingHour: schedule.hour, minute: schedule.minute, second: 0, of: Date()) ?? Date())
        let note = item.flatMap { AutomationSchedule.savedNote(rrule: $0.rrule, timezone: $0.timezone) }
        savedScheduleNote = note
        _scheduleEditable = State(initialValue: item == nil || note == nil)
    }

    var body: some View {
        WonderForm {
            Section("Project thread") {
                if let item {
                    Text(targetLabel ?? "Project thread")
                        .accessibilityIdentifier("project-automation-target")
                    if item.scopeId.isEmpty { Text("This thread is unavailable.").foregroundStyle(.secondary) }
                } else {
                    if loadingProjects { ProgressView("Loading Projects…") }
                    if computerIsOffline(model) { ComputerOfflineRow(name: model.macName) }
                    if let targetFailure {
                        Text(targetFailure).foregroundStyle(.red)
                            .accessibilityIdentifier("project-automation-target-failure")
                        Button("Try again") { Task {
                            if projects.isEmpty { await loadProjects() }
                            else { await loadThreads(more: false) }
                        } }
                            .accessibilityIdentifier("project-automation-target-retry")
                    }
                    if projects.isEmpty && !loadingProjects && targetFailure == nil && model.macConnected == true {
                        Text("Add a Project and start a chat in it, then schedule it here.")
                            .foregroundStyle(.secondary)
                    }
                    if !projects.isEmpty {
                        Picker("Project", selection: $projectID) {
                            ForEach(projects) { project in Text(project.name).tag(project.id) }
                        }
                        .accessibilityIdentifier("project-automation-project")
                        .task(id: projectID) {
                            guard !projectID.isEmpty else { return }
                            threadLoadToken = UUID()
                            loadingThreads = false
                            threads = []; threadID = ""; nextCursor = nil
                            await loadThreads(more: false)
                        }
                        if loadingThreads { ProgressView("Loading threads…") }
                        if !threads.isEmpty {
                            Picker("Thread", selection: $threadID) {
                                Text("Choose a thread").tag("")
                                ForEach(threads.filter(\.isSchedulable)) { thread in
                                    if let id = thread.conversationId { Text(thread.label).tag(id) }
                                }
                            }
                            .accessibilityIdentifier("project-automation-thread")
                        } else if !loadingThreads {
                            Text("Start a chat in this Project first, then schedule it here.")
                                .foregroundStyle(.secondary)
                        }
                        if nextCursor != nil {
                            Button("Show more threads") { Task { await loadThreads(more: true) } }
                                .disabled(loadingThreads)
                                .accessibilityIdentifier("project-automation-more-threads")
                        }
                    }
                }
            }
            Section("Task") {
                TextField("Name", text: $name)
                    .accessibilityIdentifier("project-automation-name")
                TextField("What should the agent do?", text: $prompt, axis: .vertical)
                    .lineLimit(3...6)
                    .accessibilityIdentifier("project-automation-prompt")
            }
            Section("Schedule") {
                if scheduleEditable {
                    Picker("Repeat", selection: $recurrence) {
                        ForEach(AutomationRecurrence.allCases) { option in Text(option.title).tag(option) }
                    }
                    .accessibilityIdentifier("project-automation-repeat")
                    if recurrence == .weekly {
                        Picker("Day", selection: $weekday) {
                            ForEach(AutomationSchedule.weekdays, id: \.0) { day in Text(day.1).tag(day.0) }
                        }
                        .accessibilityIdentifier("project-automation-day")
                    }
                    DatePicker("Time", selection: $time, displayedComponents: .hourAndMinute)
                        .accessibilityIdentifier("project-automation-time")
                    Text("Times use \(localZoneName).")
                        .font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("project-automation-zone")
                } else {
                    Text(savedScheduleNote ?? "")
                        .font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("project-automation-saved-schedule")
                    Button("Change schedule") { scheduleEditable = true }
                        .accessibilityIdentifier("project-automation-change-schedule")
                }
                Toggle("Active", isOn: $isActive).systemSwitch()
                    .accessibilityIdentifier("project-automation-active")
                Picker("Notify me", selection: $notificationPolicy) {
                    Text("Every run").tag("all_runs")
                    Text("Failures only").tag("failed_runs_only")
                }
                .accessibilityIdentifier("project-automation-notifications")
            }
        }
        .disabled(saving)
        .safeAreaInset(edge: .top) { failureBanner }
        .navigationTitle(item == nil ? "New automation" : "Edit automation")
        .navigationBarTitleDisplayMode(.inline)
        // Back leaves without saving; it waits while a save is in flight.
        .navigationBarBackButtonHidden(saving)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button("Save") { Task { await save() } }
                    .disabled(!canSave)
                    .accessibilityIdentifier("project-automation-save")
            }
        }
        .task(id: model.assignmentScope) {
            saving = false
            loadingProjects = false
            loadingThreads = false
            threadLoadToken = UUID()
            projects = []; projectID = ""; threads = []; threadID = ""; nextCursor = nil
            if item == nil { await loadProjects() }
        }
        .onChange(of: model.macConnected) { _, connected in
            if connected == true, item == nil, projects.isEmpty { Task { await loadProjects() } }
        }
    }

    @ViewBuilder private var failureBanner: some View {
        if let saveFailure {
            VStack(alignment: .leading, spacing: 6) {
                Text(saveFailure)
                    .font(.subheadline)
                    .foregroundStyle(.red)
                    .accessibilityIdentifier("project-automation-save-failure")
                if saveConflict {
                    Button("Reload latest version") { onReload() }
                        .accessibilityIdentifier("project-automation-reload")
                    Text("Your draft will be replaced with the version on your computer.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if createConflict {
                    Button("Keep draft for a new task") {
                        requestID = UUID().uuidString
                        createConflict = false
                        self.saveFailure = nil
                    }
                    .accessibilityIdentifier("project-automation-new-request")
                    Text("The first request may have created a task. Check the list before saving another.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(.regularMaterial)
        }
    }

    private var canSave: Bool {
        !saving && model.macConnected == true && !model.accessEnded
            && !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && (item != nil || !threadID.isEmpty)
    }

    private var rrule: String {
        if !scheduleEditable, let item { return item.rrule }
        return AutomationSchedule(recurrence: recurrence, weekday: weekday,
                                  hour: Calendar.current.component(.hour, from: time),
                                  minute: Calendar.current.component(.minute, from: time)).rrule
    }

    @MainActor private func loadProjects() async {
        guard !loadingProjects, let saved = model.connection, model.macConnected == true, !model.accessEnded else { return }
        let scope = model.assignmentScope
        loadingProjects = true; targetFailure = nil
        defer { if scope == model.assignmentScope { loadingProjects = false } }
        do {
            let response: AutomationProjectsResponse = try await model.api.request(
                "/api/v1/projects", origin: saved.origin, credential: saved.credential)
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin, !Task.isCancelled else { return }
            projects = response.projects.filter(\.isIncluded).sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            if projectID.isEmpty, let first = projects.first { projectID = first.id }
        } catch {
            guard scope == model.assignmentScope, !Task.isCancelled else { return }
            targetFailure = model.macConnected == true ? "Projects couldn’t be loaded. Check your computer connection and try again." : nil
        }
    }

    @MainActor private func loadThreads(more: Bool) async {
        guard !loadingThreads, !projectID.isEmpty, let saved = model.connection,
              model.macConnected == true, !model.accessEnded else { return }
        let scope = model.assignmentScope
        let requestedProject = projectID
        let token = threadLoadToken
        let cursor = more ? nextCursor : nil
        loadingThreads = true; targetFailure = nil
        defer { if scope == model.assignmentScope, threadLoadToken == token { loadingThreads = false } }
        guard let escaped = requestedProject.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else { return }
        var path = "/api/v1/projects/\(escaped)/threads?limit=30"
        if let cursor, let value = cursor.addingPercentEncoding(withAllowedCharacters: .alphanumerics) { path += "&cursor=\(value)" }
        do {
            let page: AutomationThreadsPage = try await model.api.request(path, origin: saved.origin, credential: saved.credential)
            guard scope == model.assignmentScope, threadLoadToken == token, projectID == requestedProject,
                  model.connection?.origin == saved.origin, !Task.isCancelled else { return }
            if !more { threads = [] }
            let existing = Set(threads.map(\.id))
            threads += page.threads.filter { $0.isSchedulable && !existing.contains($0.id) }
            nextCursor = page.nextCursor
            if threadID.isEmpty, threads.count == 1, nextCursor == nil {
                threadID = threads[0].conversationId ?? ""
            }
        } catch {
            guard scope == model.assignmentScope, threadLoadToken == token,
                  projectID == requestedProject, !Task.isCancelled else { return }
            targetFailure = model.macConnected == true ? "Project threads couldn’t be loaded. Try again." : nil
        }
    }

    @MainActor private func save() async {
        guard canSave, let saved = model.connection else { return }
        let scope = model.assignmentScope
        let selectedProject = projectID
        let selectedThread = item?.scopeId ?? threadID
        let selectedLabel = item == nil ? projects.first(where: { $0.id == selectedProject }).flatMap { project in
            threads.first(where: { $0.conversationId == selectedThread }).map { "\(project.name) · \($0.title)" }
        } : targetLabel
        saving = true; saveFailure = nil; saveConflict = false; createConflict = false
        defer { if scope == model.assignmentScope { saving = false } }
        do {
            let body = try JSONEncoder().encode(AutomationWrite(
                name: name.trimmingCharacters(in: .whitespacesAndNewlines),
                prompt: prompt.trimmingCharacters(in: .whitespacesAndNewlines), rrule: rrule,
                timezone: scheduleEditable ? TimeZone.current.identifier : (item?.timezone ?? TimeZone.current.identifier),
                status: isActive ? "active" : "paused", notificationPolicy: notificationPolicy,
                kind: item == nil ? "continuation" : nil,
                scopeType: item == nil ? "project_thread" : nil,
                scopeId: item == nil ? selectedThread : nil,
                conversationId: item == nil ? selectedThread : nil,
                clientRequestId: item == nil ? requestID : nil,
                expectedRevision: item?.revision))
            let path = item.map { "/api/v1/automations/\(ConnectionModel.escape($0.id))" } ?? "/api/v1/automations"
            let updated: AutomationItem = try await model.api.request(path, origin: saved.origin, body: body,
                credential: saved.credential, method: item == nil ? "POST" : "PATCH")
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            guard updated.scopeType == "project_thread", updated.scopeId == selectedThread else {
                throw ReadFailure.resync
            }
            onSaved(updated, selectedLabel)
        } catch {
            guard scope == model.assignmentScope, model.connection?.origin == saved.origin,
                  !model.accessEnded, !Task.isCancelled else { return }
            if case PairingFailure.response(409) = error {
                if item == nil {
                    createConflict = true
                    saveFailure = "This request may already have created an automation, or the Project thread changed. Your draft is still here."
                } else {
                    saveConflict = true
                    saveFailure = "This automation or Project thread changed. Reload the latest version to continue."
                }
            } else if case PairingFailure.response(404) = error {
                saveFailure = "This automation or Project thread is no longer available."
            } else {
                saveFailure = "Couldn’t save this automation. Check your computer connection and try again."
            }
        }
    }
}

#if WONDER_DIAGNOSTICS
/// An isolated, owner-authenticated transport fixture for iPhone and iPad UI checks.
struct DiagnosticAutomationsFixtureView: View {
    @StateObject private var model: ConnectionModel

    init() {
        let credential = try! JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"automation-fixture-session","deviceId":"automation-fixture-device","csrfToken":"automation-fixture-csrf","hostInstallationId":"automation-fixture-host"}"#.utf8))
        let saved = SavedConnection(origin: "https://automations.invalid", credential: credential, hostName: "Fixture Mac")
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [DiagnosticAutomationURLProtocol.self]
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("wonder-automations-fixture-\(UUID().uuidString)")
        _model = StateObject(wrappedValue: ConnectionModel(cameraFixtureStoreRoot: root, saved: saved,
            api: PairingAPI(configuration: configuration), replayEnabled: false))
    }

    /// Which chat Open chat asked for, as host installation and conversation.
    @State private var opened: String?

    var body: some View {
        NavigationStack { // theme-exempt: Diagnostics fixture host
            AutomationsView(models: [model]) { host, conversation in opened = "\(host) · \(conversation)" }
                .safeAreaInset(edge: .bottom) {
                    if let opened {
                        Text("Opened \(opened)").font(.caption)
                            .accessibilityIdentifier("automations-fixture-opened")
                    }
                }
        }
    }
}

private final class DiagnosticAutomationURLProtocol: URLProtocol, @unchecked Sendable {
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var morningStatus = "active"
        var morningRevision = 0
        var projectCreated = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-existing-project")
        var projectRevision = 0
        var projectStatus = "active"
        var projectName = "Project check-in"
        var projectPrompt = "Review the Project progress."
        var projectRRule = "FREQ=DAILY;BYHOUR=9;BYMINUTE=0"
        /// Saved on another computer's clock: a zone this device isn't in.
        var projectTimezone = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-other-zone")
            ? (TimeZone.current.identifier == "Asia/Tokyo" ? "Europe/Paris" : "Asia/Tokyo") : TimeZone.current.identifier
        var projectNotifications = "all_runs"
        var createdRequestID: String?
        var lostCreateResponseRemaining = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-create-lost-response") ? 1 : 0
        var listFailuresRemaining = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-list-fails") ? 1 : 0
        var patchFailuresRemaining = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-patch-fails") ? 1 : 0
        var projectSaveFailuresRemaining = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-project-save-fails") ? 1 : 0
        var staleMorningPatchesRemaining = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-revision-conflict") ? 1 : 0
        var staleProjectPatchesRemaining = ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-revision-conflict") ? 1 : 0
        var morningDeleted = false
        /// The newest run's status per schedule once Run Now was tapped.
        var startedRuns: [String: String] = [:]
    }
    /// Takes the whole launch flag so `wonderctl fixtures` can find it in source.
    private static func flag(_ name: String) -> Bool { ProcessInfo.processInfo.arguments.contains(name) }
    private static let state = State()
    /// Holds thread names and recent runs well past when the list shows, so a
    /// check can see rows before their details (under the 15 s request timeout).
    private static let detailsDelay: TimeInterval = flag("-diagnostics-automations-slow-details") ? 12 : 0

    /// Answers after `detailsDelay`; at once without the flag.
    private func respondDetail(json: String) {
        guard Self.detailsDelay > 0 else { respond(json: json); return }
        DispatchQueue.global().asyncAfter(deadline: .now() + Self.detailsDelay) { [weak self] in self?.respond(json: json) }
    }

    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "automations.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url else { respond(status: 400); return }
        let path = url.path
        let method = request.httpMethod ?? "GET"
        guard request.value(forHTTPHeaderField: "Cookie") == "__Host-wonder_session=automation-fixture-session",
              request.value(forHTTPHeaderField: "x-wonder-csrf") == "automation-fixture-csrf" else {
            respond(status: 401); return
        }
        if method == "GET" && path == "/api/v1/projects" {
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-switch-project") ||
                ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-slow-create") {
                respond(json: #"{"projects":[{"id":"fixture-project","name":"Roadmap","isIncluded":true},{"id":"fixture-project-two","name":"Zephyr","isIncluded":true}]}"#)
            } else {
                respond(json: #"{"projects":[{"id":"fixture-project","name":"Roadmap","isIncluded":true}]}"#)
            }
        } else if method == "GET" && path == "/api/v1/projects/fixture-project/threads" {
            let response = #"{"threads":[{"reference":"codex:fixture-native-thread","conversationId":"fixture-project-thread","title":"Plan launch","family":"codex"},{"reference":"wonder:draft","conversationId":"fixture-draft","title":"Unstarted draft","family":"codex"}],"nextCursor":null}"#
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-switch-project") {
                DispatchQueue.global().asyncAfter(deadline: .now() + 3) { [weak self] in self?.respond(json: response) }
            } else { respond(json: response) }
        } else if method == "GET" && path == "/api/v1/projects/fixture-project-two/threads" {
            respond(json: #"{"threads":[{"reference":"codex:zephyr-thread","conversationId":"zephyr-project-thread","title":"Check Zephyr","family":"codex"}],"nextCursor":null}"#)
        } else if method == "GET" && path == "/api/v1/project-conversations/fixture-project-thread" {
            respondDetail(json: #"{"projectName":"Roadmap","title":"Plan launch"}"#)
        } else if method == "GET" && path == "/api/v1/automations" {
            let response = Self.state.lock.withLock { () -> String? in
                if Self.state.listFailuresRemaining > 0 { Self.state.listFailuresRemaining -= 1; return nil }
                return Self.listJSON(Self.state)
            }
            if let response { respond(json: response) } else { respond(status: 503) }
        } else if method == "POST" && path == "/api/v1/automations" {
            guard let body = requestBody(), let fields = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any],
                  fields["scopeType"] as? String == "project_thread",
                  fields["scopeId"] as? String == "fixture-project-thread",
                  fields["conversationId"] as? String == "fixture-project-thread",
                  let name = fields["name"] as? String, let prompt = fields["prompt"] as? String,
                  let rrule = fields["rrule"] as? String, let timezone = fields["timezone"] as? String,
                  let status = fields["status"] as? String,
                  let notifications = fields["notificationPolicy"] as? String else { respond(status: 400); return }
            let result = Self.state.lock.withLock { () -> (Int, String?) in
                if Self.state.projectSaveFailuresRemaining > 0 {
                    Self.state.projectSaveFailuresRemaining -= 1; return (503, nil)
                }
                let requestID = fields["clientRequestId"] as? String
                if Self.state.projectCreated, requestID == Self.state.createdRequestID,
                   Self.state.createdRequestID != nil {
                    return Self.state.projectName == name && Self.state.projectPrompt == prompt
                        ? (200, Self.projectJSON(Self.state)) : (409, nil)
                }
                Self.state.projectCreated = true; Self.state.projectRevision = 0
                Self.state.createdRequestID = requestID
                Self.state.projectName = name; Self.state.projectPrompt = prompt
                Self.state.projectRRule = rrule; Self.state.projectTimezone = timezone
                Self.state.projectStatus = status; Self.state.projectNotifications = notifications
                if Self.state.lostCreateResponseRemaining > 0 {
                    Self.state.lostCreateResponseRemaining -= 1
                    return (503, nil)
                }
                return (201, Self.projectJSON(Self.state))
            }
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-automations-slow-create") {
                DispatchQueue.global().asyncAfter(deadline: .now() + 2) { [weak self] in
                    guard let self else { return }
                    if let body = result.1 { self.respond(status: result.0, data: Data(body.utf8)) }
                    else { self.respond(status: result.0) }
                }
            } else if let body = result.1 { respond(status: result.0, data: Data(body.utf8)) }
            else { respond(status: result.0) }
        } else if method == "GET" && path.hasPrefix("/api/v1/automations/") && path.hasSuffix("/runs") {
            let id = String(path.dropFirst("/api/v1/automations/".count).dropLast("/runs".count))
            let runs = Self.state.lock.withLock { () -> String in
                if let status = Self.state.startedRuns[id] { return "[\(Self.runJSON(id: id, run: "fixture-run-now", status: status))]" }
                guard id == "fixture-morning-schedule" else { return "[]" }
                return "[\(Self.runJSON(id: id, run: "fixture-run-1", status: Self.flag("-diagnostics-automations-failed-run") ? "failed" : "completed"))]"
            }
            respondDetail(json: runs)
        } else if method == "POST" && path.hasPrefix("/api/v1/automations/") && path.hasSuffix("/run") {
            let id = String(path.dropFirst("/api/v1/automations/".count).dropLast("/run".count))
            if Self.flag("-diagnostics-automations-run-refused") {
                respond(status: 503, data: Data("The selected agent is offline. Connect the computer it runs on, then try again.".utf8)); return
            }
            Self.state.lock.withLock { Self.state.startedRuns[id] = "running" }
            respond(status: 202, data: Data(Self.runJSON(id: id, run: "fixture-run-now", status: "running").utf8))
        } else if method == "DELETE" && path.hasPrefix("/api/v1/automations/") {
            let id = String(path.dropFirst("/api/v1/automations/".count))
            if Self.flag("-diagnostics-automations-delete-refused") {
                respond(status: 409, data: Data("Wait for this automation to finish, or stop its run in the conversation before deleting it.".utf8)); return
            }
            Self.state.lock.withLock {
                if id == "fixture-morning-schedule" { Self.state.morningDeleted = true }
                if id == "fixture-project-schedule" { Self.state.projectCreated = false }
            }
            respond(status: 204)
        } else if method == "PATCH" && path.hasPrefix("/api/v1/automations/") {
            let id = String(path.dropFirst("/api/v1/automations/".count))
            let fields = (requestBody().flatMap { try? JSONSerialization.jsonObject(with: $0) } as? [String: Any]) ?? [:]
            guard id == "fixture-morning-schedule" || id == "fixture-project-schedule" else {
                respond(status: 400); return
            }
            let response = Self.state.lock.withLock { () -> (Int, String?) in
                if id == "fixture-morning-schedule", Self.state.staleMorningPatchesRemaining > 0 {
                    Self.state.staleMorningPatchesRemaining -= 1
                    Self.state.morningRevision += 1
                }
                if id == "fixture-project-schedule", Self.state.staleProjectPatchesRemaining > 0 {
                    Self.state.staleProjectPatchesRemaining -= 1
                    Self.state.projectRevision += 1
                    Self.state.projectName = "Project updated on Mac"
                    Self.state.projectPrompt = "Updated on Mac."
                }
                let revision = id == "fixture-project-schedule" ? Self.state.projectRevision : Self.state.morningRevision
                if let expected = fields["expectedRevision"] as? Int, expected != revision {
                    return (409, nil)
                }
                if id == "fixture-project-schedule" {
                    guard Self.state.projectCreated else { return (404, nil) }
                    if Self.state.projectSaveFailuresRemaining > 0 {
                        Self.state.projectSaveFailuresRemaining -= 1; return (503, nil)
                    }
                    if let name = fields["name"] as? String { Self.state.projectName = name }
                    if let prompt = fields["prompt"] as? String { Self.state.projectPrompt = prompt }
                    if let rrule = fields["rrule"] as? String { Self.state.projectRRule = rrule }
                    if let timezone = fields["timezone"] as? String { Self.state.projectTimezone = timezone }
                    if let notifications = fields["notificationPolicy"] as? String { Self.state.projectNotifications = notifications }
                    if let status = fields["status"] as? String { Self.state.projectStatus = status }
                    Self.state.projectRevision += 1
                    return (200, Self.projectJSON(Self.state))
                }
                guard let requested = fields["status"] as? String,
                      requested == "active" || requested == "paused" else { return (400, nil) }
                if Self.state.patchFailuresRemaining > 0 { Self.state.patchFailuresRemaining -= 1; return (503, nil) }
                if id == "fixture-morning-schedule" {
                    Self.state.morningStatus = requested; Self.state.morningRevision += 1
                    return (200, Self.itemJSON(id: id, status: requested, revision: Self.state.morningRevision))
                }
                return (404, nil)
            }
            if let body = response.1 { respond(json: body) } else { respond(status: response.0) }
        } else {
            respond(status: 404)
        }
    }

    override func stopLoading() {}

    private func requestBody() -> Data? {
        if let data = request.httpBody { return data }
        guard let stream = request.httpBodyStream else { return nil }
        stream.open()
        defer { stream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            guard count > 0 else { break }
            data.append(contentsOf: buffer.prefix(count))
        }
        return data
    }

    private static func listJSON(_ state: State) -> String {
        var entries = state.morningDeleted ? [] : [itemJSON(id: "fixture-morning-schedule", status: state.morningStatus, revision: state.morningRevision)]
        if state.projectCreated { entries.append(projectJSON(state)) }
        // What wonder_schedule saves: the maker chat is in the ID, the rule is the agent's.
        if flag("-diagnostics-automations-from-chat") {
            for (suffix, name, rule) in [("a1", "Check CI", "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0"),
                                         ("b2", "Look at deploys", "FREQ=HOURLY;INTERVAL=2;BYMINUTE=30")] {
                entries.append(#"{"id":"agent-automation:fixture-project-thread:\#(suffix)","revision":0,"name":"\#(name)","scopeType":"project_thread","scopeId":"fixture-project-thread","prompt":"Summarise what changed.","rrule":"\#(rule)","timezone":"UTC","notificationPolicy":"all_runs","status":"active","nextRunAt":"2026-10-30T09:00:00.000Z","lastRunAt":null,"lastAttemptAt":null}"#)
            }
        }
        return "[\(entries.joined(separator: ","))]"
    }

    private static func itemJSON(id: String, status: String, revision: Int) -> String {
        return """
        {"id":"\(id)","revision":\(revision),"name":"Morning brief","scopeType":"project_thread","scopeId":"fixture-project-thread","prompt":"Check progress","rrule":"FREQ=DAILY;BYHOUR=9;BYMINUTE=0","timezone":"UTC","notificationPolicy":"all_runs","status":"\(status)","nextRunAt":\(status == "paused" ? "null" : "\"2026-10-03T09:00:00.000Z\""),"lastRunAt":"2026-10-01T09:00:00.000Z","lastAttemptAt":"2026-10-01T09:00:00.000Z"}
        """
    }

    private static func runJSON(id: String, run: String, status: String) -> String {
        let error = status == "failed" ? #""This automation needs a paired phone or iPad. Pair one again with cupola pair on your computer.""# : "null"
        return #"{"id":"\#(run)","automationId":"\#(id)","scheduledFor":"manual","status":"\#(status)","startedAt":"2026-10-01T09:00:00.000Z","finishedAt":null,"error":\#(error),"messageId":null,"conversationId":"fixture-project-thread"}"#
    }

    private static func projectJSON(_ state: State) -> String {
        let fields: [String: Any] = [
            "id": "fixture-project-schedule", "revision": state.projectRevision,
            "name": state.projectName, "scopeType": "project_thread",
            "scopeId": "fixture-project-thread", "prompt": state.projectPrompt, "rrule": state.projectRRule,
            "timezone": state.projectTimezone, "notificationPolicy": state.projectNotifications,
            "status": state.projectStatus, "nextRunAt": state.projectStatus == "paused" ? NSNull() : "2026-10-03T09:00:00.000Z",
            "lastRunAt": NSNull(), "lastAttemptAt": state.startedRuns["fixture-project-schedule"] == nil ? NSNull() : "2026-10-01T09:00:00.000Z"
        ]
        let data = try! JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
        return String(decoding: data, as: UTF8.self)
    }

    private func respond(json: String) { respond(status: 200, data: Data(json.utf8)) }
    private func respond(status: Int, data: Data = Data()) {
        guard let url = request.url,
              let response = HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1",
                headerFields: ["Content-Type": "application/json"]) else { return }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }
}
#endif
