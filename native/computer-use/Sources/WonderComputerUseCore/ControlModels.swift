import Foundation

/// Whole-screen and selected-window input share the Accessibility and owner
/// gates; input waits for the exact window's focus before it is posted.
/// Simulator views use their own HID path (`SimulatorInputDriver`), never
/// Core Graphics events.
public enum MacControlSourcePolicy {
    public static func allows(_ kind: CaptureSourceKind?) -> Bool {
        kind != .simulator
    }

    /// Switching views under a lease: nil is the whole screen.
    public static func allowsRebind(to sourceID: String?) -> Bool {
        sourceID.map { !$0.hasPrefix("simulator:") } ?? true
    }

    /// `controlAllowed` is the owner's saved choice (`cupola computer control`);
    /// when it is off the phone shows view only rather than offering control
    /// the helper would refuse.
    public static func capability(accessibilityGranted: Bool, controlAllowed: Bool) -> [String: Any] {
        let reason: Any
        if !controlAllowed {
            reason = "control_disabled"
        } else if !accessibilityGranted {
            reason = "accessibility_permission_required"
        } else {
            reason = NSNull()
        }
        return [
            "available": accessibilityGranted && controlAllowed,
            "provider": "core-graphics-v1",
            "requiresAccessibility": true,
            "windowControlAvailable": accessibilityGranted && controlAllowed,
            "reason": reason,
        ]
    }
}

public struct ControlLeaseIdentity: Equatable, Sendable {
    public let leaseID: String
    public let requestID: String
    public let sessionID: String
    public let generation: UInt64
    public let geometryRevision: UInt64
    public let sourceID: String?

    public init(
        leaseID: String,
        requestID: String,
        sessionID: String,
        generation: UInt64,
        geometryRevision: UInt64,
        sourceID: String?
    ) {
        self.leaseID = leaseID
        self.requestID = requestID
        self.sessionID = sessionID
        self.generation = generation
        self.geometryRevision = geometryRevision
        self.sourceID = sourceID
    }
}

public enum ControlConsentDecision: String, Equatable, Sendable {
    case allowOnce
    case deny
}

public enum ControlConsentResult: Equatable, Sendable {
    case allowOnce(replayed: Bool)
    case denied(replayed: Bool)
    case identityConflict
}

public enum ControlActivationResult: Equatable, Sendable {
    case activated(replayed: Bool)
    case consentRequired
    case denied
    case busy
    case identityConflict
}

public enum ControlDeliveryResult: Equatable, Sendable {
    case delivered
    case duplicate
    case rejected(String)
}

/// The helper-side lease boundary is deliberately independent of AppKit. The
/// executable injects the local authorization policy and native delivery
/// closure; tests can exercise both without starting the helper UI.
public final class ControlLeaseGate: @unchecked Sendable {
    public typealias ConsentProvider = (ControlLeaseIdentity) -> ControlConsentDecision
    public typealias EnabledProvider = () -> Bool

    private struct ConsentRecord {
        let identity: ControlLeaseIdentity
        let decision: ControlConsentDecision
    }

    private struct ActiveLease {
        var identity: ControlLeaseIdentity
        var expiresAt: Date
        var lastSequence: UInt64
        var payloads: [UInt64: Data]
    }

    private let lock = NSLock()
    private let consentProvider: ConsentProvider
    private let enabledProvider: EnabledProvider
    private var consents: [String: ConsentRecord] = [:]
    private var active: ActiveLease?
    private let maximumRememberedPayloads = 64

    public init(
        consentProvider: @escaping ConsentProvider,
        enabledProvider: @escaping EnabledProvider = { true }
    ) {
        self.consentProvider = consentProvider
        self.enabledProvider = enabledProvider
    }

