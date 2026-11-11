import Foundation
import WonderSimulatorCapture

/// Simulator hardware buttons the phone can press. Raw values are the wire
/// names of `ControlInputAction.button`.
public enum SimulatorHardwareButton: String, CaseIterable, Sendable {
    case home, lock, volumeUp, volumeDown
}

public enum SimulatorTouchPhase: Int, Equatable, Sendable {
    case began = 0, moved = 1, ended = 2
}

/// One primitive the native injector sends, or a pause between them.
public enum SimulatorHIDStep: Equatable, Sendable {
    case touch(x: Double, y: Double, phase: SimulatorTouchPhase)
    case button(SimulatorHardwareButton, down: Bool)
    case key(usage: UInt32, down: Bool)
    case wait(TimeInterval)
}

/// Input held down between batches, so release always lifts it.
public struct SimulatorInputState: Equatable, Sendable {
    public var touch: ControlPoint?
    public var keys: Set<UInt32> = []
    public var buttons: Set<SimulatorHardwareButton> = []
    public init() {}
}

/// Codes the daemon turns into user language. None carries input content.
public enum SimulatorInputRejection: String, Error, Sendable {
    case invalidInput = "invalid_input"
    case textUnsupportedCharacters = "text_unsupported_characters"
    case buttonUnsupported = "simulator_button_unsupported"
    case actionUnsupported = "simulator_action_unsupported"
}

/// Pure translation from the phone's control actions to simulator HID steps.
/// Points are normalized 0...1 in the captured framebuffer, which is the
/// simulator's native (unrotated) display, so they are passed through as the
/// digitizer's ratio unchanged.
public enum SimulatorInputPlanner {
    /// Largest normalized jump between two delivered touch positions. Longer
    /// moves are interpolated so a swipe reads as a drag, not a teleport.
    public static let maximumStep = 0.025
    /// Spacing of interpolated moves (120 Hz, a ProMotion digitizer rate).
    public static let moveInterval = 1.0 / 120
    public static let buttonHold = 0.1
    public static let keyHold = 0.012
    public static let maximumDelayMilliseconds: UInt32 = 1_000
    /// The daemon waits 2 s for a batch; leave room for delivery itself.
    public static let maximumBatchDuration = 1.5
    public static let maximumTextLength = 512

