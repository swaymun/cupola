import CoreGraphics
import WonderComputerView
import WonderPairing
import XCTest

/// Modifier lifecycle around pointer buttons: every key down has one key up,
/// in order, and a latched modifier lasts exactly one click or one drag.
final class ComputerPointerPressTests: XCTestCase {
    private let point = CGPoint(x: 40, y: 30)
    private func pointer(_ phase: String, _ button: String?) -> ComputerInputAction {
        .pointer(x: 40, y: 30, phase: phase, button: button)
    }
    private func key(_ name: String, _ phase: String) -> ComputerInputAction {
        .key(key: name, phase: phase, modifiers: 0)
    }

    func testClickOrderingsTable() {
        let rows: [(String, [String], [String], String, [ComputerInputAction])] = [
            ("a plain click", [], [], "left",
             [pointer("move", nil), pointer("down", "left"), pointer("up", "left")]),
            ("a latched ⌘ is down already and released after the click", ["command"], [], "left",
             [pointer("move", nil), pointer("down", "left"), pointer("up", "left"), key("command", "up")]),
            ("hardware ⌘⇧ go down around a right-click, released in reverse", [], ["command", "shift"], "right",
             [pointer("move", nil), key("command", "down"), key("shift", "down"),
              pointer("down", "right"), pointer("up", "right"), key("shift", "up"), key("command", "up")]),
            ("a key both latched and held is pressed once and released once", ["shift"], ["shift", "command"], "left",
             [pointer("move", nil), key("command", "down"), pointer("down", "left"), pointer("up", "left"),
              key("command", "up"), key("shift", "up")]),
            ("a Windows row's ctrl latch covers the iPad's Control", ["ctrl"], ["control"], "left",
             [pointer("move", nil), pointer("down", "left"), pointer("up", "left"), key("ctrl", "up")]),
        ]
        for (name, latched, physical, button, expected) in rows {
            var latch = ComputerModifierLatch()
            for key in latched { _ = latch.toggle(key) }
            var press = ComputerPointerPress()
            XCTAssertEqual(press.click(at: point, button: button, physicalModifiers: physical, latch: &latch), expected, name)
            XCTAssertTrue(latch.keys.isEmpty, "\(name): the latch lasts one click")
            XCTAssertFalse(press.isHeld, name)
        }
    }

    func testALatchStaysDownThroughTheWholeDragAndIsReleasedAfterIt() {
        var latch = ComputerModifierLatch()
        _ = latch.toggle("shift")
        var press = ComputerPointerPress()
        XCTAssertEqual(press.begin(at: point, physicalModifiers: ["option"], latch: latch),
                       [key("option", "down"), pointer("down", "left")])
        XCTAssertEqual(press.begin(at: point, latch: latch), [], "one drag at a time")
        XCTAssertEqual(press.move(to: CGPoint(x: 60, y: 30)), .pointer(x: 60, y: 30, phase: "move", button: "left"))
        XCTAssertEqual(latch.keys, ["shift"], "moving keeps the latch down")
        XCTAssertEqual(press.end(latch: &latch), [
            .pointer(x: 60, y: 30, phase: "up", button: "left"), key("option", "up"), key("shift", "up"),
        ])
        XCTAssertTrue(latch.keys.isEmpty)
        XCTAssertEqual(press.end(latch: &latch), [], "nothing is released twice")
        XCTAssertEqual(press.move(to: point), pointer("move", nil), "after the drag the pointer moves without a button")
    }

}

final class ComputerSignalingCadenceTests: XCTestCase {
}
