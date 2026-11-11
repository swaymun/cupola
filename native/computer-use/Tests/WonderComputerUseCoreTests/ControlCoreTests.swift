import XCTest
import AppKit
import CoreGraphics
import IOKit.hidsystem
@testable import WonderComputerUseCore

final class ControlCoreTests: XCTestCase {
    func testMacWindowControlSharesWholeScreenPermissionGates() {
        XCTAssertTrue(MacControlSourcePolicy.allows(.window))
        XCTAssertFalse(MacControlSourcePolicy.allows(.simulator))
        XCTAssertTrue(MacControlSourcePolicy.allows(.display))
        XCTAssertTrue(MacControlSourcePolicy.allowsRebind(to: nil))
        XCTAssertTrue(MacControlSourcePolicy.allowsRebind(to: "window:42"))
        XCTAssertTrue(MacControlSourcePolicy.allowsRebind(to: "display:1"))
        XCTAssertFalse(MacControlSourcePolicy.allowsRebind(to: "simulator:ABC"))
        for granted in [false, true] {
            for allowed in [false, true] {
                let capability = MacControlSourcePolicy.capability(
                    accessibilityGranted: granted, controlAllowed: allowed)
                XCTAssertEqual(capability["available"] as? Bool, granted && allowed)
                XCTAssertEqual(capability["windowControlAvailable"] as? Bool, granted && allowed)
                XCTAssertEqual(capability["requiresAccessibility"] as? Bool, true)
                // The owner's choice outranks a missing permission.
                let reason = !allowed ? "control_disabled"
                    : !granted ? "accessibility_permission_required" : nil
                XCTAssertEqual(capability["reason"] as? String, reason)
            }
        }
    }

    private func identity(
        leaseID: String = "lease",
        requestID: String = "request",
        sessionID: String = "session",
        generation: UInt64 = 1,
        geometryRevision: UInt64 = 2,
        sourceID: String? = "display:1"
    ) -> ControlLeaseIdentity {
        ControlLeaseIdentity(
            leaseID: leaseID,
            requestID: requestID,
            sessionID: sessionID,
            generation: generation,
            geometryRevision: geometryRevision,
            sourceID: sourceID
        )
    }

    func testAllowOnceIsIdempotentAndBoundToExactIdentity() {
        var prompts = 0
        let gate = ControlLeaseGate { _ in
            prompts += 1
            return .allowOnce
        }
        let first = gate.requestConsent(identity())
        let replay = gate.requestConsent(identity())
        let conflict = gate.requestConsent(identity(generation: 2))

        XCTAssertEqual(first, .allowOnce(replayed: false))
        XCTAssertEqual(replay, .allowOnce(replayed: true))
        XCTAssertEqual(conflict, .identityConflict)
        XCTAssertEqual(prompts, 1)
    }

    func testDeniedRequestIsTerminalAndNeverReprompts() {
        var prompts = 0
        let gate = ControlLeaseGate { _ in
            prompts += 1
            return .deny
        }
        XCTAssertEqual(gate.requestConsent(identity()), .denied(replayed: false))
        XCTAssertEqual(gate.requestConsent(identity()), .denied(replayed: true))
        XCTAssertEqual(gate.activate(identity(), now: Date(), lifetime: 10), .denied)
        XCTAssertEqual(prompts, 1)
    }

    func testActivationIsExactAndSingleOwner() {
        let gate = ControlLeaseGate { _ in .allowOnce }
        let first = identity()
        let other = identity(leaseID: "other", requestID: "other-request")
        _ = gate.requestConsent(first)
        XCTAssertEqual(gate.activate(first, now: Date(timeIntervalSince1970: 10), lifetime: 10), .activated(replayed: false))
        XCTAssertEqual(gate.activate(first, now: Date(timeIntervalSince1970: 11), lifetime: 10), .activated(replayed: true))
        _ = gate.requestConsent(other)
        XCTAssertEqual(gate.activate(other, now: Date(timeIntervalSince1970: 11), lifetime: 10), .busy)
    }

