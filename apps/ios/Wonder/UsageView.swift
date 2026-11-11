import SwiftUI
import WonderPairing

/// How fast a limit window is being spent compared with the time that has
/// passed in it. Positive `delta` means ahead of pace: using it up faster than
/// the window allows.
struct UsagePace: Equatable {
    let delta: Int

    init?(window: CodexUsageWindow, now: Date = Date()) {
        guard let minutes = window.windowDurationMins, minutes > 0, let reset = window.resetDate, reset > now else { return nil }
        let total = Double(minutes) * 60
        let elapsed = min(max(1 - reset.timeIntervalSince(now) / total, 0), 1)
        // The provider reports the percent; keep a wild value from trapping Int().
        delta = Int((min(max(window.usedPercent, 0), 100) - elapsed * 100).rounded())
    }

    var text: String {
        if abs(delta) < 2 { return "On pace" }
        return delta > 0 ? "\(delta)% ahead of pace" : "\(-delta)% under pace"
    }
    var isAhead: Bool { delta >= 2 }
}

enum UsageReset {
    /// "2h 5m", "3d 4h" or "12m"; nil when the provider named no reset time.
    static func countdown(_ window: CodexUsageWindow, now: Date = Date()) -> String? {
        guard let reset = window.resetDate else { return nil }
        let seconds = Int(reset.timeIntervalSince(now))
        guard seconds > 0 else { return "Resetting now" }
        let minutes = max(seconds / 60, 1), hours = minutes / 60, days = hours / 24
        let text: String
        if days > 0 { text = hours % 24 == 0 ? "\(days)d" : "\(days)d \(hours % 24)h" }
        else if hours > 0 { text = minutes % 60 == 0 ? "\(hours)h" : "\(hours)h \(minutes % 60)m" }
        else { text = "\(minutes)m" }
        return "Resets in \(text)"
    }
}

/// One provider account's limits, merged across every computer that uses it.
private struct UsageAccount: Identifiable {
    let id: String
    let family: AgentFamily
    let label: String?
    let plan: String?
    let windows: [CodexUsageWindow]
}

/// A provider with no limits to show yet; one per provider, not per computer.
private struct UsageProblem: Identifiable {
    enum Kind: Equatable { case loading, failed(String, retryable: Bool) }
    let id: String
    let family: AgentFamily
    let kind: Kind
}

/// What a failed usage read says, and whether trying again could help.
private enum UsageFailure {
    static func describe(_ error: Error, family: AgentFamily) -> (text: String, retryable: Bool) {
        if case PairingFailure.response(501) = error {
            return ("This \(family.title) version doesn’t provide usage details in Cupola.", false)
        }
        if case PairingFailure.response(503) = error {
            return (family == .codex // provider identity: names the provider sign-in to check
                ? "Codex usage couldn’t be verified on this computer. Check Codex sign-in."
                : "Claude usage couldn’t be loaded. Check Claude’s sign-in on your computer.", true)
        }
        return (family == .codex ? "Usage couldn’t be loaded." : "Claude usage couldn’t be loaded. Check Claude’s sign-in on your computer.", true) // provider identity
    }
    static let accessEnded = "Access ended. Reconnect this computer to check usage."
    @MainActor static func isUnreachable(_ error: Error, _ model: ConnectionModel) -> Bool {
        error is URLError || model.macConnected == false
    }
}

/// Provider limits per account, not per computer.
/// The host reports limit windows only, so there are no Cost or Tokens tabs.
struct UsageView: View {
    @ObservedObject var library: ConnectionLibrary
    @StateObject private var accounts = UsageAccountsStore()
    /// Account emails stay masked until tapped; leaving the page or the app masks them again.
    @State private var revealed: Set<String> = []
    @Environment(\.scenePhase) private var scenePhase

