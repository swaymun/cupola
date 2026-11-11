import SwiftTerm
import SwiftUI
import UIKit
import WonderPairing

// MARK: - Transport

/// How a terminal reaches the Mac. The live transport uses the paired
/// connection; Diagnostics previews use a local fixture.
@MainActor protocol TerminalTransport: AnyObject {
    func list(conversationID: String) async throws -> [TerminalSession]
    func open(conversationID: String, cols: Int, rows: Int) async throws -> TerminalSession
    func close(terminalID: String) async throws
    func connect(terminalID: String, sequence: UInt64?, cols: Int, rows: Int) async throws -> any TerminalConnection
    /// Adds the thread's working folder to its Project (a worktree it leaves out).
    func addWorkingFolderToProject(conversationID: String) async throws
    /// Why a terminal can't open on the current route (Cupola's relay can't
    /// carry its stream), or nil. Read again while waiting, so a change of
    /// route opens it.
    var unavailableReason: String? { get }
}

extension TerminalTransport {
    func addWorkingFolderToProject(conversationID: String) async throws { throw PairingFailure.response(501) }
    var unavailableReason: String? { nil }
}

@MainActor protocol TerminalConnection: AnyObject {
    func receive() async throws -> TerminalServerMessage
    func send(_ bytes: [UInt8]) async throws
    func resize(cols: Int, rows: Int) async throws
    func cancel()
    /// Called synchronously before output for the new host grid is returned.
    func observeGrid(_ receive: @escaping (Int, Int) -> Void)
}


enum TerminalTransportError: Error {
    case gone
    /// The computer refused this device's sign-in or ended its access
    /// (revoked, or the session expired); reconnecting can't help.
    case accessEnded
}

@MainActor final class LiveTerminalTransport: TerminalTransport {
    private weak var model: ConnectionModel?
    init(model: ConnectionModel) { self.model = model }

    private func connected() throws -> ConnectionModel {
        guard let model else { throw CancellationError() }
        return model
    }

    func list(conversationID: String) async throws -> [TerminalSession] {
        let list: TerminalList = try await connected().manage("/api/v1/conversations/\(ConnectionModel.escape(conversationID))/terminals")
        return list.terminals
    }

    func open(conversationID: String, cols: Int, rows: Int) async throws -> TerminalSession {
        try await connected().manage("/api/v1/conversations/\(ConnectionModel.escape(conversationID))/terminals",
                                     method: "POST", body: JSONEncoder().encode(OpenTerminalRequest(cols: cols, rows: rows)))
    }

    func addWorkingFolderToProject(conversationID: String) async throws {
        try await connected().addWorkingFolderToProject(conversationID)
    }

    var unavailableReason: String? { model?.usingRelay == true ? RelayUnsupported.terminal.errorDescription : nil }

    func close(terminalID: String) async throws {
        struct Closed: Decodable, Sendable {}
        let _: Closed = try await connected().manage("/api/v1/terminals/\(ConnectionModel.escape(terminalID))", method: "DELETE")
    }

    func connect(terminalID: String, sequence: UInt64?, cols: Int, rows: Int) async throws -> any TerminalConnection {
        let model = try connected()
        let proof: (saved: SavedConnection, challengeID: String, signature: String)
        do { proof = try await model.signedTerminalChallenge() }
        catch PairingFailure.response(let status) where status == 401 || status == 403 { throw TerminalTransportError.accessEnded }
        let task = try model.api.terminalSocket(connection: proof.saved, terminalID: terminalID)
        task.resume()
        let attach = TerminalAttach(deviceId: proof.saved.credential.deviceId, csrfToken: proof.saved.credential.csrfToken,
                                    challengeId: proof.challengeID, signature: proof.signature,
                                    sequence: sequence, cols: cols, rows: rows)
        do {
            try await task.send(.string(String(decoding: try JSONEncoder().encode(SharedGridAttach(attach: attach)), as: UTF8.self)))
        } catch {
            task.cancel(with: .goingAway, reason: nil)
            throw LiveTerminalConnection.refusal(of: task) ?? error
        }
        return LiveTerminalConnection(task: task)
    }
}

/// Opt in without changing the shared package's legacy terminal protocol.
struct SharedGridAttach: Encodable {
    let attach: TerminalAttach
    private enum CodingKeys: String, CodingKey { case grid }
    func encode(to encoder: Encoder) throws {
        try attach.encode(to: encoder)
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(true, forKey: .grid)
    }
}

/// Only this app opts in to grid frames. Legacy messages still use the shared decoder.
struct TerminalHostGrid: Equatable, Decodable {
    let cols: Int
    let rows: Int

    static func read(_ text: String) throws -> Self? {
        struct Kind: Decodable { let type: String }
        let data = Data(text.utf8)
        guard try JSONDecoder().decode(Kind.self, from: data).type == "grid" else { return nil }
        let grid = try JSONDecoder().decode(Self.self, from: data)
        guard TerminalGrid.columns.contains(grid.cols), TerminalGrid.rows.contains(grid.rows) else {
            throw TerminalServerMessage.Malformed()
        }
        return grid
    }
}

@MainActor final class LiveTerminalConnection: TerminalConnection {
    private let task: URLSessionWebSocketTask
    private var gridReceiver: ((Int, Int) -> Void)?
    init(task: URLSessionWebSocketTask) { self.task = task }
    func observeGrid(_ receive: @escaping (Int, Int) -> Void) { gridReceiver = receive }

    func receive() async throws -> TerminalServerMessage {
        do {
            while true {
                switch try await task.receive() {
                case .data(let data): return try .binary(data)
                case .string(let text):
                    if let grid = try TerminalHostGrid.read(text) {
                        gridReceiver?(grid.cols, grid.rows)
                    } else {
                        return try .text(text)
                    }
                @unknown default: throw TerminalServerMessage.Malformed()
                }
            }
        } catch {
            throw Self.refusal(of: task) ?? error
        }
    }

    /// The host closes with `access-ended` (code 4401) when it refuses this
    /// device's sign-in or its access ends (`crates/wonderd/src/terminal.rs`).
    static let accessEndedCloseCode = 4401
    static func refusal(of task: URLSessionWebSocketTask) -> TerminalTransportError? {
        if (task.response as? HTTPURLResponse)?.statusCode == 404 { return .gone }
        if task.closeCode.rawValue == accessEndedCloseCode || task.closeReason == Data("access-ended".utf8) { return .accessEnded }
        return nil
    }

    func send(_ bytes: [UInt8]) async throws { try await task.send(.data(Data(bytes))) }