    public static func plan(
        _ actions: [ControlInputAction],
        state: inout SimulatorInputState,
        buttons supported: Set<SimulatorHardwareButton>
    ) -> Result<[SimulatorHIDStep], SimulatorInputRejection> {
        var next = state
        var steps: [SimulatorHIDStep] = []

        func interpolate(to point: ControlPoint, firstWait: TimeInterval) {
            guard let from = next.touch else { return }
            let distance = hypot(point.x - from.x, point.y - from.y)
            // The tolerance keeps float noise from adding a step (0.8 - 0.2 > 0.6).
            let count = max(1, Int((distance / maximumStep - 1e-9).rounded(.up)))
            for index in 1...count {
                let t = Double(index) / Double(count)
                steps.append(.wait(index == 1 ? firstWait : moveInterval))
                steps.append(.touch(x: from.x + (point.x - from.x) * t, y: from.y + (point.y - from.y) * t, phase: .moved))
            }
            next.touch = point
        }

        func press(_ modifiers: [UInt32], _ key: UInt32) {
            // Modifiers the phone already holds stay held.
            let added = modifiers.filter { !next.keys.contains($0) }
            for usage in added { steps.append(.key(usage: usage, down: true)) }
            steps += [.key(usage: key, down: true), .wait(keyHold), .key(usage: key, down: false)]
            for usage in added.reversed() { steps.append(.key(usage: usage, down: false)) }
        }

        for action in actions {
            switch action {
            case let .touch(x, y, phase, delayMilliseconds):
                guard x.isFinite, y.isFinite, (0...1).contains(x), (0...1).contains(y),
                      (delayMilliseconds ?? 0) <= maximumDelayMilliseconds else { return .failure(.invalidInput) }
                let delay = Double(delayMilliseconds ?? 0) / 1_000
                let point = ControlPoint(x: x, y: y)
                switch phase {
                case "down":
                    guard next.touch == nil else { return .failure(.invalidInput) }
                    if delay > 0 { steps.append(.wait(delay)) }
                    steps.append(.touch(x: x, y: y, phase: .began))
                    next.touch = point
                case "move":
                    // A touchscreen has no hover: a move without a finger down is dropped.
                    guard next.touch != nil else { continue }
                    interpolate(to: point, firstWait: max(delay, moveInterval))
                case "up":
                    guard let held = next.touch else { continue }
                    if held != point {
                        interpolate(to: point, firstWait: max(delay, moveInterval))
                        steps.append(.wait(moveInterval))
                    } else if delay > 0 {
                        steps.append(.wait(delay))
                    }
                    steps.append(.touch(x: x, y: y, phase: .ended))
                    next.touch = nil
                default:
                    return .failure(.invalidInput)
                }
            case let .button(name, phase):
                guard let button = SimulatorHardwareButton(rawValue: name) else { return .failure(.invalidInput) }
                guard supported.contains(button) else { return .failure(.buttonUnsupported) }
                switch phase {
                case "press":
                    steps += [.button(button, down: true), .wait(buttonHold), .button(button, down: false)]
                    next.buttons.remove(button)
                case "down":
                    steps.append(.button(button, down: true)); next.buttons.insert(button)
                case "up":
                    steps.append(.button(button, down: false)); next.buttons.remove(button)
                default:
                    return .failure(.invalidInput)
                }
            case let .key(key, phase, modifiers):
                guard let usage = SimulatorKeyboard.usage(forKey: key),
                      modifiers & ~ControlInputTranslator.allowedModifierMask == 0 else { return .failure(.invalidInput) }
                switch phase {
                case "press":
                    press(SimulatorKeyboard.modifierUsages(modifiers), usage)
                case "down":
                    if !next.keys.contains(usage) { steps.append(.key(usage: usage, down: true)) }
                    next.keys.insert(usage)
                case "up":
                    // Always sent, so a key the guest still believes is down is lifted.
                    steps.append(.key(usage: usage, down: false)); next.keys.remove(usage)
                default:
                    return .failure(.invalidInput)
                }
            case let .text(text):
                guard !text.isEmpty, text.unicodeScalars.count <= maximumTextLength else { return .failure(.invalidInput) }
                guard let keys = SimulatorKeyboard.keystrokes(for: text) else { return .failure(.textUnsupportedCharacters) }
                for stroke in keys {
                    press(stroke.shift ? [SimulatorKeyboard.leftShift] : [], stroke.usage)
                }
            case .releaseAll:
                steps += release(&next)
            case .pointer, .scroll, .clipboard:
                return .failure(.actionUnsupported)
            }
        }
        let duration = steps.reduce(0.0) { total, step in
            if case let .wait(seconds) = step { return total + seconds }
            return total
        }
        guard duration <= maximumBatchDuration else { return .failure(.invalidInput) }
        state = next
        return .success(steps)
    }

    /// Lifts everything held: the touch, keys and buttons.
    public static func release(_ state: inout SimulatorInputState) -> [SimulatorHIDStep] {
        var steps: [SimulatorHIDStep] = []
        if let touch = state.touch { steps.append(.touch(x: touch.x, y: touch.y, phase: .ended)) }
        steps += state.keys.sorted().map { .key(usage: $0, down: false) }
        steps += state.buttons.sorted { $0.rawValue < $1.rawValue }.map { .button($0, down: false) }
        state = SimulatorInputState()
        return steps
    }
}

/// Gestures as control actions, for callers that describe a whole gesture
/// rather than streaming a finger (agents, tests, the measurement probe).
public enum SimulatorGesture {
    public static let tapHoldMilliseconds: UInt32 = 50
    public static let longPressMilliseconds: UInt32 = 800

    public static func tap(x: Double, y: Double) -> [ControlInputAction] {
        [.touch(x: x, y: y, phase: "down", delayMilliseconds: nil),
         .touch(x: x, y: y, phase: "up", delayMilliseconds: tapHoldMilliseconds)]
    }

