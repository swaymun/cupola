import XCTest
@testable import WonderPairing

final class ComposerCompletionTests: XCTestCase {

    // Contract: the host's catalog decodes as sent, an unknown availability
    // degrades to `unknown`, and filtering keeps the provider's order inside
    // name-prefix, alias-prefix and substring tiers.
    func testCommandCatalogDecodesAndFiltersInNativeOrder() throws {
        let catalog = try JSONDecoder().decode(ComposerCommandCatalog.self, from: Data(#"""
        {"availability":"ready","revision":"o:3","commands":[
          {"name":"compact","description":"Compact context"},
          {"name":"review","description":"Review a PR","argumentHint":"<pr>","aliases":["cr"]},
          {"name":"pr-comments","description":"Read comments"},
          {"name":"clear","description":"Clear"}]}
        """#.utf8))
        XCTAssertEqual(catalog.availability, .ready)
        XCTAssertEqual(catalog.matching("").map(\.name), ["compact", "review", "pr-comments", "clear"])
        XCTAssertEqual(catalog.matching("c").map(\.name), ["compact", "clear", "review", "pr-comments"])
        XCTAssertEqual(catalog.matching("RE").map(\.name), ["review"])
        XCTAssertEqual(catalog.matching("comm").map(\.name), ["pr-comments"])
        XCTAssertEqual(catalog.commands[1].insertion, "/review")
        let later = try JSONDecoder().decode(ComposerCommandCatalog.self, from: Data(#"{"availability":"later","commands":[],"revision":null}"#.utf8))
        XCTAssertEqual(later.availability, .unknown)
        let cached = try JSONDecoder().decode(ComposerCommandCatalog.self, from: Data(#"""
        {"availability":"cached","revision":"cache:1","fetchedAt":"2026-11-02T18:00:00Z",
        "commands":[{"name":"cupola-summary","description":"Draft a summary request","insertionText":"Summarize this conversation."}]}
        """#.utf8))
        XCTAssertEqual(cached.availability, .cached)
        XCTAssertEqual(cached.fetchedAt, "2026-11-02T18:00:00Z")
        XCTAssertEqual(cached.commands[0].insertion, "Summarize this conversation.")
    }

    func testSearchResultsDecode() throws {
        let results = try JSONDecoder().decode(WorkspaceSearchResults.self, from: Data(#"""
        {"truncated":false,"entries":[{"rootId":"workspace","rootLabel":"Workspace","path":"src/main.rs","name":"main.rs","isDirectory":false,"mention":"@src/main.rs"}]}
        """#.utf8))
        XCTAssertEqual(results.entries.first?.mention, "@src/main.rs")
        XCTAssertEqual(results.entries.first?.folder, "src")
        let threads = try JSONDecoder().decode(ComposerThreadResults.self, from: Data(#"""
        {"truncated":false,"entries":[{"conversationId":"claude:release","title":"Release notes","family":"claude","mention":"[@Release notes](cupola://thread/claude%3Arelease)"}]}
        """#.utf8))
        let text = "Compare @rel"
        let trigger = ComposerTrigger.detect(in: text, selection: NSRange(location: text.utf16.count, length: 0), commands: true, files: true)!
        XCTAssertEqual(trigger.replacing(in: text, with: threads.entries[0].mention)?.text,
                       "Compare [@Release notes](cupola://thread/claude%3Arelease) ")
        XCTAssertEqual(threads.entries[0].id, "claude:release")
    }
}
