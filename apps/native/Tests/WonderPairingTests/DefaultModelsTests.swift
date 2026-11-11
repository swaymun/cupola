import XCTest
@testable import WonderPairing

// Contract: Settings offers only what the Mac's catalog lists for a harness,
// drops a stored choice the catalog no longer lists, and sends explicit nulls
// so a cleared effort or speed is cleared on the Mac.
final class DefaultModelsTests: XCTestCase {
    private func options() throws -> [AgentOptions.Model] {
        let json = #"{"models":[{"id":"gpt-a","agentFamily":"codex","displayName":"GPT A","hidden":false,"reasoningEfforts":[{"id":"low","label":"Low"},{"id":"high","label":"High"}],"serviceTiers":[{"id":"default","label":"Standard"},{"id":"priority","label":"Fast"}]},{"id":"gpt-b","agentFamily":"codex","displayName":"GPT B","hidden":false,"reasoningEfforts":[{"id":"low","label":"Low"}]},{"id":"gpt-hidden","agentFamily":"codex","displayName":"Hidden","hidden":true,"reasoningEfforts":[]},{"id":"claude:sonnet","agentFamily":"claude","displayName":"Sonnet","hidden":false,"reasoningEfforts":[{"id":"high","label":"High"}]}],"allowedApprovalPolicies":[],"approvalModes":[]}"#
        return try JSONDecoder().decode(AgentOptions.self, from: Data(json.utf8)).models
    }

    func testAStoredChoiceTheCatalogDroppedFallsBackToTheMacsDefault() throws {
        let gone = DefaultModelChoices(family: .codex, options: try options(), stored: .init(family: .codex, model: "retired", effort: "high"))
        XCTAssertNil(gone.selected.model)
        XCTAssertNil(gone.selected.effort)
        // The model stays, but an effort it no longer offers is dropped.
        let stale = DefaultModelChoices(family: .codex, options: try options(), stored: .init(family: .codex, model: "gpt-b", effort: "high", serviceTier: "priority"))
        XCTAssertEqual(stale.selected, .init(family: .codex, model: "gpt-b"))
        let kept = DefaultModelChoices(family: .codex, options: try options(), stored: .init(family: .codex, model: "gpt-a", effort: "high", serviceTier: "priority"))
        XCTAssertEqual(kept.selected, .init(family: .codex, model: "gpt-a", effort: "high", serviceTier: "priority"))
    }

    func testTheBodySendsNulls() throws {
        let body = try JSONSerialization.jsonObject(with: DefaultModelChoices.body(.init(family: .codex, model: "gpt-b"))) as? [String: Any]
        XCTAssertEqual(body?["model"] as? String, "gpt-b")
        XCTAssertTrue(body?["effort"] is NSNull)
        XCTAssertTrue(body?["serviceTier"] is NSNull)
        XCTAssertEqual(DefaultModelChoices.path(.claude), "/api/v1/settings/default-models/claude")
    }

    func testPreferencesDecodeFromTheHost() throws {
        let json = #"{"families":[{"family":"codex","model":null,"effort":null,"serviceTier":null},{"family":"claude","model":"claude:sonnet","effort":"high","serviceTier":null}]}"#
        let preferences = try JSONDecoder().decode(DefaultModelPreferences.self, from: Data(json.utf8))
        XCTAssertEqual(preferences.entry(.claude).model, "claude:sonnet")
        XCTAssertNil(preferences.entry(.codex).model)
        XCTAssertFalse(preferences.startsInT3Code == true, "older hosts default to off")
    }
}

// Contract: with several paired Macs a choice reaches every Mac that offers the
// model, each Mac keeps only the effort it offers, and Macs without it are named.
final class AppDefaultModelsTests: XCTestCase {
    private func mac(_ id: String, _ models: [String], stored: String? = nil, effort: String? = nil) throws -> AppDefaultModels.Mac {
        let list = models.map { name -> String in
            let high = name == "a" ? #",{"id":"high","label":"High"}"# : ""
            return #"{"id":"\#(name)","agentFamily":"codex","displayName":"\#(name.uppercased())","hidden":false,"reasoningEfforts":[{"id":"low","label":"Low"}\#(high)]}"#
        }.joined(separator: ",")
        let catalog = try JSONDecoder().decode(AgentOptions.self, from: Data(#"{"models":[\#(list)],"allowedApprovalPolicies":[],"approvalModes":[]}"#.utf8)).models
        let effortJSON = effort.map { "\"\($0)\"" } ?? "null"
        let entry = stored.map { #"{"family":"codex","model":"\#($0)","effort":\#(effortJSON)}"# } ?? #"{"family":"codex"}"#
        let prefs = try JSONDecoder().decode(DefaultModelPreferences.self, from: Data(#"{"families":[\#(entry)]}"#.utf8))
        return .init(id: id, name: id.uppercased(), models: catalog, stored: prefs)
    }

    func testChoiceGoesToEveryMacThatOffersTheModelAndPartialModelsAreFlagged() throws {
        let plan = AppDefaultModels(family: .codex, macs: [try mac("m1", ["a", "b"]), try mac("m2", ["a"])])
        XCTAssertEqual(plan.options.map(\.id), ["a", "b"])
        XCTAssertEqual(plan.options.map(\.partial), [false, true])
        XCTAssertEqual(plan.options[1].macNames, ["M1"])
        XCTAssertEqual(plan.writes(choosing: "b").map(\.macID), ["m1"])
        XCTAssertEqual(plan.writes(choosing: "a").map(\.macID), ["m1", "m2"])
    }

}
