import XCTest
import UIKit

@MainActor final class WonderUITests: XCTestCase {
    // A landscape check must not leave the next test rotated.
    override func setUp() async throws {
        XCUIDevice.shared.orientation = .portrait
    }
    private let appBundleIdentifier = "com.swaymun.wonder"

    // Keep the visible state with every failure; a failed assertion alone
    // rarely shows which control was covered or missing.
    override func record(_ issue: XCTIssue) {
        var issue = issue
        let screen = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        screen.name = "Failure screen"; screen.lifetime = .keepAlways
        issue.add(screen)
        let tree = XCTAttachment(string: XCUIApplication(bundleIdentifier: appBundleIdentifier).debugDescription)
        tree.name = "Failure hierarchy"; tree.lifetime = .keepAlways
        issue.add(tree)
        super.record(issue)
    }
    private let appDisplayName = "Cupola"

    private func workspacePreviewClose(_ app: XCUIApplication, legacyID: String) -> XCUIElement {
        let backToFiles = app.buttons["workspace-preview-back"]
        return backToFiles.exists ? backToFiles : app.buttons[legacyID]
    }

    private func selectComputerMoreAction(_ app: XCUIApplication, identifier: String) {
        // A notification banner overlaps the toolbar on physical iPhones.
        // Let it disappear instead of tapping through it into another screen.
        let banner = XCUIApplication(bundleIdentifier: "com.apple.springboard")
            .descendants(matching: .any).matching(identifier: "NotificationShortLookView").firstMatch
        if banner.exists { XCTAssertTrue(waitUntilGone(banner, timeout: 15)) }
        app.buttons["computer-session-more"].tap()
        // iOS 27's native section-backed menu items expose their spoken label
        // but may omit the SwiftUI accessibility identifier.
        let labels = ["computer-session-fit": "Fit", "computer-session-zoom-in": "Zoom in",
                      "computer-session-zoom-out": "Zoom out", "computer-session-recenter": "Recenter pointer"]
        let action = app.buttons[labels[identifier] ?? identifier]
        XCTAssertTrue(action.waitForExistence(timeout: 10))
        action.tap()
        XCTAssertTrue(waitUntilGone(action, timeout: 10))
    }

    // Opt-in USB check. No provider, thread, draft or pairing mutation is needed:
    // New chat exposes View computer even when this host has no agent threads.
    func testPhysicalPairedHostDesktopControl() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let host = environment["WONDER_PHYSICAL_HOST"], !host.isEmpty else {
            throw XCTSkip("Run scripts/physical-host-control with an explicitly authorized paired physical host")
        }
        continueAfterFailure = false
        let platform = try XCTUnwrap(environment["WONDER_PHYSICAL_PLATFORM"])
        XCTAssertTrue(["linux", "windows", "macos"].contains(platform))
        let token = try XCTUnwrap(environment["WONDER_PHYSICAL_TEXT"])
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        let monitor = installWonderLocalNetworkPermissionMonitor()
        defer { removeUIInterruptionMonitor(monitor) }
        app.launch()
        defer {
            // Release even after an assertion fails; never unpair the phone.
            let done = app.buttons["computer-session-done"]
            if done.exists && done.isHittable { done.tap() }
            let close = app.buttons["computer-session-close"]
            if close.exists && close.isHittable { close.tap() }
        }

        // Launch normally, then use only the actual new-chat computer picker.
        let picker = app.buttons["connection-picker"]
        if !picker.waitForExistence(timeout: 10) {
            openSidebarIfNeeded(app)
            let newChat = app.buttons["sidebar-new-chat"]
            XCTAssertTrue(newChat.waitForExistence(timeout: 10), app.debugDescription)
            newChat.tap()
        }
        XCTAssertTrue(picker.waitForExistence(timeout: 15), app.debugDescription)
        picker.tap()
        let hostChoice = app.buttons.matching(NSPredicate(
            format: "identifier BEGINSWITH %@ AND label == %@", "connection-option:", host
        )).firstMatch
        XCTAssertTrue(hostChoice.waitForExistence(timeout: 10), "The exact paired host must be available")
        hostChoice.tap()
        XCTAssertTrue((picker.value as? String)?.hasPrefix(host + ", ") == true)
        // New chat no longer carries a View computer pill (UX-DUO-3), so the thread-free entry has no
        // control here. Opening the computer from a chat or the rail has its own checks.
        throw XCTSkip("New chat has no View computer entry; the physical computer check needs another entry")
        let container = app.descendants(matching: .any).matching(identifier: "computer-session-container").firstMatch
        XCTAssertTrue(container.waitForExistence(timeout: 15))
        let header = app.buttons["computer-session-connection-picker"]
        XCTAssertEqual(header.label, "Computer, " + host)
        let preview = app.descendants(matching: .any).matching(identifier: "computer-session-preview").firstMatch
        func requireLive() {
            let live = XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == %@", "Live"), object: preview)
            XCTAssertEqual(XCTWaiter.wait(for: [live], timeout: 90), .completed, app.debugDescription)
        }
        requireLive()
        physicalHostCheckpoint(app, "live-status", detail: "Preview reports Live; moving frames require the recorded host sync pulse")
        // The runner displays a magenta sync card only after receiving this line.
        // It aligns the two independent recordings on that same visible event.
        print("PHYSICAL_EVENT sync-ready")
        RunLoop.current.run(until: Date().addingTimeInterval(10))
        physicalHostCheckpoint(app, "sync-observation", detail: "See phone and host video for the magenta sync card")

        let takeControl = app.buttons["computer-session-take-control"]
        XCTAssertTrue(takeControl.waitForExistence(timeout: 15))
        takeControl.tap()
        let active = app.descendants(matching: .any).matching(identifier: "computer-session-control-active").firstMatch
        XCTAssertTrue(active.waitForExistence(timeout: 125), app.debugDescription)
        physicalHostCheckpoint(app, "take-control", detail: "Control active")
        // Opt-in latency baseline: real phone gestures, existing pairing, no clicks,
        // typing or window mutation. Journal samples remain the measured result.
        if let raw = environment["WONDER_PHYSICAL_LATENCY_SECONDS"] {
            let seconds = try XCTUnwrap(Double(raw))
            XCTAssertTrue((1...120).contains(seconds))
            let pad = app.descendants(matching: .any).matching(identifier: "computer-session-trackpad").firstMatch
            if !pad.exists {
                let toggle = app.buttons["computer-session-trackpad-toggle"]
                XCTAssertTrue(toggle.waitForExistence(timeout: 10), app.debugDescription)
                toggle.tap()
            }
            XCTAssertTrue(pad.waitForExistence(timeout: 10), app.debugDescription)
            physicalHostCheckpoint(app, "latency-before", detail: "Real trackpad movement; no clicks or typing")
            let center = CGPoint(x: pad.frame.midX, y: pad.frame.midY)
            let started = Date()
            var gestures = 0
            while Date().timeIntervalSince(started) < seconds {
                XCTAssertTrue(active.exists, "Control lost during latency sampling")
                try physicalTouchGesture("Latency pointer motion", paths: [
                    PhysicalTouchPath(start: center, startTime: 0,
                        moves: [(CGPoint(x: center.x + 30, y: center.y), 1),
                                (CGPoint(x: center.x - 30, y: center.y), 2),
                                (center, 3)], liftTime: 3.1)
                ])
                gestures += 1
            }
            physicalHostCheckpoint(app, "latency-after", detail: "gestures=\(gestures); seconds=\(Date().timeIntervalSince(started))")
            return
        }
        selectComputerMoreAction(app, identifier: "computer-session-fit")
        app.buttons["computer-session-more"].tap()
        if !app.buttons["Direct touch"].exists { app.buttons["Pointer mode"].tap() }
        let direct = app.buttons["Direct touch"]
        XCTAssertTrue(direct.waitForExistence(timeout: 10))
        direct.tap()
        XCTAssertTrue(active.waitForExistence(timeout: 10))

