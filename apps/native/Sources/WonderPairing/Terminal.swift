import Foundation

/// One owner terminal on the Mac: a login shell on a host PTY in a Project
/// conversation's working folder (`crates/wonderd/src/terminal.rs`).
public struct TerminalSession: Codable, Sendable, Identifiable, Equatable {
    public let id: String
    public let conversationId: String
    /// The working folder's name.
    public let title: String
    /// The login shell's name, such as `zsh`.
    public let shell: String
    public let cols: Int
    public let rows: Int
    public let createdAt: String
    /// Bytes of output the shell has produced so far.
    public let sequence: UInt64
    public let exited: Bool
    public let exitCode: Int?

    public init(id: String, conversationId: String, title: String, shell: String, cols: Int, rows: Int,
                createdAt: String, sequence: UInt64 = 0, exited: Bool = false, exitCode: Int? = nil) {
        self.id = id; self.conversationId = conversationId; self.title = title; self.shell = shell
        self.cols = cols; self.rows = rows; self.createdAt = createdAt
        self.sequence = sequence; self.exited = exited; self.exitCode = exitCode
    }
}

public struct TerminalList: Codable, Sendable {
    public let terminals: [TerminalSession]
}

public struct OpenTerminalRequest: Codable, Sendable, Equatable {
    public let cols: Int
    public let rows: Int
    public init(cols: Int, rows: Int) {
        self.cols = min(max(cols, TerminalGrid.columns.lowerBound), TerminalGrid.columns.upperBound)
        self.rows = min(max(rows, TerminalGrid.rows.lowerBound), TerminalGrid.rows.upperBound)
    }
}

/// The sizes the host accepts.
public enum TerminalGrid {
    public static let columns = 2...500
    public static let rows = 1...300
}

/// The first frame on `/api/v1/terminals/{id}/socket`: the same fresh signed
/// challenge the events stream uses, plus the stream offset this view holds.
public struct TerminalAttach: Encodable, Sendable {
    public let deviceId: String
    public let csrfToken: String
    public let challengeId: String
    public let signature: String
    public let sequence: UInt64?
    public let cols: Int
    public let rows: Int
    public init(deviceId: String, csrfToken: String, challengeId: String, signature: String,
                sequence: UInt64?, cols: Int, rows: Int) {
        self.deviceId = deviceId; self.csrfToken = csrfToken; self.challengeId = challengeId
        self.signature = signature; self.sequence = sequence; self.cols = cols; self.rows = rows
    }
}

/// What the host sends on a terminal socket.
public enum TerminalServerMessage: Equatable, Sendable {
    case ready(sequence: UInt64, reset: Bool, cols: Int, rows: Int, exited: Bool, exitCode: Int?)
    /// Output bytes starting at stream offset `sequence`.
    case output(sequence: UInt64, bytes: [UInt8])
    /// The host dropped bytes this view had not received; clear and resume.
    case reset(sequence: UInt64)
    case exit(code: Int?)
    case closed

    public struct Malformed: Error { public init() {} }

    /// A binary frame: an 8-byte big-endian offset, then output bytes.
    public static func binary(_ data: Data) throws -> TerminalServerMessage {
        guard data.count >= 8 else { throw Malformed() }
        let sequence = data.prefix(8).reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
        return .output(sequence: sequence, bytes: Array(data.dropFirst(8)))
    }

    public static func text(_ string: String) throws -> TerminalServerMessage {
        struct Control: Decodable {
            let type: String
            let sequence: UInt64?
            let reset: Bool?
            let cols: Int?
            let rows: Int?
            let exited: Bool?
            let exitCode: Int?
        }
        let control = try JSONDecoder().decode(Control.self, from: Data(string.utf8))
        switch control.type {
        case "ready":
            guard let sequence = control.sequence else { throw Malformed() }
            return .ready(sequence: sequence, reset: control.reset ?? true, cols: control.cols ?? 80,
                          rows: control.rows ?? 24, exited: control.exited ?? false, exitCode: control.exitCode)
        case "reset":
            guard let sequence = control.sequence else { throw Malformed() }
            return .reset(sequence: sequence)
        case "exit": return .exit(code: control.exitCode)
        case "closed": return .closed
        default: throw Malformed()
        }
    }
}

