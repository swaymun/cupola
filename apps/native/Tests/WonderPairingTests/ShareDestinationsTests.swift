import XCTest
@testable import WonderPairing

final class ShareDestinationsTests: XCTestCase {
    private let folder = ProjectFolder(id: "f1", path: "/w", name: "w", isPrimary: true, isAvailable: true)
    private lazy var wonder = ProjectSummary(id: "p1", name: "Wonder", rootsRevision: 7, folders: [folder])
    private lazy var site = ProjectSummary(id: "p2", name: "Website", folders: [])
    private lazy var hidden = ProjectSummary(id: "p3", name: "Hidden", isIncluded: false, folders: [])

    private func thread(_ reference: String, _ title: String, _ family: AgentFamily, at: Int, conversation: String? = "c-\(UUID())") -> ProjectThreadSummary {
        ProjectThreadSummary(reference: reference, conversationId: conversation, title: title, family: family, updatedAt: at)
    }

    private var catalog: [ShareTarget] {
        ShareCatalog.threads(projects: [wonder, site, hidden],
            pinned: [PinnedProjectThread(projectId: "p1", thread: thread("claude:notes", "Release notes", .claude, at: 50))],
            pages: ["p1": [thread("codex:a", "Share sheet", .codex, at: 100), thread("claude:notes", "Release notes v2", .claude, at: 60),
                           thread("codex:b", "Pairing", .codex, at: 10)],
                    "p2": [thread("claude:c", "Landing copy", .claude, at: 90), thread("codex:d", "Café images", .codex, at: 5)],
                    "p3": [thread("codex:x", "Excluded", .codex, at: 999)]])
    }

    func testBodyPutsTheNoteAboveSharedItems() {
        XCTAssertEqual(ShareComposition.body(note: "  Look at this \n", shared: ["https://a.example", " ", "quote"]),
                       "Look at this\n\nhttps://a.example\n\nquote")
        XCTAssertEqual(ShareComposition.body(note: "", shared: []), "")
    }

    func testNewThreadRequestMatchesNewChat() throws {
        let json = #"[{"id":"m-fast","displayName":"Fast","hidden":false,"agentFamily":"claude","reasoningEfforts":[{"id":"low","label":"Low"}]},{"id":"m-best","displayName":"Best","hidden":false,"agentFamily":"claude","isDefault":true,"reasoningEfforts":[{"id":"medium","label":"Medium"},{"id":"high","label":"High"}],"defaultReasoningEffort":"medium"},{"id":"m-codex","displayName":"Codex","hidden":false,"agentFamily":"codex","reasoningEfforts":[]},{"id":"m-hidden","displayName":"Old","hidden":true,"agentFamily":"claude","reasoningEfforts":[]}]"#
        let models = try JSONDecoder().decode([AgentOptions.Model].self, from: Data(json.utf8))
        let claude = ShareComposition.models(models, family: .claude)
        XCTAssertEqual(claude.map(\.id), ["m-best", "m-fast"], "the host's default leads; hidden and other providers are left out")
        let body = try ShareComposition.newThreadBody(project: wonder, family: .claude, capabilities: .published(.claude), model: claude[0], supportsModes: true,
                                                      deviceID: "d", requestID: "r", body: "hi")
        let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertEqual(fields["prepareOnly"] as? Bool, true)
        XCTAssertEqual(fields["clientMessageId"] as? String, "r")
        XCTAssertEqual(fields["model"] as? String, "m-best")
        XCTAssertEqual(fields["effort"] as? String, "medium")
        XCTAssertEqual(fields["claudeApproval"] as? String, ClaudeApproval.standard.rawValue, "Claude asks before acting, as New Chat starts it")
        XCTAssertEqual(fields["accessMode"] as? String, "workspace")
        XCTAssertEqual(fields["folderId"] as? String, "f1")
        XCTAssertEqual(fields["rootsRevision"] as? Int, 7)
        let codex = try ShareComposition.newThreadBody(project: site, family: .codex, capabilities: .published(.codex), model: models[2], supportsModes: true,
                                                       deviceID: "d", requestID: "r", body: "hi")
        let codexFields = try XCTUnwrap(JSONSerialization.jsonObject(with: codex) as? [String: Any])
        XCTAssertNil(codexFields["claudeApproval"]); XCTAssertNil(codexFields["effort"]); XCTAssertNil(codexFields["folderId"])
    }
}