    @ViewBuilder private func accountHeader(_ account: UsageAccount) -> some View {
        let identifier = "\(account.family.rawValue)-usage-section"
        if let label = account.label, UsageMask.isEmail(label) {
            let shown = revealed.contains(account.id)
            Button {
                if shown { revealed.remove(account.id) } else { revealed.insert(account.id) }
            } label: {
                UsagePill(text: [account.family.title, shown ? label : UsageMask.email(label)].joined(separator: " · "), plan: account.plan)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(account.family.title)
            .accessibilityValue(shown ? label : UsageMask.email(label))
            .accessibilityHint(shown ? "Hides the email" : "Shows the email")
            .accessibilityIdentifier(identifier)
        } else {
            UsagePill(text: [account.family.title, account.label].compactMap { $0 }.joined(separator: " · "), plan: account.plan)
                .accessibilityIdentifier(identifier)
        }
    }

    var body: some View {
        let connections = library.saved.connections
        WonderForm {
            if connections.isEmpty {
                Section { Text("Pair a computer to see its usage.").foregroundStyle(.secondary) }
            }
            ForEach(accounts.accounts) { account in
                Section {
                    ForEach(account.windows) { window in
                        UsageWindowRow(window: window, identifier: "\(account.family.rawValue)-usage-window:\(window.id)")
                    }
                } header: {
                    accountHeader(account)
                }
            }
            ForEach(accounts.problems) { problem in
                Section {
                    switch problem.kind {
                    case .loading:
                        ProgressView("Loading usage…").accessibilityIdentifier("\(problem.family.rawValue)-usage-loading")
                    case .failed(let text, let retryable):
                        Text(text).foregroundStyle(.secondary)
                        if retryable {
                            Button("Try again") { Task { await accounts.retry(problem.id) } }
                                .accessibilityIdentifier("\(problem.family.rawValue)-usage-retry")
                        }
                    }
                } header: {
                    UsagePill(text: problem.family.title, plan: nil)
                        .accessibilityIdentifier("\(problem.family.rawValue)-usage-section")
                }
            }
            if let note = accounts.note {
                Section { Text(note).foregroundStyle(.secondary) }
            }
        }
        .settingsNavigationTitle("Usage")
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("usage-page")
        .onDisappear { revealed = [] }
        .onChange(of: scenePhase) { _, phase in if phase != .active { revealed = [] } }
        .task(id: connections.map(\.credential.hostInstallationId)) {
            let models = connections.map { library.model(for: $0) }
            accounts.begin(models: models)
            await UsageAutoRefresh.run { force in await accounts.refresh(force: force) }
        }
    }
}

@MainActor private final class UsageAccountsStore: ObservableObject {
    @Published var accounts: [UsageAccount] = []
    @Published var problems: [UsageProblem] = []
    @Published var note: String?

    private struct Source { let model: ConnectionModel; let family: AgentFamily; var failure: (text: String, retryable: Bool)?; var loading = false }
    private var models: [ConnectionModel] = []
    private var sources: [String: Source] = [:]
    private var order: [String] = []
    /// Bumped when the set of computers changes, so a slow old read can't write over the new page.
    private var generation = 0

    private func sourceID(_ model: ConnectionModel, _ family: AgentFamily) -> String {
        "\(family.rawValue):\(model.connection?.credential.hostInstallationId ?? model.macName)"
    }

    func begin(models: [ConnectionModel]) {
        generation += 1
        self.models = models
        sources = [:]
        order = []
        for model in models {
            for family in [AgentFamily.codex, .claude] where model.projects.capabilities(family).usageLimits || model.accessEnded {
                let id = sourceID(model, family)
                sources[id] = Source(model: model, family: family)
                order.append(id)
            }
        }
        rebuild()
    }

    func retry(_ id: String) async {
        guard let source = sources[id] else { return }
        await load(id, source, force: true)
        rebuild()
    }

    func refresh(force: Bool) async {
        let generation = generation
        for id in order {
            guard let source = sources[id] else { continue }
            await load(id, source, force: force)
            guard !Task.isCancelled, generation == self.generation else { return }
        }
    }

    private func load(_ id: String, _ source: Source, force: Bool) async {
        let generation = generation
        let model = source.model
        if model.accessEnded {
            sources[id]?.failure = (UsageFailure.accessEnded, false)
            rebuild()
            return
        }
        sources[id]?.loading = true
        sources[id]?.failure = nil
        rebuild()
        var failure: (text: String, retryable: Bool)?
        do { try await model.loadUsage(family: source.family, force: force, maxAge: UsageAutoRefresh.maxAge) }
        catch {
            if Task.isCancelled { return }
            // An unreachable computer says nothing here (its dot is red elsewhere) and is read again when it answers.
            if !model.accessEnded, UsageFailure.isUnreachable(error, model) { failure = nil }
            else { failure = model.accessEnded ? (UsageFailure.accessEnded, false) : UsageFailure.describe(error, family: source.family) }
        }
        guard generation == self.generation else { return }
        sources[id]?.loading = false
        sources[id]?.failure = failure
        rebuild()
    }

