import XCTest
@testable import WonderComputerUseCore

final class CaptureWindowTargetTests: XCTestCase {
    func testLockRestartsStopAfterTwoUntilTheNextLock() {
        var budget = LockRestartBudget()
        XCTAssertTrue(budget.spend())
        XCTAssertTrue(budget.spend())
        XCTAssertFalse(budget.spend(), "A third restart for one lock reports the stream as failed")
        XCTAssertFalse(budget.spend())
        budget.reset()
        XCTAssertTrue(budget.spend(), "A new lock starts a new allowance")
    }

    func testIconAndEscapedTitleEnvelopeStaysInsideTheJsonLineLimit() throws {
        let sources = (1...32).map { id in
            ["id": "app:editor#\(id)", "name": String(repeating: "\"", count: 160),
             "appName": String(repeating: "\"", count: 160), "windowTitle": String(repeating: "\"", count: 160),
             "iconPNG": String(repeating: "x", count: 3072)]
        }
        let bounded = ScreenCaptureSession.boundedWindowSources(sources)
        XCTAssertFalse(bounded.isEmpty)
        XCTAssertEqual(bounded, Array(sources.prefix(bounded.count)))
        XCTAssertLessThanOrEqual(try JSONSerialization.data(withJSONObject: ["available": true, "sources": bounded]).count, 128 * 1024 - 512)
    }