    public func requestConsent(_ identity: ControlLeaseIdentity) -> ControlConsentResult {
        guard enabledProvider() else {
            lock.lock()
            active = nil
            lock.unlock()
            return .denied(replayed: false)
        }
        lock.lock()
        if let record = consents[identity.requestID] {
            lock.unlock()
            guard record.identity == identity else { return .identityConflict }
            return record.decision == .allowOnce ? .allowOnce(replayed: true) : .denied(replayed: true)
        }
        lock.unlock()

        let decision = consentProvider(identity)
        lock.lock()
        defer { lock.unlock() }
        guard enabledProvider() else {
            active = nil
            return .denied(replayed: false)
        }
        // A request can only be recorded once. This also keeps a late replay
        // from replacing a decision made by an earlier request.
        if let record = consents[identity.requestID] {
            guard record.identity == identity else { return .identityConflict }
            return record.decision == .allowOnce ? .allowOnce(replayed: true) : .denied(replayed: true)
        }
        consents[identity.requestID] = ConsentRecord(identity: identity, decision: decision)
        return decision == .allowOnce ? .allowOnce(replayed: false) : .denied(replayed: false)
    }

    public func activate(_ identity: ControlLeaseIdentity, now: Date, lifetime: TimeInterval) -> ControlActivationResult {
        lock.lock()
        defer { lock.unlock() }
        guard enabledProvider() else {
            active = nil
            return .denied
        }
        if let active {
            if active.expiresAt <= now {
                self.active = nil
            } else if active.identity == identity {
                return .activated(replayed: true)
            } else {
                return .busy
            }
        }
        guard let consent = consents[identity.requestID] else { return .consentRequired }
        guard consent.identity == identity else { return .identityConflict }
        guard consent.decision == .allowOnce else { return .denied }
        active = ActiveLease(
            identity: identity,
            expiresAt: now.addingTimeInterval(lifetime),
            lastSequence: 0,
            payloads: [:]
        )
        return .activated(replayed: false)
    }

    public func heartbeat(_ identity: ControlLeaseIdentity, now: Date, lifetime: TimeInterval) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard enabledProvider() else {
            active = nil
            return false
        }
        guard var active else { return false }
        guard active.expiresAt > now else {
            self.active = nil
            return false
        }
        guard active.identity == identity else { return false }
        active.expiresAt = now.addingTimeInterval(lifetime)
        self.active = active
        return true
    }

    /// Consent and replay receipts belong to the session's lease, not its view.
    /// Native input is drained by the executable before calling this boundary.
    public func rebind(_ identity: ControlLeaseIdentity, to selected: ControlLeaseIdentity, now: Date = Date(), lifetime: TimeInterval = 10) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard enabledProvider(), var active, active.identity == identity, active.expiresAt > now,
              identity.leaseID == selected.leaseID, identity.requestID == selected.requestID,
              identity.sessionID == selected.sessionID, identity.generation == selected.generation,
              selected.geometryRevision > identity.geometryRevision else { return false }
        active.identity = selected
        active.expiresAt = now.addingTimeInterval(lifetime)
        self.active = active
        consents[identity.requestID] = ConsentRecord(identity: selected, decision: .allowOnce)
        return true
    }

    /// Focus itself is a native side effect. Check the lease and sequence before
    /// preparing it, then let deliver recheck after asynchronous activation.
    public func requiresNativeDelivery(
        _ identity: ControlLeaseIdentity, sequence: UInt64, payload: Data, now: Date = Date()
    ) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return deliveryReceipt(identity, sequence: sequence, payload: payload, now: now) == nil
    }

    // Caller holds lock. This read-only check never advances a sequence.
    private func deliveryReceipt(
        _ identity: ControlLeaseIdentity, sequence: UInt64, payload: Data, now: Date
    ) -> ControlDeliveryResult? {
        guard enabledProvider() else { return .rejected("control_disabled") }
        guard let active, active.identity == identity else { return .rejected("lease_not_active") }
        guard active.expiresAt > now else { return .rejected("lease_expired") }
        if let previous = active.payloads[sequence] {
            return previous == payload ? .duplicate : .rejected("sequence_payload_conflict")
        }
        let nextSequence = active.lastSequence == UInt64.max ? UInt64.max : active.lastSequence + 1
        return sequence == nextSequence ? nil : .rejected("input_sequence_must_increase_by_one")
    }

    public func deliver(
        _ identity: ControlLeaseIdentity,
        sequence: UInt64,
        payload: Data,
        now: Date = Date(),
        nativeDelivery: () -> Bool
    ) -> ControlDeliveryResult {
        deliver(identity, sequence: sequence, payload: payload, now: now) {
            nativeDelivery() ? nil : "input_delivery_failed"
        }
    }

    /// `nativeDeliveryFailure` returns nil once input reached the Mac, or the
    /// reason it did not (the window could not be raised, the view changed), so
    /// the phone can say why control stopped. A failure never advances the sequence.
    public func deliver(
        _ identity: ControlLeaseIdentity,
        sequence: UInt64,
        payload: Data,
        now: Date = Date(),
        nativeDeliveryFailure: () -> String?
    ) -> ControlDeliveryResult {
        lock.lock()
        defer { lock.unlock() }
        if let receipt = deliveryReceipt(identity, sequence: sequence, payload: payload, now: now) {
            if receipt == .rejected("control_disabled") || receipt == .rejected("lease_expired") { active = nil }
            return receipt
        }
        if let failure = nativeDeliveryFailure() { return .rejected(failure) }
        guard var current = self.active, current.identity == identity else {
            return .rejected("lease_not_active")
        }
        current.lastSequence = sequence
        current.payloads[sequence] = payload
        if current.payloads.count > maximumRememberedPayloads,
           let oldest = current.payloads.keys.min() {
            current.payloads.removeValue(forKey: oldest)
        }
        self.active = current
        return .delivered
    }

    @discardableResult
    public func release(_ identity: ControlLeaseIdentity) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard active?.identity == identity else { return false }
        active = nil
        return true
    }

    @discardableResult
    public func releaseAll() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        let hadActive = active != nil
        active = nil
        return hadActive
    }

    @discardableResult
    public func expire(now: Date = Date()) -> Bool {
        expireAndReturnIdentity(now: now) != nil
    }

    /// Expires the active lease and returns its exact identity so the
    /// executable can revoke the matching durable lease without guessing.
    public func expireAndReturnIdentity(now: Date = Date()) -> ControlLeaseIdentity? {
        lock.lock()
        defer { lock.unlock() }
        guard let active, active.expiresAt <= now else { return nil }
        self.active = nil
        return active.identity
    }

    public func activeIdentity() -> ControlLeaseIdentity? {
        lock.lock()
        defer { lock.unlock() }
        return active?.identity
    }
}

