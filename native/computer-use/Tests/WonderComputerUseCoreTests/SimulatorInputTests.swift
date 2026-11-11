import XCTest
@testable import WonderComputerUseCore

/// Contract: the phone's simulator control actions become bounded HID steps
/// for exactly the planned gesture, and anything the simulator cannot take is
/// refused before a single step is sent.
final class SimulatorInputTests: XCTestCase {
    private let allButtons = Set(SimulatorHardwareButton.allCases)

    private func plan(_ actions: [ControlInputAction], state: inout SimulatorInputState,
                      buttons: Set<SimulatorHardwareButton>? = nil) -> Result<[SimulatorHIDStep], SimulatorInputRejection> {
        SimulatorInputPlanner.plan(actions, state: &state, buttons: buttons ?? allButtons)
    }

    private func waits(_ steps: [SimulatorHIDStep]) -> TimeInterval {
        steps.reduce(0) { if case let .wait(seconds) = $1 { return $0 + seconds } else { return $0 } }
    }

    func testTapPassesTheNormalizedFramebufferPointThroughAsTheDigitizerRatio() throws {
        var state = SimulatorInputState()
        let steps = try plan(SimulatorGesture.tap(x: 0.25, y: 0.75), state: &state).get()
        XCTAssertEqual(steps, [.touch(x: 0.25, y: 0.75, phase: .began), .wait(0.05),
                               .touch(x: 0.25, y: 0.75, phase: .ended)])
        XCTAssertNil(state.touch)

        for (x, y) in [(-0.01, 0.5), (0.5, 1.01), (.nan, 0.5), (0.5, .infinity)] {
            var untouched = SimulatorInputState()
            let result = plan([.touch(x: x, y: y, phase: "down", delayMilliseconds: nil)], state: &untouched)
            XCTAssertEqual(result, .failure(.invalidInput))
            XCTAssertEqual(untouched, SimulatorInputState())
        }
        var state2 = SimulatorInputState()
        XCTAssertEqual(plan([.touch(x: 0.5, y: 0.5, phase: "hover", delayMilliseconds: nil)], state: &state2), .failure(.invalidInput))
        XCTAssertEqual(plan([.touch(x: 0.5, y: 0.5, phase: "down", delayMilliseconds: 1_001)], state: &state2), .failure(.invalidInput))
    }

    func testSwipeInterpolatesBoundedMovesBetweenItsEnds() throws {
        var state = SimulatorInputState()
        let steps = try plan(SimulatorGesture.swipe(from: ControlPoint(x: 0.5, y: 0.8), to: ControlPoint(x: 0.5, y: 0.2)),
                             state: &state).get()
        XCTAssertEqual(steps.first, .touch(x: 0.5, y: 0.8, phase: .began))
        XCTAssertEqual(steps.last, .touch(x: 0.5, y: 0.2, phase: .ended))
        let points = steps.compactMap { step -> (Double, Double)? in
            if case let .touch(x, y, _) = step { return (x, y) } else { return nil }
        }
        let moves = steps.filter { if case .touch(_, _, .moved) = $0 { return true } else { return false } }
        XCTAssertEqual(moves.count, Int((0.6 / SimulatorInputPlanner.maximumStep).rounded(.up)))
        for (previous, next) in zip(points, points.dropFirst()) {
            XCTAssertLessThanOrEqual(hypot(next.0 - previous.0, next.1 - previous.1), SimulatorInputPlanner.maximumStep + 1e-9)
        }
        // Each move is paced at the digitizer rate, so the swipe has a velocity.
        XCTAssertEqual(waits(steps), Double(moves.count) * SimulatorInputPlanner.moveInterval, accuracy: 1e-9)

        // A streamed finger: moves without a touch down are dropped, not hovered.
        var idle = SimulatorInputState()
        XCTAssertEqual(try plan([.touch(x: 0.1, y: 0.1, phase: "move", delayMilliseconds: nil)], state: &idle).get(), [])
    }

    func testLongPressHoldsTheTouchForItsDurationAcrossOrWithinBatches() throws {
        var state = SimulatorInputState()
        let steps = try plan(SimulatorGesture.longPress(x: 0.4, y: 0.4), state: &state).get()
        XCTAssertEqual(steps, [.touch(x: 0.4, y: 0.4, phase: .began), .wait(0.8), .touch(x: 0.4, y: 0.4, phase: .ended)])

        // The phone holds the finger between batches; the touch stays down.
        let down = try plan([.touch(x: 0.4, y: 0.4, phase: "down", delayMilliseconds: nil)], state: &state).get()
        XCTAssertEqual(down, [.touch(x: 0.4, y: 0.4, phase: .began)])
        XCTAssertEqual(state.touch, ControlPoint(x: 0.4, y: 0.4))
        XCTAssertEqual(plan([.touch(x: 0.4, y: 0.4, phase: "down", delayMilliseconds: nil)], state: &state),
                       .failure(.invalidInput), "a second finger is not supported")
        let up = try plan([.touch(x: 0.4, y: 0.4, phase: "up", delayMilliseconds: nil)], state: &state).get()
        XCTAssertEqual(up, [.touch(x: 0.4, y: 0.4, phase: .ended)])

        // One batch cannot outlast the daemon's wait for its answer.
        let tooLong = SimulatorGesture.longPress(x: 0.4, y: 0.4, milliseconds: 1_000)
            + SimulatorGesture.longPress(x: 0.4, y: 0.4, milliseconds: 1_000)
        XCTAssertEqual(plan(tooLong, state: &state), .failure(.invalidInput))
        XCTAssertNil(state.touch)
    }

