import XCTest
@testable import WonderPairing

final class TurnLifecycleTests: XCTestCase {
    func testNativeThreadLevelPreservesUnknownAndDoesNotRewriteTurnOutcomes() throws {
        struct Activity: Decodable { let threadStatus: NativeThreadStatus?; let latestTurnStatus: String }
        for (type, expected) in [("active", true), ("idle", false), ("notLoaded", nil), ("systemError", nil), ("future", nil)] as [(String, Bool?)] {
            let activity = try JSONDecoder().decode(Activity.self, from: Data("{\"latestTurnStatus\":\"completed\",\"threadStatus\":{\"type\":\"\(type)\"}}".utf8))
            XCTAssertEqual(activity.threadStatus?.isWorking, expected, type)
            XCTAssertEqual(activity.latestTurnStatus, "completed")
        }
        let older = try JSONDecoder().decode(Activity.self, from: Data(#"{"latestTurnStatus":"completed"}"#.utf8))
        XCTAssertNil(older.threadStatus, "An older host supplies no native session level")
    }

    func testReadTurnRetainsAuthoritativeDaemonStatusAndReadsOlderCaches() throws {
        let turn = try JSONDecoder().decode(ReadTurn.self, from: Data(#"{"id":"turn","status":"inProgress","items":[]}"#.utf8))
        XCTAssertEqual(turn.status, "inProgress")
        XCTAssertTrue(turn.isInProgress)

        let old = try JSONDecoder().decode(ReadTurn.self, from: Data(#"{"id":"old","items":[]}"#.utf8))
        XCTAssertEqual(old.status, "unknown")
        XCTAssertFalse(old.isInProgress)
    }

    // A turn running in a desktop app is active but owned elsewhere; older
    // payloads without the field stay ordinary turns.
    func testTurnRunningElsewhereIsActiveButOwnedByTheMac() throws {
        let elsewhere = try JSONDecoder().decode(ReadTurn.self, from: Data(#"{"id":"desk","status":"inProgress","runningElsewhere":true,"items":[]}"#.utf8))
        XCTAssertTrue(elsewhere.runningElsewhere)
        XCTAssertTrue(snapshot(messages: [], turns: [elsewhere]).activeTurnRunsElsewhere)
        let local = try JSONDecoder().decode(ReadTurn.self, from: Data(#"{"id":"local","status":"inProgress","items":[]}"#.utf8))
        XCTAssertFalse(local.runningElsewhere)
        let active = snapshot(messages: [], turns: [local])
        XCTAssertEqual(active.activeTurnID, "local")
        XCTAssertFalse(active.activeTurnRunsElsewhere)
        let restored = try JSONDecoder().decode(ReadTurn.self, from: JSONEncoder().encode(elsewhere))
        XCTAssertTrue(restored.runningElsewhere)
    }

    func testStaleReceiptsDoNotReactivateStoppedOrCompletedTurns() {
        let snapshot = snapshot(
            messages: [
                message("completed-receipt", turn: "turn-completed", state: "streaming"),
                message("stopped-receipt", turn: "turn-stopped", state: "accepted_by_codex")
            ],
            turns: [
                turn("turn-completed", status: "completed"),
                turn("turn-stopped", status: "interrupted")
            ])

        XCTAssertTrue(snapshot.activeTurnIDs.isEmpty)
        XCTAssertNil(snapshot.activeTurnID)
    }

    func testUnassignedAcceptanceAndDispatchKeepComposerWorkingWithoutAnActiveTurn() {
        let snapshot = snapshot(
            messages: [
                message("accepted", turn: nil, state: "accepted_by_wonder"),
                message("dispatching", turn: nil, state: "dispatching_to_codex")
            ],
            turns: [])

        XCTAssertTrue(snapshot.hasUnassignedPreTurnWork)
        XCTAssertTrue(snapshot.activeTurnIDs.isEmpty)
        XCTAssertNil(snapshot.activeTurnID)
    }

    func testNewestTerminalTurnSuppressesOlderActiveTurnAfterSteerAndReplay() throws {
        let first = turn("turn-first", status: "completed", items: [row("first-item", type: "commandExecution")])
        let steered = turn("turn-steered", status: "inProgress", items: [row("steered-item", type: "webSearch", state: "streaming")])
        let replayed = turn("turn-late", status: "interrupted", items: [row("late-item", type: "commandExecution")])
        let snapshot = snapshot(
            messages: [
                message("first-receipt", turn: "turn-first", state: "streaming"),
                message("steered-receipt", turn: "turn-steered", state: "completed"),
                message("late-receipt", turn: "turn-late", state: "streaming")
            ],
            turns: [first, steered, replayed])

        XCTAssertNil(snapshot.activeTurnID)
        XCTAssertTrue(snapshot.activeTurnIDs.isEmpty)

        let older = ThreadProjection(nextCursor: nil, hydrated: true, turns: [
            turn("turn-steered", status: "inProgress", items: [row("old-steered", type: "commandExecution")])
        ])
        let newer = ThreadProjection(nextCursor: nil, hydrated: true, turns: [
            turn("turn-steered", status: "completed", items: [row("new-steered", type: "agentMessage")])
        ])
        let merged = newer.mergingOlder(older)
        XCTAssertEqual(merged.turns?.first?.status, "completed")
        XCTAssertEqual(merged.turns?.first?.items.map(\.id), ["old-steered", "new-steered"])

        let unknown = ThreadProjection(nextCursor: nil, hydrated: true, turns: [
            turn("turn-steered", status: "unknown", items: [row("current-unknown", type: "commandExecution")])
        ])
        let unknownMerge = unknown.mergingOlder(older)
        XCTAssertEqual(unknownMerge.turns?.first?.status, "unknown")
        XCTAssertNil(unknownMerge.turns?.first?.terminalLabel)
        XCTAssertTrue(unknownMerge.turns?.first?.isInProgress == false)

        // A reply running in another app on the Mac keeps that ownership
        // through a refresh or paging merge, so Cupola does not offer Guide/Stop.
        let elsewhere = ThreadProjection(nextCursor: nil, hydrated: true, turns: [
            ReadTurn(id: "turn-steered", items: [row("desktop", type: "agentMessage")], status: "inProgress", runningElsewhere: true)
        ])
        XCTAssertEqual(elsewhere.mergingOlder(older).turns?.first?.runningElsewhere, true)
    }

    func testOlderInProgressTurnAndNewerCompletedTurnHaveNoActiveTurn() throws {
        let cached = snapshot(messages: [], turns: [
            turn("turn-old", status: "inProgress", items: [row("old-item", type: "commandExecution")]),
            turn("turn-new", status: "completed", items: [row("new-item", type: "agentMessage")])
        ])
        let newest = ConversationSnapshot(
            conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: "older", hydrated: true,
                                     turns: [turn("turn-new", status: "completed", items: [row("new-reply", type: "agentMessage")])]))

        let merged = try newest.mergingOlder(cached)

        XCTAssertNil(merged.activeTurnID)
        XCTAssertTrue(merged.activeTurnIDs.isEmpty)
        XCTAssertEqual(merged.thread.turns?.map(\.id), ["turn-old", "turn-new"])
        XCTAssertEqual(merged.thread.turns?.first?.items.map(\.id), ["old-item"])
        XCTAssertEqual(merged.thread.turns?.first?.status, "inProgress")
        XCTAssertEqual(merged.thread.turns?.last?.items.map(\.id), ["new-item", "new-reply"])
        XCTAssertEqual(merged.thread.turns?.last?.status, "completed")
    }

    func testOlderCompletedTurnAndNewestInProgressTurnHaveActiveTurn() throws {
        let cached = snapshot(messages: [], turns: [
            turn("turn-old", status: "completed", items: [row("old-item", type: "commandExecution")])
        ])
        let newest = ConversationSnapshot(
            conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: "older", hydrated: true,
                                     turns: [turn("turn-current", status: "inProgress", items: [row("current-item", type: "webSearch", state: "streaming")])]))

        let merged = try newest.mergingOlder(cached)

        XCTAssertEqual(merged.activeTurnID, "turn-current")
        XCTAssertEqual(merged.activeTurnIDs, ["turn-current"])
        XCTAssertEqual(merged.thread.turns?.last?.status, "inProgress")
    }

    func testMergeWithOnlyNonTurnNewestPageKeepsCachedActiveTurnActionable() throws {
        let cached = snapshot(messages: [], turns: [
            turn("turn-current", status: "inProgress", items: [row("current-item", type: "webSearch", state: "streaming")])
        ])
        let newest = ConversationSnapshot(
            conversationId: "chat", hostEpoch: "epoch", lastSequence: 3,
            messages: [message("receipt", turn: "turn-current", state: "streaming")], assistantMessages: [],
            thread: ThreadProjection(nextCursor: "older", hydrated: true, turns: nil))

        let merged = try newest.mergingOlder(cached)

        XCTAssertEqual(merged.activeTurnID, "turn-current")
        XCTAssertEqual(merged.activeTurnIDs, ["turn-current"])
        XCTAssertEqual(merged.thread.turns?.first?.status, "inProgress")
        XCTAssertEqual(merged.thread.turns?.first?.items.map(\.id), ["current-item"])
    }

    func testMergePreservesHistoricalStatusesAndItemsWhileIgnoringStaleActiveTurn() throws {
        let cached = snapshot(messages: [], turns: [
            turn("turn-old", status: "inProgress", items: [row("old-cached", type: "commandExecution")])
        ])
        let newest = ConversationSnapshot(
            conversationId: "chat", hostEpoch: "epoch", lastSequence: 3,
            messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: "older", hydrated: true,
                                     turns: [turn("turn-new", status: "completed", items: [row("new-reply", type: "agentMessage")])]))
        let hydratedOlder = ConversationSnapshot(
            conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: nil, hydrated: true,
                                     turns: [turn("turn-old", status: "inProgress", items: [row("old-hydrated", type: "webSearch", state: "waiting")])]))

        let hydrated = try newest.mergingOlder(hydratedOlder)
        let merged = try hydrated.mergingOlder(cached)

        XCTAssertNil(merged.activeTurnID)
        XCTAssertTrue(merged.activeTurnIDs.isEmpty)
        XCTAssertEqual(merged.thread.turns?.map(\.id), ["turn-old", "turn-new"])
        XCTAssertEqual(merged.thread.turns?.first?.items.map(\.id), ["old-cached", "old-hydrated"])
        XCTAssertEqual(merged.thread.turns?.first?.status, "inProgress")
        XCTAssertEqual(merged.thread.turns?.last?.status, "completed")
    }

    func testLatestCanonicalTurnWinsWhenReplayLeavesMultipleTurnsInProgress() {
        let oldActive = turn("turn-old-active", status: "inProgress", items: [row("old-command", type: "commandExecution")])
        let canonical = turn("turn-canonical", status: "inProgress", items: [row("canonical-command", type: "commandExecution")])
        let rows = [
            feedRow("old-command", type: "commandExecution", turn: "turn-old-active", timestamp: "1000"),
            feedRow("canonical-command", type: "commandExecution", turn: "turn-canonical", timestamp: "2000")
        ]
        let snapshot = snapshot(
            messages: [
                message("old-late-receipt", turn: "turn-old-active", state: "streaming"),
                message("canonical-receipt", turn: "turn-canonical", state: "completed")
            ],
            turns: [oldActive, canonical])
        let entries = ChatFeedEntry.grouping(rows, activeTurnIDs: snapshot.activeTurnIDs)

        XCTAssertEqual(snapshot.activeTurnIDs, ["turn-canonical"])
        XCTAssertEqual(snapshot.activeTurnID, "turn-canonical")
        XCTAssertEqual(ChatFeedEntry.latestActivityEntryID(entries, turnID: snapshot.activeTurnID), "canonical-command")
        XCTAssertEqual(ChatFeedEntry.lifecycleLabel(rows: entries[0].rows, turn: oldActive,
                                                    isLatestSegmentForTurn: true, isLatestActiveSegment: false), "Ran 1 command")
        XCTAssertEqual(ChatFeedEntry.lifecycleLabel(rows: entries[1].rows, turn: canonical,
                                                    isLatestSegmentForTurn: true, isLatestActiveSegment: true), "Working…")
    }

    func testLocalQueuePlaceholdersCannotReplaceTheActiveRuntimeTurn() throws {
        for state in ["accepted_by_wonder", "dispatching_to_codex", "interrupted", "failed"] {
            let running = snapshot(messages: [message("guide", turn: "runtime", state: "streaming")],
                                   turns: [turn("runtime", status: "inProgress", items: [row("command", type: "commandExecution")])])
            let queuePage = snapshot(messages: [message("queued", turn: nil, state: state)],
                                     turns: [turn("local:queued", status: "unknown")])
            let merged = try queuePage.mergingOlder(running)
            XCTAssertEqual(merged.activeTurnID, "runtime", state)
            XCTAssertEqual(merged.activeTurnIDs, ["runtime"], state)
            XCTAssertNil(merged.latestRequestIssue, state)
            let entries = ChatFeedEntry.grouping(merged.rows(author: "Agent"), activeTurnIDs: merged.activeTurnIDs)
            let latest = try XCTUnwrap(ChatFeedEntry.latestActivityEntryID(entries, turnID: merged.activeTurnID))
            let activity = try XCTUnwrap(entries.first(where: { $0.id == latest }))
            XCTAssertEqual(ChatFeedEntry.lifecycleLabel(rows: activity.rows, turn: merged.thread.turns?.first,
                isLatestSegmentForTurn: true, isLatestActiveSegment: true), "Working…")
        }
        let ended = snapshot(messages: [], turns: [turn("old", status: "inProgress"),
            turn("new", status: "completed"), turn("local:cancelled", status: "unknown")])
        XCTAssertNil(ended.activeTurnID)
    }

    func testCancelledUndispatchedCopiesStayInHistoryButDoNotAppearDelivered() throws {
        let cancelled = message("cancelled", turn: nil, state: "interrupted")
        let item = ReadItem(id: "cancelled", type: "userMessage", state: "interrupted", text: "cancelled", createdAt: "2000",
                            payload: ["clientId": .string("client-cancelled")])
        for turns in [[], [turn("local:cancelled", status: "unknown", items: [item])]] {
            let value = snapshot(messages: [cancelled], turns: turns)
            XCTAssertEqual(value.messages.count, 1)
            XCTAssertTrue(value.rows(author: "Agent").isEmpty)
            XCTAssertNil(value.latestRequestIssue)
        }
        let delivered = snapshot(messages: [message("stopped", turn: "runtime", state: "interrupted")],
                                 turns: [turn("runtime", status: "interrupted")])
        XCTAssertEqual(delivered.rows(author: "Agent").count, 1)
        XCTAssertNil(delivered.latestRequestIssue, "a stopped reply says so in its activity row")
    }

    // Owner (Oct 21): a failed send showed two errors. A message the Mac never
    // delivered says so once, on its bubble with Retry; no line under the thread.
    func testRealFailuresRemainVisibleAndStaleReceiptsCannotOverrideCompletedWork() {
        XCTAssertEqual(snapshot(messages: [message("failed", turn: nil, state: "uncertain")], turns: []).latestRequestIssue,
                       "Your last message may not have been delivered. Check this thread on your computer before sending it again.")
        for state in ["safe_to_retry", "failed"] {
            let refused = snapshot(messages: [message("failed", turn: nil, state: state)], turns: [])
            XCTAssertNil(refused.latestRequestIssue, state)
            XCTAssertNotNil(refused.rows(author: "Agent").first?.undelivered, "the bubble says it wasn’t sent: \(state)")
        }
        let finished = snapshot(messages: [message("stale", turn: "runtime", state: "uncertain"),
                                           message("cancelled", turn: nil, state: "interrupted")],
                                turns: [turn("runtime", status: "completed"), turn("local:cancelled", status: "unknown")])
        XCTAssertNil(finished.latestRequestIssue)
        XCTAssertFalse(finished.hasUnassignedPreTurnWork)
    }

    // Observable contract: the host's queue explanation wins while delivery
    // lacks a turn, including behind another reply. A turn receipt retires it
    // independently of when the Project detail refresh clears its notice.
    func testHostDeliveryNoticeSurvivesRunningWorkAndClearsOnBinding() {
        for hostNotice in ["Codex has queued this message on your Mac.", "Check the thread in Codex on your Mac."] {
            for priorTurns in [[], [turn("earlier", status: "inProgress")]] {
                for unbound in [nil, ""] as [String?] {
                    let waiting = snapshot(messages: [message("queued", turn: unbound, state: "uncertain")], turns: priorTurns)
                    XCTAssertEqual(waiting.pendingDeliveryNotice(hostNotice), hostNotice)
                    XCTAssertEqual(waiting.pendingDeliveryNotice(" \n" + hostNotice + " \n"), hostNotice)
                    XCTAssertNil(waiting.pendingDeliveryNotice(nil))
                    XCTAssertNil(waiting.pendingDeliveryNotice(" \n"))
                }
            }
            for status in ["inProgress", "completed", "failed", "interrupted"] {
                let bound = snapshot(messages: [message("queued", turn: "received", state: "uncertain")],
                                     turns: [turn("received", status: status)])
                XCTAssertNil(bound.pendingDeliveryNotice(hostNotice), status)
            }
            for state in ["accepted_by_wonder", "dispatching_to_codex", "safe_to_retry", "failed", "interrupted", "completed"] {
                XCTAssertNil(snapshot(messages: [message("other", turn: nil, state: state)], turns: [])
                    .pendingDeliveryNotice(hostNotice), state)
            }
        }
    }

    func testDeliveryNoticeWaitsForTheRemainingUnboundMessage() {
        let notice = "Codex has queued your messages on your Mac."
        let partiallyBound = snapshot(messages: [message("first", turn: nil, state: "uncertain"),
                                                 message("second", turn: "received", state: "accepted_by_codex")],
                                     turns: [turn("received", status: "inProgress")])
        XCTAssertEqual(partiallyBound.pendingDeliveryNotice(notice), notice)
        XCTAssertNil(snapshot(messages: [], turns: []).pendingDeliveryNotice(notice))
    }

    func testApprovalsAreNotConversationMessages() {
        // An approval is answered on its own card; history keeps the record but
        // the conversation shows no "Request closed." line for it.
        let value = snapshot(messages: [], turns: [turn("runtime", status: "completed", items: [
            row("asked", type: "approval", state: "waiting"), row("answered", type: "approval"), row("reply", type: "agentMessage")])])
        XCTAssertEqual(value.rows(author: "Agent").map(\.text), ["A reply"])
    }

    private func snapshot(messages: [ConversationMessage], turns: [ReadTurn]) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
                             messages: messages, assistantMessages: [],
                             thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: turns))
    }

    private func message(_ id: String, turn: String?, state: String) -> ConversationMessage {
        ConversationMessage(clientMessageId: "client-" + id, codexTurnId: turn, messageId: id,
                            body: id, state: state, createdAt: "1000", attachmentIds: [])
    }

    private func turn(_ id: String, status: String, items: [ReadItem] = [], startedAt: String? = nil, completedAt: String? = nil) -> ReadTurn {
        ReadTurn(id: id, items: items, startedAt: startedAt, completedAt: completedAt, status: status)
    }

    private func row(_ id: String, type: String, state: String = "completed", turn: String = "turn", timestamp: String = "1000") -> ReadItem {
        ReadItem(id: id, type: type, state: state, text: type == "agentMessage" ? "A reply" : nil,
                 createdAt: timestamp, payload: type == "commandExecution" ? ["command": .string("example")] : nil)
    }

    private func feedRow(_ id: String, type: String, state: String = "completed", turn: String = "turn",
                         timestamp: String = "1000", payload: [String: ThreadValue] = [:]) -> ReadRow {
        ReadRow(id: id, author: "Ada", text: type == "agentMessage" ? "A reply" : "",
                isUser: false, timestamp: timestamp, turnId: turn,
                item: ReadItem(id: id, type: type, state: state, text: type == "agentMessage" ? "A reply" : nil,
                               createdAt: timestamp, payload: payload))
    }
}
