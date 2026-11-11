import Foundation

/// Finds the model the owner named ("DeepSeek Flash") among a provider's
/// models, so voice never starts a chat on a model other than the one asked for.
public enum VoiceModelMatch {
    public struct Candidate: Equatable, Sendable {
        public let id: String
        public let name: String
        public init(id: String, name: String) { self.id = id; self.name = name }
    }
    public enum Result: Equatable, Sendable {
        case match(Candidate)
        case none
        case ambiguous([Candidate])
    }

    private static func tokens(_ text: String) -> [String] {
        text.lowercased().split { !$0.isLetter && !$0.isNumber }.map(String.init)
    }

    /// An exact id or name wins; otherwise every word the owner said must
    /// begin a word of the model's name (ids only match whole, so a provider
    /// word like "opencode" is not a model name). Several fits are ambiguous.
    public static func resolve(_ spoken: String, among candidates: [Candidate]) -> Result {
        let wanted = tokens(spoken)
        guard !wanted.isEmpty else { return .none }
        let exact = candidates.filter { tokens($0.id) == wanted || tokens($0.name) == wanted }
        if exact.count == 1 { return .match(exact[0]) }
        let fits = candidates.filter { candidate in
            let have = tokens(candidate.name)
            return wanted.allSatisfy { word in have.contains { $0.hasPrefix(word) } }
        }
        switch fits.count {
        case 0: return .none
        case 1: return .match(fits[0])
        default: return .ambiguous(fits)
        }
    }
}
