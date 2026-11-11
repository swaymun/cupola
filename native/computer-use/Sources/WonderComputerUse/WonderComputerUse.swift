import AppKit
@preconcurrency import ApplicationServices
import CoreGraphics
import Darwin
import Foundation
import ImageIO
import UniformTypeIdentifiers
import WonderComputerUseCore

private let maxTextLength = 4_096
private let maxScreenshotBytes = 16 * 1024 * 1024
private let maxRequestBytes = 64 * 1024
private let stdinReadSize = 4 * 1024
private let controlLeaseLifetime: TimeInterval = 10

// Local Stop is exposed in Wonder's Settings (Access → Remote control). The
// helper owns the active lease and held inputs; the notification can only
// revoke control. Lease start/end is announced so Settings can enable Stop.
private final class ControlSurfaceController: NSObject, @unchecked Sendable {
    private var stopHandler: (() -> Void)?

    override init() {
        super.init()
        let center = DistributedNotificationCenter.default()
        center.addObserver(
            self,
            selector: #selector(stopControl),
            name: ControlSessionSignal.stop,
            object: nil,
            suspensionBehavior: .deliverImmediately
        )
        center.addObserver(
            self,
            selector: #selector(reportState),
            name: ControlSessionSignal.stateRequest,
            object: nil,
            suspensionBehavior: .deliverImmediately
        )
    }

    deinit { DistributedNotificationCenter.default().removeObserver(self) }

    @MainActor
    func show(stopHandler: @escaping () -> Void) {
        self.stopHandler = stopHandler
        post(active: true)
    }

    @MainActor
    func hide() {
        guard stopHandler != nil else { return }
        stopHandler = nil
        post(active: false)
    }

    @MainActor
    private func post(active: Bool) {
        DistributedNotificationCenter.default().postNotificationName(
            ControlSessionSignal.state,
            object: active ? ControlSessionSignal.active : ControlSessionSignal.inactive,
            userInfo: nil,
            deliverImmediately: true
        )
    }

    @objc private func stopControl() {
        Task { @MainActor [weak self] in self?.stopHandler?() }
    }

    @objc private func reportState() {
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.post(active: self.stopHandler != nil)
        }
    }
}

@main
struct WonderComputerUse {
    private final class HeldInputState: @unchecked Sendable {
        var mouseButtons: Set<String> = []
        var keyboard = ControlKeyboardState()
        var clicks = ControlClickCounter()
    }

    private static let publisher: WebRTCPublisher = WebRTCPublisher { object in
        write(object)
        // A still Simulator sends no new frames, and the one sent while the
        // phone was still connecting went nowhere: give the new link a picture.
        if object["event"] as? String == "signal.peerState",
           ["connected", "completed"].contains(object["state"] as? String) {
            captureSession.redrawSimulatorFrame()
        }
    }

    private static let captureSession: ScreenCaptureSession = ScreenCaptureSession(eventSink: { event in
        guard let object = jsonObject(event) else { return }
        write(object)
    }, frameSink: { frame in
        publisher.push(frame)
    })

    private static let controlGate = ControlLeaseGate(
        consentProvider: { identity in requestLocalControlConsent(identity) },
        enabledProvider: { controlPreferenceEnabled() }
    )
    private static let clipboardWatch = ComputerClipboardWatch()
    private static let manualClipboard = ComputerClipboardManualCopy()
    private static let heldInput = HeldInputState()
    private static let simulatorInput = SimulatorInputDriver()
    private static let controlSurface = ControlSurfaceController()
    @MainActor private static var pendingControlInput: Task<Void, Never>?
    private static let textFocusQueue = DispatchQueue(label: "com.wonder.computer-use.text-focus", qos: .userInitiated)
    // Accessed only on textFocusQueue.
    nonisolated(unsafe) private static var textFocusChanges = TextFocusChanges()
    nonisolated(unsafe) private static var textFocusTimer: DispatchSourceTimer?

