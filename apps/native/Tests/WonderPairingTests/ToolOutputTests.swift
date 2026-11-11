import XCTest
@testable import WonderPairing

/// The output reader's pages and how an open tool panel takes new text.
final class ToolOutputTests: XCTestCase {
    // Every character of the output is on exactly one page, in order, and no
    // page is larger than the limit: VoiceOver can read all of it.
    func testPagesKeepEveryCharacterOfMultilineOutputLongDiffsAndLargeText() {
        let multiline = (0..<5_000).map { "Output line \($0): a deterministic tool result." }.joined(separator: "\n")
        let diff = (0..<3_000).map { ($0.isMultiple(of: 2) ? "+" : "-") + "line \($0)" }.joined(separator: "\n")
        let unbroken = String(repeating: "☃e\u{301}👩‍👩‍👧x", count: 9_000)
        let crlf = (0..<2_000).map { "row \($0)" }.joined(separator: "\r\n")
        for text in [multiline, diff, unbroken, crlf] {
            let pages = ToolOutputPages.split(text)
            XCTAssertEqual(pages.joined(), text)
            XCTAssertGreaterThan(pages.count, 1)
            XCTAssertTrue(pages.allSatisfy { !$0.isEmpty && $0.count <= ToolOutputPages.characterLimit })
        }
        // Lines stay whole when they fit on a page.
        let pages = ToolOutputPages.split(multiline)
        XCTAssertTrue(pages.dropLast().allSatisfy { $0.hasSuffix("\n") })
        XCTAssertTrue(pages[0].hasPrefix("Output line 0: "))
        XCTAssertTrue(pages[1].hasPrefix("Output line "))
        XCTAssertTrue(pages.last?.hasSuffix("Output line 4999: a deterministic tool result.") == true)
        XCTAssertTrue(ToolOutputPages.split(crlf).dropLast().allSatisfy { $0.hasSuffix("\r\n") })
        XCTAssertTrue(ToolOutputPages.split(diff).allSatisfy { $0.hasPrefix("+") || $0.hasPrefix("-") })
    }


    private struct Fixture: Decodable { let compactItem: ReadItem; let fullOutput: ToolOutput }
    private func fixture() throws -> Fixture {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Fixture.self, from: Data(contentsOf: root.appendingPathComponent("tests/contracts/native/tool-output.json")))
    }
    private func row(_ item: ReadItem) -> ReadRow {
        ReadRow(id: "turn/tool", author: "Agent", text: "", isUser: false, timestamp: "1000", turnId: "turn", item: item)
    }
    private actor Loads {
        var identities: [ToolOutputIdentity] = []
        func record(_ identity: ToolOutputIdentity) { identities.append(identity) }
    }

    func testExpandingCompactOutputFetchesAndDisplaysTheCompleteSavedText() async throws {
        let fixture = try fixture(), loads = Loads()
        let row = row(fixture.compactItem), full = fixture.fullOutput
        let load: @Sendable (ToolOutputIdentity) async throws -> ToolOutput = { identity in
            await loads.record(identity)
            return full
        }
        let collapsed = try await ToolOutputDetail.prepare(row: row, expanded: false, conversationId: full.conversationId, load: load)
        XCTAssertNil(collapsed)
        let initialCount = await loads.identities.count
        XCTAssertEqual(initialCount, 0)
        let expanded = try await ToolOutputDetail.prepare(row: row, expanded: true, conversationId: full.conversationId, load: load)
        XCTAssertEqual(expanded?.detail(.output), full.output)
        XCTAssertTrue(expanded?.commandPanelText?.hasSuffix("END OF FULL OUTPUT") == true)
        XCTAssertEqual(row.item?.payload?["output"]?.string?.utf8.count, 2048, "The saved preview must stay compact")
        let identities = await loads.identities
        XCTAssertEqual(identities, [ToolOutputIdentity(conversationId: full.conversationId, turnId: "turn", itemId: "tool")])
    }

    func testShortAndLegacyOutputNeedsNoRequestAndCompactEnvelopeTextSurvives() async throws {
        for payload in ["", ",\"payload\":{\"id\":\"message\",\"text\":\"Hello\"}"] {
            let item = try JSONDecoder().decode(ReadItem.self, from: Data("{\"id\":\"message\",\"type\":\"agentMessage\",\"state\":\"completed\",\"text\":\"Hello\",\"createdAt\":\"1\"\(payload)}".utf8))
            XCTAssertEqual(item.text, "Hello")
            XCTAssertEqual(item.id, "message")
        }
        var item = try fixture().compactItem
        item.payload?["output"] = .string("short output")
        item.payload?["outputTruncated"] = nil
        let presentation = try await ToolOutputDetail.prepare(row: row(item), expanded: true, conversationId: "chat")
        XCTAssertEqual(presentation?.detail(.output), "short output")
    }

    func testFailedReadCanRetryAndWrongItemOrByteCountCannotBeDisplayed() async throws {
        let fixture = try fixture(), full = fixture.fullOutput
        let row = row(fixture.compactItem)
        do {
            _ = try await ToolOutputDetail.prepare(row: row, expanded: true, conversationId: full.conversationId) { _ in throw URLError(.notConnectedToInternet) }
            XCTFail("An unavailable full output must not look complete")
        } catch is URLError {}
        for invalid in [
            ToolOutput(conversationId: "other", turnId: full.turnId, itemId: full.itemId, output: full.output, outputBytes: full.outputBytes),
            ToolOutput(conversationId: full.conversationId, turnId: "other", itemId: full.itemId, output: full.output, outputBytes: full.outputBytes),
            ToolOutput(conversationId: full.conversationId, turnId: full.turnId, itemId: "other", output: full.output, outputBytes: full.outputBytes),
            ToolOutput(conversationId: full.conversationId, turnId: full.turnId, itemId: full.itemId, output: full.output, outputBytes: 1)
        ] {
            do {
                _ = try await ToolOutputDetail.prepare(row: row, expanded: true, conversationId: full.conversationId) { _ in invalid }
                XCTFail("An unrelated or incomplete response must be rejected")
            } catch ToolOutputDetail.Failure.mismatchedResponse {}
        }
        let retried = try await ToolOutputDetail.prepare(row: row, expanded: true, conversationId: full.conversationId) { _ in full }
        XCTAssertEqual(retried?.detail(.output), full.output)
    }

    func testCancelledExpansionDiscardsEvenATransportThatIgnoresCancellation() async throws {
        let fixture = try fixture(), full = fixture.fullOutput, row = row(fixture.compactItem)
        let entered = expectation(description: "fetch entered")
        let release = AsyncStream<Void>.makeStream()
        let task = Task {
            try await ToolOutputDetail.prepare(row: row, expanded: true, conversationId: full.conversationId) { _ in
                entered.fulfill()
                // A detached read deliberately ignores the expansion's cancellation.
                await Task.detached { for await _ in release.stream { break } }.value
                return full
            }
        }
        await fulfillment(of: [entered], timeout: 10)
        task.cancel(); release.continuation.yield(()); release.continuation.finish()
        do { _ = try await task.value; XCTFail("Cancelled output was applied") }
        catch is CancellationError {}
    }

    func testOutputPathEscapesOpaqueIdentities() {
        XCTAssertEqual(ToolOutputIdentity(conversationId: "chat/a", turnId: "turn&x=1", itemId: "item?#").path,
                       "/api/v1/conversations/chat%2Fa/items/item%3F%23/output?turnId=turn%26x%3D1")
    }
}
