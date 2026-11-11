import XCTest
@testable import WonderPairing

final class ManagementTests: XCTestCase {
    // The family is whatever the host sent. A model id that looks like another
    // provider's, or one the host did not list, never changes it.
    func testFamilyComesFromTheHostNotTheModelId() throws {
        let options = try JSONDecoder().decode(AgentOptions.self, from: Data(#"{"models":[{"id":"claude:lookalike","agentFamily":"codex","displayName":"Lookalike","hidden":false,"reasoningEfforts":[]},{"id":"plain","displayName":"Old host","hidden":false,"reasoningEfforts":[]}],"allowedApprovalPolicies":[],"approvalModes":[{"id":"ask-for-approval","allowed":true}],"permissionsByFamily":{"claude":{"permissionModes":[],"approvalModes":[{"id":"full-access","allowed":true}]}}}"#.utf8))
        XCTAssertEqual(options.models[0].family, .codex)
        XCTAssertEqual(options.models[1].family, .codex)
    }
    func testDraftRoundTripPartitionsHostsAndPreservesRequestIdentity() throws {
        let suite = "WonderManagementTests." + UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let first = ManagementDraftStore(host: "first", defaults: defaults)
        let second = ManagementDraftStore(host: "second", defaults: defaults)
        var draft = ManagementDraft(); draft.values = ["name": "Ada", "prompt": "Check tomorrow’s weather"]
        try first.save(draft, key: "new")
        XCTAssertEqual(first.load("new"), draft)
        XCTAssertNil(second.load("new"))
        try second.save(draft, key: "new")
        first.removeAll()
        XCTAssertNil(first.load("new"))
        XCTAssertEqual(second.load("new")?.requestId, draft.requestId)
    }
    func testCursorModelsRetainOpaqueNativeIdentity() throws {
        let id = "cursor:gpt-6-luna[context=272k,reasoning=low,fast=false]"
        let data = try JSONSerialization.data(withJSONObject: ["allowedApprovalPolicies": [], "models": [["id": id, "displayName": "Luna", "agentFamily": "cursor", "hidden": false, "reasoningEfforts": []]]])
        let options = try JSONDecoder().decode(AgentOptions.self, from: data)
        XCTAssertEqual(options.models.first?.family, .cursor)
        XCTAssertEqual(options.models.first?.id, id)
    }

    func testModelOptionsDecodeDaemonContract() throws {
        let options = try JSONDecoder().decode(AgentOptions.self, from: Data(##"{"models":[{"id":"model","displayName":"Model","description":null,"modelSpecialty":null,"hidden":false,"reasoningEfforts":[{"id":"high","label":"High","description":null}],"defaultReasoningEffort":"high","serviceTiers":[{"id":"default","label":"Standard","description":"Standard speed, standard usage"},{"id":"priority","label":"Fast","description":"2x speed, increased usage"}],"defaultServiceTier":null}],"timezone":"America/New_York","allowedApprovalPolicies":["never"]}"##.utf8))
        XCTAssertEqual(options.models.first?.displayName, "Model")
        XCTAssertEqual(options.models.first?.reasoningEfforts.first?.id, "high")
        XCTAssertEqual(options.models.first?.serviceTiers?.last?.id, "priority")
        XCTAssertEqual(options.models.first?.serviceTiers?.last?.description, "2x speed, increased usage")
        XCTAssertEqual(options.timezone, "America/New_York")
    }

    func testDeletedConversationIntentRemovalDoesNotRemoveAnotherChat() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        try store.saveIntent(Data("deleted draft".utf8), conversation: "deleted")
        try store.saveIntent(Data("retained draft".utf8), conversation: "retained")
        try store.removeIntent(conversation: "deleted")
        try store.removeIntent(conversation: "deleted")
        XCTAssertNil(try store.loadIntent(conversation: "deleted"))
        XCTAssertEqual(try store.loadIntent(conversation: "retained"), Data("retained draft".utf8))
    }

}
