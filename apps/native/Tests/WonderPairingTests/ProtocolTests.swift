import XCTest
@testable import WonderPairing

private final class ComputerControlResponseProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host == "computer-control-test.invalid"
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let data = Data(#"{"granted":false,"acknowledged":true,"status":"rejected","reason":"Control was not allowed.","lease":null,"control":{"available":true,"action":"none","reason":"Control is available.","heartbeatIntervalSeconds":3,"leaseExpirySeconds":10}}"#.utf8)
        client?.urlProtocol(
            self,
            didReceive: HTTPURLResponse(
                url: request.url!,
                statusCode: 409,
                httpVersion: "HTTP/1.1",
                headerFields: ["Content-Type": "application/json"]
            )!,
            cacheStoragePolicy: .notAllowed
        )
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

private final class AnnotationRejectionProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool {
        ["annotation-test.invalid", "roots-revision-test.invalid", "removed-root-test.invalid", "ordinary-conflict.invalid"].contains(request.url?.host ?? "")
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let message: String
        switch request.url?.host {
        case "annotation-test.invalid":
            message = "This file changed since you annotated it. Reopen the preview and update the annotation."
        case "roots-revision-test.invalid":
            message = "Project folders changed. Reopen the annotation preview before sending."
        case "removed-root-test.invalid":
            message = "An annotated Project folder changed. Reopen the preview before sending."
        default: message = "The conversation changed."
        }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 409,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/plain; charset=utf-8"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(message.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class SkillRejectionProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host?.hasSuffix("skill-test.invalid") == true
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let host = request.url!.host!
        let status = host.hasPrefix("roots.") ? 412 : host.hasPrefix("conflict.") ? 409 : host.hasPrefix("uncertain.") ? 503 : 422
        let detail = host.hasPrefix("oversized.") ? String(repeating: "x", count: 5000) : "Choose the skill again."
        let body = try! JSONEncoder().encode(["error": host.hasPrefix("ordinary.") ? "other_failure" : "codex_skill_rejected", "detail": detail])
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class ProviderRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "providers-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let body = try! JSONEncoder().encode(["detail": "An agent is working. Try again after it finishes."])
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 409,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

/// The host's 409 bodies for opening a computer view (crates/wonderd computer_sessions.rs
/// `ViewerConflictResponse`), an older host's plain refusal and an unknown code.
private final class ComputerViewerConflictProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "computer-view-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let body = switch request.url?.query ?? "" {
        case "case=busy": #"{"code":"viewerBusy","message":"This computer is being viewed on Sam’s iPad.","deviceName":"Sam’s iPad","sessionId":"s1","generation":2}"#
        case "case=unnamed": #"{"code":"viewerBusy","message":"This computer is being viewed on another device.","sessionId":"s1","generation":2}"#
        case "case=changed": #"{"code":"viewChanged","message":"The computer view changed. Close this screen and try again.","sessionId":"s2","generation":1}"#
        case "case=unknown": #"{"code":"somethingNew","message":"New.","sessionId":"s1","generation":1}"#
        default: "another computer session is already preparing on this computer"
        }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 409,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(body.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class AutomationRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "automations-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let status = request.url?.path.hasSuffix("/run") == true ? 503 : 409
        let words = status == 503 ? "The selected agent is offline. Connect the computer it runs on, then try again."
            : "Wait for this automation to finish, or stop its run in the conversation before deleting it."
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/plain"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(words.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class AgentSettingsRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "agent-settings-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 422,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/plain; charset=utf-8"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data("Write an instruction before turning it on.".utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class GitSummaryRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "git-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        // The /old thread stands for a Mac that answers without words.
        let words = request.url?.path.contains("/old/") == true ? "" : "This worktree is not one of the Project's folders."
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 403,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/plain"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(words.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class FolderRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "folder-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        // /wt/ is a worktree the Project leaves out; /gone/ a folder removed
        // from the Project; /missing/ a folder that no longer exists.
        let path = request.url!.path
        let worktree = path.contains("/wt/")
        let status = path.contains("/missing/") ? 404 : 403
        let words = worktree ? "This worktree is not one of the Project's folders. Add it to the Project on your computer to open it here."
            : status == 404 ? "Not found" : "This conversation's working folder is no longer in the project."
        var headers = ["Content-Type": "text/plain"]
        if worktree { headers[PairingAPI.errorCodeHeader] = PairingAPI.worktreeNotInProject }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status,
            httpVersion: "HTTP/1.1", headerFields: headers)!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(words.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

/// Real HTTP refusal shapes from attach, detail and history, including old hosts.
private final class ChatOpeningRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "chat-opening.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let variant = request.url?.query ?? ""
        let status = variant == "temporary" ? 503 : variant == "missing" ? 404 : variant == "helper" ? 422 : 403
        let body = switch variant {
        case "temporary": #"{"error":"This chat's Project could not be checked. Try again."}"#
        case "missing": #"{"error":"This chat's Project is no longer available."}"#
        case "helper": "Open this helper thread from its parent."
        case "old": ""
        case "oversized": String(repeating: "x", count: 513)
        case "markup": "<html>upstream error</html>"
        default: #"{"error":"This thread is outside the project's folders."}"#
        }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(body.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class PairingTrustProbe: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    private var redirects = 0
    var calls: Int { lock.withLock { count } }
    var redirectCalls: Int { lock.withLock { redirects } }
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        lock.withLock { redirects += 1 }
        completionHandler(request)
    }
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        lock.withLock { count += 1 }
        completionHandler(.cancelAuthenticationChallenge, nil)
    }
}
private final class PairingChallengeSender: NSObject, URLAuthenticationChallengeSender {
    func use(_ credential: URLCredential, for challenge: URLAuthenticationChallenge) {}
    func continueWithoutCredential(for challenge: URLAuthenticationChallenge) {}
    func cancel(_ challenge: URLAuthenticationChallenge) {}
    func performDefaultHandling(for challenge: URLAuthenticationChallenge) {}
    func rejectProtectionSpaceAndContinue(with challenge: URLAuthenticationChallenge) {}
}

final class ProtocolTests: XCTestCase {
    // Contract: host identity selects both the visible controls and actual wire
    // actions. A Windows/Linux regression to Mac Command must fail on macOS too.
    func testMacComputerControlsPreserveMissionControlAndCommandSwitcher() throws {
        let controls = ComputerPlatformControls.forHost("macos")
        XCTAssertEqual(try button("all-windows", in: controls).action, .keys([.key(key: "up", phase: "press", modifiers: 2)]))
        XCTAssertEqual(try button("app-windows", in: controls).action, .keys([.key(key: "down", phase: "press", modifiers: 2)]))
        XCTAssertEqual(try button("next-window", in: controls).action, .keys([.key(key: "`", phase: "press", modifiers: 8)]))
        XCTAssertEqual(try button("applications", in: controls).action, .applications(
            open: .key(key: "space", phase: "press", modifiers: 8),
            browse: .key(key: "1", phase: "press", modifiers: 8)))
        XCTAssertNil(controls.limitation)
        checkSwitcher(controls, key: "command", modifiers: 8)
    }

