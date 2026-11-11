import SwiftUI
import WonderPairing

func managementError(_ error: Error) -> String {
    if (error as NSError).domain == "Wonder" { return error.localizedDescription }
    if case PairingFailure.hostMessage(_, let message) = error { return message }
    // The one case where an archive or other change can't happen: the computer is out of reach.
    if error is URLError { return "Your computer can’t be reached right now. Try again once it’s connected." }
    if case PairingFailure.response(let status) = error {
        switch status {
        case 400, 422: return "These settings could not be saved. Check the fields and try again."
        case 401, 403: return "Access has ended. Reconnect to your computer in Settings."
        case 404: return "This item is no longer available. Refresh to see the latest changes."
        case 409: return "This change is blocked. Stop any active work on your computer, then try again."
        default: break
        }
    }
    return "Your computer could not confirm this change. Your choices are saved; reconnect and retry."
}
/// The one state for a computer that is off: a plain line with no error text
/// and no Try again. Pages reload on their own when the computer is back.
struct ComputerOfflineRow: View {
    let name: String
    var body: some View {
        Label("\(name) is offline", systemImage: "wifi.slash").foregroundStyle(.secondary)
            .accessibilityIdentifier("computer-offline")
    }
}

/// Whether a failed request is only the computer being off, not something to retry.
@MainActor func computerIsOffline(_ model: ConnectionModel, _ error: Error? = nil) -> Bool {
    !model.accessEnded && (model.macConnected == false || error is URLError)
}

extension ConnectionModel {
    func manage<T: Decodable & Sendable>(_ path: String, method: String = "GET", values: [String: String]? = nil,
                                          body: Data? = nil, decodingStatuses: Set<Int> = []) async throws -> T {
        guard let saved = connection, !accessEnded else { throw PairingFailure.response(401) }
        let value: T = try await api.request(
            path,
            origin: saved.origin,
            body: try body ?? values.map { try JSONEncoder().encode($0) },
            credential: saved.credential,
            method: method,
            decodingStatuses: decodingStatuses
        )
        guard connection?.credential.hostInstallationId == saved.credential.hostInstallationId,
            connection?.credential.deviceId == saved.credential.deviceId, !accessEnded else { throw CancellationError() }
        return value
    }
    var managementDrafts: ManagementDraftStore? { connection.map { ManagementDraftStore(host: $0.credential.hostInstallationId) } }
}

/// What the owner chose for model lists, kept on this device so the Settings
/// page reads and edits without a connection. A computer takes each change the
/// next time it is reachable (Settings, New chat), so a choice made offline
/// still lands.
struct LocalModelSettings: Codable, Equatable {
    struct Choice: Codable, Equatable {
        var model: String?
        var effort: String?
        var serviceTier: String?
    }
    /// The owner's last default per harness.
    var defaults: [String: Choice] = [:]
    /// Model ids the owner hid, per harness then computer (host installation id).
    var hidden: [String: [String: [String]]] = [:]
    /// Computers (host installation ids) that still owe a change, keyed
    /// `default:<family>` or `hidden:<family>`.
    var owed: [String: [String]] = [:]
    /// What each computer is to store for a default, keyed `<family>` then host
    /// id: each computer keeps only what its own catalog offers, so the choices
    /// differ. A computer without an entry takes `defaults`.
    var hostDefaults: [String: [String: Choice]] = [:]
    /// What each harness last listed, so the page reads the same offline.
    var catalog: [String: [AgentOptions.Model]] = [:]
    /// Each harness's models as every computer last listed them.
    var visibility: [String: [ComputerModels]] = [:]

    private static let key = "wonder.localModelSettings.v2"
    static func load() -> LocalModelSettings {
        UserDefaults.standard.data(forKey: key).flatMap { try? JSONDecoder().decode(LocalModelSettings.self, from: $0) } ?? .init()
    }
    /// Read, change and save in one step, so concurrent computers never clobber each other.
    @discardableResult static func update(_ change: (inout LocalModelSettings) -> Void) -> LocalModelSettings {
        var settings = load()
        change(&settings)
        settings.owed = settings.owed.filter { !$0.value.isEmpty }
        if let data = try? JSONEncoder().encode(settings) { UserDefaults.standard.set(data, forKey: key) }
        return settings
    }
    @MainActor static func hostID(_ model: ConnectionModel) -> String? { model.connection?.credential.hostInstallationId }

    /// Computers with a delivery running, and those asked again meanwhile. Two
    /// deliveries to one computer never overlap, so a stale list cannot land
    /// after a newer one.
    @MainActor private static var delivering: Set<String> = []
    @MainActor private static var deliverAgain: Set<String> = []

    /// Gives this computer every change it still owes. A computer that cannot
    /// be reached keeps its debt for the next time; one that refuses the choice
    /// (it lacks the model) is let go. Returns whether anything was applied.
    @MainActor @discardableResult static func flush(_ model: ConnectionModel, refreshOptions: Bool = true) async -> Bool {
        guard !model.previewMode, let id = hostID(model) else { return false }
        if delivering.contains(id) { deliverAgain.insert(id); return false }
        delivering.insert(id)
        defer { delivering.remove(id) }
        var applied = false
        repeat {
            deliverAgain.remove(id)
            if await deliverOwed(model, to: id) { applied = true }
        } while deliverAgain.contains(id)
        if applied, refreshOptions { await model.projects.refreshOptions() }
        return applied
    }

    @MainActor private static func deliverOwed(_ model: ConnectionModel, to id: String) async -> Bool {
        var applied = false
        for key in load().owed.filter({ $0.value.contains(id) }).keys.sorted() {
            let parts = key.split(separator: ":")
            guard parts.count == 2, let family = AgentFamily(rawValue: String(parts[1])) else { continue }
            let settled: Bool
            // What this delivery sent, so a change made meanwhile stays owed.
            var sentHidden: [String]?
            var sentChoice: Choice?
            do {
                if parts[0] == "hidden" {
                    let hidden = load().hidden[family.rawValue]?[id] ?? []
                    sentHidden = hidden
                    let _: ModelVisibility = try await model.manage(ModelVisibility.path(family), method: "PUT", body: ModelVisibility.body(hidden: hidden))
                } else if let choice = load().hostDefaults[family.rawValue]?[id] ?? load().defaults[family.rawValue] {
                    sentChoice = choice
                    func put(_ choice: Choice) async throws {
                        let entry = DefaultModelPreferences.Entry(family: family, model: choice.model, effort: choice.effort, serviceTier: choice.serviceTier)
                        let _: DefaultModelPreferences = try await model.manage(DefaultModelChoices.path(family), method: "PUT", body: DefaultModelChoices.body(entry))
                    }
                    do { try await put(choice) } catch PairingFailure.response(let status) where status == 422 {
                        try await put(Choice(model: choice.model))
                    }
                }
                settled = true
            } catch PairingFailure.response(let status) where status == 400 || status == 404 || status == 422 {
                settled = true
            } catch {
                settled = false
            }
            guard settled else { continue }
            let current = load()
            let changedMeanwhile = sentHidden.map { $0 != (current.hidden[family.rawValue]?[id] ?? []) }
                ?? sentChoice.map { $0 != (current.hostDefaults[family.rawValue]?[id] ?? current.defaults[family.rawValue]) }
                ?? false
            if changedMeanwhile { deliverAgain.insert(id); continue }
            update { $0.owed[key]?.removeAll { $0 == id } }
            applied = true
        }
        return applied
    }
}