public struct ControlPoint: Equatable, Sendable {
    public let x: Double
    public let y: Double

    public init(x: Double, y: Double) {
        self.x = x
        self.y = y
    }
}

public enum ControlInputTranslator {
    public static func validText(_ text: String, maximum: Int, allowEmpty: Bool = false) -> Bool {
        (allowEmpty || !text.isEmpty) && text.unicodeScalars.count <= maximum
            && text.unicodeScalars.allSatisfy { $0.properties.generalCategory != .control || $0 == "\n" || $0 == "\t" }
    }

    /// CGEvent accepts at most 64 UTF-16 units per text event. Keep surrogate
    /// pairs together so an emoji at an event boundary remains valid text.
    public static func unicodeEventChunks(_ text: String) -> [[UInt16]] {
        let units = Array(text.utf16)
        var chunks: [[UInt16]] = []
        var start = 0
        while start < units.count {
            var end = min(start + 64, units.count)
            if end < units.count, (0xD800...0xDBFF).contains(units[end - 1]) { end -= 1 }
            chunks.append(Array(units[start..<end]))
            start = end
        }
        return chunks
    }

    public static let shiftModifier: UInt32 = 1 << 0
    public static let controlModifier: UInt32 = 1 << 1
    public static let optionModifier: UInt32 = 1 << 2
    public static let commandModifier: UInt32 = 1 << 3
    public static let capsLockModifier: UInt32 = 1 << 4
    public static let allowedModifierMask: UInt32 = 0b1_1111

    public static func point(x: Double, y: Double, in source: CaptureSourceDescriptor) -> ControlPoint? {
        guard x.isFinite, y.isFinite, (0...1).contains(x), (0...1).contains(y) else { return nil }
        let bounds = source.contentRect.width > 0 && source.contentRect.height > 0
            ? source.contentRect
            : CaptureRect(
                x: source.contentRect.x,
                y: source.contentRect.y,
                width: Double(source.width) / max(source.scale, 0.01),
                height: Double(source.height) / max(source.scale, 0.01)
            )
        return ControlPoint(x: bounds.x + x * bounds.width, y: bounds.y + y * bounds.height)
    }