    func testWindowsComputerControlsSendWindowsKeysAndAltTab() throws {
        let controls = ComputerPlatformControls.forHost("windows")
        XCTAssertEqual(try button("win", in: controls).action, .keys([.key(key: "win", phase: "press", modifiers: 0)]))
        XCTAssertEqual(try button("ctrl", in: controls).action, .modifier("ctrl"))
        XCTAssertEqual(try button("alt", in: controls).action, .modifier("alt"))
        for (id, key) in [("task-view", "tab"), ("show-desktop", "d")] {
            XCTAssertEqual(try button(id, in: controls).action, .keys([
                .key(key: "win", phase: "down", modifiers: 0),
                .key(key: key, phase: "press", modifiers: 0),
                .key(key: "win", phase: "up", modifiers: 0)
            ]))
        }
        XCTAssertEqual(try button("right-click", in: controls).action, .rightClick)
        XCTAssertNotNil(controls.limitation)
        XCTAssertFalse(controls.rows.flatMap { $0 }.contains { $0.id == "all-windows" })
        checkSwitcher(controls, key: "alt", modifiers: 4)
    }

    func testLinuxComputerControlsSendSuperAndAltTab() throws {
        let controls = ComputerPlatformControls.forHost("linux")
        XCTAssertEqual(try button("super", in: controls).action, .modifier("super"))
        XCTAssertEqual(try button("ctrl", in: controls).action, .modifier("ctrl"))
        XCTAssertEqual(try button("alt", in: controls).action, .modifier("alt"))
        XCTAssertEqual(try button("activities", in: controls).action, .keys([.key(key: "super", phase: "press", modifiers: 0)]))
        XCTAssertEqual(try button("right-click", in: controls).action, .rightClick)
        XCTAssertNil(controls.limitation)
        checkSwitcher(controls, key: "alt", modifiers: 4)
    }

    func testLegacyAndUnknownComputerPlatformsDoNotInventShortcuts() {
        XCTAssertEqual(ComputerPlatformControls.forHost(nil), .forHost("macos"))
        XCTAssertEqual(ComputerPlatformControls.forHost("WINDOWS"), .forHost("windows"))
        let unknown = ComputerPlatformControls.forHost("other")
        XCTAssertTrue(unknown.rows.isEmpty)
        var switcher = ComputerAppSwitcher()
        XCTAssertEqual(switcher.toggle(controls: unknown), [])
        XCTAssertFalse(switcher.isPresented)
    }

    func testComputerModifierLatchTypesAKeyAndReleasesAfterIt() {
        var latch = ComputerModifierLatch()
        XCTAssertEqual(latch.toggle("ctrl"), [.key(key: "ctrl", phase: "down", modifiers: 0)])
        XCTAssertEqual(latch.applying(to: [.text("c")]), [
            .key(key: "c", phase: "press", modifiers: 0), .key(key: "ctrl", phase: "up", modifiers: 0)
        ])
        XCTAssertTrue(latch.keys.isEmpty)
        _ = latch.toggle("ctrl")
        XCTAssertEqual(latch.applying(to: [.text("C")]), [
            .key(key: "c", phase: "press", modifiers: 1), .key(key: "ctrl", phase: "up", modifiers: 0)
        ])
        _ = latch.toggle("ctrl")
        XCTAssertEqual(latch.applying(to: [.text("!")]), [
            .key(key: "1", phase: "press", modifiers: 1), .key(key: "ctrl", phase: "up", modifiers: 0)
        ])
        _ = latch.toggle("alt")
        XCTAssertNil(latch.applying(to: [.text("é")]))
        XCTAssertNil(latch.applying(to: [.text("many keys")]))
        XCTAssertEqual(latch.keys, ["alt"])
        XCTAssertEqual(latch.toggle("alt"), [.key(key: "alt", phase: "up", modifiers: 0)])
        XCTAssertEqual(latch.applying(to: [.text("é")]), [.text("é")])
        _ = latch.toggle("super")
        XCTAssertEqual(latch.release(), [.key(key: "super", phase: "up", modifiers: 0)])
    }

    /// The Duo trackpad's key row: a Mac gets ⌃ ⌥ ⌘ ⇧ latches (its shortcut row
    /// has none); ⌘ then c sends Command-C and releases Command.
    func testStickyModifiersLatchForTheNextKeyOnEachHost() throws {
        let mac = ComputerPlatformControls.stickyModifiers(forHost: nil)
        XCTAssertEqual(mac.map(\.title), ["⌃", "⌥", "⌘", "⇧"])
        let macRowModifiers = ComputerPlatformControls.forHost("macos").rows.flatMap { $0 }.filter {
            if case .modifier = $0.action { return true } else { return false }
        }
        XCTAssertTrue(macRowModifiers.isEmpty, "the Mac's modifiers live only in the sticky row")
        let command = try XCTUnwrap(mac.first { $0.id == "command" })
        guard case .modifier(let key) = command.action else { return XCTFail("Command must latch") }
        var latch = ComputerModifierLatch()
        XCTAssertEqual(latch.toggle(key), [.key(key: "command", phase: "down", modifiers: 0)])
        XCTAssertEqual(latch.applying(to: [.text("c")]), [
            .key(key: "c", phase: "press", modifiers: 0), .key(key: "command", phase: "up", modifiers: 0)
        ])
        XCTAssertTrue(latch.keys.isEmpty)
        // Windows and Linux rows already latch Ctrl/Alt/Super; their pad row adds Shift.
        for host in ["windows", "linux"] {
            XCTAssertEqual(ComputerPlatformControls.stickyModifiers(forHost: host).map(\.id), ["shift"], host)
        }
        XCTAssertTrue(ComputerPlatformControls.stickyModifiers(forHost: "other").isEmpty)
    }

    private func button(_ id: String, in controls: ComputerPlatformControls) throws -> ComputerPlatformControls.Button {
        try XCTUnwrap(controls.rows.flatMap { $0 }.first { $0.id == id })
    }