    public static func longPress(x: Double, y: Double, milliseconds: UInt32 = longPressMilliseconds) -> [ControlInputAction] {
        [.touch(x: x, y: y, phase: "down", delayMilliseconds: nil),
         .touch(x: x, y: y, phase: "up", delayMilliseconds: milliseconds)]
    }

    /// The planner interpolates between the end points at its move rate.
    public static func swipe(from: ControlPoint, to: ControlPoint) -> [ControlInputAction] {
        [.touch(x: from.x, y: from.y, phase: "down", delayMilliseconds: nil),
         .touch(x: to.x, y: to.y, phase: "move", delayMilliseconds: nil),
         .touch(x: to.x, y: to.y, phase: "up", delayMilliseconds: nil)]
    }
}

/// US-layout USB HID keyboard usages (page 0x07). Typing is limited to what a
/// US keyboard produces; other text is rejected so the phone can offer paste.
public enum SimulatorKeyboard {
    public static let leftShift: UInt32 = 0xE1
    public struct Keystroke: Equatable, Sendable {
        public let usage: UInt32
        public let shift: Bool
    }

    private static let unshifted: [Character: UInt32] = {
        var table: [Character: UInt32] = [:]
        for (offset, letter) in "abcdefghijklmnopqrstuvwxyz".enumerated() { table[letter] = 0x04 + UInt32(offset) }
        for (offset, digit) in "1234567890".enumerated() { table[digit] = 0x1E + UInt32(offset) }
        let symbols: [(Character, UInt32)] = [
            ("\n", 0x28), ("\t", 0x2B), (" ", 0x2C), ("-", 0x2D), ("=", 0x2E), ("[", 0x2F), ("]", 0x30),
            ("\\", 0x31), (";", 0x33), ("'", 0x34), ("`", 0x35), (",", 0x36), (".", 0x37), ("/", 0x38),
        ]
        for (character, usage) in symbols { table[character] = usage }
        return table
    }()

    private static let shifted: [Character: UInt32] = {
        var table: [Character: UInt32] = [:]
        for (offset, letter) in "ABCDEFGHIJKLMNOPQRSTUVWXYZ".enumerated() { table[letter] = 0x04 + UInt32(offset) }
        for (offset, symbol) in "!@#$%^&*()".enumerated() { table[symbol] = 0x1E + UInt32(offset) }
        let symbols: [(Character, UInt32)] = [
            ("_", 0x2D), ("+", 0x2E), ("{", 0x2F), ("}", 0x30), ("|", 0x31), (":", 0x33), ("\"", 0x34),
            ("~", 0x35), ("<", 0x36), (">", 0x37), ("?", 0x38),
        ]
        for (character, usage) in symbols { table[character] = usage }
        return table
    }()

    public static func keystroke(for character: Character) -> Keystroke? {
        if let usage = unshifted[character] { return Keystroke(usage: usage, shift: false) }
        if let usage = shifted[character] { return Keystroke(usage: usage, shift: true) }
        return nil
    }

    /// Nil when any character has no US-layout key. "\r\n" is one Character
    /// in Swift and types as Return.
    public static func keystrokes(for text: String) -> [Keystroke]? {
        var strokes: [Keystroke] = []
        for character in text {
            let normalized: Character = character == "\r\n" || character == "\r" ? "\n" : character
            guard let stroke = keystroke(for: normalized) else { return nil }
            strokes.append(stroke)
        }
        return strokes
    }

    private static let named: [String: UInt32] = [
        "return": 0x28, "enter": 0x28, "escape": 0x29, "esc": 0x29, "delete": 0x2A, "backspace": 0x2A,
        "tab": 0x2B, "space": 0x2C, "capslock": 0x39, "home": 0x4A, "pageup": 0x4B, "forwarddelete": 0x4C,
        "end": 0x4D, "pagedown": 0x4E, "right": 0x4F, "left": 0x50, "down": 0x51, "up": 0x52,
        "control": 0xE0, "shift": 0xE1, "option": 0xE2, "command": 0xE3,
        "rightcontrol": 0xE4, "rightshift": 0xE5, "rightoption": 0xE6, "rightcommand": 0xE7,
        "f1": 0x3A, "f2": 0x3B, "f3": 0x3C, "f4": 0x3D, "f5": 0x3E, "f6": 0x3F,
        "f7": 0x40, "f8": 0x41, "f9": 0x42, "f10": 0x43, "f11": 0x44, "f12": 0x45,
    ]

