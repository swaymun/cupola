import Foundation

/// Available agents and models shared by Project creation, the composer and Settings.
public struct AgentOptions: Codable, Sendable, Equatable {
    public struct Choice: Codable, Identifiable, Sendable, Equatable {
        public let id: String
        public let label: String
        public let description: String?
    }
    public struct Model: Codable, Identifiable, Sendable, Equatable {
        public var agentFamily: String? = nil
        public var family: AgentFamily { AgentFamily(host: agentFamily) }
        public struct Capabilities: Codable, Sendable, Equatable {
            public let guide: Bool?
            public let goals: Bool?
            public let imageGeneration: Bool?
        }
        public var capabilities: Capabilities? = nil
        public let id: String
        public let displayName: String
        public var hidden: Bool
        public let reasoningEfforts: [Choice]
        public let serviceTiers: [Choice]?
        public let defaultServiceTier: String?
        public let defaultReasoningEffort: String?
        /// The host's pick for this provider when a thread has no model yet.
        public var isDefault: Bool? = nil
    }
    public let models: [Model]
    public let timezone: String?
    public let allowedApprovalPolicies: [String]

}

/// Every model a computer offers for each harness, with whether the owner hid it
/// from chats and from the models agents can delegate to.
public struct ModelVisibility: Codable, Equatable, Sendable {
    public struct Model: Codable, Equatable, Sendable, Identifiable {
        public let id: String
        public let displayName: String
        public var hidden: Bool
        public init(id: String, displayName: String, hidden: Bool) { self.id = id; self.displayName = displayName; self.hidden = hidden }
    }
    public struct Family: Codable, Equatable, Sendable {
        public let family: AgentFamily
        public let models: [Model]
    }
    public let families: [Family]
    public func models(_ family: AgentFamily) -> [Model] { families.first { $0.family == family }?.models ?? [] }
    public static let path = "/api/v1/settings/model-visibility"
    public static func path(_ family: AgentFamily) -> String { path + "/" + family.rawValue }
    public static func body(hidden: [String]) throws -> Data { try JSONSerialization.data(withJSONObject: ["hiddenModels": hidden]) }
}

/// The owner's default model, reasoning effort and speed for each harness, as
/// the Mac stores them. A nil model means the first model the Mac lists, a nil
/// effort that model's first effort, and a nil speed that model's own. The Mac applies them to the model
/// catalog (`isDefault`, `defaultReasoningEffort`), so New Chat, the composer
/// and Settings show the same choice without a rule of their own.
public struct DefaultModelPreferences: Decodable, Equatable, Sendable {
    public struct Entry: Decodable, Equatable, Sendable {
        public var family: AgentFamily
        public var model: String?
        public var effort: String?
        public var serviceTier: String?
        public init(family: AgentFamily, model: String? = nil, effort: String? = nil, serviceTier: String? = nil) {
            self.family = family; self.model = model; self.effort = effort; self.serviceTier = serviceTier
        }
    }
    public var families: [Entry]
    /// Per-computer New chat default. Older hosts omit it, meaning off.
    public var startsInT3Code: Bool?
    public init(families: [Entry], startsInT3Code: Bool = false) {
        self.families = families; self.startsInT3Code = startsInT3Code
    }
    public func entry(_ family: AgentFamily) -> Entry { families.first { $0.family == family } ?? Entry(family: family) }
}

/// What the Settings picker offers for one harness, limited to the catalog.
public struct DefaultModelChoices: Sendable {
    public let family: AgentFamily
    public let models: [AgentOptions.Model]
    public let selected: DefaultModelPreferences.Entry

    /// A stored model the catalog no longer offers is dropped: the Mac then
    /// uses its default model.
    public init(family: AgentFamily, options: [AgentOptions.Model], stored: DefaultModelPreferences.Entry) {
        self.family = family
        models = options.filter { $0.family == family && !$0.hidden }
        var entry = DefaultModelPreferences.Entry(family: family)
        if let model = models.first(where: { $0.id == stored.model }) {
            entry.model = model.id
            entry.effort = model.reasoningEfforts.contains { $0.id == stored.effort } ? stored.effort : nil
            entry.serviceTier = (model.serviceTiers ?? []).contains { $0.id == stored.serviceTier } ? stored.serviceTier : nil
        }
        selected = entry
    }

    /// The PUT body: nulls are sent so a cleared effort or speed is cleared.
    public static func body(_ entry: DefaultModelPreferences.Entry) throws -> Data {
        let value: [String: Any] = ["model": entry.model ?? NSNull(), "effort": entry.effort ?? NSNull(), "serviceTier": entry.serviceTier ?? NSNull()]
        return try JSONSerialization.data(withJSONObject: value)
    }
    public static func path(_ family: AgentFamily) -> String { "/api/v1/settings/default-models/\(family.rawValue)" }
}