/// What the view does with one host message.
public enum TerminalFeed: Equatable, Sendable {
    case none
    case feed([UInt8])
    /// Clear the screen and scrollback first (the host replays its retained tail).
    case clear
}

/// The stream offset this view has drawn up to. Reconnects resume from here,
/// and overlapping output is trimmed, so a byte is never drawn twice.
public struct TerminalStreamCursor: Equatable, Sendable {
    public private(set) var held: UInt64?
    public init(held: UInt64? = nil) { self.held = held }

    public mutating func accept(_ message: TerminalServerMessage) -> TerminalFeed {
        switch message {
        case let .ready(sequence, reset, _, _, _, _):
            if reset || held != sequence {
                held = sequence
                return .clear
            }
            return .none
        case let .reset(sequence):
            held = sequence
            return .clear
        case let .output(sequence, bytes):
            // The offset comes off the socket: one that cannot end inside UInt64 is dropped, not trapped on.
            let (end, overflow) = sequence.addingReportingOverflow(UInt64(bytes.count))
            if overflow { return .none }
            guard let current = held else {
                held = end
                return .feed(bytes)
            }
            if end <= current { return .none }
            // The host never skips without a reset; a gap means this view
            // missed bytes, so it shows what follows rather than stalling.
            let skip = sequence >= current ? 0 : Int(current - sequence)
            held = end
            return .feed(Array(bytes[skip...]))
        case .exit, .closed:
            return .none
        }
    }
}

/// Keys from the extra-keys bar. `applicationCursor` follows the running
/// program (vim and htop switch arrows to `ESC O x`).
public enum TerminalKey: String, CaseIterable, Sendable {
    case escape, tab, up, down, left, right

    public func bytes(applicationCursor: Bool) -> [UInt8] {
        let arrow: (UInt8) -> [UInt8] = { [0x1b, applicationCursor ? 0x4f : 0x5b, $0] }
        switch self {
        case .escape: return [0x1b]
        case .tab: return [0x09]
        case .up: return arrow(0x41)
        case .down: return arrow(0x42)
        case .right: return arrow(0x43)
        case .left: return arrow(0x44)
        }
    }

    /// The control byte for Ctrl plus `character`, as a terminal sends it.
    public static func control(_ character: Character) -> [UInt8]? {
        guard let scalar = character.unicodeScalars.first, character.unicodeScalars.count == 1,
              scalar.isASCII else { return nil }
        let value = UInt8(scalar.value)
        switch value {
        case 0x61...0x7a: return [value - 0x60]            // a-z
        case 0x40...0x5f: return [value - 0x40]            // @ A-Z [ \ ] ^ _
        case 0x20: return [0x00]                            // Ctrl-Space
        case 0x3f: return [0x7f]                            // Ctrl-?
        default: return nil
        }
    }
}

/// Paste as the running program expects: bracketed when it enabled mode 2004
/// (so a shell does not run pasted lines), with line breaks sent as Return
/// otherwise. Escape bytes inside the text cannot end the bracket early.
public enum TerminalPaste {
    public static func bytes(_ text: String, bracketed: Bool) -> [UInt8] {
        let normalized = text.replacingOccurrences(of: "\r\n", with: "\n")
        if bracketed {
            let body = Array(normalized.utf8).filter { $0 != 0x1b }
            return Array("\u{1b}[200~".utf8) + body + Array("\u{1b}[201~".utf8)
        }
        return Array(normalized.replacingOccurrences(of: "\n", with: "\r").utf8)
    }
}