    /// Scroll carries no position, so it lands under the Mac pointer. After a
    /// window switch that pointer can still be over the previous window: start
    /// from the selected window's center instead. Nil means the window is
    /// covered there too, and the scroll is refused.
    public static func scrollAnchor(
        pointer: ControlPoint, in source: CaptureSourceDescriptor, receivesInput: (ControlPoint) -> Bool
    ) -> (point: ControlPoint, movesPointer: Bool)? {
        if source.kind != .window || receivesInput(pointer) { return (pointer, false) }
        guard let center = point(x: 0.5, y: 0.5, in: source), receivesInput(center) else { return nil }
        return (center, true)
    }

    public static func keyCode(for key: String) -> UInt16? {
        let normalized = key.lowercased()
        if normalized.count == 1, let scalar = normalized.unicodeScalars.first {
            let letters: [Unicode.Scalar: UInt16] = [
                "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7,
                "c": 8, "v": 9, "b": 11, "q": 12, "w": 13, "e": 14, "r": 15,
                "y": 16, "t": 17, "1": 18, "2": 19, "3": 20, "4": 21, "6": 22,
                "5": 23, "=": 24, "9": 25, "7": 26, "-": 27, "8": 28, "0": 29,
                "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35, "l": 37,
                "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44,
                "n": 45, "m": 46, ".": 47, "`": 50,
            ]
            return letters[scalar]
        }
        return [
            "return": 36, "enter": 36, "tab": 48, "space": 49, "delete": 51,
            "backspace": 51, "escape": 53, "esc": 53, "command": 55, "shift": 56,
            "capslock": 57, "option": 58, "control": 59, "rightcommand": 54,
            "rightshift": 60, "rightoption": 61, "rightcontrol": 62, "left": 123,
            "right": 124, "down": 125, "up": 126, "home": 115, "end": 119,
            "pageup": 116, "pagedown": 121, "forwarddelete": 117,
            "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97,
            "f7": 98, "f8": 100, "f9": 101, "f10": 109, "f11": 103, "f12": 111,
            "f13": 105, "f14": 107, "f15": 113, "f16": 106, "f17": 64, "f18": 79, "f19": 80,
            // The Mac's Help key sits where other keyboards have Insert.
            "insert": 114,
            // Aliases the Windows and Linux rows send.
            "ctrl": 59, "alt": 58,
        ][normalized]
    }

    /// Media, volume and brightness keys are not virtual keys: the Mac posts
    /// them as system-defined events carrying one of these NX key types.
    public static func systemKeyType(for key: String) -> Int? {
        [
            "volumeup": 0, "volumedown": 1, "brightnessup": 2, "brightnessdown": 3,
            "mute": 7, "playpause": 16, "next": 17, "previous": 18,
        ][key.lowercased()]
    }

    /// Names every helper accepts that this Mac has no key for. The action is
    /// skipped; it must not fail the batch around it.
    public static func isIgnoredOnMac(_ key: String) -> Bool {
        ["win", "super", "printscreen", "menu"].contains(key.lowercased())
    }

    /// NX system-defined event (subtype 8): key type in the high word of
    /// data1, 0xA00 for down and 0xB00 for up.
    public static func systemKeyEvent(type: Int, down: Bool) -> CGEvent? {
        NSEvent.otherEvent(
            with: .systemDefined, location: .zero,
            modifierFlags: NSEvent.ModifierFlags(rawValue: down ? 0xA00 : 0xB00),
            timestamp: 0, windowNumber: 0, context: nil, subtype: 8,
            data1: (type << 16) | ((down ? 0xA : 0xB) << 8), data2: -1
        )?.cgEvent
    }

    /// Whether the Mac helper takes this key name at all.
    public static func acceptsKey(_ key: String) -> Bool {
        keyCode(for: key) != nil || systemKeyType(for: key) != nil || isIgnoredOnMac(key)
    }
}

