import Foundation

/// Syntax colouring for a source file opened from Files, chosen by its name.
///
/// Files use the line tokenizer (`SyntaxHighlighter`), not highlight.js: the
/// tokenizer can stop after any line and resume with its state, so the lines
/// on screen are coloured first and the rest follows in small chunks. Measured
/// on an M-series Mac (release, 2026-10-23): about 40 MB/s, 10 ms for a
/// 6,700-line Swift file; highlight.js took 64 ms for 1,000 lines of it with a
/// JIT, which iOS apps do not get, and only colours a whole text at once.
public enum SourceSyntax {
    /// Larger texts stay plain. File previews stop at 128 KB, well inside this.
    public static let maxBytes = 2_000_000

    /// The language key for a file name (its lowercased extension, or the name
    /// for Dockerfile, Makefile and the like), or nil when it stays plain.
    public static func language(fileName: String) -> String? {
        guard SyntaxHighlighter(path: fileName) != nil else { return nil }
        let name = (fileName as NSString).lastPathComponent.lowercased()
        let ext = (name as NSString).pathExtension
        return ext.isEmpty ? name : ext
    }
}

/// Coloured runs for one stretch of whole lines, in UTF-16 offsets of the text.
public struct SyntaxChunk: Sendable, Equatable {
    /// The lines covered, newlines included.
    public let range: NSRange
    public let runs: [SyntaxRun]
}

/// Tokenizes a text in chunks of whole lines. Block comments and multi-line
/// strings carry across chunk boundaries, so any chunking yields exactly the
/// runs of a single pass. A value: hand it to a background task, take it back
/// with the chunk. Pure and thread-safe; never call it on the main thread for
/// more than a few lines.
public struct SyntaxChunker: Sendable {
    private let lines: [Substring]
    private var highlighter: SyntaxHighlighter
    private var nextLine = 0
    private var offset = 0

    /// Nil when the file type is not one the tokenizer knows or the text is
    /// over `maxBytes`.
    public init?(text: String, fileName: String, maxBytes: Int = SourceSyntax.maxBytes) {
        guard text.utf8.count <= maxBytes, let highlighter = SyntaxHighlighter(path: fileName) else { return nil }
        self.init(text: text, highlighter: highlighter)
    }

    init(text: String, highlighter: SyntaxHighlighter) {
        // Split on the newline byte: "\r\n" is one Character, so a Character
        // split would leave Windows line endings joined.
        lines = text.utf8.split(separator: 0x0A, omittingEmptySubsequences: false).map { Substring($0) }
        self.highlighter = highlighter
    }

    public var isFinished: Bool { nextLine >= lines.count }
    /// The UTF-16 offset where the next chunk starts.
    public var position: Int { offset }

    /// Tokenizes whole lines from where the last chunk ended until at least
    /// `utf16Length` units are covered or the text ends.
    public mutating func next(utf16Length: Int) -> SyntaxChunk {
        let start = offset
        var runs: [SyntaxRun] = []
        while nextLine < lines.count, offset - start < max(1, utf16Length) {
            offset += highlighter.appendRuns(String(lines[nextLine]), at: offset, to: &runs)
            nextLine += 1
            if nextLine < lines.count { offset += 1 }
        }
        return SyntaxChunk(range: NSRange(location: start, length: offset - start), runs: runs)
    }

    /// Every run of the rest of the text in one pass.
    public mutating func remainingRuns() -> [SyntaxRun] {
        var runs: [SyntaxRun] = []
        while !isFinished { runs += next(utf16Length: 64 * 1024).runs }
        return runs
    }
}
