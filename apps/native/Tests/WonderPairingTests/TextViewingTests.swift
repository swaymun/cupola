import XCTest
@testable import WonderPairing

final class TextViewingTests: XCTestCase {

    func testMarkdownHeadingRunsUseUTF16OffsetsAcrossEmojiAndCRLF() throws {
        let text = "🙂 intro\r\n# Title\n\nplain"
        var chunker = try XCTUnwrap(SyntaxChunker(text: text, fileName: "notes.md"))
        let heading = try XCTUnwrap(chunker.remainingRuns().first { $0.role == .keyword })
        XCTAssertEqual((text as NSString).substring(with: heading.range), "# Title")
        XCTAssertNil(SyntaxChunker(text: text, fileName: "notes.md", maxBytes: 3), "over the limit stays plain")
    }

}