/// One computer's models for a harness, each with whether the owner hid it.
struct ComputerModels: Codable, Equatable, Identifiable {
    /// The computer's host installation id.
    let id: String
    let name: String
    var models: [ModelVisibility.Model]
}

/// The order every model list uses: grouped by the provider a model id names
/// (OpenCode's `opencode/deepseek…`), then by name, so a model is found by
/// looking for it rather than by remembering where a computer listed it.
enum ModelListOrder {
    /// The provider an id names before its first `/`, if any.
    static func provider(of id: String) -> String? {
        guard let slash = id.firstIndex(of: "/") else { return nil }
        let head = id[..<slash]
        let name = head.split(separator: ":", maxSplits: 1).last.map(String.init) ?? String(head)
        return name.isEmpty ? nil : name
    }

    private static let known = ["opencode": "OpenCode", "openai": "OpenAI", "openrouter": "OpenRouter", "xai": "xAI",
                                "github-copilot": "GitHub Copilot", "deepseek": "DeepSeek"]
    static func title(ofProvider key: String) -> String {
        known[key] ?? key.replacingOccurrences(of: "-", with: " ").replacingOccurrences(of: "_", with: " ").capitalized
    }

    static func sorted<M>(_ models: [M], id: (M) -> String, name: (M) -> String) -> [M] {
        models.sorted { lhs, rhs in
            let (lp, rp) = (provider(of: id(lhs)) ?? "", provider(of: id(rhs)) ?? "")
            if lp != rp { return lp.localizedStandardCompare(rp) == .orderedAscending }
            return name(lhs).localizedStandardCompare(name(rhs)) == .orderedAscending
        }
    }

    /// Consecutive runs of one provider in an already sorted list.
    static func groups<M>(_ sorted: [M], id: (M) -> String) -> [(provider: String?, models: [M])] {
        var groups: [(provider: String?, models: [M])] = []
        for model in sorted {
            let provider = provider(of: id(model))
            if let last = groups.last, last.provider == provider { groups[groups.count - 1].models.append(model) }
            else { groups.append((provider, [model])) }
        }
        return groups
    }
}

/// The defaults and hidden models of every paired Mac, read from this device
/// first and from each reachable Mac as it answers. A Mac that cannot be
/// reached is simply left out; nothing here reports a connection error.
@MainActor final class AppDefaultModelsModel: ObservableObject {
    @Published private(set) var macs: [AppDefaultModels.Mac] = []
    /// Per harness: each computer's models with whether they are hidden.
    @Published private(set) var visibility: [String: [ComputerModels]] = [:]
    @Published private(set) var loading = false
    @Published private(set) var loaded = false
    private var computers: [ConnectionModel] = []
    private var pairedIDs: [String] = []

    init() { showSaved() }

    private struct LoadOutcome: Sendable {
        let id: String
        let name: String
        let options: [AgentOptions.Model]
        let stored: DefaultModelPreferences
        let visibility: ModelVisibility?
    }

    /// This device's own copy, standing in for a Mac until one answers.
    private func showSaved() {
        let local = LocalModelSettings.load()
        visibility = local.visibility
        guard !local.catalog.isEmpty else { return }
        let entries = local.defaults.compactMap { key, choice in
            AgentFamily(rawValue: key).map { DefaultModelPreferences.Entry(family: $0, model: choice.model, effort: choice.effort, serviceTier: choice.serviceTier) }
        }
        macs = [.init(id: "device", name: "This device", models: local.catalog.values.flatMap { $0 }, stored: .init(families: entries))]
    }

    func load(library: ConnectionLibrary) async {
        pairedIDs = library.saved.connections.map { $0.credential.hostInstallationId }
        await load(computers: library.saved.connections.filter { !$0.requiresPairing }.map { library.model(for: $0) })
    }

    /// Reads these computers; one slow or offline computer does not hold up the rest.
    func load(computers usable: [ConnectionModel]) async {
        guard !loading else { reloadWanted = true; return }
        loading = true
        defer { loading = false; loaded = true }
        repeat {
            reloadWanted = false
            let edits = editCount
            await read(computers: usable, edits: edits)
            // A choice made while the computers answered is newer than what they
            // sent, so read again instead of letting the old lists replace it.
        } while reloadWanted || editCount != readEdits
    }

    /// Counts the owner's changes; a read that began before one is out of date.
    private var editCount = 0
    private var readEdits = 0
    private var reloadWanted = false

    private func read(computers usable: [ConnectionModel], edits: Int) async {
        readEdits = edits
        computers = usable
        if pairedIDs.isEmpty { pairedIDs = usable.compactMap { LocalModelSettings.hostID($0) } }
        let requests: [Task<LoadOutcome?, Never>] = usable.map { model in
            Task { @MainActor in
                guard let id = LocalModelSettings.hostID(model) else { return nil }
                await LocalModelSettings.flush(model)
                guard let options: AgentOptions = try? await model.manage("/api/v1/agent-options"),
                      let stored: DefaultModelPreferences = try? await model.manage("/api/v1/settings/default-models") else { return nil }
                let visibility: ModelVisibility? = try? await model.manage(ModelVisibility.path)
                return LoadOutcome(id: id, name: model.macName, options: options.models, stored: stored, visibility: visibility)
            }
        }
        var loadedMacs: [AppDefaultModels.Mac] = []
        var lists: [AgentFamily: [ComputerModels]] = [:]
        let local = LocalModelSettings.load()
        for request in requests {
            guard let outcome = await request.value else { continue }
            loadedMacs.append(.init(id: outcome.id, name: outcome.name, models: outcome.options, stored: owed(outcome.stored, for: outcome.id)))
            for family in AgentFamily.allCases {
                var models = outcome.visibility?.models(family) ?? []
                guard !models.isEmpty else { continue }
                // A change this computer has not taken yet shows as the owner made it.
                if local.owed["hidden:" + family.rawValue]?.contains(outcome.id) == true {
                    let hidden = Set(local.hidden[family.rawValue]?[outcome.id] ?? [])
                    for index in models.indices { models[index].hidden = hidden.contains(models[index].id) }
                }
                lists[family, default: []].append(.init(id: outcome.id, name: outcome.name, models: Self.sorted(models)))
            }
        }
        guard !loadedMacs.isEmpty, edits == editCount else { return }
        macs = loadedMacs
        var shown: [String: [ComputerModels]] = [:]
        for (family, list) in lists { shown[family.rawValue] = list }
        visibility = shown
        LocalModelSettings.update { settings in
            for family in AgentFamily.allCases {
                let models = loadedMacs.flatMap { $0.models.filter { $0.family == family && !$0.hidden } }
                if !models.isEmpty {
                    var unique: [AgentOptions.Model] = []
                    for model in models where !unique.contains(where: { $0.id == model.id }) { unique.append(model) }
                    settings.catalog[family.rawValue] = unique
                }
            }
            settings.visibility = shown
        }
    }

    private static func sorted(_ models: [ModelVisibility.Model]) -> [ModelVisibility.Model] {
        ModelListOrder.sorted(models, id: \.id, name: \.displayName)
    }

    /// A choice this Mac has not taken yet shows as the owner made it.
    private func owed(_ stored: DefaultModelPreferences, for macID: String) -> DefaultModelPreferences {
        let local = LocalModelSettings.load()
        var stored = stored
        for family in AgentFamily.allCases where local.owed["default:" + family.rawValue]?.contains(macID) == true {
            guard let choice = local.hostDefaults[family.rawValue]?[macID] ?? local.defaults[family.rawValue] else { continue }
            stored.families = stored.families.filter { $0.family != family }
                + [.init(family: family, model: choice.model, effort: choice.effort, serviceTier: choice.serviceTier)]
        }
        return stored
    }

