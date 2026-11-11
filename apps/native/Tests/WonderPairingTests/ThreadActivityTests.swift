import XCTest
@testable import WonderPairing

/// SM-02: "Working" is one derived value, and a live turn the history lacked is running, not unknown.
final class ThreadActivityTests: XCTestCase {
    private let turn = "turn-1"
    private let t0 = Date(timeIntervalSince1970: 1_000)

    private func snapshot(turns: [ReadTurn]) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 10,
            messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: turns), messageRecords: nil)
    }
    private func upsert(_ sequence: UInt64, id: String, type: String, state: String, turn: String) -> ReplayEvent {
        let detail: [String: Any] = ["turnId": turn, "itemId": id, "state": state, "lifecycleAuthority": 2, "itemType": type,
                                     "item": ["id": id, "type": type, "status": state]]
        let json = String(decoding: try! JSONSerialization.data(withJSONObject: detail), as: UTF8.self)
        return ReplayEvent(eventId: String(sequence), hostEpoch: "epoch", sequence: sequence, occurredAt: "2026-10-10T00:00:0\(sequence % 10)Z",
                           conversationId: "chat", event: EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: json),
                           turnId: turn, itemId: id)
    }
    private func installed(_ s: ConversationSnapshot) -> ProjectionState { var p = ProjectionState(); p.install(s); return p }

    // Measured failing before the fix (N3c/N3d): the turn was created "unknown", so Working dropped mid-stream.
    func testALiveToolForATurnTheHistoryLacksKeepsTheChatWorking() throws {
        var p = installed(snapshot(turns: []))
        XCTAssertEqual(try p.consumeLive(upsert(11, id: "cmd", type: "commandExecution", state: "started", turn: turn)), .applied("chat"))
        XCTAssertEqual(p.snapshots["chat"]?.activeTurnID, turn)
    }

    func testANewTurnStreamingAfterACompletedOneKeepsTheChatWorking() throws {
        var p = installed(snapshot(turns: [ReadTurn(id: "turn-0", items: [], status: "completed")]))
        _ = try p.consumeLive(upsert(11, id: "cmd", type: "commandExecution", state: "started", turn: turn))
        _ = try p.consumeLive(upsert(12, id: "msg", type: "agentMessage", state: "streaming", turn: turn))
        XCTAssertEqual(p.snapshots["chat"]?.activeTurnID, turn)
    }

    func testAFinishedFirstItemSaysNothingAboutTheTurn() throws {
        var p = installed(snapshot(turns: []))
        _ = try p.consumeLive(upsert(11, id: "cmd", type: "commandExecution", state: "completed", turn: turn))
        XCTAssertNil(p.snapshots["chat"]?.activeTurnID)
    }

    // Control: the host's real turn end still forces a read, which carries the turn's status.
    func testTheHostsTurnEndStillInvalidatesForARead() throws {
        var p = installed(snapshot(turns: [ReadTurn(id: turn, items: [], status: "inProgress")]))
        let end = ReplayEvent(eventId: "11", hostEpoch: "epoch", sequence: 11, occurredAt: "2026-10-10T00:00:11Z", conversationId: "chat",
                              event: EventBody(type: "message_state", state: "completed"), turnId: turn, itemId: nil)
        XCTAssertEqual(try p.consumeLive(end), .invalidated("chat"))
        XCTAssertTrue(p.dirty.contains("chat"))
    }

    private func working(_ flags: [String]? = nil) -> NativeThreadStatus {
        NativeThreadStatus(type: "active", activeFlags: flags)
    }
    private func derive(turn: Bool = false, pending: Bool = false, host: ThreadActivity.HostReading? = nil,
                        row: Bool = false, at seconds: TimeInterval = 0) -> ThreadActivity {
        ThreadActivity.derive(turnActive: turn, pendingMessage: pending, host: host, rowWorking: row, now: t0.addingTimeInterval(seconds))
    }

    func testEachSourceAloneIsWorkingAndNoSourceIsIdle() {
        XCTAssertEqual(derive(), .idle)
        XCTAssertEqual(derive(turn: true), .working(.turn, since: nil))
        XCTAssertEqual(derive(pending: true), .working(.pendingMessage, since: nil))
        XCTAssertEqual(derive(host: .init(status: working(), answeredAt: t0)), .working(.computer, since: nil))
        XCTAssertEqual(derive(row: true), .working(.row, since: nil))
    }

    func testWaitingOnApprovalIsNeedsInputWhateverElseIsRunning() {
        let waiting = ThreadActivity.HostReading(status: working(["waitingOnApproval"]), answeredAt: t0)
        XCTAssertEqual(derive(turn: true, host: waiting), .needsInput(since: nil))
        XCTAssertTrue(derive(host: waiting).isBusy)
    }

    // Measured failing before the fix (a): a reading from a check that stopped stayed "Working" until another poll answered.
    func testAnAnswerExpiresInsteadOfLingering() {
        let reading = ThreadActivity.HostReading(status: working(), answeredAt: t0)
        XCTAssertTrue(derive(host: reading, at: ThreadActivity.hostReadingLifetime - 1).isBusy)
        XCTAssertEqual(derive(host: reading, at: ThreadActivity.hostReadingLifetime + 1), .idle)
    }

    // Measured failing before the fix (a): one failed poll cleared Working and the next restored it.
    func testAFailedCheckKeepsTheLastAnswerThenBecomesUnknown() {
        let reading = ThreadActivity.HostReading(status: working(), answeredAt: t0).failing(at: t0.addingTimeInterval(15))
        XCTAssertTrue(derive(host: reading, at: 16).isBusy, "no flicker on one failed poll")
        let lapsed = derive(host: reading, at: ThreadActivity.hostReadingLifetime + 1)
        XCTAssertEqual(lapsed, .unknown(since: t0.addingTimeInterval(15)))
        XCTAssertFalse(lapsed.isBusy)
    }

    func testAnotherSourceStillWinsWhileTheCheckFails() {
        let failing = ThreadActivity.HostReading(status: nil, answeredAt: t0).failing(at: t0)
        XCTAssertEqual(derive(turn: true, host: failing, at: 100), .working(.turn, since: nil))
    }

    func testTheFirstFailureKeepsTheEarliestFailureTime() {
        let once = ThreadActivity.HostReading(status: working(), answeredAt: t0).failing(at: t0.addingTimeInterval(5))
        XCTAssertEqual(once.failing(at: t0.addingTimeInterval(9)).failedSince, t0.addingTimeInterval(5))
    }
}