    /// The same key names as Mac control (`ControlInputTranslator.keyCode`).
    public static func usage(forKey key: String) -> UInt32? {
        let normalized = key.lowercased()
        if normalized.count == 1, let character = normalized.first, character != "\n", character != "\t" {
            return unshifted[character]
        }
        return named[normalized]
    }

    /// Mac control's modifier bits, in press order.
    public static func modifierUsages(_ modifiers: UInt32) -> [UInt32] {
        [(ControlInputTranslator.controlModifier, UInt32(0xE0)), (ControlInputTranslator.optionModifier, 0xE2),
         (ControlInputTranslator.shiftModifier, 0xE1), (ControlInputTranslator.commandModifier, 0xE3),
         (ControlInputTranslator.capsLockModifier, 0x39)]
            .filter { modifiers & $0.0 != 0 }.map(\.1)
    }
}

/// Owns the native injector for one simulator and delivers planned steps on
/// its own serial queue; waits happen there, never on the main thread.
/// Input only ever goes to the exact UDID it was prepared for, and each
/// delivery re-checks that this device is still booted.
public final class SimulatorInputDriver: @unchecked Sendable {
    public struct Capability: Equatable, Sendable {
        public let sourceID: String
        public let available: Bool
        public let reason: String?
        public let transport: String?
        public let buttons: [SimulatorHardwareButton]

        public var json: [String: Any] {
            [
                "sourceID": sourceID, "available": available, "provider": "simulator-hid-v1",
                "requiresAccessibility": false, "reason": reason ?? NSNull(),
                "buttons": buttons.map(\.rawValue), "text": "us-ascii",
            ]
        }
    }

    /// dtuhid: all four measured on Xcode 27.1 (volume moves the guest's own
    /// level). The legacy Indigo path is offered only home and lock; it could
    /// not be measured here because CoreSimulator 1174 drops legacy input.
    public static func supportedButtons(transport: String) -> [SimulatorHardwareButton] {
        transport == "dtuhid" ? SimulatorHardwareButton.allCases : [.home, .lock]
    }

    private let queue = DispatchQueue(label: "com.wonder.simulator-input", qos: .userInitiated)
    private let developerDirectory: () -> String?
    // Queue-confined.
    private var native: WonderSimulatorInput?
    private var capability: Capability?
    private var state = SimulatorInputState()
    // Read from the main thread without waiting behind a delivery.
    private let readyLock = NSLock()
    private var readySourceID: String?

    public convenience init() {
        self.init(developerDirectory: { SimulatorCapture.developerDirectory() })
    }

    init(developerDirectory: @escaping () -> String?) {
        self.developerDirectory = developerDirectory
    }

    /// Connects to `sourceID` (`simulator:<UDID>`) and reports whether it can
    /// take input. Blocks for up to a few seconds: never call on the main thread.
    /// `transport` and `allButtons` are for the Debug measurement probe only.
    public func prepare(sourceID: String, transport: String? = nil, allButtons: Bool = false) -> Capability {
        queue.sync { prepareOnQueue(sourceID: sourceID, transport: transport, allButtons: allButtons) }
    }

