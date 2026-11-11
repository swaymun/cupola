import Foundation

/// How a provider's identity is drawn, shared by the app and the share
/// extension so both show the same mark, and each provider looks different. The switch is exhaustive: a new
/// provider fails to build until it has a mark.
public enum ProviderMark: Hashable, Sendable {
    /// Bundled artwork, by asset name.
    case artwork(String)
    /// A system symbol on a dark tile, for a provider without bundled artwork.
    case symbol(String)

    public init(_ family: AgentFamily) {
        switch family { // provider identity: artwork
        case .claude: self = .artwork("ProviderClaude") // provider identity
        case .codex: self = .artwork("ProviderCodex") // provider identity
        case .opencode: self = .symbol("chevron.left.forwardslash.chevron.right") // provider identity
        case .cursor: self = .artwork("ProviderCursor") // provider identity
        }
    }
}
