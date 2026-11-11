import Foundation

/// How much of an agent's work a conversation shows, per provider on each
/// computer, and per chat when a chat overrides its provider. The computer's
/// host owns both choices (`ChatDetailSettings`); the phone's provider page
/// and a chat's menu edit them there. Each level adds to the one before, in
/// the order the pickers list them.
public enum ChatDetailLevel: String, CaseIterable, Identifiable, Codable, Sendable {
    /// Replies only. Images, video and audio that tools produce still show.
    case conversation
    /// Adds the agent's commentary, its tool calls and how long it thought.
    case everything
    /// Adds the thinking a provider writes (Codex's summaries, Claude's
    /// thinking text). The host sends it only at this level. Hosts report no
    /// thinking capability, and OpenCode and Cursor (ACP) send none today, so
    /// the copy says it may add nothing rather than naming providers.
    case thinking
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .conversation: "Conversation"
        case .everything: "Commentary & tools"
        case .thinking: "Thinking"
        }
    }
    /// One line under the title in the picker.
    public var summary: String {
        switch self {
        case .conversation: "Replies, plus the pictures, video and audio tools make."
        case .everything: "Adds the agent’s notes as it works and each command, edit and tool."
        case .thinking: "Adds the thinking an agent writes. Some share none."
        }
    }
}

/// One computer's chat detail per provider and the chats that override it, as
/// its host answers `GET /api/v1/settings/chat-detail`, every
/// `PUT /api/v1/settings/chat-detail/{family}` `{"detail": ...}` and every
/// `PUT /api/v1/conversations/{id}/chat-detail` `{"detail": ... | null}`.
public struct ChatDetailSettings: Codable, Equatable, Sendable {
    public struct Provider: Codable, Equatable, Sendable {
        public let family: AgentFamily
        public let detail: ChatDetailLevel
        public init(family: AgentFamily, detail: ChatDetailLevel) { self.family = family; self.detail = detail }
    }
    public let providers: [Provider]
    /// Chats that override their provider, by conversation ID. A chat not
    /// listed follows its provider.
    public let chats: [String: ChatDetailLevel]
    public init(providers: [Provider], chats: [String: ChatDetailLevel] = [:]) {
        self.providers = providers
        self.chats = chats
    }

    public static let path = "/api/v1/settings/chat-detail"
    public static func path(_ family: AgentFamily) -> String { path + "/" + family.rawValue }
    public static func path(chat conversationID: String) -> String {
        "/api/v1/conversations/" + conversationID + "/chat-detail"
    }

    /// A provider or level this app does not know is left out rather than
    /// failing the whole answer; a provider left out shows everything.
    public init(from decoder: Decoder) throws {
        struct Loose: Decodable { let family: String; let detail: String }
        let container = try decoder.container(keyedBy: CodingKeys.self)
        providers = try container.decode([Loose].self, forKey: .providers).compactMap { row in
            guard let family = AgentFamily(rawValue: row.family), let detail = ChatDetailLevel(rawValue: row.detail) else { return nil }
            return Provider(family: family, detail: detail)
        }
        // An older host sends no chats; a level this app does not know follows the provider.
        let loose = try container.decodeIfPresent([String: String].self, forKey: .chats) ?? [:]
        chats = loose.compactMapValues { ChatDetailLevel(rawValue: $0) }
    }

    /// A conversation without a known provider shows everything.
    public func detail(for family: AgentFamily?) -> ChatDetailLevel {
        guard let family else { return .everything }
        return providers.first { $0.family == family }?.detail ?? .everything
    }

    /// The level `chat` shows: its own override, else its provider's.
    public func detail(for family: AgentFamily?, chat conversationID: String?) -> ChatDetailLevel {
        if let conversationID, let level = chats[conversationID] { return level }
        return detail(for: family)
    }

    /// The same settings with `conversationID` overriding its provider, or
    /// following it again for nil.
    public func setting(chat conversationID: String, to level: ChatDetailLevel?) -> ChatDetailSettings {
        var next = chats
        next[conversationID] = level
        return ChatDetailSettings(providers: providers, chats: next)
    }

    public static func request(_ detail: ChatDetailLevel) -> Data {
        (try? JSONEncoder().encode(["detail": detail.rawValue])) ?? Data()
    }

    /// A chat's body; nil is JSON null (follow the provider).
    public static func request(chat level: ChatDetailLevel?) -> Data {
        struct Body: Encodable {
            let detail: ChatDetailLevel?
            func encode(to encoder: Encoder) throws {
                var container = encoder.container(keyedBy: CodingKeys.self)
                try container.encode(detail, forKey: .detail)
            }
            enum CodingKeys: String, CodingKey { case detail }
        }
        return (try? JSONEncoder().encode(Body(detail: level))) ?? Data()
    }
}
