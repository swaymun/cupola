import XCTest
@testable import WonderPairing

/// A finished turn shows the owner's message, one Working group and the reply;
/// every earlier agent text is inside the group (`ReadRow.settlingFinishedTurns`).
/// The mini-map has one mark per turn (`ConversationMinimapProjection`).
final class TurnCollapseTests: XCTestCase {
    private func user(_ id: String, turn: String, text: String? = nil) -> ReadRow {
        ReadRow(id: id, author: "You", text: text ?? "Question \(id)", isUser: true, timestamp: "1000", turnId: turn)
    }
    private func text(_ id: String, turn: String, phase: String? = nil, body: String? = nil) -> ReadRow {
        let item = ReadItem(id: id, type: "agentMessage", state: "completed", text: body ?? "Text \(id)", createdAt: "1000",
                            payload: phase.map { ["phase": .string($0)] } ?? [:])
        return ReadRow(id: id, author: "Claude", text: body ?? "Text \(id)", isUser: false, timestamp: "1000", turnId: turn, item: item)
    }
    private func tool(_ id: String, turn: String) -> ReadRow {
        let item = ReadItem(id: id, type: "commandExecution", state: "completed", text: nil, createdAt: "1000",
                            payload: ["command": .string("swift test"), "exitCode": .number(0)])
        return ReadRow(id: id, author: "Activity", text: "", isUser: false, timestamp: "1000", turnId: turn, item: item)
    }
    private func turns(_ statuses: [String: String]) -> [String: ReadTurn] {
        statuses.mapValues { ReadTurn(id: "", items: [], status: $0) }
    }
    /// The visible layout of a projection: user and reply bubbles by row ID,
    /// a collapsed Working group as "work[rows...]".
    private func layout(_ rows: [ReadRow], _ statuses: [String: String], active: Set<String> = [], expanded: Bool = false) -> [String] {
        let settled = ReadRow.settlingFinishedTurns(rows, turns: turns(statuses), activeTurnIDs: active)
        let entries = ChatFeedEntry.grouping(settled, activeTurnIDs: active)
        let open = expanded ? Set(entries.filter(\.isActivity).map(\.id)) : []
        return ChatFeedNode.visible(entries, expanded: open).map { node in
            switch node.content {
            case .entry(let entry) where entry.isActivity: "work[" + entry.rows.map(\.id).joined(separator: ",") + "]"
            case .entry(let entry): entry.id
            case .activity(let row): "  " + row.id
            default: node.id
            }
        }
    }

    func testStreamingShowsLiveNotesThenFoldsThemUnderTheSameIDs() {
        let rows = [user("u", turn: "t"), text("a", turn: "t", phase: "commentary"), tool("x", turn: "t"),
                    text("b", turn: "t"), text("c", turn: "t")]
        // Live: the open group shows its notes; unmarked text shows as it arrives.
        XCTAssertEqual(layout(rows, ["t": "inProgress"], active: ["t"], expanded: true),
                       ["u", "work[a,x]", "  a", "  x", "b", "c"])
        let done = layout(rows, ["t": "completed"])
        XCTAssertEqual(done, ["u", "work[a,x,b]", "c"])
        // Expanded afterwards, the earlier texts read inside the group.
        XCTAssertEqual(layout(rows, ["t": "completed"], expanded: true), ["u", "work[a,x,b]", "  a", "  x", "  b", "c"])
        // A reading anchor on a text that folded lands on its group's header.
        let entries = ChatFeedEntry.grouping(ReadRow.settlingFinishedTurns(rows, turns: turns(["t": "completed"])))
        let nodes = ChatFeedNode.visible(entries, expanded: []).map(\.id)
        XCTAssertEqual(ChatFeedNode.survivingAnchor(for: "b", entries: entries, nodeIDs: nodes), "a")
    }

    // MARK: Mini-map

}
