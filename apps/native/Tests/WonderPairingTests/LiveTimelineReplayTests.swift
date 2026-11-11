import XCTest
@testable import WonderPairing

/// Replays a streamed reply into a 3,000-item chat and times each event through
/// the steps the phone takes per frame (`ConnectionModel.applyFrame`): apply to
/// the projection, find what changed, refresh the activity summaries and the
/// open chat's prepared rows. Set `WONDER_LIVE_REPLAY_REPORT` to a file path to
/// write p50/p90/p99 per scenario there.
final class LiveTimelineReplayTests: XCTestCase {
    private let conversation = "chat", epoch = "epoch"

    // MARK: Fixture

    /// 500 finished turns of six items each (3,000 items) plus the live turn.
    private func longChat(turns: Int = 500) -> ConversationSnapshot {
        var readTurns: [ReadTurn] = []
        var messages: [ConversationMessage] = []
        var assistant: [AssistantMessage] = []
        for t in 0..<turns {
            let id = "turn-\(t)", stamp = String(format: "2026-10-10T%02d:%02d:00Z", (t / 60) % 24, t % 60)
            let items = [
                ReadItem(id: "\(id):user", type: "userMessage", state: "completed", text: "Question \(t)", createdAt: stamp,
                         payload: ["clientId": .string("client-\(t)")]),
                ReadItem(id: "\(id):think", type: "reasoning", state: "completed", text: nil, createdAt: stamp),
                ReadItem(id: "\(id):cmd0", type: "commandExecution", state: "completed", text: nil, createdAt: stamp,
                         payload: ["command": .string("ls -la"), "status": .string("completed"), "exitCode": .number(0),
                                   "aggregatedOutput": .string(String(repeating: "output line\n", count: 20))]),
                ReadItem(id: "\(id):cmd1", type: "commandExecution", state: "completed", text: nil, createdAt: stamp,
                         payload: ["command": .string("swift build"), "status": .string("completed"), "exitCode": .number(0)]),
                ReadItem(id: "\(id):file", type: "fileChange", state: "completed", text: nil, createdAt: stamp,
                         payload: ["status": .string("completed"),
                                   "changes": .array([.object(["path": .string("a.swift"), "kind": .string("update")])])]),
                ReadItem(id: "\(id):msg", type: "agentMessage", state: "completed", text: "Answer \(t) with some words in it.", createdAt: stamp),
            ]
            readTurns.append(ReadTurn(id: id, items: items, startedAt: stamp, completedAt: stamp, status: "completed"))
            messages.append(ConversationMessage(clientMessageId: "client-\(t)", codexTurnId: id, messageId: "m\(t)", body: "Question \(t)",
                                                state: "completed", createdAt: stamp, attachmentIds: []))
            assistant.append(AssistantMessage(itemId: "\(id):msg", codexTurnId: id, messageId: "assistant:\(id):\(id):msg",
                                              text: "Answer \(t) with some words in it.", state: "completed", createdAt: stamp, updatedAt: stamp))
        }
        let live = ReadTurn(id: "live", items: [
            ReadItem(id: "live:user", type: "userMessage", state: "completed", text: "Go", createdAt: "2026-10-11T00:00:00Z"),
        ], startedAt: "2026-10-11T00:00:00Z", status: "inProgress")
        readTurns.append(live)
        messages.append(ConversationMessage(clientMessageId: "client-live", codexTurnId: "live", messageId: "mlive", body: "Go",
                                            state: "streaming", createdAt: "2026-10-11T00:00:00Z", attachmentIds: []))
        return ConversationSnapshot(conversationId: conversation, hostEpoch: epoch, lastSequence: 100, messages: messages,
                                    assistantMessages: assistant, thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: readTurns),
                                    messageRecords: nil)
    }

    /// The host stamps events to the millisecond, so events do not share a time.
    private static func timestamp(_ sequence: UInt64) -> String {
        let ms = Int(sequence) * 40
        return String(format: "2026-10-11T01:%02d:%02d.%03dZ", (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
    }

    private func event(_ sequence: UInt64, _ body: EventBody, item: String?) -> ReplayEvent {
        ReplayEvent(eventId: String(sequence), hostEpoch: epoch, sequence: sequence,
                    occurredAt: Self.timestamp(sequence),
                    conversationId: conversation, event: body, turnId: "live", itemId: item)
    }

    private func upsert(_ sequence: UInt64, id: String, type: String, state: String, text: String? = nil, turn: String = "live") -> ReplayEvent {
        var detail: [String: Any] = ["turnId": turn, "itemId": id, "state": state, "lifecycleAuthority": 2, "itemType": type,
                                     "item": ["id": id, "type": type, "status": state, "command": "swift test"]]
        if let text { detail["text"] = text }
        let json = String(decoding: try! JSONSerialization.data(withJSONObject: detail), as: UTF8.self)
        return event(sequence, EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: json), item: id)
    }

    /// A streamed reply: a thought, a command, then the text in `deltas` parts, as the batching host sends
    /// them (a journal notice, then the delta, for each part).
    private func textReply(deltas: Int, from first: UInt64) -> [ReplayEvent] {
        var next = first
        func take() -> UInt64 { defer { next += 1 }; return next }
        let reply = "live:msg"
        var events = [
            upsert(take(), id: "live:think", type: "reasoning", state: "started"),
            upsert(take(), id: "live:think", type: "reasoning", state: "completed"),
            upsert(take(), id: "live:cmd", type: "commandExecution", state: "started"),
            upsert(take(), id: "live:cmd", type: "commandExecution", state: "completed"),
            upsert(take(), id: reply, type: "agentMessage", state: "started"),
        ]
        for part in 0..<deltas {
            events.append(event(take(), EventBody(type: "activity", category: "projection_changed", state: "updated",
                                                   detail: #"{"resource":"assistant_messages","change":"update"}"#), item: reply))
            events.append(event(take(), EventBody(type: "assistant_delta", text: "word\(part) "), item: reply))
        }
        events.append(event(take(), EventBody(type: "assistant_completed", text: (0..<deltas).map { "word\($0) " }.joined()), item: reply))
        return events
    }

    /// Tool steps only: each command starts and finishes.
    private func toolBurst(commands: Int, from first: UInt64) -> [ReplayEvent] {
        var next = first
        var events: [ReplayEvent] = []
        for index in 0..<commands {
            events.append(upsert(next, id: "live:burst\(index)", type: "commandExecution", state: "started")); next += 1
            events.append(upsert(next, id: "live:burst\(index)", type: "commandExecution", state: "completed")); next += 1
        }
        return events
    }

    // MARK: The phone's per-event work

    /// `ConnectionModel.applyFrame` for one event, then its publish: the model lets go
    /// of its other copies of the open chat, the projection applies the event in place,
    /// and the rows are refreshed from what the event says it changed.
    private struct Phone {
        var projection = ProjectionState()
        var mirror: [String: ConversationSnapshot] = [:]
        var rowProjection = ConversationRowProjection()
        var rows: [ReadRow] = []
        var activity: CurrentActivity?
        var computer: ComputerAgentActivity = .idle
        var fresh = 0, incremental = 0

        init(_ snapshot: ConversationSnapshot) {
            projection.install(snapshot)
            mirror = projection.snapshots
            rows = rowProjection.rows(for: snapshot, author: "Codex")
        }

        mutating func apply(_ event: ReplayEvent, conversation: String) throws {
            rows = []                         // the row cache lets go of its rows
            mirror[conversation] = nil        // and the mirror of the chat
            _ = try projection.consumeLive(event)
            let changes = projection.takeLiveChanges(conversation)
            mirror[conversation] = projection.snapshots[conversation]
            guard let snapshot = mirror[conversation] else { return }
            rows = rowProjection.rows(for: snapshot, author: "Codex", changes: changes)
            if rowProjection.lastPreparedRowCount > 1 { fresh += 1 } else { incremental += 1 }
            if changes.contains(where: { if case .item = $0.kind { true } else if $0.kind == .structural { true } else { false } }) {
                activity = CurrentActivity.latest(in: snapshot)
                computer = ComputerAgentActivity.latest(in: snapshot)
            }
        }
    }

    // MARK: Measurement

    private func percentiles(_ samples: [Double]) -> (p50: Double, p90: Double, p99: Double, mean: Double) {
        let sorted = samples.sorted()
        func at(_ q: Double) -> Double { sorted[min(sorted.count - 1, Int((Double(sorted.count) * q).rounded(.up)) - 1)] }
        return (at(0.5), at(0.9), at(0.99), samples.reduce(0, +) / Double(samples.count))
    }

    private func measure(_ name: String, events: [ReplayEvent]) throws -> String {
        var phone = Phone(longChat())
        var samples: [Double] = []
        samples.reserveCapacity(events.count)
        let clock = ContinuousClock()
        for event in events {
            let started = clock.now
            try phone.apply(event, conversation: conversation)
            let elapsed = started.duration(to: clock.now)
            samples.append(Double(elapsed.components.seconds) * 1000 + Double(elapsed.components.attoseconds) / 1e15)
        }
        XCTAssertEqual(phone.projection.snapshots[conversation]?.lastSequence, events.last?.sequence)
        let final = try XCTUnwrap(phone.projection.snapshots[conversation])
        assertRows(phone.rows, equal: final.rows(author: "Codex"))
        let p = percentiles(samples)
        return String(format: "%@ events=%d p50=%.4fms p90=%.4fms p99=%.4fms mean=%.4fms", name, samples.count, p.p50, p.p90, p.p99, p.mean)
    }

    func testReplayAThreeThousandItemChat() throws {
        let lines = [try measure("text-stream", events: textReply(deltas: 300, from: 101)),
                     try measure("tool-burst", events: toolBurst(commands: 100, from: 101))]
        print(lines.joined(separator: "\n"))
        if let path = ProcessInfo.processInfo.environment["WONDER_LIVE_REPLAY_REPORT"] {
            try (lines.joined(separator: "\n") + "\n").write(toFile: path, atomically: true, encoding: .utf8)
        }
    }

    /// Contract owner: rows refreshed from live changes equal a fresh projection after every event,
    /// whatever the mix of replies, tool steps, questions, user messages and new turns.
    func testRowsRefreshedFromLiveChangesEqualAFreshProjectionAfterEveryEvent() throws {
        var events = textReply(deltas: 25, from: 101)
        var next = events.last!.sequence + 1
        events += toolBurst(commands: 6, from: next); next += 12
        events.append(upsert(next, id: "live:question", type: "userMessage", state: "completed", text: "Thanks")); next += 1
        events.append(upsert(next, id: "live:msg", type: "agentMessage", state: "completed", text: "Final")); next += 1
        events.append(upsert(next, id: "later:cmd", type: "commandExecution", state: "completed", turn: "later")); next += 1
        events.append(event(next, EventBody(type: "assistant_delta", text: "after completion "), item: "live:msg")); next += 1
        events.append(upsert(next, id: "live:cmd", type: "commandExecution", state: "started")) // forward only: ignored
        var phone = Phone(longChat(turns: 20))
        for event in events {
            try phone.apply(event, conversation: conversation)
            let snapshot = try XCTUnwrap(phone.projection.snapshots[conversation])
            assertRows(phone.rows, equal: snapshot.rows(author: "Codex"))
        }
        let snapshot = try XCTUnwrap(phone.projection.snapshots[conversation])
        var rebuilt = ConversationRowProjection()
        _ = rebuilt.rows(for: snapshot, author: "Codex")
        XCTAssertEqual(phone.rowProjection.retainedInputBytes, rebuilt.retainedInputBytes, "the row cache's byte measure follows its own terms, not the snapshot estimate")
        XCTAssertGreaterThan(phone.incremental, phone.fresh * 3, "text and tool steps refresh in place; only first tokens, messages and turns rebuild")
    }

    /// Contract owner: a live event applies in place and says what it changed.
    func testLiveEventsApplyInPlaceAndNameTheirChange() throws {
        var snapshot = longChat(turns: 10)
        let before = snapshot
        let first = try XCTUnwrap(snapshot.applyLive(upsert(101, id: "live:cmd", type: "commandExecution", state: "started")))
        XCTAssertEqual(first.kind, .item(turn: 10, slot: 1, added: true))
        let delta = try XCTUnwrap(snapshot.applyLive(event(102, EventBody(type: "assistant_delta", text: "Hi"), item: "live:msg")))
        XCTAssertEqual(delta.kind, .reply(slot: 10, added: true))
        let more = try XCTUnwrap(snapshot.applyLive(event(103, EventBody(type: "assistant_delta", text: " there"), item: "live:msg")))
        XCTAssertEqual(more.kind, .reply(slot: 10, added: false))
        XCTAssertEqual(snapshot.assistantMessages.last?.text, "Hi there")
        XCTAssertEqual(snapshot.live.revision, 3)
        XCTAssertEqual(before.live.revision, 0, "a copy taken before the events does not change")
        XCTAssertEqual(before.thread.turns?.last?.items.count, 1)
        XCTAssertNil(snapshot.applyLive(event(103, EventBody(type: "assistant_delta", text: "replayed"), item: "live:msg")), "an event at or below the sequence is not applied twice")
        XCTAssertEqual(snapshot.assistantMessages.last?.text, "Hi there")
    }

    /// Contract owner: live events keep the saved body and its size estimate current without a pass over the chat.
    func testLiveEventsRenewTheSavedBodyOnlyWhenContentChangesAndTrackItsSize() throws {
        var state = ProjectionState()
        state.install(longChat(turns: 30))
        let installed = try XCTUnwrap(state.bodyRecords?[conversation])
        _ = try state.consumeLive(event(101, EventBody(type: "activity", category: "projection_changed", state: "updated",
                                                       detail: #"{"resource":"assistant_messages","change":"update"}"#), item: "live:msg"))
        let sequenceOnly = try XCTUnwrap(state.bodyRecords?[conversation])
        XCTAssertEqual(sequenceOnly.bodyID, installed.bodyID, "a moved sequence rewrites no body")
        for event in textReply(deltas: 10, from: 102) { _ = try state.consumeLive(event) }
        let streamed = try XCTUnwrap(state.bodyRecords?[conversation])
        XCTAssertNotEqual(streamed.bodyID, installed.bodyID, "changed content is a new body")
        var fresh = ProjectionState()
        fresh.install(try XCTUnwrap(state.snapshots[conversation]))
        let exact = Double(try XCTUnwrap(fresh.bodyRecords?[conversation]).residentBytes)
        XCTAssertEqual(Double(streamed.residentBytes), exact, accuracy: exact * 0.01, "the incremental estimate stays within 1% of a full count")
        let restored = try JSONDecoder().decode(ProjectionState.self, from: JSONEncoder().encode(state))
        XCTAssertEqual(restored.snapshots[conversation]?.lastSequence, state.snapshots[conversation]?.lastSequence)
        XCTAssertEqual(restored.bodyRecords?[conversation]?.bodyID, streamed.bodyID)
    }

    /// Contract owner: a read that catches up on events changing no content keeps its saved body.
    func testCatchUpWithoutContentChangesKeepsTheReadsBody() throws {
        for (streamed, expectNewBody) in [(false, false), (true, true)] {
            var state = ProjectionState()
            state.install(longChat(turns: 5))
            var log = LiveEventLog(); log.watch(conversation, from: 100)
            var events = [event(101, EventBody(type: "message_state", state: "streaming"), item: nil),
                          event(102, EventBody(type: "activity", category: "native_history", state: "updated"), item: nil)]
            if streamed { events.append(event(103, EventBody(type: "assistant_delta", text: "x"), item: "live:msg")) }
            for event in events { _ = try state.consumeLive(event); log.record(event) }
            XCTAssertTrue(state.dirty.contains(conversation))
            // The read finds the chat as it was saved, so its body is the saved one until events change content.
            let saved = try XCTUnwrap(state.bodyRecords?[conversation])
            var read = longChat(turns: 5)
            read.lastSequence = 101           // read after the lifecycle event, before the rest
            state.install(read, catchingUp: log)
            XCTAssertEqual(state.snapshots[conversation]?.lastSequence, events.last?.sequence)
            let caughtUp = try XCTUnwrap(state.bodyRecords?[conversation])
            XCTAssertEqual(caughtUp.bodyID != saved.bodyID, expectNewBody)
        }
    }

    /// Contract owner: a projection that missed the changes (too many, or none given) rebuilds instead of guessing.
    func testRowsRebuildWhenTheChangesAreMissingOrBounded() throws {
        var state = ProjectionState()
        state.install(longChat(turns: 8))
        var projection = ConversationRowProjection()
        _ = projection.rows(for: try XCTUnwrap(state.snapshots[conversation]), author: "Codex")
        var sequence: UInt64 = 101
        for _ in 0..<(ProjectionState.liveChangeLimit + 90) {
            _ = try state.consumeLive(event(sequence, EventBody(type: "activity", category: "native_history", state: "updated"), item: nil)); sequence += 1
        }
        _ = try state.consumeLive(event(sequence, EventBody(type: "assistant_delta", text: "late"), item: "live:msg"))
        let snapshot = try XCTUnwrap(state.snapshots[conversation])
        let changes = state.takeLiveChanges(conversation)
        XCTAssertLessThanOrEqual(changes.count, ProjectionState.liveChangeLimit)
        assertRows(projection.rows(for: snapshot, author: "Codex", changes: changes), equal: snapshot.rows(author: "Codex"))
        XCTAssertEqual(state.takeLiveChanges(conversation), [], "taken changes are not replayed twice")
        // Without the changes a descendant of the retained snapshot is rebuilt too.
        var later = snapshot
        _ = later.applyLive(event(sequence + 1, EventBody(type: "assistant_delta", text: " more"), item: "live:msg"))
        assertRows(projection.rows(for: later, author: "Codex"), equal: later.rows(author: "Codex"))
    }
}
