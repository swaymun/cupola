import Foundation

/// One computer's runtime settings per provider (`GET/PUT
/// /api/v1/settings/provider-runtime`): the program to run, launch arguments,
/// environment variables and, for Codex, its home folder. The computer checks
/// them and says why one is refused; they apply to the next session it starts.
/// A secret variable's value goes to the computer's Keychain and never comes
/// back: the computer reports only whether one is set.
public struct ProviderRuntimeSettings: Codable, Equatable, Sendable {
    public static let path = "/api/v1/settings/provider-runtime"
    public var providers: [Entry]

    public init(providers: [Entry]) { self.providers = providers }

    public func entry(_ family: AgentFamily) -> Entry? {
        providers.first { $0.family == family.rawValue }
    }

    public struct Variable: Codable, Equatable, Sendable, Identifiable {
        public var name: String
        public var secret: Bool
        /// Present for a plain variable only.
        public var value: String?
        /// A secret has a value in the Keychain.
        public var set: Bool?
        /// The owner typed a value for this secret here, so it replaces the one
        /// in the Keychain. Never sent as a field; it only decides whether `value` is.
        public var replaced = false
        public var id: String { name }

        private enum CodingKeys: String, CodingKey { case name, secret, value, set }

        public init(name: String, secret: Bool, value: String? = nil, set: Bool? = nil) {
            self.name = name; self.secret = secret; self.value = value; self.set = set
        }
    }

    /// What a runtime was last started with: names only, never values.
    public struct LastLaunch: Codable, Equatable, Sendable {
        public var binary: String?
        public var arguments: [String]
        public var environmentNames: [String]
        public var home: String?
    }

    public struct Entry: Codable, Equatable, Sendable {
        public var family: String
        public var binary: String?
        public var arguments: String
        public var env: [Variable]
        public var home: String?
        public var supportsHome: Bool
        /// Why the saved settings are not in effect yet, when they are not.
        public var notice: String?
        public var lastLaunch: LastLaunch?

        public init(family: String, binary: String? = nil, arguments: String = "", env: [Variable] = [],
                    home: String? = nil, supportsHome: Bool = false, notice: String? = nil,
                    lastLaunch: LastLaunch? = nil) {
            self.family = family; self.binary = binary; self.arguments = arguments; self.env = env
            self.home = home; self.supportsHome = supportsHome; self.notice = notice
            self.lastLaunch = lastLaunch
        }

        /// The environment variable that carries this provider's API key, so the
        /// runtime uses the key instead of a subscription login. Nil: no API key support.
        public var apiKeyVariable: String? { Self.apiKeyVariable(family: family) }

        public static func apiKeyVariable(family: String) -> String? {
            switch family {
            case "claude": "ANTHROPIC_API_KEY" // provider identity
            case "codex": "OPENAI_API_KEY" // provider identity
            default: nil
            }
        }

        /// Nothing is customized: Cupola runs the provider as it finds it.
        public var isDefault: Bool {
            (binary ?? "").isEmpty && arguments.isEmpty && env.isEmpty && (home ?? "").isEmpty
        }

        /// The row's second line: what is customized, in the owner's words.
        public var summary: String {
            var parts: [String] = []
            if !(binary ?? "").isEmpty { parts.append("Own program") }
            if !arguments.isEmpty { parts.append("Arguments") }
            let others = env.filter { $0.name != apiKeyVariable }.count
            if others != env.count { parts.append("API key") }
            if others > 0 { parts.append(others == 1 ? "1 variable" : "\(others) variables") }
            if !(home ?? "").isEmpty { parts.append("Own home folder") }
            return parts.isEmpty ? "Default" : parts.joined(separator: " · ")
        }
    }

    /// A secret the owner left alone sends no value, so the computer keeps the
    /// one in its Keychain.
    public static func body(_ entry: Entry) throws -> Data {
        struct Body: Encodable {
            struct Variable: Encodable { let name: String; let secret: Bool; let value: String? }
            let binary: String?; let arguments: String; let env: [Variable]; let home: String?
        }
        func trimmed(_ text: String?) -> String? {
            let value = text?.trimmingCharacters(in: .whitespaces) ?? ""
            return value.isEmpty ? nil : value
        }
        let body = Body(
            binary: trimmed(entry.binary), arguments: entry.arguments.trimmingCharacters(in: .whitespaces),
            env: entry.env.map {
                Body.Variable(name: $0.name.trimmingCharacters(in: .whitespaces), secret: $0.secret,
                              value: $0.secret ? ($0.replaced ? $0.value ?? "" : nil) : $0.value ?? "")
            },
            home: trimmed(entry.home))
        return try JSONEncoder().encode(body)
    }
}