public enum ControlInputAction: Codable, Equatable, Sendable {
    case pointer(x: Double, y: Double, phase: String, button: String?)
    case scroll(deltaX: Double, deltaY: Double)
    case key(key: String, phase: String, modifiers: UInt32)
    case text(String)
    case clipboard(operation: String, text: String?)
    /// Simulator only: one finger at a normalized point; phase down, move or
    /// up, optionally after a pause (a long press is an up after 800 ms).
    case touch(x: Double, y: Double, phase: String, delayMilliseconds: UInt32?)
    /// Simulator only: a hardware button (`SimulatorHardwareButton`); phase
    /// press, down or up.
    case button(name: String, phase: String)
    case releaseAll

    private enum CodingKeys: String, CodingKey { case type, x, y, phase, button, deltaX, deltaY, key, modifiers, text, operation, delayMs }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .type) {
        case "pointer":
            self = .pointer(
                x: try container.decode(Double.self, forKey: .x),
                y: try container.decode(Double.self, forKey: .y),
                phase: try container.decode(String.self, forKey: .phase),
                button: try container.decodeIfPresent(String.self, forKey: .button)
            )
        case "scroll":
            self = .scroll(deltaX: try container.decode(Double.self, forKey: .deltaX), deltaY: try container.decode(Double.self, forKey: .deltaY))
        case "key":
            self = .key(key: try container.decode(String.self, forKey: .key), phase: try container.decode(String.self, forKey: .phase), modifiers: try container.decode(UInt32.self, forKey: .modifiers))
        case "text":
            self = .text(try container.decode(String.self, forKey: .text))
        case "clipboard":
            self = .clipboard(operation: try container.decode(String.self, forKey: .operation), text: try container.decodeIfPresent(String.self, forKey: .text))
        case "touch":
            self = .touch(
                x: try container.decode(Double.self, forKey: .x),
                y: try container.decode(Double.self, forKey: .y),
                phase: try container.decode(String.self, forKey: .phase),
                delayMilliseconds: try container.decodeIfPresent(UInt32.self, forKey: .delayMs)
            )
        case "button":
            self = .button(name: try container.decode(String.self, forKey: .button), phase: try container.decode(String.self, forKey: .phase))
        case "releaseAll":
            self = .releaseAll
        default:
            throw DecodingError.dataCorruptedError(forKey: .type, in: container, debugDescription: "unsupported control action")
        }
    }

    /// The bytes the lease compares to recognize a replayed batch. Keys are
    /// sorted: JSONEncoder's default key order differs between encodes, which
    /// made an identical retry look like a conflicting batch.
    public static func leasePayload(_ actions: [ControlInputAction]) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return try encoder.encode(actions)
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .pointer(x, y, phase, button):
            try container.encode("pointer", forKey: .type); try container.encode(x, forKey: .x); try container.encode(y, forKey: .y); try container.encode(phase, forKey: .phase); try container.encodeIfPresent(button, forKey: .button)
        case let .scroll(deltaX, deltaY):
            try container.encode("scroll", forKey: .type); try container.encode(deltaX, forKey: .deltaX); try container.encode(deltaY, forKey: .deltaY)
        case let .key(key, phase, modifiers):
            try container.encode("key", forKey: .type); try container.encode(key, forKey: .key); try container.encode(phase, forKey: .phase); try container.encode(modifiers, forKey: .modifiers)
        case let .text(text):
            try container.encode("text", forKey: .type); try container.encode(text, forKey: .text)
        case let .clipboard(operation, text):
            try container.encode("clipboard", forKey: .type); try container.encode(operation, forKey: .operation); try container.encodeIfPresent(text, forKey: .text)
        case let .touch(x, y, phase, delayMilliseconds):
            try container.encode("touch", forKey: .type); try container.encode(x, forKey: .x); try container.encode(y, forKey: .y); try container.encode(phase, forKey: .phase); try container.encodeIfPresent(delayMilliseconds, forKey: .delayMs)
        case let .button(name, phase):
            try container.encode("button", forKey: .type); try container.encode(name, forKey: .button); try container.encode(phase, forKey: .phase)
        case .releaseAll:
            try container.encode("releaseAll", forKey: .type)
        }
    }
}

/// Distributed notifications between the control helper and Wonder's Settings.
/// Stop can only revoke the current lease; the state names carry no lease data.
public enum ControlSessionSignal {
    public static let stop = Notification.Name("com.wonder.stop-control")
    /// Posted by the helper with object `active` or `inactive` when a lease starts or ends.
    public static let state = Notification.Name("com.wonder.control-state")
    /// Asks a running helper to repost its current state.
    public static let stateRequest = Notification.Name("com.wonder.control-state-request")
    public static let active = "active"
    public static let inactive = "inactive"
}

