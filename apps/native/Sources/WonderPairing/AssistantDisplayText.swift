import Foundation

/// Presentation for legacy/cached assistant streams. Keep the snapshot's raw
/// text: deltas append to it, including controls split across events. Typed
/// host history already applies the same projection (history.rs); this fallback
/// is idempotent. Never call it on the owner's message (`OwnerDisplayText`) or a tool argument.
enum AssistantDisplayText {
    static func render(_ text: String) -> String {
        memoryAttribution(terminalText(text))
    }

    private static func terminalText(_ text: String) -> String {
        guard text.unicodeScalars.contains(where: { $0.value == 0x1b || $0.value == 7 || (0x90...0x9f).contains($0.value) }) else { return text }
        var scalars = text.unicodeScalars.makeIterator()
        var next = scalars.next(), output = String.UnicodeScalarView()
        func take() -> Unicode.Scalar? {
            let value = next; next = scalars.next(); return value
        }
        while let scalar = take() {
            let ch = scalar.value
            var control: UInt32?
            if ch == 0x1b {
                if let value = next?.value, [0x5b, 0x5d, 0x50, 0x58, 0x5e, 0x5f, 0x5c].contains(value) {
                    control = take()?.value
                }
            } else {
                switch ch {
                case 0x9b: control = 0x5b
                case 0x9d: control = 0x5d
                case 0x90, 0x98, 0x9e, 0x9f: control = 0x50
                case 0x9c: control = 0x5c
                default: break
                }
            }
            if control == 0x5b {
                while let value = take()?.value {
                    if (0x40...0x7e).contains(value) || value == 0x18 || value == 0x1a { break }
                }
                continue
            }
            if let control, [0x5d, 0x50, 0x58, 0x5e, 0x5f].contains(control) {
                while let value = take()?.value {
                    if [0x9c, 0x18, 0x1a].contains(value) || (control == 0x5d && value == 7) { break }
                    if value == 0x1b, next?.value == 0x5c { _ = take(); break }
                    // An unterminated control ends at its line, as in history.rs.
                    if value == 0x0a { output.append(Unicode.Scalar(0x0a)); break }
                }
                continue
            }
            if ch == 0x1b, control == nil {
                while let value = next?.value, (0x20...0x2f).contains(value) { _ = take() }
                if let value = next?.value, (0x30...0x7e).contains(value) { _ = take() }
                continue
            }
            if ch == 0x1b || control != nil || ch == 7 { continue }
            output.append(scalar)
        }
        return String(output)
    }

    private static func memoryAttribution(_ text: String) -> String {
        guard let boundary = text.range(of: "\n<oai-mem-citation>\n", options: .backwards) else { return text }
        let body = text[..<boundary.lowerBound]
        var fence: (Character, Int)?
        for line in body.split(separator: "\n", omittingEmptySubsequences: false) {
            let indent = line.prefix(while: { $0 == " " }).count
            let trimmed = line.dropFirst(indent)
            guard indent <= 3, let marker = trimmed.first, marker == "`" || marker == "~" else { continue }
            let count = trimmed.prefix(while: { $0 == marker }).count
            guard count >= 3 else { continue }
            let rest = trimmed.dropFirst(count)
            if let open = fence {
                if marker == open.0, count >= open.1, rest.trimmingCharacters(in: .whitespaces).isEmpty { fence = nil }
            } else if marker != "`" || !rest.contains("`") { fence = (marker, count) }
        }
        guard fence == nil else { return text }
        let rawTrailer = text[boundary.upperBound...]
        guard rawTrailer.hasPrefix("<citation_entries>\n") else { return text }
        let trailer = rawTrailer.trimmingCharacters(in: .whitespacesAndNewlines)
        let prefix = "<citation_entries>\n", suffix = "\n</rollout_ids>\n</oai-mem-citation>"
        guard trailer.hasPrefix(prefix), trailer.hasSuffix(suffix) else { return text }
        let content = trailer.dropFirst(prefix.count).dropLast(suffix.count)
        guard let middle = content.range(of: "\n</citation_entries>\n<rollout_ids>\n") else { return text }
        let ids = content[middle.upperBound...].split(separator: "\n", omittingEmptySubsequences: false)
        guard ids.allSatisfy({ $0.isEmpty && ids.count == 1 || UUID(uuidString: String($0)) != nil }) else { return text }
        var notes: [String] = []
        for entry in content[..<middle.lowerBound].split(separator: "\n", omittingEmptySubsequences: false) {
            guard let split = entry.range(of: "|note=["), entry.hasSuffix("]") else { return text }
            let source = entry[..<split.lowerBound]
            guard let colon = source.lastIndex(of: ":"), colon > source.startIndex else { return text }
            let lines = source[source.index(after: colon)...].split(separator: "-", omittingEmptySubsequences: false)
            guard lines.count == 2, lines.allSatisfy({ UInt32($0).map { $0 > 0 } == true }) else { return text }
            let note = entry[split.upperBound...].dropLast()
            guard !note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return text }
            notes.append("- " + note)
        }
        guard !notes.isEmpty else { return text }
        let bodyEnd = body.lastIndex(where: { !$0.isWhitespace }).map { body.index(after: $0) } ?? body.startIndex
        return String(body[..<bodyEnd]) + "\n\nMemory sources:\n" + notes.joined(separator: "\n")
    }
}
