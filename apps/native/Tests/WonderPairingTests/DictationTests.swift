import XCTest
@testable import WonderPairing

final class DictationTests: XCTestCase {
    func testFinalPhrasesAccumulateAndVolatilePhrasesReplaceWithoutDuplicates() {
        var transcript = DictationTranscript()
        for (words, start, end, final, expected) in [
            ("A blue", 0.0, 1.0, false, "A blue"),
            ("A green card.", 0, 2, true, "A green card."),
            (" A new", 2, 3, false, "A green card. A new"),
            (" Another phrase.", 2, 4, true, "A green card. Another phrase."),
            (" Another phrase.", 2, 4, true, "A green card. Another phrase."),
            ("stale", 0, 1, false, "A green card. Another phrase."),
            (" 最後", 4, 5, false, "A green card. Another phrase. 最後"),
            (" 最後。", 4, 6, true, "A green card. Another phrase. 最後。")
        ] {
            transcript.update(words, start: start, end: end, isFinal: final)
            XCTAssertEqual(transcript.text, expected)
        }
    }

    func testProgressiveRevisionsReplaceOnlySelectedSpanWithoutChangingBase() {
        var projection = DictationProjection(base: "Hello old friend 👋", selection: NSRange(location: 6, length: 3))
        XCTAssertTrue(projection.update("blue"))
        XCTAssertEqual(projection.text, "Hello blue friend 👋")
        XCTAssertTrue(projection.update("green card"))
        XCTAssertEqual(projection.text, "Hello green card friend 👋")
        XCTAssertEqual(projection.base, "Hello old friend 👋")
        XCTAssertTrue(projection.update(""))
        XCTAssertEqual(projection.text, projection.base)
    }
    func testProgressiveUnicodeCursorMappingAndLimitKeepLastAcceptedWords() {
        var projection = DictationProjection(base: "👋 tail", selection: NSRange(location: 3, length: 0))
        let original = projection
        XCTAssertTrue(projection.update("こんにちは"))
        XCTAssertEqual(projection.text, "👋 こんにちは tail")
        XCTAssertEqual(projection.selection(afterReplacing: original, selection: NSRange(location: 7, length: 0)),
                       NSRange(location: 13, length: 0))
        let accepted = projection.text
        XCTAssertFalse(projection.update(String(repeating: "x", count: 65537)))
        XCTAssertEqual(projection.text, accepted)
    }


}
