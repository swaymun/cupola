import Foundation

/// Where a JSON document stopped being valid, in the reader's terms.
public struct JSONFormatError: Error, Equatable, Sendable {
    public let line: Int
    public let column: Int
    public let message: String
    public var summary: String { "Line \(line), column \(column): \(message)" }
}

/// Pretty-prints JSON without parsing it into a dictionary, so key order,
/// duplicate keys, number text (big integers, exponents, trailing zeros) and
/// escape sequences come out exactly as written. It validates as it goes and is
/// iterative, so deeply nested input fails with a message instead of overflowing
/// the stack. Pure and thread-safe: call it off the main thread.
public enum JSONFormatter {
    public static let defaultMaxBytes = 2 * 1024 * 1024
    public static let maxDepth = 256

    public enum Outcome: Equatable, Sendable {
        case formatted(String)
        case invalid(JSONFormatError)
        case tooLarge
    }

    public static func format(_ text: String, indent: Int = 2, maxBytes: Int = defaultMaxBytes) -> Outcome {
        guard text.utf8.count <= maxBytes else { return .tooLarge }
        var text = text
        return text.withUTF8 { run($0, indent: indent) }
    }

    /// Syntax-coloured lines of already formatted JSON, or nil when the text is
    /// larger than `maxBytes` (it then stays plain).
    public static func highlightedLines(_ text: String, maxBytes: Int = 400_000) -> [[SyntaxSpan]]? {
        guard text.utf8.count <= maxBytes, var highlighter = SyntaxHighlighter(language: "json") else { return nil }
        highlighter.emitsPunctuation = true
        return text.split(separator: "\n", omittingEmptySubsequences: false).map { highlighter.spans(String($0)) }
    }

    private enum Expect { case value, key, colon, afterValue, end }

    private static func run(_ b: UnsafeBufferPointer<UInt8>, indent: Int) -> Outcome {
        let n = b.count
        var out = [UInt8]()
        out.reserveCapacity(n + n / 2)
        var stack: [UInt8] = []
        var expect = Expect.value
        var i = 0
        if n >= 3, b[0] == 0xEF, b[1] == 0xBB, b[2] == 0xBF { i = 3 }

        func fail(_ message: String, at offset: Int) -> Outcome {
            var line = 1, lineStart = 0
            for k in 0..<min(offset, n) where b[k] == 0x0A { line += 1; lineStart = k + 1 }
            var column = 1
            for k in lineStart..<min(offset, n) where b[k] & 0xC0 != 0x80 { column += 1 }
            return .invalid(JSONFormatError(line: line, column: column, message: message))
        }
        func isSpace(_ c: UInt8) -> Bool { c == 0x20 || c == 0x09 || c == 0x0A || c == 0x0D }
        func newline() {
            out.append(0x0A)
            out.append(contentsOf: repeatElement(0x20, count: stack.count * indent))
        }
        func isDigit(_ c: UInt8) -> Bool { c >= 0x30 && c <= 0x39 }
        func isHex(_ c: UInt8) -> Bool { isDigit(c) || (c >= 0x41 && c <= 0x46) || (c >= 0x61 && c <= 0x66) }

        /// Copies the string starting at `i`; returns an error outcome when invalid.
        func copyString() -> Outcome? {
            var j = i + 1
            while true {
                guard j < n else { return fail("this text never closes", at: i) }
                let c = b[j]
                if c == 0x22 { break }
                if c < 0x20 { return fail("a line break or control character inside text", at: j) }
                if c == 0x5C {
                    guard j + 1 < n else { return fail("this text never closes", at: i) }
                    switch b[j + 1] {
                    case 0x22, 0x5C, 0x2F, 0x62, 0x66, 0x6E, 0x72, 0x74: j += 2
                    case 0x75:
                        guard j + 5 < n, isHex(b[j + 2]), isHex(b[j + 3]), isHex(b[j + 4]), isHex(b[j + 5]) else {
                            return fail("an incomplete \\u escape", at: j)
                        }
                        j += 6
                    default: return fail("an unknown escape", at: j)
                    }
                } else { j += 1 }
            }
            out.append(contentsOf: UnsafeBufferPointer(rebasing: b[i...j]))
            i = j + 1
            return nil
        }
        func copyNumber() -> Outcome? {
            var j = i
            if b[j] == 0x2D { j += 1 }
            guard j < n, isDigit(b[j]) else { return fail("this isn't a valid number", at: i) }
            if b[j] == 0x30 { j += 1 } else { while j < n, isDigit(b[j]) { j += 1 } }
            if j < n, b[j] == 0x2E {
                j += 1
                guard j < n, isDigit(b[j]) else { return fail("digits are missing after the decimal point", at: j) }
                while j < n, isDigit(b[j]) { j += 1 }
            }
            if j < n, b[j] == 0x65 || b[j] == 0x45 {
                j += 1
                if j < n, b[j] == 0x2B || b[j] == 0x2D { j += 1 }
                guard j < n, isDigit(b[j]) else { return fail("digits are missing in the exponent", at: j) }
                while j < n, isDigit(b[j]) { j += 1 }
            }
            out.append(contentsOf: UnsafeBufferPointer(rebasing: b[i..<j]))
            i = j
            return nil
        }
        func copyLiteral(_ word: String) -> Outcome? {
            let bytes = Array(word.utf8)
            let matches = i + bytes.count <= n && (0..<bytes.count).allSatisfy { b[i + $0] == bytes[$0] }
            guard matches else { return fail("expected a value", at: i) }
            out.append(contentsOf: bytes)
            i += bytes.count
            return nil
        }

        while true {
            while i < n, isSpace(b[i]) { i += 1 }
            if i >= n {
                if expect == .end { break }
                return fail("the file ends before the JSON is complete", at: n)
            }
            let c = b[i]
            switch expect {
            case .end:
                return fail("there is extra text after the JSON value", at: i)
            case .value:
                switch c {
                case 0x7B, 0x5B:
                    guard stack.count < maxDepth else { return fail("nested more than \(maxDepth) levels deep", at: i) }
                    let close: UInt8 = c == 0x7B ? 0x7D : 0x5D
                    var j = i + 1
                    while j < n, isSpace(b[j]) { j += 1 }
                    if j < n, b[j] == close {
                        out.append(c); out.append(close); i = j + 1
                        expect = stack.isEmpty ? .end : .afterValue
                    } else {
                        out.append(c); stack.append(c); i += 1; newline()
                        expect = c == 0x7B ? .key : .value
                    }
                case 0x22:
                    if let error = copyString() { return error }
                    expect = stack.isEmpty ? .end : .afterValue
                case 0x2D, 0x30...0x39:
                    if let error = copyNumber() { return error }
                    expect = stack.isEmpty ? .end : .afterValue
                case 0x74, 0x66, 0x6E:
                    if let error = copyLiteral(c == 0x74 ? "true" : c == 0x66 ? "false" : "null") { return error }
                    expect = stack.isEmpty ? .end : .afterValue
                default:
                    return fail("expected a value", at: i)
                }
            case .key:
                guard c == 0x22 else { return fail("expected a quoted name", at: i) }
                if let error = copyString() { return error }
                expect = .colon
            case .colon:
                guard c == 0x3A else { return fail("expected a colon", at: i) }
                out.append(0x3A); out.append(0x20); i += 1
                expect = .value
            case .afterValue:
                let isObject = stack.last == 0x7B
                if c == 0x2C {
                    out.append(0x2C); i += 1; newline()
                    expect = isObject ? .key : .value
                } else if c == (isObject ? 0x7D : 0x5D) {
                    stack.removeLast(); newline(); out.append(c); i += 1
                    expect = stack.isEmpty ? .end : .afterValue
                } else {
                    return fail("expected a comma or the end of the \(isObject ? "object" : "list")", at: i)
                }
            }
        }
        return .formatted(String(decoding: out, as: UTF8.self))
    }
}

