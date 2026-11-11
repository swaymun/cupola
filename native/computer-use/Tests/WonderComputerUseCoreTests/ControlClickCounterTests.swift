import XCTest
@testable import WonderComputerUseCore

final class ControlClickCounterTests: XCTestCase {
    func testRepeatedClicksCountToThreeThenRestart() {
        var counter = ControlClickCounter()
        let counts = (0..<5).map { counter.down(button: "left", x: 100, y: 100, time: Double($0) * 0.2, interval: 0.5) }
        XCTAssertEqual(counts, [1, 2, 3, 1, 2])
        XCTAssertEqual(counter.current, 2)
    }

    func testSlowMovedOrOtherButtonClicksStartOver() {
        var counter = ControlClickCounter()
        XCTAssertEqual(counter.down(button: "left", x: 10, y: 10, time: 0, interval: 0.5), 1)
        XCTAssertEqual(counter.down(button: "left", x: 10, y: 10, time: 0.9, interval: 0.5), 1)
        XCTAssertEqual(counter.down(button: "left", x: 200, y: 10, time: 1.0, interval: 0.5), 1)
        XCTAssertEqual(counter.down(button: "right", x: 200, y: 10, time: 1.1, interval: 0.5), 1)
    }
}
