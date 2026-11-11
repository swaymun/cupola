import Foundation

/// File types that get a rendered view beside their source.
public enum TextViewerKind: String, Sendable, Equatable {
    case markdown, json, jsonl

    public static func detect(name: String, mimeType: String?) -> TextViewerKind? {
        switch (name as NSString).pathExtension.lowercased() {
        case "md", "markdown", "mdown", "mkd": return .markdown
        case "jsonl", "ndjson": return .jsonl
        case "json": return .json
        default: break
        }
        switch mimeType?.lowercased().split(separator: ";").first.map(String.init) {
        case "text/markdown", "text/x-markdown": return .markdown
        case "application/json": return .json
        case "application/x-ndjson", "application/jsonl", "application/x-jsonlines": return .jsonl
        default: return nil
        }
    }

    /// Both JSON kinds share one remembered choice.
    var preferenceKey: String { self == .markdown ? "wonder.viewer.markdown" : "wonder.viewer.json" }

    /// Segment titles: the rendered view first, the source second.
    public var titles: (rendered: String, source: String) {
        self == .markdown ? ("Preview", "Source") : ("Formatted", "Raw")
    }
}

public enum TextViewerMode: String, Sendable, Equatable {
    case rendered, source
}

/// The last choice per device and kind, defaulting to the rendered view.
public enum TextViewerPreference {
    public static func storageKey(for kind: TextViewerKind) -> String { kind.preferenceKey }

    public static func mode(for kind: TextViewerKind, defaults: UserDefaults = .standard) -> TextViewerMode {
        defaults.string(forKey: kind.preferenceKey).flatMap(TextViewerMode.init(rawValue:)) ?? .rendered
    }
    public static func remember(_ mode: TextViewerMode, for kind: TextViewerKind, defaults: UserDefaults = .standard) {
        defaults.set(mode.rawValue, forKey: kind.preferenceKey)
    }
}

/// Splits Markdown into blocks of roughly `targetBytes` at blank lines outside
/// code fences, so a long document renders lazily one chunk at a time and each
/// chunk is cheap to parse. Joining the chunks with "\n" restores the text.
public enum MarkdownChunker {
    public static func chunks(_ text: String, targetBytes: Int = 4_096) -> [String] {
        guard !text.isEmpty else { return [] }
        // "\r\n" is one Character, so normalise before splitting on lines.
        let text = text.contains("\r\n") ? text.replacingOccurrences(of: "\r\n", with: "\n") : text
        var result: [String] = []
        var current: [Substring] = []
        var size = 0
        var fence: (marker: Character, count: Int)?
        func flush() {
            guard !current.isEmpty else { return }
            result.append(current.joined(separator: "\n"))
            current = []; size = 0
        }
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let body = line.drop(while: { $0 == " " })
            if let marker = body.first, marker == "`" || marker == "~" {
                let count = body.prefix(while: { $0 == marker }).count
                if let open = fence {
                    if marker == open.marker, count >= open.count,
                       body.dropFirst(count).allSatisfy({ $0 == " " }) { fence = nil }
                } else if count >= 3 { fence = (marker, count) }
            }
            current.append(line)
            size += line.utf8.count + 1
            if fence == nil, size >= targetBytes, body.allSatisfy({ $0 == " " || $0 == "\r" }) { flush() }
        }
        flush()
        return result
    }

    /// How many leading chunks fit within `limitBytes`; always at least one.
    public static func visibleCount(of chunks: [String], limitBytes: Int) -> Int {
        var total = 0
        for (index, chunk) in chunks.enumerated() {
            total += chunk.utf8.count
            if total >= limitBytes { return index + 1 }
        }
        return chunks.count
    }
}

/// A coloured stretch of a text view, in UTF-16 offsets so it applies directly
/// to an `NSTextStorage`.
public struct SyntaxRun: Equatable, Sendable {
    public let range: NSRange
    public let role: SyntaxRole
}
