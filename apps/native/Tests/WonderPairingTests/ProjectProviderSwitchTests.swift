import XCTest
@testable import WonderPairing

/// A Project thread's model picker is New chat's: the harness first, then its
/// models. A model of the thread's own harness is saved as before; another
/// harness's travels with the next message and the host moves the thread on
/// delivery. The access menu follows the harness the next message goes to.
final class ProjectProviderSwitchTests: XCTestCase {
    private func models() throws -> [AgentOptions.Model] {
        let json = #"""
        {"models":[
         {"id":"gpt-a","displayName":"GPT A","hidden":false,"reasoningEfforts":[],"agentFamily":"codex"},
         {"id":"claude:sonnet","displayName":"Sonnet","hidden":false,"reasoningEfforts":[{"id":"high","label":"High"}],"defaultReasoningEffort":"high","agentFamily":"claude"},
         {"id":"claude:hidden","displayName":"Hidden","hidden":true,"reasoningEfforts":[],"agentFamily":"claude"},
         {"id":"gpt-b","displayName":"GPT B","hidden":false,"reasoningEfforts":[],"agentFamily":"codex"}],
         "timezone":"UTC","allowedApprovalPolicies":[]}
        """#
        return try JSONDecoder().decode(AgentOptions.self, from: Data(json.utf8)).models
    }

    func testTheAccessMenuShowsTheNextHarnessAndSavesOnlyWhatTheMoveKeeps() {
        let codex = ProviderCapabilities.published(.codex), claude = ProviderCapabilities.published(.claude) // provider identity
        let cursor = ProviderCapabilities.published(.cursor) // provider identity
        let thread = ProjectAccess(accessMode: .fullAccess, claudeApproval: nil, planMode: false)
        // Codex → Claude: Claude's menu, at the access the move keeps and Claude's starting mode.
        let shown = ProjectModelPicker.access(thread, movingTo: claude)
        XCTAssertEqual(shown, ProjectAccess(accessMode: .fullAccess))
        let rows = ProjectAccessChoice.choices(capabilities: claude, supportsModes: true, current: shown)
        XCTAssertTrue(rows.contains(.manual) && rows.contains(.auto), "Claude's own approval modes are offered")
        XCTAssertEqual(ProjectAccessChoice.selected(for: shown, capabilities: claude, supportsModes: true), .fullAccess)
        func saves(_ choice: ProjectAccessChoice, from current: ProviderCapabilities = codex, to target: ProviderCapabilities = claude,
                   shown: ProjectAccess = shown) -> Bool {
            let next = choice.result(from: shown, capabilities: target, supportsModes: true)
            return ProjectModelPicker.canSaveBeforeMove(next, shown: shown, current: current, target: target, supportsModes: true)
        }
        XCTAssertTrue(saves(.auto), "Auto is Claude's starting mode; only the access level changes")
        XCTAssertTrue(saves(.plan), "plan mode is kept by the move")
        XCTAssertFalse(saves(.manual), "Ask waits until the thread is Claude's")
        XCTAssertFalse(saves(.acceptEdits))
        // What is saved keeps the thread's own approval mode.
        let next = ProjectAccessChoice.auto.result(from: shown, capabilities: claude, supportsModes: true)
        let claudeThread = ProjectAccess(accessMode: .fullAccess, claudeApproval: .ask)
        XCTAssertEqual(ProjectModelPicker.savedBeforeMove(next, thread: claudeThread), ProjectAccess(accessMode: .workspace, claudeApproval: .ask))
        // Claude → Codex keeps the access level and drops Claude's approval mode.
        XCTAssertEqual(ProjectModelPicker.access(claudeThread, movingTo: codex), ProjectAccess(accessMode: .fullAccess))
        XCTAssertTrue(saves(.readOnly, from: claude, to: codex, shown: ProjectModelPicker.access(claudeThread, movingTo: codex)))
        // A harness without access choices cannot store a level for the next one.
        let fromCursor = ProjectModelPicker.access(ProjectAccess(), movingTo: codex)
        XCTAssertFalse(saves(.fullAccess, from: cursor, to: codex, shown: fromCursor))
        XCTAssertTrue(saves(.workspace, from: cursor, to: codex, shown: fromCursor), "the current choice needs no save")
        // Moving to a harness without access choices starts in its settings' access.
        XCTAssertEqual(ProjectModelPicker.access(thread, movingTo: cursor).accessMode, .workspace)
    }

    func testAModelOfTheSameProviderIsSavedAndAnotherProvidersTravelsWithTheMessage() throws {
        let all = try models()
        let gpt = all.first { $0.id == "gpt-b" }!
        let sonnet = all.first { $0.id == "claude:sonnet" }!
        XCTAssertEqual(ProjectModelPicker.choice(current: .codex, option: gpt, effort: nil, serviceTier: nil), .saveToThread)
        XCTAssertEqual(
            ProjectModelPicker.choice(current: .codex, option: sonnet, effort: "high", serviceTier: "fast"),
            .sendWithNextMessage(ProjectMessageModel(family: .claude, model: "claude:sonnet", effort: "high", serviceTier: "fast")))
        XCTAssertEqual(ProjectModelPicker.choice(current: .claude, option: sonnet, effort: nil, serviceTier: nil), .saveToThread)
    }

    private func detail(family: String, model: String) throws -> ProjectConversationDetail {
        let json = #"""
        {"conversationId":"c","projectId":"p","projectName":"App","title":"Fix","family":"\#(family)","model":"\#(model)",
         "accessMode":"workspace","workingFolder":"/work","workingFolderName":"work","isPinned":false,"hasUnread":false,
         "hasNativeSession":true,"folderInProject":true}
        """#
        return try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(json.utf8))
    }

    func testACarriedChoiceEndsOnlyOnceTheHostHasMovedTheThread() throws {
        let pending = ProjectMessageModel(family: .claude, model: "claude:sonnet")
        XCTAssertFalse(ProjectModelPicker.isApplied(pending, to: nil))
        XCTAssertFalse(ProjectModelPicker.isApplied(pending, to: try detail(family: "codex", model: "gpt-a")))
        XCTAssertFalse(ProjectModelPicker.isApplied(pending, to: try detail(family: "claude", model: "claude:opus")))
        XCTAssertTrue(ProjectModelPicker.isApplied(pending, to: try detail(family: "claude", model: "claude:sonnet")))
    }

    func testTheMessageRequestCarriesItsModelAndOlderRequestsStillDecode() throws {
        var intent = ComposerIntent()
        intent.draft = "Continue in Claude"
        try intent.begin(device: "phone", projectModel: ProjectMessageModel(family: .claude, model: "claude:sonnet", effort: "high"))
        let request = try XCTUnwrap(intent.pending?.request)
        let wire = try JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any]
        let model = try XCTUnwrap(wire?["projectModel"] as? [String: Any])
        XCTAssertEqual(model["family"] as? String, "claude")
        XCTAssertEqual(model["model"] as? String, "claude:sonnet")
        XCTAssertEqual(model["effort"] as? String, "high")
        XCTAssertNil(model["serviceTier"])
        // A send saved before this existed decodes without a model, and an
        // ordinary send does not name one.
        var old = try XCTUnwrap(wire)
        old.removeValue(forKey: "projectModel")
        let decoded = try JSONDecoder().decode(SendRequest.self, from: JSONSerialization.data(withJSONObject: old))
        XCTAssertNil(decoded.projectModel)
        var plain = ComposerIntent()
        plain.draft = "Hello"
        try plain.begin(device: "phone")
        XCTAssertNil(plain.pending?.request.projectModel)
        // The saved send keeps the model across a relaunch, so a retry sends the same one.
        let restored = try JSONDecoder().decode(SendRequest.self, from: JSONEncoder().encode(request))
        XCTAssertEqual(restored.projectModel, request.projectModel)
    }

}
