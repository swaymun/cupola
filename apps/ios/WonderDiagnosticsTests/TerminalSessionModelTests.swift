import XCTest
import SwiftTerm
import UIKit
import WonderPairing
@testable import Wonder

/// The terminal sheet's session model against a scripted Mac: it reattaches to
/// the conversation's running shell, resumes from the last drawn byte after a
/// dropped connection, sends typing made while reconnecting once the Mac is
/// back, and stops when the Mac closed the terminal. It says why instead of
/// reconnecting forever (the relay, a refused device), ends only what the
/// computer confirmed, and bounds typing and clipboard writes.
@MainActor final class TerminalSessionModelTests: XCTestCase {
    private final class InputRecorder: NSObject, @preconcurrency TerminalViewDelegate {
        var bytes: [UInt8] = []
        func send(source: TerminalView, data: ArraySlice<UInt8>) { bytes += data }
        func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {}
        func setTerminalTitle(source: TerminalView, title: String) {}
        func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        func scrolled(source: TerminalView, position: Double) {}
        func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {}
        func bell(source: TerminalView) {}
        func clipboardCopy(source: TerminalView, content: Data) {}
        func clipboardRead(source: TerminalView) -> Data? { nil }
        func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
        func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }

    func testSharedGridAttachPreservesProofAndOptsIn() throws {
        let attach = TerminalAttach(deviceId: "device", csrfToken: "csrf", challengeId: "challenge",
                                    signature: "signature", sequence: 42, cols: 80, rows: 24)
        let data = try JSONEncoder().encode(SharedGridAttach(attach: attach))
        let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(fields["grid"] as? Bool, true)
        XCTAssertEqual(fields["deviceId"] as? String, "device")
        XCTAssertEqual(fields["csrfToken"] as? String, "csrf")
        XCTAssertEqual(fields["challengeId"] as? String, "challenge")
        XCTAssertEqual(fields["signature"] as? String, "signature")
        XCTAssertEqual(fields["sequence"] as? Int, 42)
        XCTAssertEqual(fields["cols"] as? Int, 80)
        XCTAssertEqual(fields["rows"] as? Int, 24)
    }