    func testAllWindowDiscoveryListsOtherAppsButNeverHiddenTinyOrSystemWindows() {
        let candidates = [
            window(1, bundle: "com.apple.TextEdit", name: "TextEdit", title: "Notes", zOrder: 0),
            window(2, bundle: "com.apple.Safari", name: "Safari", title: "Route", zOrder: 1),
            window(3, bundle: "com.apple.Safari", onScreen: false),
            window(4, bundle: "com.apple.Safari", layer: 25),
            window(5, bundle: "com.apple.Safari", width: 79, height: 59)
        ]
        let sources = ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: [], allWindows: true)
        XCTAssertEqual(sources.map { $0["id"] }, ["app:com.apple.TextEdit#1", "app:com.apple.Safari#2"])
        XCTAssertEqual(sources.first?["appName"], "TextEdit")
        XCTAssertEqual(sources.first?["windowTitle"], "Notes")
        XCTAssertTrue(ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: []).isEmpty)
    }

    func testDiscoveryExcludesSoftwareCursorOverlay() {
        let overlay = window(1, bundle: "com.openai.sky.CUAService", name: "ChatGPT Computer Use", title: "Software Cursor")
        let document = window(2, bundle: "com.apple.TextEdit", name: "TextEdit", title: "Software Cursor")
        let sources = ScreenCaptureSession.discoveredWindowSources(candidates: [overlay, document], appIDs: [], allWindows: true)
        XCTAssertEqual(sources.map { $0["id"] }, ["app:com.apple.TextEdit#2"], "Filter the overlay host, never a document merely sharing its title")
    }

    func testDiscoveryFiltersSystemAndInvisibleLayersWithoutHidingRealUntitledDocuments() {
        for (candidate, included) in [
            (window(1, bundle: "com.apple.TextEdit", title: ""), true),
            (window(1, bundle: "com.apple.TextEdit", width: 0), false),
            (window(1, bundle: "com.apple.TextEdit", opacity: 0), false),
            (window(1, bundle: "com.example.utility", title: "", regular: false), false),
            (window(1, bundle: "com.example.utility", title: "Settings", regular: false), true),
            (window(1, bundle: "com.apple.dock", title: "Desktop"), false),
            (window(1, bundle: "com.apple.WindowManager", title: "Stage Manager"), false),
            (window(1, bundle: "com.apple.controlcenter", title: "Control Center"), false)
        ] {
            XCTAssertEqual(!ScreenCaptureSession.discoveredWindowSources(candidates: [candidate], appIDs: [], allWindows: true).isEmpty,
                           included, "\(candidate)")
        }
    }

    func testCaptureCapabilityDistinguishesPermissionFromNoDisplay() {
        for (hasDisplay, permission, available, reason) in [
            (true, true, true, nil),
            (true, false, false, "screen_recording_permission_required"),
            (false, true, false, "no_display"),
            (false, false, false, "no_display")
        ] {
            let capability = ScreenCaptureSession.CaptureAvailability(hasDisplay: hasDisplay, permissionGranted: permission)
            XCTAssertEqual(capability.available, available)
            XCTAssertEqual(capability.json["available"] as? Bool, available)
            XCTAssertEqual(capability.json["reason"] as? String, reason)
            XCTAssertEqual(capability.json["liveQualityChange"] as? Bool, true)
        }
    }

    func testParsesAndRoundTripsAppTargets() throws {
        let plain = try XCTUnwrap(CaptureWindowTarget(sourceID: "app:com.apple.TextEdit"))
        XCTAssertEqual(plain.appIdentifier, "com.apple.TextEdit")
        XCTAssertNil(plain.windowNumber)
        XCTAssertEqual(plain.sourceID, "app:com.apple.TextEdit")

        let numbered = try XCTUnwrap(CaptureWindowTarget(sourceID: "app:TextEdit#4211"))
        XCTAssertEqual(numbered.appIdentifier, "TextEdit")
        XCTAssertEqual(numbered.windowNumber, 4_211)
        XCTAssertEqual(numbered.sourceID, "app:TextEdit#4211")

        let path = "/Applications/TextEdit.app/Contents/MacOS/TextEdit"
        let pathTarget = try XCTUnwrap(CaptureWindowTarget(sourceID: "app:\(path)#7"))
        XCTAssertEqual(pathTarget.appIdentifier, path)
        XCTAssertEqual(CaptureWindowTarget(sourceID: pathTarget.sourceID), pathTarget)
    }

    func testRejectsMalformedTargets() {
        let rejected = [
            "display:1", "window:42", "app:", "app:#3", "app: TextEdit", "app:Text\nEdit",
            "app:Text\u{0}Edit", "app:TextEdit#", "app:TextEdit#-1", "app:TextEdit#12a",
            "app:TextEdit#99999999999", "app:Text#Edit#3", "APP:TextEdit",
            "app:" + String(repeating: "a", count: 157),
        ]
        for sourceID in rejected {
            XCTAssertNil(CaptureWindowTarget(sourceID: sourceID), sourceID)
        }
        XCTAssertNotNil(CaptureWindowTarget(sourceID: "app:" + String(repeating: "a", count: 156)))
        XCTAssertNil(CaptureWindowTarget(appIdentifier: String(repeating: "a", count: 150), windowNumber: 1_234_567))
    }

    func testMatchesByBundleNameOrPath() {
        let candidates = [
            window(10, bundle: "com.apple.Safari", name: "Safari", zOrder: 0),
            window(20, bundle: "com.apple.TextEdit", name: "TextEdit", zOrder: 1),
        ]
        for identifier in [
            "com.apple.TextEdit", "COM.APPLE.TEXTEDIT", "TextEdit", "textedit",
            "/Applications/TextEdit.app/Contents/MacOS/TextEdit", "/System/Applications/TextEdit.app",
        ] {
            XCTAssertEqual(resolve(identifier, candidates), 20, identifier)
        }
        // A path whose executable differs from the bundle still matches the bundle name.
        let renamed = [window(30, bundle: "com.example.tool", name: "Tool Studio", zOrder: 0)]
        XCTAssertEqual(resolve("/Applications/Tool Studio.app/Contents/MacOS/tool-bin", renamed), 30)
    }

    func testExactWindowCanBeBehindAnotherWindowFromTheSameApp() {
        let candidates = [
            window(20, bundle: "com.apple.TextEdit", name: "TextEdit", title: "Front", zOrder: 0),
            window(21, bundle: "com.apple.TextEdit", name: "TextEdit", title: "Back", zOrder: 5),
        ]
        XCTAssertEqual(resolve("com.apple.TextEdit", candidates, windowNumber: 21), 21)
        // A reused number without an owning app cannot establish identity.
        XCTAssertNil(resolve("TextEdit", [window(55, zOrder: 9)], windowNumber: 55))
    }

    func testMissingClosedOrMismatchedWindowNeverSelectsAnotherWindow() {
        let candidates = [
            window(20, bundle: "com.apple.TextEdit", name: "TextEdit", zOrder: 3),
            window(99, bundle: "com.apple.Safari", name: "Safari", zOrder: 0),
        ]
        XCTAssertNil(resolve("com.apple.TextEdit", candidates, windowNumber: 99))
        XCTAssertNil(resolve("com.apple.TextEdit", candidates, windowNumber: 12_345))
        let both = candidates + [window(21, bundle: "com.apple.TextEdit", zOrder: 5)]
        XCTAssertEqual(resolve("com.apple.TextEdit", both, windowNumber: 21), 21)
        XCTAssertNil(resolve("com.apple.TextEdit", both.filter { $0.windowID != 21 }, windowNumber: 21))
    }

    func testIgnoresOffScreenNonZeroLayerAndTinyWindows() {
        let candidates = [
            window(1, bundle: "com.apple.TextEdit", zOrder: 0, onScreen: false),
            window(2, bundle: "com.apple.TextEdit", zOrder: 1, layer: 25),
            window(3, bundle: "com.apple.TextEdit", width: 79, height: 600, zOrder: 2),
            window(4, bundle: "com.apple.TextEdit", width: 800, height: 59, zOrder: 3),
            window(5, bundle: "com.apple.TextEdit", width: 80, height: 60, zOrder: 4),
        ]
        XCTAssertEqual(resolve("com.apple.TextEdit", candidates), 5)
        // The window number cannot select a filtered window either.
        XCTAssertNil(resolve("com.apple.TextEdit", candidates, windowNumber: 2))
        XCTAssertNil(resolve("com.apple.TextEdit", Array(candidates.prefix(4))))
    }

    func testTieBreaksFrontmostThenTitledThenAreaThenID() {
        let app = "com.apple.TextEdit"
        XCTAssertEqual(resolve(app, [
            window(1, bundle: app, width: 2_000, height: 2_000, zOrder: 4),
            window(2, bundle: app, width: 200, height: 200, zOrder: 1),
            window(3, bundle: app, width: 3_000, height: 3_000),
        ]), 2)
        XCTAssertEqual(resolve(app, [
            window(1, bundle: app, title: "", width: 2_000, height: 2_000),
            window(2, bundle: app, title: "Notes", width: 200, height: 200),
        ]), 2)
        XCTAssertEqual(resolve(app, [
            window(1, bundle: app, title: "A", width: 200, height: 200),
            window(2, bundle: app, title: "B", width: 900, height: 700),
        ]), 2)
        XCTAssertEqual(resolve(app, [
            window(9, bundle: app, title: "A"),
            window(4, bundle: app, title: "B"),
        ]), 4)
    }

    func testNoMatchReturnsNil() {
        let candidates = [window(10, bundle: "com.apple.Safari", name: "Safari", zOrder: 0)]
        XCTAssertNil(resolve("com.apple.TextEdit", candidates))
        XCTAssertNil(resolve("com.apple.TextEdit", candidates, windowNumber: 10))
        XCTAssertNil(resolve("Safari Technology Preview", candidates))
        XCTAssertNil(resolve("com.apple.TextEdit", []))
    }

    func testDiscoveryKeepsThe32MostRecentlyActivatedEligibleWindows() {
        let app = "com.apple.TextEdit"
        // Shareable-content and ID order disagree with activation order.
        let candidates = (1...40).map { id in
            window(UInt32(id), bundle: app, zOrder: 40 - id)
        }
        let sources = ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: [app])
        XCTAssertEqual(sources.count, 32)
        XCTAssertEqual(sources.compactMap { $0["id"] }, (9...40).reversed().map { "app:\(app)#\($0)" })
    }

    func testDiscoveryReordersOnFocusChangeAndFiltersBeforeItsCap() {
        let app = "com.apple.TextEdit"
        var candidates = (1...40).map { window(UInt32($0), bundle: app, zOrder: $0) }
        candidates += [
            window(100, bundle: "com.apple.Safari", zOrder: 0),
            window(101, bundle: app, zOrder: 0, onScreen: false),
            window(102, bundle: app, zOrder: 0, layer: 25),
            window(103, bundle: app, width: 79, zOrder: 0),
            window(0, bundle: app, zOrder: 0),
        ]
        let first = ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: [app])
        XCTAssertEqual(first.count, 32)
        XCTAssertEqual(first.last?["id"], "app:\(app)#32")
        candidates[39] = window(40, bundle: app, zOrder: 0)
        let focused = ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: [app])
        XCTAssertEqual(focused.first?["id"], "app:\(app)#40")
        XCTAssertEqual(focused.last?["id"], "app:\(app)#31")
        XCTAssertFalse(focused.contains { $0["id"] == "app:\(app)#32" })
    }

    func testDiscoveryRetainsFocusRecencyWhenAnAppRaisesItsOtherWindows() {
        let app = "com.apple.TextEdit"
        let candidates = (1...40).map { window(UInt32($0), bundle: app, zOrder: $0) }
        let history = WindowSourceFocusHistory()
        var recency: [UInt32: UInt64] = [:]
        for id in 1...32 {
            recency = history.observe(candidates: candidates, focusedWindowID: UInt32(id))
        }
        recency = history.observe(candidates: candidates, focusedWindowID: 40)
        let sources = ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: [app], focusRecency: recency)
        XCTAssertEqual(sources.compactMap { $0["id"] }, ([40] + Array((2...32).reversed())).map { "app:\(app)#\($0)" })
    }

    func testDiscoveryForgetsClosedAndReusedWindowFocusAndBoundsHistory() {
        let history = WindowSourceFocusHistory()
        let app = "com.apple.TextEdit"
        let candidates = (1...200).map { window(UInt32($0), bundle: app, zOrder: $0) }
        var recency: [UInt32: UInt64] = [:]
        for id in 1...200 { recency = history.observe(candidates: candidates, focusedWindowID: UInt32(id)) }
        XCTAssertEqual(recency.count, 128)
        XCTAssertNil(recency[1])
        recency = history.observe(candidates: Array(candidates.dropLast()), focusedWindowID: nil)
        XCTAssertNil(recency[200])
        let reused = [window(199, bundle: "com.apple.Safari", zOrder: 0)]
        XCTAssertTrue(history.observe(candidates: reused, focusedWindowID: nil).isEmpty)
    }

    func testDiscoveryKeepsSanitizedNamesAndDeterministicUnknownOrder() {
        let app = "com.apple.TextEdit"
        let candidates = [window(9, bundle: app, title: "\n" + String(repeating: "🪟", count: 100)),
                          window(4, bundle: app, title: "  ")]
        let sources = ScreenCaptureSession.discoveredWindowSources(candidates: candidates, appIDs: [app])
        XCTAssertEqual(sources.first?["id"], "app:\(app)#4")
        XCTAssertEqual(sources.first?["name"], app)
        XCTAssertLessThanOrEqual(sources.last?["name"]?.utf8.count ?? Int.max, 160)
        XCTAssertFalse(sources.last?["name"]?.contains("\n") ?? true)
    }

    private func resolve(_ identifier: String, _ candidates: [CaptureWindowCandidate], windowNumber: UInt32? = nil) -> UInt32? {
        guard let target = CaptureWindowTarget(appIdentifier: identifier, windowNumber: windowNumber) else {
            XCTFail("invalid target \(identifier)")
            return nil
        }
        XCTAssertEqual(CaptureWindowTarget(sourceID: target.sourceID), target)
        return CaptureWindowResolver.resolve(target, candidates: candidates)
    }

    private func window(
        _ id: UInt32,
        bundle: String? = nil,
        name: String? = nil,
        title: String? = "Untitled",
        width: Double = 800,
        height: Double = 600,
        zOrder: Int? = nil,
        onScreen: Bool = true,
        layer: Int = 0,
        opacity: Double = 1,
        regular: Bool = true
    ) -> CaptureWindowCandidate {
        CaptureWindowCandidate(
            windowID: id, bundleIdentifier: bundle, applicationName: name, processID: 100,
            title: title, width: width, height: height, isOnScreen: onScreen,
            windowLayer: layer, zOrder: zOrder, opacity: opacity, isRegularApplication: regular
        )
    }
}

