import Foundation

/// Prepared with the read row, never by scanning patches during SwiftUI layout.
public struct FileChangeSummary: Equatable, Sendable {
    public let action: String
    public let target: String
    public let path: String?
    public let title: String
    public let fileCount: Int
    public let additions: Int?
    public let deletions: Int?
    public let accessibilityLabel: String

    public static func filename(_ path: String) -> String {
        guard !path.isEmpty, !path.hasPrefix("[") else { return "a file" }
        return path.split(separator: "/").last.map(String.init) ?? "a file"
    }

    public static func relativePath(_ path: String?) -> String? {
        guard let path, !path.isEmpty, !path.hasPrefix("/"), !path.hasPrefix("["), !path.contains("\0"),
              path.split(separator: "/", omittingEmptySubsequences: false).allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else { return nil }
        return path
    }

    /// A path the host named for a file, as opposed to its placeholder for one it
    /// would not reveal ("[path outside workspace]") or an empty field.
    static func isNamed(_ path: String?) -> Bool {
        guard let path, !path.isEmpty, !path.hasPrefix("[") else { return false }
        return true
    }

    /// The file a patch is for, read from its own header (`+++ b/path`, or `--- a/path`
    /// for a deletion, or `diff --git a/x b/path`). The host leaves a change's `path`
    /// field out when it cannot name the file; its patch still says which file it was.
    static func patchPath(_ patch: String?) -> String? {
        guard let patch else { return nil }
        func clean(_ value: Substring, prefix: String) -> String? {
            var name = String(value.split(separator: "\t", maxSplits: 1, omittingEmptySubsequences: false).first ?? "")
            if name.hasPrefix("\"") && name.hasSuffix("\"") && name.count > 1 { name = String(name.dropFirst().dropLast()) }
            guard name != "/dev/null" else { return nil }
            if name.hasPrefix(prefix) { name.removeFirst(prefix.count) }
            return relativePath(name)
        }
        var old: String?, git: String?
        for line in patch.split(separator: "\n", maxSplits: 12, omittingEmptySubsequences: false).prefix(12) {
            if line.hasPrefix("+++ "), let name = clean(line.dropFirst(4), prefix: "b/") { return name }
            if line.hasPrefix("--- "), old == nil { old = clean(line.dropFirst(4), prefix: "a/") }
            if line.hasPrefix("diff --git "), git == nil, let split = line.range(of: " b/", options: .backwards) {
                git = relativePath(String(line[split.upperBound...]))
            }
            if line.hasPrefix("@@") { break }
        }
        return old ?? git
    }

    public static func prepare(_ item: ReadItem) -> Self? {
        guard item.type == "fileChange" else { return nil }
        let payload = item.payload ?? [:]
        let diffs = (payload["diffs"]?.array ?? []).compactMap { value -> [String: ThreadValue]? in
            if case .object(let fields) = value { return fields }; return nil
        }
        let paths = payload["paths"]?.array?.compactMap(\.string) ?? diffs.compactMap { $0["path"]?.string }
        let count = Set(paths).count
        let target = paths.contains(where: { $0.hasPrefix("[") }) ? "files" : count > 1 ? "\(count) files" : paths.first.map(filename) ?? "a file"
        let kinds = Set(diffs.compactMap { $0["kind"]?.string })
        let verb: String
        switch ActivityState.prepare(item) {
        case .completed: verb = kinds == ["add"] ? "Wrote" : kinds == ["delete"] ? "Deleted" : "Edited"
        case .started, .streaming, .waiting: verb = "Editing"
        case .failed: verb = "Couldn’t edit"
        case .interrupted: verb = "Stopped editing"
        default: verb = "Changes to"
        }
        func total(_ key: String, marker: Character) -> Int? {
            if let value = payload[key]?.number, value.isFinite, value >= 0, value <= Double(Int32.max) { return Int(value) }
            guard !diffs.isEmpty, diffs.allSatisfy({ $0[key]?.number != nil || $0["diff"]?.string != nil }) else { return nil }
            return diffs.reduce(0) { total, fields in
                if let value = fields[key]?.number, value.isFinite, value >= 0, value <= Double(Int32.max) { return total + Int(value) }
                return total + (fields["diff"]?.string?.split(separator: "\n").filter {
                    $0.first == marker && !$0.hasPrefix(String(repeating: String(marker), count: 3))
                }.count ?? 0)
            }
        }
        let added = total("additions", marker: "+"), removed = total("deletions", marker: "-")
        let title = "\(verb) \(target)"
        let counts = [added.map { "\($0) added lines" }, removed.map { "\($0) removed lines" }].compactMap { $0 }
        return Self(action: verb, target: target, path: count == 1 ? relativePath(paths.first) : nil, title: title, fileCount: max(count, 1), additions: added, deletions: removed,
                    accessibilityLabel: ([title] + counts).joined(separator: ", "))
    }
}