    private func rebuild() {
        var merged: [String: UsageAccount] = [:]
        var accountOrder: [String] = []
        var pending: [UsageProblem] = []
        for id in order {
            guard let source = sources[id] else { continue }
            let model = source.model, family = source.family
            guard let response = model.usageCache[family]?[model.assignmentScope]?.response, !response.windows.isEmpty else {
                // Nothing cached: say why, or that it is still loading.
                if source.loading { pending.append(UsageProblem(id: id, family: family, kind: .loading)) }
                else if let failure = source.failure { pending.append(UsageProblem(id: id, family: family, kind: .failed(failure.text, retryable: failure.retryable))) }
                else if model.usageCache[family]?[model.assignmentScope] != nil {
                    pending.append(UsageProblem(id: id, family: family, kind: .failed("\(family.title) returned no usage windows. Check \(family.title) sign-in on your computer.", retryable: true)))
                }
                continue
            }
            // A provider that names no account stays with its own computer.
            let key = "\(family.rawValue):" + (response.accountKey ?? "computer:\(model.connection?.credential.hostInstallationId ?? model.macName)")
            if let existing = merged[key] {
                // Same account on another computer: keep the first reading, fill any missing name or plan.
                merged[key] = UsageAccount(id: key, family: family, label: existing.label ?? response.accountLabel, plan: existing.plan ?? response.planType,
                                           windows: existing.windows)
            } else {
                merged[key] = UsageAccount(id: key, family: family, label: response.accountLabel, plan: response.planType, windows: response.windows)
                accountOrder.append(key)
            }
        }
        accounts = accountOrder.compactMap { merged[$0] }
        // A computer that can't read a provider stays quiet when another computer shows it;
        // otherwise one row per provider, loading before failed.
        let shown = Set(accounts.map(\.family))
        var seen = Set<AgentFamily>()
        problems = pending.filter { !shown.contains($0.family) }
            .sorted { ($0.kind == .loading ? 0 : 1) < ($1.kind == .loading ? 0 : 1) }
            .filter { seen.insert($0.family).inserted }
        note = order.isEmpty && !models.isEmpty ? "No provider on your computers reports usage limits." : nil
        // Every computer is out of reach: one quiet line instead of an empty page.
        if note == nil, accounts.isEmpty, problems.isEmpty, !models.isEmpty, !sources.values.contains(where: \.loading) {
            note = "Usage appears when a computer is connected."
        }
    }
}

/// One provider's limit windows on one computer: % left, a bar, pace and the
/// reset countdown, refreshed while visible.
private struct ProviderUsageSection: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    /// nil where the screen's own title already says it.
    var title: String?
    @State private var loading = false
    @State private var failure: String?
    /// An update or a different provider version can't be fixed by trying again.
    @State private var retryable = true

    private var entry: CodexUsageCacheEntry? { model.usageCache[family]?[model.assignmentScope] }
    private var prefix: String { family.rawValue }
    private var host: String { model.connection?.credential.hostInstallationId ?? "" }