        // Coordinates refer to the desktop, excluding the viewport letterbox.
        // The runner supplies the known fixture's title-bar centre for dragging.
        func desktopPoint(_ x: Double, _ y: Double) -> XCUICoordinate {
            let width = Double(environment["WONDER_PHYSICAL_WIDTH"] ?? "1280") ?? 1280
            let height = Double(environment["WONDER_PHYSICAL_HEIGHT"] ?? "800") ?? 800
            let frame = preview.frame
            let scale = min(frame.width / width, frame.height / height)
            return preview.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(
                dx: (frame.width - width * scale) / 2 + x * width * scale,
                dy: (frame.height - height * scale) / 2 + y * height * scale))
        }
        desktopPoint(0.5, 0.5).tap()
        let keyboard = app.buttons["computer-session-keyboard"]
        XCTAssertTrue(keyboard.waitForExistence(timeout: 10))
        keyboard.tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 10))
        app.typeText(token + "\n")
        keyboard.tap()
        XCTAssertTrue(waitUntilGone(app.keyboards.firstMatch, timeout: 10))
        physicalHostCheckpoint(app, "tap-and-type", detail: "Typed unique token into the remote fixture; runner must verify host file")

        // Keep checking independent contracts after a product failure. XCTest's
        // abort-on-first-failure can bypass Swift defer and leave video pending.
        continueAfterFailure = true
        let modifiers = platform == "linux" ? ["ctrl", "alt", "super"] : platform == "windows" ? ["ctrl", "alt", "win"] : []
        for key in modifiers {
            let control = app.buttons["computer-session-shortcut-" + key]
            XCTAssertTrue(control.waitForExistence(timeout: 10), "Missing \(platform) key \(key)")
            control.tap()
            if key != "win" {
                XCTAssertEqual(control.value as? String, "Selected for next key")
                control.tap()
                XCTAssertEqual(control.value as? String, "")
            }
        }
        if platform == "macos" {
            for key in ["all-windows", "app-windows", "next-window", "applications", "app-switcher"] {
                XCTAssertTrue(app.buttons["computer-session-shortcut-" + key].exists, "Missing Mac platform control \(key)")
            }
        }
        for key in ["escape", "tab"] {
            let control = app.buttons["computer-session-key-" + key]
            XCTAssertTrue(control.exists && control.isHittable)
            control.tap()
        }
        physicalHostCheckpoint(app, "platform-keys", detail: "\(platform): \(modifiers.joined(separator: ", ")), Escape, Tab")

        let dragX = Double(environment["WONDER_PHYSICAL_DRAG_X"] ?? "0.5") ?? 0.5
        let dragY = Double(environment["WONDER_PHYSICAL_DRAG_Y"] ?? "0.15") ?? 0.15
        let from = desktopPoint(dragX, dragY)
        try physicalTouchDrag(from: from, to: desktopPoint(dragX + 0.08, dragY + 0.06))
        physicalHostCheckpoint(app, "drag", detail: "Continuous tap-then-hold drag sent; runner compares host geometry", status: "requested")

        // iPhone rejects XCTest's public pointer-wheel API. Send two parallel
        // touch paths through XCTest's own synthesizer, as a real two-finger pan.
        // Drag ends on the title bar. Put the remote pointer back inside
        // the fixture so the wheel events target its content, not the WM.
        desktopPoint(0.5, 0.5).tap()
        let scrollStartedActive = active.exists
        XCTAssertTrue(scrollStartedActive, "Scroll must start with active control, independently of the preceding drag")
        print("PHYSICAL_EVENT scroll-before")
        physicalHostCheckpoint(app, "scroll-before", detail: "Pointer inside fixture; active control: \(scrollStartedActive)",
                               status: scrollStartedActive ? "passed" : "failed")
        if scrollStartedActive { try physicalTwoFingerScroll(preview) }
        RunLoop.current.run(until: Date().addingTimeInterval(2))
        let scrollKeptControl = active.exists
        XCTAssertTrue(scrollKeptControl, "Two-finger scrolling must not release control: " + app.debugDescription)
        physicalHostCheckpoint(app, "scroll-after", detail: scrollKeptControl
            ? "Two-finger touch scroll sent; inspect host content movement"
            : "Product failure: input was rejected and control was released", status: scrollKeptControl ? "passed" : "failed")

        XCTAssertTrue(active.waitForExistence(timeout: 10), app.debugDescription)
        XCTAssertFalse(takeControl.exists)
        app.buttons["computer-session-done"].tap()
        XCTAssertTrue(takeControl.waitForExistence(timeout: 15))
        XCTAssertFalse(active.exists)
        physicalHostCheckpoint(app, "release", detail: "Done restores Take control and removes active controls")
        app.buttons["computer-session-close"].tap()
        XCTAssertTrue(waitUntilGone(container, timeout: 10))
        physicalHostCheckpoint(app, "close", detail: "Returned to New chat without creating a thread")
    }

    /// Opt-in physical-phone smoke (scripts/device-smoke): the first screens a new user opens, on the
    /// real app with no fixtures, so a device-only crash (build 213, New chat) fails here and not in the
    /// owner's hands. Taps only navigation, menus and the trackpad; it never sends, pairs, unpairs or deletes.
    /// Each step prints SMOKE_BEGIN/SMOKE_OK/SMOKE_SKIP lines the runner reads; a crash leaves the last
    /// SMOKE_BEGIN without its SMOKE_OK, which names the step.
    func testDeviceSmoke() throws {
        guard ProcessInfo.processInfo.environment["WONDER_DEVICE_SMOKE"] == "1" else {
            throw XCTSkip("Run scripts/device-smoke")
        }
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        let monitor = installWonderLocalNetworkPermissionMonitor()
        defer { removeUIInterruptionMonitor(monitor) }

        func step(_ name: String, _ body: () throws -> String?) rethrows {
            print("SMOKE_BEGIN \(name)")
            let skipped = try body()
            XCTAssertEqual(app.state, .runningForeground, "\(name): the app is no longer running (crashed?)")
            let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
            shot.name = "smoke-" + name; shot.lifetime = .keepAlways
            add(shot)
            print(skipped.map { "SMOKE_SKIP \(name) \($0)" } ?? "SMOKE_OK \(name)")
        }
        func needs(_ element: XCUIElement, _ name: String, _ seconds: TimeInterval = 15) {
            XCTAssertTrue(element.waitForExistence(timeout: seconds), "\(name) not found within \(Int(seconds))s")
        }
        func dismissMenu() {
            // Close without choosing anything. The Model pill opens a sheet whose rows fill the screen, so it
            // is dragged down from its title; a menu or popover closes on a tap outside it.
            let sheet = app.descendants(matching: .any).matching(NSPredicate(
                format: "label == 'Search models' OR placeholderValue == 'Search models'")).firstMatch
            if sheet.exists {
                app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.06))
                    .press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.95)))
            } else {
                // The top-left corner beside the status bar: outside every menu and popover. Not the left margin
                // halfway down, where the title menu's computer and simulator rows now reach (a tap there opened
                // a simulator), and never the middle, where the chat header menu holds Fork, Archive and Delete.
                app.coordinate(withNormalizedOffset: CGVector(dx: 0.015, dy: 0.02)).tap()
            }
            RunLoop.current.run(until: Date().addingTimeInterval(1.0))
        }
        func openNewChat() {
            openSidebarIfNeeded(app)
            let newChat = app.buttons["sidebar-new-chat"]
            needs(newChat, "sidebar-new-chat")
            newChat.tap()
            needs(app.descendants(matching: .any).matching(identifier: "new-chat-composer").firstMatch, "new-chat-composer", 20)
        }

        func onScreen(_ element: XCUIElement) -> Bool {
            let frame = element.frame
            return frame.width > 0 && frame.height > 0 && app.windows.firstMatch.frame.contains(frame)
        }
        func folders(_ app: XCUIApplication) -> Int {
            app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'project-disclosure:'")).count
        }
        // A fresh launch lands on New chat, so a crash while that screen builds fails here, at launch.
        try step("launch") {
            app.launch()
            needs(app.descendants(matching: .any).matching(NSPredicate(
                format: "identifier IN {'new-chat-composer', 'open-sidebar', 'conversation-title-menu', 'sidebar-search'}")).firstMatch,
                  "the first screen", 45)
            return nil
        }

        try step("chats-list") {
            openSidebarIfNeeded(app)
            needs(app.buttons["sidebar-new-chat"], "sidebar-new-chat", 30)
            return nil
        }

        try step("new-chat") {
            openNewChat()
            // Every setup and composer pill: open it, look, close it. iOS can drop a styled control's
            // identifier, so each pill also matches its spoken label. Absent pills are reported, not failed.
            let hierarchy = XCTAttachment(string: app.debugDescription)
            hierarchy.name = "smoke-new-chat-hierarchy"; hierarchy.lifetime = .keepAlways
            add(hierarchy)
            var opened: [String] = []
            let pills: [(String, NSPredicate)] = [
                ("computer", NSPredicate(format: "identifier == 'connection-picker'")),
                ("project", NSPredicate(format: "identifier == 'destination-picker'")),
                ("model", NSPredicate(format: "identifier == 'project-agent-picker' OR label == 'Model'")),
                ("permissions", NSPredicate(format: "identifier == 'new-chat-access' OR label == 'Permissions'")),
            ]
            for (name, predicate) in pills {
                let pill = app.buttons.matching(predicate).firstMatch
                guard pill.waitForExistence(timeout: 10), pill.isHittable else { continue }
                pill.tap()
                RunLoop.current.run(until: Date().addingTimeInterval(1.2))
                XCTAssertEqual(app.state, .runningForeground, "\(name) pill: the app is no longer running (crashed?)")
                opened.append(name)
                dismissMenu()
            }
            print("SMOKE_DETAIL new-chat pills=\(opened.joined(separator: ","))")
            XCTAssertFalse(opened.isEmpty, "New chat showed none of its pills")
            return nil
        }

        try step("back-to-chats") {
            openSidebarIfNeeded(app)
            needs(app.buttons["sidebar-new-chat"], "sidebar-new-chat")
            return nil
        }

        // The Chats list shows Projects collapsed; expand them one by one until a chat row appears.
        var expanded: [XCUIElement] = []
        defer {
            // Leave the owner's sidebar as it was found.
            if !expanded.isEmpty {
                openSidebarIfNeeded(app)
                for folder in expanded where folder.exists && onScreen(folder) && ((folder.value as? String) == "Expanded" || folder.label.hasPrefix("Collapse ")) { tapVisible(folder, in: app) }
            }
        }
        try step("open-chat-scroll") {
            openSidebarIfNeeded(app)
            let rows = app.descendants(matching: .any).matching(NSPredicate(
                format: "identifier BEGINSWITH 'chat-thread:' OR identifier BEGINSWITH 'project-thread:' OR identifier BEGINSWITH 'pinned-thread:'"))
            if !rows.firstMatch.waitForExistence(timeout: 10) {
                let folders = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'project-disclosure:'"))
                for index in 0..<min(folders.count, 8) {
                    let folder = folders.element(boundBy: index)
                    // The sidebar slides over the pane, so XCTest reports its rows as not hittable; tap by frame.
                    guard folder.exists, onScreen(folder), (folder.value as? String) == "Collapsed" || folder.label.hasPrefix("Expand ") else { continue }
                    tapVisible(folder, in: app)
                    expanded.append(folder)
                    if rows.firstMatch.waitForExistence(timeout: 10) { break }
                }
            }
            guard rows.firstMatch.exists else { return "no existing chat in the first \(min(folders(app), 8)) Projects" }
            // A chat whose Project folder is gone answers with an alert instead of opening; try the next one.
            let title = app.buttons["conversation-title-menu"]
            var alerts: [String] = []
            for row in rows.allElementsBoundByIndex.filter({ onScreen($0) }).prefix(6) {
                tapVisible(row, in: app)
                if title.waitForExistence(timeout: 10) { break }
                let alert = app.alerts.firstMatch
                if alert.exists {
                    alerts.append(alert.label)
                    // Only a button that declines or acknowledges; never whatever happens to be first.
                    let decline = ["OK", "Cancel", "Not Now", "Dismiss"].map { alert.buttons[$0] }.first { $0.exists }
                    XCTAssertNotNil(decline, "Unexpected alert \"\(alert.label)\" with no OK or Cancel button")
                    decline?.tap()
                }
                openSidebarIfNeeded(app)
            }
            guard title.exists else { return "no chat opened (\(alerts.count) refused: \(alerts.joined(separator: "; ")))" }
            app.swipeDown(); app.swipeDown(); app.swipeUp(); app.swipeUp()
            return nil
        }

        try step("header-menu") {
            let title = app.buttons["conversation-title-menu"]
            guard title.exists else { return "no open chat" }
            title.tap()
            RunLoop.current.run(until: Date().addingTimeInterval(1.2))
            XCTAssertEqual(app.state, .runningForeground, "header menu: the app is no longer running (crashed?)")
            dismissMenu()
            return nil
        }

        try step("settings") {
            openSidebarIfNeeded(app)
            let settings = app.buttons["sidebar-settings"]
            needs(settings, "sidebar-settings")
            settings.tap()
            let done = app.buttons["settings-done"]
            needs(done.exists ? done : app.navigationBars["Settings"], "Settings")
            RunLoop.current.run(until: Date().addingTimeInterval(1.5))
            leaveSettings(app)
            return nil
        }

        try step("computer-view") {
            // The computer opens from the chat's status pill; New chat has no entry (UX-DUO-3).
            guard app.buttons["conversation-title-menu"].exists else { return "no open chat to open the computer from" }
            let open = app.buttons["computer-status-pill"]
            guard open.waitForExistence(timeout: 10) else { return "no View computer entry in the open chat (no paired computer)" }
            open.tap()
            let pill = open
            let container = app.descendants(matching: .any).matching(identifier: "computer-session-container").firstMatch
            needs(container, "computer-session-container", 20)
            defer {
                let done = app.buttons["computer-session-done"]
                if done.exists && done.isHittable { done.tap() }
                let close = app.buttons["computer-session-close"]
                if close.exists && close.isHittable { close.tap() }
                // In a chat the pane closes from the same pill that opened it.
                if pill.exists && pill.isHittable && pill.isSelected { pill.tap() }
            }
            let takeControl = app.buttons["computer-session-take-control"]
            guard takeControl.waitForExistence(timeout: 20) else { return "computer view opened without Take control (view only)" }
            takeControl.tap()
            let active = app.descendants(matching: .any).matching(identifier: "computer-session-control-active").firstMatch
            guard active.waitForExistence(timeout: 40) else { return "control was not granted within 40s" }
            let pad = app.descendants(matching: .any).matching(identifier: "computer-session-trackpad").firstMatch
            if !pad.exists {
                // The view can open keyboard-first (no trackpad toggle while the keys are up).
                let keyboard = app.buttons["computer-session-keyboard"]
                if keyboard.exists && keyboard.isSelected && keyboard.isHittable { keyboard.tap() }
                let toggle = app.buttons["computer-session-trackpad-toggle"]
                if !pad.waitForExistence(timeout: 10) && toggle.waitForExistence(timeout: 10) { toggle.tap() }
            }
            needs(pad, "computer-session-trackpad", 10)
            // Pointer motion only: no clicks, typing or window changes.
            let center = CGPoint(x: pad.frame.midX, y: pad.frame.midY)
            let started = Date()
            while Date().timeIntervalSince(started) < 10 {
                try physicalTouchGesture("Smoke pointer motion", paths: [
                    PhysicalTouchPath(start: center, startTime: 0,
                        moves: [(CGPoint(x: center.x + 30, y: center.y), 0.5), (CGPoint(x: center.x - 30, y: center.y), 1.0),
                                (center, 1.5)], liftTime: 1.6)
                ])
                XCTAssertEqual(app.state, .runningForeground, "trackpad: the app is no longer running (crashed?)")
            }
            let done = app.buttons["computer-session-done"]
            needs(done, "computer-session-done", 10)
            done.tap()
            return nil
        }
        print("SMOKE_DONE")
    }

    private struct PhysicalTouchPath {
        let start: CGPoint
        let startTime: Double
        let moves: [(CGPoint, Double)]
        let liftTime: Double
    }

    private func physicalTouchDrag(from: XCUICoordinate, to: XCUICoordinate) throws {
        // A single record preserves the tap interval; separate public XCTest
        // calls wait for idleness and can miss the product's tap-then-hold.
        try physicalTouchGesture("Cupola title-bar drag", paths: [
            PhysicalTouchPath(start: from.screenPoint, startTime: 0, moves: [], liftTime: 0.06),
            PhysicalTouchPath(start: from.screenPoint, startTime: 0.12,
                              moves: [(from.screenPoint, 0.4), (to.screenPoint, 1.0)], liftTime: 1.05)
        ])
    }

    private func physicalTwoFingerScroll(_ preview: XCUIElement) throws {
        let center = CGPoint(x: preview.frame.midX, y: preview.frame.midY)
        let paths = [-18.0, 18.0].map { offset in
            // The terminal starts at the bottom: pull down to reveal its
            // numbered scrollback, giving both recordings a visible change.
            PhysicalTouchPath(start: CGPoint(x: center.x + offset, y: center.y - 40), startTime: 0,
                              moves: [(CGPoint(x: center.x + offset, y: center.y + 40), 0.8)], liftTime: 0.9)
        }
        try physicalTouchGesture("Cupola two-finger scroll", paths: paths)
    }

    /// XCTest exposes no public two-finger swipe on iPhone. Keep the private
    /// event bridge confined to this opt-in UI test; fail if Xcode changes it.
    /// The public pointer-wheel alternative was measured unsupported on iPhone 11.
    private func physicalTouchGesture(_ name: String, paths: [PhysicalTouchPath]) throws {
        func require(_ condition: Bool, _ message: String) throws {
            guard condition else {
                XCTFail(message)
                throw NSError(domain: "WonderPhysicalTouch", code: 1, userInfo: [NSLocalizedDescriptionKey: message])
            }
        }
        guard let recordClass = NSClassFromString("XCSynthesizedEventRecord") as? NSObject.Type,
              let pathClass = NSClassFromString("XCPointerEventPath") as? NSObject.Type else {
            try require(false, "This Xcode lacks the XCTest touch event classes")
            return
        }
        let allocate = NSSelectorFromString("alloc")
        let recordObject = recordClass.perform(allocate).takeRetainedValue() as! NSObject
        let recordInit = NSSelectorFromString("initWithName:interfaceOrientation:")
        try require(recordObject.responds(to: recordInit), "XCTest touch record initializer changed")
        typealias RecordInit = @convention(c) (AnyObject, Selector, NSString, Int) -> AnyObject
        let createRecord = unsafeBitCast(recordObject.method(for: recordInit), to: RecordInit.self)
        let record = createRecord(recordObject, recordInit, name as NSString, 1) as! NSObject
        for touch in paths {
            let pathObject = pathClass.perform(allocate).takeRetainedValue() as! NSObject
            let pathInit = NSSelectorFromString("initForTouchAtPoint:offset:")
            try require(pathObject.responds(to: pathInit), "XCTest touch path initializer changed")
            typealias PathInit = @convention(c) (AnyObject, Selector, CGPoint, Double) -> AnyObject
            let createPath = unsafeBitCast(pathObject.method(for: pathInit), to: PathInit.self)
            let path = createPath(pathObject, pathInit, touch.start, touch.startTime) as! NSObject
            let move = NSSelectorFromString("moveToPoint:atOffset:")
            let lift = NSSelectorFromString("liftUpAtOffset:")
            let add = NSSelectorFromString("addPointerEventPath:")
            try require(path.responds(to: move) && path.responds(to: lift) && record.responds(to: add),
                        "XCTest touch path methods changed")
            typealias Move = @convention(c) (AnyObject, Selector, CGPoint, Double) -> Void
            typealias Lift = @convention(c) (AnyObject, Selector, Double) -> Void
            typealias Add = @convention(c) (AnyObject, Selector, AnyObject) -> Void
            for (point, offset) in touch.moves {
                unsafeBitCast(path.method(for: move), to: Move.self)(path, move, point, offset)
            }
            unsafeBitCast(path.method(for: lift), to: Lift.self)(path, lift, touch.liftTime)
            unsafeBitCast(record.method(for: add), to: Add.self)(record, add, path)
        }
        let getter = NSSelectorFromString("eventSynthesizer")
        try require(XCUIDevice.shared.responds(to: getter), "XCTest device synthesizer changed")
        let synthesizer = XCUIDevice.shared.perform(getter).takeUnretainedValue() as! NSObject
        let send = NSSelectorFromString("synthesizeEvent:completion:")
        try require(synthesizer.responds(to: send), "XCTest event synthesis method changed")
        let finished = expectation(description: name + " synthesized")
        var succeeded = false
        var failure: NSError?
        let completion: @convention(block) (Bool, NSError?) -> Void = { success, error in
            succeeded = success; failure = error; finished.fulfill()
        }
        typealias Send = @convention(c) (AnyObject, Selector, AnyObject, @convention(block) (Bool, NSError?) -> Void) -> Void
        unsafeBitCast(synthesizer.method(for: send), to: Send.self)(synthesizer, send, record, completion)
        XCTAssertEqual(XCTWaiter.wait(for: [finished], timeout: 15), .completed)
        XCTAssertTrue(succeeded, failure?.localizedDescription ?? "XCTest rejected the touch event")
    }

    private func physicalHostCheckpoint(_ app: XCUIApplication, _ check: String, detail: String, status: String = "passed") {
        let row = ["check": check, "status": status, "detail": detail, "time": String(Date().timeIntervalSince1970)]
        let data = try! JSONSerialization.data(withJSONObject: row, options: [.sortedKeys])
        print("PHYSICAL_CHECK " + String(decoding: data, as: UTF8.self))
        XCTContext.runActivity(named: "Physical host: " + check) { activity in
            let capture = XCTAttachment(screenshot: app.screenshot())
            capture.name = "physical-host-" + check
            capture.lifetime = .keepAlways
            activity.add(capture)
            let tree = XCTAttachment(string: app.debugDescription)
            tree.name = "physical-host-" + check + "-hierarchy"
            tree.lifetime = .keepAlways
            activity.add(tree)
        }
    }

    func testPhysicalComputerControlExternalRevocationLeavesViewOnly() throws {
        continueAfterFailure = false
        guard let qaRowID = ProcessInfo.processInfo.environment["WONDER_PAIRING_QA_CONVERSATION_ID"],
              isThreadRowIdentifier(qaRowID) else {
            throw XCTSkip("Supply WONDER_PAIRING_QA_CONVERSATION_ID with the exact dedicated QA thread-row accessibility identifier.")
        }
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        let localNetworkMonitor = installWonderLocalNetworkPermissionMonitor()
        defer { removeUIInterruptionMonitor(localNetworkMonitor) }
        app.launch()

        openSidebarIfNeeded(app)
        let row = app.buttons.matching(identifier: qaRowID).firstMatch
        guard row.waitForExistence(timeout: 20) else {
            throw XCTSkip("Requires the explicitly selected QA chat and a paired, unlocked physical device.")
        }
        row.tap()
        let viewComputer = app.buttons["computer-status-pill"]
        XCTAssertTrue(viewComputer.waitForExistence(timeout: 15))
        viewComputer.tap()

        let computerView = app.descendants(matching: .any)
            .matching(identifier: "computer-session-container").firstMatch
        XCTAssertTrue(computerView.waitForExistence(timeout: 15))
        defer {
            let done = app.buttons["computer-session-done"]
            if done.exists {
                done.tap()
                _ = app.buttons["computer-session-take-control"].waitForExistence(timeout: 10)
            }
            let close = app.buttons["computer-session-close"]
            if close.exists {
                close.tap()
                _ = computerView.waitForExistence(timeout: 10)
            }
        }

        let preview = app.descendants(matching: .any)
            .matching(identifier: "computer-session-preview").firstMatch
        let live = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "value == %@", "Live"),
            object: preview
        )
        XCTAssertEqual(XCTWaiter.wait(for: [live], timeout: 90), .completed,
                       "The authenticated local WebRTC stream never became live.")

        let takeControl = app.buttons["computer-session-take-control"]
        XCTAssertTrue(takeControl.waitForExistence(timeout: 10))
        takeControl.tap()
        let waiting = app.descendants(matching: .any)
            .matching(identifier: "computer-session-waiting").firstMatch
        let active = app.descendants(matching: .any)
            .matching(identifier: "computer-session-control-active").firstMatch

        // Persistent paired-device authorization can make control active before
        // the transient starting state is sampled. Accept either state first,
        // then require the active state within the same overall timeout.
        let controlStartTimeout: TimeInterval = 125
        let controlStartDeadline = Date().addingTimeInterval(controlStartTimeout)
        let enteredStartingOrActive = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in waiting.exists || active.exists },
            object: nil
        )
        XCTAssertEqual(
            XCTWaiter.wait(for: [enteredStartingOrActive], timeout: controlStartTimeout),
            .completed,
            "Control never reached a starting or active state."
        )
        XCTAssertTrue(
            active.waitForExistence(timeout: max(0, controlStartDeadline.timeIntervalSinceNow)),
            "Control did not start on the Mac."
        )

        // This wait is the manual gesture handoff. The coordinator then presses
        // Stop control on the Mac; the phone must return to view-only. The test
        // injects no pointer or keyboard input during the handoff.
        let externallyRevoked = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in
                takeControl.exists && !active.exists
            },
            object: nil
        )
        XCTAssertEqual(
            XCTWaiter.wait(for: [externallyRevoked], timeout: 180),
            .completed,
            "The phone did not leave active control after the external Mac revocation."
        )
        XCTAssertTrue(takeControl.exists)
        XCTAssertFalse(active.exists)
    }

    private func installWonderLocalNetworkPermissionMonitor() -> NSObjectProtocol {
        addUIInterruptionMonitor(withDescription: "Cupola Local Network permission") { alert in
            let mentionsLocalNetwork = alert.label.localizedCaseInsensitiveContains("local network")
                || alert.staticTexts.matching(
                    NSPredicate(format: "label CONTAINS[c] %@", "local network")
                ).firstMatch.exists
            guard mentionsLocalNetwork else { return false }

            // Do not accept arbitrary system prompts. The only supported
            // permission transition in this test is the explicit Allow action
            // on Cupola's Local Network prompt.
            let allow = alert.buttons["Allow"]
            guard allow.exists else { return false }
            allow.tap()
            return true
        }
    }

    private func retainMenuScreenshot(_ app: XCUIApplication, name: String, fullScreen: Bool = false) {
        let attachment = XCTAttachment(screenshot: fullScreen ? XCUIScreen.main.screenshot() : app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testWorkspaceHiddenRefreshCannotReplaceNavigatedProjectFolder() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-read-preview", "-send-preview", "-files-preview", "-workspace-delayed-hidden"]
        app.launch()
        let files = app.buttons["conversation-files-pill"]
        XCTAssertTrue(files.waitForExistence(timeout: 10))
        files.tap()
        let projectFolder = app.buttons["workspace-directory-entry:Projects"]
        XCTAssertTrue(projectFolder.waitForExistence(timeout: 10))
        let hiddenToggle = app.buttons["workspace-hidden-toggle"]
        hiddenToggle.tap()
        XCTAssertEqual(hiddenToggle.value as? String, "On")
        // The app process keeps running while XCTest waits; give the delayed
        // fixture request a turn before changing the browser location.
        Thread.sleep(forTimeInterval: 0.25)
        XCTAssertFalse(app.descendants(matching: .any)["workspace-refreshing"].exists,
                       "Background refresh must not interrupt browsing with a spinner")
        projectFolder.tap()
        let plan = app.buttons["workspace-file-entry:Projects/Plan.md"]
        XCTAssertTrue(plan.waitForExistence(timeout: 10))
        Thread.sleep(forTimeInterval: 3.5)
        XCTAssertTrue(plan.exists, "The late root response must not replace the Project folder")
        XCTAssertFalse(app.buttons["workspace-file-entry:.gitignore"].exists,
                       "A hidden file from the previous folder must not appear in Projects")
        XCTAssertFalse(app.descendants(matching: .any)["workspace-unavailable"].exists)
    }

    func testWorkspaceGitAndDiffRepliesStayWithCurrentFilesView() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-read-preview", "-send-preview", "-files-preview", "-workspace-delayed-git"]
        app.launch()
        let files = app.buttons["conversation-files-pill"]
        XCTAssertTrue(files.waitForExistence(timeout: 10))
        files.tap()
        XCTAssertTrue(app.buttons["workspace-directory-entry:Projects"].waitForExistence(timeout: 10))
        app.buttons["Modified"].tap()
        XCTAssertTrue(app.staticTexts["Checking changes…"].waitForExistence(timeout: 10))
        app.buttons["workspace-view-toggle"].tap()
        XCTAssertTrue(app.buttons["workspace-directory-entry:Projects"].waitForExistence(timeout: 10))
        Thread.sleep(forTimeInterval: 3.5)
        XCTAssertTrue(app.buttons["workspace-directory-entry:Projects"].exists)
        XCTAssertFalse(app.descendants(matching: .any)["workspace-unavailable"].exists,
                       "The delayed Git failure belongs to Modified, not All files")

        app.terminate()
        app.launchArguments = ["-read-preview", "-send-preview", "-files-preview", "-workspace-delayed-diff"]
        app.launch()
        XCTAssertTrue(files.waitForExistence(timeout: 10))
        files.tap()
        app.buttons["Modified"].tap()
        let readme = app.buttons["workspace-modified-entry:README.md"]
        let renamed = app.buttons["workspace-modified-entry:new-name.md"]
        XCTAssertTrue(readme.waitForExistence(timeout: 10))
        readme.tap()
        XCTAssertTrue(app.descendants(matching: .any)["workspace-diff-loading"].waitForExistence(timeout: 10),
                      "A slow diff should show that Files is opening it")
        renamed.tap()
        XCTAssertTrue(workspacePreviewClose(app, legacyID: "workspace-diff-close").waitForExistence(timeout: 10))
        let diffText = anyElement(app, identifier: "workspace-diff-text")
        XCTAssertTrue(diffText.waitForExistence(timeout: 10))
        XCTAssertTrue((diffText.label).contains("new-name.md"))
        Thread.sleep(forTimeInterval: 3.5)
        XCTAssertTrue(diffText.label.contains("new-name.md"),
                      "A late README diff must not replace the currently selected diff")
        XCTAssertFalse(diffText.label.contains("README.md"))
    }

    func testProjectFilesClearRevokedRootsAfterInitialLoad() throws {
        verifyProjectFilesRevocation(argument: "-diagnostics-project-files-revoked-roots", modified: false)
    }

    private func verifyProjectFilesRevocation(argument: String, modified: Bool) {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-diagnostics-chat-layout", "-diagnostics-chat-layout-unsaved",
                               "-diagnostics-project-files-send", "-diagnostics-project-files-refresh",
                               argument, "-diagnostics-usage-fixture"]
        app.launch()
        openSidebarIfNeeded(app)
        let thread = app.buttons["pinned-thread:codex:read-fixture"]
        XCTAssertTrue(thread.waitForExistence(timeout: 15))
        thread.tap()
        let files = app.buttons["conversation-files-pill"]
        XCTAssertTrue(files.waitForExistence(timeout: 10))
        files.tap()
        let oldFile = app.buttons["workspace-file-entry:README.md"]
        XCTAssertTrue(oldFile.waitForExistence(timeout: 10))
        let draft = app.textViews["message-draft"]
        draft.tap(); draft.typeText("Review this file while Files stays open.")
        app.buttons["send-message"].tap()
        if modified {
            XCTAssertTrue(app.buttons["workspace-file-entry:Project update.md"].waitForExistence(timeout: 15),
                          "The message must be accepted before Git access is revoked")
            app.buttons["Modified"].tap()
        }
        let unavailable = anyElement(app, identifier: "workspace-unavailable")
        XCTAssertTrue(unavailable.waitForExistence(timeout: 15),
                      "A denied workspace refresh must hide the previously authorized listing")
        XCTAssertFalse(oldFile.exists)
        XCTAssertFalse(app.staticTexts["No modified files"].exists)
        app.buttons[modified ? "Workspace" : "Modified"].tap()
        XCTAssertTrue(unavailable.exists, "Switching views must not resurrect revoked rows")
        XCTAssertFalse(oldFile.exists)
        closeWorkspace(app)
        XCTAssertTrue(app.images["Received by your computer"].waitForExistence(timeout: 10),
                      "The fixture must revoke only after accepting the exact message")
        if !modified { retainMenuScreenshot(app, name: "Sent message in view after closing Files") }
    }

    func testComposerEditWithLargeAttachmentSurvivesDurableReload() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-diagnostics-fixtures", "-diagnostics-composer-large-draft"]
        app.launch()
        let editor = app.textViews["message-draft"]
        XCTAssertTrue(editor.waitForExistence(timeout: 10))
        XCTAssertEqual(app.staticTexts["camera-draft-count"].label, "Draft attachments: 1")
        editor.tap()
        editor.typeText(" More text")
        let changed = try XCTUnwrap(editor.value as? String)
        XCTAssertNotEqual(changed, "Keep this draft.")
        app.buttons["camera-reload-draft"].tap()
        XCTAssertEqual(editor.value as? String, changed)
        XCTAssertEqual(app.staticTexts["camera-draft-count"].label, "Draft attachments: 1")
        XCTAssertEqual(app.staticTexts["camera-pending-send"].label, "No pending send")
        retainMenuScreenshot(app, name: "Large attachment draft after durable reload")
    }

    private func anyElement(_ app: XCUIApplication, identifier: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: identifier).firstMatch
    }

    private func closeWorkspace(_ app: XCUIApplication) {
        let conversationToggle = app.buttons["conversation-files-pill"]
        let sheetDone = app.buttons["workspace-sheet-done"]
        let toggle = conversationToggle.exists ? conversationToggle : sheetDone
        XCTAssertTrue(toggle.waitForExistence(timeout: 5) && toggle.isHittable,
                      "The bottom Files control must close the workspace")
        toggle.tap()
    }

    private func tapVisible(_ element: XCUIElement, in app: XCUIApplication) {
        let window = app.windows.firstMatch
        let frame = element.frame
        guard frame.width > 0, frame.height > 0, window.frame.contains(frame) else {
            XCTFail("The selected control must be fully on screen")
            return
        }
        // The moved SwiftUI card can make XCTest's hit-point query disagree
        // with the rendered bounds. Send the touch inside the selected control;
        // callers must still assert the resulting screen state.
        window.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: frame.midX - window.frame.minX, dy: frame.midY - window.frame.minY)).tap()
    }

    private func openSidebarIfNeeded(_ app: XCUIApplication) {
        // iOS 26.5 can omit the identifier on a styled TextField. The unique
        // native placeholder still identifies this same search control.
        let search = app.textFields.matching(NSPredicate(format:
            "identifier == %@ OR placeholderValue == %@", "sidebar-search", "Search chats and projects")).firstMatch
        func isVisible() -> Bool {
            search.exists && search.frame.width > 0 && app.windows.firstMatch.frame.contains(search.frame)
        }
        if isVisible() { return }
        // A simulator view (or its list) restored from an earlier run covers the whole app; close it first.
        let closeSimulator = app.buttons["simulator-close"]
        if closeSimulator.exists { closeSimulator.tap(); _ = closeSimulator.waitForNonExistence(timeout: 10) }
        if isVisible() { return }
        let open = app.buttons["open-sidebar"]
        if open.waitForExistence(timeout: 2) {
            for _ in 0..<2 {
                let banner = XCUIApplication(bundleIdentifier: "com.apple.springboard").descendants(matching: .any)
                    .matching(identifier: "NotificationShortLookView").firstMatch
                if banner.exists { banner.swipeUp() }
                let ready = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                    open.exists && open.frame.width > 0 && app.windows.firstMatch.frame.contains(open.frame)
                }, object: nil)
                XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: 5), .completed)
                tapVisible(open, in: app)
                let visible = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in isVisible() }, object: nil)
                if XCTWaiter.wait(for: [visible], timeout: 5) == .completed { return }
            }
            XCTFail("The sidebar did not become visible")
        }
    }

    private func leaveSettings(_ app: XCUIApplication) {
        if app.navigationBars["Diagnostics"].exists { app.navigationBars.buttons["Settings"].tap() }
        let done = app.buttons["settings-done"]
        if done.exists { done.tap() }
        else {
            let back = app.navigationBars["Settings"].buttons.firstMatch
            XCTAssertTrue(back.waitForExistence(timeout: 5), "Pushed Settings must offer a native Back button")
            back.tap()
        }
    }

    private func isThreadRowIdentifier(_ identifier: String) -> Bool {
        ["project-thread:", "pinned-thread:"].contains { identifier.hasPrefix($0) && identifier.count > $0.count }
    }

    private func waitUntilGone(_ element: XCUIElement, timeout: TimeInterval) -> Bool {
        let gone = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in !element.exists }, object: nil)
        return XCTWaiter.wait(for: [gone], timeout: timeout) == .completed
    }

    func testPairForLiveRun() throws {
        guard let link = ProcessInfo.processInfo.environment["WONDER_PAIRING_LINK"], !link.isEmpty else {
            throw XCTSkip("No explicit pairing offer supplied.")
        }
        try pairForLiveRun(link: link, address: nil, code: nil)
    }

    func testPairWithCodeForLiveRun() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let address = environment["WONDER_PAIRING_ADDRESS"], !address.isEmpty,
              let code = environment["WONDER_PAIRING_CODE"], !code.isEmpty else {
            throw XCTSkip("No explicit computer pairing address and code supplied.")
        }
        try pairForLiveRun(link: nil, address: address, code: code)
    }

    private func pairForLiveRun(link: String?, address: String?, code: String?) throws {
        continueAfterFailure = false
        let hostID = try XCTUnwrap(ProcessInfo.processInfo.environment["WONDER_PAIRING_HOST_ID"],
                                   "Supply the hostInstallationId from cupola --json status.")
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        let monitor = installWonderLocalNetworkPermissionMonitor()
        defer { removeUIInterruptionMonitor(monitor) }
        app.launchArguments = ["-show-connections"]
        app.launch()
        let add = app.buttons["settings-add-computer"]
        let settingsReady = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "exists == true AND enabled == true"), object: add)
        XCTAssertEqual(XCTWaiter.wait(for: [settingsReady], timeout: 20), .completed,
                       "Saved computers must finish loading before checking for a previous pairing.")
        let computer = app.descendants(matching: .any)["settings-computer:" + hostID].firstMatch
        if computer.exists { retainMenuScreenshot(app, name: "Previous pairing refused — " + hostID) }
        XCTAssertFalse(computer.exists,
                       "This computer is already saved. Use a fresh fixture host; never delete its chats or drafts to rerun pairing.")
        add.tap()
        let field: XCUIElement
        if let link {
            field = app.textFields["pairing-link"]
            XCTAssertTrue(field.waitForExistence(timeout: 10))
            field.tap(); field.typeText(link)
            XCTAssertEqual(field.value as? String, link, "Pairing link input must arrive intact.")
        } else {
            app.buttons["pairing-entry-mode"].tap()
            let addressField = app.textFields["pairing-mac-address"]
            field = app.textFields["pairing-code"]
            XCTAssertTrue(addressField.waitForExistence(timeout: 10))
            addressField.tap(); addressField.typeText(try XCTUnwrap(address))
            field.tap(); field.typeText(try XCTUnwrap(code))
            XCTAssertEqual(addressField.value as? String, address)
            XCTAssertEqual(field.value as? String, code)
        }
        let connect = app.buttons[link == nil ? "pairing-connect-code" : "pairing-connect-link"]
        let keyboard = app.keyboards.firstMatch
        let visibleConnect = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            guard connect.exists, connect.isEnabled, connect.isHittable else { return false }
            let frame = connect.frame
            return frame.minY >= app.frame.minY && frame.maxY <= app.frame.maxY
                && (!keyboard.exists || frame.maxY <= keyboard.frame.minY)
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [visibleConnect], timeout: 10), .completed,
                       "Wait for the Connect button to move above the software keyboard.")
        retainMenuScreenshot(app, name: "Pairing entry ready above the keyboard")
        // On the USB iPhone XCTest used a stale pre-keyboard accessibility hit
        // point (y=836) while the button was at y=533. Use its verified frame.
        connect.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        // --yes can approve before XCTest observes the brief verification view.
        // Only the exact saved computer row after the sheet closes proves that
        // the phone completed enrollment; a successful tap/dismissal is insufficient.
        let completed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            computer.exists && add.isHittable && !field.exists && !app.buttons["Stop pairing"].exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [completed], timeout: 70), .completed,
                       "App did not save the expected computer. \(app.debugDescription)")
        XCTAssertTrue(computer.isHittable)
        retainMenuScreenshot(app, name: "Pairing completed — saved computer " + hostID)
        // Relaunch proves the saved computer survived the pairing presentation.
        app.terminate()
        app.launch()
        XCTAssertTrue(app.descendants(matching: .any)["settings-computer:" + hostID].firstMatch.waitForExistence(timeout: 20))
        retainMenuScreenshot(app, name: "Pairing persisted after relaunch — " + hostID)
    }

    // Exercise the real App/WindowGroup boundary. A view-only fixture cannot
    // catch a root builder invoked by SwiftUI's asynchronous renderer.
    func testRootSceneSurvivesRepeatedLaunchAndForeground() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-connections-preview", "-show-connections"]
        for _ in 0..<10 {
            app.launch()
            XCTAssertTrue(app.buttons["settings-add-computer"].waitForExistence(timeout: 15))
            for _ in 0..<2 {
                leaveSettings(app)
                XCUIDevice.shared.press(.home)
                app.activate()
                openSidebarIfNeeded(app)
                XCTAssertTrue(app.buttons["sidebar-settings"].waitForExistence(timeout: 10))
                app.buttons["sidebar-settings"].tap()
                XCTAssertTrue(app.buttons["settings-add-computer"].waitForExistence(timeout: 10))
                XCTAssertEqual(app.state, .runningForeground)
            }
            app.terminate()
        }
    }

    func testPairedConnectionSurvivesRelaunchAndForeground() throws {
        continueAfterFailure = false
        guard let host = ProcessInfo.processInfo.environment["WONDER_PAIRING_HOST_NAME"] else {
            throw XCTSkip("An explicit paired host is required.")
        }
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-show-connections"]
        for pass in 0..<10 {
            app.launch()
            let computer = app.buttons[host]
            XCTAssertTrue(computer.waitForExistence(timeout: 15))
            computer.tap()
            let check = app.buttons["Check connection"]
            XCTAssertTrue(check.waitForExistence(timeout: 10))
            check.tap()
            let connected = app.descendants(matching: .any).matching(NSPredicate(format: "identifier == %@ AND label == %@", "connection-state", "Connected")).firstMatch
            if !connected.waitForExistence(timeout: 25) {
                retainMenuScreenshot(app, name: "Connection failure")
                XCTFail("Connection failed: \(app.staticTexts["connection-status"].label)")
            }
            XCTAssertFalse(app.buttons["Pair again"].exists)
            for _ in 0..<2 {
                XCUIDevice.shared.press(.home)
                app.activate()
                XCTAssertTrue(connected.waitForExistence(timeout: 25))
                XCTAssertTrue(check.isEnabled)
                XCTAssertFalse(app.buttons["Pair again"].exists)
            }
            if pass == 9 { retainMenuScreenshot(app, name: "Paired connection after ten launches and twenty resumes") }
            app.terminate()
        }
    }

}