/// Saved edits from one response, distinct from the workspace's current Git status.
/// Patch strings stay inert and are assembled only when their file is opened.
public struct ResponseEditedFile: Identifiable, Sendable {
    public let path: String
    public var additions: Int?
    public var deletions: Int?
    public var patches: [String]
    /// The host caps each saved patch; its line counts still describe the full edit.
    public var partial = false
    public var id: String { path }
    public var name: String { FileChangeSummary.filename(path) }
}

public struct ResponseEditedFiles: Sendable {
    public let turnID: String
    public let files: [ResponseEditedFile]

    /// Added lines across the files; nil when any file's count is unknown.
    public var additions: Int? {
        files.allSatisfy { $0.additions != nil } ? files.reduce(0) { $0 + ($1.additions ?? 0) } : nil
    }
    public var deletions: Int? {
        files.allSatisfy { $0.deletions != nil } ? files.reduce(0) { $0 + ($1.deletions ?? 0) } : nil
    }
    /// The timeline row after the reply: "Changed 2 files".
    public var title: String { files.count == 1 ? "Changed 1 file" : "Changed \(files.count) files" }
    /// "+5 −1" ("+3" when nothing was removed), or nil when the counts are unknown.
    public var countsText: String? {
        let removed = deletions.flatMap { $0 > 0 || additions == nil ? "−\($0)" : nil }
        let parts = [additions.map { "+\($0)" }, removed].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " ")
    }
    /// "Changed 2 files · +5 −1", the row and the files list's header.
    public var summaryText: String { ([title] + [countsText].compactMap { $0 }).joined(separator: " · ") }
    public var accessibilityLabel: String {
        ([title] + [additions.map { "\($0) added lines" }, deletions.map { "\($0) removed lines" }].compactMap { $0 })
            .joined(separator: ", ")
    }

    /// Which visible node carries each reply's "Changed N files" row: the last
    /// node of the entry its footer belongs to or, when a detail level hides
    /// that entry, the last visible node before where it would be. Built from
    /// the footers the timeline prepared, without reading patches. Keyed by
    /// node ID; replies keep their order.
    public static func hosts(_ nodes: [ChatFeedNode], entries: [ChatFeedEntry], footers: [String: Self]) -> [String: [Self]] {
        guard !footers.isEmpty, !nodes.isEmpty else { return [:] }
        var position: [String: Int] = [:]
        for (index, entry) in entries.enumerated() where position[entry.id] == nil { position[entry.id] = index }
        let pending = footers.compactMap { entryID, edits in position[entryID].map { ($0, edits) } }.sorted { $0.0 < $1.0 }
        var result: [String: [Self]] = [:]
        var next = 0
        var host: String?
        for node in nodes {
            let at = position[node.entryID] ?? -1
            // Footers of entries before this node's go to the last node before it
            // (this one when nothing earlier shows).
            while next < pending.count, pending[next].0 < at {
                result[host ?? node.id, default: []].append(pending[next].1)
                next += 1
            }
            host = node.id
        }
        if let host { for item in pending[next...] { result[host, default: []].append(item.1) } }
        return result
    }

    /// Attach once, after the turn's last visible entry, even when commentary
    /// splits its activity into several groups. Failed/pending edits are not receipts.
    /// A turn's whole-turn diff, when the Mac saved one, replaces its edit rows:
    /// it also covers shell edits and sub-agent work the rows miss.
    public static func footers(entries: [ChatFeedEntry], activeTurnIDs: Set<String>, turns: [String: ReadTurn] = [:]) -> [String: Self] {
        var lastEntry: [String: String] = [:]
        var files: [String: [ResponseEditedFile]] = [:]
        var indices: [String: [String: Int]] = [:]
        for entry in entries {
            for row in entry.rows {
                guard !row.isUser, let turn = row.turnId, !activeTurnIDs.contains(turn) else { continue }
                lastEntry[turn] = entry.id
                guard turns[turn]?.editedFiles == nil,
                      let item = row.item, item.type == "fileChange", ActivityState.prepare(item) == .completed else { continue }
                let single = (row.fileChangeSummary?.additions, row.fileChangeSummary?.deletions)
                for file in editedFiles(in: item.payload ?? [:], singlePathCounts: single) {
                    merge(file, into: &files[turn, default: []], indices: &indices[turn, default: [:]])
                }
            }
        }
        for turn in lastEntry.keys {
            guard let payload = turns[turn]?.editedFiles else { continue }
            files[turn] = editedFiles(in: payload, singlePathCounts: nil)
        }
        return Dictionary(files.compactMap { turn, files in
            guard !files.isEmpty, let entry = lastEntry[turn] else { return nil }
            return (entry, Self(turnID: turn, files: files))
        }, uniquingKeysWith: { first, _ in first })
    }

    /// One file per path in a `fileChange`-shaped payload. A single-path edit
    /// takes the counts already prepared for its row.
    static func editedFiles(in payload: [String: ThreadValue], singlePathCounts: (Int?, Int?)?) -> [ResponseEditedFile] {
        let diffs = (payload["diffs"]?.array ?? []).compactMap { value -> [String: ThreadValue]? in
            if case .object(let fields) = value { return fields }; return nil
        }
        // A change is named by its own `path`, else by its patch header; the host's
        // placeholder for a path it would not reveal names nothing, and a change whose
        // event had no path at all would otherwise vanish from the list.
        func name(of fields: [String: ThreadValue]) -> String? {
            let own = fields["path"]?.string
            return FileChangeSummary.isNamed(own) ? own : FileChangeSummary.patchPath(fields["diff"]?.string)
        }
        var paths: [String] = []
        let listed = payload["paths"]?.array?.compactMap(\.string) ?? []
        // The timeline row counts a withheld path as one file, so a paths-only payload keeps a row for it;
        // when a change's own patch accounts for it, that change's row stands in.
        let withheld = listed.contains { !FileChangeSummary.isNamed($0) && !$0.isEmpty }
            && !diffs.contains { $0["path"]?.string.map { !FileChangeSummary.isNamed($0) } ?? false }
        for path in listed where FileChangeSummary.isNamed(path) && !paths.contains(path) { paths.append(path) }
        var byPath: [String: [[String: ThreadValue]]] = [:]
        var unnamed: [[String: ThreadValue]] = []
        for fields in diffs {
            guard let path = name(of: fields) else { unnamed.append(fields); continue }
            if !paths.contains(path) { paths.append(path) }
            byPath[path, default: []].append(fields)
        }
        let single = paths.count + (withheld ? 1 : 0) == 1 && unnamed.isEmpty ? singlePathCounts : nil
        func file(_ path: String, _ matching: [[String: ThreadValue]]) -> ResponseEditedFile {
            func count(_ key: String) -> Int? {
                if let single {
                    return key == "additions" ? single.0 : single.1
                }
                let values = matching.compactMap { $0[key]?.number }
                if !matching.isEmpty, values.count == matching.count,
                   values.allSatisfy({ $0.isFinite && $0 >= 0 && $0 <= Double(Int32.max) }) {
                    return values.reduce(0) { $0 + Int($1) }
                }
                guard !matching.isEmpty, matching.allSatisfy({ $0["diff"]?.string != nil }) else { return nil }
                let marker = key == "additions" ? "+" : "-"
                return matching.reduce(0) { total, fields in
                    total + (fields["diff"]?.string?.split(separator: "\n").filter {
                        $0.hasPrefix(marker) && !$0.hasPrefix(String(repeating: marker, count: 3))
                    }.count ?? 0)
                }
            }
            let patches = matching.compactMap { $0["diff"]?.string }.filter { !$0.isEmpty }
            let partial = matching.contains { fields in
                guard let text = fields["diff"]?.string else { return false }
                let lines = text.split(separator: "\n")
                func short(_ key: String, _ marker: String) -> Bool {
                    guard let expected = fields[key]?.number, expected.isFinite else { return false }
                    let shown = lines.filter { $0.hasPrefix(marker) && !$0.hasPrefix(String(repeating: marker, count: 3)) }.count
                    return Double(shown) < expected
                }
                return short("additions", "+") || short("deletions", "-")
            }
            return ResponseEditedFile(path: path, additions: count("additions"), deletions: count("deletions"), patches: patches, partial: partial)
        }
        var files: [ResponseEditedFile] = []
        var indices: [String: Int] = [:]
        for path in paths { merge(file(path, byPath[path] ?? []), into: &files, indices: &indices) }
        // Nothing names these changes, so say which one of the list each is.
        for fields in unnamed {
            merge(file("File \(files.count + 1)", [fields]), into: &files, indices: &indices)
        }
        if withheld { merge(file("File \(files.count + 1)", []), into: &files, indices: &indices) }
        return files
    }

    /// Later edits to a path add to its counts and patches.
    private static func merge(_ file: ResponseEditedFile, into files: inout [ResponseEditedFile], indices: inout [String: Int]) {
        guard let index = indices[file.path] else {
            indices[file.path] = files.count
            files.append(file)
            return
        }
        var merged = files[index]
        merged.additions = merged.additions.flatMap { prior in file.additions.map { prior + $0 } }
        merged.deletions = merged.deletions.flatMap { prior in file.deletions.map { prior + $0 } }
        merged.patches += file.patches
        merged.partial = merged.partial || file.partial
        files[index] = merged
    }

    /// The latest finished response that edited files (what Code changes calls
    /// "Last reply"), and whether that response is the conversation's last
    /// finished reply, which marks the Git pill with a dot.
    public static func latest(entries: [ChatFeedEntry], activeTurnIDs: Set<String>,
                              turns: [String: ReadTurn] = [:]) -> (edits: Self?, fromLastReply: Bool) {
        latest(footers: footers(entries: entries, activeTurnIDs: activeTurnIDs, turns: turns),
               entries: entries, activeTurnIDs: activeTurnIDs)
    }

    /// `latest` from footers the timeline already prepared.
    public static func latest(footers: [String: Self], entries: [ChatFeedEntry],
                              activeTurnIDs: Set<String>) -> (edits: Self?, fromLastReply: Bool) {
        guard let latest = entries.lazy.reversed().compactMap({ footers[$0.id] }).first else { return (nil, false) }
        let lastReply = entries.lazy.reversed().flatMap { $0.rows.reversed() }
            .first { !$0.isUser && $0.turnId.map { !activeTurnIDs.contains($0) } == true }?.turnId
        return (latest, lastReply == latest.turnID)
    }
}

