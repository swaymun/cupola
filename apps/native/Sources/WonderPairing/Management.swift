import Foundation

public enum AgentFamily: String, Codable, CaseIterable, Sendable, Identifiable {
    case codex, claude, opencode, cursor
    public var id: String { rawValue }
    public var title: String {
        switch self { // provider identity: names
        case .codex: "Codex" // provider identity
        case .claude: "Claude" // provider identity
        case .opencode: "OpenCode" // provider identity
        case .cursor: "Cursor" // provider identity
        }
    }
    /// The family the host sent. A host that sends none predates Claude, so the
    /// conversation is Codex; the family is never read from a model id.
    public init(host value: String?) { self = value.flatMap(AgentFamily.init(rawValue:)) ?? .codex } // provider identity
}

/// Durable request identity and payload live together: retries reuse both.
public struct ManagementDraft: Codable, Equatable, Sendable {
    public var requestId = UUID().uuidString
    public var values: [String: String] = [:]
    public init() {}
}
public struct ManagementDraftStore {
    public let host: String
    public let defaults: UserDefaults
    public init(host: String, defaults: UserDefaults = .standard) { self.host = host; self.defaults = defaults }
    private var prefix: String { "wonder.management." + Data(host.utf8).base64EncodedString() + "." }
    public func load(_ key: String) -> ManagementDraft? {
        defaults.data(forKey: prefix + key).flatMap { try? JSONDecoder().decode(ManagementDraft.self, from: $0) }
    }
    public func save(_ draft: ManagementDraft, key: String) throws { defaults.set(try JSONEncoder().encode(draft), forKey: prefix + key) }
    public func remove(_ key: String) { defaults.removeObject(forKey: prefix + key) }
    public func removeAll() { for key in defaults.dictionaryRepresentation().keys where key.hasPrefix(prefix) { defaults.removeObject(forKey: key) } }
}

public struct MacLocationPage: Decodable, Sendable {
    public struct Entry: Decodable, Identifiable, Sendable {
        public let name: String
        public let path: String
        public let isDirectory: Bool
        public var id: String { path + "\u{0}" + name }
    }
    public let path: String
    public let parentPath: String?
    public let entries: [Entry]
    public let nextOffset: Int?
}