/// The scenario owns a synthetic catalog and one real, continuous finger per
/// drag. AXe's separate down/swipe submissions cannot represent that gesture.
@MainActor final class SidebarReorderUITests: XCTestCase {
    private let pin = "pinned-thread:codex:read-fixture"
    private let secondPin = "pinned-thread:codex:reorder-pin"
    private let sprint = "codex:0b5e2a10-7c4d-4e1f-9a2b-3c4d5e6f7a80"
    private let copy = "claude:7d1c9e42-5b3a-4f60-8e21-0a9b8c7d6e54"
    private let base = ["-diagnostics-subagent-fixture", "-diagnostics-chat-layout",
                        "-diagnostics-chat-layout-unsaved", "-diagnostics-project-actions",
                        "-diagnostics-sidebar-reorder"]

    override func record(_ issue: XCTIssue) {
        var issue = issue
        let screen = XCTAttachment(screenshot: XCUIApplication().screenshot())
        screen.name = "Sidebar reorder failure"; screen.lifetime = .keepAlways
        issue.add(screen)
        super.record(issue)
    }

    func testLongPressOrderSurvivesRelaunch() throws {
        continueAfterFailure = false
        executionTimeAllowance = 240
        let app = XCUIApplication()
        app.terminate()
        XCUIDevice.shared.orientation = .portrait
        try launch(app, extra: ["-diagnostics-sidebar-reorder-reset"])
        ordered(app, [pin, secondPin])
        ordered(app, ["project-thread:" + sprint, "project-thread:" + copy])
        capture(app, "Before drag")
        // A fast vertical pan is scrolling, never a reorder.
        let panTop = element(app, pin).coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.2))
        let panBottom = element(app, pin).coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.8))
        panBottom.press(forDuration: 0.05, thenDragTo: panTop)
        panTop.press(forDuration: 0.05, thenDragTo: panBottom)
        ordered(app, [pin, secondPin])
        XCTAssertFalse(app.buttons["threads-select-all"].exists, "A scrolling pan must not enter chat selection")
        drag(app, pin, onto: secondPin)
        ordered(app, [secondPin, pin])
        drag(app, pin, onto: secondPin, after: false)
        ordered(app, [pin, secondPin])
        drag(app, pin, onto: secondPin)
        ordered(app, [secondPin, pin])
        drag(app, "project-thread:" + sprint, onto: "project-thread:" + copy)
        ordered(app, ["project-thread:" + copy, "project-thread:" + sprint])
        capture(app, "Pinned and ordinary threads moved")
        drag(app, "project-row:read-project", onto: "project-row:reorder-project")
        ordered(app, ["project-row:reorder-project", "project-row:read-project",
                      "project-thread:" + copy, "project-thread:" + sprint])
        capture(app, "Expanded Project moved with its chats")
        // Invalid section changes cannot pin, attach or move the source.
        drag(app, pin, onto: "project-row:read-project")
        ordered(app, [secondPin, pin])
        ordered(app, ["project-row:reorder-project", "project-row:read-project"])
        try launch(app)
        ordered(app, [secondPin, pin])
        ordered(app, ["project-row:reorder-project", "project-row:read-project"])
        ordered(app, ["project-thread:" + copy, "project-thread:" + sprint])
        capture(app, "Order restored after relaunch and catalog reconnect")
        let chats = ["-diagnostics-sidebar-all-chats", "-diagnostics-sidebar-reorder-new"]
        try launch(app, extra: chats)
        let new = "chat-thread:codex:reorder-new"
        ordered(app, [new, "chat-thread:" + copy, "chat-thread:" + sprint])
        capture(app, "New chat first in All Chats")
        drag(app, new, onto: "chat-thread:" + copy)
        ordered(app, ["chat-thread:" + copy, new, "chat-thread:" + sprint])
        try launch(app, extra: chats)
        ordered(app, ["chat-thread:" + copy, new, "chat-thread:" + sprint])
        ordered(app, [secondPin, pin])
        capture(app, "All Chats manual order restored")
        // Tapping still opens the existing fixture; no model send occurs.
        element(app, pin).coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertTrue(app.buttons["conversation-title-menu"].waitForExistence(timeout: 20))
        let opened = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "Reply 13. Continuing the project")).firstMatch
        XCTAssertTrue(opened.waitForExistence(timeout: 20))
        capture(app, "Tap opens the existing chat")
    }

    private func launch(_ app: XCUIApplication, extra: [String] = []) throws {
        app.terminate()
        app.launchArguments = base + extra
        app.launch()
        let header = app.buttons.matching(identifier: "open-sidebar").firstMatch
        XCTAssertTrue(header.waitForExistence(timeout: 20))
        let search = app.textFields.matching(identifier: "sidebar-search").firstMatch
        if !search.isHittable {
            let visible = try XCTUnwrap(app.buttons.matching(identifier: "open-sidebar").allElementsBoundByIndex.first { $0.isHittable })
            visible.tap()
        }
        XCTAssertTrue(element(app, secondPin).waitForExistence(timeout: 20))
        visible(app, element(app, secondPin))
    }

    private func element(_ app: XCUIApplication, _ id: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: id).firstMatch
    }
    private func ordered(_ app: XCUIApplication, _ ids: [String], file: StaticString = #filePath, line: UInt = #line) {
        let rows = ids.map { element(app, $0) }
        let condition = NSPredicate { _, _ in
            rows.allSatisfy { $0.exists && !$0.frame.isEmpty }
                && zip(rows, rows.dropFirst()).allSatisfy { $0.frame.minY < $1.frame.minY }
        }
        let expectation = XCTNSPredicateExpectation(predicate: condition, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [expectation], timeout: 10), .completed, ids.joined(separator: " before "), file: file, line: line)
    }
    private func drag(_ app: XCUIApplication, _ source: String, onto target: String, after: Bool = true) {
        let from = element(app, source), to = element(app, target)
        XCTAssertTrue(from.waitForExistence(timeout: 10))
        XCTAssertTrue(to.waitForExistence(timeout: 10))
        visible(app, from); visible(app, to)
        // Freeze the endpoints before UIKit lifts and repositions the row.
        let origin = app.coordinate(withNormalizedOffset: .zero)
        let start = origin.withOffset(CGVector(dx: from.frame.midX, dy: from.frame.midY))
        let end = origin.withOffset(CGVector(dx: to.frame.maxX - 16, dy: after ? to.frame.maxY - 4 : to.frame.minY + 4))
        start.press(forDuration: 0.4, thenDragTo: end, withVelocity: .slow, thenHoldForDuration: 0.3)
    }
    private func visible(_ app: XCUIApplication, _ row: XCUIElement) {
        // SwiftUI's drag source reports isHittable=false for this visible row.
        // Require a fully visible native frame, then prove the actual touch's result.
        XCTAssertFalse(row.frame.isEmpty, row.debugDescription)
        XCTAssertTrue(app.frame.contains(row.frame), row.debugDescription)
    }
    private func capture(_ app: XCUIApplication, _ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name; attachment.lifetime = .keepAlways
        add(attachment)
    }
}