// Clipboard safety rules informed by Sill (https://github.com/Saffsanity/sill),
// Saffsanity/sill is Apache-2.0. Independently implemented for Wonder's session relay.
public enum ComputerClipboardPolicy {
    public static let maximumBytes = 256 * 1024
    public static let ownType = "com.swaymun.wonder.clipboard"
    public static let excludedTypes: Set<String> = [
        "org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType",
        "org.nspasteboard.AutoGeneratedType", "de.petermaurer.TransientPasteboardType",
        "com.agilebits.onepassword", "com.apple.is-remote-clipboard", ownType,
    ]

    public static func canSend(lastDeliveryAt: TimeInterval?, now: TimeInterval) -> Bool {
        lastDeliveryAt.map { now - $0 >= 0.25 } ?? true
    }

    /// Background sharing never prompts: only Always Allow (or macOS before 15.4) reads.
    public static func allowsRead(accessRawValue: Int?) -> Bool {
        accessRawValue == nil || accessRawValue == 2
    }

    /// NSPasteboard.AccessBehavior raw values: default 0, ask 1, alwaysAllow 2, alwaysDeny 3.
    /// Manual Copy from Mac follows the user's tap, so it may let macOS ask first.
    public static func manualAccess(accessRawValue: Int?) -> ComputerClipboardManualAccess {
        switch accessRawValue {
        case nil, 2: .read
        case 0, 1: .readWithPrompt
        default: .denied
        }
    }

    /// Inspect all item types before invoking the data reader, even for a manual copy.
    public static func copy(types: [String], read: () -> String?) -> ComputerClipboardCopy {
        guard excludedTypes.isDisjoint(with: types) else { return .unavailable(.sensitive) }
        guard types.contains("public.utf8-plain-text"), let text = read(), !text.isEmpty else {
            return .unavailable(.noText)
        }
        guard text.utf8.count <= maximumBytes else { return .unavailable(.tooLarge) }
        return .text(text)
    }

    public static func text(types: [String], read: () -> String?) -> String? {
        if case let .text(text) = copy(types: types, read: read) { return text }
        return nil
    }
}

public enum ComputerClipboardManualAccess: Equatable, Sendable {
    case read, readWithPrompt, denied
}

public enum ComputerClipboardCopy: Equatable, Sendable {
    case text(String)
    case unavailable(Reason)

    /// Codes the daemon turns into user language; none of them carry content.
    public enum Reason: String, Sendable {
        case permissionDenied = "clipboard_permission_denied"
        case permissionPrompt = "clipboard_permission_prompt"
        case sensitive = "clipboard_sensitive"
        case noText = "clipboard_no_text"
        case tooLarge = "clipboard_too_large"
    }
}

#if canImport(AppKit)
import AppKit

public extension ComputerClipboardPolicy {
    static func accessRawValue(of board: NSPasteboard) -> Int? {
        if #available(macOS 15.4, *) { return board.accessBehavior.rawValue }
        return nil
    }

    static func allowsRead(from board: NSPasteboard) -> Bool {
        allowsRead(accessRawValue: accessRawValue(of: board))
    }
}

/// Manual Copy from Mac. Helper requests run serially on the main thread, so a
/// read that may wait on macOS's paste prompt runs on its own queue: the helper
/// keeps handling input, including a remote click on that prompt, and the next
/// tap collects the result while the pasteboard is unchanged. Nothing is logged.
public final class ComputerClipboardManualCopy: @unchecked Sendable {
    /// The control response carries at most 8,192 code points (wonder-http-v1).
    public static let maximumScalars = 8_192
    public static let resultLifetime: TimeInterval = 60
    public static let promptTimeout: TimeInterval = 30

    private let boardName: String?
    private let accessRawValue: @Sendable (NSPasteboard) -> Int?
    private let readString: @Sendable (NSPasteboard) -> String?
    private let queue = DispatchQueue(label: "com.wonder.computer-use.clipboard-copy", qos: .userInitiated)
    private let lock = NSLock()
    private var pending: (count: Int, startedAt: TimeInterval, result: ComputerClipboardCopy?)?

