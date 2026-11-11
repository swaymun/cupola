import XCTest
@testable import WonderPairing

final class MacProvidersTests: XCTestCase {

    // The Claude app setting decodes as the Mac sends it, saves as
    // {"enabled": …}, and explains the claude --resume alternative.
    func testClaudeAppSettingRoundTripsAndNamesTheResumeAlternative() throws {
        let settings = try JSONDecoder().decode(ClaudeAppSettings.self, from: Data(#"{"enabled":false,"appInstalled":true}"#.utf8))
        XCTAssertEqual(settings, ClaudeAppSettings(enabled: false, appInstalled: true))
        let body = try JSONSerialization.jsonObject(with: ClaudeAppSettings.body(enabled: true)) as? [String: Bool]
        XCTAssertEqual(body, ["enabled": true])
        for installed in [true, false] {
            let text = ClaudeAppSettings(enabled: false, appInstalled: installed).explanation(computer: "Studio")
            XCTAssertTrue(text.contains("claude --resume"), text)
            XCTAssertTrue(text.contains("Studio"), text)
        }
        XCTAssertTrue(ClaudeAppSettings(enabled: false, appInstalled: false).explanation(computer: "Studio")
            .hasPrefix("The Claude app isn’t installed on Studio."))
    }

    func testDecodesTheMacResponse() throws {
        let json = #"{"providers":[{"id":"codex","running":true,"starting":false,"incompatible":false,"updatePending":false,"detail":null,"state":"signedIn","version":"1","account":"API key","working":false}]}"#
        let response = try JSONDecoder().decode(MacProvidersResponse.self, from: Data(json.utf8))
        XCTAssertEqual(response.providers.first?.family, .codex)
        XCTAssertEqual(response.providers.first?.account, "API key")
        XCTAssertEqual(response.providers.first?.starting, false)
        // Phone and Mac updates are independent; earlier hosts omit startup state.
        let earlierHost = json.replacingOccurrences(of: "\"starting\":false,", with: "")
        let earlier = try JSONDecoder().decode(MacProvidersResponse.self, from: Data(earlierHost.utf8))
        XCTAssertEqual(earlier.providers.first?.account, "API key")
        XCTAssertNil(earlier.providers.first?.starting)
        XCTAssertNil(earlier.providers.first?.update)
        let withUpdate = json.replacingOccurrences(of: "\"working\":false", with:
            #""working":false,"update":{"owner":"homebrew","available":true,"latestVersion":"0.170.0","canUpdate":true,"cupolaUpdateNeeded":false,"status":"idle","message":null}"#)
        let update = try JSONDecoder().decode(MacProvidersResponse.self, from: Data(withUpdate.utf8)).providers.first?.update
        XCTAssertEqual(update, MacProviderUpdate(owner: "homebrew", available: true, latestVersion: "0.170.0", canUpdate: true))
    }

    // The host's version block and a provider's installed/latest line; a Mac
    // that sends no `host` is older than this app needs and is told so.
    func testHostVersionsWarningAndOlderMac() throws {
        let json = #"""
        {"providers":[{"id":"codex","running":true,"incompatible":false,"updatePending":false,"detail":null,"state":"signedIn","version":"0.166.0","account":"Pro","working":false,
          "warning":"Chats work. These features are off until Cupola is updated on your computer: Message search.",
          "update":{"owner":"npm","available":true,"installedVersion":"0.166.0","latestVersion":"0.170.0","canUpdate":true,"cupolaUpdateNeeded":false,"status":"idle","message":null}}],
         "host":{"version":"1.0.256","latestVersion":"1.0.300","available":true,"command":"brew upgrade --cask cupola"}}
        """#
        let response = try JSONDecoder().decode(MacProvidersResponse.self, from: Data(json.utf8))
        let row = MacProviderRow(try XCTUnwrap(response.providers.first), macName: "Studio")
        XCTAssertEqual(row.versions, "Version 0.166.0 · Latest 0.170.0")
        XCTAssertEqual(row.action, .install)
        XCTAssertEqual(row.warning, "Chats work. These features are off until Cupola is updated on your computer: Message search.")
        XCTAssertNil(row.detail, "the version line replaces a detail that only repeats the version")
        let host = try XCTUnwrap(response.host)
        XCTAssertEqual(host.versions, "Cupola 1.0.256 · Latest 1.0.300")
        XCTAssertEqual(host.updateNote(macName: "Studio"), "Update Cupola on Studio: brew upgrade --cask cupola")
        XCTAssertNil(MacHostCupola(version: "1.0.300", latestVersion: "1.0.300").updateNote(macName: "Studio"))
        XCTAssertEqual(MacHostCupola(version: "1.0.300", latestVersion: "1.0.300").versions, "Cupola 1.0.300 · Up to date")
        XCTAssertEqual(MacHostCupola(version: "1.0.300").versions, "Cupola 1.0.300")

        // An earlier Mac: no host, no warning, no installed/latest pair.
        let older = #"{"providers":[{"id":"claude","running":true,"incompatible":false,"updatePending":false,"detail":null,"state":"signedIn","version":"2.1.4","account":"Max","working":false}]}"#
        let earlier = try JSONDecoder().decode(MacProvidersResponse.self, from: Data(older.utf8))
        XCTAssertNil(earlier.host)
        let earlierRow = MacProviderRow(try XCTUnwrap(earlier.providers.first), macName: "Studio")
        XCTAssertEqual(earlierRow.versions, "Version 2.1.4")
        XCTAssertNil(earlierRow.warning)
        XCTAssertEqual(MacHostCupola.olderMacNote(macName: "Studio"), "Update Cupola on Studio: brew upgrade --cask cupola")
    }

    // A warning shows only for a provider that runs.
    func testWarningIsHiddenWhenTheProviderDoesNotRun() {
        let provider = MacProviderStatus(id: "codex", state: "signedOut", version: "0.170.0", warning: "Chats work.")
        XCTAssertNil(MacProviderRow(provider, macName: "Studio").warning)
    }
}