    func testHostGridFramesValidateDimensionsAndLeaveLegacyMessagesAlone() throws {
        XCTAssertEqual(try TerminalHostGrid.read(#"{"type":"grid","cols":120,"rows":40}"#),
                       TerminalHostGrid(cols: 120, rows: 40))
        XCTAssertNil(try TerminalHostGrid.read(#"{"type":"closed"}"#))
        for frame in [#"{"type":"grid","cols":0,"rows":24}"#,
                      #"{"type":"grid","cols":501,"rows":24}"#,
                      #"{"type":"grid","cols":80,"rows":301}"#,
                      #"{"type":"grid","cols":80}"#] {
            XCTAssertThrowsError(try TerminalHostGrid.read(frame), frame)
        }
    }

    func testHostGridIsAppliedBeforeOutputAndNeverEchoedAsAResize() async {
        let mac = ScriptedMac()
        let socket = ScriptedConnection([Self.ready])
        mac.connections = [socket]
        let model = TerminalSessionModel(conversationID: "grid-order", folderName: "app", transport: mac)
        var events: [String] = []
        model.attachSurface(feed: { events.append(String(decoding: $0, as: UTF8.self)) }, clear: {},
                            resize: { events.append("grid:\($0)x\($1)") })
        model.gridChanged(cols: 40, rows: 12)
        await wait { model.phase == .live }
        socket.hostGrid(cols: 120, rows: 40)
        socket.push(.success(.output(sequence: 0, bytes: Array("wide".utf8))))
        await wait { events.last == "wide" }
        XCTAssertEqual(Array(events.suffix(2)), ["grid:120x40", "wide"])
        XCTAssertEqual(model.hostGrid, TerminalHostGrid(cols: 120, rows: 40))
        XCTAssertTrue(socket.resizes.isEmpty, "Rendering another viewer's grid must not claim its size")
        model.suspend()
    }

    func testResizeSendIsSerializedCoalescesFinalGridAndPrecedesInput() async {
        let mac = ScriptedMac()
        let socket = ScriptedConnection([Self.ready])
        mac.connections = [socket]
        let model = TerminalSessionModel(conversationID: "resize-order", folderName: "app", transport: mac)
        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .live }
        socket.holdResize = true
        model.gridChanged(cols: 100, rows: 30)
        await wait { socket.resizes.count == 1 }
        for cols in 101...180 { model.gridChanged(cols: cols, rows: 40) }
        // Different-sized typing, escape sequences and a paste all wait behind
        // the resize. Separate turns reproduce the old Task.value waiters.
        let chunks: [[UInt8]] = [Array("a".utf8), Array("bc".utf8), [0x1b, 0x5b, 0x44],
                                Array("paste-δ".utf8), [0x0d], [0x03]]
        for (index, chunk) in chunks.enumerated() {
            if index == 3 { model.paste(chunk) } else { model.send(chunk) }
            for _ in 0..<3 { await Task.yield() }
        }
        XCTAssertEqual(socket.resizes, [TerminalHostGrid(cols: 100, rows: 30)], "One send remains in flight")
        XCTAssertTrue(socket.sent.isEmpty)
        socket.holdInput = true
        socket.holdResize = false; socket.releaseResize()
        await wait { !socket.sent.isEmpty }
        XCTAssertEqual(socket.sent, [chunks[0]], "Only one input send may be in flight")
        // Input arriving after resize completes cannot overtake the waiting keys.
        let later = Array("later".utf8)
        model.send(later)
        model.gridChanged(cols: 200, rows: 50)
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(socket.sent, [chunks[0]], "Later typing waits for the same sender")
        socket.holdInput = false; socket.releaseInput()
        await wait { socket.sent.count == chunks.count + 1 }
        XCTAssertEqual(socket.sent, chunks + [later], "Typing must reach the shell in arrival order")
        XCTAssertEqual(socket.resizes, [TerminalHostGrid(cols: 100, rows: 30), TerminalHostGrid(cols: 180, rows: 40),
                                       TerminalHostGrid(cols: 200, rows: 50)])
        XCTAssertEqual(socket.events, ["resize:100", "resize:180", "input", "resize:200"] +
                       Array(repeating: "input", count: chunks.count))
        model.suspend()
    }

    func testSuspendedResizeCannotSendPendingSizeOrInputToOldShell() async {
        let mac = ScriptedMac()
        let socket = ScriptedConnection([Self.ready])
        mac.connections = [socket]
        let model = TerminalSessionModel(conversationID: "resize-cancel", folderName: "app", transport: mac)
        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .live }
        socket.holdResize = true
        model.gridChanged(cols: 100, rows: 30)
        await wait { socket.resizes.count == 1 }
        model.gridChanged(cols: 120, rows: 40)
        model.send(Array("old".utf8))
        model.suspend()
        socket.hostGrid(cols: 200, rows: 50)
        socket.releaseResize()
        await wait { socket.completedResizes == 1 }
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(socket.resizes, [TerminalHostGrid(cols: 100, rows: 30)])
        XCTAssertTrue(socket.sent.isEmpty)
        XCTAssertEqual(model.hostGrid, TerminalHostGrid(cols: 80, rows: 24), "A late old socket cannot alter the new surface")
    }

    func testSuspendedInputDropsBacklogAndLateFailureCannotCancelReplacementSender() async {
        let mac = ScriptedMac()
        let old = ScriptedConnection([Self.ready]), next = ScriptedConnection([Self.ready])
        mac.connections = [old, next]
        let model = TerminalSessionModel(conversationID: "input-cancel", folderName: "app", transport: mac)
        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .live }
        old.holdInput = true
        old.inputError = URLError(.networkConnectionLost)
        model.send(Array("in-flight".utf8))
        await wait { old.sent.count == 1 }
        model.send(Array("discarded".utf8))
        model.suspend()
        model.resume()
        await wait { mac.attaches.count == 2 && model.phase == .live }
        next.holdResize = true
        model.gridChanged(cols: 120, rows: 40)
        await wait { next.resizes.count == 1 }
        model.send(Array("new".utf8))
        old.releaseInput()
        await wait { old.completedInputs == 1 }
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(old.sent, [Array("in-flight".utf8)])
        XCTAssertTrue(next.sent.isEmpty, "The replacement still waits for its resize")
        next.holdResize = false; next.releaseResize()
        await wait { next.sent == [Array("new".utf8)] }
        XCTAssertEqual(model.phase, .live, "A late old failure must leave the new connection alone")
        model.suspend()
    }