/// One harness's default across every paired Mac. The picker lists the union of
/// the Macs' catalogs; a choice is written to each Mac that offers that model.
public struct AppDefaultModels: Sendable {
    public struct Mac: Sendable, Identifiable {
        public let id: String
        public let name: String
        public let models: [AgentOptions.Model]
        public let stored: DefaultModelPreferences
        public init(id: String, name: String, models: [AgentOptions.Model], stored: DefaultModelPreferences) {
            self.id = id; self.name = name; self.models = models; self.stored = stored
        }
    }
    public struct Option: Identifiable, Sendable, Equatable {
        public let id: String
        public let displayName: String
        /// Names of the Macs that offer this model.
        public let macNames: [String]
        /// True when some paired Mac does not offer it.
        public let partial: Bool
    }
    public struct Write: Equatable, Sendable {
        public let macID: String
        public let entry: DefaultModelPreferences.Entry
    }

    public let family: AgentFamily
    public let macs: [Mac]
    private let perMac: [(mac: Mac, choices: DefaultModelChoices)]

    public init(family: AgentFamily, macs: [Mac]) {
        self.family = family
        self.macs = macs
        perMac = macs.map { ($0, DefaultModelChoices(family: family, options: $0.models, stored: $0.stored.entry(family))) }
    }

    public var options: [Option] {
        var order: [String] = []
        var names: [String: String] = [:]
        var holders: [String: [String]] = [:]
        for (mac, choices) in perMac {
            for model in choices.models {
                if names[model.id] == nil { order.append(model.id); names[model.id] = model.displayName }
                holders[model.id, default: []].append(mac.name)
            }
        }
        return order.map { id in
            Option(id: id, displayName: names[id] ?? id, macNames: holders[id] ?? [], partial: (holders[id] ?? []).count < macs.count)
        }
    }

    /// The first Mac with a valid stored model speaks for the picker.
    private var reference: DefaultModelChoices? {
        perMac.first { $0.choices.selected.model != nil }?.choices
    }
    /// What the Macs store: a nil model, effort or speed follows the Mac's default.
    public var selected: DefaultModelPreferences.Entry { reference?.selected ?? .init(family: family) }

    /// The model the picker shows: the stored one, else the one the first Mac
    /// uses by default, which is the first it lists. Never an empty choice.
    public var model: AgentOptions.Model? {
        let listed = perMac.flatMap(\.choices.models)
        if let stored = selected.model { return listed.first { $0.id == stored } }
        return perMac.lazy.compactMap { ModelDefaults.defaultModel(in: $0.choices.models) }.first
    }
    public var efforts: [AgentOptions.Choice] { model?.reasoningEfforts ?? [] }
    public var speeds: [AgentOptions.Choice] { model?.serviceTiers ?? [] }

    /// The model, effort and speed the picker shows: what is stored, else what
    /// the Mac uses for that model by default (its first effort or speed when
    /// it names none).
    public var shown: DefaultModelPreferences.Entry {
        guard let model else { return selected }
        let efforts = model.reasoningEfforts
        let effort = selected.effort ?? model.defaultReasoningEffort.flatMap { id in efforts.first { $0.id == id }?.id } ?? efforts.first?.id
        let speeds = model.serviceTiers ?? []
        let speed = selected.serviceTier ?? model.defaultServiceTier.flatMap { tier in speeds.first { $0.id == tier }?.id } ?? speeds.first?.id
        return .init(family: family, model: model.id, effort: effort, serviceTier: speed)
    }

    private func offering(_ id: String?) -> [(mac: Mac, choices: DefaultModelChoices)] {
        guard let id else { return perMac }
        return perMac.filter { $0.choices.models.contains { $0.id == id } }
    }

    /// True when the Macs that offer the chosen model do not store the same choice.
    public var differsBetweenComputers: Bool {
        let entries = offering(selected.model).map(\.choices.selected)
        return entries.contains { $0 != entries[0] }
    }

    /// Macs that do not offer the chosen model and keep their own default.
    public var computersWithoutSelection: [String] {
        guard let id = selected.model else { return [] }
        return perMac.filter { !$0.choices.models.contains { $0.id == id } }.map(\.mac.name)
    }

    public func writes(choosing model: String) -> [Write] {
        writes(applying: .init(family: family, model: model, effort: selected.effort, serviceTier: selected.serviceTier), to: offering(model))
    }
    /// An effort or speed chosen for the shown default model also stores that model.
    public func writes(choosingEffort effort: String) -> [Write] {
        var entry = selected; entry.model = shown.model; entry.effort = effort
        return writes(applying: entry, to: offering(entry.model))
    }
    public func writes(choosingSpeed speed: String) -> [Write] {
        var entry = selected; entry.model = shown.model; entry.serviceTier = speed
        return writes(applying: entry, to: offering(entry.model))
    }

    /// Each Mac keeps only what its own catalog offers for the model.
    private func writes(applying entry: DefaultModelPreferences.Entry,
                        to targets: [(mac: Mac, choices: DefaultModelChoices)]) -> [Write] {
        targets.map { Write(macID: $0.mac.id, entry: DefaultModelChoices(family: family, options: $0.mac.models, stored: entry).selected) }
    }
}
