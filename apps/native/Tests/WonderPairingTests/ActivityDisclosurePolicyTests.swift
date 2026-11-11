import XCTest
@testable import WonderPairing

final class ActivityDisclosurePolicyTests: XCTestCase {
    func testTurnLifecycleMappingNeverTreatsUnknownAsTerminal() {
        XCTAssertEqual(ActivityDisclosurePolicy.lifecycle(status: "inProgress"), .active)
        XCTAssertEqual(ActivityDisclosurePolicy.lifecycle(status: "completed"), .completed)
        XCTAssertEqual(ActivityDisclosurePolicy.lifecycle(status: "interrupted"), .interrupted)
        XCTAssertEqual(ActivityDisclosurePolicy.lifecycle(status: "failed"), .failed)
        XCTAssertEqual(ActivityDisclosurePolicy.lifecycle(status: "quiet"), .unknown)
        XCTAssertFalse(ActivityDisclosurePolicy.lifecycle(status: nil).isTerminal)
    }

    private func entry(_ turnID: String, _ entryID: String, _ lifecycle: ActivityDisclosurePolicy.Lifecycle) -> ActivityDisclosurePolicy.Entry {
        entry("chat", turnID, entryID, lifecycle)
    }

    private func entry(
        _ conversationID: String,
        _ turnID: String,
        _ entryID: String,
        _ lifecycle: ActivityDisclosurePolicy.Lifecycle,
        autoOpenWhileActive: Bool = true
    ) -> ActivityDisclosurePolicy.Entry {
        ActivityDisclosurePolicy.Entry(
            conversationID: conversationID,
            turnID: turnID,
            entryID: entryID,
            lifecycle: lifecycle,
            autoOpenWhileActive: autoOpenWhileActive
        )
    }

}