/// Offline performance baseline driven by `scripts/ios-perf-baseline` (targets: apps/ios/PERFORMANCE.md).
/// Each test attaches its own numbers as "perf-<name>" JSON next to the XCTMetrics the script reads from the xcresult.
@MainActor final class PerformanceBaselineUITests: XCTestCase {
    private let bundleID = "com.swaymun.wonder"
    /// The 300-message offline chat, open on launch; no pairing, host or model.
    private let longChat = ["-read-preview", "-scroll-performance-preview",
                            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL"]
    private let firstMessage = NSPredicate(format: "label CONTAINS %@", "Message ")

    private func attach(_ name: String, _ value: [String: Any]) {
        let data = (try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])) ?? Data()
        let attachment = XCTAttachment(string: String(decoding: data, as: UTF8.self))
        attachment.name = "perf-" + name; attachment.lifetime = .keepAlways
        add(attachment)
    }

    /// Cold launch to a responsive first frame (Apple's launch metric), fixture chat list.
    func testPerfColdLaunch() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: bundleID)
        app.launchArguments = ["-read-preview"]
        let options = XCTMeasureOptions(); options.iterationCount = Int(ProcessInfo.processInfo.environment["WONDER_PERF_RUNS"] ?? "") ?? 5
        measure(metrics: [XCTApplicationLaunchMetric(waitUntilResponsive: true), XCTMemoryMetric(application: app)], options: options) {
            app.launch()
        }
    }

    /// Cold launch until the restored long chat shows a message (wall clock, includes XCTest's query latency, so an upper bound).
    func testPerfFirstMessage() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: bundleID)
        app.launchArguments = longChat
        let runs = Int(ProcessInfo.processInfo.environment["WONDER_PERF_RUNS"] ?? "") ?? 5
        var samples: [Double] = []
        for _ in 0..<runs {
            app.terminate()
            let start = Date()
            app.launch()
            let message = app.staticTexts.matching(firstMessage).firstMatch
            XCTAssertTrue(message.waitForExistence(timeout: 30), "The long chat never showed a message")
            samples.append(Date().timeIntervalSince(start) * 1000)
        }
        attach("first-message", ["samplesMs": samples])
    }

    /// Scroll round trips through the 300-message chat: hitches, scroll duration, CPU and resident memory.
    func testPerfLongChatScroll() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: bundleID)
        app.launchArguments = longChat
        app.launch()
        let scroll = app.descendants(matching: .any).matching(identifier: "conversation-scroll").firstMatch
        XCTAssertTrue(scroll.waitForExistence(timeout: 30))
        XCTAssertTrue(app.staticTexts.matching(firstMessage).firstMatch.waitForExistence(timeout: 30))
        scroll.swipeDown(); scroll.swipeUp() // warm both directions
        let options = XCTMeasureOptions()
        options.iterationCount = Int(ProcessInfo.processInfo.environment["WONDER_PERF_RUNS"] ?? "") ?? 5
        measure(metrics: [XCTClockMetric(), XCTCPUMetric(application: app), XCTMemoryMetric(application: app),
                          XCTHitchMetric(application: app), XCTOSSignpostMetric.scrollingAndDecelerationMetric], options: options) {
            scroll.swipeDown()
            scroll.swipeUp()
        }
        XCTAssertEqual(app.state, .runningForeground)
    }
}

