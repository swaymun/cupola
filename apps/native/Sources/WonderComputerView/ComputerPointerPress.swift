import CoreGraphics
import Foundation
import WonderPairing

/// One pointer button on the computer and the modifier keys it carries.
///
/// Latched on-screen modifiers (the ⌘ or ⇧ key the person tapped) are one-shot:
/// they are already down, stay down through the click or the whole drag, and are
/// released right after its button comes up. A scroll is not a click, so a
/// latched modifier stays down through it (⌘-scroll zooms).
///
/// Modifiers held on an iPad's hardware keyboard during a click or drag are
/// pressed just around it, unless a latch already holds the same key, so every
/// key down has exactly one key up and a latch is never released early.
public struct ComputerPointerPress: Sendable {
    public private(set) var button: String?
    public private(set) var point: CGPoint?
    /// Hardware-keyboard modifiers this press put down; released with its button.
    private var pressedKeys: [String] = []

    public init() {}

    public var isHeld: Bool { button != nil }

    /// A click: move, button down and up, with any modifiers around it. A held
    /// button is let go first.
    public mutating func click(at point: CGPoint, button: String, physicalModifiers: [String] = [],
                               latch: inout ComputerModifierLatch) -> [ComputerInputAction] {
        var actions = end(latch: &latch)
        let extra = Self.unlatched(physicalModifiers, latch: latch)
        let x = Double(point.x), y = Double(point.y)
        actions.append(.pointer(x: x, y: y, phase: "move", button: nil))
        actions += extra.map { .key(key: $0, phase: "down", modifiers: 0) }
        actions.append(.pointer(x: x, y: y, phase: "down", button: button))
        actions.append(.pointer(x: x, y: y, phase: "up", button: button))
        actions += extra.reversed().map { .key(key: $0, phase: "up", modifiers: 0) }
        return actions + latch.release()
    }

    /// Presses the button for a drag. Nothing happens while one is already held.
    public mutating func begin(at point: CGPoint, button: String = "left", physicalModifiers: [String] = [],
                               latch: ComputerModifierLatch) -> [ComputerInputAction] {
        guard self.button == nil else { return [] }
        let extra = Self.unlatched(physicalModifiers, latch: latch)
        self.button = button
        self.point = point
        pressedKeys = extra
        return extra.map { .key(key: $0, phase: "down", modifiers: 0) }
            + [.pointer(x: Double(point.x), y: Double(point.y), phase: "down", button: button)]
    }

    /// Moves the pointer, carrying the held button.
    public mutating func move(to point: CGPoint) -> ComputerInputAction {
        if button != nil { self.point = point }
        return .pointer(x: Double(point.x), y: Double(point.y), phase: "move", button: button)
    }

    /// Records where a held drag ended without sending a move.
    public mutating func relocate(to point: CGPoint) {
        if button != nil { self.point = point }
    }

    /// Lets the held button go, then the keys pressed for it, then (unless the
    /// person is changing the latches themselves) the one-shot latch.
    public mutating func end(latch: inout ComputerModifierLatch, releasingLatch: Bool = true) -> [ComputerInputAction] {
        guard let button, let point else { return [] }
        let keys = pressedKeys
        reset()
        return [.pointer(x: Double(point.x), y: Double(point.y), phase: "up", button: button)]
            + keys.reversed().map { .key(key: $0, phase: "up", modifiers: 0) }
            + (releasingLatch ? latch.release() : [])
    }

    /// Forgets the press without sending anything: control ended, and the
    /// computer releases everything it held itself.
    public mutating func reset() {
        button = nil
        point = nil
        pressedKeys = []
    }

    /// Names that reach the same key on the computer (a Windows row latches
    /// "ctrl" while an iPad keyboard's Control arrives as "control").
    public static func canonical(_ key: String) -> String {
        switch key {
        case "ctrl": "control"
        case "alt": "option"
        case "win": "super"
        default: key
        }
    }

    private static func unlatched(_ physical: [String], latch: ComputerModifierLatch) -> [String] {
        let latched = Set(latch.keys.map(canonical))
        var seen = Set<String>()
        return physical.filter { key in
            let name = canonical(key)
            return !latched.contains(name) && seen.insert(name).inserted
        }
    }
}
