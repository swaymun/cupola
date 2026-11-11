import XCTest
@testable import WonderPairing

// Contract: the host sends a window of 0 (an older host sent null) when the provider reported a
// request's tokens but not the window; the phone keeps that as tokens only,
// and saves the last reading per chat so an opening chat shows it at once.
final class ContextUsageTests: XCTestCase {
    func testAReadingWithoutAWindowShowsTokensOnly() throws {
        let json = #"{"usedTokens":61600,"windowTokens":null,"atMs":5}"#
        let usage = try XCTUnwrap(JSONDecoder().decode(ContextUsage.self, from: Data(json.utf8)).known)
        XCTAssertFalse(usage.hasWindow)
        XCTAssertEqual(usage.fraction, 0)
        XCTAssertEqual(usage.summary, "62K tokens")
        XCTAssertEqual(usage.accessibilityValue, "61,600 tokens used, window size unknown")
        let full = try JSONDecoder().decode(ContextUsage.self, from: Data(#"{"usedTokens":84210,"windowTokens":258400}"#.utf8))
        XCTAssertEqual(full.percent, 33)
        XCTAssertFalse(full.windowIsTypical)
        let typical = try JSONDecoder().decode(ContextUsage.self, from: Data(#"{"usedTokens":84210,"windowTokens":1000000,"windowIsTypical":true}"#.utf8))
        XCTAssertEqual(typical.accessibilityValue, "8 percent used, 84,210 of 1,000,000 tokens, typical window for this model")
        XCTAssertNil(try JSONDecoder().decode(ContextUsage.self, from: Data(#"{"usedTokens":0,"windowTokens":null}"#.utf8)).known)
    }

    func testReadingsSurviveARestartAndKeepTheNewest() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("context-usage-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        XCTAssertEqual(try store.loadContextUsage(), [:])
        let readings = ["a": ContextUsage(usedTokens: 10, windowTokens: 100, atMs: 1),
                        "b": ContextUsage(usedTokens: 20, windowTokens: 0, atMs: 2)]
        try store.saveContextUsage(readings)
        XCTAssertEqual(try ReadStore(root: root, host: "mac", device: "phone").loadContextUsage(), readings)
        var many: [String: ContextUsage] = [:]
        for index in 0..<200 { many["chat-\(index)"] = ContextUsage(usedTokens: 1, windowTokens: 2, atMs: Int64(index)) }
        let kept = many.trimmedToNewest()
        XCTAssertEqual(kept.count, 128)
        XCTAssertNotNil(kept["chat-199"])
        XCTAssertNil(kept["chat-0"])
        try store.saveContextUsage([:])
        XCTAssertEqual(try store.loadContextUsage(), [:])
    }

    func testASaveQueuedDuringForgetDoesNotRecreateTheRemovedStore() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("context-usage-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let writer = IntentWriter(store: store)
        let reading = ["a": ContextUsage(usedTokens: 10, windowTokens: 100, atMs: 1)]
        writer.saveContextUsage(reading)
        await writer.flush()
        XCTAssertEqual(try ReadStore(root: root, host: "mac", device: "phone").loadContextUsage(), reading)
        await writer.discard()
        try store.remove()
        writer.saveContextUsage(reading)
        await writer.flush()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path))
    }
}