    /// The nil board name is production-only; tests always supply a private pasteboard name.
    public init(
        privateBoardName: String? = nil,
        accessRawValue: @escaping @Sendable (NSPasteboard) -> Int? = { ComputerClipboardPolicy.accessRawValue(of: $0) },
        readString: @escaping @Sendable (NSPasteboard) -> String? = { $0.string(forType: .string) }
    ) {
        boardName = privateBoardName
        self.accessRawValue = accessRawValue
        self.readString = readString
    }

    private static func board(named name: String?) -> NSPasteboard {
        name.map { NSPasteboard(name: .init($0)) } ?? .general
    }

    public func copy(now: TimeInterval = ProcessInfo.processInfo.systemUptime) -> ComputerClipboardCopy {
        let board = Self.board(named: boardName)
        let access = ComputerClipboardPolicy.manualAccess(accessRawValue: accessRawValue(board))
        guard access != .denied else { return .unavailable(.permissionDenied) }
        // Change count and types never show the prompt; only reading data can.
        let count = board.changeCount
        let types = (board.pasteboardItems ?? []).flatMap { $0.types.map(\.rawValue) }
        guard ComputerClipboardPolicy.excludedTypes.isDisjoint(with: types) else { return .unavailable(.sensitive) }
        guard types.contains("public.utf8-plain-text") else { return .unavailable(.noText) }
        guard access == .readWithPrompt else {
            let result = Self.bounded(ComputerClipboardPolicy.copy(types: types) { readString(board) })
            // A provider can change the pasteboard while fulfilling a promised value.
            return count == board.changeCount ? result : .unavailable(.noText)
        }
        return lock.withLock {
            if let pending, pending.count == count, now - pending.startedAt < Self.resultLifetime {
                guard let result = pending.result else {
                    return .unavailable(now - pending.startedAt < Self.promptTimeout ? .permissionPrompt : .permissionDenied)
                }
                self.pending = nil
                return result
            }
            pending = (count, now, nil)
            let name = boardName, read = readString
            queue.async { [self] in
                let board = Self.board(named: name)
                var unreadable = false
                var result = ComputerClipboardPolicy.copy(types: types) {
                    let value = read(board)
                    unreadable = value == nil
                    return value
                }
                // No data after a prompt means macOS (or the person) declined it.
                if unreadable { result = .unavailable(.permissionDenied) }
                let unchanged = board.changeCount == count
                lock.withLock {
                    guard self.pending?.count == count, self.pending?.startedAt == now else { return }
                    self.pending = unchanged ? (count, now, Self.bounded(result)) : nil
                }
                // An uncollected copy does not linger in helper memory.
                queue.asyncAfter(deadline: .now() + Self.resultLifetime) { [self] in
                    lock.withLock {
                        if self.pending?.count == count, self.pending?.startedAt == now { self.pending = nil }
                    }
                }
            }
            return .unavailable(.permissionPrompt)
        }
    }

    private static func bounded(_ result: ComputerClipboardCopy) -> ComputerClipboardCopy {
        if case let .text(text) = result, text.unicodeScalars.count > maximumScalars { return .unavailable(.tooLarge) }
        return result
    }
}

public struct ComputerClipboardSnapshot: Sendable {
    public let subscriptionID: String?
    public let sequence: UInt64
    public let status: String
    public let text: String?
}