    /// Keeps the choice on this device at once and hands it to each Mac that answers.
    func apply(_ writes: [AppDefaultModels.Write], family: AgentFamily) {
        guard let entry = writes.first?.entry else { return }
        editCount += 1
        LocalModelSettings.update {
            $0.defaults[family.rawValue] = .init(model: entry.model, effort: entry.effort, serviceTier: entry.serviceTier)
            $0.hostDefaults[family.rawValue] = Dictionary(
                writes.filter { pairedIDs.contains($0.macID) }.map { ($0.macID, LocalModelSettings.Choice(model: $0.entry.model, effort: $0.entry.effort, serviceTier: $0.entry.serviceTier)) },
                uniquingKeysWith: { _, last in last })
            $0.owed["default:" + family.rawValue] = pairedIDs
        }
        for write in writes { replace(write.macID, with: write.entry, in: &macs) }
        deliver()
    }

    /// Hides or shows these models on one computer. Hiding every model leaves
    /// the provider out of New chat.
    func setHidden(_ ids: Set<String>, _ hidden: Bool, family: AgentFamily, computer: String) {
        guard var lists = visibility[family.rawValue], let list = lists.firstIndex(where: { $0.id == computer }) else { return }
        editCount += 1
        for index in lists[list].models.indices where ids.contains(lists[list].models[index].id) { lists[list].models[index].hidden = hidden }
        visibility[family.rawValue] = lists
        let hiddenIDs = lists[list].models.filter(\.hidden).map(\.id)
        // The default picker follows at once: a hidden model leaves it.
        macs = macs.map { mac in
            guard mac.id == computer || mac.id == "device" else { return mac }
            var models = mac.models
            for index in models.indices where models[index].family == family && ids.contains(models[index].id) { models[index].hidden = hidden }
            return .init(id: mac.id, name: mac.name, models: models, stored: mac.stored)
        }
        LocalModelSettings.update {
            $0.hidden[family.rawValue, default: [:]][computer] = hiddenIDs
            var owing = $0.owed["hidden:" + family.rawValue] ?? []
            if !owing.contains(computer) { owing.append(computer) }
            $0.owed["hidden:" + family.rawValue] = owing
            $0.visibility[family.rawValue] = lists
            if hidden { $0.catalog[family.rawValue]?.removeAll { ids.contains($0.id) } }
        }
        deliver()
    }

    /// Gives each computer what it owes, then reads the lists again so a model
    /// shown again returns to the default picker.
    private func deliver() {
        let computers = computers
        Task {
            let deliveries = computers.map { model in Task { @MainActor in await LocalModelSettings.flush(model) } }
            var applied = false
            for delivery in deliveries where await delivery.value { applied = true }
            if applied { await load(computers: computers) }
        }
    }

    private func replace(_ id: String, with entry: DefaultModelPreferences.Entry, in macs: inout [AppDefaultModels.Mac]) {
        guard let index = macs.firstIndex(where: { $0.id == id }) else { return }
        var stored = macs[index].stored
        stored.families = stored.families.filter { $0.family != entry.family } + [entry]
        macs[index] = .init(id: id, name: macs[index].name, models: macs[index].models, stored: stored)
    }
}

/// One provider's default model, reasoning effort and speed, and which of its
/// models are offered. The page reads from this device, so it never waits on a
/// connection; each computer takes the choices when it is reachable and applies
/// them to its own model list, so New Chat and the composer follow.
struct AppDefaultModelsView: View {
    @ObservedObject var library: ConnectionLibrary
    /// The provider whose default this page sets.
    let family: AgentFamily
    @StateObject private var state = AppDefaultModelsModel()

    var body: some View {
        WonderForm {
            if !state.macs.isEmpty {
                familySection(AppDefaultModels(family: family, macs: state.macs)).id(SettingsSearch.Anchor.defaultFields.id)
                ProviderModelSections(state: state, family: family)
            } else if state.loading || !state.loaded {
                Section { ProgressView("Loading models…") }
            } else {
                Section { Text("Models appear once a computer connects.").foregroundStyle(.secondary) }
            }
        }
        .settingsNavigationTitle(family.title)
        .navigationBarTitleDisplayMode(.inline)
        .task { await state.load(library: library) }
    }

    private var computerNames: [String] { library.paired.map { library.model(for: $0).macName } }

    @ViewBuilder private func familySection(_ defaults: AppDefaultModels) -> some View {
        let family = defaults.family
        // Alphabetical within a provider, so a model is found by name.
        let options = ModelListOrder.sorted(defaults.options, id: \.id, name: \.displayName)
        // Nothing stored shows the Mac's own default, never an empty choice.
        let shown = defaults.shown
        let unavailable = state.macs.filter { mac in
            guard let saved = mac.stored.entry(family).model else { return false }
            // A model the owner hid is not missing: the picker just moves on to another.
            if (state.visibility[family.rawValue] ?? []).contains(where: { $0.models.contains { $0.id == saved } }) { return false }
            return !mac.models.contains { !$0.hidden && $0.family == family && $0.id == saved }
        }
        let needsChoice = !unavailable.isEmpty && defaults.selected.model == nil
        Section {
            if !unavailable.isEmpty {
                Text("Saved model unavailable on \(unavailable.map(\.name).joined(separator: ", ")). Choose another model.")
                    .font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("default-model-unavailable")
            }
            if options.isEmpty {
                Text("\(family.title) has no models available yet.").foregroundStyle(.secondary)
            } else {
                Picker("Model", selection: Binding(
                    get: { needsChoice ? "unavailable" : shown.model ?? "" },
                    set: { state.apply(defaults.writes(choosing: $0), family: family) }
                )) {
                    if needsChoice { Text("Choose another model").tag("unavailable") }
                    ForEach(options) { option in
                        Text(option.displayName).tag(option.id)
                    }
                }
                .accessibilityIdentifier("default-model-\(family.rawValue)").settingsRow("default-model-\(family.rawValue)")
                if !defaults.efforts.isEmpty {
                    Picker("Reasoning effort", selection: Binding(
                        get: { shown.effort ?? "" },
                        set: { state.apply(defaults.writes(choosingEffort: $0), family: family) }
                    )) {
                        ForEach(defaults.efforts) { Text(ModelDefaults.title(of: $0)).tag($0.id) }
                    }
                    .accessibilityIdentifier("default-effort-\(family.rawValue)").settingsRow("default-effort-\(family.rawValue)")
                }
                if defaults.speeds.count > 1 {
                    Picker("Speed", selection: Binding(
                        get: { shown.serviceTier ?? "" },
                        set: { state.apply(defaults.writes(choosingSpeed: $0), family: family) }
                    )) {
                        ForEach(defaults.speeds) { Text($0.label).tag($0.id) }
                    }
                    .accessibilityIdentifier("default-speed-\(family.rawValue)").settingsRow("default-speed-\(family.rawValue)")
                }
            }
        } header: {
            Text(SettingsSearch.computersScopeLabel(computerNames))
        } footer: {
            if !defaults.computersWithoutSelection.isEmpty, defaults.macs.first?.id != "device" {
                Text("Not offered on \(defaults.computersWithoutSelection.joined(separator: ", ")), which keeps its own default.")
            }
        }
    }
}

