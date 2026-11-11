import CoreGraphics
import WonderComputerView
import WonderPairing
import XCTest

/// What is released when the keyboard or focus goes away, and the scroll remainder.
@MainActor final class ComputerInputReleaseTests: XCTestCase {
    private final class FakeChannel: ComputerInputChannel {
        private(set) var batches: [[ComputerInputAction]] = []
        func send(_ actions: [ComputerInputAction]) { batches.append(actions) }
    }

    private let point = CGPoint(x: 0.4, y: 0.3)
    private func key(_ name: String) -> ComputerInputAction { .key(key: name, phase: "up", modifiers: 0) }
    private let buttonUp = ComputerInputAction.pointer(x: 0.4, y: 0.3, phase: "up", button: "left")

    func testEveryReasonReleasesAHeldButtonItsKeysAndTheLatches() {
        for reason in ComputerInputRelease.Reason.allCases {
            var latch = ComputerModifierLatch()
            _ = latch.toggle("command")
            var press = ComputerPointerPress()
            _ = press.begin(at: point, physicalModifiers: ["option"], latch: latch)
            let channel = FakeChannel()
            XCTAssertTrue(ComputerInputRelease.release(reason, press: &press, latch: &latch, to: channel), "\(reason)")
            XCTAssertEqual(channel.batches, [[buttonUp, key("option"), key("command")]], "\(reason): button, then keys, then latches")
            XCTAssertFalse(press.isHeld, "\(reason)")
            XCTAssertTrue(latch.keys.isEmpty, "\(reason)")
            XCTAssertFalse(ComputerInputRelease.release(reason, press: &press, latch: &latch, to: channel), "\(reason): nothing is released twice")
            XCTAssertEqual(channel.batches.count, 1, reason.rawValue)
        }
    }

    func testLatchedModifiersAloneAndNothingHeld() {
        var latch = ComputerModifierLatch()
        _ = latch.toggle("shift")
        _ = latch.toggle("command")
        var press = ComputerPointerPress()
        let channel = FakeChannel()
        ComputerInputRelease.release(.keyboardHidden, press: &press, latch: &latch, to: channel)
        XCTAssertEqual(channel.batches, [[key("command"), key("shift")]], "latches come up in reverse")
        var idle = ComputerModifierLatch()
        let silent = FakeChannel()
        XCTAssertFalse(ComputerInputRelease.release(.background, press: &press, latch: &idle, to: silent))
        XCTAssertTrue(silent.batches.isEmpty, "an idle input sends nothing")
    }

    func testSmallScrollsAddUpInsteadOfBeingDropped() {
        var scroll = ComputerScrollAccumulator()
        XCTAssertNil(scroll.add(CGSize(width: 0, height: 0.4)))
        XCTAssertNil(scroll.add(CGSize(width: 0, height: 0.4)))
        XCTAssertEqual(scroll.add(CGSize(width: 0, height: 0.4)), CGSize(width: 0, height: 1))
        XCTAssertEqual(scroll.remainder.height, 0.2, accuracy: 1e-9)
        XCTAssertEqual(scroll.add(CGSize(width: -2.5, height: -3.1)), CGSize(width: -2, height: -2),
                       "whole points go out, the fraction stays with its own sign")
        XCTAssertNil(scroll.add(CGSize(width: CGFloat.nan, height: 1)), "a bad delta changes nothing")
        scroll.reset()
        XCTAssertEqual(scroll.remainder, .zero)
    }
}
