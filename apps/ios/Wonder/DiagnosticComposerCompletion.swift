#if WONDER_DIAGNOSTICS
import Foundation
import WonderPairing

/// A Claude Project thread whose Mac serves a native command catalog and
/// Project file search, for the composer's inline `/` and `@` lists. No model
/// work: choosing only edits the draft.
extension DiagnosticSubagentFixture {
    static var composerCompletionFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-composer-completion") }

    /// The capability record a current Mac publishes for Claude.
    static func composerCompletionCapabilities() -> Any {
        var record = ProviderCapabilities.fixtureMac(projectFamily)
        record.slashCommands = true; record.fileMentions = true
        return (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(record))) ?? [:]
    }

    static var composerCodexFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-composer-codex") }
    static var composerCachedFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-composer-cached") }
    static var composerPreviewText: String? {
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-composer-preview-commands") { return "/" }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-composer-preview-mentions") { return "Compare @rel" }
        return nil
    }

    static var composerCommands: [String: Any] {
        var result: [String: Any] = [
        "availability": "ready", "revision": "fixture:1",
        "commands": [
            ["name": "review", "description": "Review a pull request", "argumentHint": "<pr>"],
            ["name": "compact", "description": "Clear history but keep a summary in context"],
            ["name": "init", "description": "Write a CLAUDE.md for this project"],
        ],
        ]
        if composerCachedFixture { result["availability"] = "cached"; result["fetchedAt"] = "2026-11-02T18:00:00Z" }
        var commands = result["commands"] as! [[String: Any]]
        if composerCodexFixture {
            commands.removeAll { $0["name"] as? String == "init" || (t3CodeRunningFixture && $0["name"] as? String == "review") }
        }
        commands += [["name": "cupola-summary", "description": "Cupola · Draft a request for a summary", "insertionText": "Summarize this conversation's decisions, open questions, and next steps."]]
        result["commands"] = commands
        return result
    }

    static func composerFileSearch(_ query: String) -> [String: Any] {
        let files = [("src/ComposerEditor.swift", false), ("src", true), ("docs/Release notes.md", false), ("README.md", false)]
        let needle = query.lowercased()
        let entries: [[String: Any]] = files
            .filter { file in
                var remaining = file.0.lowercased()[...]
                return needle.allSatisfy { character in
                    guard let found = remaining.firstIndex(of: character) else { return false }
                    remaining = remaining[remaining.index(after: found)...]; return true
                }
            }
            .map { path, isDirectory in
                let mention = composerCodexFixture ? (path.contains(" ") ? "\"\(path)\"" : path)
                    : path.contains(" ") ? "@\"\(path)\"" : "@" + path
                return ["rootId": "workspace", "rootLabel": "Workspace", "path": path,
                        "name": String(path.split(separator: "/").last ?? ""), "isDirectory": isDirectory, "mention": mention]
            }
        return ["entries": entries, "truncated": false]
    }
    static func composerThreadSearch(_ query: String) -> [String: Any] {
        let threads = [("claude:release-notes", "Release notes", "claude"), ("codex:release-plan", "Release planning", "codex")]
        let entries: [[String: Any]] = threads.filter { query.isEmpty || $0.1.lowercased().contains(query.lowercased()) }.map { id, title, family in
            ["conversationId": id, "title": title, "family": family,
             "mention": "[@\(title)](cupola://thread/\(id.replacingOccurrences(of: ":", with: "%3A")))"]
        }
        return ["entries": entries, "truncated": false]
    }
}
#endif
