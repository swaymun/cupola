import Foundation

/// Block-level Markdown for chat bubbles. Inline syntax (emphasis, code spans,
/// links) is left to each block's text; this only finds the block structure.
public enum MarkdownBlock: Equatable, Sendable {
    case paragraph(String)
    case heading(level: Int, text: String)
    case listItem(marker: String, depth: Int, text: String)
    case quote(String)
    case code(language: String?, text: String)
    case table(header: [String], rows: [[String]])
    case rule

    public static func parse(_ text: String) -> [MarkdownBlock] { parse(text, continuing: nil).blocks }

    /// A parse that a longer text with the same beginning continues, as a
    /// reply grows while it streams: the next parse re-reads only from the
    /// last blank line outside code, where no block is open.
    public struct Progress: Sendable {
        public let blocks: [MarkdownBlock]
        public let source: String
        /// UTF-8 offset of the line after that blank line, the blocks before
        /// it, and the last of them as it was there (a later indented line
        /// can still continue a list item).
        let resume: (offset: Int, count: Int, last: MarkdownBlock?)?
    }

    /// The same blocks as `parse(text)`, continuing `previous` when `text`
    /// begins with the part of its source before its resume point.
    public static func parse(_ text: String, continuing previous: Progress?) -> Progress {
        var blocks: [MarkdownBlock] = []
        var start = 0
        if let previous, let resume = previous.resume, text.utf8.count >= resume.offset,
           text.utf8.starts(with: previous.source.utf8.prefix(resume.offset)) {
            blocks = Array(previous.blocks.prefix(resume.count))
            if let last = resume.last, !blocks.isEmpty { blocks[blocks.count - 1] = last }
            start = resume.offset
        }
        var paragraph: [String] = [], quote: [String] = [], code: [String] = []
        var fence: (marker: Character, count: Int, language: String?, indent: Int)?
        let tail = start == 0 ? text : String(Substring(text.utf8[text.utf8.index(text.utf8.startIndex, offsetBy: start)...]))
        let lines = tail.components(separatedBy: "\n")
        var resume: (offset: Int, count: Int, last: MarkdownBlock?)?
        var lineStart = start, lineStarts: [Int] = []
        lineStarts.reserveCapacity(lines.count)
        for line in lines { lineStarts.append(lineStart); lineStart += line.utf8.count + 1 }
        func flushParagraph() {
            if !paragraph.isEmpty { blocks.append(.paragraph(paragraph.joined(separator: "\n"))); paragraph = [] }
        }
        func flushQuote() {
            if !quote.isEmpty { blocks.append(.quote(quote.joined(separator: "\n"))); quote = [] }
        }
        var index = 0
        while index < lines.count {
            let line = lines[index]
            index += 1
            let indent = line.prefix(while: { $0 == " " }).count
            let body = line.dropFirst(indent)
            // Fenced code keeps every line exactly.
            if let open = fence {
                let count = body.prefix(while: { $0 == open.marker }).count
                if indent <= open.indent + 3, count >= open.count, body.dropFirst(count).trimmingCharacters(in: .whitespaces).isEmpty {
                    blocks.append(.code(language: open.language, text: code.joined(separator: "\n"))); code = []; fence = nil
                } else { code.append(String(line.dropFirst(min(indent, open.indent)))) }
                continue
            }
            // A list's fenced example is indented with its content. It is still
            // code, not an inline triple-backtick paragraph. Outside a list,
            // four-space indentation is not a fence opener.
            let inList: Bool = if case .listItem? = blocks.last { paragraph.isEmpty } else { false }
            if indent <= 3 || inList, let marker = body.first, marker == "`" || marker == "~" {
                let count = body.prefix(while: { $0 == marker }).count
                let info = body.dropFirst(count).trimmingCharacters(in: .whitespaces)
                if count >= 3, marker != "`" || !info.contains("`") {
                    flushParagraph(); flushQuote()
                    fence = (marker, count, info.isEmpty ? nil : String(info.split(separator: " ")[0]), indent)
                    continue
                }
            }
            let trimmed = body.trimmingCharacters(in: .whitespaces)
            if trimmed.isEmpty {
                flushParagraph(); flushQuote()
                if index < lines.count { resume = (lineStarts[index], blocks.count, blocks.last) }
                continue
            }
            if indent <= 3, body.hasPrefix(">") {
                flushParagraph()
                quote.append(String(body.dropFirst().drop(while: { $0 == " " })))
                continue
            }
            flushQuote()
            if indent <= 3, let level = heading(body) {
                flushParagraph()
                blocks.append(.heading(level: level, text: String(body.dropFirst(level)).trimmingCharacters(in: .whitespaces)
                    .trimmingCharacters(in: CharacterSet(charactersIn: "#")).trimmingCharacters(in: .whitespaces)))
                continue
            }
            if indent <= 3, isRule(trimmed) {
                flushParagraph(); blocks.append(.rule); continue
            }
            if let (marker, text) = listItem(body) {
                flushParagraph()
                blocks.append(.listItem(marker: marker, depth: min(indent / 2, 6), text: text))
                continue
            }
            // A pipe table needs a header and a --- separator row.
            if trimmed.contains("|"), index < lines.count, isTableSeparator(lines[index], columns: cells(trimmed).count) {
                flushParagraph()
                let header = cells(trimmed)
                index += 1
                var rows: [[String]] = []
                while index < lines.count {
                    let row = lines[index].trimmingCharacters(in: .whitespaces)
                    guard !row.isEmpty, row.contains("|") else { break }
                    rows.append(normalized(cells(row), count: header.count)); index += 1
                }
                blocks.append(.table(header: header, rows: rows))
                continue
            }
            // A line under an open list item continues it.
            if indent >= 2, paragraph.isEmpty, case .listItem(let marker, let depth, let text)? = blocks.last {
                blocks[blocks.count - 1] = .listItem(marker: marker, depth: depth, text: text + "\n" + trimmed)
                continue
            }
            paragraph.append(line)
        }
        if fence != nil { blocks.append(.code(language: fence?.language, text: code.joined(separator: "\n"))) }
        flushParagraph(); flushQuote()
        return Progress(blocks: blocks, source: text, resume: resume ?? previousResume(previous, start: start))
    }
    /// A continued parse that met no new blank line keeps the point it resumed from.
    private static func previousResume(_ previous: Progress?, start: Int) -> (offset: Int, count: Int, last: MarkdownBlock?)? {
        guard start > 0, let resume = previous?.resume, resume.offset == start else { return nil }
        return resume
    }

