import Foundation

/// A `!` shell command or slash command the owner ran in Claude Code, with its
/// output. Claude Code records these as tagged user entries (`<bash-input>`,
/// `<bash-stdout>`, `<command-name>`, `<local-command-stdout>`, ...); the host
/// joins a command and its output into one prompt. Prepared once per row so
/// tag scanning never runs from a SwiftUI body.
public struct ClaudeCommandBlock: Sendable, Equatable {
    public enum Kind: Sendable { case shell, slash }
    public let kind: Kind
    /// The shell command line, or the slash command with its leading `/`.
    public let command: String
    public let arguments: String
    public let output: String
    public let errorOutput: String
    /// Text from tags this version does not know, and any untagged text, kept readable.
    public let notes: [String]
    public let outputLineCount: Int
    public let errorLineCount: Int

    init(kind: Kind, command: String, arguments: String, output: String, errorOutput: String, notes: [String]) {
        self.kind = kind; self.command = command; self.arguments = arguments
        self.output = output; self.errorOutput = errorOutput; self.notes = notes
        func lines(_ text: String) -> Int { text.isEmpty ? 0 : text.utf8.reduce(1) { $1 == 10 ? $0 + 1 : $0 } }
        outputLineCount = lines(output); errorLineCount = lines(errorOutput)
    }

    /// The block as plain text, for Copy and Markdown export.
    public var plainText: String {
        let head = kind == .shell ? "$ " + command : [command, arguments].filter { !$0.isEmpty }.joined(separator: " ")
        return ([head] + [output, errorOutput] + notes).filter { !$0.isEmpty }.joined(separator: "\n\n")
    }

    private static let shellTags: Set = ["bash-input", "bash-stdout", "bash-stderr"]
    private static let slashTags: Set = ["command-name", "command-message", "command-args", "local-command-stdout", "local-command-stderr"]
    private static var tagBlock: Regex<(Substring, Substring, Substring)> { #/<([a-z][a-z0-9_-]{0,63})>([\s\S]*?)<\/\1>/# }
    private static var terminalEscape: Regex<Substring> { #/\x{1B}(?:\[[0-9;?]*[ -\/]*[@-~]|\][^\x{07}\x{1B}]*(?:\x{07}|\x{1B}\\))/# }

    public static func parse(_ text: String) -> Self? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        // Only a message that opens with a command tag is a command; prose that
        // mentions a tag stays prose.
        guard trimmed.utf8.count <= 524_288, trimmed.hasPrefix("<"),
              let first = trimmed.prefixMatch(of: tagBlock).map({ String($0.1) }),
              shellTags.contains(first) || slashTags.contains(first) else { return nil }
        var values: [String: [String]] = [:], notes: [String] = []
        var cursor = trimmed.startIndex
        func note(_ text: Substring) {
            let readable = text.trimmingCharacters(in: .whitespacesAndNewlines)
            if !readable.isEmpty { notes.append(readable) }
        }
        for match in trimmed.matches(of: tagBlock) {
            note(trimmed[cursor..<match.range.lowerBound])
            cursor = match.range.upperBound
            let name = String(match.1)
            if shellTags.contains(name) || slashTags.contains(name) {
                values[name, default: []].append(clean(match.2))
            } else {
                note(match.2)
            }
        }
        note(trimmed[cursor...])
        func value(_ name: String) -> String { (values[name] ?? []).filter { !$0.isEmpty }.joined(separator: "\n") }
        let kind: Kind = shellTags.contains(first) ? .shell : .slash
        let fields = kind == .shell ? ["bash-input", "bash-stdout", "bash-stderr"]
            : ["command-name", "command-args", "local-command-stdout", "local-command-stderr"]
        var command = value(fields[0]).trimmingCharacters(in: .whitespaces)
        let message = value("command-message").trimmingCharacters(in: .whitespaces)
        if kind == .slash {
            // The message repeats the name ("review is running…"); it names the command only when the name is missing.
            if command.isEmpty { command = message }
            if !command.isEmpty, !command.hasPrefix("/") { command = "/" + command }
        }
        // A known tag from the other kind of command is still the owner's text.
        let shown = Set(fields + ["command-message"])
        for name in (shellTags.union(slashTags)).subtracting(shown).sorted() where !value(name).isEmpty { notes.append(value(name)) }
        return Self(kind: kind, command: command,
                    arguments: kind == .slash ? value("command-args").trimmingCharacters(in: .whitespaces) : "",
                    output: value(fields[kind == .shell ? 1 : 2]), errorOutput: value(fields[kind == .shell ? 2 : 3]), notes: notes)
    }

    private static func clean(_ body: Substring) -> String {
        // Output keeps its indentation; only surrounding blank lines go.
        String(body).replacing(terminalEscape, with: "").trimmingCharacters(in: .newlines)
    }
}
