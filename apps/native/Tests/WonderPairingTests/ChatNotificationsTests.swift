import XCTest
@testable import WonderPairing

/// Contract: a chat's notification choice shows at once, is kept only when the
/// computer confirms it, and returns to the old choice when it does not.
final class ChatNotificationsTests: XCTestCase {
    func testChangeShowsAtOnceAndIsKeptWhenConfirmed() throws {
        var choices = ChatNotificationChoices()
        XCTAssertEqual(choices.level("a"), .default)
        let change = try XCTUnwrap(choices.begin("a", to: .follow))
        XCTAssertTrue(choices.isSaving("a"))
        XCTAssertEqual(choices.level("a"), .follow)
        choices.finish(change, savedAs: .follow)
        XCTAssertFalse(choices.isSaving("a"))
        XCTAssertEqual(choices.level("a"), .follow)
    }

    func testFailureRestoresTheOldChoiceAndNamesBoth() throws {
        var choices = ChatNotificationChoices()
        choices.learn("a", .follow)
        let change = try XCTUnwrap(choices.begin("a", to: .mute))
        XCTAssertEqual(choices.level("a"), .mute)
        choices.fail(change)
        XCTAssertEqual(choices.level("a"), .follow)
        XCTAssertFalse(choices.isSaving("a"))
        XCTAssertEqual(ChatNotificationChoices.failureMessage(change), "Couldn’t set notifications to Mute. Still Follow.")
    }

    func testOneChangeAtATimeAndNoChangeToTheSameChoice() throws {
        var choices = ChatNotificationChoices()
        let first = try XCTUnwrap(choices.begin("a", to: .mute))
        XCTAssertNil(choices.begin("a", to: .follow), "already saving")
        XCTAssertNotNil(choices.begin("b", to: .follow), "other chats are independent")
        choices.fail(first)
        XCTAssertNil(choices.begin("a", to: .default), "already the choice")
    }

    func testReadingDoesNotOverrideAChangeInFlightAndStaleResultsAreIgnored() throws {
        var choices = ChatNotificationChoices()
        let stale = try XCTUnwrap(choices.begin("a", to: .mute))
        choices.learn("a", .follow)
        XCTAssertEqual(choices.level("a"), .mute)
        choices.fail(stale)
        let current = try XCTUnwrap(choices.begin("a", to: .follow))
        choices.finish(stale, savedAs: .mute)
        XCTAssertTrue(choices.isSaving("a"), "an older change cannot end a newer one")
        choices.finish(current, savedAs: .follow)
        XCTAssertEqual(choices.level("a"), .follow)
    }

    func testOnlyFollowAndMuteShowABadge() {
        XCTAssertNil(ChatNotificationLevel.default.badgeSymbolName)
        XCTAssertNil(ChatNotificationLevel.default.badgeLabel)
        XCTAssertNotNil(ChatNotificationLevel.follow.badgeLabel)
        XCTAssertNotNil(ChatNotificationLevel.mute.badgeLabel)
        XCTAssertEqual(ChatNotificationLevel(rawValue: "mute"), .mute)
    }
}
