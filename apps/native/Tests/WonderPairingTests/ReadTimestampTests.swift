import XCTest
@testable import WonderPairing

final class ReadTimestampTests: XCTestCase {
    func testMessageTimestampFormatsPreserveOrdering() {
        let values = ["1789153200000", "2026-09-11T19:00:00Z", "2026-09-11T15:00:00-04:00", "2026-09-11T19:00:00.123456Z"]
        let rows = values.enumerated().map { index, value in
            ReadRow(id: String(index), author: "Agent", text: "Message", isUser: false, timestamp: value)
        }
        XCTAssertEqual(rows[0].time, rows[1].time, accuracy: 0.000001)
        XCTAssertEqual(rows[1].time, rows[2].time, accuracy: 0.000001)
        XCTAssertEqual(rows[3].time - rows[1].time, 0.123456, accuracy: 0.000001)
        XCTAssertEqual(ReadRow(id: "bad", author: "Agent", text: "", isUser: false, timestamp: "unavailable").time, 0)
    }

}
