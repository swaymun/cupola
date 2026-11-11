import Foundation

/// What the owner wrote in a user message, without what T3 Code, Claude Code
/// or Codex wrapped around it for the agent. Typed host history already
/// applies this (`injected_context::owner_text` in crates/wonderd); this is the
/// same rule for legacy/cached rows and is idempotent. Display only: the stored
/// text, and what is sent to the provider, stays as written.
enum OwnerDisplayText {
    /// Codex writes these as a user message of their own; a message made of
    /// nothing else is the harness's and shows nothing.
    private static let agentBlocks = ["environment_context", "user_instructions"]
    /// Older T3 Code context blocks, appended at the end of a prompt.
    private static let legacyT3Blocks = ["terminal_context", "element_context", "preview_annotation"]
    private static let t3Envelope = "t3_context"

    private static let newline = UInt8(ascii: "\n")

    static func render(_ text: String) -> String {
        guard text.contains("<") || text.contains("; ref=") else { return text }
        var changed = false
        var bytes = Array(text.utf8)

        let ids = takeEnvelope(&bytes, &changed)
        if !ids.isEmpty {
            bytes = Array(unwrapMarkers(String(decoding: bytes, as: UTF8.self), ids: ids, changed: &changed).utf8)
        }
        takeLegacyBlocks(&bytes, &changed)
        // Codex's briefing: a heading line, then the instructions in a block.
        if bytes.starts(with: Array("# AGENTS.md instructions for".utf8)), find(bytes, "<INSTRUCTIONS>") != nil {
            bytes.removeSubrange(..<(bytes.firstIndex(of: newline) ?? bytes.count))
            removeBlocks(&bytes, tag: "INSTRUCTIONS", changed: &changed)
            changed = true
        }
        // Tags the owner pasted among their words stay; these go only when the
        // message is nothing else.
        var probe = bytes, removed = false
        for tag in agentBlocks { removeBlocks(&probe, tag: tag, changed: &removed) }
        if removed, trimmed(probe).isEmpty { return "" }
        if isImageTag(bytes) { return "" }
        let result = String(decoding: bytes, as: UTF8.self)
        return changed ? result.trimmingCharacters(in: .whitespacesAndNewlines) : result
    }

    // MARK: T3 Code

    /// `<t3_context version="1">` … `</t3_context>` at the very end of the text;
    /// returns the ids of its entries. A block the text ends inside of goes too.
    /// A tag quoted in a code span or mid-message is not at the end and stays.
    private static func takeEnvelope(_ bytes: inout [UInt8], _ changed: inout Bool) -> [String] {
        let open = Array("<\(t3Envelope) version=\"1\">\n".utf8), close = Array("</\(t3Envelope)>".utf8)
        let end = trimmedEnd(bytes)
        guard let start = rfind(bytes[..<end], open) else { return [] }
        if start > 0, bytes[start - 1] != newline { return [] }
        var body = bytes[(start + open.count)..<end]
        if body.suffix(close.count).elementsEqual(close) {
            body = body.dropLast(close.count)
        } else if find(Array(body), close) != nil {
            return []  // closed earlier: the owner's words follow it
        }
        // Only entries T3 Code writes: `<context kind=… id=…`.
        guard body.drop { isSpace($0) }.starts(with: Array("<context ".utf8)) else { return [] }
        let ids = entryIds(String(decoding: body, as: UTF8.self))
        bytes.removeSubrange(start...)
        changed = true
        return ids
    }