/// One line of a unified diff, numbered for display. Hunk headers without
/// line numbers (as Claude's edits produce) leave the numbers unknown.
public struct DiffLine: Equatable, Sendable, Identifiable {
    public enum Kind: Sendable { case context, added, removed, hunk, note }
    public let id: Int
    public let kind: Kind
    public let oldNumber: Int?
    public let newNumber: Int?
    public let text: String

    public static func parse(_ diff: String, limit: Int = 20_000) -> [DiffLine] {
        var lines: [DiffLine] = []
        var old: Int?, new: Int?
        for raw in diff.split(separator: "\n", omittingEmptySubsequences: false).prefix(limit) {
            let line = String(raw)
            let id = lines.count
            if line.hasPrefix("@@") {
                let numbers = hunkStarts(line)
                old = numbers?.0; new = numbers?.1
                lines.append(DiffLine(id: id, kind: .hunk, oldNumber: nil, newNumber: nil, text: line))
            } else if line.hasPrefix("+++") || line.hasPrefix("---") || line.hasPrefix("diff ") || line.hasPrefix("index ") {
                lines.append(DiffLine(id: id, kind: .note, oldNumber: nil, newNumber: nil, text: line))
            } else if line.hasPrefix("Edit "), line.dropFirst(5).allSatisfy(\.isNumber) {
                old = nil; new = nil
                lines.append(DiffLine(id: id, kind: .note, oldNumber: nil, newNumber: nil, text: line))
            } else if line.hasPrefix("+") {
                lines.append(DiffLine(id: id, kind: .added, oldNumber: nil, newNumber: new, text: String(line.dropFirst())))
                new = new.map { $0 + 1 }
            } else if line.hasPrefix("-") {
                lines.append(DiffLine(id: id, kind: .removed, oldNumber: old, newNumber: nil, text: String(line.dropFirst())))
                old = old.map { $0 + 1 }
            } else if line.hasPrefix("\\") {
                lines.append(DiffLine(id: id, kind: .note, oldNumber: nil, newNumber: nil, text: line))
            } else {
                // A blank line at the very end is the patch's trailing newline.
                if line.isEmpty, raw.endIndex == diff.endIndex { continue }
                lines.append(DiffLine(id: id, kind: .context, oldNumber: old, newNumber: new,
                                      text: line.hasPrefix(" ") ? String(line.dropFirst()) : line))
                old = old.map { $0 + 1 }; new = new.map { $0 + 1 }
            }
        }
        return lines
    }

