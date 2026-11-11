import CoreGraphics
import IOKit.hidsystem

/// Builds native key chords without borrowing the local keyboard's state.
/// Explicit modifier-down actions also apply to later pointer/scroll events.
public struct ControlKeyboardState: Sendable {
    public private(set) var heldKeyCodes: Set<UInt16> = []

    public init() {}

    private static let modifiers: [(code: UInt16, bit: UInt32, flag: CGEventFlags, device: UInt64)] = [
        (56, 1, .maskShift, UInt64(NX_DEVICELSHIFTKEYMASK)),
        (60, 1, .maskShift, UInt64(NX_DEVICERSHIFTKEYMASK)),
        (59, 2, .maskControl, UInt64(NX_DEVICELCTLKEYMASK)),
        (62, 2, .maskControl, UInt64(NX_DEVICERCTLKEYMASK)),
        (58, 4, .maskAlternate, UInt64(NX_DEVICELALTKEYMASK)),
        (61, 4, .maskAlternate, UInt64(NX_DEVICERALTKEYMASK)),
        (55, 8, .maskCommand, UInt64(NX_DEVICELCMDKEYMASK)),
        (54, 8, .maskCommand, UInt64(NX_DEVICERCMDKEYMASK)),
    ]

    public var modifierFlags: CGEventFlags { Self.flags(for: heldKeyCodes) }

    private static func flags(for keyCodes: Set<UInt16>) -> CGEventFlags {
        modifiers.reduce(into: CGEventFlags()) { flags, modifier in
            if keyCodes.contains(modifier.code) {
                flags.formUnion(modifier.flag)
                flags.formUnion(CGEventFlags(rawValue: modifier.device))
            }
        }
    }

    private static func event(code: UInt16, down: Bool, flags: CGEventFlags) -> CGEvent? {
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down) else { return nil }
        // Arrow keys carry native numeric-pad/Fn flags. Replacing all flags
        // strips those semantics; retaining all flags borrows physical modifiers.
        event.flags = event.flags.intersection([.maskNumericPad, .maskSecondaryFn, .maskNonCoalesced]).union(flags)
        return event
    }

    public mutating func events(keyCode: UInt16, phase: String, modifiers: UInt32) -> [CGEvent]? {
        var next = self
        var events: [CGEvent] = []
        let isModifier = Self.modifiers.contains { $0.code == keyCode }
        var temporary: [UInt16] = []
        // A press with modifiers is a complete chord, including flagsChanged
        // transitions. Flags on the ordinary key alone do not hold a modifier.
        if phase == "press" && !isModifier {
            for modifier in Self.modifiers where [56, 59, 58, 55].contains(modifier.code) {
                guard modifiers & modifier.bit != 0,
                      !next.modifierFlags.contains(modifier.flag) else { continue }
                next.heldKeyCodes.insert(modifier.code)
                guard let event = Self.event(code: modifier.code, down: true, flags: next.modifierFlags) else { return nil }
                events.append(event)
                temporary.append(modifier.code)
            }
        }
        var requestedFlags: CGEventFlags = []
        for modifier in Self.modifiers where modifiers & modifier.bit != 0 { requestedFlags.insert(modifier.flag) }
        if modifiers & ControlInputTranslator.capsLockModifier != 0 { requestedFlags.insert(.maskAlphaShift) }
        if phase == "down" || phase == "press" {
            next.heldKeyCodes.insert(keyCode)
            guard let event = Self.event(code: keyCode, down: true, flags: next.modifierFlags.union(requestedFlags)) else { return nil }
            events.append(event)
        }
        if phase == "up" || phase == "press" {
            next.heldKeyCodes.remove(keyCode)
            if isModifier, let modifier = Self.modifiers.first(where: { $0.code == keyCode }) {
                requestedFlags.remove(modifier.flag)
            }
            guard let event = Self.event(code: keyCode, down: false, flags: next.modifierFlags.union(requestedFlags)) else { return nil }
            events.append(event)
        }
        for code in temporary.reversed() {
            next.heldKeyCodes.remove(code)
            guard let event = Self.event(code: code, down: false, flags: next.modifierFlags) else { return nil }
            events.append(event)
        }
        self = next
        return events
    }

    public mutating func releaseEvents() -> [CGEvent] {
        // Release ordinary keys before modifiers so no key remains held when
        // Command-up commits a switcher selection.
        let modifierCodes = Set(Self.modifiers.map(\.code))
        let codes = heldKeyCodes.subtracting(modifierCodes).sorted() + heldKeyCodes.intersection(modifierCodes).sorted()
        let releases = codes.flatMap { events(keyCode: $0, phase: "up", modifiers: 0) ?? [] }
        heldKeyCodes.removeAll()
        return releases
    }
}
