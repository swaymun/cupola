#if WONDER_DIAGNOSTICS
import Foundation
import WonderPairing

/// A local stand-in for a terminal on the Mac, for Diagnostics previews and the
/// `terminal.wctl` scenario: recorded shell output (prompt, coloured `ls`, `git
/// status`), an echoing line editor, Ctrl-C, and a reconnect that replays only
/// what the view has not drawn. New Terminal opens more shells (up to the
/// computer's four), each with its own output, starting at a bare prompt.
/// `-diagnostics-terminal-preview` shows the Terminal entry in a preview
/// Project conversation.
@MainActor final class DiagnosticTerminalTransport: TerminalTransport {
    static var enabled: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-terminal-preview") }
    /// One fixture shell per launch, so reopening the sheet reattaches and replays.
    static let shared = DiagnosticTerminalTransport()

    /// One fixture shell: its output so far and the line being typed.
    private final class Shell {
        let session: TerminalSession
        var output: [UInt8]
        var line = ""
        weak var live: DiagnosticTerminalConnection?
        init(session: TerminalSession, output: String) { self.session = session; self.output = Array(output.utf8) }
    }

    private var shells: [Shell] = []
    private var opens = 0

    static let prompt = "\u{1b}[1;32mowner@mac\u{1b}[0m \u{1b}[1;34mwonder-app\u{1b}[0m % "
    static let transcript = [
        "Last login: Tue Oct 14 09:41:07 on ttys004\r\n",
        prompt, "git status\r\n",
        "On branch \u{1b}[32magent/terminal\u{1b}[0m\r\n",
        "Changes not staged for commit:\r\n",
        "\t\u{1b}[31mmodified:   Sources/TerminalSheet.swift\u{1b}[0m\r\n",
        "\t\u{1b}[31mmodified:   README.md\u{1b}[0m\r\n\r\n",
        prompt, "ls\r\n",
        "\u{1b}[1;34mApps\u{1b}[0m        \u{1b}[1;34mSources\u{1b}[0m     Package.swift  README.md\r\n",
        "\u{1b}[1;34mDocs\u{1b}[0m        \u{1b}[1;34mTests\u{1b}[0m       \u{1b}[32mbuild.sh\u{1b}[0m       日本語.txt 😀\r\n",
        prompt,
    ].joined()

    func list(conversationID: String) async throws -> [TerminalSession] { shells.map(\.session) }

    /// The first shell replays the recorded transcript; later ones start at a prompt.
    func open(conversationID: String, cols: Int, rows: Int) async throws -> TerminalSession {
        guard shells.count < TerminalSessionModel.shellLimit else {
            throw PairingFailure.hostMessage(429, "This device has the most terminals it can open. Close one first.")
        }
        opens += 1
        let session = TerminalSession(id: opens == 1 ? "term_00000000000000000000000000fixture" : "term_0000000000000000000000000fixture\(opens)",
                                      conversationId: "", title: "wonder-app", shell: "zsh", cols: 80, rows: 24,
                                      createdAt: "2026-10-14T12:0\(min(opens, 9)):00Z")
        shells.append(Shell(session: session, output: opens == 1 ? Self.transcript : Self.prompt))
        return session
    }

    /// End Terminal can't reach the computer, so the sheet must stay with the reason.
    static var endUnreachable: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-terminal-end-unreachable") }

    func close(terminalID: String) async throws {
        if Self.endUnreachable {
            try await Task.sleep(for: .milliseconds(400))
            throw URLError(.notConnectedToInternet)
        }
        guard let index = shells.firstIndex(where: { $0.session.id == terminalID }) else { return }
        shells.remove(at: index).live?.finish(.closed)
    }

    func connect(terminalID: String, sequence: UInt64?, cols: Int, rows: Int) async throws -> any TerminalConnection {
        guard let shell = shells.first(where: { $0.session.id == terminalID }) else { throw TerminalTransportError.gone }
        let output = shell.output
        let held = sequence.flatMap { $0 <= UInt64(output.count) ? $0 : nil }
        let connection = DiagnosticTerminalConnection(transport: self, terminalID: terminalID)
        connection.enqueue(.ready(sequence: held ?? 0, reset: held == nil, cols: cols, rows: rows, exited: false, exitCode: nil))
        let start = Int(held ?? 0)
        if start < output.count { connection.enqueue(.output(sequence: UInt64(start), bytes: Array(output[start...]))) }
        shell.live = connection
        return connection
    }

    fileprivate func input(_ bytes: [UInt8], to terminalID: String) {
        guard let shell = shells.first(where: { $0.session.id == terminalID }) else { return }
        var line = shell.line
        defer { shell.line = line }
        var echo: [UInt8] = []
        for byte in bytes {
            switch byte {
            case 0x03:
                line = ""
                echo += Array("^C\r\n\(Self.prompt)".utf8)
            case 0x0d:
                echo += Array("\r\n".utf8) + Array(run(line).utf8) + Array(Self.prompt.utf8)
                line = ""
            case 0x7f:
                if !line.isEmpty { line.removeLast(); echo += [0x08, 0x20, 0x08] }
            case 0x1b, 0x09:
                break
            default:
                line.unicodeScalars.append(UnicodeScalar(byte))
                echo.append(byte)
            }
        }
        guard !echo.isEmpty else { return }
        let sequence = UInt64(shell.output.count)
        shell.output += echo
        shell.live?.enqueue(.output(sequence: sequence, bytes: echo))
    }

    private func run(_ command: String) -> String {
        switch command.trimmingCharacters(in: .whitespaces) {
        case "": return ""
        case "pwd": return "/Users/owner/Projects/wonder-app\r\n"
        case let text where text.hasPrefix("echo "): return String(text.dropFirst(5)) + "\r\n"
        default: return "zsh: command not found: \(command)\r\n"
        }
    }
}

@MainActor final class DiagnosticTerminalConnection: TerminalConnection {
    private weak var transport: DiagnosticTerminalTransport?
    private let terminalID: String
    private enum Event {
        case message(TerminalServerMessage)
        case grid(Int, Int)
    }
    private var queue: [Event] = []
    private var waiter: CheckedContinuation<Event, Error>?
    private var gridReceiver: ((Int, Int) -> Void)?
    func observeGrid(_ receive: @escaping (Int, Int) -> Void) { gridReceiver = receive }
    private var cancelled = false

    init(transport: DiagnosticTerminalTransport, terminalID: String) { self.transport = transport; self.terminalID = terminalID }

    func enqueue(_ message: TerminalServerMessage) {
        enqueueEvent(.message(message))
    }

    private func enqueueEvent(_ event: Event) {
        if let waiter { self.waiter = nil; waiter.resume(returning: event) } else { queue.append(event) }
    }

    func finish(_ message: TerminalServerMessage) { enqueue(message) }

    func receive() async throws -> TerminalServerMessage {
        while !cancelled {
            let event: Event
            if !queue.isEmpty { event = queue.removeFirst() }
            else { event = try await withCheckedThrowingContinuation { waiter = $0 } }
            switch event {
            case .message(let message): return message
            case .grid(let cols, let rows): gridReceiver?(cols, rows)
            }
        }
        throw CancellationError()
    }

    func send(_ bytes: [UInt8]) async throws {
        if cancelled { throw CancellationError() }
        transport?.input(bytes, to: terminalID)
    }

    func resize(cols: Int, rows: Int) async throws {
        if cancelled { throw CancellationError() }
        enqueueEvent(.grid(cols, rows))
    }

    func cancel() {
        cancelled = true
        waiter?.resume(throwing: CancellationError())
        waiter = nil
    }
}
#endif