/// Which of a provider's models each computer offers in New chat and the
/// composer: a search field, Select all and Select none for what the search
/// shows, and one list per computer, grouped by the provider a model belongs to.
struct ProviderModelSections: View {
    @ObservedObject var state: AppDefaultModelsModel
    let family: AgentFamily
    /// Only this computer's list, when set.
    var computer: String?
    @State private var query = ""

    var body: some View {
        let lists = (state.visibility[family.rawValue] ?? []).filter { !$0.models.isEmpty && (computer == nil || $0.id == computer) }
        if lists.contains(where: { $0.models.count > 1 }) || lists.contains(where: { $0.models.contains(where: \.hidden) }) {
            Section {
                HStack(spacing: 8) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary).accessibilityHidden(true)
                    TextField("Search models", text: $query)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .accessibilityIdentifier("models-search:" + family.rawValue)
                    if !query.isEmpty {
                        Button { query = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                            .buttonStyle(.borderless).accessibilityLabel("Clear search")
                            .accessibilityIdentifier("models-search-clear:" + family.rawValue)
                    }
                }
            } footer: {
                Text("Hidden models are left out of chats and of what agents can delegate to. Hide every model to leave \(family.title) out of New chat.")
            }
            .id(SettingsSearch.Anchor.hiddenModels.id)
            ForEach(lists) { list in
                computerSections(list, named: lists.count > 1)
            }
        }
    }

    @ViewBuilder private func computerSections(_ list: ComputerModels, named: Bool) -> some View {
        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines)
        let matching = needle.isEmpty ? list.models : list.models.filter {
            $0.displayName.localizedCaseInsensitiveContains(needle) || $0.id.localizedCaseInsensitiveContains(needle)
        }
        let shown = list.models.filter { !$0.hidden }.count
        let groups = ModelListOrder.groups(matching, id: \.id)
        let first = groups.first?.models.first?.id
        Section {
            HStack {
                Text("\(shown) of \(list.models.count) shown").font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("models-count:" + family.rawValue + ":" + list.id)
                Spacer()
                Button("Select all") { state.setHidden(Set(matching.map(\.id)), false, family: family, computer: list.id) }
                    .disabled(matching.allSatisfy { !$0.hidden })
                    .accessibilityIdentifier("models-select-all:" + family.rawValue + ":" + list.id)
                Button("Select none") { state.setHidden(Set(matching.map(\.id)), true, family: family, computer: list.id) }
                    .disabled(matching.allSatisfy(\.hidden))
                    .accessibilityIdentifier("models-select-none:" + family.rawValue + ":" + list.id)
            }
            .buttonStyle(.borderless).font(.subheadline)
            if matching.isEmpty {
                Text("No models match").foregroundStyle(.secondary).accessibilityIdentifier("models-none-match")
            } else if groups.count == 1 {
                rows(groups[0].models, list: list, first: first)
            }
        } header: {
            Text(named ? "Models on \(list.name)" : "Models")
        }
        if groups.count > 1 {
            ForEach(Array(groups.enumerated()), id: \.offset) { _, group in
                Section {
                    rows(group.models, list: list, first: first)
                } header: {
                    Text(group.provider.map(ModelListOrder.title(ofProvider:)) ?? "Other")
                }
            }
        }
    }

    /// The first list shown, whose first model is the page's search target.
    private var listID: String? { state.visibility[family.rawValue]?.first { computer == nil || $0.id == computer }?.id }

    @ViewBuilder private func rows(_ models: [ModelVisibility.Model], list: ComputerModels, first: String?) -> some View {
        ForEach(models) { model in
            Toggle(model.displayName, isOn: Binding(
                get: { !model.hidden },
                set: { state.setHidden([model.id], !$0, family: family, computer: list.id) }
            )).systemSwitch()
            .accessibilityIdentifier("model-shown:" + model.id)
            .settingsRow(model.id == first && list.id == listID ? "hidden-models-" + family.rawValue : "model-shown:" + model.id)
        }
    }
}

/// One computer's models for one provider, reached from that provider's Runtime row.
struct ProviderModelsView: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @StateObject private var state = AppDefaultModelsModel()

    var body: some View {
        WonderForm {
            let host = LocalModelSettings.hostID(model)
            if state.visibility[family.rawValue]?.contains(where: { $0.id == host }) == true {
                ProviderModelSections(state: state, family: family, computer: host)
            } else if state.loading || !state.loaded {
                Section { ProgressView("Loading models…") }
            } else {
                Section { Text("Models appear once \(model.macName) connects.").foregroundStyle(.secondary) }
            }
        }
        .settingsNavigationTitle("Models").navigationBarTitleDisplayMode(.inline)
        .task { await state.load(computers: [model]) }
    }
}

/// The computer owns this switch; native ChatGPT computer use is independent.
struct ComputerUseSettings: Decodable, Sendable, Equatable {
    let computerUseAvailable: Bool
    let cuaDriverEnabled: Bool
    let cuaDriverInstalled: Bool
    let codexUsesChatGPT: Bool
    var canToggleCuaDriver: Bool { computerUseAvailable && cuaDriverInstalled }
    static let path = "/api/v1/settings/computer-use"
    static func body(_ enabled: Bool) throws -> Data {
        try JSONEncoder().encode(["cuaDriverEnabled": enabled])
    }
}

/// Settings > computer > Connected apps: "Control this computer" in plain words.
/// The host's own status line names its driver and setup commands; this row
/// derives its text from the settings instead and never shows that line.
struct ComputerControlPresentation: Equatable {
    static let title = "Control this computer"
    /// Under the title: what the switch does, or why there is none.
    let detail: String
    let showsSwitch: Bool
    /// One limitation worth knowing, under the row.
    let note: String?

    init(_ settings: ComputerUseSettings, macName: String) {
        showsSwitch = settings.canToggleCuaDriver
        if !settings.computerUseAvailable {
            detail = "Turned off in Cupola’s setup on \(macName)."
        } else if !settings.cuaDriverInstalled {
            // The install runs on the computer, not the phone, so there is no link here.
            detail = "Install Cua Driver on \(macName) to use this."
        } else {
            detail = "Lets agents see the screen and use apps on \(macName)."
        }
        if settings.codexUsesChatGPT {
            note = "Codex uses ChatGPT’s computer use."
        } else if showsSwitch {
            note = "Codex chats already open pick up a change after Cupola restarts."
        } else {
            note = nil
        }
    }
}

/// Only confirmed host writes change the switch. Saves outlive navigation;
/// pairing changes reject responses belonging to the previous owner.
@MainActor final class ComputerUseSettingsModel: ObservableObject {
    @Published private(set) var settings: ComputerUseSettings?
    @Published private(set) var loading = false
    @Published private(set) var saving: Bool?
    @Published private(set) var failure: String?
    @Published private(set) var failedValue: Bool?
    /// The computer couldn't be reached: nothing to retry by hand, it is read again when it answers.
    @Published private(set) var unreachable = false
    private var generation = 0
    private var loadTask: Task<Void, Never>?

    func invalidate() {
        loadTask?.cancel(); loadTask = nil
        generation += 1
        loading = false; saving = nil
    }

    func load(request: @escaping @MainActor () async throws -> ComputerUseSettings) async {
        // Reappearing while a write is in flight must not discard its result.
        guard saving == nil else { return }
        invalidate()
        let generation = generation
        settings = nil; failure = nil; failedValue = nil; unreachable = false; loading = true
        // The model owns the read, like a save: a list that rebuilds the row
        // cancels the row's task, which used to leave neither settings nor a
        // failure, so the page stayed on "Checking computer control…".
        let task = Task { [weak self] in
            let result: Result<ComputerUseSettings, Error>
            do { result = .success(try await request()) } catch { result = .failure(error) }
            self?.finishLoad(result, generation: generation)
        }
        loadTask = task
        await task.value
    }

