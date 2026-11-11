import Foundation
import WonderPairing

extension ProviderCapabilities {
    /// The record a current Mac publishes for `family`, read from the shared
    /// fixture the Mac's own record is tested against
    /// (packages/protocol/fixtures/provider-capabilities-v1.json).
    static func published(_ family: AgentFamily) -> Self {
        PublishedCapabilities.response.families.capabilities(for: family)
    }
}

enum PublishedCapabilities {
    static var fixtureURL: URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return root.appendingPathComponent("packages/protocol/fixtures/provider-capabilities-v1.json")
    }
    static let response: ProjectsResponse = {
        do { return try JSONDecoder().decode(ProjectsResponse.self, from: Data(contentsOf: fixtureURL)) }
        catch { fatalError("Unreadable capability fixture: \(error)") }
    }()
}