    private func checkSwitcher(_ controls: ComputerPlatformControls, key: String, modifiers: UInt32,
                               file: StaticString = #filePath, line: UInt = #line) {
        var switcher = ComputerAppSwitcher()
        let open: [ComputerInputAction] = [.key(key: key, phase: "down", modifiers: modifiers), .key(key: "tab", phase: "press", modifiers: modifiers)]
        let release: [ComputerInputAction] = [.key(key: key, phase: "up", modifiers: 0)]
        XCTAssertEqual(switcher.toggle(controls: controls), open, file: file, line: line)
        XCTAssertTrue(switcher.isPresented, file: file, line: line)
        for navigation in ["tab", "left", "right", "up", "down"] {
            XCTAssertEqual(switcher.key(navigation), [.key(key: navigation, phase: "press", modifiers: modifiers)], file: file, line: line)
            XCTAssertTrue(switcher.isPresented, file: file, line: line)
        }
        XCTAssertEqual(switcher.toggle(controls: controls), release, file: file, line: line)
        XCTAssertFalse(switcher.isPresented, file: file, line: line)
        XCTAssertEqual(switcher.finish(), [], file: file, line: line)
        _ = switcher.toggle(controls: controls)
        XCTAssertEqual(switcher.key("escape"), [.key(key: "escape", phase: "press", modifiers: modifiers)] + release, file: file, line: line)
        _ = switcher.toggle(controls: controls)
        XCTAssertEqual(switcher.key("return"), release, file: file, line: line)
        _ = switcher.toggle(controls: controls)
        XCTAssertEqual(switcher.key("delete"), [.key(key: "escape", phase: "press", modifiers: modifiers)] + release + [.key(key: "delete", phase: "press", modifiers: 0)], file: file, line: line)
    }

