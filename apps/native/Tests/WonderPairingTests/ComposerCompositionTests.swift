import XCTest
@testable import WonderPairing

final class ComposerCompositionTests: XCTestCase {
    func testCommitPreservesExternalTranscriptAndSelectedTextReplacement() {
        XCTAssertEqual(ComposerComposition.merge(base: "Hello", committed: "Hello世界", currentDraft: "Hello Spoken transcript."), "Hello世界 Spoken transcript.")
        XCTAssertEqual(ComposerComposition.merge(base: "Hello friend", committed: "Hello 友達", currentDraft: "Hello friend Spoken transcript."), "Hello 友達 Spoken transcript.")
        XCTAssertEqual(ComposerComposition.merge(base: "", committed: "こんにちは", currentDraft: "Spoken transcript."), "こんにちは Spoken transcript.")
    }
    func testCommitWithoutExternalChangeAndCancelledCompositionKeepExactText() {
        XCTAssertEqual(ComposerComposition.merge(base: "Café ", committed: "Café 👨‍👩‍👧‍👦", currentDraft: "Café "), "Café 👨‍👩‍👧‍👦")
        XCTAssertEqual(ComposerComposition.merge(base: "Hello", committed: "Hello", currentDraft: "Hello Transcript."), "Hello Transcript.")
        XCTAssertEqual(ComposerComposition.merge(base: "Hello ", committed: "Hello世界", currentDraft: "Hello Transcript."), "Hello世界 Transcript.")
    }
    func testDraftClearDoesNotResurrectSentBaseAndPreservesCommittedCharacters() {
        XCTAssertEqual(ComposerComposition.merge(base: "Sent message", committed: "Sent message日本語", currentDraft: ""), "日本語")
        XCTAssertEqual(ComposerComposition.merge(base: "Hello friend", committed: "Hello 友達", currentDraft: "New draft"), "New draft 友達")
    }
    func testRejectedCommitRetriesAgainstClearedOrReplacedDraftWithoutRestoringSentBase() {
        let pending = ComposerComposition.PendingCommit(base: "Sent message", committed: "Sent message日本語")
        var draft = "Sent message Transcript."
        XCTAssertNil(pending.apply(to: draft, save: { _ in draft }), "upload model rejects writes")
        XCTAssertEqual(draft, "Sent message Transcript.")
        draft = ""
        XCTAssertEqual(pending.apply(to: draft, save: { draft = $0; return draft }), "日本語")
        XCTAssertEqual(draft, "日本語")
        let replacement = ComposerComposition.PendingCommit(base: "Hello friend", committed: "Hello 友達")
        draft = "New draft"
        XCTAssertEqual(replacement.apply(to: draft, save: { draft = $0; return draft }), "New draft 友達")
    }
    func testRejectedCommitDoesNotMergeTranscriptIntoItsOriginalInputTwice() {
        let pending = ComposerComposition.PendingCommit(base: "Hello", committed: "Hello世界")
        var draft = "Hello First transcript."
        XCTAssertNil(pending.apply(to: draft, save: { _ in draft }))
        draft += " Second transcript."
        XCTAssertEqual(pending.apply(to: draft, save: { draft = $0; return draft }), "Hello世界 First transcript. Second transcript.")
    }

}
