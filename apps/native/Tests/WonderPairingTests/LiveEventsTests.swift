import XCTest
@testable import WonderPairing

/// Streamed events from a batching host apply to the open conversation in
/// place; everything else still marks it stale for an authoritative read.
final class LiveEventsTests: XCTestCase {
    private let turn = "turn-1", item = "turn-1:msg:0"

    // Shared with wonderd: the native fallback cleans legacy/cache display,
    // never the authoritative text to which later deltas are appended.
    func testRenderingFixturesKeepRawStreamsAndUserPastes() throws {
        struct Probe: Decodable { let name: String, type: String, input: String, expected: String }
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let probes = try JSONDecoder().decode([Probe].self, from: Data(contentsOf:
            root.appendingPathComponent("tests/fixtures/protocol/render-sanitize/host-cases.json")))
        for probe in probes {
            let user = probe.type == "userMessage"
            let row = ReadRow(id: "probe", author: user ? "You" : "Codex", text: probe.input, isUser: user, timestamp: "1000")
            XCTAssertEqual(row.text, probe.expected, probe.name)
            if user { continue }
            var chat = snapshot()
            var sequence: UInt64 = 11
            // Split within controls/tags as a real stream can. Every prefix
            // keeps its original bytes in the snapshot and cleans only display.
            var prefix = ""
            for part in probe.input.map(String.init) {
                prefix += part
                chat = try XCTUnwrap(chat.applyingLive(delta(sequence, part)))
                XCTAssertEqual(chat.assistantMessages.first?.text, prefix, probe.name)
                sequence += 1
            }
            XCTAssertEqual(chat.assistantMessages.first?.text, probe.input, probe.name)
            XCTAssertEqual(chat.rows(author: "Codex").last?.text, probe.expected, probe.name)
            let complete = event(sequence, EventBody(type: "assistant_completed", text: probe.input))
            chat = try XCTUnwrap(chat.applyingLive(complete))
            let restored = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(chat))
            XCTAssertEqual(restored.assistantMessages.first?.text, probe.input, probe.name)
            XCTAssertEqual(restored.rows(author: "Codex").last?.text, probe.expected, probe.name)
        }
        let partial = try XCTUnwrap(snapshot().applyingLive(delta(11, "Before\u{1b}]8;;https://example.invalid")))
        XCTAssertEqual(partial.rows(author: "Codex").last?.text, "Before")
        let linked = try XCTUnwrap(partial.applyingLive(delta(12, "\u{7}link\u{1b}]8;;\u{7}")))
        XCTAssertEqual(linked.rows(author: "Codex").last?.text, "Beforelink")
    }

    private func snapshot(sequence: UInt64 = 10, assistant: [AssistantMessage] = [], turns: [ReadTurn]? = []) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: sequence,
            messages: [ConversationMessage(messageId: "m1", body: "Hi", state: "streaming", createdAt: "2026-10-10T00:00:00Z", attachmentIds: [])],
            assistantMessages: assistant, thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: turns))
    }
    private func event(_ sequence: UInt64, _ body: EventBody, conversation: String? = "chat", item: String? = nil) -> ReplayEvent {
        ReplayEvent(eventId: String(sequence), hostEpoch: "epoch", sequence: sequence, occurredAt: "2026-10-10T00:00:0\(sequence % 10)Z",
                    conversationId: conversation, event: body, turnId: turn, itemId: item ?? self.item)
    }
    private func delta(_ sequence: UInt64, _ text: String) -> ReplayEvent { event(sequence, EventBody(type: "assistant_delta", text: text)) }
    private func textChanged(_ sequence: UInt64) -> ReplayEvent {
        event(sequence, EventBody(type: "activity", category: "projection_changed", state: "updated", detail: #"{"resource":"assistant_messages","change":"update"}"#))
    }
    private func upsert(_ sequence: UInt64, id: String, type: String, state: String, text: String? = nil, turn: String? = nil, historyRefresh: Bool = false) -> ReplayEvent {
        var detail: [String: Any] = ["turnId": turn ?? self.turn, "itemId": id, "state": state, "lifecycleAuthority": 2, "itemType": type,
                                     "item": ["id": id, "type": type, "status": state, "command": "ls"]].merging(text.map { ["text": $0] } ?? [:]) { a, _ in a }
        if historyRefresh { detail["historyRefresh"] = true }
        let json = String(decoding: try! JSONSerialization.data(withJSONObject: detail), as: UTF8.self)
        return event(sequence, EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: json), item: id)
    }
    private func state(with snapshot: ConversationSnapshot) -> ProjectionState {
        var state = ProjectionState(); state.install(snapshot); return state
    }

    func testThinkingTimeMatchesObservedEventsAndSurvivesCacheRoundTrip() throws {
        func observed(_ sequence: UInt64, _ state: String, _ time: String, authority: Int = 2) -> ReplayEvent {
            let detail = "{\"turnId\":\"turn-1\",\"itemId\":\"thinking\",\"state\":\"\(state)\",\"lifecycleAuthority\":\(authority),\"itemType\":\"reasoning\",\"item\":{\"id\":\"thinking\",\"type\":\"reasoning\",\"summary\":[\"private\"]}}"
            return ReplayEvent(eventId: "event-\(sequence)", hostEpoch: "epoch", sequence: sequence, occurredAt: time,
                conversationId: "chat", event: EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: detail), turnId: turn)
        }
        let base = snapshot(turns: [ReadTurn(id: turn, items: [], status: "inProgress")])
        let started = try XCTUnwrap(base.applyingLive(observed(11, "started", "1000")))
        let active = try XCTUnwrap(started.rows(author: "Codex").first?.thinkingTiming)
        XCTAssertEqual(active.activeLabel(at: Date(timeIntervalSince1970: 13.9)), "Thinking · 12s")
        XCTAssertEqual(active.activeLabel(at: Date(timeIntervalSince1970: 0)), "Thinking")
        XCTAssertEqual(active.activeLabel(at: Date(timeIntervalSince1970: 90_000)), "Thinking")
        XCTAssertFalse(String(data: try JSONEncoder().encode(started), encoding: .utf8)!.contains("private"))
        let duplicate = try XCTUnwrap(started.applyingLive(observed(12, "started", "9000")))
        let completed = try XCTUnwrap(duplicate.applyingLive(observed(13, "completed", "13900")))
        let restored = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(completed))
        XCTAssertEqual(restored.rows(author: "Codex").first?.thinkingTiming?.completedLabel, "Thought for 12s")
        for state in ["failed", "interrupted"] {
            let stopped = try XCTUnwrap(started.applyingLive(observed(12, state, "13900")))
            XCTAssertNil(stopped.rows(author: "Codex").first?.thinkingTiming)
        }
        for events in [[observed(11, "completed", "13900"), observed(12, "started", "14000")],
                       [observed(11, "started", "9000"), observed(12, "completed", "1000")],
                       [observed(11, "started", "1000", authority: 1), observed(12, "completed", "13900")]] {
            var chat = base
            for event in events { chat = try XCTUnwrap(chat.applyingLive(event)) }
            XCTAssertNil(chat.rows(author: "Codex").first?.thinkingTiming)
        }
        let revised = try XCTUnwrap(completed.applyingLive(observed(14, "completed", "90000", authority: 1)))
        XCTAssertEqual(revised.rows(author: "Codex").first?.thinkingTiming?.completedLabel, "Thought for 12s")
        let isoStart = try XCTUnwrap(base.applyingLive(observed(11, "started", "2026-10-13T10:00:00.125Z")))
        let isoEnd = try XCTUnwrap(isoStart.applyingLive(observed(12, "completed", "2026-10-13T10:00:12.999Z")))
        XCTAssertEqual(isoEnd.rows(author: "Codex").first(where: { $0.item?.id == "thinking" })?.thinkingTiming?.completedLabel, "Thought for 12s")
        let item = try XCTUnwrap(started.thread.turns?.first?.items.first)
        XCTAssertNil(ThinkingTiming.prepare(item, hostEpoch: "restarted-host"))
    }

    func testDeltasApplyInPlaceAndKeepTheChatCurrent() throws {
        var state = state(with: snapshot())
        XCTAssertEqual(try state.consumeLive(textChanged(11)), .applied("chat"))
        XCTAssertEqual(try state.consumeLive(delta(12, "Hel")), .applied("chat"))
        XCTAssertEqual(try state.consumeLive(textChanged(13)), .applied("chat"))
        XCTAssertEqual(try state.consumeLive(delta(14, "lo")), .applied("chat"))
        let chat = try XCTUnwrap(state.snapshots["chat"])
        XCTAssertEqual(chat.assistantMessages.map(\.text), ["Hello"])
        // The host's own message identity, so a later read replaces this row.
        XCTAssertEqual(chat.assistantMessages.first?.messageId, "assistant:\(turn):\(item)")
        XCTAssertEqual(chat.lastSequence, 14)
        XCTAssertFalse(state.dirty.contains("chat"))
        XCTAssertEqual(chat.rows(author: "Claude").last?.text, "Hello")
    }

    // Streamed text changes nothing a Chats row shows, so it reads no list; a
    // tool step changes the row's current step, so the list follows it.
    func testOnlyToolStepsMarkTheChatsListForReading() throws {
        var state = state(with: snapshot())
        state.listDirty = false
        _ = try state.consumeLive(textChanged(11))
        _ = try state.consumeLive(delta(12, "Hel"))
        XCTAssertFalse(state.listDirty)
        XCTAssertEqual(try state.consumeLive(upsert(13, id: "command", type: "commandExecution", state: "started")), .applied("chat"))
        XCTAssertTrue(state.listDirty)
    }

    func testAReplayedEventIsNotAppliedTwiceAndAGapResyncs() throws {
        var state = state(with: snapshot())
        _ = try state.consumeLive(delta(11, "Hi"))
        XCTAssertEqual(try state.consumeLive(delta(11, "Hi")), .skipped)
        XCTAssertEqual(state.snapshots["chat"]?.assistantMessages.first?.text, "Hi")
        XCTAssertThrowsError(try state.consumeLive(delta(13, "!")))

        // A GET may be ahead of the socket. It covers content, never the
        // global replay cursor or unrelated caches (including attention).
        let ahead = snapshot(sequence: 16)
        state.install(ahead)
        XCTAssertEqual(state.lastSequence, 11)
        state.listDirty = false
        XCTAssertEqual(try state.consumeLive(delta(12, "already in the GET")), .applied("chat"))
        XCTAssertTrue(state.dirty.isEmpty)
        XCTAssertFalse(state.listDirty, "Already-covered text must not schedule a list read")
        XCTAssertEqual(state.snapshots["chat"]?.lastSequence, 16)
        XCTAssertEqual(state.snapshots["chat"]?.assistantMessages.count, ahead.assistantMessages.count)
        XCTAssertEqual(state.lastSequence, 12)
        state.listDirty = false
        XCTAssertEqual(try state.consumeLive(textChanged(13)), .applied("chat"))
        XCTAssertFalse(state.listDirty, "The text's covered journal notice also needs no list read")
        XCTAssertEqual(try state.consumeLive(event(14, EventBody(type: "approval_opened"))), .applied("chat"))
        XCTAssertTrue(state.listDirty, "Covered approval events still reach non-content consumers")
        XCTAssertTrue(state.dirty.isEmpty)
        XCTAssertEqual(state.lastSequence, 14)
        XCTAssertThrowsError(try state.consumeLive(delta(16, "gap")))
        var changedEpoch = delta(15, "epoch")
        changedEpoch = ReplayEvent(eventId: changedEpoch.eventId, hostEpoch: "new-epoch", sequence: 15,
            occurredAt: changedEpoch.occurredAt, conversationId: "chat", event: changedEpoch.event,
            turnId: changedEpoch.turnId, itemId: changedEpoch.itemId)
        XCTAssertThrowsError(try state.consumeLive(changedEpoch))
        XCTAssertEqual(state.lastSequence, 14)
        _ = try state.consumeLive(event(15, EventBody(type: "approval_resolved")))
        _ = try state.consumeLive(event(16, EventBody(type: "message_state", state: "completed")))
        XCTAssertTrue(state.dirty.isEmpty)
        XCTAssertEqual(try state.consumeLive(event(17, EventBody(type: "message_state", state: "completed"))), .invalidated("chat"))
    }

    func testLifecycleEventsStillNeedAnAuthoritativeRead() throws {
        var state = state(with: snapshot())
        XCTAssertEqual(try state.consumeLive(event(11, EventBody(type: "message_state", state: "completed"))), .invalidated("chat"))
        XCTAssertTrue(state.dirty.contains("chat"))
        // Once stale, text waits for the read too instead of patching old content.
        XCTAssertEqual(try state.consumeLive(delta(12, "late")), .invalidated("chat"))
        XCTAssertTrue(state.snapshots["chat"]?.assistantMessages.isEmpty == true)
        // Other invalidations (delivery state, settings) are not applied.
        XCTAssertEqual(try state.consumeLive(event(13, EventBody(type: "activity", category: "projection_changed", state: "updated",
            detail: #"{"resource":"messages","change":"update"}"#))), .invalidated("chat"))
    }

    // Another app wrote the chat on the Mac, or its work there changed status.
    // The host's wake-up starts the open chat's own history check (and the
    // sidebar's read); it does not force a snapshot read or change it.
    func testANativeHistoryWakeUpLeavesTheChatCurrentAndUnchanged() throws {
        for category in ["native_history", "native_status"] {
            let before = snapshot(assistant: [AssistantMessage(itemId: item, codexTurnId: turn, messageId: "a", text: "Done",
                state: "completed", createdAt: "2026-10-10T00:00:01Z", updatedAt: "2026-10-10T00:00:01Z")],
                turns: [ReadTurn(id: turn, items: [], startedAt: "1", completedAt: "2", status: "completed")])
            var state = state(with: before)
            XCTAssertEqual(try state.consumeLive(event(11, EventBody(type: "activity", category: category, state: "changed"))), .applied("chat"))
            let chat = try XCTUnwrap(state.snapshots["chat"])
            XCTAssertFalse(state.dirty.contains("chat"), category)
            XCTAssertEqual(chat.lastSequence, 11)
            XCTAssertEqual(chat.assistantMessages.map(\.text), ["Done"])
            XCTAssertEqual(chat.messages.map(\.messageId), before.messages.map(\.messageId))
            XCTAssertEqual(chat.thread.turns, before.thread.turns, "a child completion wake must not start a parent turn")
            XCTAssertNil(chat.activeTurnID)
        }
    }

    func testAReadLandingAfterNewerEventsCatchesUpInsteadOfStayingStale() throws {
        var state = state(with: snapshot())
        var log = LiveEventLog(); log.watch("chat", from: 10)
        // A lifecycle event makes the chat stale; text keeps streaming while
        // the read is in flight.
        for event in [event(11, EventBody(type: "message_state", state: "streaming")), textChanged(12), delta(13, "Hel"), textChanged(14), delta(15, "lo")] {
            _ = try state.consumeLive(event); log.record(event)
        }
        XCTAssertTrue(state.dirty.contains("chat"))
        // The host read happened after the message state and the first delta.
        let read = snapshot(sequence: 13, assistant: [AssistantMessage(itemId: item, codexTurnId: turn, messageId: "assistant:\(turn):\(item)",
            text: "Hel", state: "streaming", createdAt: "2026-10-10T00:00:03Z", updatedAt: "2026-10-10T00:00:03Z")])
        state.install(read, catchingUp: log)
        XCTAssertFalse(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.assistantMessages.map(\.text), ["Hello"])
        XCTAssertEqual(state.snapshots["chat"]?.lastSequence, 15)
    }

    func testCatchUpStopsAtAnEventItCannotApply() throws {
        var state = state(with: snapshot())
        var log = LiveEventLog(); log.watch("chat", from: 10)
        for event in [event(11, EventBody(type: "message_state", state: "streaming")), delta(12, "Hel"),
                      event(13, EventBody(type: "approval_opened")), delta(14, "lo")] {
            _ = try state.consumeLive(event); log.record(event)
        }
        state.install(snapshot(sequence: 11), catchingUp: log)
        XCTAssertTrue(state.dirty.contains("chat"))
        // A log that started after the read's sequence cannot vouch for it.
        var late = LiveEventLog(); late.watch("chat", from: 12)
        XCTAssertNil(late.events(after: 11))
    }

    // Large tool output must not keep megabytes of applied events alive. The
    // log trims by bytes as it does by count, and never lets a read catch up
    // across events it dropped: that read stays stale and is read again.
    func testTheCatchUpLogIsBoundedByBytesAndNeverClaimsDroppedEvents() throws {
        let large = String(repeating: "x", count: 1000)
        let size = LiveEventLog.size(of: delta(11, large))
        var log = LiveEventLog(byteLimit: size * 4); log.watch("chat", from: 10)
        for sequence in UInt64(11)...15 { log.record(delta(sequence, large)) }
        XCTAssertLessThanOrEqual(log.bytes, size * 4)
        XCTAssertLessThan(log.count, 5)
        // Reads older than what was dropped cannot catch up; newer ones still can.
        XCTAssertNil(log.events(after: 10))
        let kept = try XCTUnwrap(log.events(after: 15 - UInt64(log.count)))
        XCTAssertEqual(kept.map(\.sequence), Array((16 - UInt64(log.count))...15))

        // A read that landed before the trim stays stale instead of replaying a gap.
        var state = state(with: snapshot())
        var racing = LiveEventLog(byteLimit: size * 4); racing.watch("chat", from: 10)
        let stale = event(11, EventBody(type: "message_state", state: "streaming"))
        _ = try state.consumeLive(stale); racing.record(stale)
        for sequence in UInt64(12)...16 { let next = delta(sequence, large); _ = try state.consumeLive(next); racing.record(next) }
        state.install(snapshot(sequence: 11), catchingUp: racing)
        XCTAssertTrue(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.lastSequence, 11)
    }

    func testAnOversizedEventMovesTheCoverageFloorInsteadOfBeingKept() throws {
        let small = LiveEventLog.size(of: delta(11, "Hi"))
        var log = LiveEventLog(byteLimit: small * 8); log.watch("chat", from: 10)
        log.record(delta(11, "Hi"))
        log.record(delta(12, String(repeating: "x", count: small * 8)))
        XCTAssertEqual(log.count, 0)
        XCTAssertEqual(log.bytes, 0)
        XCTAssertNil(log.events(after: 11), "the oversized event cannot be replayed")
        XCTAssertEqual(log.events(after: 12)?.count, 0)
        log.record(delta(13, "lo"))
        XCTAssertEqual(log.events(after: 12)?.map(\.sequence), [13])
    }

    func testAHostWideBarrierSurvivesByteTrimmingOrMovesTheFloorPastIt() throws {
        let size = LiveEventLog.size(of: delta(11, "Hi"))
        var log = LiveEventLog(byteLimit: size * 3); log.watch("chat", from: 10)
        let barrier = event(12, EventBody(type: "approval_opened"), conversation: nil)
        // The chat and the log see the same events: a read at 11 lands after all of them.
        var state = state(with: snapshot())
        for next in [event(11, EventBody(type: "message_state", state: "streaming")), barrier, delta(13, "!"), delta(14, "!"), delta(15, "!")] {
            _ = try state.consumeLive(next); log.record(next)
        }
        // Either the barrier is still recorded, or no read before it can catch up.
        for after in UInt64(10)...14 {
            guard let pending = log.events(after: after) else { continue }
            XCTAssertTrue(after >= 12 || pending.contains { $0.sequence == 12 }, "after \(after)")
        }
        state.install(snapshot(sequence: 11), catchingUp: log)
        XCTAssertTrue(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.lastSequence, 11)
    }

    // Once a read is installed and current, or under memory pressure, the
    // covered events go. A read already in flight from before then is not
    // caught up from the emptied log: it stays stale for another read.
    func testReleasingCoveredEventsKeepsAnInFlightOlderReadStale() throws {
        var state = state(with: snapshot())
        var log = LiveEventLog(); log.watch("chat", from: 10)
        for next in [event(11, EventBody(type: "message_state", state: "streaming")), delta(12, "Hel"), delta(13, "lo")] {
            _ = try state.consumeLive(next); log.record(next)
        }
        log.release(through: 13)
        XCTAssertEqual(log.count, 0)
        XCTAssertEqual(log.bytes, 0)
        state.install(snapshot(sequence: 11), catchingUp: log)
        XCTAssertTrue(state.dirty.contains("chat"))
        // A read at the released point still catches up on what follows.
        log.record(delta(14, "!"))
        let read = snapshot(sequence: 13, assistant: [AssistantMessage(itemId: item, codexTurnId: turn, messageId: "assistant:\(turn):\(item)",
            text: "Hello", state: "streaming", createdAt: "2026-10-10T00:00:03Z", updatedAt: "2026-10-10T00:00:03Z")])
        _ = try state.consumeLive(delta(14, "!"))
        state.install(read, catchingUp: log)
        XCTAssertFalse(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.assistantMessages.map(\.text), ["Hello!"])
        // Watching another chat drops everything recorded for this one.
        log.watch(nil, from: 14)
        XCTAssertEqual(log.count, 0)
        XCTAssertNil(log.conversation)
    }

    func testItemsUpsertForwardOnly() throws {
        var state = state(with: snapshot(turns: [ReadTurn(id: turn, items: [], status: "inProgress")]))
        XCTAssertEqual(try state.consumeLive(upsert(11, id: "tool-1", type: "commandExecution", state: "started")), .applied("chat"))
        XCTAssertEqual(try state.consumeLive(upsert(12, id: "tool-1", type: "commandExecution", state: "completed")), .applied("chat"))
        _ = try state.consumeLive(upsert(13, id: "tool-1", type: "commandExecution", state: "started"))
        let items = try XCTUnwrap(state.snapshots["chat"]?.thread.turns?.first?.items)
        XCTAssertEqual(items.map(\.id), ["tool-1"])
        XCTAssertEqual(items.first?.state, "completed")
        XCTAssertEqual(items.first?.payload?["command"]?.string, "ls")
        // A new turn's first item opens that turn after the others.
        XCTAssertEqual(try state.consumeLive(upsert(14, id: "x", type: "agentMessage", state: "started", turn: "turn-2")), .applied("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.thread.turns?.map(\.id), [turn, "turn-2"])
        // Without loaded turns, items need a read.
        var bare = self.state(with: snapshot(turns: nil))
        XCTAssertEqual(try bare.consumeLive(upsert(11, id: "tool-1", type: "commandExecution", state: "started")), .invalidated("chat"))
    }

    // A tool update changes one turn: it labels that turn alone and returns
    // the others as they were. New records still label every loaded turn they
    // name, and an update that changes no label copies no other turn.
    func testAToolUpdateLabelsOnlyItsTurnAndRecordsLabelEveryTurn() throws {
        func prompt(_ id: String) -> ReadItem {
            ReadItem(id: id, type: "userMessage", state: "completed", text: "Hi", createdAt: "2026-10-10T00:00:00Z")
        }
        let earlier = ReadTurn(id: "earlier", items: [prompt("asked")], status: "completed")
        let current = ReadTurn(id: turn, items: [prompt("now")], status: "inProgress")
        let records = [MessageRecord(turnId: "earlier", itemId: "asked", clientId: "client-1"),
                       MessageRecord(turnId: turn, itemId: "now", clientId: "client-2")]
        let base = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 10, messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [earlier, current]), messageRecords: records)
        let tool = try XCTUnwrap(base.applyingLive(upsert(11, id: "tool-1", type: "commandExecution", state: "started")))
        let turns = try XCTUnwrap(tool.thread.turns)
        XCTAssertEqual(turns[0], earlier, "A tool update in another turn leaves this one as it was")
        XCTAssertEqual(turns[1].items.map(\.id), ["now", "tool-1"])
        XCTAssertEqual(turns[1].items[0].payload?["clientId"]?.string, "client-2", "The updated turn keeps its message's bubble")
        // Records arriving later label every loaded turn they name.
        let unlabelled = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 10, messages: [], assistantMessages: [],
            thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [earlier, current]), messageRecords: nil)
        let detail = String(decoding: try JSONEncoder().encode(["records": records]), as: UTF8.self)
        let named = try XCTUnwrap(unlabelled.applyingLive(event(11, EventBody(type: "activity", category: "message_records", state: "updated", detail: detail))))
        XCTAssertEqual(named.thread.turns?.map { $0.items[0].payload?["clientId"]?.string }, ["client-1", "client-2"])
        // Labelling a thread whose labels are all present returns it unchanged.
        XCTAssertEqual(named.thread.labeling(records), named.thread)
        XCTAssertEqual(named.thread.labeling(records, onlyTurn: "missing"), named.thread)
    }

    func testFramesDecodeAsOneEventOrABatch() throws {
        let one = #"{"eventId":"e","hostEpoch":"h","sequence":3,"occurredAt":"now","conversationId":"c","turnId":"t","itemId":"i","event":{"type":"assistant_delta","data":{"text":"Hi"}}}"#
        let single = try ReplayEvent.frame(Data(one.utf8))
        XCTAssertFalse(single.batched)
        XCTAssertEqual(single.events.first?.event.text, "Hi")
        XCTAssertEqual(single.events.first?.itemId, "i")
        let batch = try ReplayEvent.frame(Data(("[" + one + "," + one.replacingOccurrences(of: "\"sequence\":3", with: "\"sequence\":4") + "]").utf8))
        XCTAssertTrue(batch.batched)
        XCTAssertEqual(batch.events.map(\.sequence), [3, 4])
    }

    func testHistoryRefreshKeepsOlderItemsOutOfTheLiveTailUntilRead() throws {
        // A paged chat contains only its latest turn. Importing an older item
        // again must not date it at refresh time or append it after that reply.
        let latest = ReadTurn(id: "latest", items: [ReadItem(id: "latest-reply", type: "agentMessage", state: "completed",
            text: "Latest reply", createdAt: "2026-10-10T00:00:01Z")], status: "completed")
        let before = snapshot(turns: [latest])
        var state = state(with: before)
        let refresh = upsert(11, id: "older-reply", type: "agentMessage", state: "completed", text: "Older reply",
                             turn: "older", historyRefresh: true)
        var log = LiveEventLog(); log.watch("chat", from: 10); log.record(refresh)
        XCTAssertEqual(try state.consumeLive(refresh), .invalidated("chat"))
        XCTAssertTrue(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.thread.turns?.map(\.id), ["latest"])
        // A read that races the import cannot replay it as fresh live speech.
        state.install(before, catchingUp: log)
        XCTAssertTrue(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.thread.turns?.map(\.id), ["latest"])
        // The authoritative read still completes hydration, including a first
        // import or a newly discovered item, with its stored position intact.
        let older = ReadTurn(id: "older", items: [ReadItem(id: "older-reply", type: "agentMessage", state: "completed",
            text: "Older reply", createdAt: "2026-10-10T00:00:00Z")], status: "completed")
        state.install(snapshot(sequence: 11, turns: [older, latest]), catchingUp: log)
        XCTAssertFalse(state.dirty.contains("chat"))
        XCTAssertEqual(state.snapshots["chat"]?.thread.turns?.map(\.id), ["older", "latest"])
        XCTAssertEqual(state.snapshots["chat"]?.rows(author: "Claude").last?.text, "Latest reply")
    }

    // A message sent through T3 Code (owner, Oct 21) shows exactly once. The
    // agent's record of it (same words) reaches the phone before the Mac knows
    // it is that message's, so until the phone hears so, the two may show
    // apart; from then on, and after every reload, they are one bubble, the
    // owner's with its files, and the message never disappears. Every order of
    // the Mac's news, live updates (one of them re-sending the record
    // unlabelled), a refresh, a refresh read before the news, and an older
    // page asked for before the news, starting with the record saved or not
    // and with the news heard or missed, ends in a reload.
    func testASentMessageAndItsAgentsRecordShowOnceInEveryOrder() throws {
        let words = "Continue here"
        let message = #"{"clientMessageId":"client","messageId":"stored","body":"Continue here","state":"completed","createdAt":"2026-10-10T00:00:01Z","attachmentIds":["phone-photo"]}"#
        let sent = #"{"id":"local:stored","status":"unknown","items":[{"id":"stored","type":"userMessage","state":"completed","text":"Continue here","createdAt":"2026-10-10T00:00:01Z","payload":{"clientId":"client"}}]}"#
        func t3(labelled: Bool) -> String {
            #"{"id":"t3-turn","status":"completed","items":[{"id":"record","type":"userMessage","state":"completed","text":"Continue here","createdAt":"2026-10-10T00:00:02Z""#
                + (labelled ? #","payload":{"clientId":"client"}"# : "")
                + #"},{"id":"reply","type":"agentMessage","state":"completed","text":"Done","createdAt":"2026-10-10T00:00:03Z"}]}"#
        }
        let later = #"{"id":"latest","status":"completed","items":[{"id":"later","type":"agentMessage","state":"completed","text":"Later","createdAt":"2026-10-10T00:00:04Z"}]}"#
        let newer = #"{"id":"newer-turn","status":"unknown","items":[{"id":"newer","type":"agentMessage","state":"completed","text":"Newer","createdAt":"2026-10-10T00:00:05Z","payload":{"id":"newer","type":"agentMessage","status":"completed","command":"ls"}}]}"#
        let records = #"[{"turnId":"t3-turn","itemId":"record","clientId":"client"}]"#
        // The Mac at sequence `at`: 11 has saved the record, 12 knows it is the
        // message's (and says so), 13 adds a reply, 14 re-sends the record.
        func read(_ at: UInt64, turns: [String], cursor: String?) throws -> ConversationSnapshot {
            try JSONDecoder().decode(ConversationSnapshot.self, from: Data("""
            {"conversationId":"chat","hostEpoch":"epoch","lastSequence":\(at),"assistantMessages":[],"messages":[\(message)],
             "thread":{"nextCursor":\(cursor.map { "\"\($0)\"" } ?? "null"),"hydrated":true,"turns":[\(turns.joined(separator: ","))]}
             \(at >= 12 ? #","messageRecords":"# + records : "")}
            """.utf8))
        }
        func latestPage(_ at: UInt64) throws -> ConversationSnapshot {
            try read(at, turns: [sent, later] + (at >= 13 ? [newer] : []), cursor: "older")
        }
        func olderPage(_ at: UInt64) throws -> ConversationSnapshot { try read(at, turns: [t3(labelled: at >= 12)], cursor: nil) }
        let news: [UInt64: ReplayEvent] = [
            12: event(12, EventBody(type: "activity", category: "message_records", state: "updated", detail: #"{"records":"# + records + "}"), conversation: "chat"),
            13: upsert(13, id: "newer", type: "agentMessage", state: "completed", text: "Newer", turn: "newer-turn"),
            14: upsert(14, id: "record", type: "userMessage", state: "completed", text: words, turn: "t3-turn"),
        ]
        enum Step: String { case mac, refresh, partialRefresh, staleRefresh, olderPage }
        func orders(_ steps: [Step]) -> [[Step]] {
            guard !steps.isEmpty else { return [[]] }
            return Set(steps.indices.flatMap { index -> [[Step]] in
                var rest = steps; let first = rest.remove(at: index)
                return orders(rest).map { [first] + $0 }
            }).sorted { $0.map(\.rawValue).joined() < $1.map(\.rawValue).joined() }
        }
        var failures = [String]()
        var cases = 0
        for recordSaved in [false, true] {
            for heard in [true, false] {
                for order in orders([.mac, .mac, .mac, .refresh, .partialRefresh, .staleRefresh, .olderPage]) {
                    cases += 1
                    let label = "saved=\(recordSaved) heard=\(heard) \(order.map(\.rawValue).joined(separator: ","))"
                    var state = state(with: recordSaved ? try latestPage(11).mergingOlder(olderPage(11)) : try latestPage(11))
                    var log = LiveEventLog(); log.watch("chat", from: 11)
                    var mac: UInt64 = 11, knows = false, broken = false
                    // ConnectionModel's refresh: the page merged over the saved chat, then installed.
                    func refresh(_ page: ConversationSnapshot, partial: Bool = false) throws {
                        guard let cached = state.snapshots["chat"], page.lastSequence >= cached.lastSequence else { return }
                        state.install(try partial ? page.mergingOlderKeepingCursor(cached) : page.mergingOlder(cached), catchingUp: log)
                    }
                    func check(_ step: String) {
                        guard let rows = state.snapshots["chat"]?.rows(author: "Claude").filter({ $0.text == words }) else {
                            return failures.append("\(label) after \(step): no chat")
                        }
                        if rows.isEmpty || (knows && rows.count != 1) {
                            failures.append("\(label) after \(step): \(rows.map(\.id))")
                        } else if knows, rows[0].author != "You" || rows[0].attachmentIds != ["phone-photo"] {
                            failures.append("\(label) after \(step): \(rows[0].author) \(rows[0].attachmentIds)")
                        }
                    }
                    check("start")
                    for step in order {
                        switch step {
                        case .mac:
                            mac += 1
                            let next = try XCTUnwrap(news[mac])
                            // Missing the news breaks the stream: nothing more arrives until a read.
                            if broken || (!heard && mac == 12) { broken = true; break }
                            log.record(next)
                            let outcome = try state.consumeLive(next)
                            // The first news reaches a current chat: one bubble without a read.
                            if mac == 12 { XCTAssertEqual(outcome, .applied("chat"), label); knows = true }
                        case .refresh:
                            try refresh(try latestPage(mac)); if mac >= 12 { knows = true }
                        case .partialRefresh:
                            // The fresh page published while older unfinished items
                            // are re-read, and that older read then fails.
                            try refresh(try latestPage(mac), partial: true); if mac >= 12 { knows = true }
                        case .staleRefresh:
                            try refresh(try latestPage(11))
                        case .olderPage:
                            // ConnectionModel.loadOlder: asked for at 11, merged into the latest chat.
                            guard let latest = state.snapshots["chat"], latest.thread.nextCursor == "older" else { break }
                            state.snapshots["chat"] = try latest.mergingOlder(olderPage(11))
                        }
                        check("\(step.rawValue)@\(mac)")
                    }
                    // Reopening reads the latest page, then the older one.
                    try refresh(try latestPage(mac)); knows = true
                    if let latest = state.snapshots["chat"], latest.thread.nextCursor == "older" {
                        state.snapshots["chat"] = try latest.mergingOlder(olderPage(mac))
                    }
                    check("reload")
                }
            }
        }
        XCTAssertEqual(cases, 2 * 2 * 840)
        XCTAssertEqual(failures, [], "\(Set(failures.map { $0.split(separator: " ")[0...2].joined(separator: " ") }).count) of \(cases) orders fail")
    }

    func testAReadReplacesLiveTextWithoutDuplicatingTheRow() throws {
        var state = state(with: snapshot())
        _ = try state.consumeLive(delta(11, "Hel"))
        let live = try XCTUnwrap(state.snapshots["chat"])
        let read = snapshot(sequence: 20, assistant: [AssistantMessage(itemId: item, codexTurnId: turn, messageId: "assistant:\(turn):\(item)",
            text: "Hello", state: "completed", createdAt: "2026-10-10T00:00:01Z", updatedAt: "2026-10-10T00:00:02Z")])
        let merged = try read.mergingOlder(live)
        XCTAssertEqual(merged.assistantMessages.map(\.text), ["Hello"])
    }
}


/// Observable contract: concurrent GETs share a read, bursts keep one trailing
/// read, and attention/terminal work is not trapped behind progress cadence.
@MainActor final class RefreshCoordinatorTests: XCTestCase {
    private func waitFor(_ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition(), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
        XCTAssertTrue(condition(), "Coordinator did not reach the expected state before the deadline")
    }

    func testReadsJoinAndWakesDuringReadKeepOneTrailingPass() async throws {
        let coordinator = RefreshCoordinator()
        defer { coordinator.cancel() }
        var reads = 0, attention = 0
        var release: CheckedContinuation<Void, Never>?
        let operation: @MainActor () async throws -> Void = {
            reads += 1
            if reads == 1 { await withCheckedContinuation { release = $0 } }
        }
        let first = Task { try await coordinator.read(.conversation("a"), operation: operation) }
        try await waitFor { reads == 1 }
        let joined = Task { try await coordinator.read(.conversation("a"), operation: operation) }
        for _ in 0..<20 {
            coordinator.request(.conversation("a"), cadence: .zero, delay: .zero) {
                try? await coordinator.read(.conversation("a"), operation: operation)
            }
        }
        coordinator.request(.attention, cadence: .zero, delay: .zero) {
            try? await coordinator.read(.attention) { attention += 1 }
        }
        try await waitFor { attention == 1 }
        XCTAssertEqual(reads, 1, "Attention runs while history stays held; overlapping GETs join")
        release?.resume(); release = nil
        try await first.value; try await joined.value
        try await waitFor { reads == 2 }
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(reads, 2, "Twenty wakes produce exactly one trailing read")
    }

    func testProgressCadenceAndPromptTerminalBypass() async throws {
        let coordinator = RefreshCoordinator()
        defer { coordinator.cancel() }
        var started: [ContinuousClock.Instant] = []
        let request: @MainActor (Bool) -> Void = { urgent in
            coordinator.request(.nativeHistory("a"), cadence: urgent ? .seconds(30) : .milliseconds(180), delay: .zero, urgent: urgent) {
                try? await coordinator.read(.nativeHistory("a")) { started.append(.now) }
            }
        }
        request(false)
        try await waitFor { started.count == 1 }
        for _ in 0..<20 { request(false) }
        try await waitFor { started.count == 2 }
        XCTAssertGreaterThanOrEqual(started[1] - started[0], .milliseconds(180))
        request(false) // Sleeping on the next progress interval.
        request(true)  // A terminal event interrupts that sleep.
        try await waitFor { started.count == 3 }
        XCTAssertLessThan(started[2] - started[1], .seconds(10), "Terminal reconciliation bypasses even a long progress interval")
    }

    func testPostImportGETWaitsForAnOlderGETThenReadsImportedContent() async throws {
        let coordinator = RefreshCoordinator()
        defer { coordinator.cancel() }
        var reads = 0, importedContentRead = false
        var release: CheckedContinuation<Void, Never>?
        let old = Task {
            try await coordinator.read(.conversation("a")) {
                reads += 1
                await withCheckedContinuation { release = $0 }
            }
        }
        try await waitFor { reads == 1 }
        let canonical = Task {
            try await coordinator.read(.conversation("a"), afterCurrent: true) {
                reads += 1
                importedContentRead = true
            }
        }
        try await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(reads, 1)
        release?.resume(); release = nil
        try await old.value; try await canonical.value
        XCTAssertEqual(reads, 2, "The final canonical GET must not join a GET started before import finished")
        XCTAssertTrue(importedContentRead)
    }

    func testCancelledCallerCannotStartANewReadOrWake() async throws {
        let coordinator = RefreshCoordinator()
        var reads = 0
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            do {
                try await coordinator.read(.conversation("old")) { reads += 1 }
                XCTFail("A cancelled caller must not start a new scope")
            } catch is CancellationError {
            } catch { XCTFail("Unexpected error: \(error)") }
            coordinator.request(.attention, cadence: .zero, delay: .zero) { reads += 1 }
        }
        await cancelled.value
        try await Task.sleep(for: .milliseconds(20))
        XCTAssertEqual(reads, 0)
        coordinator.cancel()
    }

    func testClosingAScreenDropsItsPendingImportAndKeepsOtherScopes() async throws {
        let coordinator = RefreshCoordinator()
        defer { coordinator.cancel() }
        var imports = 0, attention = 0
        coordinator.request(.nativeHistory("a"), cadence: .zero, delay: .seconds(30)) { imports += 1 }
        coordinator.discardRequests(.nativeHistory("a"))
        coordinator.request(.attention, cadence: .zero, delay: .zero) { attention += 1 }
        try await waitFor { attention == 1 }
        XCTAssertEqual(imports, 0)
        // Reopening the same screen must schedule normally even if the old
        // pump has not returned from its interrupted sleep yet.
        coordinator.request(.nativeHistory("a"), cadence: .zero, delay: .zero) { imports += 1 }
        try await waitFor { imports == 1 }
        XCTAssertEqual(attention, 1)
    }

    func testEndingConnectionCancelsPendingWorkAndAllowsFreshScope() async throws {
        let coordinator = RefreshCoordinator()
        var reads = 0
        coordinator.request(.list, cadence: .seconds(30), delay: .seconds(30)) { reads += 1 }
        coordinator.cancel()
        coordinator.request(.list, cadence: .zero, delay: .zero) {
            try? await coordinator.read(.list) { reads += 1 }
        }
        try await waitFor { reads == 1 }
        coordinator.cancel()
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(reads, 1, "Old connection work never runs in the new connection")
    }
}
