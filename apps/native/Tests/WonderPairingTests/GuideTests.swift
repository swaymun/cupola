import XCTest
@testable import WonderPairing
final class GuideTests: XCTestCase {
    func testGuideTargetSurvivesRestartAndCannotBecomeSend() throws {
        var intent = ComposerIntent(); intent.draft = "Change direction"
        try intent.begin(device: "phone", expectedTurnId: "old-turn")
        let restored = try JSONDecoder().decode(ComposerIntent.self, from: JSONEncoder().encode(intent))
        XCTAssertEqual(restored.pending?.request.expectedTurnId, "old-turn")
        XCTAssertEqual(restored.pending?.request.clientMessageId, intent.pending?.request.clientMessageId)
        XCTAssertEqual(restored.pending?.request.body, "Change direction")
        var next = restored; next.draft = "Separate follow-up"
        XCTAssertThrowsError(try next.begin(device: "phone", expectedTurnId: "new-turn"))
        XCTAssertEqual(next.draft, "Separate follow-up")
    }
    func testDefinitivelyRejectedGuideRestoresOnlyWhenItCannotOverwriteDraft() throws {
        var intent = ComposerIntent(); intent.draft = "Original Guide"
        try intent.begin(device: "phone", expectedTurnId: "turn")
        intent.draft = "New draft"
        intent.markRejected()
        XCTAssertThrowsError(try intent.restoreRejected())
        XCTAssertEqual(intent.draft, "New draft")
        intent.draft = ""
        try intent.restoreRejected()
        XCTAssertEqual(intent.draft, "Original Guide")
        XCTAssertNil(intent.pending)
    }
}