    private func finishLoad(_ result: Result<ComputerUseSettings, Error>, generation: Int) {
        guard generation == self.generation else { return }
        loadTask = nil
        loading = false
        switch result {
        case .success(let value):
            settings = value
        case .failure(let error) where error is CancellationError:
            break
        case .failure(let error) where error is URLError:
            unreachable = true
        case .failure:
            failure = "Couldn’t check computer control. Check your connection and try again."
        }
    }

    func save(_ enabled: Bool, request: () async throws -> ComputerUseSettings) async {
        guard let settings, settings.canToggleCuaDriver, enabled != settings.cuaDriverEnabled,
              !loading, saving == nil else { return }
        let generation = generation
        saving = enabled; failure = nil; failedValue = nil
        defer { if generation == self.generation { saving = nil } }
        do {
            let value = try await request()
            guard generation == self.generation, !Task.isCancelled else { return }
            self.settings = value
        } catch {
            guard generation == self.generation, !Task.isCancelled, !(error is CancellationError) else { return }
            failedValue = enabled
            failure = "Couldn’t save the change. The previous setting is still in use."
        }
    }
}

/// Rows for the Connected apps page's computer section. The first row carries
/// the loads: modifiers on several rows would run once per row. The computer's
/// page owns `state`, so a save started here still lands after Back.
struct ComputerControlRows: View {
    @ObservedObject var model: ConnectionModel
    @ObservedObject var state: ComputerUseSettingsModel
    @State private var saveTask: Task<Void, Never>?

    var body: some View {
        // One row whatever it shows (see T3CodeConnectionRows): otherwise the
        // load's own result restarts this task.
        VStack(alignment: .leading, spacing: 0) { lead }
            .task(id: model.assignmentScope + (model.macConnected == true ? ":connected" : "")) { await load() }
            .onChange(of: model.assignmentScope) { _, _ in saveTask?.cancel(); state.invalidate() }
            .onChange(of: model.macConnected) { _, connected in
                if connected == true, state.settings == nil { saveTask = Task { await load() } }
            }
        if let settings = state.settings, !computerIsOffline(model),
           let note = ComputerControlPresentation(settings, macName: model.macName).note {
            Text(note).font(.callout).foregroundStyle(.secondary)
                .accessibilityIdentifier(settings.codexUsesChatGPT ? "computer-use-native" : "computer-use-note")
        }
        if state.failure != nil, !computerIsOffline(model) {
            Button("Try again") {
                if let enabled = state.failedValue { save(enabled) }
                else { saveTask = Task { await load() } }
            }
            .disabled(state.loading || state.saving != nil || model.accessEnded)
            .accessibilityIdentifier("computer-use-retry")
        }
    }

    @ViewBuilder private var lead: some View {
        // Offline says so once, whether or not settings loaded before: a save
        // or load failure from before the disconnect is not repeated here.
        if computerIsOffline(model) {
            ComputerOfflineRow(name: model.macName).frame(minHeight: 44)
        } else if let settings = state.settings {
            let presented = ComputerControlPresentation(settings, macName: model.macName)
            HStack(spacing: 12) {
                Image(systemName: "cursorarrow.rays")
                    .frame(width: 32, height: 32).foregroundStyle(.secondary).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ComputerControlPresentation.title).font(.headline)
                    Text(presented.detail).font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("computer-use-current")
                    if let failure = state.failure {
                        Text(failure).font(.caption).foregroundStyle(.red)
                            .accessibilityIdentifier("computer-use-error")
                    }
                }
                Spacer(minLength: 8)
                if state.saving != nil {
                    ProgressView().accessibilityLabel("Saving")
                        .accessibilityIdentifier("computer-use-saving")
                } else if presented.showsSwitch {
                    Toggle(ComputerControlPresentation.title, isOn: Binding(get: { settings.cuaDriverEnabled }, set: save))
                        .labelsHidden().systemSwitch()
                        .disabled(state.loading || model.accessEnded)
                        .accessibilityIdentifier("computer-use-cua-driver").settingsRow("computer-use-cua-driver")
                }
            }
            .frame(minHeight: 44)
        } else if let failure = state.failure {
            Text(failure).font(.caption).foregroundStyle(.red)
                .accessibilityIdentifier("computer-use-error")
        } else if state.unreachable {
            Text(ComputerControlPresentation.title).foregroundStyle(.secondary)
                .frame(minHeight: 44, alignment: .leading)
                .accessibilityIdentifier("computer-use-unreachable")
        } else {
            ProgressView("Checking computer control…")
                .accessibilityIdentifier("computer-use-loading")
        }
    }

    private func save(_ enabled: Bool) {
        saveTask = Task {
            await state.save(enabled) {
                #if WONDER_DIAGNOSTICS
                try await DiagnosticSubagentFixture.holdComputerUseRequest(method: "PUT")
                #endif
                return try await model.manage(ComputerUseSettings.path, method: "PUT", body: ComputerUseSettings.body(enabled))
            }
        }
    }

    private func load() async {
        await state.load {
            #if WONDER_DIAGNOSTICS
            try await DiagnosticSubagentFixture.holdComputerUseRequest(method: "GET")
            #endif
            return try await model.manage(ComputerUseSettings.path)
        }
    }
}

/// Wonder's own skills on one Mac (`GET /api/v1/settings/wonder-skills`).
struct WonderSkillSettings: Decodable, Sendable {
    struct Skill: Decodable, Sendable, Identifiable, Equatable {
        let id: String
        let title: String
        let summary: String
        let enabled: Bool
    }
    let skills: [Skill]
}

/// Settings > computer: Wonder's skills for working from the phone. Each is
/// off until the owner turns it on; the Mac stores the choice. Shown only when
/// a provider on this Mac can use them.
struct WonderSkillsSection: View {
    @ObservedObject var model: ConnectionModel
    @State private var skills: [WonderSkillSettings.Skill] = []
    @State private var loading = false
    @State private var failure: String?
    @State private var saving: String?
    @State private var saveFailure: (id: String, message: String)?

    private var families: [AgentFamily] { AgentFamily.allCases.filter { model.projects.capabilities($0).wonderSkills } }

    var body: some View {
        // An unreachable computer shows no skills until it answers, rather than a failure with Try again.
        if !families.isEmpty, !(skills.isEmpty && model.macConnected == false) {
            Section {
                if skills.isEmpty {
                    if let failure, model.macConnected == true {
                        Text(failure).foregroundStyle(.secondary).accessibilityIdentifier("wonder-skills-failure")
                        Button("Try again") { Task { await load() } }.disabled(loading)
                            .accessibilityIdentifier("wonder-skills-retry")
                    } else {
                        ProgressView("Loading skills…").accessibilityIdentifier("wonder-skills-loading")
                    }
                }
                ForEach(skills, id: \.settingsSearchID) { skill in row(skill) }
            } header: {
                SettingsScopeHeader(title: "Cupola skills", scope: .computer, computer: model.macName)
            } footer: {
                Text(footer).accessibilityIdentifier("wonder-skills-footer")
            }
            .id(SettingsSearch.Anchor.skills.id)
            .task(id: model.assignmentScope + (model.macConnected == true ? ":connected" : "")) { await load() }
        }
    }