/// One line of a JSON Lines file.
public struct JSONLRecord: Equatable, Sendable, Identifiable {
    /// 1-based line number in the file.
    public let line: Int
    /// The line as written; clipped to `JSONLDocument.rawDisplayLimit` characters.
    public let raw: String
    /// Pretty-printed JSON; nil for invalid or oversize lines.
    public let formatted: String?
    public let error: JSONFormatError?
    public let isOversize: Bool
    public var id: Int { line }
}

public struct JSONLDocument: Equatable, Sendable {
    public static let defaultMaxRecords = 5_000
    public static let defaultMaxRecordBytes = 16 * 1024
    public static let rawDisplayLimit = 4_000

    public let records: [JSONLRecord]
    /// Non-blank lines after the record cap that were not formatted.
    public let omittedLines: Int
    /// The last line was dropped because the file was cut mid-line.
    public let droppedPartialLine: Bool

    public init(parsing text: String, maxRecords: Int = defaultMaxRecords,
                maxRecordBytes: Int = defaultMaxRecordBytes, isTruncated: Bool = false) {
        var segments = text.utf8.split(separator: 0x0A, omittingEmptySubsequences: false)
        var dropped = false
        if isTruncated, let last = segments.last, !last.isEmpty { segments.removeLast(); dropped = true }
        var records: [JSONLRecord] = []
        var omitted = 0
        for (index, segment) in segments.enumerated() {
            var line = String(decoding: segment, as: UTF8.self)
            if line.hasSuffix("\r") || line.hasSuffix("\r\n") { line = String(line.dropLast()) }
            if line.allSatisfy({ $0.isWhitespace }) { continue }
            guard records.count < maxRecords else { omitted += 1; continue }
            let raw = line.count > Self.rawDisplayLimit ? String(line.prefix(Self.rawDisplayLimit)) + "…" : line
            if line.utf8.count > maxRecordBytes {
                records.append(JSONLRecord(line: index + 1, raw: raw, formatted: nil, error: nil, isOversize: true))
                continue
            }
            switch JSONFormatter.format(line, maxBytes: maxRecordBytes) {
            case .formatted(let pretty):
                records.append(JSONLRecord(line: index + 1, raw: raw, formatted: pretty, error: nil, isOversize: false))
            case .invalid(let error):
                records.append(JSONLRecord(line: index + 1, raw: raw, formatted: nil, error: error, isOversize: false))
            case .tooLarge:
                records.append(JSONLRecord(line: index + 1, raw: raw, formatted: nil, error: nil, isOversize: true))
            }
        }
        self.records = records
        omittedLines = omitted
        droppedPartialLine = dropped
    }

    /// What Copy puts on the pasteboard: each record as shown, one blank line apart.
    public var shownText: String {
        records.map { $0.formatted ?? $0.raw }.joined(separator: "\n\n")
    }
}
