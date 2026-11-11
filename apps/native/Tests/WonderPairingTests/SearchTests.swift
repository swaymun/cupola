import XCTest
@testable import WonderPairing

final class SearchTests: XCTestCase {
    // Contract: the app decodes the shared search response fixture that the
    // Mac's schema test validates, including native results it opens by
    // attaching their thread.
    func testSearchResponseDecodesFromTheSharedFixture() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/search-response-v1.json"))
        let page = try JSONDecoder().decode(MessageSearchPage.self, from: data)
        XCTAssertEqual(page.results.map(\.identity), ["thread:c0000000-0004", "native_message:u-b0000000-0001:msg_01:1", "message:message-1"])
        XCTAssertEqual(page.partial, true)
        let thread = page.results[0]
        XCTAssertNil(thread.conversationId)
        XCTAssertEqual(thread.projectId, "project-1")
        XCTAssertEqual(thread.reference, "codex:c0000000-0004")
        XCTAssertNil(thread.focus, "a whole-thread match asks the Mac where in the thread")
        XCTAssertEqual(page.results[1].focus, .item("u-b0000000-0001:msg_01:1"))
        XCTAssertEqual(page.results[2].focus, .message("message-1"))
        XCTAssertEqual(thread.snippetRuns, [.init(text: "found the ", isMatch: false), .init(text: "walrus", isMatch: true),
                                            .init(text: " rename", isMatch: false)])
        XCTAssertEqual(thread.updatedDate, Date(timeIntervalSince1970: 1_700_000_001))
    }

    func testSnippetMarkersAreTheOnlyMarkup() {
        let result = MessageSearchResult(kind: "message", id: "m", title: "t", snippet: "<b>a</b> <mark>x</mark><mark>y", conversationId: nil, updatedAt: "1")
        XCTAssertEqual(result.plainSnippet, "<b>a</b> xy")
        XCTAssertEqual(result.snippetRuns.filter(\.isMatch).map(\.text), ["x"])
    }

    func testLaterPagesKeepOneRowPerResult() {
        var page = MessageSearchPage(results: [.init(kind: "message", id: "a", title: "", snippet: nil, conversationId: nil, updatedAt: "2")],
                                     nextCursor: "1")
        page.append(MessageSearchPage(results: [.init(kind: "message", id: "a", title: "", snippet: nil, conversationId: nil, updatedAt: "2"),
                                                .init(kind: "thread", id: "a", title: "", snippet: nil, conversationId: nil, updatedAt: "1")],
                                      nextCursor: nil, partial: true))
        XCTAssertEqual(page.results.map(\.identity), ["message:a", "thread:a"])
        XCTAssertNil(page.nextCursor)
        XCTAssertEqual(page.partial, true)
    }

    func testOccurrencesDecode() throws {
        let data = Data(#"{"supported":true,"occurrences":[{"itemId":"i","turnId":null,"snippet":"a <mark>b</mark>"}]}"#.utf8)
        let decoded = try JSONDecoder().decode(SearchOccurrences.self, from: data)
        XCTAssertTrue(decoded.supported)
        XCTAssertEqual(decoded.occurrences.first?.itemId, "i")
    }
}
