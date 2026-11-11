import XCTest
@testable import WonderPairing

// Contract: a plain Send steers by default; an explicit Queue choice waits behind a
// running reply the agent can be guided in, but never takes a thread over.
final class FollowUpBehaviorTests: XCTestCase {

    func testPlainSendSteersOnlyWhenChosenAndGuidable() {
        XCTAssertFalse(FollowUpBehavior.plainSendSteers(choice: FollowUpBehavior.queue, offersTakeOver: false, canGuide: true))
        XCTAssertTrue(FollowUpBehavior.plainSendSteers(choice: FollowUpBehavior.steer, offersTakeOver: false, canGuide: true))
        XCTAssertFalse(FollowUpBehavior.plainSendSteers(choice: FollowUpBehavior.steer, offersTakeOver: false, canGuide: false),
                       "Nothing to steer: the message queues")
        XCTAssertFalse(FollowUpBehavior.plainSendSteers(choice: FollowUpBehavior.steer, offersTakeOver: true, canGuide: true),
                       "Taking over from the Mac always asks through Send now")
        XCTAssertFalse(FollowUpBehavior.plainSendSteers(choice: "unknown", offersTakeOver: false, canGuide: true))
    }
}

// Contract (owner, Oct 21; B-075): Stop takes Send's place while a stoppable
// reply runs and nothing is typed, spoken or attached; anything to send brings
// Send back (it queues or steers) with Stop still beside it, so the reply can
// be stopped without clearing the draft.
final class ComposerTrailingButtonTests: XCTestCase {
}