    @MainActor static func main() {
        // Only the Cupola host may start this helper: it holds the Screen
        // Recording and Accessibility grants. Debug builds, and release builds
        // made with `-Xswiftc -DCUPOLA_DEV_HELPER` for scripts that drive the
        // binary from a terminal, are never shipped and skip the check.
        #if !DEBUG && !CUPOLA_DEV_HELPER
        guard HelperCaller.currentProcessIsHostLaunched() else {
            FileHandle.standardError.write(Data("This helper only runs inside Cupola.\n".utf8))
            exit(EXIT_FAILURE)
        }
        #endif
        // A permission-only helper never reads screen content or accepts input actions.
        // Relaunching this same executable avoids caching grants across helper updates.
        if CommandLine.arguments.contains("--permissions") {
            if CommandLine.arguments.contains("--request-screen") {
                _ = CGRequestScreenCaptureAccess()
            }
            if CommandLine.arguments.contains("--request-input") {
                let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
                _ = AXIsProcessTrustedWithOptions(options as CFDictionary)
            }
            write(["screenRecording": CGPreflightScreenCaptureAccess(),
                   "accessibility": AXIsProcessTrusted()])
            exit(EXIT_SUCCESS)
        }
        // Reports, never prompts: the phone explains a permission before Wonder asks for it.
        if let index = CommandLine.arguments.firstIndex(of: "--automation"), index + 1 < CommandLine.arguments.count {
            let bundle = CommandLine.arguments[index + 1]
            // `--ask` shows the macOS prompt; only the owner's explicit request passes it.
            let ask = CommandLine.arguments.contains("--ask")
            write(["automation": AutomationPermission.check(bundleIdentifier: bundle, askIfNeeded: ask).rawValue])
            exit(EXIT_SUCCESS)
        }
        // Lists macOS dialogs waiting for the person at the Mac whenever the list
        // changes, until standard input closes. Observation only.
        if CommandLine.arguments.contains("--watch-dialogs") {
            watchSystemDialogs()
            return
        }
        if let index = CommandLine.arguments.firstIndex(of: "--raise-dialog"),
           index + 2 < CommandLine.arguments.count,
           let windowID = UInt32(CommandLine.arguments[index + 1]), windowID > 0 {
            let result = SystemDialogWindowRaiser.raise(windowID: windowID, owner: CommandLine.arguments[index + 2])
            write(["state": result.rawValue, "windowId": windowID])
            exit(EXIT_SUCCESS)
        }
        // A discovery-only worker is reused by wonderd. It accepts bounded
        // JSONL queries until stdin closes, never capture or input commands.
        if CommandLine.arguments.contains("--discover-sources") {
            startStdinReader(dryRun: true, discoveryOnly: true)
            RunLoop.current.run()
            return
        }
        if CommandLine.arguments.contains("--simulators") {
            DispatchQueue.global(qos: .userInitiated).async {
                let sources = ScreenCaptureSession.simulatorSources()
                write(["available": sources != nil, "sources": sources ?? []])
                exit(EXIT_SUCCESS)
            }
            RunLoop.current.run()
            return
        }
        if let index = CommandLine.arguments.firstIndex(of: "--windows") {
            let apps = Set(CommandLine.arguments.dropFirst(index + 1))
            let tabIndex = CommandLine.arguments.firstIndex(of: "--browser-tab")
            let tab = tabIndex.flatMap { tabIndex in
                tabIndex + 1 < index ? UInt64(CommandLine.arguments[tabIndex + 1]) : nil
            }
            discoverWindows(apps: apps, browserTab: tab) { result in
                write(result)
                exit(EXIT_SUCCESS)
            }
            RunLoop.current.run()
            return
        }
        #if DEBUG
        // Debug builds only: drive one simulator's injector without a session,
        // to measure each gesture against screenshots. Reads one JSON array of
        // control actions per line and answers each with the delivery result.
        if let index = CommandLine.arguments.firstIndex(of: "--simulator-input-probe"),
           index + 1 < CommandLine.arguments.count {
            let sourceID = "simulator:" + CommandLine.arguments[index + 1]
            let transport = CommandLine.arguments.firstIndex(of: "--transport").flatMap {
                $0 + 1 < CommandLine.arguments.count ? CommandLine.arguments[$0 + 1] : nil
            }
            DispatchQueue.global(qos: .userInitiated).async {
                let started = Date()
                let capability = simulatorInput.prepare(sourceID: sourceID, transport: transport, allButtons: true)
                var ready = capability.json
                ready["connectMs"] = Int(Date().timeIntervalSince(started) * 1_000)
                ready["transport"] = capability.transport ?? NSNull()
                write(ready)
                while let line = readLine() {
                    guard let data = line.data(using: .utf8),
                          let actions = try? JSONDecoder().decode([ControlInputAction].self, from: data) else {
                        write(["error": "invalid_request"]); continue
                    }
                    let begin = Date()
                    let semaphore = DispatchSemaphore(value: 0)
                    let box = ProbeResult()
                    Task { box.failure = await simulatorInput.deliver(sourceID: sourceID, actions: actions); semaphore.signal() }
                    semaphore.wait()
                    write(["delivered": box.failure == nil, "reason": box.failure ?? NSNull(),
                           "ms": Int(Date().timeIntervalSince(begin) * 1_000)])
                }
                simulatorInput.close()
                Thread.sleep(forTimeInterval: 0.2)
                exit(EXIT_SUCCESS)
            }
            RunLoop.current.run()
            return
        }
        #endif
        let dryRun = CommandLine.arguments.contains("--dry-run")
        let application = NSApplication.shared
        application.setActivationPolicy(.accessory)
        startStdinReader(dryRun: dryRun)
        startTextFocusReporting()
        _ = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            if !controlPreferenceEnabled(), let identity = controlGate.activeIdentity() {
                revokeActiveControl(identity, reason: "control_disabled")
            } else if let identity = controlGate.expireAndReturnIdentity() {
                releaseHeldInput()
                MainActor.assumeIsolated {
                    controlSurface.hide()
                }
                emitControlRevoked(identity, reason: "lease_expired")
            }
        }
        // ScreenCaptureKit's picker and observer callbacks are delivered on
        // the process run loop. Keep request parsing off this thread, but
        // serialize each request back onto the main run loop.
        application.run()
    }

    private struct DiscoveryRequest: Decodable, Sendable {
        let apps: [String]
        let browserTab: UInt64?
        let allWindows: Bool?
    }

    @MainActor private static var pendingDiscovery = 0
    @MainActor private static var discoveryInputClosed = false

    @MainActor private static func discoveryFinished() {
        pendingDiscovery -= 1
        if discoveryInputClosed && pendingDiscovery == 0 { exit(EXIT_SUCCESS) }
    }

    @MainActor private static func handleDiscovery(_ line: String) {
        pendingDiscovery += 1
        let reply: @Sendable ([String: Any]) -> Void = { result in
            write(result)
            DispatchQueue.main.async { discoveryFinished() }
        }
        guard let data = line.data(using: .utf8),
              let request = try? JSONDecoder().decode(DiscoveryRequest.self, from: data),
              request.apps.count <= 8,
              request.apps.allSatisfy({ app in
                  !app.isEmpty && app.utf8.count <= 150 && app.utf8.allSatisfy {
                      (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0)
                          || [45, 46, 95].contains($0)
                  }
              }) else {
            reply(["error": "invalid_discovery_request"])
            return
        }
        if request.apps.isEmpty && request.allWindows != true {
            DispatchQueue.global(qos: .userInitiated).async {
                if let sources = ScreenCaptureSession.simulatorSources() {
                    reply(["available": true, "sources": sources])
                } else {
                    reply(["error": "source_discovery_failed"])
                }
            }
        } else {
            #if DEBUG
            if CommandLine.arguments.contains("--fixture-window-sources") {
                fixtureWindowSources(completion: reply)
                return
            }
            #endif
            discoverWindows(apps: Set(request.apps), browserTab: request.browserTab,
                            allWindows: request.allWindows == true, completion: reply)
        }
    }

    #if DEBUG
    /// Debug builds only: a fixed window list that answers after stdin closes,
    /// so tests drain a pending reply at EOF without the desktop or icon work.
    @MainActor private static func fixtureWindowSources(completion: @escaping @Sendable ([String: Any]) -> Void) {
        guard discoveryInputClosed else {
            DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(10)) {
                fixtureWindowSources(completion: completion)
            }
            return
        }
        DispatchQueue.global(qos: .userInitiated).async {
            completion(["available": true, "sources": [["id": "fixture-window", "name": "Fixture"]]])
        }
    }
    #endif

    @MainActor private static func discoverWindows(
        apps: Set<String>, browserTab: UInt64?, allWindows: Bool = false,
        completion: @escaping @Sendable ([String: Any]) -> Void
    ) {
        let capture = ScreenCaptureSession.captureCapability
        guard capture.available else {
            completion(["available": false, "sources": [], "capture": capture.json])
            return
        }
        var tabAnswer: (String?, [BrowserTabWindow.ScriptWindow]?) = (nil, nil)
        if let browserTab, allWindows || apps.contains(BrowserTabWindow.bundleIdentifier) {
            tabAnswer = browserTabWindows(browserTab)
        }
        let (tabStatus, script) = tabAnswer
        ScreenCaptureSession.windowSources(appIDs: apps, allWindows: allWindows, browserTab: script) { sources, match, failed in
            guard !failed, let sources else { completion(["error": "source_discovery_failed"]); return }
            var result: [String: Any] = ["available": true, "sources": sources, "capture": capture.json]
            if let match {
                result["browserTab"] = match.status
                if case let .exact(window) = match {
                    result["browserTabSource"] = CaptureWindowTarget(appIdentifier: BrowserTabWindow.bundleIdentifier, windowNumber: window)?.sourceID
                }
            } else if let tabStatus {
                result["browserTab"] = tabStatus
            }
            completion(result)
        }
    }

    /// Chrome's own report of its windows for one tab. Checks Automation first
    /// and never shows the macOS prompt: without permission it reports why
    /// (`automationNeeded`, `automationDenied`, `notRunning`) and lists no match.
    @MainActor private static func browserTabWindows(_ tab: UInt64) -> (String?, [BrowserTabWindow.ScriptWindow]?) {
        switch AutomationPermission.check(bundleIdentifier: BrowserTabWindow.bundleIdentifier) {
        case .granted: break
        case .needsConsent: return ("automationNeeded", nil)
        case .denied: return ("automationDenied", nil)
        case .notRunning: return ("notFound", nil)
        case .unknown: return ("unavailable", nil)
        }
        var error: NSDictionary?
        guard let output = NSAppleScript(source: BrowserTabWindow.script(tabID: tab))?.executeAndReturnError(&error).stringValue,
              error == nil else { return ("unavailable", nil) }
        return (nil, BrowserTabWindow.windows(fromScript: output))
    }

    private static func watchSystemDialogs() {
        DispatchQueue.global(qos: .utility).async {
            // The supervisor closes standard input when no turn is running.
            var byte: UInt8 = 0
            while Darwin.read(STDIN_FILENO, &byte, 1) != 0 {}
            exit(EXIT_SUCCESS)
        }
        // A dialog that settles into place is the same dialog: compare identities, not bounds.
        var last: [String]?
        let timer = Timer(timeInterval: 1, repeats: true) { _ in
            let dialogs = SystemDialogDetector.current()
            let identities = dialogs.map { "\($0.kind.rawValue):\($0.windowID)" }
            guard identities != last else { return }
            last = identities
            write(["dialogs": dialogs.map(\.json)])
        }
        RunLoop.current.add(timer, forMode: .default)
        timer.fire()
        RunLoop.current.run()
    }

    private static func startStdinReader(dryRun: Bool, discoveryOnly: Bool = false) {
        DispatchQueue(label: "com.wonder.computer-use.stdin", qos: .userInitiated).async {
            var line = Data()
            line.reserveCapacity(maxRequestBytes)
            var discardingOversizedLine = false

            while true {
                var chunk = [UInt8](repeating: 0, count: stdinReadSize)
                let count = chunk.withUnsafeMutableBytes { buffer in
                    Darwin.read(STDIN_FILENO, buffer.baseAddress, buffer.count)
                }
                if count < 0, errno == EINTR { continue }
                guard count > 0 else { break }

                for byte in chunk.prefix(count) {
                    if discardingOversizedLine {
                        if byte == 0x0A {
                            discardingOversizedLine = false
                        }
                        continue
                    }
                    if byte == 0x0A {
                        enqueue(line, dryRun: dryRun, discoveryOnly: discoveryOnly)
                        line.removeAll(keepingCapacity: true)
                    } else if line.count < maxRequestBytes {
                        line.append(byte)
                    } else {
                        enqueueInvalidRequest()
                        line.removeAll(keepingCapacity: true)
                        discardingOversizedLine = true
                    }
                }
            }

            if discoveryOnly {
                // Drain requests already queued on the main run loop before
                // EOF exits. LaunchServices clients can send a batch and close
                // input immediately; exiting here discarded their error replies.
                DispatchQueue.main.async {
                    discoveryInputClosed = true
                    if pendingDiscovery == 0 { exit(EXIT_SUCCESS) }
                }
                return
            }
            DispatchQueue.main.async {
                _ = captureSession.shutdown(reason: "stdin_closed")
                releaseHeldInput()
                _ = controlGate.releaseAll()
                // Tell Settings the session ended so its Stop button disables.
                MainActor.assumeIsolated { controlSurface.hide() }
                exit(0)
            }
        }
    }

    private static func enqueue(_ data: Data, dryRun: Bool, discoveryOnly: Bool) {
        var data = data
        if data.last == 0x0D {
            data.removeLast()
        }
        guard let line = String(data: data, encoding: .utf8) else {
            enqueueInvalidRequest()
            return
        }
        DispatchQueue.main.async {
            if discoveryOnly { handleDiscovery(line) }
            else { handle(line, dryRun: dryRun) }
        }
    }

    private static func enqueueInvalidRequest() {
        DispatchQueue.main.async {
            write(["error": "invalid_request"])
        }
    }

    @MainActor
    private static func handle(_ line: String, dryRun: Bool) {
        precondition(Thread.isMainThread)
        guard line.utf8.count <= maxRequestBytes,
              let data = line.data(using: .utf8),
              let request = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let method = request["method"] as? String else {
            write(["error": "invalid_request"])
            return
        }
        let id = request["id"]
        let params = request["params"] as? [String: Any] ?? [:]
        do {
            if method != "stop" {
                try requireHandshake(params)
            }
            var result: [String: Any]
            switch method {
            case "capabilities":
                result = [
                    "protocolVersion": 1,
                    "clipboard": ["version": 1],
                    "capture": ScreenCaptureSession.captureCapability.json,
                    "control": MacControlSourcePolicy.capability(
                        accessibilityGranted: AXIsProcessTrusted(), controlAllowed: controlPreferenceEnabled()),
                ]
                // A simulator session also learns whether that exact device
                // takes input. Connecting can wait on the simulator's input
                // service, so it answers from the injector's own queue.
                if let sourceID = params["sourceID"] as? String, sourceID.hasPrefix("simulator:"),
                   validSignalText(sourceID, maximumBytes: 160) {
                    let reply = result.merging(["action": method]) { current, _ in current }
                    DispatchQueue.global(qos: .userInitiated).async {
                        var answer = reply
                        answer["simulatorControl"] = simulatorInput.prepare(sourceID: sourceID).json
                        writeResponse(id: id, result: answer)
                    }
                    return
                }
            case "clipboard.poll":
                let status = captureSession.status()
                guard let sessionID = params["sessionID"] as? String,
                      let generation = number(params["generation"]),
                      status.sessionID == sessionID, status.generation == generation,
                      let cursor = number(params["cursor"]) else { throw ComputerUseError.invalidInput }
                let subscription = params["subscriptionID"] as? String
                if let subscription, !validSignalText(subscription, maximumBytes: 128) {
                    throw ComputerUseError.invalidInput
                }
                Task { @MainActor in
                    let snapshot = await clipboardWatch.poll(
                        subscriptionID: !dryRun && status.state == .capturing ? subscription : nil, after: cursor)
                    var reply: [String: Any] = ["sequence": snapshot.sequence, "status": snapshot.status]
                    if let subscriptionID = snapshot.subscriptionID { reply["subscriptionID"] = subscriptionID }
                    if let text = snapshot.text { reply["text"] = text }
                    writeResponse(id: id, result: reply)
                }
                return
            case "status":
                result = [
                    "accessibility": AXIsProcessTrusted(),
                    "screenRecording": CGPreflightScreenCaptureAccess(),
                    "lockedUse": false,
                    "dryRun": dryRun,
                    "observation": accessibilityObservation(),
                    "capture": jsonObject(captureSession.status()) as Any,
                ]
            case "screenshot":
                result = try screenshot()
                    .merging(["observation": accessibilityObservation()]) { current, _ in current }
            case "control.consent":
                guard controlAllowedForSelectedSource() else { throw ComputerUseError.inputUnavailable }
                let identity = try controlIdentity(params)
                try requireCaptureBinding(identity)
                if !controlPreferenceEnabled() {
                    revokeActiveControl(identity, reason: "control_disabled")
                    result = ["granted": false, "decision": "deny", "replayed": false, "reason": "control_disabled"]
                    break
                }
                switch controlGate.requestConsent(identity) {
                case let .allowOnce(replayed):
                    result = ["granted": true, "decision": "allowOnce", "replayed": replayed]
                case let .denied(replayed):
                    result = [
                        "granted": false,
                        "decision": "deny",
                        "replayed": replayed,
                        "reason": controlPreferenceEnabled() ? "consent_denied" : "control_disabled",
                    ]
                case .identityConflict:
                    result = ["granted": false, "decision": "deny", "reason": "request_identity_conflict"]
                }
            case "control.activate":
                guard controlAllowedForSelectedSource() else { throw ComputerUseError.inputUnavailable }
                let identity = try controlIdentity(params)
                try requireCaptureBinding(identity)
                if !controlPreferenceEnabled() {
                    revokeActiveControl(identity, reason: "control_disabled")
                    result = ["activated": false, "reason": "control_disabled"]
                    break
                }
                switch controlGate.activate(identity, now: Date(), lifetime: controlLeaseLifetime) {
                case let .activated(replayed):
                    controlSurface.show {
                        guard let current = controlGate.activeIdentity() else { return }
                        _ = controlGate.release(current)
                        releaseHeldInput()
                        controlSurface.hide()
                        emitControlRevoked(current, reason: "local_stop")
                    }
                    result = ["activated": true, "replayed": replayed]
                case .consentRequired:
                    result = ["activated": false, "reason": "consent_required"]
                case .denied:
                    result = ["activated": false, "reason": "consent_denied"]
                case .busy:
                    result = ["activated": false, "reason": "control_busy"]
                case .identityConflict:
                    result = ["activated": false, "reason": "request_identity_conflict"]
                }
            case "control.heartbeat":
                guard controlAllowedForSelectedSource() else { throw ComputerUseError.inputUnavailable }
                let identity = try controlIdentity(params)
                try requireCaptureBinding(identity)
                if !controlPreferenceEnabled() {
                    revokeActiveControl(identity, reason: "control_disabled")
                    result = ["renewed": false, "reason": "control_disabled"]
                    break
                }
                result = ["renewed": controlGate.heartbeat(identity, now: Date(), lifetime: controlLeaseLifetime)]
            case "control.input":
                guard controlAllowedForSelectedSource() else { throw ComputerUseError.inputUnavailable }
                let identity = try controlIdentity(params)
                try requireCaptureBinding(identity)
                if !controlPreferenceEnabled() {
                    revokeActiveControl(identity, reason: "control_disabled")
                    result = ["accepted": false, "reason": "control_disabled"]
                    break
                }
                guard captureSession.status().state == .capturing else {
                    result = ["accepted": false, "reason": "capture_not_live"]
                    break
                }
                let simulator = captureSession.selectedSourceDescriptor()?.kind == .simulator
                guard let sequence = number(params["sequence"]), sequence > 0,
                      let rawActions = params["actions"] as? [[String: Any]],
                      !rawActions.isEmpty,
                      rawActions.count <= 32,
                      let actionsData = try? JSONSerialization.data(withJSONObject: rawActions),
                      let actions = try? JSONDecoder().decode([ControlInputAction].self, from: actionsData),
                      actions.count == rawActions.count,
                      simulator ? validSimulatorBatch(actions) : validControlBatch(actions) else {
                    throw ComputerUseError.invalidInput
                }
                let payload = try ControlInputAction.leasePayload(actions)
                // Serialize input batches while allowing focus activation, heartbeats,
                // release and view changes to make progress on the main run loop.
                let previous = pendingControlInput
                pendingControlInput = Task { @MainActor in
                    await previous?.value
                    let needsDelivery = !dryRun && controlGate.requiresNativeDelivery(identity, sequence: sequence, payload: payload)
                    guard simulator else {
                        let focusFailure = needsDelivery ? await prepareControlWindow(identity) : nil
                        writeResponse(id: id, result: controlInputResult(
                            identity, sequence: sequence, payload: payload, actions: actions,
                            dryRun: dryRun, focusFailure: focusFailure
                        ))
                        return
                    }
                    // Simulator input waits between touches on the injector's
                    // queue, so it is sent before the lease records the batch.
                    var failure: String?
                    if needsDelivery {
                        failure = simulatorBindingFailure(identity)
                        if failure == nil, let sourceID = identity.sourceID {
                            failure = await simulatorInput.deliver(sourceID: sourceID, actions: actions)
                        }
                    }
                    writeResponse(id: id, result: controlInputResult(
                        identity, sequence: sequence, payload: payload, actions: actions,
                        dryRun: dryRun, focusFailure: failure, alreadyDelivered: true
                    ))
                }
                return
            case "control.release":
                let identity = try controlIdentity(params)
                let released = controlGate.release(identity)
                if released {
                    releaseHeldInput()
                    controlSurface.hide()
                }
                result = ["released": released]
            case "control.releaseAll":
                releaseHeldInput()
                controlSurface.hide()
                result = ["released": controlGate.releaseAll()]
            case "control.revoke":
                releaseHeldInput()
                controlSurface.hide()
                result = ["released": controlGate.releaseAll()]
            case "capture.prepare", "prepare":
                let quality = try (params["videoQuality"] as? String).map { value -> CaptureVideoQuality in
                    guard let quality = CaptureVideoQuality(rawValue: value) else { throw ComputerUseError.invalidInput }
                    return quality
                } ?? .standard
                let configuration = quality.configuration(framesPerSecond: try Self.requestedFramesPerSecond(params))
                let sessionID: String
                if let suppliedSessionID = params["sessionID"] as? String {
                    guard !suppliedSessionID.isEmpty, suppliedSessionID.count <= 128 else { throw ComputerUseError.invalidInput }
                    sessionID = suppliedSessionID
                } else {
                    sessionID = UUID().uuidString
                }
                let generation = number(params["generation"]) ?? 0
                let viewerCount = Int(min(number(params["viewerCount"]) ?? 1, UInt64(Int.max)))
                let captureResult = captureSession.prepare(
                    sessionID: sessionID, generation: generation, viewerCount: viewerCount,
                    configuration: configuration,
                    sourceKind: (params["sourceID"] as? String)?.hasPrefix("simulator:") == true ? .simulator : .display
                )
                if captureResult.accepted {
                    publisher.prepare(sessionID: sessionID, generation: generation,
                                      configuration: configuration)
                }
                result = jsonObject(captureResult) ?? [:]
            case "capture.setQuality":
                guard let value = params["videoQuality"] as? String,
                      let quality = CaptureVideoQuality(rawValue: value) else {
                    throw ComputerUseError.invalidInput
                }
                let sessionID = try captureSessionID(params)
                let generation = try captureGeneration(params)
                let configuration = quality.configuration(framesPerSecond: try Self.requestedFramesPerSecond(params))
                Task { @MainActor in
                    let captureResult = await captureSession.updateConfiguration(
                        sessionID: sessionID, generation: generation, configuration: configuration
                    )
                    guard captureResult.accepted else {
                        writeResponse(id: id, error: captureResult.errorCode ?? "capture_configuration_failed")
                        return
                    }
                    guard publisher.updateQuality(sessionID: sessionID, generation: generation,
                                                  configuration: configuration) else {
                        writeResponse(id: id, error: "publisher_unavailable")
                        return
                    }
                    writeResponse(id: id, result: ["accepted": true, "videoQuality": value])
                }
                return
            case "capture.sources", "capture.listSources", "listSources":
                result = jsonObject(captureSession.listSources()) ?? [:]
            case "capture.rebind":
                let sessionID = try captureSessionID(params)
                let generation = try captureGeneration(params)
                let before = captureSession.status()
                guard before.sessionID == sessionID, before.generation == generation,
                      before.sourceID == params["sourceID"] as? String,
                      before.geometryRevision == number(params["geometryRevision"]) else {
                    throw ComputerUseError.invalidInput
                }
                let identity = params["leaseID"] == nil ? nil : try controlIdentity(params)
                guard controlGate.activeIdentity() == identity else { throw ComputerUseError.invalidInput }
                if identity != nil {
                    guard controlPreferenceEnabled(), AXIsProcessTrusted() else { throw ComputerUseError.inputUnavailable }
                    // A simulator lease never becomes control of the Mac itself.
                    guard captureSession.selectedSourceDescriptor()?.kind != .simulator,
                          MacControlSourcePolicy.allowsRebind(to: params["nextSourceID"] as? String) else {
                        writeResponse(id: id, result: ["accepted": false, "status": jsonObject(before) ?? [:]])
                        return
                    }
                }
                releaseHeldInput()
                Task { @MainActor in
                    let changed = await captureSession.rebind(sessionID: sessionID, generation: generation,
                                                             sourceID: params["nextSourceID"] as? String)
                    guard changed.accepted else {
                        // The daemon can retain the current view only when this
                        // explicit refusal still reports its original binding.
                        writeResponse(id: id, result: jsonObject(changed) ?? [:])
                        return
                    }
                    let status = captureSession.status()
                    if let identity {
                        let selected = ControlLeaseIdentity(leaseID: identity.leaseID, requestID: identity.requestID,
                            sessionID: identity.sessionID, generation: identity.generation,
                            geometryRevision: status.geometryRevision, sourceID: status.sourceID)
                        guard controlGate.rebind(identity, to: selected) else {
                            releaseHeldInput()
                            writeResponse(id: id, error: "lease_not_active")
                            return
                        }
                    }
                    var reply = jsonObject(changed) ?? [:]
                    reply["source"] = captureSession.selectedSourceDescriptor().flatMap(jsonObject)
                    writeResponse(id: id, result: reply)
                }
                return
            case "capture.pick", "pick":
                if let sessionID = params["sessionID"] as? String {
                    let status = captureSession.status()
                    guard let generation = number(params["generation"]),
                          sessionID == status.sessionID,
                          generation == status.generation else {
                        throw ComputerUseError.invalidInput
                    }
                }
                result = jsonObject(captureSession.pick(sourceID: params["sourceID"] as? String)) ?? [:]
            case "capture.start", "startCapture":
                result = jsonObject(captureSession.start(
                    sessionID: try captureSessionID(params),
                    generation: try captureGeneration(params)
                )) ?? [:]
            case "capture.pause", "pauseCapture":
                result = jsonObject(captureSession.pause(
                    sessionID: try captureSessionID(params),
                    generation: try captureGeneration(params)
                )) ?? [:]
            case "capture.resume", "resumeCapture":
                result = jsonObject(captureSession.resume(
                    sessionID: try captureSessionID(params),
                    generation: try captureGeneration(params)
                )) ?? [:]
            case "capture.stop", "stopCapture":
                let reason = (params["reason"] as? String) ?? "stopped_by_request"
                guard !reason.isEmpty, reason.count <= 128 else { throw ComputerUseError.invalidInput }
                let sessionID = try captureSessionID(params)
                let generation = try captureGeneration(params)
                let captureResult = captureSession.stop(
                    sessionID: sessionID,
                    generation: generation,
                    reason: reason
                )
                guard publisher.close(sessionID: sessionID, generation: generation, reason: reason) else {
                    throw ComputerUseError.invalidInput
                }
                result = jsonObject(captureResult) ?? [:]
            case "capture.sourceRemoved":
                let sessionID = try captureSessionID(params)
                let generation = try captureGeneration(params)
                let captureResult = captureSession.markSourceRemoved(
                    sessionID: sessionID,
                    generation: generation
                )
                _ = publisher.close(sessionID: sessionID, generation: generation, reason: "source_removed")
                result = jsonObject(captureResult) ?? [:]
            case "capture.status":
                result = jsonObject(captureSession.status()) ?? [:]
            case "capture.viewerCount":
                result = jsonObject(captureSession.setViewerCount(
                    Int(min(number(params["viewerCount"]) ?? 0, UInt64(Int.max))),
                    sessionID: try captureSessionID(params),
                    generation: try captureGeneration(params)
                )) ?? [:]
            case "capture.restart":
                publisher.close(reason: "helper_restart")
                result = jsonObject(captureSession.restart()) ?? [:]
            case "signal.answer":
                guard let sdp = params["sdp"] as? String,
                      let sessionID = params["sessionID"] as? String,
                      let generation = number(params["generation"]),
                      let peerRevision = params["peerRevision"] as? String,
                      validSignalText(peerRevision, maximumBytes: 128),
                      publisher.setRemoteAnswer(sessionID: sessionID, generation: generation,
                                                peerRevision: peerRevision, sdp: sdp) else {
                    throw ComputerUseError.invalidInput
                }
                result = ["accepted": true]
            case "signal.candidate":
                guard let candidate = params["candidate"] as? String,
                      let sessionID = params["sessionID"] as? String,
                      let generation = number(params["generation"]),
                      let peerRevision = params["peerRevision"] as? String,
                      validSignalText(peerRevision, maximumBytes: 128),
                      let sequence = number(params["sequence"]),
                      let rawIndex = exactInt32(params["sdpMLineIndex"]) else {
                    throw ComputerUseError.invalidInput
                }
                let sdpMid = params["sdpMid"] as? String
                let usernameFragment = params["usernameFragment"] as? String
                guard publisher.addRemoteCandidate(sessionID: sessionID, generation: generation,
                                                    peerRevision: peerRevision,
                                                    sequence: sequence, sdp: candidate, sdpMid: sdpMid,
                                                    sdpMLineIndex: rawIndex, usernameFragment: usernameFragment) else {
                    throw ComputerUseError.invalidInput
                }
                result = ["accepted": true]
            case "signal.close":
                let reason = (params["reason"] as? String) ?? "closed_by_request"
                guard let sessionID = params["sessionID"] as? String,
                      let generation = number(params["generation"]),
                      let peerRevision = params["peerRevision"] as? String,
                      validSignalText(peerRevision, maximumBytes: 128),
                      validSignalText(reason, maximumBytes: 128) else { throw ComputerUseError.invalidInput }
                guard publisher.close(sessionID: sessionID, generation: generation,
                                      peerRevision: peerRevision, reason: reason) else {
                    throw ComputerUseError.invalidInput
                }
                result = ["closed": true, "sessionID": sessionID, "generation": generation]
            case "stop":
                _ = captureSession.shutdown()
                releaseHeldInput()
                _ = controlGate.releaseAll()
                controlSurface.hide()
                publisher.close(reason: "helper_stopped")
                writeResponse(id: id, result: ["stopped": true])
                exit(0)
            default:
                throw ComputerUseError.unsupportedAction
            }
            if method != "stop" { result["action"] = method }
            writeResponse(id: id, result: result)
        } catch let error as ComputerUseError {
            writeResponse(id: id, error: error.rawValue)
        } catch {
            writeResponse(id: id, error: "computer_use_failed")
        }
    }

    private static func screenshot() throws -> [String: Any] {
        guard CGPreflightScreenCaptureAccess() else {
            throw ComputerUseError.screenRecordingRequired
        }
        guard let image = CGWindowListCreateImage(
            .null,
            .optionOnScreenOnly,
            kCGNullWindowID,
            [.bestResolution, .boundsIgnoreFraming]
        ) else {
            throw ComputerUseError.screenRecordingRequired
        }
        let data = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(
            data,
            UTType.png.identifier as CFString,
            1,
            nil
        ) else {
            throw ComputerUseError.screenshotFailed
        }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination), data.length <= maxScreenshotBytes else {
            throw ComputerUseError.screenshotFailed
        }
        return [
            "mimeType": "image/png",
            "imageBase64": (data as Data).base64EncodedString(),
        ]
    }

    private static func controlIdentity(_ params: [String: Any]) throws -> ControlLeaseIdentity {
        let sourceID: String?
        if let rawSourceID = params["sourceID"], !(rawSourceID is NSNull) {
            guard let value = rawSourceID as? String else { throw ComputerUseError.invalidInput }
            sourceID = value
        } else {
            sourceID = nil
        }
        guard let leaseID = boundedString(params["leaseID"], maximum: 128),
              let requestID = boundedString(params["requestID"], maximum: 160),
              let sessionID = boundedString(params["sessionID"], maximum: 128),
              let generation = number(params["generation"]), generation > 0,
              let geometryRevision = number(params["geometryRevision"]) else {
            throw ComputerUseError.invalidInput
        }
        if let sourceID, !validSignalText(sourceID, maximumBytes: 160) { throw ComputerUseError.invalidInput }
        return ControlLeaseIdentity(
            leaseID: leaseID,
            requestID: requestID,
            sessionID: sessionID,
            generation: generation,
            geometryRevision: geometryRevision,
            sourceID: sourceID
        )
    }

    private static func requireCaptureBinding(_ identity: ControlLeaseIdentity) throws {
        let status = captureSession.status()
        guard status.sessionID == identity.sessionID,
              status.generation == identity.generation,
              status.sourceID == identity.sourceID,
              status.geometryRevision == identity.geometryRevision else {
            throw ComputerUseError.invalidInput
        }
    }

    private static func controlPreferenceEnabled() -> Bool {
        guard let path = ProcessInfo.processInfo.environment["WONDER_SERVICE_DIR"] else {
            return false
        }
        return ControlPreferencesStore(serviceDirectory: URL(fileURLWithPath: path)).isEnabled
    }

    private static func requestLocalControlConsent(_ identity: ControlLeaseIdentity) -> ControlConsentDecision {
        // The preference gate is checked again by ControlLeaseGate before it
        // records this decision. There is intentionally no modal consent path:
        // only the local Mac Settings toggle can opt in.
        _ = identity
        return .allowOnce
    }

    @discardableResult
    private static func revokeActiveControl(_ identity: ControlLeaseIdentity, reason: String) -> Bool {
        guard controlGate.release(identity) else { return false }
        releaseHeldInput()
        MainActor.assumeIsolated {
            controlSurface.hide()
        }
        emitControlRevoked(identity, reason: reason)
        return true
    }

    private static func emitControlRevoked(_ identity: ControlLeaseIdentity, reason: String) {
        var event = controlEvent("control.revoked", identity)
        event["reason"] = reason
        write(event)
    }

    private static func controlEvent(_ name: String, _ identity: ControlLeaseIdentity) -> [String: Any] {
        var event: [String: Any] = [
            "event": name,
            "sessionID": identity.sessionID,
            "generation": identity.generation,
            "leaseID": identity.leaseID,
            "requestID": identity.requestID,
            "geometryRevision": identity.geometryRevision,
        ]
        if let sourceID = identity.sourceID {
            event["sourceID"] = sourceID
        }
        return event
    }

    /// While the phone controls the Mac, tell it when typing focus enters or
    /// leaves a text field so it can raise or lower its keyboard. Accessibility
    /// reads stay off the main run loop that delivers input.
    private static func startTextFocusReporting() {
        let timer = DispatchSource.makeTimerSource(queue: textFocusQueue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(250), leeway: .milliseconds(50))
        timer.setEventHandler {
            guard let change = textFocusChanges.poll(activeIdentity: controlGate.activeIdentity,
                                                     sample: textInputFocused) else { return }
            var event = controlEvent("control.textFocus", change.identity)
            event["textInput"] = change.textInput
            write(event)
        }
        timer.resume()
        textFocusTimer = timer
    }

    /// A shared window counts only while it, or a sheet on it, holds focus.
    private static func textInputFocused() -> Bool {
        guard captureSession.status().state == .capturing else { return false }
        guard let source = captureSession.selectedSourceDescriptor(), source.kind == .window else {
            return TextInputFocus.current(in: nil)
        }
        guard source.id.hasPrefix("window:"), let windowID = UInt32(source.id.dropFirst(7)),
              let processID = captureSession.selectedWindowProcessID() else { return false }
        return TextInputFocus.current(in: TextInputFocus.ShownWindow(processID: processID, windowID: windowID))
    }

    private static func boundedString(_ value: Any?, maximum: Int) -> String? {
        guard let value = value as? String,
              !value.isEmpty,
              value.utf8.count <= maximum,
              !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else { return nil }
        return value
    }

    private static func validControlAction(_ action: ControlInputAction) -> Bool {
        switch action {
        case let .pointer(x, y, phase, button):
            return x.isFinite && y.isFinite && (0...1).contains(x) && (0...1).contains(y)
                && ["move", "down", "up"].contains(phase)
                && button.map { ["left", "right", "middle"].contains($0) } ?? true
                && (phase == "move" || button != nil)
        case let .scroll(deltaX, deltaY):
            return deltaX.isFinite && deltaY.isFinite && abs(deltaX) <= 4_096 && abs(deltaY) <= 4_096
        case let .key(key, phase, modifiers):
            return !key.isEmpty && key.count <= 32 && ControlInputTranslator.acceptsKey(key)
                && ["down", "up", "press"].contains(phase)
                && modifiers & ~ControlInputTranslator.allowedModifierMask == 0
        case let .text(text):
            return ControlInputTranslator.validText(text, maximum: maxTextLength)
        case let .clipboard(operation, text):
            return (operation == "copyToPhone" && text == nil) || (operation == "pasteFromPhone" && text.map { ControlInputTranslator.validText($0, maximum: 8_192, allowEmpty: true) } == true)
        case .releaseAll:
            return true
        case .touch, .button:
            return false
        }
    }

    /// Simulator input: touch, hardware buttons, keys and typed text. Points,
    /// buttons and characters are checked again by `SimulatorInputPlanner`.
    private static func validSimulatorBatch(_ actions: [ControlInputAction]) -> Bool {
        actions.allSatisfy { action in
            switch action {
            case let .touch(x, y, phase, delay):
                return x.isFinite && y.isFinite && (0...1).contains(x) && (0...1).contains(y)
                    && ["down", "move", "up"].contains(phase)
                    && (delay ?? 0) <= SimulatorInputPlanner.maximumDelayMilliseconds
            case let .button(name, phase):
                return SimulatorHardwareButton(rawValue: name) != nil && ["press", "down", "up"].contains(phase)
            case let .key(key, phase, modifiers):
                return SimulatorKeyboard.usage(forKey: key) != nil && ["down", "up", "press"].contains(phase)
                    && modifiers & ~ControlInputTranslator.allowedModifierMask == 0
            case let .text(text):
                // Non-US characters pass here and are refused with a reason.
                return ControlInputTranslator.validText(text, maximum: SimulatorInputPlanner.maximumTextLength)
            case .releaseAll:
                return true
            case .pointer, .scroll, .clipboard:
                return false
            }
        }
    }

    /// Simulator control needs the injector for the selected device; Mac views
    /// keep their source policy.
    private static func controlAllowedForSelectedSource() -> Bool {
        let source = captureSession.selectedSourceDescriptor()
        if source?.kind == .simulator { return simulatorInput.isReady(for: source?.id) }
        return MacControlSourcePolicy.allows(source?.kind)
    }

    /// The Mac checks without Accessibility or window focus: simulator input
    /// never posts Mac events.
    @MainActor private static func simulatorBindingFailure(_ identity: ControlLeaseIdentity) -> String? {
        let status = captureSession.status()
        if !controlPreferenceEnabled() { return "control_disabled" }
        if controlGate.activeIdentity() != identity { return "lease_not_active" }
        guard status.sessionID == identity.sessionID, status.generation == identity.generation,
              status.sourceID == identity.sourceID, status.geometryRevision == identity.geometryRevision,
              let sourceID = identity.sourceID, sourceID.hasPrefix("simulator:"),
              simulatorInput.isReady(for: sourceID) else { return "view_changed" }
        return status.state == .capturing ? nil : "capture_not_live"
    }

    private static func validControlBatch(_ actions: [ControlInputAction]) -> Bool {
        guard actions.allSatisfy(validControlAction) else { return false }
        let containsPasteFromPhone = actions.contains { action in
            if case let .clipboard(operation, _) = action {
                return operation == "pasteFromPhone"
            }
            return false
        }
        return !containsPasteFromPhone || actions.count == 1
    }

    /// Returns nil when input may be posted, or why not, so the phone can say
    /// what stopped control instead of a generic message.
    @MainActor private static func prepareControlWindow(_ identity: ControlLeaseIdentity) async -> String? {
        func bindingFailure() -> String? {
            let status = captureSession.status()
            if !controlPreferenceEnabled() { return "control_disabled" }
            if !AXIsProcessTrusted() { return "accessibility_permission_required" }
            if controlGate.activeIdentity() != identity { return "lease_not_active" }
            guard status.sessionID == identity.sessionID, status.generation == identity.generation,
                  status.sourceID == identity.sourceID, status.geometryRevision == identity.geometryRevision
            else { return "view_changed" }
            return status.state == .capturing ? nil : "capture_not_live"
        }
        if let failure = bindingFailure() { return failure }
        guard let source = captureSession.selectedSourceDescriptor() else { return "view_changed" }
        guard source.kind == .window else { return nil }
        guard source.id.hasPrefix("window:"), let id = UInt32(source.id.dropFirst(7)),
              let expectedPID = captureSession.selectedWindowProcessID() else { return "window_not_found" }
        func targetWindow() -> [String: Any]? {
            let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
            return windows.first { ($0[kCGWindowNumber as String] as? NSNumber)?.uint32Value == id
                && ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == expectedPID }
        }
        guard let owner = targetWindow()?[kCGWindowOwnerName as String] as? String else { return "window_not_found" }
        let ready = await SystemDialogWindowRaiser.prepareForInput(windowID: id, owner: owner) {
            bindingFailure() == nil && targetWindow() != nil
        }
        if ready { return nil }
        if let failure = bindingFailure() { return failure }
        return targetWindow() == nil ? "window_not_found" : "window_not_focused"
    }

    @MainActor private static func controlInputResult(
        _ identity: ControlLeaseIdentity, sequence: UInt64, payload: Data,
        actions: [ControlInputAction], dryRun: Bool, focusFailure: String?, alreadyDelivered: Bool = false
    ) -> [String: Any] {
        var result: [String: Any]
        var clipboard: ComputerClipboardCopy?
        let delivery = controlGate.deliver(identity, sequence: sequence, payload: payload, nativeDeliveryFailure: {
            if dryRun { return nil }
            if let focusFailure { return focusFailure }
            if alreadyDelivered { return nil }
            do { try requireCaptureBinding(identity) }
            catch { return "view_changed" }
            do {
                clipboard = try deliverControlActions(actions)
                return nil
            } catch {
                releaseHeldInput()
                return ControlWindowFailure.deliveryReason(for: error)
            }
        })
        switch delivery {
        case .delivered:
            result = ["accepted": true, "duplicate": false]
        case .duplicate:
            result = ["accepted": true, "duplicate": true]
        case let .rejected(reason):
            if reason == "control_disabled" {
                releaseHeldInput()
                controlSurface.hide()
                emitControlRevoked(identity, reason: "control_disabled")
            }
            result = ["accepted": false, "reason": reason]
        }
        // An unavailable copy is still delivered input: control stays active
        // and the daemon explains the reason instead of releasing it.
        switch clipboard {
        case let .text(text): result["clipboardText"] = text
        case let .unavailable(reason): result["clipboardReason"] = reason.rawValue
        case nil: break
        }
        return result
    }

    private enum PreparedControlEffect {
        case post(CGEvent)
        case setMouseHeld(String, Bool)
        case setKeyboard(ControlKeyboardState)
        case setClipboardResult(ComputerClipboardCopy)
    }

    private static func deliverControlActions(_ actions: [ControlInputAction]) throws -> ComputerClipboardCopy? {
        try requireAccessibility()
        guard let selected = captureSession.selectedSourceDescriptor() else { throw ComputerUseError.inputUnavailable }
        let source = try controlSource(selected)

        var effects: [PreparedControlEffect] = []
        var preparedMouseButtons = heldInput.mouseButtons
        var preparedKeyboard = heldInput.keyboard
        var preparedPointerLocation = CGEvent(source: nil)?.location ?? .zero
        var preparedClipboard: ComputerClipboardCopy?
        var clipboardResult: ComputerClipboardCopy?

        func appendKey(keyCode: UInt16, phase: String, modifiers: UInt32) throws {
            guard let events = preparedKeyboard.events(keyCode: keyCode, phase: phase, modifiers: modifiers) else {
                throw ComputerUseError.inputUnavailable
            }
            effects.append(contentsOf: events.map(PreparedControlEffect.post))
            effects.append(.setKeyboard(preparedKeyboard))
        }

        func appendSystemKey(_ type: Int, phase: String) throws {
            let states: [Bool]
            switch phase {
            case "press": states = [true, false]
            case "down": states = [true]
            case "up": states = [false]
            default: throw ComputerUseError.invalidInput
            }
            for down in states {
                guard let event = ControlInputTranslator.systemKeyEvent(type: type, down: down)
                else { throw ComputerUseError.inputUnavailable }
                effects.append(.post(event))
            }
        }

        func appendText(_ text: String) throws {
            for chunk in ControlInputTranslator.unicodeEventChunks(text) {
                guard let event = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true) else {
                    throw ComputerUseError.inputUnavailable
                }
                event.keyboardSetUnicodeString(
                    stringLength: chunk.count,
                    unicodeString: chunk
                )
                effects.append(.post(event))
            }
        }

        for action in actions {
            switch action {
            case let .pointer(x, y, phase, button):
                guard let point = ControlInputTranslator.point(x: x, y: y, in: source) else {
                    throw ComputerUseError.invalidInput
                }
                if phase != "move" && button == nil {
                    throw ComputerUseError.invalidInput
                }
                let buttonName = button ?? "left"
                guard let mouseButton = mouseButton(buttonName, required: phase != "move") else {
                    throw ComputerUseError.invalidInput
                }
                let mouseType: CGEventType = phase == "down" ? .leftMouseDown : phase == "up" ? .leftMouseUp : .mouseMoved
                let eventType = button == nil ? .mouseMoved : mouseEventType(mouseType, button: mouseButton)
                guard let event = CGEvent(
                    mouseEventSource: nil,
                    mouseType: eventType,
                    mouseCursorPosition: CGPoint(x: point.x, y: point.y),
                    mouseButton: mouseButton
                ) else {
                    throw ComputerUseError.inputUnavailable
                }
                // A held button must always be released, including if a new
                // window covers the target during a drag.
                if !(phase == "up" && heldInput.mouseButtons.contains(buttonName)) {
                    try requireWindowAtPoint(source, point: CGPoint(x: point.x, y: point.y))
                }
                event.flags = preparedKeyboard.modifierFlags
                effects.append(.post(event))
                preparedPointerLocation = CGPoint(x: point.x, y: point.y)
                if phase == "down" {
                    effects.append(.setMouseHeld(buttonName, true))
                    preparedMouseButtons.insert(buttonName)
                } else if phase == "up" {
                    effects.append(.setMouseHeld(buttonName, false))
                    preparedMouseButtons.remove(buttonName)
                }
            case let .scroll(deltaX, deltaY):
                let windows = source.kind == .window
                    ? CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
                    : []
                let windowID = UInt32(source.id.dropFirst(7)) ?? 0
                guard let anchor = ControlInputTranslator.scrollAnchor(
                    pointer: ControlPoint(x: preparedPointerLocation.x, y: preparedPointerLocation.y), in: source,
                    receivesInput: { ScreenCaptureSession.windowReceivesInput(windowID, at: CGPoint(x: $0.x, y: $0.y), windows: windows) }
                ) else { throw ControlWindowFailure.covered }
                if anchor.movesPointer {
                    let point = CGPoint(x: anchor.point.x, y: anchor.point.y)
                    let held = preparedMouseButtons.first.flatMap { mouseButton($0, required: true) }
                    guard let move = CGEvent(mouseEventSource: nil,
                                             mouseType: held.map { mouseEventType(.mouseMoved, button: $0) } ?? .mouseMoved,
                                             mouseCursorPosition: point, mouseButton: held ?? .left) else {
                        throw ComputerUseError.inputUnavailable
                    }
                    move.flags = preparedKeyboard.modifierFlags
                    effects.append(.post(move))
                    preparedPointerLocation = point
                }
                guard let event = CGEvent(
                    scrollWheelEvent2Source: nil,
                    units: .pixel,
                    wheelCount: 2,
                    wheel1: Int32(deltaY.rounded()),
                    wheel2: Int32(deltaX.rounded()),
                    wheel3: 0
                ) else {
                    throw ComputerUseError.inputUnavailable
                }
                // A new scroll event carries the pointer position at creation,
                // before the prepared move is posted; route it to the checked point.
                event.location = preparedPointerLocation
                event.flags = preparedKeyboard.modifierFlags
                effects.append(.post(event))
            case let .key(key, phase, modifiers):
                if ControlInputTranslator.isIgnoredOnMac(key) { break }
                if let systemKey = ControlInputTranslator.systemKeyType(for: key) {
                    try appendSystemKey(systemKey, phase: phase)
                    break
                }
                guard let keyCode = ControlInputTranslator.keyCode(for: key) else {
                    throw ComputerUseError.invalidInput
                }
                try appendKey(keyCode: keyCode, phase: phase, modifiers: modifiers)
            case let .text(text):
                try appendText(text)
            case let .clipboard(operation, text):
                if operation == "copyToPhone" {
                    if preparedClipboard == nil { preparedClipboard = manualClipboard.copy() }
                    if let preparedClipboard { effects.append(.setClipboardResult(preparedClipboard)) }
                } else if let text {
                    // Insert the phone text directly. This preserves the Mac
                    // clipboard and lets the entire input be prepared before
                    // the first native event is posted.
                    try appendText(text)
                } else {
                    throw ComputerUseError.invalidInput
                }
            case .touch, .button:
                throw ComputerUseError.invalidInput
            case .releaseAll:
                for buttonName in preparedMouseButtons {
                    guard let button = mouseButton(buttonName, required: true),
                          let event = CGEvent(
                            mouseEventSource: nil,
                            mouseType: mouseEventType(.leftMouseUp, button: button),
                            mouseCursorPosition: preparedPointerLocation,
                            mouseButton: button
                          ) else {
                        throw ComputerUseError.inputUnavailable
                    }
                    event.flags = preparedKeyboard.modifierFlags
                    effects.append(.post(event))
                    effects.append(.setMouseHeld(buttonName, false))
                }
                effects.append(contentsOf: preparedKeyboard.releaseEvents().map(PreparedControlEffect.post))
                effects.append(.setKeyboard(preparedKeyboard))
                preparedMouseButtons.removeAll()
            }
        }

        for effect in effects {
            switch effect {
            case let .post(event):
                stampClickState(event)
                event.post(tap: .cghidEventTap)
            case let .setMouseHeld(buttonName, held):
                if held { heldInput.mouseButtons.insert(buttonName) }
                else { heldInput.mouseButtons.remove(buttonName) }
            case let .setKeyboard(keyboard):
                heldInput.keyboard = keyboard
            case let .setClipboardResult(value):
                clipboardResult = value
            }
        }
        return clipboardResult
    }

    /// Resolve the current frame, not the frame at capture selection. Global
    /// HID events may only reach the exact selected window after it is raised.
    /// `prepareControlWindow` confirmed its Accessibility focus off the main
    /// run loop just before this, with no await in between; here the cheap
    /// window-server checks catch a window that closed or an app that took
    /// focus since.
    private static func controlSource(_ source: CaptureSourceDescriptor) throws -> CaptureSourceDescriptor {
        guard source.kind == .window else { return source }
        guard source.id.hasPrefix("window:"), let id = UInt32(source.id.dropFirst(7)) else {
            throw ComputerUseError.inputUnavailable
        }
        guard let expectedPID = captureSession.selectedWindowProcessID() else { throw ControlWindowFailure.notFound }
        let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
        let rect = try ControlWindowFailure.frame(
            windowID: id, expectedPID: expectedPID,
            frontmostPID: NSWorkspace.shared.frontmostApplication?.processIdentifier, windows: windows)
        return CaptureSourceDescriptor(id: source.id, kind: source.kind, title: source.title,
            application: source.application, width: Int(rect.width), height: Int(rect.height), scale: 1,
            contentRect: CaptureRect(x: rect.minX, y: rect.minY, width: rect.width, height: rect.height))
    }

    /// Reads the window list again for every pointer action: another window
    /// or a banner can appear in front of the shared one mid-batch.
    private static func requireWindowAtPoint(_ source: CaptureSourceDescriptor, point: CGPoint) throws {
        guard source.kind == .window else { return }
        guard let id = UInt32(source.id.dropFirst(7)) else { throw ComputerUseError.inputUnavailable }
        let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
        try ControlWindowFailure.requireInput(windowID: id, at: point, windows: windows)
    }

    /// Gives each press and release its click count so double and triple
    /// clicks select a word and a line on the Mac.
    private static func stampClickState(_ event: CGEvent) {
        let name: String
        switch event.type {
        case .leftMouseDown, .leftMouseUp: name = "left"
        case .rightMouseDown, .rightMouseUp: name = "right"
        case .otherMouseDown, .otherMouseUp: name = "middle"
        default: return
        }
        switch event.type {
        case .leftMouseDown, .rightMouseDown, .otherMouseDown:
            let count = heldInput.clicks.down(button: name, x: event.location.x, y: event.location.y,
                                              time: ProcessInfo.processInfo.systemUptime,
                                              interval: NSEvent.doubleClickInterval)
            event.setIntegerValueField(.mouseEventClickState, value: Int64(count))
        default:
            event.setIntegerValueField(.mouseEventClickState, value: Int64(heldInput.clicks.current))
        }
    }

    private static func mouseButton(_ name: String, required: Bool) -> CGMouseButton? {
        switch name { case "left": return .left; case "right": return .right; case "middle": return .center; default: return required ? nil : .left }
    }

    private static func mouseEventType(_ type: CGEventType, button: CGMouseButton) -> CGEventType {
        switch (type, button) {
        case (.leftMouseDown, .right): return .rightMouseDown
        case (.leftMouseUp, .right): return .rightMouseUp
        case (.leftMouseDown, .center): return .otherMouseDown
        case (.leftMouseUp, .center): return .otherMouseUp
        case (.mouseMoved, .left): return .leftMouseDragged
        case (.mouseMoved, .right): return .rightMouseDragged
        case (.mouseMoved, .center): return .otherMouseDragged
        default: return type
        }
    }

    private static func releaseHeldInput() {
        simulatorInput.releaseHeld()
        let point = CGEvent(source: nil)?.location ?? .zero
        for name in heldInput.mouseButtons {
            guard let button = mouseButton(name, required: true),
                  let event = CGEvent(mouseEventSource: nil, mouseType: mouseEventType(.leftMouseUp, button: button), mouseCursorPosition: point, mouseButton: button) else { continue }
            event.flags = heldInput.keyboard.modifierFlags
            event.post(tap: .cghidEventTap)
        }
        for event in heldInput.keyboard.releaseEvents() {
            event.post(tap: .cghidEventTap)
        }
        heldInput.mouseButtons.removeAll()
    }

    private static func accessibilityObservation() -> [String: Any] {
        let frontmost = NSWorkspace.shared.frontmostApplication
        var observation: [String: Any] = [
            "available": AXIsProcessTrusted(),
            "frontmostApp": frontmost?.localizedName as Any,
            "frontmostBundleId": frontmost?.bundleIdentifier as Any,
        ]
        guard AXIsProcessTrusted() else { return observation }

        let system = AXUIElementCreateSystemWide()
        guard let focusedApplication = axElement(system, attribute: kAXFocusedApplicationAttribute) else {
            return observation
        }
        if let window = axElement(focusedApplication, attribute: kAXFocusedWindowAttribute) {
            observation["window"] = axSummary(window)
        }
        if let focusedElement = axElement(focusedApplication, attribute: kAXFocusedUIElementAttribute) {
            observation["focusedElement"] = axSummary(focusedElement)
        }
        return observation
    }

    private static func axElement(_ element: AXUIElement, attribute: String) -> AXUIElement? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success,
              let value else { return nil }
        return unsafeDowncast(value, to: AXUIElement.self)
    }

    private static func axString(_ element: AXUIElement, attribute: String) -> String? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success,
              let value else { return nil }
        return value as? String
    }

    private static func axSummary(_ element: AXUIElement) -> [String: Any] {
        var summary: [String: Any] = [:]
        if let role = axString(element, attribute: kAXRoleAttribute) { summary["role"] = role }
        if let subrole = axString(element, attribute: kAXSubroleAttribute) { summary["subrole"] = subrole }
        if let title = axString(element, attribute: kAXTitleAttribute) { summary["title"] = title }
        if let description = axString(element, attribute: kAXDescriptionAttribute) { summary["description"] = description }
        if let identifier = axString(element, attribute: kAXIdentifierAttribute) { summary["identifier"] = identifier }
        var settable: DarwinBoolean = false
        if AXUIElementIsAttributeSettable(element, kAXValueAttribute as CFString, &settable) == .success {
            summary["valueSettable"] = settable.boolValue
        }
        var value: CFTypeRef?
        if AXUIElementCopyAttributeValue(element, kAXValueAttribute as CFString, &value) == .success,
           let value {
            if let text = value as? String {
                summary["valueLength"] = text.count
            } else {
                summary["valueType"] = String(describing: type(of: value))
            }
        }
        return summary
    }

    private static func requireAccessibility() throws {
        guard AXIsProcessTrusted() else {
            throw ComputerUseError.accessibilityRequired
        }
    }

    private static func requireHandshake(_ params: [String: Any]) throws {
        guard let expected = ProcessInfo.processInfo.environment["WONDER_COMPUTER_USE_HANDSHAKE"],
              let supplied = params["handshake"] as? String,
              !expected.isEmpty,
              supplied == expected else {
            throw ComputerUseError.handshakeRequired
        }
    }

    /// The viewer's frame-rate ceiling. A request without one is a client from
    /// before the field existed and gets 15.
    private static func requestedFramesPerSecond(_ params: [String: Any]) throws -> Int {
        guard let raw = params["videoMaxFramesPerSecond"] else { return VideoEncodingPolicy.defaultFramesPerSecond }
        guard let value = Self.number(raw), value <= UInt64(Int.max),
              VideoEncodingPolicy.isValid(framesPerSecond: Int(value)) else {
            throw ComputerUseError.invalidInput
        }
        return Int(value)
    }

    private static func number(_ value: Any?) -> UInt64? {
        guard let value = value as? NSNumber,
              String(cString: value.objCType) != "c" else { return nil }
        if let exact = UInt64(value.stringValue) {
            return exact
        }
        guard value.doubleValue.isFinite,
              value.doubleValue >= 0,
              value.doubleValue.rounded(.towardZero) == value.doubleValue,
              value.doubleValue < 18_446_744_073_709_551_616 else { return nil }
        return UInt64(value.doubleValue)
    }

    private static func exactInt32(_ value: Any?) -> Int32? {
        guard let value = value as? NSNumber,
              String(cString: value.objCType) != "c",
              value.doubleValue.isFinite,
              value.doubleValue.rounded(.towardZero) == value.doubleValue,
              value.doubleValue >= 0,
              value.doubleValue <= Double(Int32.max) else { return nil }
        return Int32(value.int64Value)
    }

    private static func validSignalText(_ value: String, maximumBytes: Int) -> Bool {
        !value.isEmpty && value.utf8.count <= maximumBytes
            && !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
    }

    private static func captureSessionID(_ params: [String: Any]) throws -> String {
        guard let sessionID = params["sessionID"] as? String, !sessionID.isEmpty, sessionID.count <= 128 else {
            throw ComputerUseError.invalidInput
        }
        return sessionID
    }

    private static func captureGeneration(_ params: [String: Any]) throws -> UInt64 {
        guard let generation = number(params["generation"]) else { throw ComputerUseError.invalidInput }
        return generation
    }

    private static func jsonObject<T: Encodable>(_ value: T) -> [String: Any]? {
        guard let data = try? JSONEncoder().encode(value),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        return object
    }

    private static func write(_ object: [String: Any]) {
        outputLock.lock()
        defer { outputLock.unlock() }
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object) else { return }
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data([0x0A]))
    }

    private static let outputLock = NSLock()

    private static func writeResponse(id: Any?, result: [String: Any]? = nil, error: String? = nil) {
        var response: [String: Any] = [:]
        if let id { response["id"] = id }
        if let result { response["result"] = result }
        if let error { response["error"] = ["code": error] }
        write(response)
    }
}

#if DEBUG
private final class ProbeResult: @unchecked Sendable { var failure: String? }
#endif

private enum ComputerUseError: String, Error {
    case accessibilityRequired = "accessibility_required"
    case handshakeRequired = "handshake_required"
    case inputUnavailable = "input_unavailable"
    case invalidInput = "invalid_input"
    case screenRecordingRequired = "screen_recording_required"
    case screenshotFailed = "screenshot_failed"
    case unsupportedAction = "unsupported_action"
}