    func testViewRebindKeepsConsentSequenceAndReplayReceiptsButRejectsOldGeometry() {
        var prompts = 0, deliveries = 0
        let gate = ControlLeaseGate { _ in prompts += 1; return .allowOnce }
        let now = Date()
        let old = identity()
        let selected = identity(geometryRevision: 3, sourceID: "window:42")
        _ = gate.requestConsent(old)
        _ = gate.activate(old, now: now, lifetime: 10)
        let payload = Data("click".utf8)
        XCTAssertEqual(gate.deliver(old, sequence: 1, payload: payload, now: now) { deliveries += 1; return true }, .delivered)
        XCTAssertFalse(gate.rebind(old, to: identity(leaseID: "other", geometryRevision: 3), now: now))
        XCTAssertEqual(gate.activeIdentity(), old)
        XCTAssertTrue(gate.rebind(old, to: selected, now: now))
        XCTAssertTrue(gate.requiresNativeDelivery(selected, sequence: 2, payload: payload, now: now),
                      "The next input after a switch must focus the new window before delivery")
        XCTAssertFalse(gate.requiresNativeDelivery(old, sequence: 2, payload: payload, now: now))
        XCTAssertFalse(gate.heartbeat(old, now: now, lifetime: 10))
        XCTAssertEqual(gate.activeIdentity(), selected, "A delayed old-view heartbeat must not revoke the rebound lease")
        XCTAssertEqual(gate.deliver(old, sequence: 2, payload: payload, now: now) { XCTFail("Old view delivered"); return true }, .rejected("lease_not_active"))
        XCTAssertEqual(gate.deliver(selected, sequence: 1, payload: payload, now: now) { XCTFail("Replay delivered twice"); return true }, .duplicate)
        XCTAssertEqual(gate.deliver(selected, sequence: 1, payload: Data("different".utf8), now: now) { true }, .rejected("sequence_payload_conflict"))
        XCTAssertEqual(gate.deliver(selected, sequence: 2, payload: payload, now: now) { deliveries += 1; return true }, .delivered)
        XCTAssertTrue(gate.heartbeat(selected, now: now, lifetime: 10))
        XCTAssertEqual(prompts, 1)
        XCTAssertEqual(deliveries, 2)
    }

    func testNativeDeliveryAcknowledgementPrecedesSequenceAndReplayDoesNotRedeliver() {
        let gate = ControlLeaseGate { _ in .allowOnce }
        let lease = identity()
        let payload = Data("pointer".utf8)
        XCTAssertFalse(gate.requiresNativeDelivery(lease, sequence: 1, payload: payload))
        _ = gate.requestConsent(lease)
        let activatedAt = Date()
        _ = gate.activate(lease, now: activatedAt, lifetime: 3_600)
        var deliveries = 0
        XCTAssertTrue(gate.requiresNativeDelivery(lease, sequence: 1, payload: payload))
        XCTAssertTrue(gate.requiresNativeDelivery(lease, sequence: 1, payload: payload), "Preparing focus must not consume input")
        XCTAssertEqual(gate.deliver(lease, sequence: 1, payload: payload) { deliveries += 1; return true }, .delivered)
        XCTAssertFalse(gate.requiresNativeDelivery(lease, sequence: 1, payload: payload), "A replay must not refocus the window")
        XCTAssertEqual(gate.deliver(lease, sequence: 1, payload: payload) { deliveries += 1; return true }, .duplicate)
        XCTAssertFalse(gate.requiresNativeDelivery(lease, sequence: 1, payload: Data("different".utf8)))
        XCTAssertEqual(gate.deliver(lease, sequence: 1, payload: Data("different".utf8)) { deliveries += 1; return true }, .rejected("sequence_payload_conflict"))
        XCTAssertFalse(gate.requiresNativeDelivery(lease, sequence: 3, payload: Data()))
        XCTAssertEqual(gate.deliver(lease, sequence: 3, payload: Data()) { deliveries += 1; return true }, .rejected("input_sequence_must_increase_by_one"))
        XCTAssertTrue(gate.requiresNativeDelivery(lease, sequence: 2, payload: payload))
        _ = gate.release(lease)
        XCTAssertFalse(gate.requiresNativeDelivery(lease, sequence: 2, payload: payload), "Release during activation must prevent native input")
        XCTAssertEqual(deliveries, 1)
    }