    func resize(cols: Int, rows: Int) async throws {
        try await task.send(.string(#"{"type":"resize","cols":\#(cols),"rows":\#(rows)}"#))
    }

    func cancel() { task.cancel(with: .goingAway, reason: nil) }
}

// MARK: - Session model

/// One terminal sheet's connection to a shell on the Mac. It reattaches to the
/// shell last chosen in this chat (else the newest running one) when there is
/// one, resumes from the last byte it drew after a reconnect, and reconnects
/// after network loss or a return from the background. It waits, saying why,
/// while the route can't carry a terminal, and stops when the computer refuses
/// this device. A chat can keep several shells; the sheet shows one at a time,
/// and switching redraws the chosen one from the computer's copy of its output.
@MainActor final class TerminalSessionModel: ObservableObject {
    enum Phase: Equatable {
        case starting
        case connecting
        case live
        case reconnecting
        case exited(Int?)
        case closed
        case failed(String)
        /// The thread works in a worktree its Project leaves out: adding the
        /// folder opens it, trying again would not.
        case outsideProject(String)
        /// The current route (Cupola's relay) can't carry a terminal; it
        /// opens by itself once the route changes.
        case unavailable(String)
        /// The computer refused this device's sign-in or ended its access.
        case accessEnded(String)
    }

    /// What End Terminal achieved. Only a computer that answered ends it.
    enum EndOutcome: Equatable {
        case ended
        /// The computer had already closed it.
        case alreadyGone
        /// The computer wasn't reached; the shell may still be running.
        case failed
    }

    @Published private(set) var phase: Phase = .starting
    @Published private(set) var addingFolder = false
    @Published private(set) var addFolderFailure: String?
    @Published private(set) var session: TerminalSession?
    @Published private(set) var ending = false
    @Published private(set) var endFailure: String?
    /// A short note about input or the clipboard; the sheet clears it.
    @Published private(set) var notice: String?
    /// A paste too long to hold while connecting, kept here (for this shell
    /// only) so Paste can send it once the terminal is live.
    @Published private(set) var heldPaste: [UInt8]?
    /// This chat's shells on the computer, oldest first, as last listed.
    @Published private(set) var shells: [TerminalSession] = []

    let conversationID: String
    let folderName: String
    private let transport: any TerminalTransport
    private var cursor = TerminalStreamCursor()
    private var connection: (any TerminalConnection)?
    private var runner: Task<Void, Never>?
    /// Which run is current; a cancelled one finishing late leaves the next alone.
    private var runID = 0
    private var addFolder: Task<Void, Never>?
    private var lifetime = UUID()
    /// Available space belongs to this viewer; the rendered grid belongs to the host.
    private var grid: (cols: Int, rows: Int)?
    @Published private(set) var hostGrid: TerminalHostGrid?
    private var pendingResize: (cols: Int, rows: Int)?
    private var outboundTask: Task<Void, Never>?
    /// Live input belongs to this connection, in arrival order; never replay it.
    private var liveInput: [[UInt8]] = []
    private var liveInputBytes = 0
    private var suspended = false
    /// Output that arrived before the view was ready, bounded.
    private enum SurfaceEvent {
        case grid(TerminalHostGrid)
        case bytes([UInt8])
    }
    private var pending: [SurfaceEvent] = []
    private var pendingBytes = 0
    /// Typing while the shell starts or reconnects, sent once it's live; bounded.
    private var queuedInput: [UInt8] = []
    private var surface: (feed: ([UInt8]) -> Void, clear: () -> Void, resize: (Int, Int) -> Void)?
    /// How often a route that can't carry a terminal is checked again.
    var routeRecheck: Duration = .seconds(2)
    private static let pendingLimit = 2 * 1024 * 1024
    /// The computer keeps at most this many shells for one device
    /// (`TerminalLimits.per_device` in `crates/wonderd/src/terminal.rs`).
    static let shellLimit = 4
    /// The shell last shown in each chat during this launch, so reopening
    /// Terminal returns to it rather than to the newest one.
    private static var chosen: [String: String] = [:]
    /// The next run opens another shell instead of reattaching.
    private var opensAnother = false
    static let queuedInputLimit = 4 * 1024
    /// The host's largest input frame (`max_input_frame`); a bigger one ends the socket.
    static let liveInputLimit = 64 * 1024
    /// Bound bytes and frame count held behind a slow resize or input send.
    static let liveInputQueueLimit = 256 * 1024
    static let liveInputChunkLimit = 1024

    init(conversationID: String, folderName: String, transport: any TerminalTransport) {
        self.conversationID = conversationID
        self.folderName = folderName
        self.transport = transport
    }

    var title: String { session?.title ?? folderName }
    var subtitle: String? {
        session.map { session in
            guard shells.count > 1, let name = name(of: session.id) else { return session.shell }
            return "\(name) · \(session.shell)"
        }
    }

    /// "Terminal 2": the shell's place among this chat's shells, oldest first.
    func name(of id: String) -> String? {
        shells.firstIndex { $0.id == id }.map { "Terminal \($0 + 1)" }
    }

    /// Whether a listed shell is still running; the shown one follows what
    /// its connection last said.
    func isRunning(_ shell: TerminalSession) -> Bool {
        guard shell.id == session?.id else { return !shell.exited }
        switch phase {
        case .exited, .closed: return false
        default: return !shell.exited
        }
    }

    /// Another shell can open only below the computer's limit for this device.
    var canOpenAnother: Bool { shells.count < Self.shellLimit && !ending }

    /// The pane that owns `surface`; a pane replaced by a new one (the sheet
    /// becoming the companion pane, a fold change) may be dismantled after its
    /// replacement attached, and must not detach it.
    private var surfaceOwner: ObjectIdentifier?

    func attachSurface(owner: AnyObject? = nil, feed: @escaping ([UInt8]) -> Void, clear: @escaping () -> Void,
                       resize: @escaping (Int, Int) -> Void = { _, _ in }) {
        surfaceOwner = owner.map(ObjectIdentifier.init)
        surface = (feed, clear, resize)
        if pending.isEmpty, let hostGrid { resize(hostGrid.cols, hostGrid.rows) }
        for event in pending {
            switch event {
            case .grid(let grid): resize(grid.cols, grid.rows)
            case .bytes(let bytes): feed(bytes)
            }
        }
        pending = []; pendingBytes = 0
    }

    private func applyGrid(cols: Int, rows: Int) {
        guard TerminalGrid.columns.contains(cols), TerminalGrid.rows.contains(rows) else { return }
        let next = TerminalHostGrid(cols: cols, rows: rows)
        guard hostGrid != next else { return }
        hostGrid = next
        if let surface { surface.resize(cols, rows) }
        else {
            // Adjacent updates with no output between them need only their final size.
            if case .grid = pending.last { pending.removeLast() }
            pending.append(.grid(next))
        }
    }

    func detachSurface(owner: AnyObject? = nil) {
        if let owner, surfaceOwner != ObjectIdentifier(owner) { return }
        surfaceOwner = nil
        surface = nil
        if pending.isEmpty, let hostGrid { pending = [.grid(hostGrid)] }
    }

    private func draw(_ action: TerminalFeed) {
        switch action {
        case .none: break
        case .clear:
            pending = []; pendingBytes = 0
            if surface == nil, let hostGrid { pending = [.grid(hostGrid)] }
            surface?.clear()
        case .feed(let bytes):
            if let surface { surface.feed(bytes); return }
            pending.append(.bytes(bytes)); pendingBytes += bytes.count
            // Retain the grid preceding the kept tail as well as its later changes.
            while pendingBytes > Self.pendingLimit, !pending.isEmpty {
                if case .grid = pending[0], pending.count > 1 {
                    if case .grid = pending[1] { pending.removeFirst(); continue }
                    if case .bytes(let first) = pending[1] {
                        let drop = min(first.count, pendingBytes - Self.pendingLimit)
                        pendingBytes -= drop
                        if drop == first.count { pending.remove(at: 1) }
                        else { pending[1] = .bytes(Array(first.dropFirst(drop))) }
                    }
                } else if case .bytes(let first) = pending[0] {
                    let drop = min(first.count, pendingBytes - Self.pendingLimit)
                    pendingBytes -= drop
                    if drop == first.count { pending.removeFirst() }
                    else { pending[0] = .bytes(Array(first.dropFirst(drop))) }
                }
            }
        }
    }

    /// The view measured its grid; the first size starts the session.
    func gridChanged(cols: Int, rows: Int) {
        guard TerminalGrid.columns.contains(cols), TerminalGrid.rows.contains(rows) else { return }
        let changed = grid.map { $0 != (cols, rows) } ?? true
        grid = (cols, rows)
        if runner == nil, session == nil, phase == .starting { startIfIdle(); return }
        guard changed, let connection else { return }
        switch phase {
        case .live, .connecting, .reconnecting: break
        default: return
        }
        queueResize(on: connection)
    }

    /// A layout burst retains only its final grid. The same sender drains
    /// resizes and input, so taking ownership cannot overtake an older key.
    private func queueResize(on connection: any TerminalConnection) {
        pendingResize = grid
        drainOutbound(on: connection)
    }

    private func drainOutbound(on connection: any TerminalConnection) {
        guard outboundTask == nil else { return }
        outboundTask = Task { [weak self] in
            await Task.yield()
            guard let self else { return }
            // Cancellation can finish after a replacement connection has started.
            defer { if !Task.isCancelled, self.connection === connection { self.outboundTask = nil } }
            while !Task.isCancelled, self.connection === connection {
                do {
                    if let size = self.pendingResize {
                        self.pendingResize = nil
                        try await connection.resize(cols: size.cols, rows: size.rows)
                    } else if !self.liveInput.isEmpty {
                        let bytes = self.liveInput.removeFirst()
                        self.liveInputBytes -= bytes.count
                        try await connection.send(bytes)
                    } else {
                        return
                    }
                } catch {
                    guard !Task.isCancelled, self.connection === connection else { return }
                    self.connectionLost(connection)
                    return
                }
            }
        }
    }

    private func cancelOutbound() {
        outboundTask?.cancel(); outboundTask = nil
        pendingResize = nil
        liveInput = []; liveInputBytes = 0
    }

    /// Starts once; the view's first measured size normally does this.
    func startIfIdle() {
        guard runner == nil, session == nil, phase == .starting else { return }
        start()
    }

    func start() {
        guard runner == nil else { return }
        suspended = false
        runID += 1
        let id = runID
        runner = Task { [weak self] in await self?.run(id) }
    }

    /// Opens a fresh shell after the previous one ended.
    func startNew() {
        runner?.cancel(); runner = nil
        cancelOutbound()
        connection?.cancel(); connection = nil
        session = nil
        hostGrid = nil
        cursor = TerminalStreamCursor()
        dropWaitingInput()
        endFailure = nil
        draw(.clear)
        phase = .starting
        start()
    }

    /// Opens one more shell in this chat beside the others, which keep
    /// running on the computer. Past its limit the computer refuses and the
    /// sheet shows its reason.
    func openAnother() {
        guard !ending else { return }
        opensAnother = true
        startNew()
    }

    /// Shows another of this chat's shells. The one left keeps running; the
    /// chosen one is redrawn from the computer's copy of its output, and typing
    /// waiting for the old one is never sent to it.
    func select(_ id: String) {
        guard id != session?.id, !ending, let chosen = shells.first(where: { $0.id == id }) else { return }
        cancelAddFolder()
        runner?.cancel(); runner = nil
        cancelOutbound()
        connection?.cancel(); connection = nil
        opensAnother = false
        session = chosen
        hostGrid = nil
        Self.chosen[conversationID] = id
        cursor = TerminalStreamCursor()
        dropWaitingInput()
        endFailure = nil
        draw(.clear)
        phase = .connecting
        start()
    }

    /// Lists this chat's shells again; the list stays as it was when the
    /// computer can't be reached.
    func refreshShells() async {
        guard transport.unavailableReason == nil,
              let listed = try? await transport.list(conversationID: conversationID) else { return }
        shells = listed
    }

    /// After the shown shell ended, shows the newest one still running (else
    /// the newest). False when the chat has no other shell.
    func showRemainingShell() async -> Bool {
        let ended = session?.id
        await refreshShells()
        let others = shells.filter { $0.id != ended }
        guard let next = others.last(where: { !$0.exited }) ?? others.last else { return false }
        select(next.id)
        return true
    }

    /// Tries the same shell again after the computer refused this device
    /// (it may have been signed in again since).
    func retry() {
        guard case .accessEnded = phase else { startNew(); return }
        runner?.cancel(); runner = nil
        phase = session == nil ? .starting : .reconnecting
        start()
    }

    /// Adds the thread's folder to its Project, then opens the shell there.
    /// Leaving or backgrounding the sheet cancels it and changes `lifetime`,
    /// so a late reply never reopens a shell nobody is looking at.
    func addFolderAndStart() {
        guard case .outsideProject = phase, !addingFolder else { return }
        addingFolder = true; addFolderFailure = nil
        let owner = lifetime
        addFolder = Task { [weak self] in
            guard let self else { return }
            do {
                try await transport.addWorkingFolderToProject(conversationID: conversationID)
                guard !Task.isCancelled, lifetime == owner, !suspended else { return }
                addingFolder = false; addFolder = nil
                startNew()
            } catch {
                guard !Task.isCancelled, lifetime == owner else { return }
                addingFolder = false; addFolder = nil
                if !(error is CancellationError) { addFolderFailure = ConnectionModel.addFolderFailure(error) }
            }
        }
    }

    private func cancelAddFolder() {
        lifetime = UUID()
        addFolder?.cancel(); addFolder = nil
        addingFolder = false
    }

    func suspend() {
        suspended = true
        cancelAddFolder()
        runner?.cancel(); runner = nil
        cancelOutbound()
        connection?.cancel(); connection = nil
        if phase == .live || phase == .connecting { phase = .reconnecting }
    }

    func resume() {
        guard suspended else { return }
        suspended = false
        switch phase {
        case .exited, .closed, .failed, .outsideProject, .accessEnded: return
        default: start()
        }
    }

    private enum Delivery { case sent, queued, tooLarge, backedUp, notConnected }

    /// Live typing drains in order after resizes. While it starts or reconnects,
    /// typing waits (bounded) for this same shell; what doesn't fit is refused
    /// whole, never cut, and a live send that failed is never replayed.
    private func deliver(_ bytes: [UInt8]) -> Delivery {
        guard !bytes.isEmpty else { return .sent }
        if phase == .live, let connection {
            guard bytes.count <= Self.liveInputLimit else { return .tooLarge }
            guard liveInputBytes + bytes.count <= Self.liveInputQueueLimit,
                  liveInput.count < Self.liveInputChunkLimit else { return .backedUp }
            liveInput.append(bytes); liveInputBytes += bytes.count
            drainOutbound(on: connection)
            return .sent
        }
        switch phase {
        case .starting, .connecting, .reconnecting:
            guard queuedInput.count + bytes.count <= Self.queuedInputLimit else { return .tooLarge }
            queuedInput += bytes
            return .queued
        default:
            return .notConnected
        }
    }

    func send(_ bytes: [UInt8]) {
        switch deliver(bytes) {
        case .sent, .queued: break
        case .backedUp: notice = "The terminal is catching up. The last keys weren’t sent."
        case .tooLarge where phase == .live: notice = "This input is too long to send at once (over 64 KB). Nothing was sent."
        case .tooLarge: notice = "Typing waits until the terminal connects, up to 4 KB. The last keys weren’t sent."
        case .notConnected: if phase.isWaitingOnTheComputer { notice = "The terminal isn’t connected, so that wasn’t sent." }
        }
    }

    /// A paste is sent or held whole. One too long to wait while connecting
    /// stays here, and Paste sends it once the terminal is live.
    func paste(_ bytes: [UInt8]) {
        heldPaste = nil
        switch deliver(bytes) {
        case .sent, .queued: break
        case .backedUp:
            notice = "The terminal is catching up. The paste wasn’t sent. Try again shortly."
        case .tooLarge where phase == .live:
            notice = "This paste is too long to send at once (over 64 KB). Nothing was sent."
        case .tooLarge:
            guard bytes.count <= Self.liveInputLimit else {
                notice = "This paste is too long to send at once (over 64 KB). Nothing was sent."
                return
            }
            heldPaste = bytes
            notice = "This paste is too long to hold while the terminal connects. Nothing was sent yet."
        case .notConnected:
            if phase.isWaitingOnTheComputer { notice = "The terminal isn’t connected, so the paste wasn’t sent." }
        }
    }

    /// Sends the held paste once the terminal is live.
    func sendHeldPaste() {
        guard let held = heldPaste, phase == .live else { return }
        heldPaste = nil; notice = nil
        paste(held)
    }

    func clearNotice(_ shown: String) { if notice == shown { notice = nil } }

    /// Typing and a held paste belong to one shell; a new shell, an ended one
    /// or a refused device never receives them.
    private func dropWaitingInput() {
        queuedInput = []
        heldPaste = nil
    }

    /// A program in the shell asked to copy text to this device (OSC 52).
    /// It's written only while clipboard sharing is on, bounded, local to
    /// this device and expiring, as the computer view's copies are.
    func copyFromComputer(_ content: Data, sharingAllowed: Bool, to board: UIPasteboard = .general) {
        guard !content.isEmpty else { return }
        let device = UIDevice.current.model
        guard sharingAllowed else {
            notice = "A program in the terminal tried to copy text to this \(device). Turn on Share clipboard in Terminal options to allow it."
            return
        }
        guard content.count <= ComputerPhoneClipboard.limit, let text = String(data: content, encoding: .utf8),
              ComputerPhoneClipboard.write(text, to: board) else {
            notice = "A program in the terminal tried to copy more than 256 KB, so nothing was copied."
            return
        }
        notice = "Copied from your computer"
    }

    /// Ends the shell on the computer. The terminal stops here only once the
    /// computer confirmed it (or had already closed it); otherwise it keeps
    /// running, and `endFailure` says so.
    func end() async -> EndOutcome {
        guard !ending else { return .failed }
        guard let id = session?.id else { stopHere(); return .ended }
        ending = true; endFailure = nil
        let outcome: EndOutcome
        do {
            try await transport.close(terminalID: id)
            outcome = .ended
        } catch PairingFailure.hostMessage(let status, _) where status == 404 {
            outcome = .alreadyGone
        } catch PairingFailure.response(404) {
            outcome = .alreadyGone
        } catch {
            outcome = .failed
        }
        ending = false
        guard outcome != .failed else {
            endFailure = "Cupola couldn’t reach your computer, so this terminal is still running there. Check the connection and try again."
            return .failed
        }
        stopHere()
        shells.removeAll { $0.id == id }
        if Self.chosen[conversationID] == id { Self.chosen[conversationID] = nil }
        return outcome
    }

    private func stopHere() {
        cancelAddFolder()
        runner?.cancel(); runner = nil
        cancelOutbound()
        connection?.cancel(); connection = nil
        dropWaitingInput()
        phase = .closed
    }

    private func connectionLost(_ lost: any TerminalConnection) {
        guard connection === lost else { return }
        // Receive cancellation can arrive later. Stop accepting input and grid
        // updates from this socket now; subsequent typing waits for reconnect.
        connection = nil
        phase = .reconnecting
        notice = "The terminal disconnected. Recent input may not have reached your computer."
        cancelOutbound()
        lost.cancel()
    }

    /// Waits while the route can't carry a terminal, saying why; false when
    /// the wait was cancelled. Typing made before it is not kept for later.
    private func waitForRoute() async -> Bool {
        while let reason = transport.unavailableReason {
            if phase != .unavailable(reason) {
                if !queuedInput.isEmpty { notice = "The terminal isn’t connected, so typing made while connecting wasn’t sent." }
                dropWaitingInput()
                phase = .unavailable(reason)
            }
            try? await Task.sleep(for: routeRecheck)
            if Task.isCancelled { return false }
        }
        return true
    }

    private func run(_ id: Int) async {
        defer { if runID == id { runner = nil } }
        let size = grid ?? (80, 24)
        // Say so before a shell is created that this route can't reach.
        guard await waitForRoute() else { return }
        if session == nil {
            phase = .starting
            do {
                // Reattach to the shell chosen last in this chat, else its
                // newest running one, or start one (always, for New Terminal).
                let listed = try await transport.list(conversationID: conversationID)
                // Switched or left meanwhile: the next run decides.
                if Task.isCancelled { return }
                shells = listed
                let chosen = Self.chosen[conversationID]
                if !opensAnother, let running = listed.first(where: { $0.id == chosen && !$0.exited })
                    ?? listed.last(where: { !$0.exited }) {
                    session = running
                } else {
                    let opened = try await transport.open(conversationID: conversationID, cols: size.cols, rows: size.rows)
                    // A shell opened for a sheet that moved on stays listed in the menu.
                    if Task.isCancelled { return }
                    session = opened
                    if !shells.contains(where: { $0.id == opened.id }) { shells.append(opened) }
                }
                opensAnother = false
                Self.chosen[conversationID] = session?.id
            } catch is CancellationError {
                return
            } catch PairingFailure.worktreeNotInProject(let message) {
                if Task.isCancelled { return }
                phase = .outsideProject(message)
                return
            } catch {
                if Task.isCancelled { return }
                phase = .failed(Self.describe(error))
                return
            }
        }
        var attempt = 0
        while !Task.isCancelled, let id = session?.id {
            phase = attempt == 0 && cursor.held == nil ? .connecting : .reconnecting
            let current = grid ?? size
            do {
                let connection = try await transport.connect(terminalID: id, sequence: cursor.held, cols: current.cols, rows: current.rows)
                if Task.isCancelled { connection.cancel(); return }
                self.connection = connection
                connection.observeGrid { [weak self, weak connection] cols, rows in
                    guard let self, let connection, self.connection === connection else { return }
                    self.applyGrid(cols: cols, rows: rows)
                }
                // Layout can change while attach is awaiting the network.
                if let grid, grid != current { queueResize(on: connection) }
                while !Task.isCancelled {
                    let message = try await connection.receive()
                    // A switch to another shell cancelled this run: draw nothing more.
                    if Task.isCancelled { return }
                    // A failed send retired this socket before receive noticed.
                    // Even a buffered ready must not make it live again.
                    guard self.connection === connection else { throw URLError(.networkConnectionLost) }
                    attempt = 0
                    if apply(message) { cancelOutbound(); connection.cancel(); self.connection = nil; return }
                }
                connection.cancel()
                return
            } catch TerminalTransportError.gone {
                if Task.isCancelled { return }
                cancelOutbound()
                connection?.cancel(); connection = nil
                dropWaitingInput()
                phase = .closed
                return
            } catch TerminalTransportError.accessEnded {
                if Task.isCancelled { return }
                cancelOutbound()
                connection?.cancel(); connection = nil
                dropWaitingInput()
                phase = .accessEnded(Self.accessEndedMessage)
                return
            } catch {
                // Cancelled by a switch or a new shell, whose connection this must not clear.
                if Task.isCancelled { return }
                cancelOutbound()
                connection?.cancel(); connection = nil
                // A route change to the relay waits for a direct route again.
                if transport.unavailableReason != nil {
                    guard await waitForRoute() else { return }
                    attempt = 0
                    continue
                }
                attempt += 1
                phase = .reconnecting
                let delay = min(10, 1 << min(attempt - 1, 4))
                try? await Task.sleep(for: .seconds(delay))
            }
        }
    }

    /// Applies one host message; true when the terminal is gone for good.
    private func apply(_ message: TerminalServerMessage) -> Bool {
        // Resize before clearing/replaying bytes, in the same receive turn.
        if case let .ready(_, _, cols, rows, _, _) = message { applyGrid(cols: cols, rows: rows) }
        draw(cursor.accept(message))
        switch message {
        case let .ready(_, _, cols, rows, exited, exitCode):
            if let session {
                self.session = TerminalSession(id: session.id, conversationId: session.conversationId, title: session.title,
                                               shell: session.shell, cols: cols, rows: rows, createdAt: session.createdAt)
            }
            phase = exited ? .exited(exitCode) : .live
            if exited { cancelOutbound(); dropWaitingInput() }
            if !queuedInput.isEmpty { let input = queuedInput; queuedInput = []; send(input) }
        case .exit(let code):
            cancelOutbound()
            phase = .exited(code)
            dropWaitingInput()
        case .closed:
            phase = .closed
            dropWaitingInput()
            return true
        case .output, .reset:
            break
        }
        return false
    }

    static var accessEndedMessage: String {
        let device = UIDevice.current.model
        return "Your computer didn’t accept this \(device)’s sign-in, so Terminal stopped reconnecting. If you removed this \(device) on your computer, pair it again in Settings; otherwise try again."
    }

    static func describe(_ error: Error) -> String {
        if case let PairingFailure.hostMessage(_, message) = error { return message }
        return "Cupola couldn’t reach your computer. Check the connection."
    }
}

extension TerminalSessionModel.Phase {
    /// Input can't reach the shell until the computer or the route changes.
    var isWaitingOnTheComputer: Bool {
        switch self {
        case .unavailable, .accessEnded: true
        default: false
        }
    }
}

// MARK: - Sheet

/// The composer's Terminal pill. Closing the sheet, by Done or a swipe down,
/// leaves the shell running; the pill opens it again where it was.
struct TerminalPill: View {
    let isPresented: Bool
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            Label("Terminal", systemImage: "apple.terminal")
                .labelStyle(.iconOnly)
                .modifier(ComposerStatusPill(isActive: isPresented))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("conversation-terminal-pill")
        .accessibilityLabel("Terminal")
        .accessibilityValue(isPresented ? "Open" : "Closed")
        .accessibilityAddTraits(isPresented ? .isSelected : [])
        .accessibilityHint("Opens a shell on your computer in this project’s folder")
    }
}

struct TerminalSheet: View {
    @StateObject private var terminal: TerminalSessionModel
    private let dismiss = ToolDismiss()
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    /// The owner's text size at the default Dynamic Type size; Dynamic Type
    /// scales it, and pinching or the menu changes it.
    @AppStorage("terminalFontSize") private var baseFontSize: Double = TerminalFontSize.standard
    /// Whether programs here may copy to this device's clipboard; the same
    /// choice as the computer view's clipboard sharing, off by default.
    @AppStorage(ComputerPhoneClipboard.sharingKey) private var sharesClipboard = false
    @State private var confirmingEnd = false
    @Environment(\.companionPane) private var pane
    @State private var verticalBar = false
    /// In the pane on the Duo the toolbar would share the window's bar and
    /// lose ⤢ and the menu, so the pane draws its own header row.
    private var inlineHeader: Bool { CompanionPaneChrome.usesInlineHeader(pane, verticalBar: verticalBar) }