    func testTextTypesUSKeyboardUsagesWithShiftAndRefusesOtherCharacters() throws {
        var state = SimulatorInputState()
        let steps = try plan([.text("aZ!\n")], state: &state).get()
        let hold = SimulatorHIDStep.wait(SimulatorInputPlanner.keyHold)
        XCTAssertEqual(steps, [
            .key(usage: 0x04, down: true), hold, .key(usage: 0x04, down: false),
            .key(usage: 0xE1, down: true), .key(usage: 0x1D, down: true), hold, .key(usage: 0x1D, down: false), .key(usage: 0xE1, down: false),
            .key(usage: 0xE1, down: true), .key(usage: 0x1E, down: true), hold, .key(usage: 0x1E, down: false), .key(usage: 0xE1, down: false),
            .key(usage: 0x28, down: true), hold, .key(usage: 0x28, down: false),
        ])
        // Every printable ASCII character has a key, each on the US layout.
        let printable = (0x20...0x7E).map { Character(Unicode.Scalar($0)!) }
        XCTAssertEqual(printable.compactMap(SimulatorKeyboard.keystroke(for:)).count, 95)
        XCTAssertEqual(SimulatorKeyboard.keystroke(for: "?"), .init(usage: 0x38, shift: true))
        XCTAssertEqual(SimulatorKeyboard.keystroke(for: "0"), .init(usage: 0x27, shift: false))

        for text in ["café", "日本", "👍", "a\u{00A0}b"] {
            var untouched = SimulatorInputState()
            XCTAssertEqual(plan([.text(text)], state: &untouched), .failure(.textUnsupportedCharacters))
        }
        XCTAssertEqual(plan([.text(String(repeating: "a", count: SimulatorInputPlanner.maximumTextLength + 1))], state: &state),
                       .failure(.invalidInput))
    }

    func testKeysShareMacKeyNamesAndHoldModifiersTheyPress() throws {
        var state = SimulatorInputState()
        let paste = try plan([.key(key: "v", phase: "press", modifiers: ControlInputTranslator.commandModifier)], state: &state).get()
        XCTAssertEqual(paste, [.key(usage: 0xE3, down: true), .key(usage: 0x19, down: true),
                               .wait(SimulatorInputPlanner.keyHold), .key(usage: 0x19, down: false), .key(usage: 0xE3, down: false)])
        // A modifier the phone holds down stays down through a press.
        _ = try plan([.key(key: "command", phase: "down", modifiers: 8)], state: &state).get()
        let tab = try plan([.key(key: "tab", phase: "press", modifiers: 8)], state: &state).get()
        XCTAssertEqual(tab, [.key(usage: 0x2B, down: true), .wait(SimulatorInputPlanner.keyHold), .key(usage: 0x2B, down: false)])
        XCTAssertEqual(state.keys, [0xE3])
        XCTAssertEqual(plan([.key(key: "launchpad", phase: "press", modifiers: 0)], state: &state), .failure(.invalidInput))
    }

    func testButtonsAreBoundedToTheSupportedSetAndOtherActionsAreRefused() throws {
        var state = SimulatorInputState()
        XCTAssertEqual(try plan([.button(name: "home", phase: "press")], state: &state).get(),
                       [.button(.home, down: true), .wait(SimulatorInputPlanner.buttonHold), .button(.home, down: false)])
        XCTAssertEqual(plan([.button(name: "power", phase: "press")], state: &state), .failure(.invalidInput))
        XCTAssertEqual(plan([.button(name: "volumeUp", phase: "press")], state: &state, buttons: [.home, .lock]),
                       .failure(.buttonUnsupported))
        for action in [ControlInputAction.pointer(x: 0.5, y: 0.5, phase: "down", button: "left"),
                       .scroll(deltaX: 0, deltaY: 10), .clipboard(operation: "pasteFromPhone", text: "x")] {
            XCTAssertEqual(plan([action], state: &state), .failure(.actionUnsupported))
        }
    }

    func testReleaseLiftsTheHeldTouchKeysAndButtons() throws {
        var state = SimulatorInputState()
        _ = try plan([.touch(x: 0.2, y: 0.3, phase: "down", delayMilliseconds: nil),
                      .key(key: "shift", phase: "down", modifiers: 0),
                      .button(name: "lock", phase: "down")], state: &state).get()
        let steps = try plan([.releaseAll], state: &state).get()
        XCTAssertEqual(steps, [.touch(x: 0.2, y: 0.3, phase: .ended), .key(usage: 0xE1, down: false), .button(.lock, down: false)])
        XCTAssertEqual(state, SimulatorInputState())
    }

    func testSimulatorActionsUseTheSharedControlWireShape() throws {
        let json = #"[{"type":"touch","x":0.5,"y":0.25,"phase":"up","delayMs":800},{"type":"button","button":"lock","phase":"press"}]"#
        let actions = try JSONDecoder().decode([ControlInputAction].self, from: Data(json.utf8))
        XCTAssertEqual(actions, [.touch(x: 0.5, y: 0.25, phase: "up", delayMilliseconds: 800), .button(name: "lock", phase: "press")])
        let encoded = try JSONEncoder().encode(actions)
        XCTAssertEqual(try JSONDecoder().decode([ControlInputAction].self, from: encoded), actions)
        let tap = try JSONSerialization.jsonObject(with: JSONEncoder().encode(ControlInputAction.touch(x: 0, y: 1, phase: "down", delayMilliseconds: nil))) as? [String: Any]
        XCTAssertNil(tap?["delayMs"])
    }
}