    /// A retried batch is decoded and encoded again; its replay bytes must
    /// match the first delivery or the lease calls it a conflict.
    func testRetriedBatchEncodesToTheSameLeasePayload() throws {
        let json = Data(#"[{"type":"pointer","x":0.5,"y":0.25,"phase":"down","button":"left"},{"type":"key","key":"a","phase":"press","modifiers":1},{"type":"touch","x":0.1,"y":0.9,"phase":"up","delayMs":50}]"#.utf8)
        let encodings = try Set((0..<50).map { _ in
            try ControlInputAction.leasePayload(JSONDecoder().decode([ControlInputAction].self, from: json))
        })
        XCTAssertEqual(encodings.count, 1)
    }

    func testFailedNativeDeliveryDoesNotAdvanceAndExpiryReleases() {
        let gate = ControlLeaseGate { _ in .allowOnce }
        let lease = identity()
        _ = gate.requestConsent(lease)
        let activatedAt = Date()
        _ = gate.activate(lease, now: activatedAt, lifetime: 3_600)
        XCTAssertEqual(gate.deliver(lease, sequence: 1, payload: Data()) { false }, .rejected("input_delivery_failed"))
        // The phone names why control stopped: the specific native reason survives.
        XCTAssertEqual(gate.deliver(lease, sequence: 1, payload: Data(), nativeDeliveryFailure: { "window_not_focused" }),
                       .rejected("window_not_focused"))
        var deliveries = 0
        XCTAssertEqual(gate.deliver(lease, sequence: 1, payload: Data()) { deliveries += 1; return true }, .delivered)
        XCTAssertTrue(gate.expire(now: activatedAt.addingTimeInterval(3_601)))
        XCTAssertNil(gate.activeIdentity())
        XCTAssertEqual(deliveries, 1)
    }

    func testWindowInputRejectsCoveredPointsAndMapsOffsetBounds() {
        func window(_ id: UInt32, _ rect: CGRect, alpha: Double = 1) -> [String: Any] {
            [kCGWindowNumber as String: NSNumber(value: id), kCGWindowBounds as String: rect.dictionaryRepresentation,
             kCGWindowAlpha as String: alpha]
        }
        let target = window(42, CGRect(x: -800, y: 120, width: 640, height: 480))
        let cover = window(43, CGRect(x: -700, y: 220, width: 200, height: 200))
        XCTAssertTrue(ScreenCaptureSession.windowReceivesInput(42, at: CGPoint(x: -750, y: 150), windows: [cover, target]))
        XCTAssertFalse(ScreenCaptureSession.windowReceivesInput(42, at: CGPoint(x: -600, y: 300), windows: [cover, target]))
        XCTAssertFalse(ScreenCaptureSession.windowReceivesInput(42, at: .zero, windows: [target]))
        XCTAssertFalse(ScreenCaptureSession.windowReceivesInput(42, at: CGPoint(x: -600, y: 300), windows: []))
        // Recorded 2026-10-20: the pointer is listed front-most at its own location.
        var pointer = window(4, CGRect(x: -755, y: 145, width: 28, height: 40))
        pointer[kCGWindowLayer as String] = NSNumber(value: 2_147_483_630)
        pointer[kCGWindowOwnerName as String] = "Window Server"
        XCTAssertTrue(ScreenCaptureSession.windowReceivesInput(42, at: CGPoint(x: -750, y: 150), windows: [pointer, cover, target]),
                      "Input at the pointer's own location (scroll, a second tap) must reach the window under it")
        let source = CaptureSourceDescriptor(id: "window:42", kind: .window, title: "Offset window", width: 640, height: 480,
            scale: 1, contentRect: CaptureRect(x: -800, y: 120, width: 640, height: 480))
        XCTAssertEqual(ControlInputTranslator.point(x: 0.25, y: 0.5, in: source), ControlPoint(x: -640, y: 360))
    }

    // After switching to window B the Mac pointer is still over A; the phone's
    // scroll has no position, so the first scroll must start inside B.
    func testScrollAfterWindowSwitchStartsInsideTheSelectedWindow() {
        func window(_ id: UInt32, _ rect: CGRect) -> [String: Any] {
            [kCGWindowNumber as String: NSNumber(value: id), kCGWindowBounds as String: rect.dictionaryRepresentation,
             kCGWindowAlpha as String: 1]
        }
        let a = window(41, CGRect(x: 80, y: 300, width: 420, height: 380))
        let b = window(42, CGRect(x: 620, y: 300, width: 420, height: 380))
        let source = CaptureSourceDescriptor(id: "window:42", kind: .window, title: "B", width: 420, height: 352,
            scale: 1, contentRect: CaptureRect(x: 620, y: 328, width: 420, height: 352))
        func anchor(_ pointer: ControlPoint, _ windows: [[String: Any]]) -> (point: ControlPoint, movesPointer: Bool)? {
            ControlInputTranslator.scrollAnchor(pointer: pointer, in: source) {
                ScreenCaptureSession.windowReceivesInput(42, at: CGPoint(x: $0.x, y: $0.y), windows: windows)
            }
        }
        let overA = anchor(ControlPoint(x: 290, y: 540), [a, b])
        XCTAssertEqual(overA?.point, ControlPoint(x: 830, y: 504))
        XCTAssertEqual(overA?.movesPointer, true)
        let overB = anchor(ControlPoint(x: 700, y: 400), [a, b])
        XCTAssertEqual(overB?.point, ControlPoint(x: 700, y: 400))
        XCTAssertEqual(overB?.movesPointer, false, "A pointer already in the window stays where the person left it")
        let cover = window(43, CGRect(x: 600, y: 280, width: 500, height: 450))
        XCTAssertNil(anchor(ControlPoint(x: 290, y: 540), [cover, a, b]), "A covered window still refuses the scroll")
        let display = CaptureSourceDescriptor(id: "display:1", kind: .display, title: "Display", width: 1_512, height: 982,
            scale: 1, contentRect: CaptureRect(x: 0, y: 0, width: 1_512, height: 982))
        XCTAssertEqual(ControlInputTranslator.scrollAnchor(pointer: ControlPoint(x: 5, y: 5), in: display) { _ in false }?.movesPointer, false)
    }

    func testPointerAndKeyTranslationUseSelectedSourceBounds() throws {
        let source = CaptureSourceDescriptor(
            id: "display:1",
            kind: .display,
            title: "Display",
            width: 3_840,
            height: 2_160,
            scale: 2,
            contentRect: CaptureRect(x: 100, y: 50, width: 1_920, height: 1_080)
        )
        XCTAssertEqual(ControlInputTranslator.point(x: 0.25, y: 0.5, in: source), ControlPoint(x: 580, y: 590))
        XCTAssertEqual(ControlInputTranslator.keyCode(for: "return"), 36)
        XCTAssertNil(ControlInputTranslator.keyCode(for: "sh -c rm"))
        // Canonical key names (crates/wonderd/RPC.md, "Computer key names").
        for (name, code) in [("insert", 114), ("f13", 105), ("f14", 107), ("f15", 113), ("f16", 106),
                             ("f17", 64), ("f18", 79), ("f19", 80), ("ctrl", 59), ("alt", 58),
                             ("rightoption", 61)] {
            XCTAssertEqual(ControlInputTranslator.keyCode(for: name), UInt16(code), name)
        }
        for (name, type) in [("volumeup", 0), ("volumedown", 1), ("brightnessup", 2), ("brightnessdown", 3),
                             ("mute", 7), ("playpause", 16), ("next", 17), ("previous", 18)] {
            XCTAssertEqual(ControlInputTranslator.systemKeyType(for: name), type, name)
            XCTAssertTrue(ControlInputTranslator.acceptsKey(name), name)
        }
        for name in ["win", "super", "printscreen", "menu"] {
            XCTAssertNil(ControlInputTranslator.keyCode(for: name))
            XCTAssertTrue(ControlInputTranslator.isIgnoredOnMac(name))
            XCTAssertTrue(ControlInputTranslator.acceptsKey(name), name)
        }
        // Posted-event log per new key (K0 evidence): kept on stdout for the lead.
        var keyboard = ControlKeyboardState()
        for name in ["insert", "f13", "f14", "f15", "f16", "f17", "f18", "f19", "rightoption"] {
            let code = try XCTUnwrap(ControlInputTranslator.keyCode(for: name))
            let events = try XCTUnwrap(keyboard.events(keyCode: code, phase: "press", modifiers: 0))
            for e in events {
                print("K0-CGEVENT \(name) key code=\(e.getIntegerValueField(.keyboardEventKeycode)) type=\(e.type.rawValue)")
            }
            XCTAssertFalse(events.isEmpty, name)
        }
        for name in ["volumeup", "volumedown", "brightnessup", "brightnessdown", "mute", "playpause", "next", "previous"] {
            let type = try XCTUnwrap(ControlInputTranslator.systemKeyType(for: name))
            for down in [true, false] {
                let e = try XCTUnwrap(ControlInputTranslator.systemKeyEvent(type: type, down: down), name)
                let ns = try XCTUnwrap(NSEvent(cgEvent: e))
                XCTAssertEqual(ns.subtype.rawValue, 8)
                XCTAssertEqual(ns.data1 >> 16, type, name)
                XCTAssertEqual((ns.data1 >> 8) & 0xFF, down ? 0xA : 0xB, name)
                print("K0-CGEVENT \(name) nx type=\(type) down=\(down) data1=0x\(String(ns.data1, radix: 16))")
            }
        }
        XCTAssertFalse(ControlInputTranslator.acceptsKey("f20"))
        XCTAssertFalse(ControlInputTranslator.acceptsKey("sh -c rm"))
        XCTAssertEqual(
            ControlInputTranslator.shiftModifier
                | ControlInputTranslator.controlModifier
                | ControlInputTranslator.optionModifier
                | ControlInputTranslator.commandModifier
                | ControlInputTranslator.capsLockModifier,
            0b1_1111
        )
    }

    func testNativeClipboardValidationMatchesScalarLimitsAndSafeWhitespace() {
        XCTAssertTrue(ControlInputTranslator.validText("first\nsecond\t🙂", maximum: 8_192))
        XCTAssertTrue(ControlInputTranslator.validText("", maximum: 8_192, allowEmpty: true))
        XCTAssertFalse(ControlInputTranslator.validText("", maximum: 4_096))
        for value in ["bad\0value", "bad\rvalue", "bad\u{1b}value", String(repeating: "e\u{301}", count: 4_097)] {
            XCTAssertFalse(ControlInputTranslator.validText(value, maximum: 8_192, allowEmpty: true))
        }
        XCTAssertTrue(ControlInputTranslator.validText(String(repeating: "e\u{301}", count: 4_096), maximum: 8_192))
    }

    func testWindowShortcutPostsACompleteChordAndPreservesNativeArrowFlags() throws {
        for keyCode: UInt16 in [125, 126] {
            var keyboard = ControlKeyboardState()
            let events = try XCTUnwrap(keyboard.events(keyCode: keyCode, phase: "press", modifiers: 2))
            XCTAssertEqual(events.map(\.type), [.flagsChanged, .keyDown, .keyUp, .flagsChanged])
            XCTAssertEqual(events.map { $0.getIntegerValueField(.keyboardEventKeycode) }, [59, Int64(keyCode), Int64(keyCode), 59])
            for event in events.prefix(3) {
                XCTAssertTrue(event.flags.contains(.maskControl))
                XCTAssertNotEqual(event.flags.rawValue & UInt64(NX_DEVICELCTLKEYMASK), 0)
            }
            for event in events[1...2] {
                XCTAssertTrue(event.flags.contains(.maskNumericPad))
                XCTAssertTrue(event.flags.contains(.maskSecondaryFn))
            }
            XCTAssertFalse(events.last!.flags.contains(.maskControl))
            XCTAssertTrue(keyboard.heldKeyCodes.isEmpty)
        }
    }

    func testAppSwitcherModifierSurvivesPointerInteractionUntilExplicitRelease() throws {
        var keyboard = ControlKeyboardState()
        let down = try XCTUnwrap(keyboard.events(keyCode: 55, phase: "down", modifiers: 8)).first!
        XCTAssertEqual(down.type, .flagsChanged)
        XCTAssertNotEqual(down.flags.rawValue & UInt64(NX_DEVICELCMDKEYMASK), 0)
        let tab = try XCTUnwrap(keyboard.events(keyCode: 48, phase: "press", modifiers: 8))
        XCTAssertEqual(tab.map(\.type), [.keyDown, .keyUp])
        XCTAssertTrue(tab.allSatisfy { $0.flags.contains(.maskCommand) })
        XCTAssertTrue(keyboard.modifierFlags.contains(.maskCommand))
        XCTAssertEqual(keyboard.heldKeyCodes, [55])
        // Pointer and scroll delivery use this same state between batches.
        let up = try XCTUnwrap(keyboard.events(keyCode: 55, phase: "up", modifiers: 0)).first!
        XCTAssertEqual(up.type, .flagsChanged)
        XCTAssertFalse(up.flags.contains(.maskCommand))
        XCTAssertEqual(up.flags.rawValue & UInt64(NX_DEVICELCMDKEYMASK), 0)
        XCTAssertTrue(keyboard.heldKeyCodes.isEmpty)
        XCTAssertTrue(keyboard.modifierFlags.isEmpty)
        _ = keyboard.events(keyCode: 54, phase: "down", modifiers: 8)
        _ = keyboard.events(keyCode: 48, phase: "down", modifiers: 8)
        let releases = keyboard.releaseEvents()
        XCTAssertEqual(releases.map { $0.getIntegerValueField(.keyboardEventKeycode) }, [48, 54])
        XCTAssertTrue(releases.first!.flags.contains(.maskCommand))
        XCTAssertFalse(releases.last!.flags.contains(.maskCommand))
        XCTAssertTrue(keyboard.heldKeyCodes.isEmpty)
    }

    func testNativeTextEventsKeepUnicodeSurrogatesTogetherAtTheLimit() {
        let text = String(repeating: "a", count: 63) + "🙂" + String(repeating: "b", count: 62) + "🌍"
        let chunks = ControlInputTranslator.unicodeEventChunks(text)
        XCTAssertTrue(chunks.allSatisfy { !$0.isEmpty && $0.count <= 64 })
        XCTAssertEqual(chunks.map { String(decoding: $0, as: UTF16.self) }.joined(), text)
        XCTAssertEqual(chunks.flatMap { $0 }, Array(text.utf16))
        XCTAssertTrue(ControlInputTranslator.unicodeEventChunks("").isEmpty)
    }

    /// The phone raises its keyboard only where typing lands on the Mac:
    /// editable text, search, password and combo fields, never a button or
    /// read-only text (roles measured against AppKit controls on 2026-10-21).
    func testTextInputFocusRaisesTheKeyboardOnlyForEditableText() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox"] {
            XCTAssertTrue(TextInputFocus.acceptsTyping(role: role, valueSettable: true), role)
            XCTAssertTrue(TextInputFocus.acceptsTyping(role: role, valueSettable: nil), "\(role) when the app does not say")
            XCTAssertFalse(TextInputFocus.acceptsTyping(role: role, valueSettable: false), "read-only \(role)")
        }
        for role in ["AXButton", "AXWindow", "AXStaticText", "AXWebArea", "AXGroup"] {
            XCTAssertFalse(TextInputFocus.acceptsTyping(role: role, valueSettable: true), role)
        }
        XCTAssertFalse(TextInputFocus.acceptsTyping(role: nil, valueSettable: true))
    }

    func testTextFocusIsReportedPerLeaseOnChangeAndClearedWhenControlEnds() {
        var changes = TextFocusChanges()
        let lease = identity()
        XCTAssertEqual(changes.observe(lease, textInput: false)?.textInput, false, "A new lease starts with a known state")
        XCTAssertNil(changes.observe(lease, textInput: false))
        XCTAssertEqual(changes.observe(lease, textInput: true)?.textInput, true)
        XCTAssertNil(changes.observe(lease, textInput: true))
        let switched = identity(geometryRevision: 3, sourceID: "window:7")
        XCTAssertEqual(changes.observe(switched, textInput: true)?.identity, switched, "A new view reports again")
        let ended = changes.observe(nil, textInput: true)
        XCTAssertEqual(ended?.identity, switched)
        XCTAssertEqual(ended?.textInput, false, "Ending control while typing lowers the phone keyboard")
        XCTAssertNil(changes.observe(nil, textInput: false))
        XCTAssertEqual(changes.observe(lease, textInput: false)?.textInput, false)
        XCTAssertNil(changes.observe(nil, textInput: false), "Nothing to clear when no field had focus")
    }

    /// Control can end while the focused app answers Accessibility. That
    /// answer must not reach the phone under the lease that just ended.
    func testFocusReadWhileControlEndsIsNotReported() {
        var changes = TextFocusChanges()
        let lease = identity()
        var active: ControlLeaseIdentity? = lease
        XCTAssertNil(changes.poll(activeIdentity: { active }, sample: { active = nil; return true }))
        XCTAssertNil(changes.poll(activeIdentity: { active }, sample: { true }), "The phone never heard yes")

        active = lease
        XCTAssertEqual(changes.poll(activeIdentity: { active }, sample: { true })?.textInput, true)
        let switched = identity(geometryRevision: 3, sourceID: "window:7")
        XCTAssertNil(changes.poll(activeIdentity: { active }, sample: { active = switched; return false }),
                     "A read that spans a window switch belongs to neither view")
        let next = changes.poll(activeIdentity: { active }, sample: { false })
        XCTAssertEqual(next?.identity, switched)
        XCTAssertEqual(next?.textInput, false)
    }

    /// With one window of an app shared, a field focused in its other window
    /// takes no typing from the phone; a sheet on the shared window does.
    func testTextFocusCountsOnlyTheSharedWindowOfTheApp() {
        typealias Window = SystemDialogWindowRaiser.FocusedWindow
        let shared: UInt32 = 10_538
        func typing(_ window: Window?, shown: UInt32? = shared) -> Bool {
            TextInputFocus.acceptsTyping(role: "AXTextField", valueSettable: true, in: window, shownWindow: shown)
        }
        XCTAssertTrue(typing(Window(windowID: shared, sheetParentID: nil, sheetIDs: [])))
        XCTAssertFalse(typing(Window(windowID: 10_541, sheetParentID: nil, sheetIDs: [])), "The app's other window")
        XCTAssertTrue(typing(Window(windowID: 10_542, sheetParentID: shared, sheetIDs: [])), "A sheet on the shared window")
        XCTAssertTrue(typing(Window(windowID: shared, sheetParentID: nil, sheetIDs: [10_542]), shown: 10_542),
                      "The shared sheet's window")
        XCTAssertFalse(typing(nil), "A field whose window is unknown")
        XCTAssertTrue(typing(Window(windowID: 10_541, sheetParentID: nil, sheetIDs: []), shown: nil), "A whole display")
        XCTAssertFalse(TextInputFocus.acceptsTyping(role: "AXButton", valueSettable: true,
                                                    in: Window(windowID: shared, sheetParentID: nil, sheetIDs: []),
                                                    shownWindow: shared))
    }

    func testDeliveryExpiryUsesTheSuppliedClock() {
        let gate = ControlLeaseGate { _ in .allowOnce }
        let lease = identity()
        let activatedAt = Date(timeIntervalSince1970: 100)
        _ = gate.requestConsent(lease)
        _ = gate.activate(lease, now: activatedAt, lifetime: 10)

        XCTAssertFalse(gate.requiresNativeDelivery(lease, sequence: 1, payload: Data(), now: activatedAt.addingTimeInterval(11)))

        var delivered = false
        XCTAssertEqual(
            gate.deliver(
                lease,
                sequence: 1,
                payload: Data(),
                now: activatedAt.addingTimeInterval(11)
            ) {
                delivered = true
                return true
            },
            .rejected("lease_expired")
        )
        XCTAssertFalse(delivered)
    }
}