    init(model: @autoclosure @escaping () -> TerminalSessionModel) {
        _terminal = StateObject(wrappedValue: model())
    }

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                statusBanner
                noticeBanner
                TerminalSurface(model: terminal, theme: theme, typography: typography,
                                baseFontSize: $baseFontSize, dynamicTypeSize: dynamicTypeSize,
                                keysPlacement: .for(UIDevice.current.userInterfaceIdiom, inCompanionPane: pane != nil),
                                keysBottomMargin: pane?.panelBottomMargin ?? ToolPanelMetrics.bottomMargin)
            }
            // Unfolded on a fold the pane reaches the window's bottom, so the
            // key panel ends where the composer on the other pane does.
            .ignoresSafeArea(.container, edges: pane?.alignsPanelToComposer == true ? .bottom : [])
            // The terminal sits on the page colour (owner decision O1).
            .background(theme.page)
            .wonderPage()
            .safeAreaInset(edge: .top, spacing: 0) {
                if inlineHeader {
                    CompanionInlineHeader {
                        titleLabel(alignment: .leading)
                    } controls: {
                        menu.frame(minWidth: 44, minHeight: 44)
                        doneControl
                    }
                }
            }
            // Only interior slot edges can discard an inherited window inset.
            // The edge beside the actual window rail keeps its native safe area.
            .ignoresSafeArea(.container, edges: pane?.interiorHorizontalEdges ?? [])
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if !inlineHeader {
                    ToolbarItem(placement: .principal) { titleLabel(alignment: .center) }
                    ToolbarItem(placement: .topBarTrailing) {
                        HStack {
                            menu
                            doneControl
                            CompanionExpandButton()
                        }
                    }
                }
            }
            .confirmationDialog("End this terminal?", isPresented: $confirmingEnd, titleVisibility: .visible) {
                Button("End terminal", role: .destructive) { endTerminal() }
            } message: {
                Text(terminal.shells.count > 1
                     ? "This shell and anything running in it stop on your computer. Your other terminals keep running."
                     : "The shell and anything running in it stop on your computer.")
            }
        }
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background: terminal.suspend()
            case .active:
                terminal.resume()
                Task { await terminal.refreshShells() }
            default: break
            }
        }
        .onDisappear { terminal.suspend() }
        .readsVerticalBar($verticalBar)
        // The view reports its grid after layout; start anyway if it never does.
        .task {
            try? await Task.sleep(for: .milliseconds(600))
            terminal.startIfIdle()
        }
        .accessibilityIdentifier("terminal-sheet")
    }

    /// Ends only the shown shell. Once the computer confirmed it, the sheet
    /// shows another of the chat's shells, or closes when none is left;
    /// otherwise it stays with the reason and Try Again.
    private func endTerminal() {
        Task {
            guard await terminal.end() != .failed else { return }
            let showsAnother = await terminal.showRemainingShell()
            if !showsAnother { dismiss() }
        }
    }

    private var doneControl: some View {
        ToolCloseControl {
            Button("Done") { dismiss() }
                .accessibilityHint("Keeps the shell running on your computer for 30 minutes")
                .accessibilityIdentifier("terminal-done")
        }
    }

    private func titleLabel(alignment: HorizontalAlignment) -> some View {
        VStack(alignment: alignment, spacing: 0) {
            Text(terminal.title).font(.headline).lineLimit(1)
            if let subtitle = terminal.subtitle {
                Text(subtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(terminal.session.flatMap { terminal.shells.count > 1 ? terminal.name(of: $0.id) : nil }
            .map { "\($0) in \(terminal.title)" } ?? "Terminal in \(terminal.title)")
        .accessibilityIdentifier("terminal-title")
    }

    private var menu: some View {
        Menu {
            shellsSection
            Section("Text size") {
                Button("Larger", systemImage: "textformat.size.larger") { baseFontSize = TerminalFontSize.step(baseFontSize, by: 1) }
                    .disabled(baseFontSize >= TerminalFontSize.range.upperBound)
                Button("Smaller", systemImage: "textformat.size.smaller") { baseFontSize = TerminalFontSize.step(baseFontSize, by: -1) }
                    .disabled(baseFontSize <= TerminalFontSize.range.lowerBound)
                Button("Default size", systemImage: "textformat.size") { baseFontSize = TerminalFontSize.standard }
                    .disabled(baseFontSize == TerminalFontSize.standard)
            }
            Section {
                Toggle("Share clipboard", systemImage: "doc.on.clipboard", isOn: $sharesClipboard)
                    .accessibilityHint("Off by default. Lets programs in this terminal copy text to this \(UIDevice.current.model).")
                    .accessibilityIdentifier("terminal-share-clipboard")
            }
            Section {
                Button("End terminal", systemImage: "xmark.octagon", role: .destructive) { confirmingEnd = true }
                    .disabled(terminal.session == nil || terminal.phase == .closed || terminal.ending)
                    .accessibilityIdentifier("terminal-end")
            } footer: {
                Text("Closing this view keeps the shell running on your computer for 30 minutes.")
            }
        } label: {
            Image(systemName: "ellipsis.circle")
                .accessibilityLabel("Terminal options")
        }
        .accessibilityIdentifier("terminal-menu")
    }

    /// The chat's shells (each keeps running while another is shown) and New
    /// Terminal, shared by the iPhone sheet and the iPad and Duo pane.
    @ViewBuilder private var shellsSection: some View {
        Section {
            if terminal.shells.count > 1 {
                Picker("Terminal", selection: Binding(get: { terminal.session?.id ?? "" },
                                                      set: { terminal.select($0) })) {
                    ForEach(terminal.shells) { shell in
                        Text("\(terminal.name(of: shell.id) ?? shell.title) · \(terminal.isRunning(shell) ? "Running" : "Ended")")
                            .tag(shell.id)
                    }
                }
                .pickerStyle(.inline)
                .disabled(terminal.ending)
                .accessibilityIdentifier("terminal-shells")
            }
            Button("New terminal", systemImage: "plus") { terminal.openAnother() }
                .disabled(!terminal.canOpenAnother)
                .accessibilityHint("Opens another shell in this project’s folder. The others keep running.")
                .accessibilityIdentifier("terminal-new")
        } footer: {
            if !terminal.canOpenAnother, !terminal.ending {
                Text("Your computer keeps up to \(TerminalSessionModel.shellLimit) terminals for this \(UIDevice.current.model). End one to open another.")
            }
        }
    }

    @ViewBuilder private var statusBanner: some View {
        if terminal.ending {
            banner(Text("Ending the terminal on your computer…"), progress: true)
                .accessibilityIdentifier("terminal-ending")
        } else if let failure = terminal.endFailure, terminal.phase != .closed {
            banner(Text(failure), action: ("Try again", endTerminal))
                .accessibilityIdentifier("terminal-end-failed")
        } else {
            phaseBanner
        }
    }

    @ViewBuilder private var phaseBanner: some View {
        switch terminal.phase {
        case .live:
            EmptyView()
        case .starting, .connecting:
            banner(Text(terminal.phase == .starting ? "Starting a shell on your computer…" : "Connecting to your computer…"), progress: true)
                .accessibilityIdentifier("terminal-connecting")
        case .reconnecting:
            banner(Text("Reconnecting to your computer… Output you missed appears when it’s back."), progress: true)
                .accessibilityIdentifier("terminal-reconnecting")
        case .exited(let code):
            banner(Text(code.map { "The shell ended (exit \($0))." } ?? "The shell ended."), action: ("New terminal", terminal.openAnother))
                .accessibilityIdentifier("terminal-exited")
        case .closed:
            banner(Text("This terminal was closed on your computer."), action: ("New terminal", terminal.openAnother))
                .accessibilityIdentifier("terminal-closed")
        case .failed(let message):
            banner(Text(message), action: ("Try again", terminal.startNew))
                .accessibilityIdentifier("terminal-failed")
        case .outsideProject(let message):
            let addFolder: (String, () -> Void)? = terminal.addingFolder ? nil : ("Add folder", { terminal.addFolderAndStart() })
            banner(Text(terminal.addFolderFailure ?? message), progress: terminal.addingFolder, action: addFolder)
                .accessibilityIdentifier("terminal-outside-project")
        case .unavailable(let message):
            banner(Text(message))
                .accessibilityIdentifier("terminal-relay-unavailable")
        case .accessEnded(let message):
            banner(Text(message), action: ("Try again", terminal.retry))
                .accessibilityIdentifier("terminal-access-ended")
        }
    }

    /// Input and clipboard notes. A held paste stays until it's sent; other
    /// notes clear after a few seconds.
    @ViewBuilder private var noticeBanner: some View {
        if terminal.heldPaste != nil {
            let live: Bool = terminal.phase == .live
            let paste: (String, () -> Void)? = live ? ("Paste", { terminal.sendHeldPaste() }) : nil
            let message: String = live ? "Your paste is ready to send."
                : (terminal.notice ?? "Your paste is waiting for the terminal to connect.")
            banner(Text(message), action: paste)
                .accessibilityIdentifier("terminal-held-paste")
        } else if let notice = terminal.notice {
            banner(Text(notice))
                .accessibilityIdentifier("terminal-notice")
                .task(id: notice) {
                    AccessibilityNotification.Announcement(notice).post()
                    try? await Task.sleep(for: .seconds(4))
                    if !Task.isCancelled { terminal.clearNotice(notice) }
                }
        }
    }

    private func banner(_ text: Text, progress: Bool = false, action: (String, () -> Void)? = nil) -> some View {
        HStack(spacing: 10) {
            if progress { ProgressView().controlSize(.small) }
            text.font(.footnote).frame(maxWidth: .infinity, alignment: .leading)
            if let action {
                Button(action.0, action: action.1).font(.footnote.weight(.semibold)).buttonStyle(.borderless)
            }
        }
        .padding(.horizontal, 14).padding(.vertical, 8)
        .background(theme.surface)
        .accessibilityElement(children: .combine)
    }
}

enum TerminalFontSize {
    static let standard: Double = 13
    static let range: ClosedRange<Double> = 8...28
    static func step(_ size: Double, by delta: Double) -> Double {
        min(range.upperBound, max(range.lowerBound, (size + delta).rounded()))
    }
    /// Dynamic Type scales the owner's size, within limits that keep a usable grid.
    static func scaled(_ size: Double, for dynamicType: DynamicTypeSize) -> CGFloat {
        let traits = UITraitCollection(preferredContentSizeCategory: UIContentSizeCategory(dynamicType))
        return min(40, UIFontMetrics(forTextStyle: .body).scaledValue(for: CGFloat(size), compatibleWith: traits))
    }
}

// MARK: - SwiftTerm surface

/// Where the extra keys go. A phone-width sheet spans the screen, so they ride
/// on the keyboard. Anywhere the terminal is narrower than the keyboard (an
/// iPad's centred sheet, the companion pane beside a chat on an iPad, an
/// unfolded Duo or a large iPhone on its side), an accessory would run across
/// the whole window under the chat, so the keys sit under the terminal instead.
enum TerminalKeysPlacement: Equatable {
    case keyboardAccessory
    case inline

    static func `for`(_ idiom: UIUserInterfaceIdiom, inCompanionPane: Bool) -> Self {
        idiom == .pad || inCompanionPane ? .inline : .keyboardAccessory
    }
}

/// The terminal with its extra keys, placed for where it is shown. The
/// placement can change without replacing the terminal, its scrollback, the
/// Ctrl latch or the keyboard focus.
final class TerminalPane: UIView {
    /// The space between the pane's edge and the text, on every device.
    static let contentInset: CGFloat = 10
    let terminal = WonderTerminalView(frame: .zero)
    let keys: TerminalKeysBar
    private let viewport = UIScrollView()
    private var hostGrid: TerminalHostGrid?
    private var measuredGrid: TerminalHostGrid?
    var onGrid: ((Int, Int) -> Void)?
    private(set) var placement: TerminalKeysPlacement?
    private var keysHeight: NSLayoutConstraint?
    private var placementConstraints: [NSLayoutConstraint] = []

    init(placement: TerminalKeysPlacement) {
        keys = TerminalKeysBar(terminal: terminal)
        super.init(frame: .zero)
        keys.onPaste = { [weak terminal] in terminal?.paste(nil) }
        viewport.translatesAutoresizingMaskIntoConstraints = false
        viewport.contentInsetAdjustmentBehavior = .never
        viewport.isDirectionalLockEnabled = true
        addSubview(viewport)
        viewport.addSubview(terminal)
        NSLayoutConstraint.activate([
            viewport.leadingAnchor.constraint(equalTo: leadingAnchor),
            viewport.trailingAnchor.constraint(equalTo: trailingAnchor),
            viewport.topAnchor.constraint(equalTo: topAnchor),
        ])
        setPlacement(placement)
    }

    required init?(coder: NSCoder) { nil }

    /// Moves the keys; nothing happens when the placement is unchanged. The
    /// keyboard reloads its accessory only when it actually changed.
    func setPlacement(_ next: TerminalKeysPlacement) {
        guard next != placement else { return }
        let first = placement == nil
        placement = next
        NSLayoutConstraint.deactivate(placementConstraints)
        switch next {
        case .keyboardAccessory:
            keys.removeFromSuperview()
            keys.translatesAutoresizingMaskIntoConstraints = true
            keys.setKeyboardAccessory(true)
            terminal.inputAccessoryView = keys
            placementConstraints = [viewport.bottomAnchor.constraint(equalTo: bottomAnchor)]
        case .inline:
            // SwiftTerm installs its own accessory; the keys below replace it.
            terminal.inputAccessoryView = nil
            keys.setKeyboardAccessory(false)
            keys.translatesAutoresizingMaskIntoConstraints = false
            addSubview(keys)
            let height = keys.heightAnchor.constraint(equalToConstant: TerminalKeysBar.inlineHeight(bottomMargin: keys.bottomMargin))
            keysHeight = height
            placementConstraints = [
                keys.leadingAnchor.constraint(equalTo: leadingAnchor),
                keys.trailingAnchor.constraint(equalTo: trailingAnchor),
                keys.topAnchor.constraint(equalTo: viewport.bottomAnchor),
                keys.bottomAnchor.constraint(equalTo: bottomAnchor),
                height,
            ]
        }
        NSLayoutConstraint.activate(placementConstraints)
        if !first, terminal.isFirstResponder { terminal.reloadInputViews() }
    }

    /// A host update changes the emulator itself, not just the surrounding
    /// whitespace. Larger grids can be panned without wrapping at this viewer's edge.
    func applyGrid(cols: Int, rows: Int) {
        guard TerminalGrid.columns.contains(cols), TerminalGrid.rows.contains(rows) else { return }
        hostGrid = TerminalHostGrid(cols: cols, rows: rows)
        let emulator = terminal.getTerminal()
        if emulator.cols != cols || emulator.rows != rows { emulator.resize(cols: cols, rows: rows) }
        layoutTerminal()
    }

    func setFont(_ font: UIFont) {
        guard terminal.font != font else { return }
        // SwiftTerm's font setter otherwise auto-fits the old frame and reflows
        // existing text before we can restore the shared grid.
        terminal.frame = .zero
        terminal.font = font
        layoutTerminal()
    }

    override func layoutSubviews() {
        if placement == .inline {
            let height = TerminalKeysBar.inlineHeight(bottomMargin: keys.bottomMargin)
            if keysHeight?.constant != height { keysHeight?.constant = height }
        }
        super.layoutSubviews()
        layoutTerminal()
    }

    private func layoutTerminal() {
        let inset = Self.contentInset
        var available = viewport.bounds.insetBy(dx: inset, dy: inset)
        if placement == .inline { available.size.height += inset / 2 }
        guard available.width > 0, available.height > 0 else { return }
        let emulator = terminal.getTerminal()
        let optimal = terminal.getOptimalFrameSize().size
        let cell = CGSize(width: optimal.width / CGFloat(emulator.cols),
                          height: optimal.height / CGFloat(emulator.rows))
        guard cell.width > 0, cell.height > 0 else { return }
        let measured = TerminalHostGrid(
            cols: min(500, max(2, Int(available.width / cell.width))),
            rows: min(300, max(1, Int(available.height / cell.height))))
        if measured != measuredGrid {
            measuredGrid = measured
            // UIKit layout must not publish into SwiftUI synchronously.
            DispatchQueue.main.async { [weak self] in
                guard let self, self.measuredGrid == measured else { return }
                self.onGrid?(measured.cols, measured.rows)
            }
        }
        let size: CGSize
        if let hostGrid {
            // A fraction of a cell avoids floating-point rounding down in SwiftTerm's layout.
            size = CGSize(width: (CGFloat(hostGrid.cols) + 0.001) * cell.width,
                          height: (CGFloat(hostGrid.rows) + 0.001) * cell.height)
        } else {
            size = available.size
        }
        terminal.frame = CGRect(origin: CGPoint(x: inset, y: inset), size: size)
        let bottomInset = placement == .inline ? inset / 2 : inset
        viewport.contentSize = CGSize(width: size.width + 2 * inset, height: size.height + inset + bottomInset)
    }

    func restyle(page: UIColor, surface: UIColor, accent: UIColor, glyph: UIColor, armedGlyph: UIColor) {
        backgroundColor = page
        keys.restyle(page: page, surface: surface, accent: accent, glyph: glyph, armedGlyph: armedGlyph)
    }
}

private struct TerminalSurface: UIViewRepresentable {
    @ObservedObject var model: TerminalSessionModel
    let theme: WonderTheme
    let typography: WonderTypography
    @Binding var baseFontSize: Double
    let dynamicTypeSize: DynamicTypeSize
    let keysPlacement: TerminalKeysPlacement
    /// The margin under the inline key panel; see `TerminalKeysBar.bottomMargin`.
    var keysBottomMargin = ToolPanelMetrics.bottomMargin

    func makeCoordinator() -> Coordinator { Coordinator(model: model) }

    func makeUIView(context: Context) -> TerminalPane {
        let pane = TerminalPane(placement: keysPlacement)
        let view = pane.terminal
        view.terminalDelegate = context.coordinator
        pane.onGrid = { [weak model] cols, rows in model?.gridChanged(cols: cols, rows: rows) }
        view.optionAsMetaKey = true
        view.accessibilityIdentifier = "terminal-output"
        view.onPaste = { [weak model] text, bracketed in model?.paste(TerminalPaste.bytes(text, bracketed: bracketed)) }
        let pinch = UIPinchGestureRecognizer(target: context.coordinator, action: #selector(Coordinator.pinched(_:)))
        view.addGestureRecognizer(pinch)
        context.coordinator.view = view
        context.coordinator.fontSize = $baseFontSize
        context.coordinator.openLink = context.environment.openURL
        model.attachSurface(owner: pane, feed: { [weak view] bytes in view?.feed(byteArray: bytes[...]) },
                            clear: { [weak view] in view?.feed(text: "\u{1b}c") },
                            resize: { [weak pane] cols, rows in pane?.applyGrid(cols: cols, rows: rows) })
        DispatchQueue.main.async { [weak view] in _ = view?.becomeFirstResponder() }
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-ux3-terminal") {
            context.coordinator.signalTask = Task { @MainActor [weak pane] in
                var consumed: Set<String> = []
                while !Task.isCancelled {
                    for name in ["ux3-keyboard-first", "ux3-keyboard-second", "ux3-keyboard-third"] where !consumed.contains(name) && DiagnosticSubagentFixture.signalled(name) {
                        consumed.insert(name)
                        // The inline panel nests its keys in a scroll view.
                        func hideKey(in view: UIView) -> UIButton? {
                            if let button = view as? UIButton, button.accessibilityIdentifier == "terminal-key-hide" { return button }
                            return view.subviews.lazy.compactMap(hideKey).first
                        }
                        if let keys = pane?.keys { hideKey(in: keys)?.sendActions(for: .primaryActionTriggered) }
                    }
                    if let pane {
                        let state: [String: Any] = ["focused": pane.terminal.isFirstResponder,
                            "keyboardHeight": pane.window?.keyboardLayoutGuide.layoutFrame.height ?? 0,
                            "signals": consumed.sorted()]
                        if let data = try? JSONSerialization.data(withJSONObject: state, options: .sortedKeys) {
                            try? data.write(to: FileManager.default.temporaryDirectory.appendingPathComponent("ux3-terminal-keyboard.json"), options: .atomic)
                        }
                    }
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
        }
        #endif
        return pane
    }

    func updateUIView(_ pane: TerminalPane, context: Context) {
        context.coordinator.fontSize = $baseFontSize
        context.coordinator.openLink = context.environment.openURL
        // Before the style check: a placement change must not wait for a new theme.
        pane.setPlacement(keysPlacement)
        pane.keys.bottomMargin = keysBottomMargin
        let style = TerminalStyle(theme: theme, typography: typography,
                                  size: TerminalFontSize.scaled(baseFontSize, for: dynamicTypeSize))
        // Idempotent: only a changed theme or size touches the view.
        guard context.coordinator.style != style else { return }
        context.coordinator.style = style
        pane.setFont(style.font)
        style.apply(to: pane.terminal)
        pane.restyle(page: style.page, surface: style.surface, accent: style.accentColor,
                     glyph: style.keyGlyph, armedGlyph: style.armedKeyGlyph)
    }

    static func dismantleUIView(_ pane: TerminalPane, coordinator: Coordinator) {
        #if WONDER_DIAGNOSTICS
        coordinator.signalTask?.cancel()
        #endif
        coordinator.model?.detachSurface(owner: pane)
    }

    @MainActor final class Coordinator: NSObject, @preconcurrency TerminalViewDelegate {
        weak var model: TerminalSessionModel?
        weak var view: WonderTerminalView?
        var fontSize: Binding<Double>?
        /// The app's link opener, so links follow Settings → Link opening.
        var openLink: OpenURLAction?
        var style: TerminalStyle?
        #if WONDER_DIAGNOSTICS
        var signalTask: Task<Void, Never>?
        #endif
        private var pinchStart: Double = TerminalFontSize.standard

        init(model: TerminalSessionModel) { self.model = model }

        func send(source: TerminalView, data: ArraySlice<UInt8>) { model?.send(Array(data)) }
        // Only the viewport measures desired space. Emulator changes include
        // the host's grid and must never echo back as another resize request.
        func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {}
        func setTerminalTitle(source: TerminalView, title: String) {}
        func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        func scrolled(source: TerminalView, position: Double) {}
        func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
            guard let url = URL(string: link), ["http", "https"].contains(url.scheme?.lowercased()) else { return }
            openLink?(url)
        }
        func bell(source: TerminalView) {}
        /// Only while clipboard sharing is on, read at the moment of the copy.
        func clipboardCopy(source: TerminalView, content: Data) {
            model?.copyFromComputer(content, sharingAllowed: UserDefaults.standard.bool(forKey: ComputerPhoneClipboard.sharingKey))
        }
        /// Programs on the Mac cannot read this phone's clipboard.
        func clipboardRead(source: TerminalView) -> Data? { nil }
        func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
        func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}

        @objc func pinched(_ gesture: UIPinchGestureRecognizer) {
            guard let fontSize else { return }
            switch gesture.state {
            case .began: pinchStart = fontSize.wrappedValue
            case .changed:
                let next = min(TerminalFontSize.range.upperBound, max(TerminalFontSize.range.lowerBound, (pinchStart * gesture.scale).rounded()))
                if next != fontSize.wrappedValue { fontSize.wrappedValue = next }
            default: break
            }
        }
    }
}

/// The resolved look of a terminal: colours from the Wonder theme and the
/// code font at the scaled size.
private struct TerminalStyle: Equatable {
    /// The theme and appearance; every colour below follows from it.
    let themeKey: String
    let page: UIColor
    let surface: UIColor
    let foreground: UIColor
    let accentColor: UIColor
    let keyGlyph: UIColor
    let armedKeyGlyph: UIColor
    let isDark: Bool
    let codeFont: String
    let size: CGFloat
    let font: UIFont

    init(theme: WonderTheme, typography: WonderTypography, size: CGFloat) {
        themeKey = theme.cacheKey
        page = theme.pageUIColor
        surface = theme.surfaceUIColor
        foreground = theme.textUIColor.resolvedColor(with: UITraitCollection(userInterfaceStyle: theme.isDark ? .dark : .light))
        accentColor = UIColor(hex: theme.palette.accent)
        let keys = TerminalKeyColors(palette: theme.palette)
        keyGlyph = UIColor(hex: keys.glyph)
        armedKeyGlyph = UIColor(hex: keys.armedGlyph)
        isDark = theme.isDark
        codeFont = typography.cacheKey
        self.size = size
        let base = typography.codeUIFont(.body, size: 13)
        font = base.withSize(size)
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.themeKey == rhs.themeKey && lhs.codeFont == rhs.codeFont && lhs.size == rhs.size
    }

    @MainActor func apply(to view: WonderTerminalView) {
        view.nativeBackgroundColor = page
        view.nativeForegroundColor = foreground
        view.caretColor = accentColor
        view.selectedTextBackgroundColor = accentColor.withAlphaComponent(0.35)
        view.keyboardAppearance = isDark ? .dark : .light
        view.installColors(isDark ? TerminalPalette.dark : TerminalPalette.light)
    }
}

/// The extra keys' glyphs for a palette. A key is the agent bubble's colour on
/// the page with its glyph in the accent, moved toward the text in tenths until
/// it reads at 4.5:1 on the key. An armed Ctrl fills with the accent; its glyph
/// is the first of the page, the text, black or white that reads on it.
struct TerminalKeyColors: Equatable {
    static let minimumContrast = 4.5
    let glyph: UInt32
    let armedGlyph: UInt32

    init(palette: ThemePalette) {
        let readable = { (color: UInt32, on: UInt32) in ThemePalette.contrast(color, on) >= Self.minimumContrast }
        glyph = (0...10).lazy.map { Self.mix(palette.accent, palette.primaryText, Double($0) / 10) }
            .first { readable($0, palette.agentBubble) } ?? palette.primaryText
        let candidates: [UInt32] = [palette.background, palette.primaryText, 0x000000, 0xFFFFFF]
        armedGlyph = candidates.first { readable($0, palette.accent) }
            ?? candidates.max { ThemePalette.contrast($0, palette.accent) < ThemePalette.contrast($1, palette.accent) }!
    }

    private static func mix(_ a: UInt32, _ b: UInt32, _ amount: Double) -> UInt32 {
        func channel(_ shift: UInt32) -> UInt32 {
            let x = Double((a >> shift) & 0xFF), y = Double((b >> shift) & 0xFF)
            return UInt32((x + (y - x) * amount).rounded()) << shift
        }
        return channel(16) | channel(8) | channel(0)
    }
}

/// ANSI colours readable on the theme's page. The light palette
/// darkens yellow, cyan and the bright colours, which wash out on white.
@MainActor private enum TerminalPalette {
    private static func colors(_ values: [UInt32]) -> [SwiftTerm.Color] {
        values.map { SwiftTerm.Color(red: UInt16(($0 >> 16) & 0xff) * 257, green: UInt16(($0 >> 8) & 0xff) * 257, blue: UInt16($0 & 0xff) * 257) }
    }
    static let dark = colors([0x000000, 0xE5534B, 0x57AB5A, 0xC69026, 0x539BF5, 0xB083F0, 0x39C5CF, 0xD0D7DE,
                              0x6E7681, 0xFF7B72, 0x7EE787, 0xF2CC60, 0x79C0FF, 0xD2A8FF, 0x56D4DD, 0xFFFFFF])
    static let light = colors([0x24292F, 0xCF222E, 0x116329, 0x7D4E00, 0x0550AE, 0x8250DF, 0x1B7C83, 0x6E7781,
                               0x57606A, 0xA40E26, 0x1A7F37, 0x633C01, 0x218BFF, 0xA475F9, 0x3192AA, 0x8C959F])
}

/// SwiftTerm's view with Wonder's paste, labels and VoiceOver additions.
final class WonderTerminalView: TerminalView {
    var onPaste: ((String, Bool) -> Void)?
    var onFocusChange: (() -> Void)?

    override func becomeFirstResponder() -> Bool {
        let result = super.becomeFirstResponder()
        onFocusChange?()
        return result
    }

    override func resignFirstResponder() -> Bool {
        let result = super.resignFirstResponder()
        onFocusChange?()
        return result
    }

    /// Bracketed when the running program asked for it, so a pasted command
    /// does not run by itself.
    override func paste(_ sender: Any?) {
        guard let text = UIPasteboard.general.string, !text.isEmpty else { return }
        onPaste?(text, getTerminal().bracketedPasteMode)
    }

    override var accessibilityLabel: String? {
        get { "Terminal output" }
        set {}
    }

    /// The newest lines, so VoiceOver reads what just happened on focus.
    override var accessibilityValue: String? {
        get { lastLines(6) }
        set {}
    }

    override var accessibilityCustomActions: [UIAccessibilityCustomAction]? {
        get {
            [UIAccessibilityCustomAction(name: "Read last lines") { [weak self] _ in
                guard let self else { return false }
                UIAccessibility.post(notification: .announcement, argument: self.lastLines(5))
                return true
            }]
        }
        set {}
    }

    /// The last non-empty lines of the screen, for VoiceOver.
    func lastLines(_ count: Int) -> String {
        let terminal = getTerminal()
        var lines: [String] = []
        var row = terminal.rows - 1
        while row >= 0, lines.count < count {
            // Unwritten cells read as NUL; VoiceOver should hear spaces.
            if let line = terminal.getLine(row: row)?.translateToString(trimRight: true)
                .replacingOccurrences(of: "\0", with: " "),
               !line.trimmingCharacters(in: .whitespaces).isEmpty {
                lines.insert(line, at: 0)
            }
            row -= 1
        }
        return lines.isEmpty ? "No output yet" : lines.joined(separator: "\n")
    }
}

// MARK: - Extra keys

/// The inline key row: it fades at an edge with more keys behind it, so a
/// narrow pane says the row scrolls.
private final class FadingKeyStrip: UIScrollView {
    private let fade = CAGradientLayer()

    override init(frame: CGRect) {
        super.init(frame: frame)
        fade.startPoint = CGPoint(x: 0, y: 0.5)
        fade.endPoint = CGPoint(x: 1, y: 0.5)
        layer.mask = fade
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override var contentOffset: CGPoint {
        didSet { if contentOffset.x != oldValue.x { updateFade() } }
    }

    func updateFade() {
        let overflow = contentSize.width - bounds.width
        let leading = overflow > 1 && contentOffset.x > 1
        let trailing = overflow > 1 && contentOffset.x < overflow - 1
        let edge = bounds.width > 0 ? min(0.25, 56 / bounds.width) : 0
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        // A scroll view's mask scrolls with its content, so it follows the offset.
        fade.frame = CGRect(origin: contentOffset, size: bounds.size)
        fade.colors = [leading ? UIColor.clear.cgColor : UIColor.black.cgColor, UIColor.black.cgColor,
                       UIColor.black.cgColor, trailing ? UIColor.clear.cgColor : UIColor.black.cgColor]
        fade.locations = [0, NSNumber(value: Double(edge)), NSNumber(value: Double(1 - edge)), 1]
        CATransaction.commit()
    }
}

/// Keys a phone keyboard lacks, above the keyboard or under the terminal: a Ctrl latch, Esc, Tab, arrows that follow the program's cursor
/// mode, and common shell symbols.
final class TerminalKeysBar: UIInputView {
    static let height: CGFloat = 56
    /// Under the terminal the keys are one panel with the chat composer's shape
    /// and bottom edge (`ToolPanelMetrics`), the margin below it included.
    static func inlineHeight(bottomMargin: CGFloat) -> CGFloat { ToolPanelMetrics.height + bottomMargin }
    /// The margin under the panel: the composer dock's, or (on an unfolded
    /// fold, where the pane reaches the window's bottom) the composer's rise.
    var bottomMargin = ToolPanelMetrics.bottomMargin {
        didSet {
            guard bottomMargin != oldValue else { return }
            invalidateIntrinsicContentSize()
            setNeedsLayout()
        }
    }
    /// The panel's side margin, the composer dock's.
    private static let panelMargin: CGFloat = 16
    private static func columns(for width: CGFloat) -> Int {
        let fitting = max(1, min(10, Int(max(0, width - 10) / 58)))
        // Keep the cursor keys together on a second row in a narrow pane.
        return fitting < 10 ? min(5, fitting) : 10
    }
    static func height(for width: CGFloat) -> CGFloat {
        let columns = columns(for: width)
        return CGFloat((10 + columns - 1) / columns) * 50 + 6
    }
    private weak var terminal: WonderTerminalView?
    private var controlButton: UIButton?
    private var keyboardButton: UIButton?
    private var buttons: [UIButton] = []
    private var measuredHeight: CGFloat = 0
    private var accessoryHeight: NSLayoutConstraint?
    /// Inline, the keys sit in one row inside a composer-shaped panel that
    /// scrolls sideways when the pane is narrower than the row.
    private var panelStyle = false
    private let panel = UIView()
    private let strip = FadingKeyStrip()
    /// The theme's key colours; nil keeps the system's until the first restyle.
    private var surface: UIColor?
    private var accent: UIColor?
    private var glyph: UIColor?
    private var armedGlyph: UIColor?
    var onPaste: (() -> Void)?

    init(terminal: WonderTerminalView) {
        self.terminal = terminal
        // It draws the page colour, on the keyboard or under the terminal.
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: Self.height(for: 320)), inputViewStyle: .default)
        allowsSelfSizing = true
        controlButton = add(title: "Ctrl", label: "Control", id: "terminal-key-ctrl") { [weak self] in self?.toggleControl() }
        add(title: "Esc", label: "Escape", id: "terminal-key-esc") { [weak self] in self?.key(.escape) }
        add(title: "Tab", label: "Tab", id: "terminal-key-tab") { [weak self] in self?.key(.tab) }
        add(symbol: "doc.on.clipboard", label: "Paste", id: "terminal-key-paste") { [weak self] in self?.onPaste?() }
        keyboardButton = add(symbol: "keyboard", label: "Show keyboard", id: "terminal-key-hide") { [weak self] in
            guard let terminal = self?.terminal else { return }
            if terminal.isFirstResponder { _ = terminal.resignFirstResponder() }
            else { _ = terminal.becomeFirstResponder() }
        }
        terminal.onFocusChange = { [weak self] in self?.updateKeyboardButton() }
        updateKeyboardButton()
        add(symbol: "arrow.left", label: "Left arrow", id: "terminal-key-left") { [weak self] in self?.key(.left) }
        add(symbol: "arrow.down", label: "Down arrow", id: "terminal-key-down") { [weak self] in self?.key(.down) }
        add(symbol: "arrow.up", label: "Up arrow", id: "terminal-key-up") { [weak self] in self?.key(.up) }
        add(symbol: "arrow.right", label: "Right arrow", id: "terminal-key-right") { [weak self] in self?.key(.right) }
        let symbols = add(symbol: "ellipsis", label: "Shell symbols", id: "terminal-key-symbols") {}
        symbols.menu = UIMenu(children: [("|", "Pipe"), ("~", "Tilde"), ("/", "Slash"), ("-", "Hyphen")].map { symbol, label in
            UIAction(title: "\(label)  \(symbol)") { [weak self] _ in self?.type(symbol) }
        })
        symbols.showsMenuAsPrimaryAction = true
        NotificationCenter.default.addObserver(self, selector: #selector(controlReset), name: .terminalViewControlModifierReset, object: terminal)
        updateControl()
    }

    required init?(coder: NSCoder) { nil }

    private func updateKeyboardButton() {
        let focused = terminal?.isFirstResponder == true
        keyboardButton?.configuration?.image = UIImage(systemName: focused ? "keyboard.chevron.compact.down" : "keyboard",
            withConfiguration: UIImage.SymbolConfiguration(pointSize: 18, weight: .regular))
        keyboardButton?.accessibilityLabel = focused ? "Hide keyboard" : "Show keyboard"
        keyboardButton?.accessibilityValue = focused ? "Shown" : "Hidden"
    }

    private func setPanelStyle(_ on: Bool) {
        guard on != panelStyle else { return }
        panelStyle = on
        panel.removeFromSuperview()
        if on {
            panel.layer.cornerRadius = ToolPanelMetrics.cornerRadius
            panel.layer.cornerCurve = .circular
            panel.backgroundColor = surface
            strip.showsHorizontalScrollIndicator = false
            strip.alwaysBounceHorizontal = false
            addSubview(panel)
            panel.addSubview(strip)
            for button in buttons { strip.addSubview(button) }
        } else {
            for button in buttons { addSubview(button) }
        }
        for button in buttons where button !== controlButton { button.configuration?.baseBackgroundColor = idleBackground }
        updateControl()
        invalidateIntrinsicContentSize()
        setNeedsLayout()
    }

    /// A key's fill at rest: the theme's surface as a tile above the keyboard,
    /// none on the panel, which is the surface already.
    private var idleBackground: UIColor? { panelStyle ? .clear : surface }

    func setKeyboardAccessory(_ enabled: Bool) {
        setPanelStyle(!enabled)
        accessoryHeight?.isActive = false
        accessoryHeight = nil
        if enabled {
            let height = Self.height(for: bounds.width)
            frame.size.height = height
            let constraint = heightAnchor.constraint(equalToConstant: height)
            constraint.isActive = true
            accessoryHeight = constraint
        }
    }

    override var intrinsicContentSize: CGSize {
        CGSize(width: UIView.noIntrinsicMetric, height: panelStyle ? Self.inlineHeight(bottomMargin: bottomMargin) : Self.height(for: bounds.width))
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if panelStyle {
            panel.frame = CGRect(x: Self.panelMargin, y: 0, width: max(0, bounds.width - 2 * Self.panelMargin),
                                 height: max(0, bounds.height - bottomMargin))
            strip.frame = panel.bounds.insetBy(dx: ToolPanelMetrics.inset, dy: 0)
            // 40 pt wide at least (44 tall); a narrower pane scrolls the rest.
            let width = max(40, strip.bounds.width / CGFloat(max(1, buttons.count)))
            for (index, button) in buttons.enumerated() {
                button.frame = CGRect(x: CGFloat(index) * width, y: (strip.bounds.height - ToolPanelMetrics.rowHeight) / 2,
                                      width: width, height: ToolPanelMetrics.rowHeight)
            }
            strip.contentSize = CGSize(width: width * CGFloat(buttons.count), height: strip.bounds.height)
            strip.updateFade()
            return
        }
        let columns = Self.columns(for: bounds.width)
        let width = max(0, (bounds.width - 16 - CGFloat(columns - 1) * 6) / CGFloat(columns))
        for (index, button) in buttons.enumerated() {
            button.frame = CGRect(x: 8 + CGFloat(index % columns) * (width + 6),
                                  y: 6 + CGFloat(index / columns) * 50, width: width, height: 44)
        }
        // UIInputView sizes the phone keyboard accessory from this intrinsic
        // height; the inline host uses the same width-to-height contract.
        let height = Self.height(for: bounds.width)
        if measuredHeight != height {
            measuredHeight = height
            accessoryHeight?.constant = height
            invalidateIntrinsicContentSize()
        }
    }


    @discardableResult
    private func add(title: String? = nil, symbol: String? = nil, label: String, id: String, action: @escaping () -> Void) -> UIButton {
        var configuration = UIButton.Configuration.gray()
        configuration.cornerStyle = .medium
        configuration.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 2, bottom: 4, trailing: 2)
        configuration.titleLineBreakMode = .byClipping
        if let title {
            var attributed = AttributedString(title)
            attributed.font = UIFontMetrics(forTextStyle: .body).scaledFont(for: .monospacedSystemFont(ofSize: 14, weight: .medium), maximumPointSize: 16)
            configuration.attributedTitle = attributed
        }
        if let symbol {
            configuration.image = UIImage(systemName: symbol, withConfiguration: UIImage.SymbolConfiguration(pointSize: 18, weight: .regular))
        }
        let button = UIButton(configuration: configuration, primaryAction: UIAction { _ in action() })
        button.accessibilityLabel = label
        button.accessibilityIdentifier = id
        addSubview(button)
        buttons.append(button)
        return button
    }

    /// Theme-coloured keys (the surface, with accent glyphs) on the page colour.
    func restyle(page: UIColor, surface: UIColor, accent: UIColor, glyph: UIColor, armedGlyph: UIColor) {
        self.surface = surface
        self.accent = accent
        self.glyph = glyph
        self.armedGlyph = armedGlyph
        tintColor = accent
        backgroundColor = page
        panel.backgroundColor = surface
        for button in buttons where button !== controlButton {
            button.configuration?.baseBackgroundColor = idleBackground
            button.configuration?.baseForegroundColor = glyph
        }
        updateControl()
    }

    private func key(_ key: TerminalKey) {
        guard let terminal else { return }
        terminal.send(data: key.bytes(applicationCursor: terminal.getTerminal().applicationCursor)[...])
        if terminal.controlModifier { terminal.controlModifier = false }
    }

    private func type(_ text: String) {
        terminal?.insertText(text)
    }

    private func toggleControl() {
        guard let terminal else { return }
        terminal.controlModifier.toggle()
        updateControl()
    }

    @objc private func controlReset() { updateControl() }

    private func updateControl() {
        let on = terminal?.controlModifier == true
        // Armed, the key fills with the accent under a glyph that reads on it.
        controlButton?.configuration?.baseBackgroundColor = on ? (accent ?? tintColor) : idleBackground
        controlButton?.configuration?.baseForegroundColor = on ? armedGlyph : glyph
        controlButton?.accessibilityValue = on ? "On, applies to the next key" : "Off"
        controlButton?.accessibilityTraits = on ? [.button, .selected] : .button
    }
}