/// Exercise the real discovery executable's framing and lifetime without
/// enumerating the desktop, starting capture, or invoking simulator services.
final class WindowDiscoveryWorkerTests: XCTestCase {
    private final class Replies: @unchecked Sendable {
        private let lock = NSLock()
        private var data = Data()
        func append(_ byte: Data) { lock.withLock { data.append(byte) } }
        func lines() -> [String] { lock.withLock { String(decoding: data, as: UTF8.self).split(separator: "\n").map(String.init) } }
    }

    func testDiscoveryWorkerDrainsQueuedRequestsWhenInputClosesImmediately() throws {
        var directory = Bundle(for: Self.self).bundleURL.deletingLastPathComponent()
        while !FileManager.default.isExecutableFile(atPath: directory.appendingPathComponent("WonderComputerUse").path),
              directory.path != "/" {
            directory.deleteLastPathComponent()
        }
        let worker = Process()
        worker.executableURL = directory.appendingPathComponent("WonderComputerUse")
        // The debug-only fixture source answers after EOF, so the real
        // desktop's window count and icon work never decide this test.
        worker.arguments = ["--discover-sources", "--fixture-window-sources"]
        let input = Pipe()
        let output = Pipe()
        worker.standardInput = input
        worker.standardOutput = output
        worker.standardError = FileHandle.nullDevice
        let exited = expectation(description: "batch worker exits on EOF")
        worker.terminationHandler = { _ in exited.fulfill() }
        try worker.run()
        defer {
            if worker.isRunning { kill(worker.processIdentifier, SIGKILL); worker.waitUntilExit() }
        }
        try input.fileHandleForWriting.write(contentsOf: Data("{\"apps\":[\"bad app\"]}\n{\"method\":\"click\"}\n{\"apps\":[],\"allWindows\":true}\n".utf8))
        try input.fileHandleForWriting.close()
        wait(for: [exited], timeout: 10)
        if worker.isRunning { kill(worker.processIdentifier, SIGKILL); worker.waitUntilExit() }
        let data = try output.fileHandleForReading.readToEnd() ?? Data()
        let replies = try String(decoding: data, as: UTF8.self).split(separator: "\n").map { line in
            try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
        }
        XCTAssertEqual(replies.compactMap { $0["error"] as? String }, ["invalid_discovery_request", "invalid_discovery_request"])
        XCTAssertEqual(replies.filter { $0["available"] is Bool }.count, 1, "EOF must wait for an asynchronous window discovery reply")
        XCTAssertEqual(worker.terminationStatus, 0)
    }