    private static func hunkStarts(_ header: String) -> (Int, Int)? {
        // @@ -12,3 +12,4 @@
        let parts = header.split(separator: " ")
        guard parts.count >= 3, parts[1].hasPrefix("-"), parts[2].hasPrefix("+"),
              let old = Int(parts[1].dropFirst().split(separator: ",")[0]),
              let new = Int(parts[2].dropFirst().split(separator: ",")[0]) else { return nil }
        return (old, new)
    }
}

/// Side-by-side rows: each removal pairs with the addition that replaced it.
public struct SplitDiffRow: Equatable, Sendable, Identifiable {
    public let id: Int
    public let left: DiffLine?
    public let right: DiffLine?

    public static func pair(_ lines: [DiffLine]) -> [SplitDiffRow] {
        var rows: [SplitDiffRow] = []
        var removed: [DiffLine] = [], added: [DiffLine] = []
        func flush() {
            for index in 0..<max(removed.count, added.count) {
                rows.append(SplitDiffRow(id: rows.count, left: index < removed.count ? removed[index] : nil,
                                         right: index < added.count ? added[index] : nil))
            }
            removed = []; added = []
        }
        for line in lines {
            switch line.kind {
            case .removed: if !added.isEmpty { flush() }; removed.append(line)
            case .added: added.append(line)
            default: flush(); rows.append(SplitDiffRow(id: rows.count, left: line, right: line))
            }
        }
        flush()
        return rows
    }
}
