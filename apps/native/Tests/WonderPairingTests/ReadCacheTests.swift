import XCTest
@testable import WonderPairing

func assertRows(_ actual: [ReadRow], equal expected: [ReadRow], file: StaticString = #filePath, line: UInt = #line) {
    XCTAssertEqual(actual.map(\.id), expected.map(\.id), file: file, line: line)
    for (actual, expected) in zip(actual, expected) {
        XCTAssertEqual(actual.text, expected.text, file: file, line: line)
        XCTAssertEqual(actual.author, expected.author, file: file, line: line)
        XCTAssertEqual(actual.authorId, expected.authorId, file: file, line: line)
        XCTAssertEqual(actual.isUser, expected.isUser, file: file, line: line)
        XCTAssertEqual(actual.timestamp, expected.timestamp, file: file, line: line)
        XCTAssertEqual(actual.time, expected.time, file: file, line: line)
        XCTAssertEqual(actual.turnId, expected.turnId, file: file, line: line)
        XCTAssertEqual(actual.item, expected.item, file: file, line: line)
        XCTAssertEqual(actual.attachmentIds, expected.attachmentIds, file: file, line: line)
        XCTAssertEqual(actual.source, expected.source, file: file, line: line)
        XCTAssertEqual(actual.undelivered, expected.undelivered, file: file, line: line)
        XCTAssertEqual(actual.commandSummary, expected.commandSummary, file: file, line: line)
        XCTAssertEqual(actual.commandBlock, expected.commandBlock, file: file, line: line)
        XCTAssertEqual(actual.fileChangeSummary, expected.fileChangeSummary, file: file, line: line)
        XCTAssertEqual(actual.taskChecklist, expected.taskChecklist, file: file, line: line)
        XCTAssertEqual(actual.thinkingTiming?.startedAt, expected.thinkingTiming?.startedAt, file: file, line: line)
        XCTAssertEqual(actual.thinkingTiming?.completedLabel, expected.thinkingTiming?.completedLabel, file: file, line: line)
        XCTAssertEqual(actual.activitySummary?.title, expected.activitySummary?.title, file: file, line: line)
        XCTAssertEqual(actual.activitySummary?.state, expected.activitySummary?.state, file: file, line: line)
        XCTAssertEqual(actual.nativeQuestion?.answers, expected.nativeQuestion?.answers, file: file, line: line)
        XCTAssertEqual(actual.nativeQuestion?.questions.map(\.title), expected.nativeQuestion?.questions.map(\.title), file: file, line: line)
        XCTAssertEqual(actual.toolFiles.map(\.id), expected.toolFiles.map(\.id), file: file, line: line)
    }
}

final class ReadCacheTests: XCTestCase {

    private func streamingSnapshot(turnCount: Int, frame: Int) -> ConversationSnapshot {
        let messages = (0..<turnCount).map { index in
            ConversationMessage(clientMessageId: "client-\(index)", codexTurnId: "turn-\(index)",
                messageId: "message-\(index)", body: "Canonical prompt \(index)", state: "completed",
                createdAt: String(1_700_000_000_000 + index * 10), attachmentIds: ["attachment-\(index)"])
        }
        let turns = (0..<turnCount).map { index in
            ReadTurn(id: "turn-\(index)", items: [
                ReadItem(id: "prompt", type: "userMessage", state: "completed", text: "Runtime prompt",
                    createdAt: String(1_700_000_000_000 + index * 10), payload: ["clientId": .string("client-\(index)")]),
                ReadItem(id: "tool", type: "commandExecution", state: "completed", text: nil,
                    createdAt: String(1_700_000_000_001 + index * 10),
                    payload: ["command": .string("printf 'fixture'"), "output": .string("fixture")]),
                ReadItem(id: "reply", type: "agentMessage", state: index == turnCount - 1 ? "streaming" : "completed",
                    text: "Reply \(index)", createdAt: String(1_700_000_000_002 + index * 10))
            ], status: index == turnCount - 1 ? "inProgress" : "completed")
        }
        let assistants = (0..<turnCount).map { index in
            AssistantMessage(itemId: "reply", codexTurnId: "turn-\(index)", messageId: "assistant-\(index)",
                text: index == turnCount - 1 ? "Streaming frame \(frame)" : "Reply \(index)",
                state: index == turnCount - 1 ? "streaming" : "completed",
                createdAt: String(1_700_000_000_002 + index * 10), updatedAt: index == turnCount - 1 ? String(frame) : "0")
        }
        return ConversationSnapshot(conversationId: "benchmark", hostEpoch: "epoch", lastSequence: UInt64(frame),
            messages: messages, assistantMessages: assistants,
            thread: ThreadProjection(nextCursor: "older", hydrated: true, turns: turns))
    }

    // Contract owner: prepared read-cache rows must equal a fresh projection.
    // Regression: a cache can retain stale text, identity, canonical content or
    // prepared artifacts when a frame changes only part of a snapshot.
    func testPreparedRowsFollowLiveDeltasCompletionAndLateFrames() throws {
        var snapshot = streamingSnapshot(turnCount: 12, frame: 0)
        var projection = ConversationRowProjection()
        let original = projection.rows(for: snapshot, author: "Agent")
        for sequence in 1...8 {
            let event = ReplayEvent(eventId: "event-\(sequence)", hostEpoch: snapshot.hostEpoch,
                sequence: UInt64(sequence), occurredAt: String(2_000 + sequence), conversationId: snapshot.conversationId,
                event: EventBody(type: sequence == 7 ? "assistant_completed" : "assistant_delta",
                    text: sequence == 7 ? "Final reply" : " part \(sequence)"), turnId: "turn-11", itemId: "reply")
            snapshot = try XCTUnwrap(snapshot.applyingLive(event))
            let rows = projection.rows(for: snapshot, author: "Agent")
            assertRows(rows, equal: snapshot.rows(author: "Agent"))
            XCTAssertEqual(rows.map(\.id), original.map(\.id))
            XCTAssertEqual(rows.first?.text, "Canonical prompt 0")
            XCTAssertEqual(rows.first?.attachmentIds, ["attachment-0"])
            if sequence >= 7 { XCTAssertEqual(rows.last?.text, "Final reply") }
        }
    }

    // Contract owner: a tool step in a long history prepares only the rows it
    // changed. Regression: every item event re-prepared every row (command
    // summaries first), so each step cost grew with history (65 ms at 2,100 items).
    func testItemEventsPrepareOnlyChangedRowsAtAnyHistoryLength() throws {
        for turnCount in [40, 700] {
            let base = streamingSnapshot(turnCount: turnCount, frame: 0)
            var projection = ConversationRowProjection()
            _ = projection.rows(for: base, author: "Agent")
            XCTAssertEqual(projection.lastPreparedRowCount, turnCount * 4, "First projection prepares every row and reply")
            var turns = try XCTUnwrap(base.thread.turns)
            let last = turns[turns.count - 1]
            for state in ["started", "completed"] {
                let step = ReadItem(id: "step", type: "commandExecution", state: state, text: nil, createdAt: "1700000009999",
                    payload: ["command": .string("/bin/zsh -lc 'swift test'"), "output": .string(state)])
                turns[turns.count - 1] = ReadTurn(id: last.id, items: last.items + [step], status: last.status)
                let snapshot = replacing(base, thread: ThreadProjection(nextCursor: "older", hydrated: true, turns: turns))
                let rows = projection.rows(for: snapshot, author: "Agent")
                assertRows(rows, equal: snapshot.rows(author: "Agent"))
                XCTAssertEqual(projection.lastPreparedRowCount, 1, "\(turnCount) turns, step \(state)")
            }
        }
    }