    /// What the formatted message says, for VoiceOver and previews: block and
    /// inline syntax removed, list markers and table cells kept as words.
    public static func plainText(_ text: String) -> String {
        parse(text).compactMap(\.plainText).joined(separator: "\n")
    }

    /// What one block says (see `plainText(_:)`); nil for a rule.
    public var plainText: String? {
        func inline(_ source: String) -> String {
            guard let value = try? AttributedString(markdown: source,
                options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)) else { return source }
            return String(value.characters)
        }
        switch self {
        case .paragraph(let text), .quote(let text): return inline(text)
        case .heading(_, let text): return inline(text)
        case .listItem(let marker, _, let text): return (marker == "•" ? "" : marker + " ") + inline(text)
        case .code(_, let text): return text
        case .table(let header, let rows):
            return ([header] + rows).map { $0.map(inline).joined(separator: ", ") }.joined(separator: "\n")
        case .rule: return nil
        }
    }

    private static func heading(_ body: Substring) -> Int? {
        let level = body.prefix(while: { $0 == "#" }).count
        guard (1...6).contains(level) else { return nil }
        let rest = body.dropFirst(level)
        return rest.isEmpty || rest.first == " " ? level : nil
    }
    private static func isRule(_ line: String) -> Bool {
        let compact = line.filter { $0 != " " }
        guard compact.count >= 3, let first = compact.first, "-*_".contains(first) else { return false }
        return compact.allSatisfy { $0 == first }
    }
    private static func listItem(_ body: Substring) -> (String, String)? {
        if let first = body.first, "-*+".contains(first), body.dropFirst().first == " " {
            let text = String(body.dropFirst(2))
            if text.hasPrefix("[ ] ") { return ("☐", String(text.dropFirst(4))) }
            if text.lowercased().hasPrefix("[x] ") { return ("☑", String(text.dropFirst(4))) }
            return ("•", text)
        }
        let digits = body.prefix(while: \.isNumber)
        guard (1...9).contains(digits.count) else { return nil }
        let rest = body.dropFirst(digits.count)
        guard let delimiter = rest.first, delimiter == "." || delimiter == ")", rest.dropFirst().first == " " else { return nil }
        return (digits + ".", String(rest.dropFirst(2)))
    }
    private static func isTableSeparator(_ line: String, columns: Int) -> Bool {
        let parts = cells(line.trimmingCharacters(in: .whitespaces))
        return columns > 0 && parts.count == columns && parts.allSatisfy { cell in
            let dashes = cell.trimmingCharacters(in: CharacterSet(charactersIn: ":"))
            return !dashes.isEmpty && dashes.allSatisfy { $0 == "-" }
        }
    }
    private static func cells(_ row: String) -> [String] {
        var parts: [String] = [], cell = "", slashes = 0, endedWithDelimiter = false
        for character in row {
            if character == "|" {
                if slashes % 2 == 1 {
                    // GFM unescapes pipes at the table boundary, including in a
                    // code span. Inline Markdown then sees the original cell.
                    cell.removeLast(); cell.append(character); endedWithDelimiter = false
                } else {
                    parts.append(cell); cell = ""; endedWithDelimiter = true
                }
            } else {
                cell.append(character); endedWithDelimiter = false
            }
            slashes = character == "\\" ? slashes + 1 : 0
        }
        parts.append(cell)
        if row.hasPrefix("|") { parts.removeFirst() }
        if endedWithDelimiter { parts.removeLast() }
        return parts.map { $0.trimmingCharacters(in: .whitespaces) }
    }
    private static func normalized(_ cells: [String], count: Int) -> [String] {
        Array((cells + Array(repeating: "", count: max(0, count - cells.count))).prefix(max(count, 1)))
    }
}