    var body: some View {
        Section {
            let windows = entry?.response.windows ?? []
            if let failure, !loading {
                Text(failure).foregroundStyle(.secondary)
                if !windows.isEmpty { Text("Previous usage values may be out of date.").foregroundStyle(.secondary) }
            }
            ForEach(windows) { window in
                UsageWindowRow(window: window, identifier: "\(prefix)-usage-window:\(window.id)")
            }
            if loading && windows.isEmpty {
                ProgressView("Loading usage…").accessibilityIdentifier("\(prefix)-usage-loading")
            } else if failure == nil && windows.isEmpty && entry != nil {
                Text("\(family.title) returned no usage windows. Check \(family.title) sign-in on your computer.").foregroundStyle(.secondary)
            } else if failure == nil && windows.isEmpty && model.macConnected != false {
                Text("Usage is unavailable right now.").foregroundStyle(.secondary)
            }
            if failure != nil, retryable {
                Button("Try again") { Task { await load(force: true) } }
                    .disabled(loading)
                    .accessibilityIdentifier("\(prefix)-usage-retry")
            }
        } header: {
            if let title {
                HStack {
                    Text(title)
                    if let plan = entry?.response.planType, !plan.isEmpty {
                        Spacer(minLength: 8)
                        Text(plan.capitalized).textCase(nil)
                    }
                }
                .accessibilityIdentifier("\(prefix)-usage-section")
            }
        }
        .task(id: model.assignmentScope) {
            await UsageAutoRefresh.run { force in await load(force: force) }
        }
        .onChange(of: model.accessEnded) { _, ended in
            if ended { failure = UsageFailure.accessEnded; retryable = false }
        }
    }

    @MainActor private func load(force: Bool = false) async {
        let scope = model.assignmentScope
        failure = nil
        retryable = true
        loading = true
        defer { if scope == model.assignmentScope { loading = false } }
        do {
            try await model.loadUsage(family: family, force: force, maxAge: UsageAutoRefresh.maxAge)
        } catch {
            guard scope == model.assignmentScope, !model.accessEnded else { return }
            // Unreachable: no failure line and no Try again; the next refresh reads it when it answers.
            if UsageFailure.isUnreachable(error, model) { return }
            let described = UsageFailure.describe(error, family: family)
            failure = described.text
            retryable = described.retryable
        }
    }
}

private struct UsageWindowRow: View {
    let window: CodexUsageWindow
    let identifier: String

    var body: some View {
        let pace = UsagePace(window: window)
        let reset = UsageReset.countdown(window)
        let exhausted = window.roundedRemainingPercent <= 0
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 12) {
                Text(window.label)
                Spacer(minLength: 12)
                Text("\(window.roundedRemainingPercent)% left").foregroundStyle(.secondary).monospacedDigit()
            }
            ProgressView(value: min(max(window.remainingPercent, 0), 100), total: 100)
                .tint(exhausted ? .red : pace?.isAhead == true ? .orange : .accentColor)
            if pace != nil || reset != nil {
                HStack(spacing: 12) {
                    if let pace { Text(pace.text).foregroundStyle(pace.isAhead ? Color.orange : Color.secondary) }
                    Spacer(minLength: 12)
                    if let reset { Text(reset).foregroundStyle(.secondary) }
                }
                .font(.footnote).monospacedDigit()
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .ignore)
        .accessibilityIdentifier(identifier)
        .accessibilityLabel(window.label)
        .accessibilityValue(["\(window.roundedRemainingPercent)% left", pace?.text, reset].compactMap { $0 }.joined(separator: ", "))
    }
}

/// A provider's usage under Settings > computer > provider.
struct SettingsProviderUsageSection: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    var body: some View { ProviderUsageSection(model: model, family: family, title: "Usage") }
}

/// The account line above a provider's limits: a centred pill, not a grouped-list header.
private struct UsagePill: View {
    let text: String
    let plan: String?
    var body: some View {
        HStack(spacing: 6) {
            Text(text).lineLimit(1)
            if let plan, !plan.isEmpty { Text("· " + plan.capitalized).foregroundStyle(.secondary) }
        }
        .font(.subheadline.weight(.semibold)).textCase(nil).foregroundStyle(.primary)
        .padding(.horizontal, 14).padding(.vertical, 6)
        .background(.quaternary, in: Capsule())
        .frame(maxWidth: .infinity)
        .padding(.vertical, 4)
        .accessibilityElement(children: .combine)
    }
}

/// Provider accounts are often emails; the page shows "j•••@example.com".
enum UsageMask {
    static func isEmail(_ label: String) -> Bool {
        guard let at = label.lastIndex(of: "@") else { return false }
        return at != label.startIndex
    }
    static func email(_ label: String) -> String {
        guard let at = label.lastIndex(of: "@"), at != label.startIndex else { return label }
        return String(label[label.startIndex]) + "•••" + label[at...]
    }
}