    private var footer: String {
        let names = families.map(\.title)
        let joined = names.count > 1 ? names.dropLast().joined(separator: ", ") + " and " + names.last! : names.joined()
        return "When on, \(joined) threads in Cupola can use these skills and know you’re writing from Cupola, away from your computer."
    }

    @ViewBuilder private func row(_ skill: WonderSkillSettings.Skill) -> some View {
        let label = VStack(alignment: .leading, spacing: 2) {
            Text(skill.title)
            Text(skill.summary).font(.caption).foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("wonder-skill-row:" + skill.id)
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .center) {
                label
                Spacer(minLength: 8)
                if saving == skill.id {
                    ProgressView()
                        .accessibilityLabel("Saving \(skill.title)")
                        .accessibilityIdentifier("wonder-skill-saving:" + skill.id)
                } else {
                    Toggle(skill.title, isOn: Binding(get: { skill.enabled }, set: { enabled in
                        Task { await save(skill, enabled: enabled) }
                    }))
                    .labelsHidden()
                    .systemSwitch()
                    .disabled(saving != nil || model.accessEnded)
                    .accessibilityIdentifier("wonder-skill:" + skill.id).settingsRow("wonder-skill:" + skill.id)
                }
            }
            if let saveFailure, saveFailure.id == skill.id {
                Text(saveFailure.message).font(.caption).foregroundStyle(.red)
                    .accessibilityIdentifier("wonder-skill-failure:" + skill.id)
            }
        }
        .padding(.vertical, 2)
    }

    private func load() async {
        guard !model.previewMode, !loading else { return }
        loading = true; defer { loading = false }
        do {
            let response: WonderSkillSettings = try await model.manage("/api/v1/settings/wonder-skills")
            skills = response.skills; failure = nil
        } catch is CancellationError {
            return
        } catch {
            failure = "Cupola couldn’t read these settings on \(model.macName)."
        }
    }

    private func save(_ skill: WonderSkillSettings.Skill, enabled: Bool) async {
        guard saving == nil else { return }
        saving = skill.id; saveFailure = nil
        defer { saving = nil }
        do {
            let response: WonderSkillSettings = try await model.manage(
                "/api/v1/settings/wonder-skills/\(ConnectionModel.escape(skill.id))", method: "PUT",
                body: try JSONEncoder().encode(["enabled": enabled]))
            skills = response.skills
        } catch is CancellationError {
            return
        } catch {
            let reason: String
            if case PairingFailure.response(let status) = error, [401, 403].contains(status) { reason = managementError(error) }
            else { reason = "\(model.macName) didn’t save the change. Try again." }
            saveFailure = (skill.id, (enabled ? "Couldn’t turn this on. " : "Couldn’t turn this off. ") + reason)
        }
    }
}

/// Only the first row carries the Settings search anchor, so a search match
/// highlights one row rather than the whole list.
private struct FirstRowFocus: ViewModifier {
    let first: Bool
    let id: String
    @ViewBuilder func body(content: Content) -> some View {
        if first { content.settingsRow("secrets-list") } else { content.settingsRow("secret-row:" + id) }
    }
}

/// A names-only inventory. Deleting a vault item cannot be undone, so the
/// native destructive confirmation names the effect on every approved chat.
struct SecretsSettingsView: View {
    let models: [ConnectionModel]
    @State private var rows: [Entry] = []
    @State private var failures: [String] = []
    @State private var loading = true
    @State private var deleting: String?
    @State private var candidate: Entry?
    private struct Entry: Identifiable {
        let secret: SavedSecret
        let model: ConnectionModel
        let id: String
    }
    var body: some View {
        WonderList {
          Section {
            ForEach(failures, id: \.self) { message in
                Text(message).foregroundStyle(.secondary).frame(minHeight: 44, alignment: .leading)
            }
            if loading && rows.isEmpty { ProgressView().frame(maxWidth: .infinity, minHeight: 44).accessibilityLabel("Loading secrets") }
            if !loading && rows.isEmpty && failures.isEmpty {
                Text("No saved secrets.").foregroundStyle(.secondary).frame(minHeight: 44, alignment: .leading)
                    .accessibilityIdentifier("secrets-empty").settingsRow("secrets-list")
            }
            ForEach(rows) { entry in
                VStack(alignment: .leading, spacing: 4) {
                    Text(entry.secret.name).font(.body.weight(.medium))
                    Text(entry.secret.projectName + (models.count > 1 ? " · " + entry.model.macName : ""))
                        .font(.subheadline).foregroundStyle(.secondary)
                    ForEach(Array(entry.secret.uses.enumerated()), id: \.offset) { _, usage in
                        Text(usage.title + " · " + providerName(usage.provider)).font(.caption).foregroundStyle(.secondary)
                    }
                    if deleting == entry.id { ProgressView().accessibilityLabel("Deleting secret") }
                }
                .padding(.vertical, 8)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("saved-secret:" + entry.secret.name)
                .modifier(FirstRowFocus(first: entry.id == rows.first?.id, id: entry.id))
                .swipeActions {
                    Button("Delete", role: .destructive) { candidate = entry }
                        .accessibilityIdentifier("secret-delete:" + entry.secret.name)
                        .disabled(deleting != nil || entry.model.accessEnded)
                }
            }
          }
        }
        .settingsNavigationTitle("Secrets").navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("secrets-list")
        .task { await load() }
        .refreshable { await load() }
        .confirmationDialog("Delete " + (candidate?.secret.name ?? "secret") + "?",
            isPresented: Binding(get: { candidate != nil }, set: { if !$0 { candidate = nil } }), titleVisibility: .visible) {
                if let candidate {
                    Button("Delete secret", role: .destructive) { Task { await remove(candidate) } }
                        .accessibilityIdentifier("secret-delete-confirm")
                }
                Button("Cancel", role: .cancel) { candidate = nil }
            } message: {
                Text("This removes it from your computer’s Keychain. These chats will need to ask for it again.")
            }
    }
    private func providerName(_ id: String) -> String {
        AgentFamily(rawValue: id)?.title ?? id
    }
    @MainActor private func load() async {
        loading = true; failures = []
        defer { loading = false }
        var loaded: [Entry] = []
        for model in models {
            do {
                let list: SecretList = try await model.manage("/api/v1/secrets")
                loaded += list.secrets.map { Entry(secret: $0, model: model, id: model.assignmentScope + ":" + $0.id) }
            } catch {
                if Task.isCancelled { continue }
                if case PairingFailure.response(501) = error {
                    failures.append(model.macName + ": Secrets aren’t available on this computer yet.")
                } else if case PairingFailure.hostMessage(501, _) = error {
                    failures.append(model.macName + ": Secrets aren’t available on this computer yet.")
                } else if case PairingFailure.hostMessage(_, let message) = error {
                    failures.append(model.macName + ": " + message)
                } else {
                    failures.append(model.macName + ": Secrets could not be loaded. Pull to retry.")
                }
            }
        }
        guard !Task.isCancelled else { return }
        rows = loaded.sorted { ($0.secret.projectName, $0.secret.name) < ($1.secret.projectName, $1.secret.name) }
    }
    @MainActor private func remove(_ entry: Entry) async {
        candidate = nil; deleting = entry.id
        defer { deleting = nil }
        do {
            struct Empty: Decodable, Sendable {}
            let _: Empty = try await entry.model.manage("/api/v1/secrets", method: "DELETE", body: entry.secret.deletionBody())
            rows.removeAll { $0.id == entry.id }
        } catch {
            failures = ["The secret could not be deleted. Check your connection and try again."]
        }
    }
}