    // Every item still shown running is settled, a finished turn's too (the
    // walk is bounded per refresh instead). Regression: excluding finished
    // turns left a tool labeled "Running" without its final output.
    func testUnfinishedItemsOfEveryTurnAreSettled() {
        func turn(_ id: String, _ status: String) -> ReadTurn {
            ReadTurn(id: id, items: [
                ReadItem(id: "tool", type: "commandExecution", state: "started", text: nil, createdAt: "1"),
                ReadItem(id: "done", type: "commandExecution", state: "completed", text: nil, createdAt: "2")], status: status)
        }
        let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1, messages: [],
            assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [
                turn("finished", "completed"), turn("failed", "failed"), turn("stopped", "interrupted"),
                turn("running", "inProgress"), turn("unknown", "unknown")]))
        XCTAssertEqual(snapshot.unsettledItemIDs, ["finished/tool", "failed/tool", "stopped/tool", "running/tool", "unknown/tool"])
    }

    // A refreshed newest page that does not reach back into the saved chat
    // keeps its own older-page cursor. Regression: the saved copy's cursor
    // (nil once fully loaded) replaced it, hiding turns never read.
    func testMergingSavedKeepsTheCursorToAnUnreadGap() throws {
        func page(_ turns: [String], cursor: String?) -> ConversationSnapshot {
            ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 9, messages: [], assistantMessages: [],
                thread: ThreadProjection(nextCursor: cursor, hydrated: true, turns: turns.map {
                    ReadTurn(id: $0, items: [ReadItem(id: "reply", type: "agentMessage", state: "completed", text: $0, createdAt: "1")], status: "completed")
                }))
        }
        let saved = page(["t1", "t2", "t3"], cursor: nil)
        let apart = try page(["t8", "t9"], cursor: "before-t8").mergingSaved(saved)
        XCTAssertEqual(apart.thread.turns?.map(\.id), ["t1", "t2", "t3", "t8", "t9"])
        XCTAssertEqual(apart.thread.nextCursor, "before-t8", "t4 to t7 were never read")
        let joined = try page(["t3", "t4"], cursor: "before-t3").mergingSaved(saved)
        XCTAssertEqual(joined.thread.turns?.map(\.id), ["t1", "t2", "t3", "t4"])
        XCTAssertNil(joined.thread.nextCursor, "The page reaches back into the saved turns")
    }

    // A repair page read out of order settles the saved turns it holds and
    // adds nothing else, so order and paging stay the saved copy's.
    func testRepairPageUpdatesOnlyKnownTurns() {
        func snapshot(_ turns: [ReadTurn], cursor: String?) -> ConversationSnapshot {
            ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 9, messages: [], assistantMessages: [],
                thread: ThreadProjection(nextCursor: cursor, hydrated: true, turns: turns))
        }
        func tool(_ state: String) -> ReadItem { ReadItem(id: "tool", type: "commandExecution", state: state, text: nil, createdAt: "1") }
        let reply = ReadItem(id: "reply", type: "agentMessage", state: "completed", text: "Done", createdAt: "2")
        let saved = snapshot([ReadTurn(id: "t1", items: [tool("started"), reply], status: "completed"),
                              ReadTurn(id: "t9", items: [reply], status: "completed")], cursor: "saved-cursor")
        let repaired = saved.updatingKnownTurns(from: snapshot([
            ReadTurn(id: "t0", items: [reply], status: "completed"),
            ReadTurn(id: "t1", items: [tool("completed")], status: "completed")], cursor: "deeper"))
        XCTAssertEqual(repaired.thread.turns?.map(\.id), ["t1", "t9"])
        XCTAssertEqual(repaired.thread.turns?.first?.items.map(\.id), ["tool", "reply"])
        XCTAssertEqual(repaired.thread.turns?.first?.items.first?.state, "completed")
        XCTAssertEqual(repaired.thread.nextCursor, "saved-cursor")
    }

    func testPreparedRowsInvalidateForAllSnapshotAndPresentationInputs() throws {
        let base = streamingSnapshot(turnCount: 4, frame: 0)
        var projection = ConversationRowProjection()
        _ = projection.rows(for: base, author: "Agent")
        var messages = base.messages
        messages[0] = ConversationMessage(clientMessageId: "client-0", codexTurnId: "turn-0",
            messageId: "message-0", body: "Edited canonical body", state: "failed", createdAt: "42",
            attachmentIds: ["new-photo"], source: MessageSource(kind: "thread", sourceTitle: "Planner"), deliveryError: "Offline")
        var turns = try XCTUnwrap(base.thread.turns)
        var items = turns[0].items
        items[1] = ReadItem(id: items[1].id, type: "commandExecution", state: "failed", text: nil,
            createdAt: items[1].createdAt, payload: ["command": .string("pwd"), "output": .string("Updated output")])
        turns[0] = ReadTurn(id: turns[0].id, items: items, status: "failed")
        let variants = [
            replacing(base, messages: messages), // Same sequence and array counts.
            replacing(base, thread: ThreadProjection(nextCursor: "older", hydrated: true, turns: turns)),
            replacing(base, thread: ThreadProjection(threadId: "replacement", nextCursor: "older", hydrated: false, turns: base.thread.turns)),
            replacing(base, revision: 1), replacing(base, epoch: "replacement-host"),
            try base.mergingOlder(ConversationSnapshot(conversationId: base.conversationId, hostEpoch: base.hostEpoch,
                lastSequence: base.lastSequence, messages: [], assistantMessages: [],
                thread: ThreadProjection(nextCursor: "earlier-page", hydrated: true, turns: [ReadTurn(id: "old", items: [
                    ReadItem(id: "old-reply", type: "agentMessage", state: "completed", text: "Earlier", createdAt: "1")], status: "completed")]))),
            replacing(base, revision: 2, messages: [], assistants: [],
                thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [])),
            replacing(base, id: "other-conversation"), base
        ]
        for snapshot in variants {
            _ = projection.rows(for: base, author: "Agent")
            assertRows(projection.rows(for: snapshot, author: "Agent"), equal: snapshot.rows(author: "Agent"))
        }
        // A title alone changes agent attribution, even at the same sequence.
        assertRows(projection.rows(for: base, author: "New title"), equal: base.rows(author: "New title"))
    }

    func testPreparedRowsPreservePendingDeliveredCancelledAndFirstTokenIdentity() throws {
        let pending = ConversationMessage(clientMessageId: "client", messageId: "stored", body: "Canonical body",
            state: "accepted_by_wonder", createdAt: "1000", attachmentIds: ["photo"])
        let base = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
            messages: [pending], assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: false))
        var projection = ConversationRowProjection()
        XCTAssertEqual(projection.rows(for: base, author: "Agent").map(\.id), ["user-client"])
        var delivered = pending
        delivered.codexTurnId = "turn"
        let item = ReadItem(id: "echo", type: "userMessage", state: "completed", text: "Runtime augmented body",
            createdAt: "9000", payload: ["clientId": .string("client"), "attachmentIds": .array([.string("wrong")])])
        let reply = ReadItem(id: "reply", type: "agentMessage", state: "started", text: nil, createdAt: "2000")
        var snapshot = replacing(base, messages: [delivered], thread: ThreadProjection(nextCursor: nil, hydrated: true,
            turns: [ReadTurn(id: "turn", items: [item, reply], status: "inProgress")]))
        let rows = projection.rows(for: snapshot, author: "Agent")
        XCTAssertEqual(rows.map(\.id), ["user-client"])
        XCTAssertEqual(rows[0].text, "Canonical body")
        XCTAssertEqual(rows[0].attachmentIds, ["photo"])
        XCTAssertEqual(rows[0].timestamp, "1000")
        for (index, text) in ["", "First token", "More text", "", "Restored"].enumerated() {
            snapshot = replacing(snapshot, assistants: [AssistantMessage(itemId: "reply", codexTurnId: "turn",
                messageId: "assistant", text: text, state: "streaming", createdAt: "2000", updatedAt: String(index))])
            assertRows(projection.rows(for: snapshot, author: "Agent"), equal: snapshot.rows(author: "Agent"))
        }
        let cancelled = ConversationMessage(clientMessageId: "client", messageId: "stored", body: pending.body,
            state: "interrupted", createdAt: "1000", attachmentIds: ["photo"])
        snapshot = replacing(snapshot, messages: [cancelled])
        let cancelledRows = projection.rows(for: snapshot, author: "Agent")
        XCTAssertFalse(cancelledRows.contains(where: \.isUser))
        assertRows(cancelledRows, equal: snapshot.rows(author: "Agent"))
    }

    func testPreparedStreamingKeepsReconciledQuestionsAndChronologicalSlot() throws {
        let form = ReadItem(id: "question", type: "agentMessage", state: "completed", text: "Which day?", createdAt: "1000",
            payload: ["delivery": .string("async"), "questions": .array([.object(["title": .string("Which day?")])])])
        let answer = #"<send_user_message_question_reply>[{"questionItemId":"[\"request_user_input_async\",\"question\",0]","question":"Which day?","answer":"Saturday"}]</send_user_message_question_reply>"#
        let pending = ConversationMessage(clientMessageId: "pending", messageId: "pending", body: "Next prompt",
            state: "accepted_by_wonder", createdAt: "4000", attachmentIds: ["pending-photo"])
        let thread = ThreadProjection(nextCursor: nil, hydrated: true, turns: [ReadTurn(id: "turn", items: [form,
            ReadItem(id: "answer", type: "userMessage", state: "completed", text: answer, createdAt: "2000"),
            ReadItem(id: "reply", type: "agentMessage", state: "streaming", text: "Reply", createdAt: "3000")], status: "inProgress")])
        var projection = ConversationRowProjection()
        for frame in 0..<4 {
            let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: UInt64(frame),
                messages: [pending], assistantMessages: [AssistantMessage(itemId: "reply", codexTurnId: "turn", messageId: "assistant",
                    text: "Reply \(frame)", state: "streaming", createdAt: "3000", updatedAt: String(frame))], thread: thread)
            let rows = projection.rows(for: snapshot, author: "Agent")
            XCTAssertEqual(rows.map(\.id), ["turn/question", "turn/reply", "user-pending"])
            XCTAssertEqual(try XCTUnwrap(rows.first?.nativeQuestion).answers, [0: "Saturday"])
            XCTAssertEqual(rows[1].text, "Reply \(frame)")
            XCTAssertEqual(rows.last?.attachmentIds, ["pending-photo"])
            assertRows(rows, equal: snapshot.rows(author: "Agent"))
        }
    }

    func testAssistantIndexKeepsLegacyFirstSuffixMatchAndAppendedRows() {
        let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
            messages: [], assistantMessages: [
                AssistantMessage(itemId: "nested/reply", messageId: "legacy", text: "Legacy first", state: "completed", createdAt: "1000", updatedAt: "1000"),
                AssistantMessage(messageId: "path/extra", text: "Appended", state: "completed", createdAt: "3000", updatedAt: "3000"),
                AssistantMessage(itemId: "extra", messageId: "overlay", text: "Appended replacement", state: "completed", createdAt: "4000", updatedAt: "4000"),
                AssistantMessage(itemId: "nested/reply", codexTurnId: "second", messageId: "qualified", text: "Exact second", state: "completed", createdAt: "2000", updatedAt: "2000")
            ], thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [
                ReadTurn(id: "first/part", items: [ReadItem(id: "nested/reply", type: "agentMessage", state: "completed", text: "First", createdAt: "1000")]),
                ReadTurn(id: "second", items: [ReadItem(id: "nested/reply", type: "agentMessage", state: "completed", text: "Second", createdAt: "2000")])
            ]))
        let rows = snapshot.rows(author: "Agent")
        XCTAssertEqual(rows.map(\.id), ["first/part/nested/reply", "second/nested/reply", "assistant-path/extra"])
        XCTAssertEqual(rows.map(\.text), ["Legacy first", "Exact second", "Appended replacement"])
        XCTAssertEqual(rows.map(\.timestamp), ["1000", "2000", "3000"])
    }

    func testPreparedRowsKeepArtifactsThroughStreamingAndTerminalChangesWithCacheLimits() throws {
        let artifact: ThreadValue = .object(["type": .string("wonderArtifact"), "file": .object([
            "id": .string("chart"), "name": .string("Chart.png"), "mimeType": .string("image/png"),
            "byteSize": .number(42), "sha256": .string("digest"), "state": .string("available"), "updatedAt": .string("1000")])])
        for projection in [ConversationRowProjection(), ConversationRowProjection(maximumEntries: 0),
                           ConversationRowProjection(maximumInputBytes: 1)] {
            var projection = projection
            for (index, status) in ["inProgress", "inProgress", "completed", "interrupted", "failed"].enumerated() {
                let item = ReadItem(id: "reply", type: "agentMessage", state: index < 2 ? "streaming" : "completed", text: nil,
                    createdAt: "1000", payload: ["contentItems": .array([artifact])])
                let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: UInt64(index),
                    messages: [], assistantMessages: [AssistantMessage(itemId: "reply", codexTurnId: "turn", messageId: "assistant",
                        text: "Reply \(index)", state: index < 2 ? "streaming" : "completed", createdAt: "1000", updatedAt: "2000")],
                    thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [ReadTurn(id: "turn", items: [item], status: status)]))
                let rows = projection.rows(for: snapshot, author: "Agent")
                assertRows(rows, equal: snapshot.rows(author: "Agent"))
                XCTAssertEqual(rows.first?.toolFiles.map(\.id), ["chart"])
                XCTAssertEqual(rows.first?.toolFiles.first?.sha256, "digest")
            }
        }
    }

    private func replacing(_ snapshot: ConversationSnapshot, id: String? = nil, epoch: String? = nil, revision: UInt64? = nil,
                           messages: [ConversationMessage]? = nil, assistants: [AssistantMessage]? = nil,
                           thread: ThreadProjection? = nil) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: id ?? snapshot.conversationId, hostEpoch: epoch ?? snapshot.hostEpoch,
            lastSequence: snapshot.lastSequence, historyRevision: revision ?? snapshot.historyRevision,
            messages: messages ?? snapshot.messages, assistantMessages: assistants ?? snapshot.assistantMessages,
            thread: thread ?? snapshot.thread)
    }

    private func assertRows(_ actual: [ReadRow], equal expected: [ReadRow], file: StaticString = #filePath, line: UInt = #line) {
        WonderPairingTests.assertRows(actual, equal: expected, file: file, line: line)
    }

    func testNativeResetReplacesCachedHistoryAndFencesOlderPages() throws {
        let old = snapshot()
        let reset = ConversationSnapshot(conversationId: old.conversationId, hostEpoch: old.hostEpoch,
            lastSequence: old.lastSequence + 1, historyRevision: 1, messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: []))
        let restored = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(reset))
        XCTAssertTrue(try restored.mergingOlder(old).messages.isEmpty)
        XCTAssertEqual(try restored.mergingOlder(old).historyRevision, 1)
        XCTAssertThrowsError(try old.mergingOlder(restored))
        var cache = ProjectionState()
        cache.install(old); cache.install(restored); cache.install(old)
        XCTAssertTrue(cache.snapshots[old.conversationId]!.messages.isEmpty)
        XCTAssertEqual(cache.snapshots[old.conversationId]!.historyRevision, 1)
    }

    func testTaskSnapshotsSurviveCacheRestartAndOlderHistoryWithoutLosingNewestUpdate() throws {
        func snapshot(_ sequence: UInt64, _ completed: Double) -> ConversationSnapshot {
            let item = ReadItem(id: "turn:tasks", type: "taskChecklist", state: "completed", text: nil, createdAt: "1000",
                payload: ["taskChecklist": .object(["entries": .array([.object(["text": .string("Check layout"), "status": .string(completed == 0 ? "pending" : "completed")])]),
                                                     "total": .number(1), "completed": .number(completed)])])
            return ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: sequence, messages: [], assistantMessages: [],
                thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [ReadTurn(id: "turn", items: [item], status: "completed")]))
        }
        let before = snapshot(1, 0), after = snapshot(2, 1)
        let restored = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(after))
        let merged = try restored.mergingOlder(before)
        XCTAssertEqual(merged.rows(author: "Codex").map(\.id), before.rows(author: "Codex").map(\.id))
        XCTAssertEqual(merged.rows(author: "Codex").first?.taskChecklist?.current.countLabel, "1/1")
        XCTAssertEqual(before.rows(author: "Codex").first?.taskChecklist?.current.countLabel, "0/1")
    }

    func testReplyWaitsForFirstTextThenKeepsItsIdentityAndArtifacts() throws {
        let artifact: ThreadValue = .object(["type": .string("wonderArtifact"), "file": .object([
            "id": .string("chart"), "name": .string("Chart.png"), "mimeType": .string("image/png"),
            "byteSize": .number(42), "sha256": .string("digest"), "state": .string("available"),
            "updatedAt": .string("1000")
        ])])
        func rows(_ state: String, text: String?, artifacts: Bool = false, turnStatus: String = "inProgress",
                  type: String = "agentMessage") -> [ReadRow] {
            let item = ReadItem(id: "reply", type: type, state: state, text: text, createdAt: "1000",
                payload: artifacts ? ["contentItems": .array([artifact])] : nil)
            return ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
                messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true,
                    turns: [ReadTurn(id: "turn", items: [item], status: turnStatus)])).rows(author: "Agent")
        }

        for state in ["started", "streaming"] {
            XCTAssertTrue(rows(state, text: nil).isEmpty, "A reply awaiting its first token is not unavailable")
            let image = try XCTUnwrap(rows(state, text: nil, artifacts: true).first)
            XCTAssertEqual(image.text, "")
            XCTAssertEqual(image.id, "turn/reply")
            XCTAssertEqual(image.toolFiles.map(\.id), ["chart"])
            XCTAssertEqual(image.toolFiles.first?.sha256, "digest")
            let files = ChatFeedNode.visible(ChatFeedEntry.grouping([image]), expanded: []).compactMap { node -> String? in
                if case .file(let file) = node.content { return file.id }
                return nil
            }
            XCTAssertEqual(files, ["chart"], "An image can arrive before its reply's first text")
            XCTAssertEqual(rows(state, text: nil, type: "userMessage").first?.text, "Message content unavailable")
        }
        for (state, text, turn) in [("streaming", "First", "inProgress"), ("completed", "First reply", "completed")] {
            let row = try XCTUnwrap(rows(state, text: text, artifacts: true, turnStatus: turn).first)
            XCTAssertEqual(row.id, "turn/reply")
            XCTAssertEqual(row.text, text)
            XCTAssertEqual(row.toolFiles.map(\.id), ["chart"])
        }
        for (state, turn) in [("completed", "completed"), ("failed", "failed"), ("unknown", "unknown"),
                              ("started", "completed"), ("completed", "inProgress")] {
            XCTAssertEqual(rows(state, text: nil, turnStatus: turn).first?.text, "Message content unavailable")
        }
    }

    func testReadingPositionPersistsWithoutRewritingHistoryOrIntent() throws {
        let root=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at:root) }
        let store=ReadStore(root:root,host:"mac",device:"phone")
        try store.save(ProjectionState())
        try store.saveIntent(Data("unsent draft".utf8),conversation:"chat")
        let cache=store.directory.appendingPathComponent("read-cache-v4.json")
        let bytes=try Data(contentsOf:cache)
        let date=try cache.resourceValues(forKeys:[.contentModificationDateKey]).contentModificationDate
        try store.savePosition("activity:row",conversation:"chat")
        let restarted=ReadStore(root:root,host:"mac",device:"phone")
        XCTAssertEqual(try restarted.loadPosition(conversation:"chat"),"activity:row")
        XCTAssertNil(try restarted.loadPosition(conversation:"other"))
        XCTAssertEqual(try Data(contentsOf:cache),bytes)
        XCTAssertEqual(try cache.resourceValues(forKeys:[.contentModificationDateKey]).contentModificationDate,date)
        XCTAssertEqual(try restarted.loadIntent(conversation:"chat"),Data("unsent draft".utf8))
    }
    // Settling a tool item still shown running continues after a relaunch
    // from the saved walk, in the same history only; items a walk could not
    // find are looked for again later rather than called finished.
    func testSettleWalkSurvivesRelaunchInItsOwnHistoryOnly() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let now = Date(timeIntervalSince1970: 1_000_000)
        let walk = SettleRepair(hostEpoch: "epoch", historyRevision: 3, cursor: "page-5", unreachable: ["t/gone"], recordedAt: now)
        try store.saveSettleRepair(walk, conversation: "chat")
        let restarted = ReadStore(root: root, host: "mac", device: "phone")
        let saved = try XCTUnwrap(restarted.loadSettleRepair(conversation: "chat"))
        XCTAssertEqual(saved, walk)
        XCTAssertNil(restarted.loadSettleRepair(conversation: "other"))
        XCTAssertEqual(saved.current(epoch: "epoch", revision: 3, now: now)?.cursor, "page-5")
        XCTAssertNil(saved.current(epoch: "restarted", revision: 3, now: now))
        XCTAssertNil(saved.current(epoch: "epoch", revision: 4, now: now))
        XCTAssertNil(saved.current(epoch: "epoch", revision: 3, now: now.addingTimeInterval(SettleRepair.recheckAfter)))
        // Kept only for items still shown running, and bounded.
        XCTAssertNil(SettleRepair(hostEpoch: "epoch", historyRevision: 3, unreachable: ["t/done"]).bounded(to: ["t/other"]))
        let many = Set((0..<1000).map { "t/\($0)" })
        XCTAssertEqual(SettleRepair(hostEpoch: "epoch", historyRevision: 3, unreachable: many).bounded(to: many)?.unreachable.count,
                       SettleRepair.maximumUnreachable)
        // A walk to the start records what it did not find; a walk the Mac
        // refused (a saved place it no longer takes) only forgets the place.
        let fresh = SettleRepair(hostEpoch: "epoch", historyRevision: 3, cursor: "page-5", recordedAt: now)
        let reachedStart = fresh.afterWalk(missing: ["t/gone"], next: nil, pagesRead: 2, placeRefused: false, now: now)
        XCTAssertEqual(reachedStart.unreachable, ["t/gone"]); XCTAssertNil(reachedStart.cursor)
        let refused = fresh.afterWalk(missing: ["t/gone"], next: nil, pagesRead: 1, placeRefused: true, now: now)
        XCTAssertEqual(refused.unreachable, []); XCTAssertNil(refused.cursor)
        XCTAssertNil(refused.bounded(to: ["t/gone"]), "nothing is saved for a refused walk")
        XCTAssertEqual(fresh.afterWalk(missing: ["t/gone"], next: "page-9", pagesRead: 4, placeRefused: false).cursor, "page-9")
        XCTAssertNil(fresh.afterWalk(missing: [], next: "page-9", pagesRead: 1, placeRefused: false).cursor)
        try restarted.saveSettleRepair(nil, conversation: "chat")
        XCTAssertNil(ReadStore(root: root, host: "mac", device: "phone").loadSettleRepair(conversation: "chat"))
    }

    // Saved approval decisions and answer drafts go once the Mac's complete
    // list no longer has the request; pending and in-flight ones stay, as do
    // other saved intent.
    func testSavedApprovalRepliesAreClearedOnceNoLongerPending() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        for id in ["done", "pending", "sending"] {
            try store.saveIntent(Data("decision".utf8), conversation: "decision-" + id)
            try store.saveIntent(Data("answer".utf8), conversation: "answer-" + id)
        }
        try store.saveIntent(Data("draft".utf8), conversation: "chat")
        try store.noteAttentionIntents(["done", "pending", "sending"])
        let restarted = ReadStore(root: root, host: "mac", device: "phone")
        let mark = restarted.attentionReadMark()
        XCTAssertEqual(try restarted.pruneAttentionIntents(pending: ["pending"], keeping: ["sending"], readMark: mark), ["done"])
        XCTAssertNil(try restarted.loadIntent(conversation: "decision-done"))
        XCTAssertNil(try restarted.loadIntent(conversation: "answer-done"))
        for id in ["pending", "sending"] {
            XCTAssertNotNil(try restarted.loadIntent(conversation: "decision-" + id))
            XCTAssertNotNil(try restarted.loadIntent(conversation: "answer-" + id))
        }
        XCTAssertEqual(try restarted.loadIntent(conversation: "chat"), Data("draft".utf8))
        XCTAssertEqual(restarted.attentionIntentIDs(), ["pending", "sending"])
        // An acknowledged reply is removed directly.
        try restarted.removeAttentionIntents(["sending"])
        XCTAssertNil(try restarted.loadIntent(conversation: "decision-sending"))
        XCTAssertEqual(restarted.attentionIntentIDs(), ["pending"])
        try restarted.removeAttentionIntents(["pending"])
        XCTAssertEqual(restarted.attentionIntentIDs(), [])
        let names = try FileManager.default.contentsOfDirectory(atPath: restarted.directory.path)
        XCTAssertEqual(names.count, 1, "only the unrelated draft remains: \(names)")
    }

    // Reads of the pending approvals can finish out of order. An older read
    // that started before a newer one listed an approval, and the user saved
    // an answer to it, must not delete that answer when its list comes back
    // without the approval. The next read that started after it does.
    func testOlderApprovalReadCannotDeleteAnAnswerToANewerRequest() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        try store.noteAttentionIntents(["old"])
        try store.saveIntent(Data("answer".utf8), conversation: "answer-old")
        let older = store.attentionReadMark()
        // A newer read lists "new" and the user saves an answer to it.
        try store.noteAttentionIntents(["old", "new"])
        try store.saveIntent(Data("answer".utf8), conversation: "answer-new")
        try store.saveIntent(Data("decision".utf8), conversation: "decision-new")
        // The older read's list comes back empty.
        let restarted = ReadStore(root: root, host: "mac", device: "phone")
        XCTAssertEqual(try restarted.pruneAttentionIntents(pending: [], readMark: older), ["old"])
        XCTAssertNil(try restarted.loadIntent(conversation: "answer-old"))
        XCTAssertEqual(try restarted.loadIntent(conversation: "answer-new"), Data("answer".utf8))
        XCTAssertEqual(try restarted.loadIntent(conversation: "decision-new"), Data("decision".utf8))
        XCTAssertEqual(restarted.attentionIntentIDs(), ["new"])
        // A read that started after "new" was listed and no longer lists it
        // knows it ended.
        XCTAssertEqual(try restarted.pruneAttentionIntents(pending: [], readMark: restarted.attentionReadMark()), ["new"])
        XCTAssertNil(try restarted.loadIntent(conversation: "answer-new"))
        XCTAssertEqual(restarted.attentionIntentIDs(), [])
    }

    func snapshot(_ chat: String = "a", epoch: String = "epoch", sequence: UInt64 = 10, start: Int = 0, count: Int = 1, cursor: String? = nil) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: chat, hostEpoch: epoch, lastSequence: sequence,
            messages: (start..<(start+count)).map { ConversationMessage(messageId: String($0), body: "Message \($0)", state: "completed", createdAt: String(1700000000000 + $0), attachmentIds: []) },
            assistantMessages: [], thread: ThreadProjection(nextCursor: cursor, hydrated: true))
    }
    func event(_ sequence: UInt64, epoch: String = "epoch", type: String = "activity") -> ReplayEvent {
        ReplayEvent(eventId: String(sequence), hostEpoch: epoch, sequence: sequence, occurredAt: "now", conversationId: nil, event: EventBody(type: type))
    }
    func testHistoryMergePreservesConcurrentRefresh() throws {
        let older = snapshot(sequence: 10, start: 0, count: 3, cursor: "earlier")
        let latest = ConversationSnapshot(conversationId: "a", hostEpoch: "epoch", lastSequence: 12,
            messages: [ConversationMessage(messageId: "2", body: "Newer text", state: "completed", createdAt: "1700000000002", attachmentIds: [])],
            assistantMessages: [], thread: ThreadProjection(nextCursor: "current", hydrated: true))
        let merged = try latest.mergingOlder(older)
        XCTAssertEqual(merged.lastSequence, 12)
        XCTAssertEqual(merged.messages.map(\.messageId), ["0", "1", "2"])
        XCTAssertEqual(merged.messages.last?.body, "Newer text")
        XCTAssertEqual(merged.thread.nextCursor, "earlier")
    }

    func testHistoryPrependKeepsEachCompactionIdentityExactlyOnce() throws {
        let compacted = ReadItem(id: "compact-1", type: "contextCompaction", state: "completed",
            text: "private summary", createdAt: "1700000000001", payload: nil)
        let older = ConversationSnapshot(conversationId: "a", hostEpoch: "epoch", lastSequence: 10,
            messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: "earlier", hydrated: true,
                turns: [ReadTurn(id: "turn", items: [compacted], status: "completed")]))
        let following = ReadItem(id: "reply-1", type: "agentMessage", state: "completed",
            text: "Following work", createdAt: "1700000000002", payload: nil)
        let newer = ConversationSnapshot(conversationId: "a", hostEpoch: "epoch", lastSequence: 12,
            messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: "current", hydrated: true,
                turns: [ReadTurn(id: "turn", items: [following], status: "completed")]))

        let merged = try newer.mergingOlder(older)
        XCTAssertEqual(merged.thread.turns?.first?.items.map(\.id), ["compact-1", "reply-1"])
        XCTAssertEqual(merged.rows(author: "Agent").filter(\.isContextCompaction).map(\.id), ["turn/compact-1"])
    }

    func testSnapshotNeverSkipsOtherConversations() throws {
        var state = ProjectionState()
        state.install(snapshot())
        state.install(snapshot("b", sequence: 90))
        XCTAssertEqual(state.lastSequence, 10)
        try state.consume(event(11))
        XCTAssertEqual(state.dirty, ["a"], "The clean snapshot at 90 already includes event 11")
        state.install(snapshot(sequence: 12))
        XCTAssertTrue(state.dirty.isEmpty)
        XCTAssertEqual(state.lastSequence, 11, "Neither snapshot skips replay for other conversations")
        XCTAssertThrowsError(try state.consume(event(13)), "Ahead snapshots cannot hide the missing event 12")
        XCTAssertEqual(state.lastSequence, 11)
    }
    func testResyncEpochGapDuplicateAndUnknownEvent() throws {
        var state = ProjectionState()
        state.install(snapshot())
        XCTAssertThrowsError(try state.consume(event(12)))
        XCTAssertThrowsError(try state.consume(event(11, epoch: "new")))
        XCTAssertThrowsError(try state.consume(event(10, type: "resync_required")))
        try state.consume(event(10))
        XCTAssertEqual(state.lastSequence, 10)
        try state.consume(event(11, type: "future_kind"))
        XCTAssertTrue(state.dirty.contains("a"))
        XCTAssertTrue(state.listDirty)
    }
    func testCrashBeforeAndAfterAtomicCommitPreservesIntent() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "Mac", device: "phone")
        var state = ProjectionState()
        state.install(snapshot())
        try store.save(state)
        try store.saveIntent(Data("unsent draft and uncertain receipt".utf8), conversation: "a")
        let intent = try FileManager.default.contentsOfDirectory(at: store.directory, includingPropertiesForKeys: nil).first { $0.lastPathComponent.hasPrefix("intent-") }!
        try state.consume(event(11))
        // Simulated death before persistence: a new reader still sees the old cursor.
        XCTAssertEqual(try store.load().lastSequence, 10)
        try store.save(state)
        let recovered = try store.load()
        XCTAssertEqual(recovered.lastSequence, 11)
        XCTAssertTrue(recovered.dirty.contains("a"))
        XCTAssertEqual(try Data(contentsOf: intent), Data("unsent draft and uncertain receipt".utf8))
        try store.save(ProjectionState())
        XCTAssertEqual(try Data(contentsOf: intent), Data("unsent draft and uncertain receipt".utf8))
    }
    func testHostPartitionRemovalAndWriteFailure() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let a = ReadStore(root: root, host: "../a", device: "phone")
        let b = ReadStore(root: root, host: "b", device: "phone")
        var state = ProjectionState(); state.install(snapshot())
        try a.save(state); try a.saveIntent(Data("draft".utf8), conversation: "a")
        XCTAssertTrue(try b.load().snapshots.isEmpty)
        try b.save(state); try a.remove()
        XCTAssertFalse(FileManager.default.fileExists(atPath: a.directory.path))
        XCTAssertFalse(try b.load().knownConversationIDs.isEmpty)
        let bad = root.appendingPathComponent("not-a-directory")
        try Data().write(to: bad)
        XCTAssertThrowsError(try ReadStore(root: bad, host: "host", device: "device").save(state))
    }
    func testThousandItemsPaginateWithoutDuplicatesAndSortChronologically() throws {
        var combined = snapshot(start: 900, count: 100, cursor: "900")
        for start in stride(from: 800, through: 0, by: -100) {
            combined = try combined.mergingOlder(snapshot(start: start, count: 100, cursor: start == 0 ? nil : String(start)))
        }
        let rows = combined.rows(author: "Ada")
        XCTAssertEqual(rows.count, 1000)
        XCTAssertEqual(rows.first?.text, "Message 0")
        XCTAssertEqual(rows.last?.text, "Message 999")
        XCTAssertNil(combined.thread.nextCursor)
        let duplicate = try combined.mergingOlder(snapshot(start: 0, count: 100))
        XCTAssertEqual(duplicate.messages.count, 1000)
        XCTAssertThrowsError(try combined.mergingOlder(snapshot("other")))
        XCTAssertThrowsError(try combined.mergingOlder(snapshot(epoch: "new")))
    }
    func testActualRustFixtureDecodesAndKeepsAuthor() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let value = try JSONDecoder().decode(ConversationSnapshot.self, from: Data(contentsOf: root.appendingPathComponent("tests/contracts/native/conversation-snapshot.json")))
        XCTAssertFalse(value.messages.isEmpty)
        XCTAssertTrue(value.rows(author: "Ada").contains { $0.author == "Ada" })
        XCTAssertEqual(value.rows(author: "Ada").filter { $0.author == "Ada" }.count, 1)
    }

    func testSnapshotRacingReplayStaysDirty() throws {
        var state = ProjectionState(); state.install(snapshot())
        try state.consume(event(11))
        state.install(snapshot(sequence: 10))
        XCTAssertTrue(state.dirty.contains("a"))
        XCTAssertEqual(state.lastSequence, 11)
    }

    private func event(_ sequence: UInt64, conversation: String?, body: EventBody = EventBody(type: "activity")) -> ReplayEvent {
        ReplayEvent(eventId: String(sequence), hostEpoch: "epoch", sequence: sequence, occurredAt: "now", conversationId: conversation, event: body)
    }

    /// Streaming in another chat must not keep the open chat stale; its read
    /// acknowledgement and live status wait on its own invalidations only.
    func testOpenChatBecomesCurrentWhileAnotherChatStreams() throws {
        var state = ProjectionState()
        state.install(snapshot("a")); state.install(snapshot("b"))
        try state.consume(event(11, conversation: "a"))
        XCTAssertEqual(state.dirty, ["a"])
        // Chat b streams while the refresh of a is in flight.
        for sequence in UInt64(12)...40 { try state.consume(event(sequence, conversation: "b")) }
        state.install(snapshot("a", sequence: 11))
        XCTAssertEqual(state.dirty, ["b"])
        XCTAssertEqual(state.lastSequence, 40)
        // A snapshot read before b's newest change leaves b stale.
        state.install(snapshot("b", sequence: 39))
        XCTAssertTrue(state.dirty.contains("b"))
        state.install(snapshot("b", sequence: 40))
        XCTAssertTrue(state.dirty.isEmpty)
    }

    func testApprovalAndAutomationEventsInvalidateOnlyTheirOwnerAndKeepCursorContinuity() throws {
        for body in [EventBody(type: "approval_resolved"),
                     EventBody(type: "activity", category: "projection_changed", detail: #"{"resource":"approvals","change":"delete"}"#),
                     EventBody(type: "activity", category: "projection_changed", detail: #"{"resource":"automation_runs","change":"update"}"#)] {
            var state = ProjectionState()
            state.install(snapshot("a")); state.install(snapshot("b"))
            state.listDirty = false
            try state.consume(event(11, conversation: "a", body: body))
            XCTAssertEqual(state.dirty, ["a"])
            XCTAssertTrue(state.listDirty)
            XCTAssertEqual(state.lastSequence, 11)
            XCTAssertThrowsError(try state.consume(event(13, conversation: "a", body: body)))
            XCTAssertEqual(state.lastSequence, 11)
            // An owner the host could not resolve keeps the conservative fallback.
            try state.consume(event(12, conversation: nil, body: body))
            XCTAssertEqual(state.dirty, ["a", "b"])
            XCTAssertEqual(state.lastSequence, 12)
        }
    }

    func testSnapshotAheadOfReplayCoversOnlyCleanSameEpochContent() throws {
        var state = ProjectionState()
        state.install(snapshot("a")); state.install(snapshot("b"))
        state.install(snapshot("a", sequence: 15))
        XCTAssertEqual(state.lastSequence, 10)
        state.listDirty = false
        try state.consume(event(11, conversation: "a", body: EventBody(type: "assistant_delta", text: "already read")))
        XCTAssertTrue(state.dirty.isEmpty)
        XCTAssertFalse(state.listDirty, "A covered delta needs no list read in either consumer")
        XCTAssertEqual(state.lastSequence, 11)
        try state.consume(event(12, conversation: "a", body: EventBody(type: "activity", category: "projection_changed",
            detail: #"{"resource":"assistant_messages","change":"update"}"#)))
        XCTAssertFalse(state.listDirty, "The covered text invalidation also needs no list read")
        XCTAssertTrue(state.dirty.isEmpty)
        try state.consume(event(13, conversation: nil))
        XCTAssertTrue(state.listDirty, "Non-content consumers still reconcile host-wide changes")
        XCTAssertEqual(state.dirty, ["b"], "Global fallback still invalidates uncovered chats")
        state.dirty.insert("a") // A read that was already stale cannot cover replay.
        try state.consume(event(14, conversation: "a"))
        XCTAssertTrue(state.dirty.contains("a"))
        XCTAssertEqual(state.invalidatedThrough?["a"], 14)
    }

    func testHostWideEventsInvalidateEveryChatButRuntimeNoticesDoNot() throws {
        var state = ProjectionState()
        state.install(snapshot("a")); state.install(snapshot("b"))
        for (category, eventState) in [("App Server", "unhandled"), ("codex_skills", "changed")] {
            let json = "{\"eventId\":\"n\",\"hostEpoch\":\"epoch\",\"sequence\":11,\"occurredAt\":\"now\",\"conversationId\":null,\"event\":{\"type\":\"activity\",\"data\":{\"category\":\"\(category)\",\"state\":\"\(eventState)\"}}}"
            let notice = try JSONDecoder().decode(ReplayEvent.self, from: Data(json.utf8))
            var snapshot = state; snapshot.listDirty = false
            try snapshot.consume(notice)
            XCTAssertEqual(snapshot.lastSequence, 11)
            XCTAssertTrue(snapshot.dirty.isEmpty)
            XCTAssertFalse(snapshot.listDirty)
            XCTAssertEqual(try state.consumeLive(notice), .skipped)
            state = ProjectionState(); state.install(self.snapshot("a")); state.install(self.snapshot("b"))
        }
        state.lastSequence = 11
        // Other payload shapes decode and stay conservative.
        let unknown = try JSONDecoder().decode(ReplayEvent.self, from: Data(#"{"eventId":"u","hostEpoch":"epoch","sequence":12,"occurredAt":"now","event":{"type":"future_kind","data":"opaque"}}"#.utf8))
        try state.consume(unknown)
        XCTAssertEqual(state.dirty, ["a", "b"])
        XCTAssertTrue(state.listDirty)
        state.install(snapshot("a", sequence: 12))
        XCTAssertEqual(state.dirty, ["b"])
    }

    func testCacheWithoutInvalidationMarksNeedsTheGlobalCursor() throws {
        var legacy = ProjectionState()
        legacy.install(snapshot("a"))
        legacy.lastSequence = 20
        legacy.dirty = ["a"]
        let encoded = try JSONEncoder().encode(legacy)
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
        object.removeValue(forKey: "invalidatedThrough")
        var restored = try JSONDecoder().decode(ProjectionState.self, from: JSONSerialization.data(withJSONObject: object))
        restored.install(snapshot("a", sequence: 19))
        XCTAssertTrue(restored.dirty.contains("a"))
        restored.install(snapshot("a", sequence: 20))
        XCTAssertFalse(restored.dirty.contains("a"))
    }

    func testComputerScreenshotDataIsDroppedBeforeCaching() throws {
        let data = "data:image/png;base64," + String(repeating: "A", count: 200_000)
        let json = #"{"id":"shot","type":"imageView","state":"completed","text":"Computer screenshot","createdAt":"1","payload":{"imageUrl":"\#(data)","alt":"Screen"}}"#
        let item = try JSONDecoder().decode(ReadItem.self, from: Data(json.utf8))
        XCTAssertNil(item.payload?["imageUrl"])
        XCTAssertEqual(item.payload?["alt"]?.string, "Screen")
        XCTAssertLessThan(try JSONEncoder().encode(item).count, 1_000)
        let linked = try JSONDecoder().decode(ReadItem.self, from: Data(#"{"id":"link","type":"imageView","state":"completed","createdAt":"1","payload":{"imageUrl":"https://example.com/a.png"}}"#.utf8))
        XCTAssertEqual(linked.payload?["imageUrl"]?.string, "https://example.com/a.png")
    }

    func testBackgroundMetadataFlushDoesNotRewriteUnchangedBodies() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let samples = CacheWriteSamples()
        let writer = ProjectionWriter(store: store, delay: .seconds(30)) { result, elapsed in samples.append(result, elapsed: elapsed) }
        var state = ProjectionState()
        for chat in 0..<12 { state.install(snapshot("chat-\(chat)", count: 200)) }
        writer.schedule(state); await writer.flush()
        let bodies = try bodyFiles(store)
        let identities = try bodyIdentities(store)
        state.lastSequence = 99; state.invalidateAll()
        writer.schedule(state); await writer.flush()
        XCTAssertEqual(try bodyFiles(store), bodies, "Invalidations write metadata, not unchanged history")
        XCTAssertEqual(try bodyIdentities(store), identities)
        XCTAssertLessThan(try XCTUnwrap(samples.values.last).bytes, 8_192)
        XCTAssertTrue(try store.load().snapshots.isEmpty, "Launch decodes no body arrays")
        assertState(try loadHistory(store), equals: state)
        state.snapshots = state.snapshots.mapValues { old in
            ConversationSnapshot(conversationId: old.conversationId, hostEpoch: "new-epoch", lastSequence: 200,
                historyRevision: 2, messages: old.messages, assistantMessages: old.assistantMessages,
                thread: ThreadProjection(threadId: "native-" + old.conversationId, nextCursor: "earlier-page", hydrated: false, turns: old.thread.turns),
                messageRecords: old.messageRecords)
        }
        state.hostEpoch = "new-epoch"; state.lastSequence = 200; state.invalidateAll()
        writer.schedule(state); await writer.flush()
        XCTAssertEqual(try bodyFiles(store), bodies, "Paging and refresh headers do not rewrite history")
        XCTAssertEqual(try bodyIdentities(store), identities)
        XCTAssertLessThan(try XCTUnwrap(samples.values.last).bytes, 8_192)
        assertState(try loadHistory(store), equals: state)
        // Same sequence and item counts do not imply equal contents.
        state.snapshots["chat-0"] = snapshot("chat-0", sequence: 200, start: 80, count: 200)
        writer.schedule(state); await writer.flush()
        let changed = try bodyFiles(store)
        XCTAssertEqual(Set(bodies.keys).intersection(changed.keys).count, bodies.count - 1)
        for name in Set(bodies.keys).intersection(changed.keys) { XCTAssertEqual(bodies[name], changed[name]) }
        assertState(try loadHistory(store), equals: state)
        writer.retire(); await writer.flush()
    }

    func testOldCacheIsRebuiltWithoutReadingBodiesOrTouchingIntent() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        try store.saveIntent(Data("unsent signed intent".utf8), conversation: "chat")
        try store.savePosition("turn/item", conversation: "chat")
        // Even malformed legacy bodies are not decoded. The pre-launch owner
        // decision drops old disposable cache versions, never independent intent.
        for name in ["read-cache-v2.json", "read-cache-overlay-v1.json"] {
            try Data("{old cache".utf8).write(to: store.directory.appendingPathComponent(name))
        }
        XCTAssertTrue(try store.load().knownConversationIDs.isEmpty)
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.appendingPathComponent("read-cache-v2.json").path))
        XCTAssertEqual(try store.loadIntent(conversation: "chat"), Data("unsent signed intent".utf8))
        XCTAssertEqual(try store.loadPosition(conversation: "chat"), "turn/item")
        var fresh = ProjectionState(); fresh.install(snapshot("chat"))
        try store.save(fresh)
        assertState(try loadHistory(store), equals: fresh)
    }

    func testAtomicIndexIgnoresUnpublishedBodiesAndCorruptionCanBeRepaired() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var original = ProjectionState(); original.install(snapshot())
        try store.save(original)
        // Death before publishing the new index leaves both cursor and body old.
        let orphan = store.directory.appendingPathComponent("history-" + UUID().uuidString + ".json")
        try JSONEncoder().encode(snapshot(sequence: 20)).write(to: orphan)
        assertState(try loadHistory(store), equals: original)
        var newest = original; newest.lastSequence = 20; newest.invalidateAll()
        newest.snapshots["a"] = snapshot(sequence: 20)
        try store.save(newest)
        XCTAssertFalse(FileManager.default.fileExists(atPath: orphan.path))
        assertState(try loadHistory(store), equals: newest)
        let index = store.directory.appendingPathComponent("read-cache-v4.json")
        try Data("{broken index".utf8).write(to: index, options: .atomic)
        XCTAssertThrowsError(try store.load())
        try store.save(newest)
        assertState(try loadHistory(store), equals: newest)
    }

    func testResidentBudgetEvictsLRUButKeepsDiskPagesAndReopensOffline() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var state = ProjectionState()
        // Above the real 32 MiB budget, including a nested tool payload.
        for chat in 0..<5 {
            state.install(ConversationSnapshot(conversationId: "chat-\(chat)", hostEpoch: "epoch", lastSequence: 10,
                messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: "older-\(chat)", hydrated: true,
                    turns: [ReadTurn(id: "turn", items: [ReadItem(id: "tool", type: "commandExecution", state: "completed",
                        text: nil, createdAt: "1", payload: ["nested": .array([.object(["output": .string(String(repeating: "x", count: 4_000_000))])])])])]),
                messageRecords: [MessageRecord(turnId: "turn", itemId: "tool", clientId: "client")]))
        }
        let original = state
        let writer = ProjectionWriter(store: store, delay: .seconds(30))
        writer.schedule(state)
        let durable = await writer.durableBodies()
        let files = try bodyFiles(store)
        XCTAssertGreaterThan(state.residentBytes, ProjectionState.residentByteLimit)
        state.touchConversation("chat-0")
        let removed = state.evictResidentBodies(keeping: ["chat-4"], durable: durable)
        XCTAssertEqual(removed, ["chat-1"], "Least recently used unprotected history goes first")
        XCTAssertLessThanOrEqual(state.residentBytes, ProjectionState.residentByteLimit)
        XCTAssertNotNil(state.snapshots["chat-0"])
        XCTAssertNotNil(state.snapshots["chat-4"])
        writer.schedule(state); await writer.flush()
        XCTAssertEqual(try bodyFiles(store), files, "Eviction never deletes disk pages")
        var noResident = state
        noResident.snapshots = [:]
        try store.save(noResident)
        XCTAssertEqual(try bodyFiles(store), files, "Saving an empty resident set never means deleting offline history")
        var cold = try await ProjectionWriter.load(store)
        XCTAssertTrue(cold.snapshots.isEmpty, "All saved conversations start as metadata")
        XCTAssertEqual(cold.knownConversationIDs, original.knownConversationIDs)
        cold.invalidateAll()
        let record = try XCTUnwrap(cold.bodyRecords?["chat-1"])
        // Unrelated corrupt history cannot make launch eagerly decode bodies
        // or prevent opening a different intact conversation offline.
        let unrelated = try XCTUnwrap(cold.bodyRecords?["chat-2"])
        try Data("{corrupt body".utf8).write(to: store.directory.appendingPathComponent("history-" + unrelated.bodyID.uuidString + ".json"), options: .atomic)
        XCTAssertEqual(try store.load().knownConversationIDs, cold.knownConversationIDs)
        let offline = try await ProjectionWriter.loadConversation(store, conversation: "chat-1", record: record)
        XCTAssertTrue(cold.restoreConversation(offline, matching: record))
        XCTAssertEqual(cold.snapshots["chat-1"], original.snapshots["chat-1"])
        XCTAssertEqual(cold.snapshots["chat-1"]?.thread.nextCursor, "older-1")
        XCTAssertTrue(cold.dirty.contains("chat-1"), "Disk restoration cannot claim an authoritative read")
        XCTAssertEqual(cold.lastSequence, original.lastSequence)
        let reopenedWriter = ProjectionWriter(store: store)
        let coldDurable = await reopenedWriter.durableBodies()
        XCTAssertEqual(cold.evictResidentBodies(keeping: [], durable: coldDurable, byteLimit: 0), ["chat-1"],
            "A new offline owner can evict reloaded history without first saving it")
        let reopened = try await ProjectionWriter.loadConversation(store, conversation: "chat-1", record: record)
        XCTAssertEqual(reopened, original.snapshots["chat-1"])
        XCTAssertTrue(cold.restoreConversation(reopened, matching: record))
        try FileManager.default.removeItem(at: store.directory.appendingPathComponent("history-" + record.bodyID.uuidString + ".json"))
        let missingFile = await reopenedWriter.durableBodies()
        XCTAssertTrue(cold.evictResidentBodies(keeping: [], durable: missingFile, byteLimit: 0).isEmpty,
            "An index entry without its body cannot authorize eviction of the remaining copy")
        writer.retire(); await writer.flush()
    }

    func testPressureDropsOnlyDurableUnprotectedBodiesAndExplicitDeletionReclaimsDisk() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var state = ProjectionState()
        for id in ["open", "pinned", "voice", "closed"] { state.install(snapshot(id, count: 80, cursor: "older")) }
        try store.saveIntent(Data("draft and retry identity".utf8), conversation: "closed")
        try store.savePosition("anchored-item", conversation: "closed")
        let gate = ProtectedDataGate()
        let writer = ProjectionWriter(store: store, delay: .seconds(30), gate: gate)
        writer.schedule(state)
        let durable = await writer.durableBodies()
        state.install(snapshot("unsaved", count: 80))
        // A memory warning while locked must retain the only copy of a page.
        gate.update(available: false)
        writer.schedule(state)
        let beforeUnlock = await writer.durableBodies()
        XCTAssertEqual(beforeUnlock, durable)
        XCTAssertEqual(state.evictResidentBodies(keeping: ["open", "pinned", "voice"], durable: beforeUnlock, byteLimit: 0), ["closed"])
        XCTAssertNotNil(state.snapshots["unsaved"])
        XCTAssertNotNil(state.snapshots["open"]); XCTAssertNotNil(state.snapshots["pinned"]); XCTAssertNotNil(state.snapshots["voice"])
        writer.schedule(state)
        gate.update(available: true)
        let afterUnlock = await writer.durableBodies()
        XCTAssertEqual(state.evictResidentBodies(keeping: ["open", "pinned", "voice"], durable: afterUnlock, byteLimit: 0), ["unsaved"])
        writer.schedule(state); await writer.flush()
        XCTAssertEqual(try store.load().knownConversationIDs, ["open", "pinned", "voice", "closed", "unsaved"])
        XCTAssertEqual(try store.loadIntent(conversation: "closed"), Data("draft and retry identity".utf8))
        XCTAssertEqual(try store.loadPosition(conversation: "closed"), "anchored-item")
        let before = try bodyFiles(store).count
        state.forgetConversation("closed")
        writer.schedule(state); await writer.flush()
        XCTAssertFalse(try store.load().knownConversationIDs.contains("closed"))
        XCTAssertEqual(try bodyFiles(store).count, before - 1)
        XCTAssertEqual(try store.loadIntent(conversation: "closed"), Data("draft and retry identity".utf8))
        try await writer.remove()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path))
    }

    func testRacingDiskRestoreCannotReplaceANewerPageOrLoseInvalidations() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var original = ProjectionState(); original.install(snapshot())
        try store.save(original)
        var state = try store.load()
        let record = try XCTUnwrap(state.bodyRecords?["a"])
        let read = try await ProjectionWriter.loadConversation(store, conversation: "a", record: record)
        try state.consume(event(11, conversation: nil))
        state.touchConversation("a") // Another window navigated while decoding.
        XCTAssertTrue(state.restoreConversation(read, matching: record))
        XCTAssertTrue(state.dirty.contains("a"))
        XCTAssertEqual(state.invalidatedThrough?["a"], 11)
        XCTAssertEqual(state.lastSequence, 11)
        var race = try store.load()
        race.install(snapshot(sequence: 12))
        XCTAssertFalse(race.restoreConversation(read, matching: record))
        XCTAssertEqual(race.snapshots["a"]?.lastSequence, 12)
        var removed = try store.load(); removed.forgetConversation("a")
        XCTAssertFalse(removed.restoreConversation(read, matching: record))
        // An in-flight write of an old body cannot authorize eviction of a
        // newly accepted replacement at the same sequence/revision/count.
        let writer = ProjectionWriter(store: store)
        writer.schedule(original)
        let durable = await writer.durableBodies()
        original.snapshots["a"] = snapshot(start: 10, count: 3)
        XCTAssertTrue(original.evictResidentBodies(keeping: [], durable: durable, byteLimit: 0).isEmpty)
        writer.retire(); await writer.flush()
    }

    func testFailedBodyWritePreservesIndexAndFlushRetriesNewestState() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var state = ProjectionState(); state.install(snapshot(count: 200))
        try store.save(state)
        let index = store.directory.appendingPathComponent("read-cache-v4.json")
        let previous = try Data(contentsOf: index)
        state.lastSequence = 88; state.snapshots["a"] = snapshot(sequence: 88, start: 1, count: 200); state.invalidateAll()
        let record = try XCTUnwrap(state.bodyRecords?["a"])
        let conflict = store.directory.appendingPathComponent("history-" + record.bodyID.uuidString + ".json")
        try FileManager.default.createDirectory(at: conflict, withIntermediateDirectories: false)
        let samples = CacheWriteSamples()
        let writer = ProjectionWriter(store: store, delay: .seconds(30)) { result, elapsed in samples.append(result, elapsed: elapsed) }
        writer.schedule(state); await writer.flush()
        XCTAssertNotNil(try XCTUnwrap(samples.values.last).error)
        XCTAssertEqual(try Data(contentsOf: index), previous)
        XCTAssertEqual(try store.load().lastSequence, 10, "Failed body publication cannot advance the replay cursor")
        let failedDurable = await writer.durableBodies()
        XCTAssertTrue(state.evictResidentBodies(keeping: [], durable: failedDurable, byteLimit: 0).isEmpty)
        try FileManager.default.removeItem(at: conflict)
        await writer.flush() // No reschedule: newest accepted state was retained.
        XCTAssertNil(try XCTUnwrap(samples.values.last).error)
        assertState(try loadHistory(store), equals: state)
        writer.retire(); await writer.flush()
    }

    private func bodyFiles(_ store: ReadStore) throws -> [String: Data] {
        let files = try FileManager.default.contentsOfDirectory(at: store.directory, includingPropertiesForKeys: nil)
        return try Dictionary(uniqueKeysWithValues: files.filter { $0.lastPathComponent.hasPrefix("history-") && $0.pathExtension == "json" }
            .map { ($0.lastPathComponent, try Data(contentsOf: $0)) })
    }
    private func bodyIdentities(_ store: ReadStore) throws -> [String: String] {
        let files = try FileManager.default.contentsOfDirectory(at: store.directory, includingPropertiesForKeys: nil)
        return try Dictionary(uniqueKeysWithValues: files.filter { $0.lastPathComponent.hasPrefix("history-") }.map { url in
            let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
            return (url.lastPathComponent, "\(attributes[.systemFileNumber] ?? "missing"):\(attributes[.modificationDate] ?? "missing")")
        })
    }
    private func loadHistory(_ store: ReadStore) throws -> ProjectionState {
        var state = try store.load()
        for (id, record) in state.bodyRecords ?? [:] {
            let body = try store.loadConversation(id, record: record)
            XCTAssertTrue(state.restoreConversation(body, matching: record))
        }
        return state
    }

    private func assertState(_ actual: ProjectionState, equals expected: ProjectionState, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertEqual(actual.snapshots, expected.snapshots, file: file, line: line)
        XCTAssertEqual(actual.hostEpoch, expected.hostEpoch, file: file, line: line)
        XCTAssertEqual(actual.lastSequence, expected.lastSequence, file: file, line: line)
        XCTAssertEqual(actual.dirty, expected.dirty, file: file, line: line)
        XCTAssertEqual(actual.invalidatedThrough, expected.invalidatedThrough, file: file, line: line)
        XCTAssertEqual(actual.listDirty, expected.listDirty, file: file, line: line)
        XCTAssertEqual(actual.positions, expected.positions, file: file, line: line)
    }

    func testWriterKeepsNewestStateAndRemovalCannotBeUndoneByPendingWrite() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let writes = LockedCounter()
        let writer = ProjectionWriter(store: store, delay: .milliseconds(50)) { result, _ in
            if case .success = result { writes.increment() }
        }
        var state = ProjectionState(); state.install(snapshot())
        for sequence in UInt64(11)...60 { state.lastSequence = sequence; writer.schedule(state) }
        await writer.flush()
        XCTAssertEqual(try store.load().lastSequence, 60)
        XCTAssertLessThan(writes.value, 5, "A burst must coalesce instead of encoding every state")
        state.lastSequence = 61
        writer.schedule(state)
        try await writer.remove()
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path))
        writer.schedule(state); await writer.flush()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path))
    }

    /// A long thread: `turns` turns of prompt, tool run (with `output` bytes of
    /// output) and reply, with the last reply streaming `frame`.
    private func longThread(turns: Int, output: Int, frame: Int) -> ConversationSnapshot {
        let tool = String(repeating: "x", count: output)
        let turnList = (0..<turns).map { index -> ReadTurn in
            let last = index == turns - 1
            return ReadTurn(id: "turn-\(index)", items: [
                ReadItem(id: "prompt", type: "userMessage", state: "completed", text: "Prompt \(index)", createdAt: String(1_700_000_000_000 + index * 10),
                         payload: ["clientId": .string("client-\(index)")]),
                ReadItem(id: "tool", type: "commandExecution", state: "completed", text: nil, createdAt: String(1_700_000_000_001 + index * 10),
                         payload: ["command": .string("make \(index)"), "output": .string(tool), "exitCode": .number(0), "ok": .bool(true)]),
                ReadItem(id: "reply", type: "agentMessage", state: last ? "streaming" : "completed",
                         text: last ? String(repeating: "streamed ", count: frame + 1) : "Reply \(index)", createdAt: String(1_700_000_000_002 + index * 10))
            ], status: last ? "inProgress" : "completed")
        }
        let messages = (0..<turns).map { ConversationMessage(clientMessageId: "client-\($0)", codexTurnId: "turn-\($0)", messageId: "message-\($0)",
            body: "Prompt \($0)", state: "completed", createdAt: String(1_700_000_000_000 + $0 * 10), attachmentIds: []) }
        let assistants = (0..<turns).map { index in AssistantMessage(itemId: "reply", codexTurnId: "turn-\(index)", messageId: "assistant-\(index)",
            text: index == turns - 1 ? String(repeating: "streamed ", count: frame + 1) : "Reply \(index)",
            state: index == turns - 1 ? "streaming" : "completed", createdAt: "1", updatedAt: index == turns - 1 ? String(frame) : "0") }
        return ConversationSnapshot(conversationId: "long", hostEpoch: "epoch", lastSequence: UInt64(10 + frame), messages: messages,
            assistantMessages: assistants, thread: ThreadProjection(nextCursor: "older", hydrated: true, turns: turnList),
            messageRecords: (0..<turns).map { MessageRecord(turnId: "turn-\($0)", itemId: "prompt", clientId: "client-\($0)") })
    }

    /// PERF-2: a streaming save writes the changed turn, its manifest and the
    /// index, a small fraction of the whole snapshot the old store re-encoded.
    func testStreamingSavesWriteOnlyTheChangedTurn() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        // 1,000 turns of 3 items = 3,000 items, about 12 MB.
        var state = ProjectionState()
        state.install(longThread(turns: 1_000, output: 12_000, frame: 0))
        let wholeSnapshot = try JSONEncoder().encode(state.snapshots["long"]).count
        let samples = CacheWriteSamples()
        let writer = ProjectionWriter(store: store, delay: .seconds(30)) { result, elapsed in samples.append(result, elapsed: elapsed) }
        writer.schedule(state); await writer.flush()
        let first = try XCTUnwrap(samples.values.last).bytes
        XCTAssertGreaterThan(first, wholeSnapshot, "The first save writes everything once")
        var worst = 0
        for frame in 1...20 {
            state.install(longThread(turns: 1_000, output: 12_000, frame: frame * 40))
            writer.schedule(state); await writer.flush()
            let sample = try XCTUnwrap(samples.values.last)
            XCTAssertNil(sample.error)
            worst = max(worst, sample.bytes)
        }
        print("CACHE_STREAM whole_snapshot_bytes=\(wholeSnapshot) first_save_bytes=\(first) worst_flush_bytes=\(worst) ratio=\(Double(worst) / Double(wholeSnapshot))")
        XCTAssertLessThanOrEqual(Double(worst), Double(wholeSnapshot) * 0.01, "A streaming flush must write at most 1% of the whole snapshot")
        // No file piles up: one manifest, one segment per turn.
        let names = try FileManager.default.contentsOfDirectory(atPath: store.directory.path)
        XCTAssertEqual(names.filter { $0.hasPrefix("segment-") }.count, 1_000)
        XCTAssertEqual(names.filter { $0.hasPrefix("history-") }.count, 1)
        assertState(try loadHistory(store), equals: state)
        writer.retire(); await writer.flush()
    }

    /// Cold launch: a new process knows nothing in memory, restores the same
    /// conversation, and its first streaming save still writes only what changed.
    func testReopenedStoreRestoresTheSameConversationAndSavesOnlyTheChange() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var state = ProjectionState()
        state.install(longThread(turns: 60, output: 4_000, frame: 0))
        try store.save(state)
        HistoryIndex.forget(store.directory) // another process
        let reopened = ReadStore(root: root, host: "mac", device: "phone")
        var restored = try loadHistory(reopened)
        assertState(restored, equals: state)
        restored.install(longThread(turns: 60, output: 4_000, frame: 40))
        let wrote = try reopened.save(restored)
        XCTAssertLessThan(wrote, 40_000, "Only the streaming turn, its manifest and the index")
        HistoryIndex.forget(store.directory)
        // A process that never loaded the body writes the same small change.
        var cold = try ReadStore(root: root, host: "mac", device: "phone").load()
        XCTAssertTrue(cold.snapshots.isEmpty)
        cold.install(longThread(turns: 60, output: 4_000, frame: 80))
        XCTAssertLessThan(try ReadStore(root: root, host: "mac", device: "phone").save(cold), 40_000)
        assertState(try loadHistory(ReadStore(root: root, host: "mac", device: "phone")), equals: cold)
    }

    func testContractFixtureSurvivesSegmentsAndBothDecodersAgree() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("tests/contracts/native/conversation-snapshot.json"))
        let fixture = try JSONDecoder().decode(ConversationSnapshot.self, from: data)
        XCTAssertEqual(try ThreadJSON.decode(ConversationSnapshot.self, from: data), fixture)
        let store = ReadStore(root: FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString), host: "mac", device: "phone")
        defer { try? FileManager.default.removeItem(at: store.directory.deletingLastPathComponent()) }
        var state = ProjectionState(); state.install(fixture)
        try store.save(state)
        HistoryIndex.forget(store.directory)
        assertState(try loadHistory(store), equals: state)
    }

    /// Lists keep their order across segments, including messages that name no
    /// loaded turn, interleaved turns and duplicate turn ids.
    func testSegmentsRestoreOrderingOrphansAndAbsentLists() throws {
        func message(_ id: String, _ turn: String?) -> ConversationMessage {
            ConversationMessage(clientMessageId: nil, codexTurnId: turn, messageId: id, body: id, state: "completed", createdAt: "1", attachmentIds: [])
        }
        func reply(_ id: String, _ turn: String?) -> AssistantMessage {
            AssistantMessage(itemId: id, codexTurnId: turn, messageId: id, text: id, state: "completed", createdAt: "1", updatedAt: "1")
        }
        let turns = [ReadTurn(id: "a", items: [], status: "completed"), ReadTurn(id: "b", items: [], status: "completed"),
                     ReadTurn(id: "a", items: [ReadItem(id: "late", type: "agentMessage", state: "completed", text: "dup", createdAt: "1")], status: "completed")]
        let mixed = ConversationSnapshot(conversationId: "mixed", hostEpoch: "epoch", lastSequence: 3,
            messages: [message("1", "b"), message("2", "a"), message("3", nil), message("4", "gone"), message("5", "b"), message("6", "a")],
            assistantMessages: [reply("x", "gone"), reply("y", "a"), reply("z", nil)],
            thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: turns),
            messageRecords: [MessageRecord(turnId: "b", itemId: "i", clientId: "c"), MessageRecord(turnId: "gone", itemId: "j", clientId: "d"),
                             MessageRecord(turnId: "a", itemId: "k", clientId: "e")])
        let bare = ConversationSnapshot(conversationId: "bare", hostEpoch: "epoch", lastSequence: 3, messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: "n", hydrated: false, turns: nil), messageRecords: nil)
        let empty = ConversationSnapshot(conversationId: "empty", hostEpoch: "epoch", lastSequence: 3, messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: []), messageRecords: [])
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var state = ProjectionState()
        for snapshot in [mixed, bare, empty] { state.install(snapshot) }
        try store.save(state)
        HistoryIndex.forget(store.directory)
        let restored = try loadHistory(store)
        XCTAssertEqual(restored.snapshots["mixed"], mixed)
        XCTAssertEqual(restored.snapshots["bare"], bare)
        XCTAssertNil(restored.snapshots["bare"]?.thread.turns)
        XCTAssertNil(restored.snapshots["bare"]?.messageRecords)
        XCTAssertEqual(restored.snapshots["empty"], empty)
        XCTAssertEqual(restored.snapshots["empty"]?.thread.turns, [])
    }

    /// A torn or hand-edited manifest reads as uncached; a negative or huge run
    /// length used to trap on every open of that chat.
    func testManifestWithImpossibleRunLengthsReadsAsUncached() {
        let segment = HistorySegment(turn: nil, messages: [], assistantMessages: [], records: [])
        func body(messageRuns: [Int]) -> Bool {
            let manifest = HistoryManifest(conversationId: "c", files: ["f"], hasTurns: false, hasRecords: false,
                                           messageRuns: messageRuns, assistantRuns: [], recordRuns: [])
            return HistoryPlan(manifest: manifest, loaded: [segment]).body() != nil
        }
        XCTAssertTrue(body(messageRuns: [0, 0]))
        XCTAssertFalse(body(messageRuns: [0, -1]))
        XCTAssertFalse(body(messageRuns: [0, 1]))
        XCTAssertFalse(body(messageRuns: [0, Int.max]))
        XCTAssertFalse(body(messageRuns: [0, 0, 0, Int.max]))
    }

    func testThreadValueDecodesFromItsRealTypeAndKeepsBoolAndNumberApart() throws {
        let json = #"{"id":"i","type":"commandExecution","state":"done","createdAt":"1","payload":{"yes":true,"no":false,"one":1,"zero":0,"pi":3.5,"big":9007199254740993,"neg":-2,"s":"true","n":null,"list":[true,1,"1",null,{"k":[false,0]}],"obj":{"a":{"b":[]}}}}"#
        let slow = try JSONDecoder().decode(ReadItem.self, from: Data(json.utf8))
        let fast = try ThreadJSON.decode(ReadItem.self, from: Data(json.utf8))
        XCTAssertEqual(fast, slow)
        let payload = try XCTUnwrap(fast.payload)
        XCTAssertEqual(payload["yes"], .bool(true)); XCTAssertEqual(payload["no"], .bool(false))
        XCTAssertEqual(payload["one"], .number(1)); XCTAssertEqual(payload["zero"], .number(0))
        XCTAssertEqual(payload["s"], .string("true")); XCTAssertEqual(payload["n"], .null)
        XCTAssertEqual(payload["list"], .array([.bool(true), .number(1), .string("1"), .null, .object(["k": .array([.bool(false), .number(0)])])]))
        XCTAssertThrowsError(try ThreadJSON.decode(ReadItem.self, from: Data(#"{"id":1}"#.utf8)))
        XCTAssertThrowsError(try ThreadJSON.decode(ReadItem.self, from: Data("{broken".utf8)))
    }

    /// Microbench for PERF-2 Q3 (numbers are printed; run with `-c release` for
    /// meaningful timings): one `JSONSerialization` pass against the old
    /// Bool/String/Double/Array/Object attempt cascade, on a compact page.
    func testThreadValuePageDecodeMicrobench() throws {
        let items = (0..<60).map { index -> [String: Any] in
            ["id": "item-\(index)", "type": "commandExecution", "state": "completed", "createdAt": "1", "payload": [
                "command": "git status --short && ls -la", "cwd": "/Users/example/proj", "exitCode": 0, "durationMs": 123.5,
                "aggregatedOutput": String(repeating: "line of output ", count: 8), "processId": NSNull(), "truncated": false, "ok": true,
                "commandActions": [["type": "read", "name": "a.swift", "path": "/a/b/a.swift"], ["type": "list", "name": "x", "path": "/x"]],
                "meta": ["a": 1, "b": "two", "c": [1, 2, 3], "d": ["e": true]]] as [String: Any]]
        }
        let page = try JSONSerialization.data(withJSONObject: ["items": items])
        struct Page: Decodable { let items: [ReadItem] }
        XCTAssertEqual(try ThreadJSON.decode(Page.self, from: page).items, try JSONDecoder().decode(Page.self, from: page).items)
        func time(_ work: () throws -> Void) rethrows -> Double {
            for _ in 0..<5 { try work() }
            let start = Date()
            for _ in 0..<50 { try work() }
            return Date().timeIntervalSince(start) / 50 * 1_000
        }
        let cascade = try time { _ = try JSONDecoder().decode(Page.self, from: page) }
        let single = try time { _ = try ThreadJSON.decode(Page.self, from: page) }
        print("THREADVALUE_DECODE cascade_ms=\(cascade) single_pass_ms=\(single) speedup=\(cascade / single)")
        XCTAssertLessThan(single, cascade, "The single pass must not be slower")
    }

    /// B-143: with the phone locked, nothing is encoded or reported as a
    /// storage failure; only the newest state waits and is written on unlock.
    /// Removal while locked still wins over the waiting state.
    func testLockedDataDefersEncodingThenWritesOnlyTheNewestStateOnUnlock() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let gate = ProtectedDataGate()
        let samples = CacheWriteSamples()
        let writer = ProjectionWriter(store: store, delay: .milliseconds(5), gate: gate) { result, elapsed in samples.append(result, elapsed: elapsed) }
        var state = ProjectionState(); state.install(snapshot(count: 20))
        writer.schedule(state); await writer.flush()
        XCTAssertEqual(samples.values.count, 1)
        gate.update(available: false)
        for sequence in UInt64(11)...30 {
            state.lastSequence = sequence
            writer.schedule(state)
            try await Task.sleep(for: .milliseconds(10))
        }
        await writer.flush()
        XCTAssertEqual(samples.values.count, 1, "Locked data must not be encoded, written or reported as a failure")
        XCTAssertEqual(try store.load().lastSequence, 10)
        gate.update(available: true)
        await writer.flush()
        XCTAssertEqual(samples.values.count, 2, "Unlocking writes once")
        XCTAssertNil(samples.values.last?.error)
        assertState(try loadHistory(store), equals: state)

        gate.update(available: false)
        state.lastSequence = 31
        writer.schedule(state); writer.retire()
        try await writer.remove()
        gate.update(available: true)
        await writer.flush()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path), "A waiting state must not recreate a removed cache")
        XCTAssertEqual(samples.values.count, 2)
    }

    /// B-360/B-143: reading positions are written off the caller's thread,
    /// newest per chat, wait while locked, and never recreate a removed partition.
    @MainActor func testPositionWritesRunOffMainNewestFirstAndWaitForUnlock() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        try store.save(ProjectionState())
        let cache = store.directory.appendingPathComponent("read-cache-v4.json")
        let checkpoint = try Data(contentsOf: cache)
        let gate = ProtectedDataGate()
        let writes = PositionWrites()
        let writer = IntentWriter(store: store, gate: gate) { key, result in
            writes.append(key: key, failed: { if case .failure = result { true } else { false } }(), main: Thread.isMainThread)
        }
        for row in 0..<50 { writer.savePosition("row-\(row)", conversation: "chat") }
        writer.savePosition("other-row", conversation: "other")
        await writer.flush()
        XCTAssertEqual(try store.loadPosition(conversation: "chat"), "row-49")
        XCTAssertEqual(try store.loadPosition(conversation: "other"), "other-row")
        XCTAssertFalse(writes.values.isEmpty)
        XCTAssertTrue(writes.values.allSatisfy { !$0.main && !$0.failed }, "Small saves run off the main thread")
        XCTAssertEqual(try Data(contentsOf: cache), checkpoint, "A reading position never rewrites history")

        gate.update(available: false)
        let before = writes.values.count
        writer.savePosition("locked-1", conversation: "chat")
        writer.savePosition("locked-2", conversation: "chat")
        await writer.flush()
        XCTAssertEqual(writes.values.count, before, "Locked data is not written or reported")
        XCTAssertEqual(try store.loadPosition(conversation: "chat"), "row-49")
        gate.update(available: true)
        await writer.flush()
        XCTAssertEqual(try store.loadPosition(conversation: "chat"), "locked-2")
        XCTAssertEqual(writes.values.count, before + 1)
        XCTAssertEqual(writes.values.last?.key, IntentWriter.positionKey(conversation: "chat"))

        gate.update(available: false)
        writer.savePosition("after-forget", conversation: "chat")
        await writer.discard()
        try store.remove()
        gate.update(available: true)
        writer.savePosition("late", conversation: "chat")
        await writer.flush()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path), "A discarded position must not recreate a removed partition")
        // Another owner removed the partition: this writer prepared it once
        // and does not recreate it for a later scroll.
        let other = ReadStore(root: root, host: "mac", device: "tablet")
        let otherWrites = PositionWrites()
        let survivor = IntentWriter(store: other, gate: gate) { key, result in
            otherWrites.append(key: key, failed: { if case .failure = result { true } else { false } }(), main: Thread.isMainThread)
        }
        survivor.savePosition("first", conversation: "chat"); await survivor.flush()
        XCTAssertEqual(try other.loadPosition(conversation: "chat"), "first")
        try other.remove()
        survivor.savePosition("late", conversation: "chat"); await survivor.flush()
        XCTAssertFalse(FileManager.default.fileExists(atPath: other.directory.path), "A queued position must not recreate a removed partition")
        XCTAssertEqual(otherWrites.values.last?.failed, true, "The dropped write is reported, not silent")
    }

    func testReplacementLoadsRetiringWritersFinalStateBeforeWriting() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let previous = ProjectionWriter(store: store, delay: .seconds(30))
        var state = ProjectionState(); state.install(snapshot())
        previous.schedule(state)
        previous.retire()
        var loaded = try await ProjectionWriter.load(store)
        XCTAssertEqual(loaded.lastSequence, state.lastSequence)
        let replacement = ProjectionWriter(store: store)
        loaded.lastSequence += 1
        replacement.schedule(loaded)
        state.lastSequence += 100
        previous.schedule(state)
        await previous.flush()
        await replacement.flush()
        XCTAssertEqual(try store.load().lastSequence, loaded.lastSequence)
    }
}

private final class LockedCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    var value: Int { lock.lock(); defer { lock.unlock() }; return count }
    func increment() { lock.lock(); count += 1; lock.unlock() }
}

private final class PositionWrites: @unchecked Sendable {
    struct Write { let key: String; let failed: Bool; let main: Bool }
    private let lock = NSLock()
    private var recorded: [Write] = []
    var values: [Write] { lock.withLock { recorded } }
    func append(key: String, failed: Bool, main: Bool) { lock.withLock { recorded.append(Write(key: key, failed: failed, main: main)) } }
}

private final class CacheWriteSamples: @unchecked Sendable {
    struct Sample { let bytes: Int; let elapsed: TimeInterval; let error: String? }
    private let lock = NSLock()
    private var recorded: [Sample] = []
    var values: [Sample] { lock.withLock { recorded } }
    func append(_ result: Result<Int, Error>, elapsed: TimeInterval) {
        lock.withLock {
            switch result {
            case .success(let bytes): recorded.append(Sample(bytes: bytes, elapsed: elapsed, error: nil))
            case .failure(let error): recorded.append(Sample(bytes: 0, elapsed: elapsed, error: String(describing: error)))
            }
        }
    }
}