/// Actor-owned pasteboard work runs off the main actor. No content is logged or persisted.
/// The nil board name is production-only; tests always supply a private pasteboard name.
public actor ComputerClipboardWatch {
    private let boardName: String?
    private var board: NSPasteboard?
    private var subscriptionID: String?
    private var seen: Int?
    private var pendingEmpty: (count: Int, since: TimeInterval)?
    private var sequence: UInt64 = 0
    private var latest: String?
    private var latestAt: TimeInterval = 0
    private var lastDeliveryAt: TimeInterval?
    private var renewedAt: TimeInterval = 0
    private var watcher: Task<Void, Never>?
    private var status = "off"

    public init(privateBoardName: String? = nil) { boardName = privateBoardName }

    public func poll(subscriptionID requested: String?, after cursor: UInt64) -> ComputerClipboardSnapshot {
        guard let requested else { stop(); return snapshot(after: cursor) }
        if subscriptionID != requested {
            stop()
            subscriptionID = requested
            // Do not send what was already copied before this subscription.
            if board == nil { board = boardName.map { NSPasteboard(name: .init($0)) } ?? .general }
            baseline()
            watcher = Task { [weak self] in
                while !Task.isCancelled {
                    do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
                    guard let self else { return }
                    await self.tick()
                }
            }
        }
        renewedAt = ProcessInfo.processInfo.systemUptime
        return snapshot(after: cursor)
    }

    public func stop() {
        watcher?.cancel()
        watcher = nil
        subscriptionID = nil
        latest = nil
        lastDeliveryAt = nil
        seen = nil
        pendingEmpty = nil
        status = "off"
    }

    private func snapshot(after cursor: UInt64) -> ComputerClipboardSnapshot {
        let now = ProcessInfo.processInfo.systemUptime
        if sequence > cursor, let latest, now - latestAt < 600 {
            // Retries keep their cursor until admitted; limiting a reply must not
            // acknowledge a new copy that the device has not actually received.
            guard ComputerClipboardPolicy.canSend(lastDeliveryAt: lastDeliveryAt, now: now) else {
                return ComputerClipboardSnapshot(subscriptionID: subscriptionID, sequence: cursor, status: status, text: nil)
            }
            lastDeliveryAt = now
            return ComputerClipboardSnapshot(subscriptionID: subscriptionID, sequence: sequence, status: status, text: latest)
        }
        return ComputerClipboardSnapshot(subscriptionID: subscriptionID, sequence: sequence, status: status, text: nil)
    }

    private func baseline() {
        guard let board, ComputerClipboardPolicy.allowsRead(from: board) else {
            seen = nil
            status = "permissionRequired"
            return
        }
        seen = board.changeCount
        status = "watching"
    }

    private func tick() {
        guard subscriptionID != nil else { return }
        let now = ProcessInfo.processInfo.systemUptime
        // A suspended/disconnected device cannot leave a clipboard watcher running.
        guard now - renewedAt < 3 else { stop(); return }
        guard let board, ComputerClipboardPolicy.allowsRead(from: board) else {
            seen = nil
            latest = nil
            pendingEmpty = nil
            status = "permissionRequired"
            return
        }
        status = "watching"
        guard let previous = seen else { baseline(); return }
        let count = board.changeCount
        guard count != previous else { return }
        let types = (board.pasteboardItems ?? []).flatMap { $0.types.map(\.rawValue) }
        if types.isEmpty {
            if pendingEmpty?.count != count { pendingEmpty = (count, now) }
            if now - (pendingEmpty?.since ?? now) < 1 { return }
        }
        pendingEmpty = nil
        seen = count
        // Drop the previous value on every new copy, including sensitive ones.
        latest = nil
        guard let text = ComputerClipboardPolicy.text(types: types, read: { board.string(forType: .string) }),
              count == board.changeCount else { return }
        sequence &+= 1
        latestAt = now
        latest = text
    }
}
#endif

/// macOS selects a word on a double click and a line on a triple click from
/// the click count on each mouse event; events without one are all single
/// clicks. Repeats of the same button within the system double-click interval
/// and a few points count up to three, then start over, like a Mac trackpad.
public struct ControlClickCounter: Sendable {
    public static let maximumCount = 3
    public static let slop: Double = 6
    private var button = ""
    private var point = (x: 0.0, y: 0.0)
    private var lastDown: TimeInterval?
    private var count = 0

    public init() {}

    /// The click count for a button press at `time` (seconds, monotonic).
    public mutating func down(button name: String, x: Double, y: Double, time: TimeInterval, interval: TimeInterval) -> Int {
        if let last = lastDown, name == button, time - last <= interval,
           abs(x - point.x) <= Self.slop, abs(y - point.y) <= Self.slop {
            count = count >= Self.maximumCount ? 1 : count + 1
        } else {
            count = 1
        }
        button = name; point = (x, y); lastDown = time
        return count
    }

    /// The count a release carries: that of its press.
    public var current: Int { max(count, 1) }
}