// MARK: - Provider runtime (Settings > computer > provider > Runtime)

/// The row on a provider's page that opens its runtime settings: the program,
/// launch arguments, environment variables and (Codex) home folder this
/// computer starts the provider with. What is customized shows under the title.
struct ProviderRuntimeRow: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @Binding var settings: ProviderRuntimeSettings?
    @Binding var claudeApp: ClaudeAppSettings?
    @Binding var fileRestore: FileRestoreSettings?
    #if WONDER_DIAGNOSTICS
    /// `-diagnostics-provider-runtime-open` opens the editor for the captures that cannot tap (Duo, #387).
    @State private var diagnosticOpen = false
    #endif

    var body: some View {
        if let entry = settings?.entry(family) {
            NavigationLink {
                ProviderRuntimeEditor(model: model, family: family, settings: $settings, claudeApp: $claudeApp, fileRestore: $fileRestore)
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Runtime")
                    Text(entry.summary).font(.caption).foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-runtime-summary:" + family.rawValue)
                }
            }
            .accessibilityIdentifier("provider-runtime:" + family.rawValue).settingsRow("provider-runtime:" + family.rawValue)
            NavigationLink {
                ProviderModelsView(model: model, family: family)
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Models")
                    Text("Choose which appear in pickers").font(.caption).foregroundStyle(.secondary)
                }
            }
            .accessibilityIdentifier("provider-models:" + family.rawValue)
            #if WONDER_DIAGNOSTICS
            .navigationDestination(isPresented: $diagnosticOpen) {
                ProviderRuntimeEditor(model: model, family: family, settings: $settings, claudeApp: $claudeApp, fileRestore: $fileRestore)
            }
            .task {
                guard ProcessInfo.processInfo.arguments.contains("-diagnostics-provider-runtime-open") else { return }
                try? await Task.sleep(for: .seconds(1))
                diagnosticOpen = true
            }
            #endif
        }
    }
}

/// Edits one provider's runtime on this computer. Save sends everything at
/// once; the computer checks it and a refusal stays here in its words, with
/// what was saved unchanged. A saved change reaches the next session the
/// computer starts. Secret values go to the computer's Keychain: they show as
/// dots, are never read back, and a variable left alone keeps its value.
struct ProviderRuntimeEditor: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @Binding var settings: ProviderRuntimeSettings?
    @Binding var claudeApp: ClaudeAppSettings?
    @Binding var fileRestore: FileRestoreSettings?
    @State private var draft: ProviderRuntimeSettings.Entry
    @State private var saving = false
    @State private var failure: String?
    @Environment(\.dismiss) private var dismiss

    init(model: ConnectionModel, family: AgentFamily, settings: Binding<ProviderRuntimeSettings?>,
         claudeApp: Binding<ClaudeAppSettings?>, fileRestore: Binding<FileRestoreSettings?>) {
        self.model = model
        self.family = family
        _settings = settings
        _claudeApp = claudeApp
        _fileRestore = fileRestore
        _draft = State(initialValue: settings.wrappedValue?.entry(family) ?? .init(family: family.rawValue))
    }

    private var saved: ProviderRuntimeSettings.Entry? { settings?.entry(family) }
    private var changed: Bool { draft != saved }
    private var canSave: Bool { changed && !saving && !model.accessEnded && !model.previewMode && model.macConnected == true }

    var body: some View {
        WonderForm {
            Section {
                TextField("Default", text: Binding(get: { draft.binary ?? "" }, set: { draft.binary = $0 }))
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    .disabled(saving)
                    .accessibilityLabel("Program")
                    .accessibilityIdentifier("provider-runtime-program:" + family.rawValue)
            } header: { Text("Program") }
            Section {
                TextField("None", text: $draft.arguments)
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    .disabled(saving)
                    .accessibilityLabel("Arguments")
                    .accessibilityIdentifier("provider-runtime-arguments:" + family.rawValue)
            } header: { Text("Arguments") }
            if draft.supportsHome {
                Section {
                    TextField("Default", text: Binding(get: { draft.home ?? "" }, set: { draft.home = $0 }))
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .disabled(saving)
                        .accessibilityLabel("Home folder")
                        .accessibilityIdentifier("provider-runtime-home:" + family.rawValue)
                } header: { Text("Home folder") }
            }
            if let keyName = draft.apiKeyVariable {
                apiKeySection(keyName)
            }
            Section {
                // By position: a name changes with every keystroke.
                ForEach(draft.env.indices, id: \.self) { index in
                    // The API key has its own section above.
                    if draft.env[index].name != draft.apiKeyVariable { variableRow(index) }
                }
                .onDelete { offsets in
                    draft.env.remove(atOffsets: offsets)
                }
                Button {
                    draft.env.append(.init(name: "", secret: false, value: ""))
                } label: { Label("Add variable", systemImage: "plus") }
                    .disabled(saving || draft.env.count >= 32)
                    .accessibilityIdentifier("provider-runtime-add-variable:" + family.rawValue)
            } header: { Text("Environment") }
            ProviderRuntimeExtras(model: model, family: family, claudeApp: $claudeApp, fileRestore: $fileRestore)
            if let failure {
                Section {
                    Text(failure).foregroundStyle(.red)
                        .accessibilityIdentifier("provider-runtime-failure:" + family.rawValue)
                }
            } else if let notice = saved?.notice, !changed {
                Section {
                    Text(notice).foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-runtime-notice:" + family.rawValue)
                }
            }
            #if WONDER_DIAGNOSTICS
            if let launch = saved?.lastLaunch {
                Section {
                    Text(launchProbe(launch)).font(.caption.monospaced()).textSelection(.enabled)
                        .accessibilityIdentifier("provider-runtime-last-launch:" + family.rawValue)
                } header: { Text("Last launch") }
            }
            #endif
        }
        // Save sits in the title row on the Duo and in the toolbar elsewhere: one row, not two.
        .settingsNavigationTitle("Runtime") {
            if saving {
                ProgressView().accessibilityLabel("Saving")
            } else {
                Button("Save") { Task { await save() } }
                    .disabled(!canSave)
                    .accessibilityIdentifier("provider-runtime-save:" + family.rawValue)
            }
        }
        .navigationBarTitleDisplayMode(.inline)
    }

    /// An API key makes the runtime use the key instead of the subscription login;
    /// removing it returns to the login. It is a secret variable under the hood:
    /// the computer keeps it in its Keychain and never sends it back.
    @ViewBuilder private func apiKeySection(_ keyName: String) -> some View {
        let index = draft.env.firstIndex { $0.name == keyName }
        Section {
            if let index, draft.env[index].set == true, !draft.env[index].replaced {
                HStack {
                    Text("API key").foregroundStyle(.secondary)
                    Spacer()
                    Text("••••••••").foregroundStyle(.secondary)
                        .accessibilityIdentifier("provider-api-key-set:" + family.rawValue)
                }
                Button("Remove API key", role: .destructive) { draft.env.remove(at: index) }
                    .disabled(saving)
                    .accessibilityIdentifier("provider-api-key-remove:" + family.rawValue)
            } else {
                SecureField("Paste API key", text: Binding(
                    get: { index.flatMap { draft.env[$0].value } ?? "" },
                    set: { text in
                        if text.isEmpty {
                            if let at = draft.env.firstIndex(where: { $0.name == keyName }) { draft.env.remove(at: at) }
                        } else if let at = draft.env.firstIndex(where: { $0.name == keyName }) {
                            draft.env[at].value = text; draft.env[at].replaced = true
                        } else {
                            var variable = ProviderRuntimeSettings.Variable(name: keyName, secret: true, value: text)
                            variable.replaced = true
                            draft.env.append(variable)
                        }
                    }))
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    // A provider key is not a login: no Save Password offer.
                    .textContentType(.oneTimeCode)
                    .disabled(saving)
                    .accessibilityLabel("API key")
                    .accessibilityIdentifier("provider-api-key-field:" + family.rawValue)
            }
        } header: { Text("API key") } footer: {
            Text(draft.env.contains { $0.name == keyName }
                 ? "\(family.title) uses this key on \(model.macName) instead of its login."
                 : "Without a key, \(family.title) uses its login on \(model.macName).")
        }
    }

    @ViewBuilder private func variableRow(_ index: Int) -> some View {
        let variable = $draft.env[index]
        HStack(spacing: 8) {
            // The name sits over the value so a long name (ANTHROPIC_BASE_URL) is never cut off.
            VStack(alignment: .leading, spacing: 4) {
                TextField("NAME", text: variable.name)
                    .textInputAutocapitalization(.characters).autocorrectionDisabled()
                    .font(.footnote.monospaced()).foregroundStyle(.secondary)
                    .accessibilityIdentifier("provider-runtime-variable-name:\(index)")
                if variable.wrappedValue.secret {
                    SecureField(variable.wrappedValue.set == true && !variable.wrappedValue.replaced ? "••••••••" : "Value",
                                text: Binding(get: { variable.wrappedValue.value ?? "" },
                                              set: { variable.wrappedValue.value = $0; variable.wrappedValue.replaced = true }))
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        // A provider key is not a login: no Save Password offer.
                        .textContentType(.oneTimeCode)
                        .accessibilityIdentifier("provider-runtime-variable-value:\(index)")
                } else {
                    TextField("Value", text: Binding(get: { variable.wrappedValue.value ?? "" }, set: { variable.wrappedValue.value = $0 }))
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .accessibilityIdentifier("provider-runtime-variable-value:\(index)")
                }
            }
            Button {
                variable.wrappedValue.secret.toggle()
                variable.wrappedValue.value = nil
                variable.wrappedValue.set = nil
                variable.wrappedValue.replaced = variable.wrappedValue.secret
            } label: {
                Image(systemName: variable.wrappedValue.secret ? "lock.fill" : "lock.open")
                    .foregroundStyle(variable.wrappedValue.secret ? Color.accentColor : Color.secondary)
                    .frame(minWidth: 44, minHeight: 44)
            }
            .buttonStyle(.borderless)
            .accessibilityLabel(variable.wrappedValue.secret ? "Secret value. Tap to make it visible" : "Keep this value secret")
            .accessibilityIdentifier("provider-runtime-variable-secret:\(index)")
        }
        .disabled(saving)
    }

    #if WONDER_DIAGNOSTICS
    /// What the computer last started this provider with: names and arguments, no values.
    private func launchProbe(_ launch: ProviderRuntimeSettings.LastLaunch) -> String {
        "program: \(launch.binary ?? "default")\narguments: \(launch.arguments.joined(separator: " "))\n"
            + "environment: \(launch.environmentNames.joined(separator: ", "))\nhome: \(launch.home ?? "default")"
    }
    #endif

    private func save() async {
        guard canSave else { return }
        saving = true; failure = nil
        defer { saving = false }
        do {
            settings = try await model.manage(ProviderRuntimeSettings.path + "/" + family.rawValue, method: "PUT",
                                              body: ProviderRuntimeSettings.body(draft))
            // A setting that is not in effect yet says why, so stay to show it.
            if settings?.entry(family)?.notice == nil { dismiss() } else { draft = settings?.entry(family) ?? draft }
        } catch is CancellationError {
            return
        } catch PairingFailure.hostMessage(_, let message) {
            failure = message
        } catch {
            failure = "\(model.macName) didn’t save the runtime. Try again."
        }
    }
}

