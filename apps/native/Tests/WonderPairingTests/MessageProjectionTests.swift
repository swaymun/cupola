import XCTest
@testable import WonderPairing

final class MessageProjectionTests: XCTestCase {
    private func snapshot(_ items: [ReadItem], clients: [String] = ["client"], attachmentIds: [String] = []) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: clients.enumerated().map { index, client in
                ConversationMessage(clientMessageId: client, codexTurnId: "turn", messageId: "stored-\(index)",
                    body: "Same message", state: "completed", createdAt: "1000", attachmentIds: attachmentIds)
            }, assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true,
                turns: [ReadTurn(id: "turn", items: items)]))
    }
    private func echo(_ id: String, client: String?) -> ReadItem {
        ReadItem(id: id, type: "userMessage", state: "completed", text: "Runtime-wrapped input", createdAt: "0",
                 payload: client.map { ["clientId": .string($0)] })
    }
    // The thread already names its latest provider. Historical replies must
    // retain the speaker on their own side of the switch, including live overlays.
    func testReplySpeakersFollowProviderSwitchThroughCacheAndStreaming() throws {
        let original = snapshot([
            ReadItem(id: "before", type: "agentMessage", state: "completed", text: "First reply", createdAt: "1000"),
            ReadItem(id: "switch", type: "providerSwitch", state: "completed", text: "Switched to Claude Sonnet", createdAt: "2000",
                payload: ["fromFamily": .string("codex"), "toFamily": .string("claude")]),
            ReadItem(id: "after", type: "agentMessage", state: "streaming", text: "Second reply", createdAt: "3000")
        ], clients: [])
        let restored = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(original))
        var projection = ConversationRowProjection()
        for value in [original, restored] {
            let rows = projection.rows(for: value, author: "Claude")
            XCTAssertEqual(rows.filter { $0.item?.type == "agentMessage" }.map(\.author), ["Codex", "Claude"])
            XCTAssertEqual(rows.first(where: { $0.item?.type == "providerSwitch" })?.providerSwitchText, "Switched to Claude Sonnet")
        }
        // A late overlay of the first provider must retain that speaker even
        // on the incremental path, whose fallback names the latest provider.
        for (item, timestamp) in [("before", "1000"), ("after", "3000")] {
            for frame in 1...2 {
                let value = ConversationSnapshot(conversationId: original.conversationId, hostEpoch: original.hostEpoch,
                    lastSequence: UInt64(frame + 2), messages: [], assistantMessages: [
                        AssistantMessage(itemId: item, codexTurnId: "turn", messageId: "live", text: "Frame \(frame)",
                            state: "streaming", createdAt: timestamp, updatedAt: "4000")], thread: original.thread)
                let rows = projection.rows(for: value, author: "Claude")
                XCTAssertEqual(rows.filter { $0.item?.type == "agentMessage" }.map(\.author), ["Codex", "Claude"])
                XCTAssertEqual(rows.first { $0.item?.id == item }?.text, "Frame \(frame)")
            }
        }
    }
    // A message sent through T3 Code (owner, Oct 21) is one bubble. Its
    // agent's record of it reaches the phone before the Mac knows it is that
    // message; a later read names the record with the message's client ID.
    // The phone then shows one bubble, the owner's, with the message's files
    // and delivery state, also over the cached read that showed two (a read
    // that left the record out would keep the cached one). The same words
    // typed in T3 Code are a bubble of their own.
    func testARecordTheMacNamesAsASentMessageIsThatMessagesBubble() throws {
        func read(named: Bool) throws -> ConversationSnapshot {
            let label = named ? #","payload":{"clientId":"client"}"# : ""
            let json = """
            {"conversationId":"chat","hostEpoch":"epoch","lastSequence":2,"assistantMessages":[],
             "thread":{"nextCursor":null,"hydrated":true,"turns":[
               {"id":"local:stored","status":"unknown","items":[{"id":"stored","type":"userMessage","state":"completed","text":"Continue here","createdAt":"1000","payload":{"clientId":"client"}}]},
               {"id":"t3-turn","status":"inProgress","items":[
                 {"id":"record","type":"userMessage","state":"completed","text":"Continue here","createdAt":"2000"\(label)},
                 {"id":"typed","type":"userMessage","state":"completed","text":"Continue here","createdAt":"2500"},
                 {"id":"reply","type":"agentMessage","state":"completed","text":"Done","createdAt":"3000"}]}]},
             "messages":[{"clientMessageId":"client","messageId":"stored","body":"Continue here","state":"completed",
               "createdAt":"1000","attachmentIds":["phone-photo"]}]}
            """
            return try JSONDecoder().decode(ConversationSnapshot.self, from: Data(json.utf8))
        }
        let cached = try read(named: false)
        XCTAssertEqual(cached.rows(author: "Claude").map(\.id), ["user-client", "user-record", "user-typed", "t3-turn/reply"])
        for snapshot in [try read(named: true), try read(named: true).mergingOlder(cached)] {
            let rows = snapshot.rows(author: "Claude")
            XCTAssertEqual(rows.map(\.id), ["user-client", "user-typed", "t3-turn/reply"])
            XCTAssertEqual(rows.map(\.author), ["You", "You", "Claude"])
            XCTAssertEqual(rows.map(\.isUser), [true, true, false])
            XCTAssertEqual(rows.map(\.attachmentIds), [["phone-photo"], [], []])
            XCTAssertNil(rows[0].undelivered)
        }
    }
    // A Project send re-reads native history only when the screen may be
    // behind the Mac: another newest turn, one still running, or a status change.
    func testASendSkipsTheFullReadOnlyWhenTheNewestNativeTurnIsShownSettled() {
        let shown = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1, messages: [],
            assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [
                ReadTurn(id: "older", items: [], status: "completed"),
                ReadTurn(id: "newest", items: [], status: "completed"),
                ReadTurn(id: "local:pending", items: [], status: "inProgress")]))
        XCTAssertTrue(shown.showsLatestNativeTurn(id: "newest", status: "completed", runningElsewhere: false))
        XCTAssertFalse(shown.showsLatestNativeTurn(id: "desktop-turn", status: "completed", runningElsewhere: false))
        XCTAssertFalse(shown.showsLatestNativeTurn(id: "newest", status: "inProgress", runningElsewhere: true))
        XCTAssertFalse(shown.showsLatestNativeTurn(id: "newest", status: "failed", runningElsewhere: false))
        XCTAssertFalse(shown.showsLatestNativeTurn(id: nil, status: "none", runningElsewhere: false))
        let empty = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1, messages: [],
            assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true))
        XCTAssertFalse(empty.showsLatestNativeTurn(id: "newest", status: "completed", runningElsewhere: false))
    }
    func testAnsweredQuestionIncludesDurableSelection() throws {
        let json = #"{"id":"q","conversationId":"chat","turnId":"turn","itemId":"question","questions":[{"title":"What next?","options":["Code","Design"]}],"state":"answered","expiresAtMs":100,"response":{"answers":["Code"],"skip":false}}"#
        let question = try JSONDecoder().decode(AsyncQuestion.self, from: Data(json.utf8))
        XCTAssertEqual(question.response?.answers, ["Code"])
        XCTAssertFalse(question.canAnswer(now: 1))
    }
    func testNativeQuestionAndReplyRenderAsOneSavedFormAfterCacheAndPaging() throws {
        let form = ReadItem(id: "call-question", type: "agentMessage", state: "completed", text: "Which day?", createdAt: "1000",
            payload: ["delivery": .string("async"), "questions": .array([
                .object(["title": .string("Which day?"), "options": .array([.string("Saturday"), .string("Sunday")])]),
                .object(["title": .string("What should we check?"), "options": .null])
            ])])
        let body = #"<send_user_message_question_reply>[{"questionItemId":"[\"request_user_input_async\",\"call-question\",0]","question":"Which day?","answer":"Saturday"},{"questionItemId":"[\"request_user_input_async\",\"call-question\",1]","question":"What should we check?","answer":"Navigation"}]</send_user_message_question_reply>"#
        let reply = ReadItem(id: "reply", type: "userMessage", state: "completed", text: body, createdAt: "2000")
        let original = snapshot([form, reply], clients: [])
        let cached = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(original))
        let paged = try snapshot([reply], clients: []).mergingOlder(snapshot([form], clients: []))
        for value in [original, cached, paged] {
            let rows = value.rows(author: "Ada")
            XCTAssertEqual(rows.map(\.id), ["turn/call-question"])
            let question = try XCTUnwrap(rows.first?.nativeQuestion)
            XCTAssertEqual(question.questions.first?.options, ["Saturday", "Sunday"])
            XCTAssertEqual(question.answers, [0: "Saturday", 1: "Navigation"])
            XCTAssertTrue(question.isAnswered)
        }
        let unloaded = try XCTUnwrap(snapshot([reply], clients: []).rows(author: "Ada").first)
        XCTAssertEqual(unloaded.text, "Which day?\nSaturday\n\nWhat should we check?\nNavigation")
        XCTAssertTrue(unloaded.isUser)
    }
    func testNativeQuestionReplyNeverHidesUnmatchedOrMalformedInputOrAttachments() throws {
        let form = ReadItem(id: "call-question", type: "agentMessage", state: "completed", text: "", createdAt: "1000",
            payload: ["delivery": .string("async"), "questions": .array([.object(["title": .string("Which day?")])])])
        let body = #"<send_user_message_question_reply>[{"questionItemId":"[\"request_user_input_async\",\"call-question\",0]","question":"Different question","answer":"Saturday"}]</send_user_message_question_reply>"#
        let reply = ReadItem(id: "reply", type: "userMessage", state: "completed", text: body, createdAt: "2000")
        let rows = snapshot([form, reply], clients: []).rows(author: "Ada")
        XCTAssertEqual(rows.count, 2)
        XCTAssertFalse(try XCTUnwrap(rows.first?.nativeQuestion).isAnswered)
        XCTAssertEqual(rows.last?.text, "Different question\nSaturday")
        for malformed in ["<send_user_message_question_reply>broken</send_user_message_question_reply>",
                          body + "\nKeep this additional text.",
                          body.replacingOccurrences(of: "request_user_input_async", with: "unknown") ] {
            XCTAssertEqual(ReadRow(id: "user", author: "You", text: malformed, isUser: true, timestamp: "1").text, malformed)
        }
        let attached = ReadRow(id: "attached", author: "You", text: body.replacingOccurrences(of: "Different question", with: "Which day?"),
                               isUser: true, timestamp: "2000", attachmentIds: ["photo"])
        let withAttachment = ReadRow.reconcilingQuestions([try XCTUnwrap(rows.first), attached])
        XCTAssertEqual(withAttachment.count, 2)
        XCTAssertEqual(withAttachment.last?.attachmentIds, ["photo"])
        XCTAssertTrue(try XCTUnwrap(withAttachment.first?.nativeQuestion).isAnswered)
    }
    func testRuntimeEchoUsesCanonicalBodyIdentityAndTimestamp() throws {
        let original = snapshot([echo("runtime", client: "client")])
        let cached = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(original))
        for value in [original, cached, try original.mergingOlder(cached)] {
            let rows = value.rows(author: "Ada")
            XCTAssertEqual(rows.count, 1)
            XCTAssertEqual(rows[0].id, "user-client")
            XCTAssertEqual(rows[0].text, "Same message")
            XCTAssertEqual(rows[0].timestamp, "1000")
        }
    }
    func testIdenticalIntentionalSendsStaySeparateIncludingGuideInSameTurn() {
        let rows = snapshot([echo("runtime-a", client: "a"), echo("runtime-b", client: "b")], clients: ["a", "b"]).rows(author: "Ada")
        XCTAssertEqual(Set(rows.map(\.id)), ["user-a", "user-b"])
        XCTAssertEqual(rows.count, 2)
    }
    func testRuntimeItemsWithoutClientIdUseOnlyUnambiguousTurnMatch() {
        XCTAssertEqual(snapshot([echo("runtime", client: nil)]).rows(author: "Ada").count, 1)
        let ambiguous = snapshot([echo("runtime", client: nil)], clients: ["a", "b"]).rows(author: "Ada")
        XCTAssertEqual(ambiguous.count, 3) // Never silently drop an unmatched user message.
    }
    func testDuplicateRuntimeEchoesCollapseByClientIdentity() {
        XCTAssertEqual(snapshot([echo("one", client: "client"), echo("two", client: "client")]).rows(author: "Ada").count, 1)
    }
    func testToolPayloadSurvivesCacheAndRowProjection() throws {
        let json = #"{"id":"tool","type":"mcpToolCall","state":"failed","text":null,"createdAt":"2000","payload":{"tool":"search","arguments":{"query":"hello"},"result":[{"text":"result"}],"error":"Unavailable","durationMs":42,"futureField":true}}"#
        let item = try JSONDecoder().decode(ReadItem.self, from: Data(json.utf8))
        let value = snapshot([item], clients: [])
        let cached = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(value))
        let row = try XCTUnwrap(cached.rows(author: "Ada").first)
        XCTAssertEqual(row.turnId, "turn")
        XCTAssertEqual(row.item?.type, "mcpToolCall")
        XCTAssertEqual(row.item?.state, "failed")
        XCTAssertEqual(row.item?.payload, item.payload)
        XCTAssertEqual(row.item?.payload?["durationMs"]?.number, 42)
    }
    func testCommentaryAndFinalKeepProtocolOrderAcrossCacheAndHistory() throws {
        // App Server items often share a hydration timestamp; IDs are not chronology.
        let items = [
            ReadItem(id: "z-comment", type: "agentMessage", state: "completed", text: "I’ll check the files.", createdAt: "1000", payload: ["phase": .string("commentary")]),
            ReadItem(id: "m-tool", type: "dynamicToolCall", state: "completed", text: nil, createdAt: "1000"),
            ReadItem(id: "a-final", type: "agentMessage", state: "completed", text: "The check passed.", createdAt: "1000", payload: ["phase": .string("final_answer")])
        ]
        let original = snapshot(items, clients: [])
        let cached = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(original))
        for value in [original, cached, try original.mergingOlder(cached)] {
            let rows = value.rows(author: "Ada")
            XCTAssertEqual(rows.map(\.id), ["turn/z-comment", "turn/m-tool", "turn/a-final"])
            XCTAssertEqual(rows.map(\.isCommentary), [true, false, false])
            XCTAssertEqual(rows.first?.text, "I’ll check the files.")
        }
    }

    func testHistoryMergeKeepsTurnOrderWithEqualTimestamps() {
        func turn(_ id: String) -> ReadTurn {
            ReadTurn(id: id, items: [ReadItem(id: id, type: "agentMessage", state: "completed", text: id, createdAt: "1000")])
        }
        let older = ThreadProjection(nextCursor: nil, hydrated: true, turns: [turn("z"), turn("m")])
        let newer = ThreadProjection(nextCursor: nil, hydrated: true, turns: [turn("m"), turn("a")])
        XCTAssertEqual(newer.mergingOlder(older).turns?.map(\.id), ["z", "m", "a"])
    }

    func testStreamingCommentaryReconcilesWithHydrationWithoutBecomingFinal() throws {
        let item = ReadItem(id: "comment", type: "agentMessage", state: "streaming", text: "Checking", createdAt: "1000", payload: ["phase": .string("commentary")])
        let base = snapshot([item], clients: [])
        let live = AssistantMessage(itemId: "comment", codexTurnId: "turn", messageId: "stored", text: "Checking the files…", state: "streaming", createdAt: "1000", updatedAt: "2000")
        let value = ConversationSnapshot(conversationId: base.conversationId, hostEpoch: base.hostEpoch, lastSequence: 3,
            messages: [], assistantMessages: [live], thread: base.thread)
        let cached = try JSONDecoder().decode(ConversationSnapshot.self, from: JSONEncoder().encode(value))
        let rows = cached.rows(author: "Ada")
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0].text, "Checking the files…")
        XCTAssertTrue(rows[0].isCommentary)
        XCTAssertEqual(rows[0].item?.state, "streaming")
        let unknown = ReadRow(id: "unknown", author: "Ada", text: "Reply", isUser: false, timestamp: "1000")
        XCTAssertFalse(unknown.isCommentary) // Missing phase stays ordinary visible agent speech.
    }

}