    private static func entryIds(_ body: String) -> [String] {
        guard let pattern = try? NSRegularExpression(pattern: #"^<context kind="[^"]*" id="([^"]*)""#, options: .anchorsMatchLines) else { return [] }
        let range = NSRange(body.startIndex..., in: body)
        return pattern.matches(in: body, range: range).compactMap { match in
            Range(match.range(at: 1), in: body).map { String(body[$0]) }
        }
    }

    /// `[Kind: label; ref=id]` for an id in the envelope becomes what the owner
    /// attached: `Thread: Title`, `@path` for a file mention, `$name` for a skill.
    private static func unwrapMarkers(_ text: String, ids: [String], changed: inout Bool) -> String {
        guard let pattern = try? NSRegularExpression(pattern: #"\[([A-Z][A-Za-z ]{0,38}): ([^\]\n]*?); ref=([A-Za-z0-9_-]{1,128})\]"#) else { return text }
        var result = text
        let matches = pattern.matches(in: text, range: NSRange(text.startIndex..., in: text))
        for match in matches.reversed() {
            guard let whole = Range(match.range, in: result), let kindRange = Range(match.range(at: 1), in: result),
                  let labelRange = Range(match.range(at: 2), in: result), let idRange = Range(match.range(at: 3), in: result),
                  ids.contains(String(result[idRange])) else { continue }
            let kind = String(result[kindRange])
            let label = result[labelRange].trimmingCharacters(in: .whitespacesAndNewlines)
            let shown: String
            switch kind {
            case "Mention": shown = "@" + label.drop { $0 == "@" }
            case "Skill": shown = "$" + label.drop { $0 == "$" }
            default: shown = "\(kind): \(label)"
            }
            result.replaceSubrange(whole, with: shown)
            changed = true
        }
        return result
    }

    private static func takeLegacyBlocks(_ bytes: inout [UInt8], _ changed: inout Bool) {
        while true {
            let end = trimmedEnd(bytes)
            guard let tag = legacyT3Blocks.first(where: { bytes[..<end].suffix($0.utf8.count + 4).elementsEqual(Array("\n</\($0)>".utf8)) }),
                  let start = rfind(bytes[..<end], Array("<\(tag)>\n".utf8)),
                  start == 0 || bytes[start - 1] == newline else { return }
            bytes.removeSubrange(start...)
            changed = true
        }
    }

    // MARK: Blocks outside code

    /// Removes every `<tag …>…</tag>` outside a code span. An opening with no
    /// closing removes through the end (a preview cut it short).
    private static func removeBlocks(_ bytes: inout [UInt8], tag: String, changed: inout Bool) {
        let open = Array("<\(tag)".utf8), close = Array("</\(tag)>".utf8)
        var from = 0
        while let found = find(bytes, open, from: from) {
            let after = found + open.count < bytes.count ? bytes[found + open.count] : nil
            let atLineStart = found == 0 || bytes[found - 1] == newline
            if ![UInt8(ascii: ">"), UInt8(ascii: " "), newline].contains(after ?? 0)
                || !(atLineStart || startsAfterBlock(bytes, found)) || inCode(bytes, found) {
                from = found + open.count
                continue
            }
            let end = find(bytes, close, from: found).map { $0 + close.count } ?? bytes.count
            bytes.removeSubrange(found..<end)
            changed = true
            from = found
        }
    }

    /// Reminders follow each other or follow the owner's sentence on the same line.
    private static func startsAfterBlock(_ bytes: [UInt8], _ start: Int) -> Bool {
        bytes[..<trimmedEnd(Array(bytes[..<start]))].last == UInt8(ascii: ">") && !inCode(bytes, start)
    }

    /// Codex wraps an attached image's part in `<image name=[Image #1]>` and
    /// `</image>` parts of their own.
    private static func isImageTag(_ bytes: [UInt8]) -> Bool {
        let text = trimmed(bytes)
        if text == Array("</image>".utf8) { return true }
        return text.starts(with: Array("<image".utf8)) && text.last == UInt8(ascii: ">") && !text.dropFirst().contains(UInt8(ascii: "<"))
    }

    private static func inCode(_ bytes: [UInt8], _ at: Int) -> Bool {
        codeRanges(bytes).contains { $0.contains(at) }
    }

    /// Fenced blocks and inline spans, as Markdown reads them.
    private static func codeRanges(_ bytes: [UInt8]) -> [Range<Int>] {
        let tick = UInt8(ascii: "`"), space = UInt8(ascii: " ")
        var ranges: [Range<Int>] = []
        var lineStart = 0, inlineFrom = 0
        var fence: (start: Int, ticks: Int)?
        while lineStart <= bytes.count {
            let lineEnd = bytes[lineStart...].firstIndex(of: newline) ?? bytes.count
            let line = bytes[lineStart..<lineEnd]
            let indent = line.prefix { $0 == space }.count
            let ticks = line.dropFirst(indent).prefix { $0 == tick }.count
            if indent <= 3, ticks >= 3 {
                if let open = fence {
                    if ticks >= open.ticks, trimmed(Array(line.dropFirst(indent + ticks))).isEmpty {
                        ranges.append(open.start..<lineEnd)
                        fence = nil
                        inlineFrom = lineEnd
                    }
                } else {
                    fence = (lineStart, ticks)
                }
            } else if fence == nil {
                // Inline spans on this line: a run of n ticks to the next run of n.
                var i = max(lineStart, inlineFrom)
                while i < lineEnd {
                    guard bytes[i] == tick else { i += 1; continue }
                    let run = bytes[i..<lineEnd].prefix { $0 == tick }.count
                    var j = i + run, closed: Int?
                    while j < lineEnd {
                        if bytes[j] == tick {
                            let next = bytes[j..<lineEnd].prefix { $0 == tick }.count
                            if next == run { closed = j + next; break }
                            j += next
                        } else {
                            j += 1
                        }
                    }
                    if let closed {
                        ranges.append(i..<closed)
                        i = closed
                    } else {
                        i += run
                    }
                }
            }
            if lineEnd >= bytes.count { break }
            lineStart = lineEnd + 1
        }
        if let fence { ranges.append(fence.start..<bytes.count) }
        return ranges
    }

    // MARK: Bytes

    private static func isSpace(_ byte: UInt8) -> Bool { [32, 9, 10, 11, 12, 13].contains(byte) }

    private static func trimmedEnd(_ bytes: [UInt8]) -> Int {
        var end = bytes.count
        while end > 0, isSpace(bytes[end - 1]) { end -= 1 }
        return end
    }

    private static func trimmed(_ bytes: [UInt8]) -> [UInt8] {
        Array(bytes[..<trimmedEnd(bytes)].drop { isSpace($0) })
    }

    private static func find(_ bytes: [UInt8], _ needle: String) -> Int? { find(bytes, Array(needle.utf8)) }

    private static func find(_ bytes: [UInt8], _ needle: [UInt8], from: Int = 0) -> Int? {
        guard !needle.isEmpty, bytes.count - from >= needle.count else { return nil }
        for start in from...(bytes.count - needle.count) where bytes[start] == needle[0] && bytes[start..<(start + needle.count)].elementsEqual(needle) {
            return start
        }
        return nil
    }

    private static func rfind(_ bytes: ArraySlice<UInt8>, _ needle: [UInt8]) -> Int? {
        guard bytes.count >= needle.count else { return nil }
        for start in stride(from: bytes.endIndex - needle.count, through: bytes.startIndex, by: -1)
        where bytes[start..<(start + needle.count)].elementsEqual(needle) {
            return start
        }
        return nil
    }
}