/// The computer view turned sideways. `wonderctl orient` only turns the Duo, so
/// these rotate the device with XCUIDevice and read the viewer's Diagnostics
/// probe (`landscape:`, `sent:`, `hardware:`). Screenshots are
/// attached to the result bundle. Offline fixtures: no paired host or model calls.
@MainActor final class ComputerLandscapeUITests: XCTestCase {
    private let bundleID = "com.swaymun.wonder"
    private let fixture = ["-diagnostics-computer-session-fixture", "-diagnostics-computer-session-available-fixture",
                           "-diagnostics-computer-reconnect"]

    override func setUp() async throws {
        continueAfterFailure = false
        XCUIDevice.shared.orientation = .portrait
    }

    override func tearDown() async throws {
        XCUIDevice.shared.orientation = .portrait
    }

    private func launch(_ extra: [String]) -> XCUIApplication {
        let app = XCUIApplication(bundleIdentifier: bundleID)
        app.terminate()
        app.launchArguments = fixture + extra
        app.launch()
        return app
    }

    private func element(_ app: XCUIApplication, _ id: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: id).firstMatch
    }

    private func probe(_ app: XCUIApplication) -> String {
        element(app, "computer-viewport-appearances").value as? String ?? ""
    }

    private func waitProbe(_ app: XCUIApplication, _ text: String, timeout: TimeInterval = 20,
                           file: StaticString = #filePath, line: UInt = #line) {
        let deadline = Date().addingTimeInterval(timeout)
        while !probe(app).contains(text) && Date() < deadline { Thread.sleep(forTimeInterval: 0.25) }
        XCTAssertTrue(probe(app).contains(text), "Probe never said \"\(text)\": \(probe(app))", file: file, line: line)
    }

    private func sent(_ app: XCUIApplication) -> Int {
        let text = probe(app)
        guard let range = text.range(of: "sent: "), let end = text[range.upperBound...].firstIndex(of: ";") else { return -1 }
        return Int(text[range.upperBound..<end]) ?? -1
    }

    private func shot(_ name: String) {
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name; shot.lifetime = .keepAlways
        add(shot)
    }

    private func requireElement(_ app: XCUIApplication, _ id: String, timeout: TimeInterval = 15,
                                file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertTrue(element(app, id).waitForExistence(timeout: timeout), "\(id) never appeared", file: file, line: line)
    }

    private func tap(_ app: XCUIApplication, _ id: String) {
        requireElement(app, id)
        element(app, id).tap()
    }

    /// iPhone: portrait, both landscape directions, the system keyboard's shortcut
    /// row, a drag on the picture, and back.
    func testPhoneLandscapeBothDirections() throws {
        try XCTSkipUnless(UIDevice.current.userInterfaceIdiom == .phone, "iPhone lane only")
        let app = launch(["-diagnostics-no-hardware-keyboard"])
        tap(app, "computer-session-take-control")
        requireElement(app, "computer-session-control-active")
        waitProbe(app, "landscape: none;")
        assertTrackpadToggle(app, "phone portrait, keyboard down")
        tap(app, "computer-session-keyboard")
        requireElement(app, "computer-session-key-escape", timeout: 10)
        assertTrackpadToggle(app, "phone portrait, keyboard up")
        shot("phone-portrait-keyboard-up")
        tap(app, "computer-session-keyboard")
        XCTAssertTrue(waitGone(app, "computer-session-key-escape"), "Hiding the keyboard must remove the shortcut row")
        shot("phone-portrait")

        XCUIDevice.shared.orientation = .landscapeLeft
        waitProbe(app, "landscape: phone;")
        requireElement(app, "computer-session-control-bar")
        assertCentredOnScreen(app, "phone landscape, keyboard down")
        assertTrackpadToggle(app, "phone landscape, keyboard down")
        shot("phone-landscape-left-controls")

        // Dragging the picture moves the pointer: more input is sent.
        let before = sent(app)
        let start = app.coordinate(withNormalizedOffset: CGVector(dx: 0.45, dy: 0.6))
        start.press(forDuration: 0.05, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.55, dy: 0.8)))
        let deadline = Date().addingTimeInterval(10)
        while sent(app) <= before && Date() < deadline { Thread.sleep(forTimeInterval: 0.25) }
        XCTAssertGreaterThan(sent(app), before, "A drag on the picture must move the pointer: \(probe(app))")

        tap(app, "computer-session-keyboard")
        for id in ["computer-session-key-escape", "computer-session-microphone"] {
            requireElement(app, id, timeout: 10)
        }
        assertCentredOnScreen(app, "phone landscape, keyboard up")
        assertTrackpadToggle(app, "phone landscape, keyboard up")
        shot("phone-landscape-left-keys")

        XCUIDevice.shared.orientation = .landscapeRight
        waitProbe(app, "landscape: phone;")
        requireElement(app, "computer-session-key-escape", timeout: 10)
        shot("phone-landscape-right-keys")

        let typed = sent(app)
        tap(app, "computer-session-key-escape")
        let typedDeadline = Date().addingTimeInterval(10)
        while sent(app) <= typed && Date() < typedDeadline { Thread.sleep(forTimeInterval: 0.25) }
        XCTAssertGreaterThan(sent(app), typed, "A key on the shortcut row must send input: \(probe(app))")

        tap(app, "computer-session-keyboard")
        XCTAssertTrue(waitGone(app, "computer-session-key-escape"), "Hiding the keyboard must remove the shortcut row")
        shot("phone-landscape-right-controls")

        XCUIDevice.shared.orientation = .portrait
        waitProbe(app, "landscape: none;")
        waitProbe(app, "appearances: 1; keyboards: 1;")
        shot("phone-back-to-portrait")
    }

    /// iPad with the experimental layout on and no hardware keyboard: portrait is
    /// unchanged, landscape shows the trackpad column beside the picture, and the
    /// shortcut row stays with the keyboard, hardware or software.
    func testPadLandscape() throws {
        try XCTSkipUnless(UIDevice.current.userInterfaceIdiom == .pad, "iPad lane only")
        let flags = ["-diagnostics-companion-computer", "-diagnostics-computer-control-active",
                     "-diagnostics-computer-keyboard", "-diagnostics-computer-ipad-layout"]
        var app = launch(flags + ["-diagnostics-no-hardware-keyboard"])
        requireElement(app, "computer-session-control-active", timeout: 30)
        waitProbe(app, "landscape: none;")
        shot("pad-portrait")

        XCUIDevice.shared.orientation = .landscapeLeft
        waitProbe(app, "landscape: pad;")
        if !element(app, "computer-session-key-escape").exists { tap(app, "computer-session-keyboard") }
        for id in ["computer-session-trackpad", "computer-session-key-escape"] {
            requireElement(app, id, timeout: 10)
        }
        waitProbe(app, "appearances: 1; keyboards: 1;")
        assertCentredOnScreen(app, "pad landscape, keyboard up")
        assertTrackpadToggle(app, "pad landscape, keyboard up")
        shot("pad-landscape-keyboard-up")

        // A hardware keyboard keeps the shortcut row.
        app = launch(flags + ["-diagnostics-hardware-keyboard"])
        waitProbe(app, "landscape: pad;")
        if !probe(app).contains("hardware: true;") { XCTFail("The hardware keyboard fixture did not apply: \(probe(app))") }
        requireElement(app, "computer-session-key-escape", timeout: 10)
        shot("pad-landscape-hardware-keyboard")

        XCUIDevice.shared.orientation = .portrait
        waitProbe(app, "landscape: none;")
        shot("pad-back-to-portrait")
    }

    /// Light and dark captures of each landscape layout with the keyboard up.
    func testLandscapeLightAndDark() throws {
        let pad = UIDevice.current.userInterfaceIdiom == .pad
        let flags = pad
            ? ["-diagnostics-companion-computer", "-diagnostics-computer-control-active", "-diagnostics-computer-keyboard",
               "-diagnostics-computer-ipad-layout", "-diagnostics-no-hardware-keyboard"]
            : ["-diagnostics-no-hardware-keyboard"]
        for theme in ["cupola-light", "cupola-dark"] {
            XCUIDevice.shared.orientation = .landscapeLeft
            let app = launch(flags + ["-diagnostics-theme", theme])
            if !pad {
                tap(app, "computer-session-take-control")
                requireElement(app, "computer-session-control-active")
            }
            waitProbe(app, pad ? "landscape: pad;" : "landscape: phone;", timeout: 30)
            if !element(app, "computer-session-key-escape").exists { tap(app, "computer-session-keyboard") }
            requireElement(app, "computer-session-key-escape", timeout: 10)
            shot("\(pad ? "pad" : "phone")-landscape-keys-\(theme)")
        }
    }

    private func waitGone(_ app: XCUIApplication, _ id: String, timeout: TimeInterval = 10) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while element(app, id).exists && Date() < deadline { Thread.sleep(forTimeInterval: 0.25) }
        return !element(app, id).exists
    }

    /// A viewer that spans the screen: the computer's title pill and the control
    /// bar are centred on it (within 1 pt), and the window buttons are the
    /// trailing end of the header (the notch and corners inset it by up to ~70 pt).
    private func assertCentredOnScreen(_ app: XCUIApplication, _ pose: String,
                                       file: StaticString = #filePath, line: UInt = #line) {
        requireElement(app, "computer-session-connection-picker", file: file, line: line)
        requireElement(app, "computer-session-window-buttons", file: file, line: line)
        requireElement(app, "computer-session-control-bar", file: file, line: line)
        let screen = app.windows.firstMatch.frame
        let title = element(app, "computer-session-connection-picker").frame
        let buttons = element(app, "computer-session-window-buttons").frame
        let bar = element(app, "computer-session-control-bar").frame
        XCTAssertEqual(title.midX, screen.midX, accuracy: 1, "\(pose): the title pill's centre is off the screen's centre (title \(title), screen \(screen))", file: file, line: line)
        XCTAssertEqual(bar.midX, screen.midX, accuracy: 1, "\(pose): the control bar's centre is off the screen's centre (bar \(bar), screen \(screen))", file: file, line: line)
        XCTAssertGreaterThanOrEqual(buttons.minX, title.maxX - 1, "\(pose): the window buttons overlap the title pill", file: file, line: line)
        XCTAssertLessThan(screen.maxX - buttons.maxX, 110, "\(pose): the window buttons are not at the trailing edge (\(buttons), screen \(screen))", file: file, line: line)
    }

    /// The trackpad toggle is on screen and takes a tap in this keyboard state.
    private func assertTrackpadToggle(_ app: XCUIApplication, _ state: String,
                                      file: StaticString = #filePath, line: UInt = #line) {
        requireElement(app, "computer-session-trackpad-toggle", timeout: 10, file: file, line: line)
        XCTAssertTrue(element(app, "computer-session-trackpad-toggle").isHittable,
                      "\(state): the trackpad toggle is not hittable", file: file, line: line)
    }
}