    func testDiscoveryWorkerHandlesMultipleBoundedRequestsAndExitsOnEOF() throws {
        var directory = Bundle(for: Self.self).bundleURL.deletingLastPathComponent()
        while !FileManager.default.isExecutableFile(atPath: directory.appendingPathComponent("WonderComputerUse").path),
              directory.path != "/" {
            directory.deleteLastPathComponent()
        }
        let worker = Process()
        worker.executableURL = directory.appendingPathComponent("WonderComputerUse")
        worker.arguments = ["--discover-sources"]
        let input = Pipe()
        let output = Pipe()
        worker.standardInput = input
        worker.standardOutput = output
        worker.standardError = FileHandle.nullDevice
        let exited = expectation(description: "worker exits after stdin closes")
        worker.terminationHandler = { _ in exited.fulfill() }
        try worker.run()
        defer {
            if worker.isRunning {
                kill(worker.processIdentifier, SIGKILL)
                worker.waitUntilExit()
            }
        }
        let pid = worker.processIdentifier
        let replies = Replies()
        let received = expectation(description: "one reply per framed request")
        received.expectedFulfillmentCount = 4
        DispatchQueue.global().async {
            while let byte = try? output.fileHandleForReading.read(upToCount: 1), !byte.isEmpty {
                replies.append(byte)
                if byte == Data([0x0A]) { received.fulfill() }
            }
        }
        let requests = [
            "{\"apps\":[\"bad app\"]}\n",
            "{\"method\":\"click\",\"params\":{}}\n",
            String(repeating: "x", count: 64 * 1024 + 1) + "\n",
            "{\"apps\":[\"\"]}\r\n",
        ]
        for request in requests { try input.fileHandleForWriting.write(contentsOf: Data(request.utf8)) }
        wait(for: [received], timeout: 10)
        XCTAssertTrue(worker.isRunning)
        XCTAssertEqual(worker.processIdentifier, pid)
        let errors = try replies.lines().map { line in
            try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: String])["error"]
        }
        XCTAssertEqual(errors, ["invalid_discovery_request", "invalid_discovery_request", "invalid_request", "invalid_discovery_request"])
        try input.fileHandleForWriting.close()
        wait(for: [exited], timeout: 3)
        XCTAssertFalse(worker.isRunning)
        XCTAssertEqual(worker.terminationStatus, 0)
    }
}