    func testChatOpenRefusalsKeepTheHostsReasonAndRecovery() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ChatOpeningRefusalProtocol.self]
        let api = PairingAPI(configuration: configuration)
        for (path, method) in [("/api/v1/projects/p/threads/attach", "POST"),
                               ("/api/v1/project-conversations/c", "GET"),
                               ("/api/v1/conversations/c", "GET"),
                               ("/api/v1/conversations/c/history", "GET")] {
            for variant in ["outside", "temporary", "missing", "helper", "old", "oversized", "markup"] {
                do {
                    let _: ProjectThreadSummary = try await api.request(path + "?" + variant,
                        origin: "https://chat-opening.invalid", method: method)
                    XCTFail("Expected refusal: \(path), \(variant)")
                } catch {
                    let issue = ChatOpenFailure(error, computer: "MBP")
                    XCTAssertEqual(issue.canRetry, variant == "temporary", "\(path), \(variant)")
                    switch variant {
                    case "outside":
                        guard case PairingFailure.hostMessage(403, let words) = error else { return XCTFail("Attach lost the host's reason: \(error)") }
                        XCTAssertEqual(words, "This thread is outside the project's folders.")
                        XCTAssertTrue(issue.message.hasPrefix(words))
                        XCTAssertTrue(issue.message.contains("MBP"))
                    case "temporary": XCTAssertEqual(issue.message, "This chat's Project could not be checked. Try again.")
                    case "missing": XCTAssertEqual(issue.message, "This chat's Project is no longer available.")
                    case "helper": XCTAssertEqual(issue.message, "Open this helper thread from its parent.")
                    default:
                        guard case PairingFailure.response(403) = error else { return XCTFail("Invalid or missing refusal must keep its status") }
                        XCTAssertTrue(issue.message.contains("check its Project"), "An unexplained 403 must not claim this device was disconnected")
                    }
                }
            }
        }
        // Delete shares bounded refusal parsing, while authentication and
        // invalid/oversized bodies retain the shared status-based recovery.
        for (variant, expectedWords) in [("temporary", "This chat's Project could not be checked. Try again."),
                                          ("helper", "Open this helper thread from its parent."),
                                          ("outside", ""), ("oversized", ""), ("markup", "")] {
            do {
                let _: ProjectThreadSummary = try await api.request("/api/v1/project-conversations/c?" + variant,
                    origin: "https://chat-opening.invalid", method: "DELETE")
                XCTFail("Expected Delete refusal")
            } catch {
                if expectedWords.isEmpty {
                    guard case PairingFailure.response(403) = error else { return XCTFail("Delete must preserve authentication/status handling") }
                } else {
                    guard case PairingFailure.hostMessage(_, let words) = error else { return XCTFail("Delete lost the host's recovery: \(error)") }
                    XCTAssertEqual(words, expectedWords)
                }
            }
        }
        let timeout = ChatOpenFailure(URLError(.timedOut), computer: "MBP")
        XCTAssertTrue(timeout.canRetry)
        XCTAssertTrue(timeout.message.contains("timed out"))
    }

    func testGitSummaryRefusalsCarryTheMacsReason() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [GitSummaryRefusalProtocol.self]
        let api = PairingAPI(configuration: configuration)
        do {
            let _: WorkspaceGitSummary = try await api.request("/api/v1/conversations/wt/workspace/git/summary?root=workspace",
                                                               origin: "https://git-test.invalid")
            XCTFail("Expected refusal")
        } catch PairingFailure.hostMessage(let status, let detail) {
            XCTAssertEqual(status, 403); XCTAssertEqual(detail, "This worktree is not one of the Project's folders.")
        }
        do {
            let _: WorkspaceGitSummary = try await api.request("/api/v1/conversations/old/workspace/git/summary?root=workspace",
                                                               origin: "https://git-test.invalid")
            XCTFail("Expected refusal")
        } catch PairingFailure.response(let status) { XCTAssertEqual(status, 403) }
    }

    /// Opening a computer view another device holds names that device so the
    /// phone can offer Take over; an older host's plain 409 stays a status.
    func testComputerViewOpenRefusalsSayWhoIsViewing() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ComputerViewerConflictProtocol.self]
        let api = PairingAPI(configuration: configuration)
        func open(_ query: String, method: String = "POST") async -> Error? {
            do {
                let _: ComputerSession = try await api.request("/api/v1/computer/sessions?" + query, origin: "https://computer-view-test.invalid",
                                                               body: method == "POST" ? Data("{}".utf8) : nil, method: method)
                return nil
            } catch { return error }
        }
        func isStatus409(_ error: Error?) -> Bool {
            if let error, case PairingFailure.response(409) = error { return true }
            return false
        }
        let busyError = await open("case=busy")
        let busy = try XCTUnwrap(busyError as? ComputerViewerConflict)
        XCTAssertEqual(busy, ComputerViewerConflict(code: .viewerBusy, deviceName: "Sam’s iPad", sessionId: "s1", generation: 2))
        XCTAssertEqual(busy.errorDescription, "This computer is being viewed on Sam’s iPad.")
        XCTAssertEqual(busy.replacement, ComputerViewerReplacement(sessionId: "s1", generation: 2))
        let unnamedError = await open("case=unnamed")
        let unnamed = try XCTUnwrap(unnamedError as? ComputerViewerConflict)
        XCTAssertEqual(unnamed.errorDescription, "This computer is being viewed on another device.")
        let changedError = await open("case=changed")
        let changed = try XCTUnwrap(changedError as? ComputerViewerConflict)
        XCTAssertEqual(changed.code, .viewChanged)
        XCTAssertEqual(changed.errorDescription, "The computer view changed on another device. Try again.")
        for query in ["case=unknown", "case=plain"] {
            let error = await open(query)
            XCTAssertTrue(isStatus409(error), query)
        }
        // Only opening a view reads the body; other requests on the path keep the status.
        let read = await open("case=busy", method: "GET")
        XCTAssertTrue(isStatus409(read), "GET")
    }

    func testComputerViewTakeOverNamesTheViewItEnds() throws {
        func encoded(_ request: StartComputerSessionRequest) throws -> [String: Any] {
            try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any])
        }
        let ordinary = try encoded(StartComputerSessionRequest(clientRequestId: "r", conversationId: "c", hostInstallationId: "mac"))
        XCTAssertNil(ordinary["replaces"]); XCTAssertNil(ordinary["takeOver"])
        let takeOver = try encoded(StartComputerSessionRequest(clientRequestId: "r", conversationId: "c", hostInstallationId: "mac",
                                                               takingOver: ComputerViewerReplacement(sessionId: "s1", generation: 2)))
        XCTAssertEqual(takeOver["takeOver"] as? Bool, true)
        let replaces = try XCTUnwrap(takeOver["replaces"] as? [String: Any])
        XCTAssertEqual(replaces["sessionId"] as? String, "s1")
        XCTAssertEqual(replaces["generation"] as? Int, 2)
    }

    // A refused provider Update says why in the Mac's words (ChatGPT updates
    // its own Codex, an update is already running), as Reconnect does.
    func testProviderUpdateAndReconnectRefusalsCarryTheMacsReason() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ProviderRefusalProtocol.self]
        let api = PairingAPI(configuration: configuration)
        for path in ["/api/v1/providers/codex/update", "/api/v1/providers/claude/reconnect"] {
            do {
                let _: SendReceipt = try await api.request(path, origin: "https://providers-test.invalid", method: "POST")
                XCTFail("Expected refusal")
            } catch PairingFailure.hostMessage(let status, let detail) {
                XCTAssertEqual(status, 409, path); XCTAssertEqual(detail, "An agent is working. Try again after it finishes.", path)
            }
        }
    }

    // Settings > computer > provider shows the Mac's own reason in place when
    // it refuses a custom instruction or thread tools change.
    func testCustomInstructionAndThreadToolRefusalsCarryTheMacsReason() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [AgentSettingsRefusalProtocol.self]
        let api = PairingAPI(configuration: configuration)
        for (path, body) in [(CustomInstructionSettings.path(.codex), CustomInstructionSettings.request(enabled: true, text: "")),
                             (ThreadToolSettings.path(.claude), ThreadToolSettings.request(enabled: false))] {
            do {
                let _: SendReceipt = try await api.request(path, origin: "https://agent-settings-test.invalid", body: body, method: "PUT")
                XCTFail("Expected refusal")
            } catch PairingFailure.hostMessage(let status, let detail) {
                XCTAssertEqual(status, 422, path); XCTAssertEqual(detail, "Write an instruction before turning it on.", path)
            }
        }
    }

    // Run now and Delete show the Mac's reason; an edit conflict stays a bare
    // 409 so the editor can offer to reload the latest version.
    func testAutomationRunAndDeleteRefusalsCarryTheMacsReason() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [AutomationRefusalProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let origin = "https://automations-test.invalid"
        do {
            let _: SendReceipt = try await api.request("/api/v1/automations/a/run", origin: origin, body: Data("{}".utf8))
            XCTFail("Expected refusal")
        } catch PairingFailure.hostMessage(let status, let detail) {
            XCTAssertEqual(status, 503); XCTAssertTrue(detail.hasPrefix("The selected agent is offline."))
        }
        do {
            let _: SendReceipt = try await api.request("/api/v1/automations/a", origin: origin, method: "DELETE")
            XCTFail("Expected refusal")
        } catch PairingFailure.hostMessage(let status, let detail) {
            XCTAssertEqual(status, 409); XCTAssertTrue(detail.hasPrefix("Wait for this automation to finish"))
        }
        do {
            let _: SendReceipt = try await api.request("/api/v1/automations/a", origin: origin, body: Data("{}".utf8), method: "PATCH")
            XCTFail("Expected refusal")
        } catch PairingFailure.response(let status) { XCTAssertEqual(status, 409) }
    }

    func testOnlyDefinitiveSkillRefusalRestoresEditableIntent() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [SkillRejectionProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let messages = "/api/v1/conversations/chat/messages"
        let steer = "/api/v1/conversations/chat/turns/turn/steer"
        for (host, path, expected) in [("skill-test.invalid", messages, 422), ("roots.skill-test.invalid", messages, 412), ("skill-test.invalid", steer, 422)] {
            do {
                let _: SendReceipt = try await api.request(path, origin: "https://" + host, body: Data("{}".utf8))
                XCTFail("Expected refusal")
            } catch PairingFailure.skillRejected(let status, let detail) {
                XCTAssertEqual(status, expected); XCTAssertEqual(detail, "Choose the skill again.")
            }
        }
        for (host, path, expected) in [("ordinary.skill-test.invalid", messages, 422), ("uncertain.skill-test.invalid", messages, 503),
                                      ("conflict.skill-test.invalid", messages, 409), ("oversized.skill-test.invalid", messages, 422),
                                      ("skill-test.invalid", "/api/v1/projects/project/skills", 422)] {
            do {
                let _: SendReceipt = try await api.request(path, origin: "https://" + host, body: Data("{}".utf8))
                XCTFail("Expected failure")
            } catch PairingFailure.response(let status) { XCTAssertEqual(status, expected) }
        }
    }

    func testOnlyAnnotationMessageConflictCarriesReannotationRecovery() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [AnnotationRejectionProtocol.self]
        let api = PairingAPI(configuration: configuration)
        for host in ["annotation-test.invalid", "roots-revision-test.invalid", "removed-root-test.invalid"] {
            do {
                let _: SendReceipt = try await api.request("/api/v1/conversations/chat/messages",
                    origin: "https://" + host, body: Data("{}".utf8))
                XCTFail("The stale annotation must be rejected: \(host)")
            } catch PairingFailure.annotationRejected(let status, let detail) {
                XCTAssertEqual(status, 409)
                XCTAssertTrue(detail.contains("Reopen"))
            }
        }
        do {
            let _: SendReceipt = try await api.request("/api/v1/conversations/chat/messages",
                origin: "https://ordinary-conflict.invalid", body: Data("{}".utf8))
            XCTFail("The ordinary conflict must be rejected.")
        } catch PairingFailure.response(let status) { XCTAssertEqual(status, 409) }
    }
    func testComputerSignalingFixturesRoundTripAndKeepIceServerShape() throws {
        let binding = ComputerSignalingBinding(
            generation: 7,
            conversationId: "conversation",
            hostInstallationId: "host",
            peerRevision: "peer-1"
        )
        let encoder = JSONEncoder()
        let poll = try JSONSerialization.jsonObject(
            with: encoder.encode(ComputerSignalingPollRequest(binding: binding, cursor: 4))
        ) as! [String: Any]
        XCTAssertEqual(poll["generation"] as? Int, 7)
        XCTAssertEqual(poll["cursor"] as? Int, 4)
        XCTAssertEqual(poll["conversationId"] as? String, "conversation")
        XCTAssertEqual(poll["peerRevision"] as? String, "peer-1")

        let candidate = try XCTUnwrap(ComputerSignalingCandidateRequest(
            binding: binding,
            sequence: 3,
            candidate: "candidate:1 1 udp 1 192.0.2.1 9 typ host",
            sdpMid: "0",
            sdpMLineIndex: 0,
            usernameFragment: "ufrag"
        ))
        let candidateData = try encoder.encode(candidate)
        let decodedCandidate = try JSONDecoder().decode(
            ComputerSignalingCandidateRequestFixture.self,
            from: candidateData
        )
        XCTAssertEqual(decodedCandidate.sequence, 3)
        XCTAssertEqual(decodedCandidate.sdpMLineIndex, 0)
        XCTAssertEqual(decodedCandidate.usernameFragment, "ufrag")
        XCTAssertEqual(decodedCandidate.peerRevision, "peer-1")
        let unbound = ComputerSignalingBinding(
            generation: 7,
            conversationId: "conversation",
            hostInstallationId: "host"
        )
        XCTAssertNil(ComputerSignalingAnswerRequest(binding: unbound, sdp: "v=0\na=x"))
        XCTAssertNil(ComputerSignalingCloseRequest(binding: unbound))

        let responseData = Data(#"{"sessionId":"session","generation":7,"peerRevision":"peer-1","state":"offerReady","captureState":"ready","peerState":"new","offer":{"type":"offer","sdp":"v=0\na=group:BUNDLE 0"},"candidates":[{"sequence":1,"candidate":"candidate:1","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":null}],"cursor":0,"nextCursor":1,"failureReason":null,"iceServers":[]}"#.utf8)
        let response = try JSONDecoder().decode(ComputerSignalingResponse.self, from: responseData)
        XCTAssertEqual(response.sessionId, "session")
        XCTAssertEqual(response.peerRevision, "peer-1")
        XCTAssertEqual(response.nextCursor, 1)
        XCTAssertTrue(response.iceServers.isEmpty)
    }

    /// Simulator touch and button actions in the host's wire form
    /// (wonder-http-v1 `touch`/`button`), and the host's refusal of text a
    /// simulator cannot type, which keeps the lease.
    func testSimulatorActionsUseTheHostsWireFormAndTextRefusalDecodes() throws {
        let actions: [ComputerInputAction] = [.touch(x: 0.25, y: 0.75, phase: "up", delayMs: 800),
                                              .touch(x: 0.5, y: 0.5, phase: "down", delayMs: nil),
                                              .button(name: "volumeUp", phase: "press")]
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let json = String(decoding: try encoder.encode(actions), as: UTF8.self)
        XCTAssertEqual(json, #"[{"delayMs":800,"phase":"up","type":"touch","x":0.25,"y":0.75},"#
                       + #"{"phase":"down","type":"touch","x":0.5,"y":0.5},{"button":"volumeUp","phase":"press","type":"button"}]"#)
        XCTAssertEqual(try JSONDecoder().decode([ComputerInputAction].self, from: Data(json.utf8)), actions)
        let refusal = Data(#"{"granted":false,"acknowledged":false,"status":"unsupported","reason":"The simulator can only type letters, numbers and symbols from a US keyboard. Paste other text instead.","lease":null,"control":{"available":true,"action":"none","reason":"Take control to use this simulator from here.","heartbeatIntervalSeconds":3,"leaseExpirySeconds":10}}"#.utf8)
        let response = try JSONDecoder().decode(ComputerControlActionResponse.self, from: refusal)
        XCTAssertEqual(response.status, "unsupported")
        XCTAssertNil(response.lease)
        XCTAssertTrue(response.control.available)
    }

    func testComputerControlModelsRoundTripClipboardTextAndSequencedBatch() throws {
        let actions: [ComputerInputAction] = [
            .pointer(x: 0.25, y: 0.5, phase: "down", button: "left"),
            .pointer(x: 0.25, y: 0.5, phase: "up", button: "left")
        ]
        let request = ComputerInputBatchRequest(
            leaseId: "lease",
            generation: 4,
            geometryRevision: 9,
            sourceId: "display:1",
            sequence: 12,
            actions: actions
        )
        let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as! [String: Any]
        XCTAssertEqual(object["leaseId"] as? String, "lease")
        XCTAssertEqual(object["generation"] as? Int, 4)
        XCTAssertEqual(object["geometryRevision"] as? Int, 9)
        XCTAssertEqual(object["sequence"] as? Int, 12)
        XCTAssertEqual((object["actions"] as? [[String: Any]])?.count, 2)
        XCTAssertEqual((object["actions"] as? [[String: Any]])?.first?["phase"] as? String, "down")

        let responseData = Data(#"{"granted":false,"acknowledged":true,"status":"active","reason":"Input accepted.","lease":{"id":"lease","sessionId":"session","ownerDeviceId":"phone","hostInstallationId":"mac","conversationId":"chat","generation":4,"sourceId":"display:1","geometryRevision":9,"status":"active","lastSequence":12,"acquiredAt":"now","updatedAt":"later","expiresAt":"later","releasedAt":null},"control":{"available":true,"action":"none","reason":"Control is active.","heartbeatIntervalSeconds":3,"leaseExpirySeconds":10},"clipboardText":"copied from Mac"}"#.utf8)
        let response = try JSONDecoder().decode(ComputerControlActionResponse.self, from: responseData)
        XCTAssertTrue(response.acknowledged)
        XCTAssertEqual(response.lease?.lastSequence, 12)
        XCTAssertEqual(response.clipboardText, "copied from Mac")

        let paste = ComputerInputBatchRequest(
            leaseId: "lease", generation: 4, geometryRevision: 9, sourceId: "display:1", sequence: 13,
            actions: [.clipboard(operation: "pasteFromPhone", text: "hello")]
        )
        let pasteObject = try JSONSerialization.jsonObject(with: JSONEncoder().encode(paste)) as! [String: Any]
        let pasteAction = try XCTUnwrap((pasteObject["actions"] as? [[String: Any]])?.first)
        XCTAssertEqual(pasteAction["operation"] as? String, "pasteFromPhone")
        XCTAssertEqual(pasteAction["text"] as? String, "hello")
    }

    func testControlConsentUsesItsFullLocalApprovalWindow() {
        XCTAssertEqual(
            PairingAPI.timeoutInterval(for: "/api/v1/computer/sessions/session/control/acquire"),
            135
        )
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/computer/sessions/session/control/input"), 15)
    }

    func testAttachmentUploadsGetALongerTimeoutThanListingFiles() {
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/conversations/chat/files", method: "POST"), 120)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/conversations/chat/files"), 15)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/conversations/chat/messages", method: "POST"), 15)
    }

    // A Mac that scans Codex's saved threads answers a project's thread list in
    // 13-24 s (measured); the list must not time out at 15 s first.
    func testProjectThreadListsOutwaitTheMacsCodexRead() {
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/projects/p/threads?limit=5"), 60)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/projects/p/threads?limit=10&cursor=c"), 60)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/projects/p/threads", method: "POST"), 15)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/projects/p/threads/archived"), 15)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/projects"), 15)
    }

    // The Mac checks each provider's install and sign-in for up to 20 s (the
    // two in parallel). Reconnect verifies and starts the runtime (Codex: up
    // to 30 s to verify plus 30 s to answer; Claude: up to 30 s) and then
    // checks again; the phone must not give up first.
    func testT3ConnectOutwaitsCommandApprovalAndItsSingleRetry() {
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/t3-code/connect", method: "POST"), 150)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/t3-code/connect?browser=true", method: "POST"), 150)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/t3-code"), 15)
    }

    func testProviderRequestsOutwaitTheMacsChecks() {
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/providers"), 30)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/providers?refresh=true"), 30)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/providers/claude/reconnect", method: "POST"), 120)
        XCTAssertEqual(PairingAPI.timeoutInterval(for: "/api/v1/providers/codex/reconnect", method: "POST"), 120)
    }

    func testResponseBodiesAreCollectedUpToTheirLimit() async throws {
        func body(_ count: Int) -> AsyncStream<UInt8> {
            AsyncStream { continuation in
                for index in 0..<count { continuation.yield(UInt8(truncatingIfNeeded: index)) }
                continuation.finish()
            }
        }
        // Around the 64 KB chunk boundary and exactly at the limit.
        for count in [0, 1, 65_535, 65_536, 65_537, 140_000] {
            let data = try await PairingAPI.collect(body(count), limit: 140_000, expected: count, exceeded: FileFailure.tooLarge)
            XCTAssertEqual(data, Data((0..<count).map { UInt8(truncatingIfNeeded: $0) }))
        }
        for count in [140_001, 200_000] {
            do {
                _ = try await PairingAPI.collect(body(count), limit: 140_000, expected: nil, exceeded: FileFailure.tooLarge)
                XCTFail("\(count) bytes must exceed the limit")
            } catch FileFailure.tooLarge {}
        }
    }

    func testControlRequestCanDecodeAnExplicitStructuredConflict() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ComputerControlResponseProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let response: ComputerControlActionResponse = try await api.request(
            "/api/v1/computer/sessions/session/control/acquire",
            origin: "https://computer-control-test.invalid",
            decodingStatuses: [409]
        )
        XCTAssertEqual(response.status, "rejected")
        XCTAssertTrue(response.acknowledged)
        XCTAssertEqual(response.reason, "Control was not allowed.")

        do {
            let _: ComputerControlActionResponse = try await api.request(
                "/api/v1/computer/sessions/session/control/acquire",
                origin: "https://computer-control-test.invalid"
            )
            XCTFail("A conflict must not decode unless its status is explicitly allowed.")
        } catch PairingFailure.response(let status) {
            XCTAssertEqual(status, 409)
        }
    }

    private struct ComputerSignalingCandidateRequestFixture: Decodable {
        let peerRevision: String?
        let sequence: UInt64
        let sdpMLineIndex: UInt32
        let usernameFragment: String?
    }
    func testSavedConnectionNameSurvivesOfflineAndOlderConnectionsDecode() throws {
        let legacy = Data(#"{"origin":"https://mac.example","credential":{"sessionToken":"test","deviceId":"phone","csrfToken":"test","hostInstallationId":"mac"}}"#.utf8)
        let saved = try JSONDecoder().decode(SavedConnection.self, from: legacy)
        XCTAssertNil(saved.hostName)
        let named = SavedConnection(origin: saved.origin, credential: saved.credential, hostName: "Studio Mac")
        let restored = try JSONDecoder().decode(SavedConnection.self, from: JSONEncoder().encode(named))
        XCTAssertEqual(restored.hostName, "Studio Mac")
        XCTAssertEqual(restored.credential.hostInstallationId, "mac")
    }

    func testOlderComputerSessionWithoutControlCapabilityStillDecodes() throws {
        let legacy = Data(#"{"id":"session","clientRequestId":"request","ownerDeviceId":"phone","hostInstallationId":"mac","conversationId":"chat","generation":1,"state":"unavailable","source":{"id":null,"name":null,"kind":null,"width":null,"height":null,"scale":null,"crop":null},"geometryRevision":0,"failureReason":"Unavailable","createdAt":"now","updatedAt":"now","lastStateAt":"now","endedAt":null,"capability":{"available":false,"action":"update-host","reason":"Unavailable"}}"#.utf8)
        let session = try JSONDecoder().decode(ComputerSession.self, from: legacy)
        XCTAssertNil(session.control)
        XCTAssertNil(session.videoQuality)
        XCTAssertNil(session.supportsLiveQualityChange)
        XCTAssertEqual(session.state, .unavailable)
    }

    func testComputerVideoQualityRequestAndResponseRoundTrip() throws {
        let request = StartComputerSessionRequest(
            clientRequestId: "request", conversationId: "chat", hostInstallationId: "mac",
            source: ComputerSource(id: "display:1"), videoQuality: .medium
        )
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any])
        XCTAssertEqual(body["videoQuality"] as? String, "auto")
        XCTAssertEqual(body["videoMaxHeight"] as? Int, 1_080)
        XCTAssertEqual((body["source"] as? [String: Any])?["id"] as? String, "display:1")
        let response = Data(#"{"id":"session","clientRequestId":"request","ownerDeviceId":"phone","hostInstallationId":"mac","conversationId":"chat","generation":1,"state":"live","source":{"id":null,"name":null,"kind":null,"width":null,"height":null,"scale":null,"crop":null},"geometryRevision":0,"failureReason":null,"createdAt":"now","updatedAt":"now","lastStateAt":"now","endedAt":null,"capability":{"available":true,"action":"none","reason":"Available"},"videoQuality":"medium"}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(ComputerSession.self, from: response).videoQuality, .medium)
        let quality = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(
            ComputerSessionQualityRequest(generation: 1, videoQuality: .medium)
        )) as? [String: Any])
        XCTAssertEqual(quality["videoQuality"] as? String, "auto")
        XCTAssertEqual(quality["videoMaxHeight"] as? Int, 1_080)
    }

    func testComputerFrameRateCeilingIsOmittedForOldCallersAndClampedToTheSupportedRange() throws {
        func body<T: Encodable>(_ value: T) throws -> [String: Any] {
            try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(value)) as? [String: Any])
        }
        let old = try body(StartComputerSessionRequest(clientRequestId: "r", conversationId: "c", hostInstallationId: "m"))
        XCTAssertNil(old["videoMaxFramesPerSecond"])
        XCTAssertNil(try body(ComputerSessionQualityRequest(generation: 1, videoQuality: .high))["videoMaxFramesPerSecond"])
        for (asked, sent) in [(30, 30), (60, 60), (120, 120), (5, 15), (240, 120)] {
            let start = try body(StartComputerSessionRequest(
                clientRequestId: "r", conversationId: "c", hostInstallationId: "m", videoMaxFramesPerSecond: asked))
            XCTAssertEqual(start["videoMaxFramesPerSecond"] as? Int, sent)
            let quality = try body(ComputerSessionQualityRequest(
                generation: 1, videoQuality: .high, videoMaxFramesPerSecond: asked))
            XCTAssertEqual(quality["videoMaxFramesPerSecond"] as? Int, sent)
        }
        let withCeiling = Data(#"{"id":"session","clientRequestId":"request","ownerDeviceId":"phone","hostInstallationId":"mac","conversationId":"chat","generation":1,"state":"live","source":{"id":null,"name":null,"kind":null,"width":null,"height":null,"scale":null,"crop":null},"geometryRevision":0,"failureReason":null,"createdAt":"now","updatedAt":"now","lastStateAt":"now","endedAt":null,"capability":{"available":true,"action":"none","reason":"Available"},"videoMaxFramesPerSecond":60}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(ComputerSession.self, from: withCeiling).videoMaxFramesPerSecond, 60)
        let legacy = Data(#"{"id":"session","clientRequestId":"request","ownerDeviceId":"phone","hostInstallationId":"mac","conversationId":"chat","generation":1,"state":"unavailable","source":{"id":null,"name":null,"kind":null,"width":null,"height":null,"scale":null,"crop":null},"geometryRevision":0,"failureReason":"Unavailable","createdAt":"now","updatedAt":"now","lastStateAt":"now","endedAt":null,"capability":{"available":false,"action":"update-host","reason":"Unavailable"}}"#.utf8)
        XCTAssertNil(try JSONDecoder().decode(ComputerSession.self, from: legacy).videoMaxFramesPerSecond)
    }

    func testRejectsUntrustedLinks() throws {
        for value in ["http://example.com", "https://user@example.com", "https://example.com/path", "https://example.com?secret=x", "https://example.com#secret=x"] {
            XCTAssertThrowsError(try PairingLink.origin(value))
        }
        let link = try PairingLink("https://example.com/pair#secret=abc&offerId=00000000-0000-0000-0000-000000000001&hostInstallationId=mac-1")
        XCTAssertEqual(link.origin, "https://example.com")
        XCTAssertEqual(link.hostID, "mac-1")
        XCTAssertThrowsError(try PairingLink("https://example.com/pair#secret=abc&offerId=00000000-0000-0000-0000-000000000001"))
    }
    /// The computer prints a bare address; https:// is added only when no scheme
    /// was typed, and nothing else a full URL could smuggle in gets through.
    func testTypedComputerAddressGetsHTTPSButKeepsTheOriginRules() throws {
        for (typed, origin) in [("studio.tail1234.ts.net", "https://studio.tail1234.ts.net"),
                                (" 100.64.1.2:3778\n", "https://100.64.1.2:3778"),
                                ("[fd7a:115c:a1e0::1]:3778", "https://[fd7a:115c:a1e0::1]:3778"),
                                ("192.168.1.20:3778/", "https://192.168.1.20:3778"),
                                ("https://studio.tail1234.ts.net:3778", "https://studio.tail1234.ts.net:3778")] {
            XCTAssertEqual(try PairingLink.address(typed), origin, typed)
        }
        for typed in ["", "http://studio.tail1234.ts.net", "ftp://studio.tail1234.ts.net", "user@studio.tail1234.ts.net",
                      "user:secret@192.168.1.20", "studio.tail1234.ts.net/path", "studio.tail1234.ts.net?code=x",
                      "studio.tail1234.ts.net#code=x", "mailto:me@example.com"] {
            XCTAssertThrowsError(try PairingLink.address(typed), typed)
        }
        // The added scheme makes a home address recognisable, not trusted.
        XCTAssertTrue(HomeNetworkTrust.isHomeOrigin(try PairingLink.address("192.168.1.20:3778")))
        XCTAssertFalse(HomeNetworkTrust.isHomeOrigin(try PairingLink.address("100.64.1.2:3778")))
        XCTAssertEqual(PairingFailure.homeNetworkCode.errorDescription,
                       "On the same Wi-Fi, scan the QR code or paste the pairing link; a code works with a Tailscale address.")
    }
    /// The QR anchor is optional for legacy links, but ambiguous or malformed
    /// pins cannot reach the network or consume a one-use offer.
    func testPairingLinksAcceptOnlyCanonicalSHA256HostKeys() throws {
        let link = "https://192.168.1.20:3778/pair#secret=abc&offerId=00000000-0000-0000-0000-000000000001&hostInstallationId=mac-1"
        let pin = Data(repeating: 255, count: 32).base64URL
        XCTAssertNil(try PairingLink(link).hostKey)
        let parsed = try PairingLink(link + "&hostKey=" + pin)
        XCTAssertEqual(parsed.hostKey, pin)
        XCTAssertEqual(parsed.origin, "https://192.168.1.20:3778")
        XCTAssertEqual(parsed.hostID, "mac-1")
        XCTAssertEqual(parsed.offerID, "00000000-0000-0000-0000-000000000001")
        XCTAssertEqual(parsed.secret, "abc")
        for malformed in ["", "short", pin + "=", String(pin.dropLast()), String(pin.dropLast()) + "9",
                          String(repeating: "/", count: 43), String(repeating: "a", count: 44)] {
            XCTAssertThrowsError(try PairingLink(link + "&hostKey=" + malformed), malformed)
        }
        for suffix in ["&hostKey", "&unknown=" + pin, "&hostKey=" + pin + "&hostKey=" + pin,
                       "&hostKey=" + pin + "&secret=other"] {
            XCTAssertThrowsError(try PairingLink(link + suffix), suffix)
        }
    }

    /// Installing custom TLS trust must never install its redirect policy.
    /// The wrapper still refuses a new request carrying a pairing secret/session.
    func testCustomTrustKeepsRedirectRefusalAndOtherAuthenticationDefault() {
        let probe = PairingTrustProbe()
        let wrapper = NoRedirect(serverTrustDelegate: probe)
        let session = URLSession(configuration: .ephemeral)
        defer { session.invalidateAndCancel() }
        var request = URLRequest(url: URL(string: "https://other.example/claim")!)
        request.httpBody = Data("pairing-secret".utf8)
        request.setValue("private-session", forHTTPHeaderField: "Cookie")
        let task = session.dataTask(with: request)
        let response = HTTPURLResponse(url: URL(string: "https://paired.example/claim")!, statusCode: 302,
                                       httpVersion: nil, headerFields: ["Location": request.url!.absoluteString])!
        wrapper.urlSession(session, task: task, willPerformHTTPRedirection: response, newRequest: request) { redirected in
            XCTAssertNil(redirected)
        }
        for method in [NSURLAuthenticationMethodServerTrust, NSURLAuthenticationMethodHTTPBasic] {
            let space = URLProtectionSpace(host: "192.168.1.20", port: 3778, protocol: "https", realm: nil, authenticationMethod: method)
            let challenge = URLAuthenticationChallenge(protectionSpace: space, proposedCredential: nil, previousFailureCount: 0,
                failureResponse: nil, error: nil, sender: PairingChallengeSender())
            wrapper.urlSession(session, didReceive: challenge) { disposition, credential in
                XCTAssertEqual(disposition, method == NSURLAuthenticationMethodServerTrust ? .cancelAuthenticationChallenge : .performDefaultHandling)
                XCTAssertNil(credential)
            }
            NoRedirect().urlSession(session, didReceive: challenge) { disposition, credential in
                XCTAssertEqual(disposition, .performDefaultHandling)
                XCTAssertNil(credential)
            }
        }
        XCTAssertEqual(probe.calls, 1)
        XCTAssertEqual(probe.redirectCalls, 0)
    }

    // Contract: Files and Terminal carry the Mac's own reason, and the
    // worktree refusal is told apart so the app offers Add folder, not Try again.
    func testFilesAndTerminalRefusalsCarryTheMacsReasonAndTheWorktreeCase() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [FolderRefusalProtocol.self]
        let api = PairingAPI(configuration: configuration)
        struct Reply: Decodable, Sendable {}
        func failure(_ path: String, method: String? = nil) async -> Error? {
            do { let _: Reply = try await api.request(path, origin: "https://folder-test.invalid", body: method == nil ? nil : Data("{}".utf8)); return nil }
            catch { return error }
        }
        for path in ["/api/v1/conversations/wt/workspace/roots", "/api/v1/conversations/wt/workspace/directory?root=r&path=",
                     "/api/v1/conversations/wt/terminals"] {
            guard case PairingFailure.worktreeNotInProject(let words)? = await failure(path, method: path.hasSuffix("/terminals") ? "POST" : nil) else {
                return XCTFail("\(path) must name the worktree case")
            }
            XCTAssertTrue(words.hasPrefix("This worktree is not one of the Project's folders."))
        }
        guard case PairingFailure.hostMessage(403, let words)? = await failure("/api/v1/conversations/gone/workspace/roots") else {
            return XCTFail("Files must carry the Mac's reason")
        }
        XCTAssertEqual(words, "This conversation's working folder is no longer in the project.")
        // A missing folder keeps its status so Files can return to the root.
        guard case PairingFailure.response(404)? = await failure("/api/v1/conversations/missing/workspace/directory?root=r&path=a") else {
            return XCTFail("A missing folder keeps its 404")
        }
        // The Git pill keeps reading the Mac's words for the same refusal.
        guard case PairingFailure.hostMessage(403, _)? = await failure("/api/v1/conversations/wt/workspace/git/summary?root=r") else {
            return XCTFail("The Git pill's refusal stays a host message")
        }
    }

    // Contract: the phone's signed action matches the host's
    // `ActionTranscript` byte for byte; `[]` hashes as the host hashes json!([]).
    func testSignedDeviceRevokeMatchesTheHostsActionTranscript() throws {
        let data = Data(#"{"origin":"https://wonder.example.ts.net","credential":{"hostInstallationId":"install-1","deviceId":"device-01","sessionToken":"s","csrfToken":"session-1"}}"#.utf8)
        let saved = try JSONDecoder().decode(SavedConnection.self, from: data)
        let transcript = try SignedAction.transcript(action: "device.revoke", path: "/api/v1/devices/device-01/revoke", values: [],
                                                     nonce: "nonce-1", connection: saved, issuedAtMs: 1000)
        XCTAssertEqual(String(decoding: transcript, as: UTF8.self),
                       "wonder-action-v1\ndevice.revoke\n/api/v1/devices/device-01/revoke\n4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945\nnonce-1\nsession-1\ndevice-01\ninstall-1\n1000\nactive")
        let body = try JSONSerialization.jsonObject(with: SignedAction.body(nonce: "nonce-1", issuedAtMs: 1000, signature: "sig")) as? [String: Any]
        XCTAssertEqual(body?["actionNonce"] as? String, "nonce-1")
        XCTAssertEqual(body?["issuedAtMs"] as? Int, 1000)
        XCTAssertEqual(body?["signature"] as? String, "sig")
    }

    func testRustTranscriptAndHostBinding() throws {
        let data = Data(#"{"challengeId":"challenge-01","deviceId":"device-01","nonce":"nonce-01","origin":"https://wonder.example.ts.net","hostInstallationId":"install-1","offerId":"offer-1","issuedAtMs":1000,"expiresAtMs":61000}"#.utf8)
        let challenge = try JSONDecoder().decode(Challenge.self, from: data)
        XCTAssertEqual(String(decoding: challenge.transcript, as: UTF8.self), "wonder-session-v1\ndevice-01\nchallenge-01\nnonce-01\nhttps://wonder.example.ts.net\ninstall-1\n1000\n61000")
        try challenge.validate(origin: challenge.origin, hostID: "install-1", deviceID: "device-01", now: 2000)
        XCTAssertThrowsError(try challenge.validate(origin: "https://wrong.example", hostID: "install-1", deviceID: "device-01", now: 2000))
        XCTAssertThrowsError(try challenge.validate(origin: challenge.origin, hostID: "wrong-mac", deviceID: "device-01", now: 2000))
        XCTAssertThrowsError(try challenge.validate(origin: challenge.origin, hostID: "install-1", deviceID: "wrong-phone", now: 2000))
        XCTAssertThrowsError(try challenge.validate(origin: challenge.origin, hostID: "install-1", deviceID: "device-01", now: 61000))
    }
}
