import Foundation

/// One computer's custom instruction per provider, as its host answers
/// `GET /api/v1/settings/custom-instructions` and every
/// `PUT /api/v1/settings/custom-instructions/{family}`. The host adds an
/// instruction that is on to every turn Cupola runs for that provider, never
/// to sessions in the provider's own apps; the text is kept while it is off.
public struct CustomInstructionSettings: Decodable, Equatable, Sendable {
    public struct Provider: Decodable, Equatable, Sendable {
        public let family: AgentFamily
        public let enabled: Bool
        public let text: String
        public init(family: AgentFamily, enabled: Bool, text: String) {
            self.family = family; self.enabled = enabled; self.text = text
        }
    }
    /// The longest instruction the host accepts, in characters.
    public let maxLength: Int
    public let providers: [Provider]
    public init(maxLength: Int = CustomInstructionSettings.defaultMaxLength, providers: [Provider]) {
        self.maxLength = maxLength; self.providers = providers
    }

    public static let defaultMaxLength = 4000
    public static let path = "/api/v1/settings/custom-instructions"
    public static func path(_ family: AgentFamily) -> String { path + "/" + family.rawValue }

    /// A provider this app does not know is left out rather than failing the
    /// whole answer.
    public init(from decoder: Decoder) throws {
        struct Loose: Decodable { let family: String; let enabled: Bool; let text: String }
        enum Keys: String, CodingKey { case maxLength, providers }
        let container = try decoder.container(keyedBy: Keys.self)
        maxLength = try container.decodeIfPresent(Int.self, forKey: .maxLength) ?? Self.defaultMaxLength
        providers = try container.decode([Loose].self, forKey: .providers).compactMap { row in
            AgentFamily(rawValue: row.family).map { Provider(family: $0, enabled: row.enabled, text: row.text) }
        }
    }

    /// nil when this computer can't add an instruction for `family`.
    public func provider(_ family: AgentFamily) -> Provider? { providers.first { $0.family == family } }

    /// The length the host checks: the trimmed text's characters as Rust
    /// counts them (Unicode scalars), so the counter and the limit agree.
    public static func length(_ text: String) -> Int {
        text.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count
    }

    /// Longer than the host accepts; Save waits until it is shortened.
    public func tooLong(_ text: String) -> Bool { Self.length(text) > maxLength }

    /// The instruction's first line, for the row that opens the editor.
    public static func preview(_ text: String) -> String? {
        text.split(whereSeparator: \.isNewline)
            .lazy.map { $0.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty }
    }

    public static func request(enabled: Bool, text: String) -> Data {
        struct Body: Encodable { let enabled: Bool; let text: String }
        return (try? JSONEncoder().encode(Body(enabled: enabled, text: text))) ?? Data()
    }
}

/// One computer's Cupola thread tools per provider (`GET
/// /api/v1/settings/thread-tools`, `PUT .../{family}` `{"enabled": bool}`):
/// whether the agent may list, read and message the owner's other chats and
/// hand work to another agent. On by default; the host lists only providers
/// that can take them.
public struct ThreadToolSettings: Decodable, Equatable, Sendable {
    public struct Provider: Decodable, Equatable, Sendable {
        public let family: AgentFamily
        public let enabled: Bool
        public init(family: AgentFamily, enabled: Bool) { self.family = family; self.enabled = enabled }
    }
    public let providers: [Provider]
    public init(providers: [Provider]) { self.providers = providers }

    public static let path = "/api/v1/settings/thread-tools"
    public static func path(_ family: AgentFamily) -> String { path + "/" + family.rawValue }

    public init(from decoder: Decoder) throws {
        struct Loose: Decodable { let family: String; let enabled: Bool }
        enum Keys: String, CodingKey { case providers }
        let container = try decoder.container(keyedBy: Keys.self)
        providers = try container.decode([Loose].self, forKey: .providers).compactMap { row in
            AgentFamily(rawValue: row.family).map { Provider(family: $0, enabled: row.enabled) }
        }
    }

    /// nil when this computer doesn't offer the thread tools to `family`.
    public func enabled(_ family: AgentFamily) -> Bool? { providers.first { $0.family == family }?.enabled }

    public static func request(enabled: Bool) -> Data {
        (try? JSONEncoder().encode(["enabled": enabled])) ?? Data()
    }
}
