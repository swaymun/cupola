import XCTest
@testable import WonderPairing

final class ConversationForkTests: XCTestCase {
    func testRequestNamesTheTurnOnlyWhenForkingFromAReply() throws {
        let from = try ForkConversationRequest(lastTurnId: "turn-2").body()
        XCTAssertEqual(try JSONSerialization.jsonObject(with: from) as? [String: String], ["lastTurnId": "turn-2"])
        let latest = try ForkConversationRequest(lastTurnId: nil).body()
        XCTAssertEqual(String(decoding: latest, as: UTF8.self), "{}")
    }

    private func row(isUser: Bool, type: String, state: String, phase: String? = nil) -> ReadRow {
        var payload: [String: ThreadValue] = [:]
        if let phase { payload["phase"] = .string(phase) }
        return ReadRow(id: "row", author: "Agent", text: "Done", isUser: isUser, timestamp: "1000", turnId: "turn-1",
                       item: ReadItem(id: "row", type: type, state: state, text: "Done", createdAt: "1000", payload: payload))
    }
}