    private func prepareOnQueue(sourceID: String, transport: String?, allButtons: Bool) -> Capability {
        if let capability, capability.sourceID == sourceID, capability.available, native?.isUsable() == true {
            return capability
        }
        native?.close(); native = nil; state = SimulatorInputState()
        readyLock.withLock { readySourceID = nil }
        guard let udid = Self.udid(sourceID) else {
            return remember(Capability(sourceID: sourceID, available: false, reason: "invalid_input", transport: nil, buttons: []))
        }
        guard let developer = developerDirectory() else {
            return remember(Capability(sourceID: sourceID, available: false, reason: "xcode_unavailable", transport: nil, buttons: []))
        }
        var reason: NSString?
        guard let input = WonderSimulatorInput.connect(udid, developer: developer, transport: transport, reason: &reason) else {
            return remember(Capability(sourceID: sourceID, available: false,
                                       reason: (reason as String?) ?? "simulator_input_unavailable", transport: nil, buttons: []))
        }
        native = input
        return remember(Capability(sourceID: sourceID, available: true, reason: nil, transport: input.transport,
                                   buttons: allButtons ? SimulatorHardwareButton.allCases : Self.supportedButtons(transport: input.transport)))
    }

    private func remember(_ value: Capability) -> Capability {
        capability = value
        readyLock.withLock { readySourceID = value.available ? value.sourceID : nil }
        return value
    }

    /// Whether `prepare` connected this exact source. Never blocks.
    public func isReady(for sourceID: String?) -> Bool {
        readyLock.withLock { sourceID != nil && readySourceID == sourceID }
    }

    /// Plans first, so a rejected batch sends nothing, then delivers in order.
    /// Returns nil once delivered, or a failure code.
    public func deliver(sourceID: String, actions: [ControlInputAction]) async -> String? {
        await withCheckedContinuation { continuation in
            queue.async { [self] in continuation.resume(returning: deliverOnQueue(sourceID: sourceID, actions: actions)) }
        }
    }

    private func deliverOnQueue(sourceID: String, actions: [ControlInputAction]) -> String? {
        guard let capability, capability.available, capability.sourceID == sourceID,
              let native, native.udid == Self.udid(sourceID), native.isUsable() else { return "simulator_not_booted" }
        var planned = state
        let steps: [SimulatorHIDStep]
        switch SimulatorInputPlanner.plan(actions, state: &planned, buttons: Set(capability.buttons)) {
        case let .success(value): steps = value
        case let .failure(rejection): return rejection.rawValue
        }
        guard send(steps, to: native) else {
            // Whatever was held may now be stuck in the guest: lift it.
            _ = send(SimulatorInputPlanner.release(&planned), to: native)
            state = SimulatorInputState()
            return "input_delivery_failed"
        }
        state = planned
        return nil
    }

    private func send(_ steps: [SimulatorHIDStep], to native: WonderSimulatorInput) -> Bool {
        for step in steps {
            let sent: Bool
            switch step {
            case let .wait(seconds):
                Thread.sleep(forTimeInterval: seconds); continue
            case let .touch(x, y, phase):
                sent = native.touchX(x, y: y, phase: WonderSimulatorTouchPhase(rawValue: phase.rawValue) ?? .ended)
            case let .button(button, down):
                sent = native.button(Self.nativeButton(button), down: down)
            case let .key(usage, down):
                sent = native.keyboardUsage(usage, down: down)
            }
            if !sent { return false }
        }
        return true
    }

    /// Lifts held input (lease end, release, stop). Asynchronous and ordered
    /// after any batch already queued.
    public func releaseHeld() {
        queue.async { [self] in
            guard let native else { state = SimulatorInputState(); return }
            _ = send(SimulatorInputPlanner.release(&state), to: native)
        }
    }

    public func close() {
        queue.async { [self] in
            if let native { _ = send(SimulatorInputPlanner.release(&state), to: native) }
            native?.close(); native = nil; capability = nil; state = SimulatorInputState()
            readyLock.withLock { readySourceID = nil }
        }
    }

    static func udid(_ sourceID: String) -> String? {
        guard sourceID.hasPrefix("simulator:"),
              let uuid = UUID(uuidString: String(sourceID.dropFirst("simulator:".count))) else { return nil }
        return uuid.uuidString
    }

    private static func nativeButton(_ button: SimulatorHardwareButton) -> WonderSimulatorButton {
        switch button {
        case .home: .home
        case .lock: .lock
        case .volumeUp: .volumeUp
        case .volumeDown: .volumeDown
        }
    }
}