    func testFailedInputDiscardsLiveBacklogInsteadOfReplayingCommandsOnReconnect() async {
        let mac = ScriptedMac()
        let old = ScriptedConnection([Self.ready]), next = ScriptedConnection([Self.ready])
        mac.connections = [old, next]
        let model = TerminalSessionModel(conversationID: "input-failure", folderName: "app", transport: mac)
        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .live }
        old.holdInput = true
        old.inputError = URLError(.networkConnectionLost)
        model.send(Array("uncertain-command\r".utf8))
        await wait { old.sent.count == 1 }
        model.send(Array("old-backlog\r".utf8))
        old.releaseInput()
        await wait { model.phase == .reconnecting }
        model.send(Array("recovery\r".utf8))
        await wait { model.phase == .live && next.sent.count == 1 }
        XCTAssertEqual(old.sent, [Array("uncertain-command\r".utf8)])
        XCTAssertEqual(next.sent, [Array("recovery\r".utf8)], "Uncertain input must never execute twice")
        model.suspend()
    }

    func testLiveInputBacklogRefusesWholeChunksAtByteAndChunkBounds() async {
        for chunk in [[UInt8](repeating: 0x61, count: TerminalSessionModel.liveInputLimit), [0x62]] {
            let mac = ScriptedMac()
            let socket = ScriptedConnection([Self.ready])
            mac.connections = [socket]
            let model = TerminalSessionModel(conversationID: "input-bound-\(chunk.count)", folderName: "app", transport: mac)
            model.gridChanged(cols: 80, rows: 24)
            await wait { model.phase == .live }
            socket.holdResize = true
            model.gridChanged(cols: 100, rows: 30)
            await wait { socket.resizes.count == 1 }
            let accepted = min(TerminalSessionModel.liveInputQueueLimit / chunk.count,
                               TerminalSessionModel.liveInputChunkLimit)
            for _ in 0..<accepted { model.send(chunk) }
            XCTAssertNil(model.notice)
            model.paste(chunk)
            XCTAssertTrue(model.notice?.contains("paste wasn’t sent") == true)
            XCTAssertNil(model.heldPaste, "A rejected live paste must not be replayed later")
            XCTAssertTrue(socket.sent.isEmpty)
            socket.holdResize = false; socket.releaseResize()
            await wait { socket.sent.count == accepted }
            XCTAssertEqual(socket.sent, Array(repeating: chunk, count: accepted))
            model.send(Array("after".utf8))
            await wait { socket.sent.count == accepted + 1 }
            XCTAssertEqual(socket.sent.last, Array("after".utf8), "The queue accepts input again after draining")
            model.suspend()
        }
    }

    /// A send can fail before receive notices cancellation. New keys and a
    /// paste must wait for the replacement, even if the old socket returns a
    /// buffered ready instead of throwing immediately.
    func testFailedSendQueuesTypingBeforeReceiveLoopNoticesDisconnect() async {
        for resizeFails in [false, true] {
            let mac = ScriptedMac()
            let old = ScriptedConnection([Self.ready]), next = ScriptedConnection([Self.ready])
            old.holdReceiveCancellation = true
            mac.connections = [old, next]
            let model = TerminalSessionModel(conversationID: "disconnect-gap-\(resizeFails)", folderName: "app", transport: mac)
            model.gridChanged(cols: 80, rows: 24)
            await wait { model.phase == .live }
            if resizeFails {
                old.resizeError = URLError(.networkConnectionLost)
                model.gridChanged(cols: 100, rows: 30)
            } else {
                old.inputError = URLError(.networkConnectionLost)
                model.send(Array("uncertain\r".utf8))
            }
            await wait { old.cancellations == 1 }
            XCTAssertEqual(model.phase, .reconnecting, "Receive is still suspended; typing must already wait")
            XCTAssertEqual(model.notice, "The terminal disconnected. Recent input may not have reached your computer.")
            model.send(Array("recovery".utf8))
            model.paste(Array("-paste\r".utf8))
            model.gridChanged(cols: 120, rows: 40)
            old.hostGrid(cols: 200, rows: 50)
            for _ in 0..<10 { await Task.yield() }
            XCTAssertEqual(old.sent, resizeFails ? [] : [Array("uncertain\r".utf8)])
            XCTAssertEqual(old.resizes, resizeFails ? [TerminalHostGrid(cols: 100, rows: 30)] : [])
            XCTAssertEqual(model.hostGrid, TerminalHostGrid(cols: 80, rows: 24), "The cancelled socket cannot change the grid")
            XCTAssertEqual(mac.attaches.count, 1, "Receive has not completed yet")

            old.push(Self.ready)
            await wait { model.phase == .live && next.sent.count == 1 }
            XCTAssertEqual(mac.attaches.count, 2, "A late ready must not revive the cancelled connection")
            XCTAssertEqual(next.sent, [Array("recovery-paste\r".utf8)], "Only input typed after failure is sent on reconnect")
            model.suspend()
        }
    }

    private final class ScriptedConnection: TerminalConnection {
        var incoming: [Result<TerminalServerMessage, Error>]
        var received = 0
        var sent: [[UInt8]] = []
        var resizes: [TerminalHostGrid] = []
        var completedResizes = 0
        var events: [String] = []
        var holdResize = false
        var holdInput = false
        var inputError: Error?
        var resizeError: Error?
        var holdReceiveCancellation = false
        private(set) var cancellations = 0
        var completedInputs = 0
        private var heldInputs: [CheckedContinuation<Void, Never>] = []
        func releaseInput() {
            let held = heldInputs; heldInputs = []
            for waiter in held { waiter.resume() }
        }
        private var heldResizes: [CheckedContinuation<Void, Never>] = []
        private var gridReceiver: ((Int, Int) -> Void)?
        func observeGrid(_ receive: @escaping (Int, Int) -> Void) { gridReceiver = receive }
        func hostGrid(cols: Int, rows: Int) { gridReceiver?(cols, rows) }
        func releaseResize() {
            let held = heldResizes; heldResizes = []
            for waiter in held { waiter.resume() }
        }
        private var waiter: CheckedContinuation<TerminalServerMessage, Error>?
        init(_ incoming: [Result<TerminalServerMessage, Error>]) { self.incoming = incoming }
        func receive() async throws -> TerminalServerMessage {
            defer { received += 1 }
            if !incoming.isEmpty { return try incoming.removeFirst().get() }
            return try await withCheckedThrowingContinuation { waiter = $0 }
        }
        func push(_ result: Result<TerminalServerMessage, Error>) {
            if let waiter { self.waiter = nil; waiter.resume(with: result) } else { incoming.append(result) }
        }
        func send(_ bytes: [UInt8]) async throws {
            defer { completedInputs += 1 }
            events.append("input"); sent.append(bytes)
            if holdInput { await withCheckedContinuation { heldInputs.append($0) } }
            if let inputError { throw inputError }
        }
        func resize(cols: Int, rows: Int) async throws {
            defer { completedResizes += 1 }
            events.append("resize:\(cols)")
            resizes.append(TerminalHostGrid(cols: cols, rows: rows))
            if holdResize { await withCheckedContinuation { heldResizes.append($0) } }
            if let resizeError { throw resizeError }
        }
        func cancel() {
            cancellations += 1
            guard !holdReceiveCancellation else { return }
            waiter?.resume(throwing: CancellationError()); waiter = nil
        }
    }

    private final class ScriptedMac: TerminalTransport {
        var running: [TerminalSession] = []
        var opened = 0
        var attaches: [UInt64?] = []
        var connections: [ScriptedConnection] = []
        var refuse: Error?
        /// The Mac refuses to open a shell until the folder is in the Project.
        var outsideProject = false
        var foldersAdded: [String] = []
        /// Set while the route can't carry a terminal (Cupola's relay).
        var unavailableReason: String?
        var listed = 0
        func list(conversationID: String) async throws -> [TerminalSession] { listed += 1; return running }
        /// Holds the folder change until the test releases it, ignoring cancellation.
        var holdFolderAdd = false
        private var heldFolderAdd: CheckedContinuation<Void, Never>?
        func open(conversationID: String, cols: Int, rows: Int) async throws -> TerminalSession {
            if outsideProject { throw PairingFailure.worktreeNotInProject("This worktree is not one of the Project's folders.") }
            opened += 1
            let session = TerminalSession(id: opened == 1 ? "term_new" : "term_new\(opened)", conversationId: conversationID, title: "app", shell: "zsh",
                                          cols: cols, rows: rows, createdAt: "now")
            running.append(session)
            return session
        }
        var closed: [String] = []
        var closeError: Error?
        func close(terminalID: String) async throws {
            if let closeError { throw closeError }
            closed.append(terminalID)
            running.removeAll { $0.id == terminalID }
        }
        func connect(terminalID: String, sequence: UInt64?, cols: Int, rows: Int) async throws -> any TerminalConnection {
            attaches.append(sequence)
            if let refuse { throw refuse }
            return connections.removeFirst()
        }
    }

    private func wait(_ condition: @escaping @MainActor () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<400 where !condition() { try? await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(condition(), file: file, line: line)
    }

    func testReconnectResumesFromTheLastDrawnByteAndFlushesQueuedTyping() async {
        let mac = ScriptedMac()
        mac.running = [TerminalSession(id: "term_running", conversationId: "c", title: "app", shell: "zsh", cols: 80, rows: 24, createdAt: "now")]
        let first = ScriptedConnection([
            .success(.ready(sequence: 0, reset: true, cols: 80, rows: 24, exited: false, exitCode: nil)),
            .success(.output(sequence: 0, bytes: Array("hello".utf8))),
        ])
        let second = ScriptedConnection([
            .success(.ready(sequence: 5, reset: false, cols: 80, rows: 24, exited: false, exitCode: nil)),
            .success(.output(sequence: 3, bytes: Array("lo world".utf8))),
        ])
        mac.connections = [first, second]
        let model = TerminalSessionModel(conversationID: "c", folderName: "app", transport: mac)
        var drawn: [UInt8] = []
        var clears = 0
        model.attachSurface(feed: { drawn += $0 }, clear: { clears += 1 })
        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .live && drawn == Array("hello".utf8) }
        XCTAssertEqual(mac.opened, 0, "the running shell is reattached, not replaced")

        // The network drops; typing while reconnecting waits for the Mac.
        first.push(.failure(URLError(.networkConnectionLost)))
        await wait { model.phase == .reconnecting }
        model.send(Array("ls\r".utf8))
        await wait { model.phase == .live && drawn == Array("hello world".utf8) }
        XCTAssertEqual(mac.attaches, [nil, 5])
        XCTAssertEqual(clears, 1, "only the first attach clears the screen")
        await wait { second.sent == [Array("ls\r".utf8)] }
    }

    private static let ready: Result<TerminalServerMessage, Error> =
        .success(.ready(sequence: 0, reset: true, cols: 80, rows: 24, exited: false, exitCode: nil))

    /// Typing while the shell starts is sent once it's live; what doesn't
    /// fit the bound is refused whole, and a long paste waits here for Paste.
    func testTypingBeforeConnectingIsBoundedAndALongPasteIsHeldWhole() async {
        let mac = ScriptedMac()
        let connection = ScriptedConnection([Self.ready])
        mac.connections = [connection]
        let model = TerminalSessionModel(conversationID: "c", folderName: "app", transport: mac)
        model.attachSurface(feed: { _ in }, clear: {})
        XCTAssertEqual(model.phase, .starting)
        model.send(Array("echo hi\r".utf8))
        XCTAssertNil(model.notice)
        model.send([UInt8](repeating: 0x61, count: TerminalSessionModel.queuedInputLimit))
        XCTAssertNotNil(model.notice, "overflowing typing says it wasn't sent")
        let long = [UInt8](repeating: 0x62, count: 5_000)
        model.paste(long)
        XCTAssertEqual(model.heldPaste, long, "the paste is kept whole, not cut to fit")

        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .live }
        await wait { connection.sent == [Array("echo hi\r".utf8)] }
        XCTAssertEqual(model.heldPaste, long, "a held paste waits for Paste, never sends by itself")
        model.sendHeldPaste()
        XCTAssertNil(model.heldPaste)
        await wait { connection.sent == [Array("echo hi\r".utf8), long] }

        model.paste([UInt8](repeating: 0x63, count: TerminalSessionModel.liveInputLimit + 1))
        XCTAssertNil(model.heldPaste)
        XCTAssertNotNil(model.notice)
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(connection.sent.count, 2, "a paste over the computer's frame limit is refused, not sent")
    }

    /// A pane replaced by a new one (sheet to companion pane, a fold change) can be
    /// dismantled after its replacement attached; that must not blank the new pane.
    func testReplacedPaneDismantlingLateKeepsTheNewPaneAttached() async {
        let mac = ScriptedMac()
        let connection = ScriptedConnection([Self.ready, .success(.output(sequence: 0, bytes: Array("hello".utf8)))])
        mac.connections = [connection]
        let model = TerminalSessionModel(conversationID: "c", folderName: "app", transport: mac)
        let oldPane = NSObject(), newPane = NSObject()
        var oldDrawn: [UInt8] = [], newDrawn: [UInt8] = []
        model.attachSurface(owner: oldPane, feed: { oldDrawn += $0 }, clear: {})
        model.attachSurface(owner: newPane, feed: { newDrawn += $0 }, clear: {})
        model.detachSurface(owner: oldPane)
        model.gridChanged(cols: 80, rows: 24)
        await wait { newDrawn == Array("hello".utf8) }
        XCTAssertTrue(oldDrawn.isEmpty)
    }

    /// Queued typing belongs to one shell: one that turned out to have ended
    /// never receives it, and neither does the next one.
    func testQueuedTypingNeverReachesAnEndedOrDifferentShell() async {
        let mac = ScriptedMac()
        let exited = ScriptedConnection([.success(.ready(sequence: 0, reset: true, cols: 80, rows: 24, exited: true, exitCode: 0))])
        mac.connections = [exited]
        let model = TerminalSessionModel(conversationID: "c", folderName: "app", transport: mac)
        model.attachSurface(feed: { _ in }, clear: {})
        model.send(Array("rm -rf build\r".utf8))
        model.paste([UInt8](repeating: 0x62, count: 5_000))
        model.gridChanged(cols: 80, rows: 24)
        await wait { model.phase == .exited(0) }
        XCTAssertNil(model.heldPaste)

        let fresh = ScriptedConnection([Self.ready])
        mac.connections = [fresh]
        mac.running = []
        model.startNew()
        await wait { model.phase == .live }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(exited.sent, [])
        XCTAssertEqual(fresh.sent, [], "the new shell receives nothing typed for the old one")
    }

    /// A program's clipboard copy reaches this device only while sharing is
    /// on, bounded as the computer view's copies are, and always says so.
    func testTerminalClipboardCopiesOnlyWhileSharingIsOnAndWithinTheLimit() throws {
        let name = UIPasteboard.Name("terminal-clipboard-" + UUID().uuidString)
        let board = try XCTUnwrap(UIPasteboard(name: name, create: true))
        defer { UIPasteboard.remove(withName: name) }
        let model = TerminalSessionModel(conversationID: "c", folderName: "app", transport: ScriptedMac())

        model.copyFromComputer(Data("secret".utf8), sharingAllowed: false, to: board)
        XCTAssertNil(board.string, "a disabled sharing choice is honoured, not just announced")
        XCTAssertEqual(model.notice?.contains("Share clipboard"), true)

        model.copyFromComputer(Data("make test".utf8), sharingAllowed: true, to: board)
        XCTAssertEqual(board.string, "make test")
        XCTAssertEqual(model.notice, "Copied from your computer")

        model.copyFromComputer(Data(repeating: 0x61, count: ComputerPhoneClipboard.limit + 1), sharingAllowed: true, to: board)
        XCTAssertEqual(board.string, "make test", "an oversized copy writes nothing")
        XCTAssertEqual(model.notice?.contains("256 KB"), true)
    }

    private static func shell(_ id: String, _ conversation: String, exited: Bool = false) -> TerminalSession {
        TerminalSession(id: id, conversationId: conversation, title: "app", shell: "zsh", cols: 80, rows: 24,
                        createdAt: "now", exited: exited)
    }

    private static func replay(_ text: String) -> ScriptedConnection {
        ScriptedConnection([.success(.ready(sequence: 0, reset: true, cols: 80, rows: 24, exited: false, exitCode: nil)),
                            .success(.output(sequence: 0, bytes: Array(text.utf8)))])
    }

}

