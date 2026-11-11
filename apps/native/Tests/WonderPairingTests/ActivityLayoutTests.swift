import XCTest
@testable import WonderPairing

final class ActivityLayoutTests: XCTestCase {
    private func row(_ i: Int, type: String = "commandExecution", output: String = "ok") -> ReadRow {
        let item = ReadItem(id:"item-\(i)",type:type,state:"completed",text:nil,createdAt:"1700000000000",payload:["output":.string(output),"arguments":.object(["input":.string(output)]),"command":.string("example"),"diffs":.array([.object(["path":.string("one"),"diff":.string("+one\n-two")])])])
        return ReadRow(id:"turn/item-\(i)",author:"Agent",text:"",isUser:false,timestamp:"1700000000000",turnId:"turn",item:item)
    }

    func testRemovedActivityDetailOrFileAnchorMapsToItsSurvivingHeader() {
        let source = row(0)
        let entry = ChatFeedEntry.grouping([source]).first!
        let header = entry.id
        let nodes = [header]

        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: "activity:" + source.id, entries: [entry], nodeIDs: nodes), header)
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: "file:" + header + ":preview", entries: [entry], nodeIDs: nodes), header)
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: source.id, entries: [entry], nodeIDs: nodes), header)
        XCTAssertNil(ChatFeedNode.survivingAnchor(for: "activity:missing", entries: [entry], nodeIDs: nodes))
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: header, entries: [entry], nodeIDs: nodes), header)
    }

    /// A reading anchor saved at one detail level (or before a turn folded)
    /// restores at another onto a node that is shown. Reopening waits for the
    /// target's frame, so a hidden target (a Working header at Conversation)
    /// left the chat on "Loading chat…" forever.
    func testEverySavedAnchorResolvesToAVisibleNodeAtEveryDetailLevel() throws {
        func user(_ id: String, _ turn: String) -> ReadRow {
            ReadRow(id: id, author: "You", text: "Question", isUser: true, timestamp: "1000", turnId: turn)
        }
        func text(_ id: String, _ turn: String, phase: String? = nil) -> ReadRow {
            ReadRow(id: id, author: "agent", text: "Text", isUser: false, timestamp: "1000", authorId: "agent", turnId: turn,
                item: ReadItem(id: id, type: "agentMessage", state: "completed", text: "Text", createdAt: "1000",
                               payload: phase.map { ["phase": .string($0)] } ?? [:]))
        }
        func tool(_ id: String, _ turn: String) -> ReadRow {
            ReadRow(id: id, author: "agent", text: "", isUser: false, timestamp: "1000", authorId: "agent", turnId: turn,
                item: ReadItem(id: id, type: "commandExecution", state: "completed", text: nil, createdAt: "1000",
                               payload: ["command": .string("swift test"), "exitCode": .number(0)]))
        }
        let rows = [user("u1", "t1"), text("a1", "t1", phase: "commentary"), tool("x1", "t1"), tool("x2", "t1"),
                    media("img", type: "imageGeneration", fileID: "pic", turn: "t1"), text("folded", "t1"), text("r1", "t1"),
                    user("u2", "t2"), text("a2", "t2", phase: "commentary"), tool("x3", "t2"), text("r2", "t2"),
                    user("u3", "t3"), tool("x4", "t3"), tool("x5", "t3")]
        let turns = ["t1", "t2", "t3"].reduce(into: [String: ReadTurn]()) { $0[$1] = ReadTurn(id: $1, items: [], status: "completed") }
        let entries = ChatFeedEntry.grouping(ReadRow.settlingFinishedTurns(rows, turns: turns))
        let work = entries.filter(\.isActivity).map(\.id)
        XCTAssertEqual(work.count, 3)
        let first = try XCTUnwrap(entries.first { $0.rows.contains { $0.id == "folded" } })
        XCTAssertTrue(first.isActivity, "the earlier message folds into Working")

        // Every anchor a reader can leave, at any level and disclosure.
        let everyExpansion: Set<String> = Set(work + ["commands:x1", "commands:x4"])
        var anchors = Set(rows.map(\.id) + entries.map(\.id))
        for detail in ChatDetailLevel.allCases {
            for open in [Set<String>(), everyExpansion] {
                anchors.formUnion(ChatFeedNode.visible(entries, expanded: open, detail: detail).map(\.id))
            }
        }
        XCTAssertTrue(anchors.isSuperset(of: [work[0], "activity:x1", "commands:x1", "file:\(work[0]):pic", "folded"]))
        for detail in ChatDetailLevel.allCases {
            for open in [Set<String>(), everyExpansion] {
                let ids = ChatFeedNode.visible(entries, expanded: open, detail: detail).map(\.id)
                for anchor in anchors {
                    let resolved = ChatFeedNode.survivingAnchor(for: anchor, entries: entries, nodeIDs: ids)
                    XCTAssertNotNil(resolved, "\(anchor) at \(detail)")
                    XCTAssertTrue(resolved.map(ids.contains) ?? false, "\(anchor) at \(detail) -> \(String(describing: resolved))")
                }
                for unknown in ["missing", "activity:missing", "file:missing:pic", "conversation-bottom"] {
                    XCTAssertNil(ChatFeedNode.survivingAnchor(for: unknown, entries: entries, nodeIDs: ids), "\(unknown) reads from the end")
                }
            }
        }

        // At Conversation: what each hidden anchor lands on.
        let conversation = ChatFeedNode.visible(entries, expanded: [], detail: .conversation).map(\.id)
        XCTAssertEqual(conversation, ["u1", "file:\(work[0]):pic", "r1", "u2", "r2", "u3"])
        func resolve(_ anchor: String) -> String? {
            ChatFeedNode.survivingAnchor(for: anchor, entries: entries, nodeIDs: conversation)
        }
        // A Working header, its tool rows and a folded message land on what
        // still shows of their group: its picture.
        for anchor in [work[0], "activity:a1", "activity:x2", "commands:x1", "folded", "activity:folded"] {
            XCTAssertEqual(resolve(anchor), "file:\(work[0]):pic", anchor)
        }
        // A group with nothing left continues at the reply after it.
        XCTAssertEqual(resolve(work[1]), "r2")
        XCTAssertEqual(resolve("activity:x3"), "r2")
        // The last group, with nothing after it, keeps the message before it.
        XCTAssertEqual(resolve(work[2]), "u3")
        XCTAssertEqual(resolve("commands:x4"), "u3")

        // Back at Everything, an anchor saved at Conversation is still there,
        // and a tool row hidden by a closed group lands on its header.
        let everything = ChatFeedNode.visible(entries, expanded: []).map(\.id)
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: "file:\(work[0]):pic", entries: entries, nodeIDs: everything), "file:\(work[0]):pic")
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: "activity:x3", entries: entries, nodeIDs: everything), work[1])
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: "folded", entries: entries, nodeIDs: everything), work[0])
    }

    private func media(_ id: String, type: String, fileID: String, turn: String = "turn", author: String = "agent", phase: String? = nil,
                       mime: String = "image/png") -> ReadRow {
        let file: ThreadValue = .object(["id": .string(fileID), "name": .string("Preview.png"),
            "mimeType": .string(mime), "state": .string("available"), "updatedAt": .string("now")])
        var payload: [String: ThreadValue] = ["result": .object(["content": .array([
            .object(["type": .string("wonderArtifact"), "file": file])])])]
        if let phase { payload["phase"] = .string(phase) }
        return ReadRow(id: id, author: author, text: "Here is the image.", isUser: false, timestamp: "1000", authorId: author,
            turnId: turn, item: ReadItem(id: id, type: type, state: "completed", text: "Here is the image.", createdAt: "1000", payload: payload))
    }

    private func files(_ nodes: [ChatFeedNode]) -> [String] {
        nodes.flatMap { node -> [String] in
            switch node.content {
            case .file(let file): return [file.id]
            case .gallery(let files): return files.map(\.id)
            default: return []
            }
        }
    }

    private func item(_ id: String, type: String, state: String = "completed", text: String? = nil,
                      payload: [String: ThreadValue] = [:]) -> ReadRow {
        ReadRow(id: id, author: "Agent", text: "", isUser: false, timestamp: "1700000000000", turnId: "turn",
                item: ReadItem(id: id, type: type, state: state, text: text, createdAt: "1700000000000", payload: payload))
    }

    private func user(_ id: String, turn: String) -> ReadRow {
        ReadRow(id: id, author: "You", text: "Please fix it.", isUser: true, timestamp: "1700000000000", turnId: turn)
    }

    private func work(_ id: String, type: String, turn: String, state: String = "completed", text: String? = nil,
                      payload: [String: ThreadValue] = [:]) -> ReadRow {
        ReadRow(id: turn + "/" + id, author: "Agent", text: text ?? "", isUser: false, timestamp: "1700000000000", turnId: turn,
                item: ReadItem(id: id, type: type, state: state, text: text, createdAt: "1700000000000", payload: payload))
    }

    private var rows0ID: String { row(0).id }

}

private extension ReadRow {
    func withTurn(_ turn: String, state: String? = nil) -> ReadRow {
        ReadRow(id: turn + "/" + (item?.id ?? id), author: author, text: text, isUser: isUser, timestamp: timestamp, turnId: turn,
                item: item.map { ReadItem(id: $0.id, type: $0.type, state: state ?? $0.state, text: $0.text, createdAt: $0.createdAt, payload: $0.payload) })
    }
}

private extension AsyncQuestion {
    func withTurn(_ turn: String) -> AsyncQuestion {
        AsyncQuestion(id: id, conversationId: conversationId, turnId: turn, itemId: itemId, questions: questions,
                      state: state, expiresAtMs: expiresAtMs, response: response)
    }
}
