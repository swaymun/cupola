import XCTest
@testable import WonderPairing

/// Settings > computer > provider reads and changes the Mac's custom
/// instruction (`/api/v1/settings/custom-instructions`) and Cupola thread
/// tools (`/api/v1/settings/thread-tools`) there.
final class AgentInstructionSettingsTests: XCTestCase {
    func testReadsEachProvidersInstructionAndTheHostsLimit() throws {
        let host = #"{"maxLength":4000,"providers":[{"family":"codex","enabled":true,"text":"Answer in British English."},{"family":"claude","enabled":false,"text":"Kept while off"},{"family":"future","enabled":true,"text":"x"}]}"#
        let settings = try JSONDecoder().decode(CustomInstructionSettings.self, from: Data(host.utf8))
        XCTAssertEqual(settings.maxLength, 4000)
        XCTAssertEqual(settings.provider(.codex), .init(family: .codex, enabled: true, text: "Answer in British English."))
        // Off keeps the text, so turning it back on doesn't lose it.
        XCTAssertEqual(settings.provider(.claude), .init(family: .claude, enabled: false, text: "Kept while off"))
        // A provider the host doesn't list can't take an instruction; an unknown one is dropped.
        XCTAssertNil(settings.provider(.opencode))
        XCTAssertEqual(settings.providers.count, 2)
    }

    func testAnOlderAnswerWithoutALimitUsesTheHostsDefault() throws {
        let host = #"{"providers":[{"family":"codex","enabled":false,"text":""}]}"#
        XCTAssertEqual(try JSONDecoder().decode(CustomInstructionSettings.self, from: Data(host.utf8)).maxLength, 4000)
    }

    func testASaveIsWrittenToTheProvidersRouteWithItsSwitch() throws {
        XCTAssertEqual(CustomInstructionSettings.path(.claude), "/api/v1/settings/custom-instructions/claude")
        let body = try JSONSerialization.jsonObject(with: CustomInstructionSettings.request(enabled: true, text: "Be brief.")) as? [String: Any]
        XCTAssertEqual(body?["enabled"] as? Bool, true)
        XCTAssertEqual(body?["text"] as? String, "Be brief.")
        XCTAssertEqual(body?.count, 2)
    }

    func testThreadToolsReadAndWriteThePerProviderSwitch() throws {
        let host = #"{"providers":[{"family":"codex","enabled":true},{"family":"claude","enabled":false},{"family":"future","enabled":true}]}"#
        let settings = try JSONDecoder().decode(ThreadToolSettings.self, from: Data(host.utf8))
        XCTAssertEqual(settings.enabled(.codex), true)
        XCTAssertEqual(settings.enabled(.claude), false)
        // Not listed: this provider can't take the tools, so no switch shows.
        XCTAssertNil(settings.enabled(.opencode))
        XCTAssertEqual(ThreadToolSettings.path(.codex), "/api/v1/settings/thread-tools/codex")
        let body = try JSONSerialization.jsonObject(with: ThreadToolSettings.request(enabled: false)) as? [String: Bool]
        XCTAssertEqual(body, ["enabled": false])
    }
}