/// The provider settings that save the moment they change and live under Runtime:
/// the Claude app setting (Claude) and file backups for Rewind. Each section
/// shows only when this computer offers it for the provider.
struct ProviderRuntimeExtras: View {
    @ObservedObject var model: ConnectionModel
    let family: AgentFamily
    @Binding var claudeApp: ClaudeAppSettings?
    @Binding var fileRestore: FileRestoreSettings?
    var body: some View {
        if family == .claude, claudeApp != nil { // provider identity: Claude app setting
            Section { ClaudeAppRow(model: model, settings: $claudeApp) } header: { Text("Claude app") }
                .id(SettingsSearch.Anchor.claudeApp.id)
        }
        if let fileRestore {
            let restorable = fileRestore.providers.filter { $0.family == family }
            if !restorable.isEmpty {
                Section {
                    FileRestoreRows(model: model, settings: $fileRestore, providers: restorable)
                } header: { Text("Rewind") }.id(SettingsSearch.Anchor.rewind.id)
            }
        }
    }
}

/// Cupola on the computer, once, on that computer's page: its version and the
/// newest, and the command that updates it. A computer that reports no Cupola
/// version is older than this app needs, and says so plainly. The host's only
/// update entry points for a device (`update.work`, `update.prepare`) pause work
/// for the supported `cupola update` on the computer; none installs, and an
/// install restarts Cupola, so the app shows the command to run there and says why.
struct ComputerCupolaSection: View {
    let macName: String
    let host: MacHostCupola?
    /// The providers list was read (or the computer has no such list).
    let loaded: Bool
    /// The computer has no providers list at all: an earlier Cupola.
    let noProviderList: Bool
    @State private var copied = false

    private var olderThanNeeded: Bool { noProviderList || (loaded && host == nil) }

    var body: some View {
        Group {
            if olderThanNeeded || host != nil {
                Section {
                    VStack(alignment: .leading, spacing: 6) {
                        if olderThanNeeded {
                            updateNote(MacHostCupola.defaultCommand)
                            copyButton(MacHostCupola.defaultCommand)
                        } else if let host {
                            Text(host.versions).font(.body.weight(.semibold))
                                .accessibilityIdentifier("host-versions")
                            if host.updateNote(macName: macName) != nil {
                                updateNote(host.command ?? MacHostCupola.defaultCommand)
                                copyButton(host.command ?? MacHostCupola.defaultCommand)
                            }
                        }
                    }
                    .padding(.vertical, 2)
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("host-cupola")
                } footer: {
                    if olderThanNeeded || host?.available == true {
                        Text("Cupola restarts to update, so it can’t update itself from this device. Run this on \(macName).")
                            .accessibilityIdentifier("host-update-why")
                    }
                }
            }
        }
    }

    /// "Update Cupola on <computer>:" with the command under it, one element for VoiceOver.
    private func updateNote(_ command: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Update Cupola on \(macName):").font(.callout)
            Text(command).font(.callout.monospaced())
        }
        .textSelection(.enabled)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Update Cupola on \(macName): \(command)")
        .accessibilityIdentifier("host-update-note")
    }

    private func copyButton(_ command: String) -> some View {
        Button(copied ? "Copied" : "Copy command") {
            UIPasteboard.general.string = command
            copied = true
        }
        .buttonStyle(.bordered).controlSize(.small)
        .accessibilityIdentifier("host-copy-command")
    }
}