/// Contract: each pill toggles one selected tool in either layout. A fold or
/// unfold moves that selection; an outgoing sheet cannot dismiss its successor.
/// The shell remembers this selection. Terminal's existing presentation tests
/// cover the actual keyboard/pane views, and composer-pills covers the controls.
@MainActor final class ToolPresentationTests: XCTestCase {
    func testSavedLayoutNamingPullRequestsStillLoadsWithoutIt() throws {
        let suite = "pill-migration-" + UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set("files,-git,terminal", forKey: ComposerPillLayout.storageKey)
        let preferences = ComposerPillPreferences(defaults: defaults)
        XCTAssertFalse(preferences.layout.order.contains(.pullRequests))
        XCTAssertFalse(preferences.layout.visible.contains(.pullRequests))
        XCTAssertEqual(preferences.layout.order.filter { [.files, .terminal].contains($0) }, [.files, .terminal])
        XCTAssertFalse(try XCTUnwrap(defaults.string(forKey: ComposerPillLayout.storageKey)).contains("git"))
    }

    func testLegacyChangesPaneRestoresAsPullRequests() throws {
        let saved = try JSONDecoder().decode(ToolPresentation.Saved.self, from: Data(#"{"kind":"changes","expanded":true}"#.utf8))
        var state = ToolPresentation()
        state.restore(saved, layout: .regular)
        XCTAssertEqual(state.pane, .pullRequests)
        XCTAssertTrue(state.expanded)
        let encoded = try JSONEncoder().encode(try XCTUnwrap(state.saved))
        XCTAssertEqual(try JSONDecoder().decode(ToolPresentation.Saved.self, from: encoded).kind, .pullRequests)
        XCTAssertTrue(String(decoding: encoded, as: UTF8.self).contains("pull-requests"))
    }

}
