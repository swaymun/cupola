import XCTest
import CoreGraphics
@testable import WonderPairing

final class ReadAcknowledgementTests: XCTestCase {
    private func snapshot(sequence: UInt64 = 4, epoch: String = "epoch") throws -> ConversationSnapshot {
        try JSONDecoder().decode(ConversationSnapshot.self, from: Data("{\"conversationId\":\"chat\",\"hostEpoch\":\"\(epoch)\",\"lastSequence\":\(sequence),\"messages\":[],\"assistantMessages\":[],\"thread\":{\"hydrated\":true}}".utf8))
    }
    func testOffscreenAndPartlySeenLatestMessageDoNotCountAsRead() {
        let viewport = CGRect(x: 0, y: 100, width: 400, height: 500)
        XCTAssertFalse(ReadVisibility.latestEndIsVisible(frame: CGRect(x: 0, y: 650, width: 400, height: 200), viewport: viewport))
        XCTAssertFalse(ReadVisibility.latestEndIsVisible(frame: CGRect(x: 0, y: 200, width: 400, height: 700), viewport: viewport))
        XCTAssertFalse(ReadVisibility.latestEndIsVisible(frame: CGRect(x: 450, y: 100, width: 300, height: 200), viewport: viewport))
        XCTAssertTrue(ReadVisibility.latestEndIsVisible(frame: CGRect(x: 0, y: -200, width: 400, height: 750), viewport: viewport))
        XCTAssertFalse(ReadVisibility.latestEndIsVisible(frame: .zero, viewport: viewport))
    }
    func testRequestUsesDisplayedSnapshotNotGlobalCursor() throws {
        let visible = VisibleReadReceipt(snapshot: try snapshot())
        let request = try XCTUnwrap(JSONSerialization.jsonObject(with: visible.requestBody()) as? [String:Any])
        XCTAssertEqual(Set(request.keys), ["markRead", "hostEpoch", "readThroughSequence"])
        XCTAssertEqual(request["readThroughSequence"] as? Int, 4)
    }
}