final class ComputerClipboardPolicyTests: XCTestCase {
    private func assertExcluded(_ marker: String, file: StaticString = #filePath, line: UInt = #line) {
        var reads = 0
        let text = ComputerClipboardPolicy.text(types: ["public.utf8-plain-text", marker]) {
            reads += 1
            return "private fixture"
        }
        XCTAssertNil(text, file: file, line: line)
        XCTAssertEqual(reads, 0, "Types must reject before reading data", file: file, line: line)
    }
    func testConcealedNeverReadsData() { assertExcluded("org.nspasteboard.ConcealedType") }
    func testTransientNeverReadsData() { assertExcluded("org.nspasteboard.TransientType") }
    func testRemoteClipboardNeverReadsData() { assertExcluded("com.apple.is-remote-clipboard") }
    func testOwnWriteNeverReadsData() { assertExcluded(ComputerClipboardPolicy.ownType) }
    func testLegacyAndPasswordManagerMarkersNeverReadData() {
        for type in ["org.nspasteboard.AutoGeneratedType", "de.petermaurer.TransientPasteboardType", "com.agilebits.onepassword"] {
            assertExcluded(type)
        }
    }
    func testNonTextNeverReadsData() {
        XCTAssertNil(ComputerClipboardPolicy.text(types: ["public.png"]) { XCTFail("Image must not be read"); return "image" })
    }
    func testTextLimitCountsUTF8BytesAndRejectsEmpty() {
        let types = ["public.utf8-plain-text"]
        let limit = ComputerClipboardPolicy.maximumBytes
        let boundary = String(repeating: "é", count: limit / 2)
        XCTAssertEqual(ComputerClipboardPolicy.text(types: types) { boundary }, boundary)
        XCTAssertNil(ComputerClipboardPolicy.text(types: types) { boundary + "a" })
        XCTAssertNil(ComputerClipboardPolicy.text(types: types) { "" })
        XCTAssertNil(ComputerClipboardPolicy.text(types: types) { nil })
    }
    func testOutboundRepliesAreLimitedToFourPerSecondIncludingRetries() {
        XCTAssertTrue(ComputerClipboardPolicy.canSend(lastDeliveryAt: nil, now: 0))
        XCTAssertFalse(ComputerClipboardPolicy.canSend(lastDeliveryAt: 1, now: 1.249))
        XCTAssertTrue(ComputerClipboardPolicy.canSend(lastDeliveryAt: 1, now: 1.25))
        XCTAssertFalse(ComputerClipboardPolicy.canSend(lastDeliveryAt: 1, now: 0))
    }
    func testOnlyExplicitAllowOrOlderMacOSPermitsBackgroundReads() {
        XCTAssertTrue(ComputerClipboardPolicy.allowsRead(accessRawValue: nil))
        XCTAssertTrue(ComputerClipboardPolicy.allowsRead(accessRawValue: 2))
        for raw in [0, 1, 3, 4, -1] { XCTAssertFalse(ComputerClipboardPolicy.allowsRead(accessRawValue: raw)) }
    }
    func testManualCopyMayPromptButStreamingNeedsAlwaysAllow() {
        XCTAssertEqual(ComputerClipboardPolicy.manualAccess(accessRawValue: nil), .read)
        XCTAssertEqual(ComputerClipboardPolicy.manualAccess(accessRawValue: 2), .read)
        XCTAssertEqual(ComputerClipboardPolicy.manualAccess(accessRawValue: 0), .readWithPrompt)
        XCTAssertEqual(ComputerClipboardPolicy.manualAccess(accessRawValue: 1), .readWithPrompt)
        for raw in [3, 4, -1] { XCTAssertEqual(ComputerClipboardPolicy.manualAccess(accessRawValue: raw), .denied) }
        XCTAssertFalse(ComputerClipboardPolicy.allowsRead(accessRawValue: 0), "Streaming must not prompt")
    }
    func testPrivatePasteboardManualCopyRejectsMarkerOnAnyItemWithoutReading() {
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        let text = NSPasteboardItem()
        text.setString("fixture", forType: .string)
        let marker = NSPasteboardItem()
        marker.setData(Data(), forType: .init("org.nspasteboard.ConcealedType"))
        board.writeObjects([text, marker])
        for access in [2, 0] {
            let copy = ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in access },
                                                   readString: { _ in XCTFail("Marked copy must not be read"); return nil })
            XCTAssertEqual(copy.copy(), .unavailable(.sensitive))
        }
    }
    func testPrivatePasteboardManualCopyReadsAllowedTextAndExplainsDenial() {
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        board.setString("fixture", forType: .string)
        for access in [nil, 2] as [Int?] {
            XCTAssertEqual(ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in access }).copy(),
                           .text("fixture"))
        }
        for access in [3, 4] {
            let denied = ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in access },
                                                     readString: { _ in XCTFail("Denied access must not read"); return nil })
            XCTAssertEqual(denied.copy(), .unavailable(.permissionDenied))
        }
        board.clearContents()
        board.setString(String(repeating: "a", count: ComputerClipboardManualCopy.maximumScalars + 1), forType: .string)
        XCTAssertEqual(ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in 2 }).copy(),
                       .unavailable(.tooLarge))
    }
    func testPrivatePasteboardPromptedCopyReadsOffTheCallerAndHandsResultToNextTap() throws {
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        board.setString("fixture", forType: .string)
        let prompt = DispatchSemaphore(value: 0)
        let finished = DispatchSemaphore(value: 0)
        let reads = LockedCounter()
        let copy = ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in 0 }) { board in
            reads.increment()
            prompt.wait() // Stands in for macOS's paste prompt waiting on the person.
            defer { finished.signal() }
            return board.string(forType: .string)
        }
        XCTAssertEqual(copy.copy(now: 10), .unavailable(.permissionPrompt), "The caller must not wait on the prompt")
        XCTAssertEqual(copy.copy(now: 11), .unavailable(.permissionPrompt))
        XCTAssertEqual(copy.copy(now: 41), .unavailable(.permissionDenied), "An unanswered prompt explains the setting")
        prompt.signal()
        XCTAssertEqual(finished.wait(timeout: .now() + 5), .success)
        try waitUntil { copy.copy(now: 42) == .text("fixture") }
        XCTAssertEqual(reads.value, 1)
        // Single use: the next tap reads (and may ask) again.
        XCTAssertEqual(copy.copy(now: 43), .unavailable(.permissionPrompt))
        prompt.signal()
        XCTAssertEqual(finished.wait(timeout: .now() + 5), .success)
    }
    func testPrivatePasteboardPromptedCopyReportsDeclinedPromptAndDropsChangedContents() throws {
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        board.setString("fixture", forType: .string)
        let declined = ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in 1 },
                                                   readString: { _ in nil })
        XCTAssertEqual(declined.copy(now: 0), .unavailable(.permissionPrompt))
        try waitUntil { declined.copy(now: 1) == .unavailable(.permissionDenied) }
        let changed = ComputerClipboardManualCopy(privateBoardName: board.name.rawValue, accessRawValue: { _ in 0 }) { board in
            let value = board.string(forType: .string)
            board.clearContents()
            board.setString("newer copy", forType: .string)
            return value
        }
        XCTAssertEqual(changed.copy(now: 0), .unavailable(.permissionPrompt))
        try waitUntil { board.string(forType: .string) == "newer copy" }
        // The held result belonged to older contents, so this tap asks again.
        XCTAssertEqual(changed.copy(now: 1), .unavailable(.permissionPrompt))
    }
    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) throws {
        let deadline = Date().addingTimeInterval(5)
        while !condition() {
            guard Date() < deadline else { return XCTFail("Timed out", file: file, line: line) }
            Thread.sleep(forTimeInterval: 0.01)
        }
    }
    func testPrivateWatcherSendsNewCopyAndStopsWithoutReplayingExistingContents() async throws {
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        board.setString("before subscription", forType: .string)
        let watch = ComputerClipboardWatch(privateBoardName: board.name.rawValue)
        let first = await watch.poll(subscriptionID: "first", after: 0)
        XCTAssertNil(first.text)
        board.clearContents()
        board.setString("copied during session", forType: .string)
        try await Task.sleep(for: .milliseconds(400))
        let received = await watch.poll(subscriptionID: "first", after: 0)
        XCTAssertEqual(received.text, "copied during session")
        XCTAssertGreaterThan(received.sequence, 0)
        let acknowledged = await watch.poll(subscriptionID: "first", after: received.sequence)
        XCTAssertNil(acknowledged.text)
        let stopped = await watch.poll(subscriptionID: nil, after: 0)
        XCTAssertEqual(stopped.status, "off")
        board.clearContents()
        board.setString("after stop", forType: .string)
        try await Task.sleep(for: .milliseconds(400))
        let restarted = await watch.poll(subscriptionID: "second", after: 0)
        XCTAssertNil(restarted.text)
        await watch.stop()
    }
    func testPrivateWatcherSkipsConcealedCopyAndExpiresWhenDeviceStopsPolling() async throws {
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        let watch = ComputerClipboardWatch(privateBoardName: board.name.rawValue)
        _ = await watch.poll(subscriptionID: "device", after: 0)
        let item = NSPasteboardItem()
        item.setString("synthetic secret", forType: .string)
        item.setData(Data(), forType: .init("org.nspasteboard.ConcealedType"))
        board.clearContents()
        board.writeObjects([item])
        try await Task.sleep(for: .milliseconds(400))
        let blocked = await watch.poll(subscriptionID: "device", after: 0)
        XCTAssertNil(blocked.text)
        try await Task.sleep(for: .milliseconds(3300))
        board.clearContents()
        board.setString("after device disappeared", forType: .string)
        try await Task.sleep(for: .milliseconds(400))
        let resumed = await watch.poll(subscriptionID: "device", after: 0)
        XCTAssertNil(resumed.text, "Expired subscription must baseline instead of reading this copy")
        await watch.stop()
    }
}

private final class LockedCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    var value: Int { lock.withLock { count } }
    func increment() { lock.withLock { count += 1 } }
}
