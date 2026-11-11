import Foundation

/// Names the operation being requested, without inferring what it changed.
/// Claude wraps MCP names in its own server identity; the embedded namespace
/// identifies the actual tool server. Shared by approval and history headings.
public enum ToolDisplayName {
    public static func title(server: String? = nil, tool: String?) -> String? {
        guard let tool, !tool.isEmpty else { return nil }
        var server = server
        var operation = tool
        if tool.hasPrefix("mcp__") {
            let parts = tool.components(separatedBy: "__")
            guard parts.count >= 3 else { return nil }
            server = parts[1]
            operation = parts.dropFirst(2).joined(separator: "_")
        }
        guard let operationWords = words(operation) else { return nil }
        let name = operationWords.lowercased()
        let source = server?.replacingOccurrences(of: "claude_ai_", with: "", options: .anchored)
        let displayServer = source.flatMap { words($0) }.map { value in
            if source?.lowercased() == "github" { return "GitHub" }
            return value.split(separator: " ").map { word in
                if word.lowercased() == "github" { return "GitHub" }
                return word == word.uppercased() ? String(word) : word.capitalized
            }.joined(separator: " ")
        }
        return (displayServer ?? "Tool") + ": " + name
    }

    private static func words(_ raw: String) -> String? {
        let characters = Array(raw.prefix(256))
        var result = ""
        for index in characters.indices {
            let character = characters[index]
            if character == "_" || character == "-" || character == "." || character.isWhitespace {
                result.append(" ")
            } else if character.isLetter || character.isNumber {
                if character.isUppercase, index > 0,
                   characters[index - 1].isLowercase || (characters[index - 1].isUppercase && index + 1 < characters.count && characters[index + 1].isLowercase) {
                    result.append(" ")
                }
                result.append(character)
            } else { return nil }
        }
        let value = result.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return value.isEmpty ? nil : value
    }
}

extension ReadRow {
    /// A finished turn reads like Codex and T3 Code: the agent's last text
    /// after each of the owner's messages is the reply; every earlier text in
    /// that turn is work, shown inside its Working group. Providers that mark
    /// their notes as commentary (Codex) and those that cannot know a note is
    /// not the answer until the turn ends (Claude, ACP) look the same.
    ///
    /// - A running turn keeps the provider's phases, so live notes show as
    ///   they arrive and fold when the turn ends, under the same row IDs.
    /// - A completed turn's last text is the reply even when the provider
    ///   called it commentary (the agent ran a tool after it), so it never
    ///   ends without an answer.
    /// - An interrupted or failed turn folds its earlier texts; its last one
    ///   stays commentary if the provider said so: that turn has no answer.
    ///
    /// Rows keep their order and IDs. Prepared once per timeline revision.
    public static func settlingFinishedTurns(_ rows: [ReadRow], turns: [String: ReadTurn],
                                             activeTurnIDs: Set<String> = []) -> [ReadRow] {
        guard !turns.isEmpty else { return rows }
        var rows = rows
        // The latest agent text of the turn segment still open, by turn.
        var pending: [String: Int] = [:]
        func settle(_ turnID: String) {
            guard let last = pending.removeValue(forKey: turnID),
                  let status = turns[turnID]?.status else { return }
            if status == "completed" { rows[last].settledCommentary = false }
        }
        for index in rows.indices {
            guard let turnID = rows[index].turnId, !activeTurnIDs.contains(turnID),
                  let status = turns[turnID]?.status, ["completed", "interrupted", "failed"].contains(status) else { continue }
            if rows[index].isUser { settle(turnID); continue }
            guard rows[index].isAgentSpeech else { continue }
            if let earlier = pending[turnID] { rows[earlier].settledCommentary = true }
            pending[turnID] = index
        }
        for turnID in Array(pending.keys) { settle(turnID) }
        return rows
    }
}

public struct ChatFeedEntry: Identifiable, Sendable {
    public var rows: [ReadRow]
    public let id: String

    init(rows: [ReadRow]) {
        self.rows = rows
        id = rows[0].id
    }
    public var isContextCompaction: Bool { rows.count == 1 && rows[0].isContextCompaction }
    /// A durable boundary in the transcript: a compaction, a provider switch or
    /// a background task's report that woke the agent.
    public var isTimelineMarker: Bool { rows.count == 1 && (rows[0].isContextCompaction || rows[0].isProviderSwitch || rows[0].isTaskNotification) }
    public var isActivity: Bool { !isTimelineMarker && !rows[0].isPlan && rows[0].taskChecklist == nil && (rows[0].activitySummary != nil || rows[0].isCommentary) }

    public static func visibleRows(_ rows: [ReadRow], queuedClientIDs: Set<String>) -> [ReadRow] {
        let queuedRows = Set(queuedClientIDs.map { "user-" + $0 })
        return rows.filter { !queuedRows.contains($0.id) }
    }

    public static func grouping(_ rows: [ReadRow], activeTurnIDs: Set<String> = [], focusedRowID: String? = nil) -> [Self] {
        var entries: [Self] = []
        for row in rows {
            // Claude's task list shows as the turn's checklist card instead.
            if row.isClaudeTaskChange { continue }
            if row.item?.type == "reasoning" {
                // Thinking belongs to the reply's Working group: its time shows
                // when the group is expanded, not as a row of its own.
                // A thought with a summary stays for the Thinking level.
                let active = row.turnId.map(activeTurnIDs.contains) == true && row.activitySummary?.isRunning == true
                guard active || row.thinkingTiming?.completedLabel != nil || row.thinkingSummary != nil else { continue }
            }
            if row.isContextCompaction || row.isProviderSwitch || row.isTaskNotification || row.taskChecklist != nil {
                // A compaction is a durable boundary in the transcript, not a
                // row inside the surrounding Working disclosure.
                entries.append(Self(rows: [row]))
                continue
            }
            if row.activitySummary != nil || row.isCommentary, !row.isPlan, let first = entries.last?.rows.first,
               !entries.last!.isTimelineMarker, !first.isPlan, first.taskChecklist == nil,
               first.activitySummary != nil || first.isCommentary,
               row.id != focusedRowID, first.id != focusedRowID,
               let turn = row.turnId, first.turnId == turn,
               first.authorId == row.authorId {
                entries[entries.count - 1].rows.append(row)
            } else { entries.append(Self(rows: [row])) }
        }
        entries = mergingFinishedTurns(entries, activeTurnIDs: activeTurnIDs, focusedRowID: focusedRowID)
        // A finished thought alone between two messages would be a Working row
        // with nothing to report; keep one only as its turn's last activity,
        // where it carries the reply's time.
        let latest = latestActivityEntryIDs(entries)
        return entries.filter { entry in
            !entry.rows.allSatisfy { $0.item?.type == "reasoning" && $0.activitySummary?.isRunning != true && $0.thinkingSummary == nil }
                || latest.contains(entry.id)
        }
    }

    /// A finished turn is one "Worked for" fold: visible messages between its
    /// tools no longer split the work into several groups. The group keeps the
    /// first segment's ID and the messages stay in place after it. A running
    /// turn keeps its segments, since its current step is the latest one.
    static func mergingFinishedTurns(_ entries: [Self], activeTurnIDs: Set<String>, focusedRowID: String?) -> [Self] {
        var firstIndex: [String: Int] = [:]
        var result: [Self] = []
        for entry in entries {
            guard entry.isActivity, let turn = entry.rows.first?.turnId, !activeTurnIDs.contains(turn) else { result.append(entry); continue }
            if entry.rows.contains(where: { $0.id == focusedRowID }) {
                // The search-focused row stays a segment of its own; the turn's
                // later segments start a new fold after it, so the focused one
                // is not mistaken for the turn's latest.
                firstIndex[turn] = nil
                result.append(entry)
                continue
            }
            if let index = firstIndex[turn] {
                result[index].rows.append(contentsOf: entry.rows)
            } else {
                firstIndex[turn] = result.count
                result.append(entry)
            }
        }
        return result
    }

    /// Returns the entries that are the final visible activity segment for
    /// each turn. A turn can have multiple activity groups when visible agent
    /// text separates them, so duration and the active spinner belong only to
    /// the final segment.
    public static func latestActivityEntryIDs(_ entries: [Self]) -> Set<String> {
        var latest: [String: String] = [:]
        for entry in entries where entry.isActivity {
            guard let turn = entry.rows.first?.turnId else { continue }
            latest[turn] = entry.id
        }
        return Set(latest.values)
    }

    /// Returns the final visible activity segment for one canonical turn.
    /// The projection can contain more than one in-progress turn after a
    /// steer/replay, so callers choose the turn from the daemon lifecycle and
    /// never infer it from an activity row or receipt.
    public static func latestActivityEntryID(_ entries: [Self], turnID: String?) -> String? {
        guard let turnID else { return nil }
        return entries.last(where: { entry in
            entry.isActivity && entry.rows.first?.turnId == turnID
        })?.id
    }

    public static func actionSummary(rows: [ReadRow]) -> String? {
        enum Key: Hashable { case action(ActivityAction.Kind), tool(String) }
        var counts: [Key: Int] = [:]
        var paths: [ActivityAction.Kind: Set<String>] = [:]
        var unknownPaths: Set<ActivityAction.Kind> = []
        var order: [Key] = []
        var failed = 0, stopped = 0
        for row in rows {
            // Thinking time is shown inside the group, never counted as an action.
            guard row.item?.type != "reasoning", let activity = row.activitySummary else { continue }
            if activity.failed { failed += 1; continue }
            if activity.state == .interrupted { stopped += 1; continue }
            guard activity.state == .completed else { continue }
            for action in activity.actions {
                let kind = action.kind
                let key = activity.toolName.map(Key.tool) ?? .action(kind)
                if counts[key] == nil { order.append(key) }
                counts[key, default: 0] += 1
                paths[kind, default: []].formUnion(action.paths)
                if action.paths.isEmpty { unknownPaths.insert(kind) }
            }
        }
        var labels = order.prefix(3).map { key in
            let count = counts[key] ?? 0
            switch key {
            case .tool(let name): return count == 1 ? name : name + " (\(count) calls)"
            case .action(let kind): return ActivityAction.summary(kind, count: count, paths: paths[kind] ?? [], hasUnknownPaths: unknownPaths.contains(kind))
            }
        }
        if order.count > labels.count { labels.append("+\(order.count - labels.count) more") }
        if failed > 0 { labels.append("\(failed) \(failed == 1 ? "action" : "actions") failed") }
        if stopped > 0 { labels.append("\(stopped) \(stopped == 1 ? "action" : "actions") stopped") }
        return labels.isEmpty ? nil : labels.joined(separator: " · ")
    }

    /// What the finished turn's "Worked for" line adds in quieter type: the
    /// actions it took. Nil while running and for a turn with no recorded end.
    public static func lifecycleDetail(rows: [ReadRow], turn: ReadTurn?, isLatestSegmentForTurn: Bool) -> String? {
        guard isLatestSegmentForTurn, let turn, ["completed", "interrupted", "failed"].contains(turn.status) else { return nil }
        return actionSummary(rows: rows)
    }

    /// When the step this running turn is on began: the running tool's or
    /// thought's own start, else the turn's.
    public static func currentStepStart(rows: [ReadRow], turn: ReadTurn?) -> Date? {
        if let running = rows.last(where: { $0.activitySummary?.isRunning == true }) {
            if let timing = running.thinkingTiming { return timing.startedAt }
            if let seconds = ReadTimestamp.seconds(running.item?.createdAt ?? running.timestamp) { return Date(timeIntervalSince1970: seconds) }
        }
        return turn?.startedDate
    }

    public static func lifecycleLabel(rows: [ReadRow], turn: ReadTurn?, isLatestSegmentForTurn: Bool, isLatestActiveSegment: Bool) -> String {
        if turn?.isInProgress == true {
            let running = rows.last(where: { $0.activitySummary?.isRunning == true })?.activitySummary?.title
            if isLatestActiveSegment { return running ?? "Working…" }
            // An earlier segment of a running turn (a follow-up was sent while
            // its command ran) names what it is doing, not "unavailable".
            return running ?? actionSummary(rows: rows) ?? "Working…"
        }
        // A stopped or failed turn is named once, by its outcome marker after
        // the work; its fold only says it worked, not the outcome again.
        if isLatestSegmentForTurn, let turn, ["completed", "interrupted", "failed"].contains(turn.status) {
            return turn.status == "completed" ? turn.workedLabel : "Worked"
        }
        if let summary = actionSummary(rows: rows) { return summary }
        return turn?.status == "unknown" || turn == nil ? "Work status unavailable" : turn?.terminalLabel ?? "Worked"
    }
}

/// A compact provider item, decoded once for the list, header and task roster.
/// The caller supplies the current turn/task lifecycle; an item alone cannot
/// prove that its conversation is still running.
public struct CurrentActivity: Codable, Hashable, Sendable {
    private let item: ReadItem
    public let label: String?

    public init(item: ReadItem) {
        self.item = item
        let state = ActivityState.prepare(item)
        let kind = ActivityKind(rawValue: item.type) ?? .unknown
        guard state.isRunning else { label = nil; return }
        switch kind {
        case .agentMessage: label = "Writing reply"
        case .plan: label = "Planning"
        case .contextCompaction: label = "Compacting context"
        default:
            let actions = ActivityAction.prepare(item, kind: kind)
            if [.mcpToolCall, .dynamicToolCall].contains(kind), actions.count == 1,
               actions[0].kind == .tool, actions[0].target == nil,
               let name = ToolDisplayName.title(server: item.payload?["server"]?.string, tool: item.payload?["tool"]?.string) {
                label = name
            } else {
                let labels = actions.prefix(3).map { $0.label(state: state) }
                label = labels.isEmpty ? nil : labels.joined(separator: " · ")
            }
        }
    }
    public init(from decoder: Decoder) throws { self.init(item: try ReadItem(from: decoder)) }
    public func encode(to encoder: Encoder) throws { try item.encode(to: encoder) }
    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.item.id == rhs.item.id && lhs.item.type == rhs.item.type
            && lhs.item.state == rhs.item.state && lhs.item.payload == rhs.item.payload
    }
    public func hash(into hasher: inout Hasher) {
        hasher.combine(item.id); hasher.combine(item.type); hasher.combine(item.state)
    }

    public static func latest(in snapshot: ConversationSnapshot) -> Self? {
        guard let turn = snapshot.thread.turns?.last(where: { !$0.id.hasPrefix("local:") }),
              turn.isInProgress else { return nil }
        // A missing recent action stays unknown; opening a long chat must not
        // project its full history merely to populate the header.
        return turn.items.suffix(32).reversed().lazy.map(Self.init(item:)).first { $0.label != nil }
    }
}

public struct ActivityDetail: Identifiable, Sendable {
    public enum Kind: Sendable { case command, output, exitCode, error, files, additions, deletions, other }
    public let title: String
    public let text: String
    public var kind: Kind = .other
    public var id: String { title }
    public var isDiff: Bool = false
    public var filePath: String? = nil
}

/// Every visible item is a peer in the conversation's one lazy layout.
public struct ChatFeedNode: Identifiable, Sendable {
    public enum Content: Sendable {
        case entry(ChatFeedEntry)
        case activity(ReadRow)
        case compaction(ReadRow)
        case file(ConversationFile)
        /// Consecutive pictures of one entry, shown as one horizontal row.
        /// The node keeps the first picture's ID, so a reading anchor, the
        /// mini map and search land on it as before.
        case gallery([ConversationFile])
        /// Finished commands one after another in an open Working group, as
        /// one "Ran 6 commands" row. Opened, each command follows it as its
        /// own node in the same lazy stack.
        case commands(CommandRun)
        /// A thought with the summary its agent wrote, at the Thinking level.
        /// It keeps the `activity:` ID the same thought has without one.
        case thinking(ReadRow)
        /// A visual a later one in this conversation updates
        /// (`wonder_show_visual` with `replace`): one line in its place,
        /// under the node ID the visual had.
        case updatedPage(ConversationFile)
        /// How a turn that did not finish ended, after its last entry and
        /// outside the Working disclosure, at every detail level.
        case outcome(TurnOutcome)
        /// An answered, skipped or expired question, after the entry it
        /// belongs to, at every detail level. Its ID is the question's.
        case question(AsyncQuestion)
    }
    public let id: String
    public let entryID: String
    public let content: Content

    /// Maps a saved or vanished reading anchor (a Working header, a tool row,
    /// a command run, a picture or file, a message that folded into Working)
    /// to a node in `nodeIDs`, the nodes visible at the current detail level
    /// and disclosure. The result is always one of `nodeIDs`, or nil when the
    /// anchor belongs to no entry here (the caller then reads from the end).
    ///
    /// In order: the anchor itself; the row of pictures or the closed command
    /// run it folded into; the first visible node of its entry (its header
    /// when shown); else the first visible node after its entry, where the
    /// reader would have continued; else the last one before it.
    ///
    /// Pure, so the scroller can restore or keep a position without waiting
    /// on a node that will never appear.
    /// `nodeOwners` names the entry each node that is not derived from a
    /// row follows (settled questions; see `ChatFeedNode.owners`).
    public static func survivingAnchor(
        for anchor: String?,
        entries: [ChatFeedEntry],
        nodeIDs: [String],
        nodeOwners: [String: String] = [:]
    ) -> String? {
        guard let anchor else { return nil }
        if nodeIDs.contains(anchor) { return anchor }
        var owners: [String: Int] = [:]
        for (index, entry) in entries.enumerated() {
            if owners[entry.id] == nil { owners[entry.id] = index }
            for row in entry.rows where owners[row.id] == nil { owners[row.id] = index }
            // A turn's outcome follows its last entry.
            for row in entry.rows { if let turn = row.turnId { owners[TurnOutcome.nodeID(turn)] = index } }
        }
        for (node, entry) in nodeOwners { if let index = owners[entry] { owners[node] = index } }
        guard let index = owner(of: anchor, in: owners) else { return nil }
        let entry = entries[index]
        if let gallery = galleryAnchor(for: anchor, entry: entry, nodeIDs: nodeIDs) { return gallery }
        if let run = commandRunAnchor(for: anchor, entry: entry, nodeIDs: nodeIDs) { return run }
        var preceding: String?
        for id in nodeIDs {
            guard let other = owner(of: id, in: owners) else { continue }
            // Nodes follow entry order: the first at or after its entry is
            // its own first node, or failing that the next entry's.
            if other >= index { return id }
            preceding = id
        }
        return preceding
    }

    /// Question and outcome nodes by the entry they follow, for
    /// `survivingAnchor(nodeOwners:)` and the mini map.
    public static func owners(_ nodes: [Self]) -> [String: String] {
        var result: [String: String] = [:]
        for node in nodes {
            switch node.content {
            case .outcome, .question: result[node.id] = node.entryID
            default: break
            }
        }
        return result
    }

    /// The entry a node or anchor ID belongs to: an entry or row ID itself,
    /// `activity:`/`commands:` plus a row ID, or `file:` plus an entry or row
    /// ID, a colon and a file ID.
    private static func owner(of id: String, in owners: [String: Int]) -> Int? {
        if let index = owners[id] { return index }
        for prefix in ["activity:", "commands:"] where id.hasPrefix(prefix) {
            return owners[String(id.dropFirst(prefix.count))]
        }
        guard id.hasPrefix("file:") else { return nil }
        let rest = id.dropFirst("file:".count)
        var end = rest.endIndex
        // IDs can contain colons; the longest known prefix wins.
        while let colon = rest[..<end].lastIndex(of: ":") {
            if let index = owners[String(rest[..<colon])] { return index }
            end = colon
        }
        return nil
    }

    /// A picture folded into a row of pictures reads as that row: the nearest
    /// earlier picture of the same entry that is still its own node.
    private static func galleryAnchor(for anchor: String, entry: ChatFeedEntry, nodeIDs: [String]) -> String? {
        let prefix = "file:" + entry.id + ":"
        guard anchor.hasPrefix(prefix) else { return nil }
        var seen: Set<String> = []
        let files = entry.rows.flatMap(\.toolFiles).filter { seen.insert($0.id).inserted }
        guard let index = files.firstIndex(where: { prefix + $0.id == anchor }), files[index].isStillImage else { return nil }
        for file in files[..<index].reversed() {
            guard file.isStillImage else { return nil }
            if nodeIDs.contains(prefix + file.id) { return prefix + file.id }
        }
        return nil
    }

    /// A command folded into a closed run reads as that run: the nearest run
    /// that starts at or before it. Runs are contiguous, so that is its own.
    private static func commandRunAnchor(for anchor: String, entry: ChatFeedEntry, nodeIDs: [String]) -> String? {
        guard anchor.hasPrefix("activity:"),
              let index = entry.rows.firstIndex(where: { "activity:" + $0.id == anchor }) else { return nil }
        let present = Set(nodeIDs)
        return entry.rows[...index].reversed().lazy.map { "commands:" + $0.id }.first(where: present.contains)
    }

    /// `expanded` holds the IDs of open Working groups (their entry IDs) and
    /// open command runs (their `commands:` node IDs). `openDetails` holds the
    /// rows whose details are open: a run holding one stays open, so output
    /// being read does not fold away when its command finishes. `outcomes`
    /// are by turn ID (`TurnOutcome.prepare`), `questions` by the entry they
    /// follow (`ChatFeedIndex.placing`).
    public static func visible(_ entries: [ChatFeedEntry], expanded: Set<String>, detail: ChatDetailLevel = .everything,
                               openDetails: Set<String> = [], outcomes: [String: TurnOutcome] = [:],
                               questions: [String: [AsyncQuestion]] = [:]) -> [Self] {
        markingUpdatedPages(groupingPictures(nodes(entries, expanded: expanded, detail: detail, openDetails: openDetails,
                                                   outcomes: outcomes, questions: questions)), entries: entries)
    }

    /// A visual that a later visual of these entries replaces reads as
    /// updated; the newer one shows in full.
    static func markingUpdatedPages(_ nodes: [Self], entries: [ChatFeedEntry]) -> [Self] {
        let replaced = Set(entries.lazy.flatMap(\.rows).compactMap(\.replacedPage))
        guard !replaced.isEmpty else { return nodes }
        return nodes.map { node in
            guard case .file(let file) = node.content, replaced.contains(file.id) else { return node }
            return Self(id: node.id, entryID: node.entryID, content: .updatedPage(file))
        }
    }

    /// Two or more pictures in a row from one entry become one gallery node.
    static func groupingPictures(_ nodes: [Self]) -> [Self] {
        var result: [Self] = []
        result.reserveCapacity(nodes.count)
        for node in nodes {
            guard case .file(let file) = node.content, file.isStillImage, let last = result.last,
                  last.entryID == node.entryID else { result.append(node); continue }
            switch last.content {
            case .file(let first) where first.isStillImage:
                result[result.count - 1] = Self(id: last.id, entryID: last.entryID, content: .gallery([first, file]))
            case .gallery(let files):
                result[result.count - 1] = Self(id: last.id, entryID: last.entryID, content: .gallery(files + [file]))
            default: result.append(node)
            }
        }
        return result
    }

    private static func nodes(_ entries: [ChatFeedEntry], expanded: Set<String>, detail: ChatDetailLevel, openDetails: Set<String>,
                              outcomes: [String: TurnOutcome], questions: [String: [AsyncQuestion]]) -> [Self] {
        var result: [Self] = []
        // Each outcome follows the last entry of its turn.
        var outcomesAfter: [String: [TurnOutcome]] = [:]
        var absorbed: Set<String> = []
        if !outcomes.isEmpty {
            var lastEntry: [String: String] = [:]
            for entry in entries {
                for row in entry.rows { if let turn = row.turnId, outcomes[turn] != nil { lastEntry[turn] = entry.id } }
            }
            for (turn, entryID) in lastEntry.sorted(by: { $0.key < $1.key }) { outcomesAfter[entryID, default: []].append(outcomes[turn]!) }
            absorbed = Set(outcomes.values.flatMap(\.errorRowIDs))
        }
        func emit(_ source: ChatFeedEntry) {
            var entry = source
            // Apply the current preference to cached history and live rows alike.
            // Keep the group's identity as its first visible row changes; questions
            // and outcomes are emitted independently below, even for an empty group.
            if detail != .thinking, entry.rows.contains(where: { $0.item?.type == "reasoning" }) {
                // A running thought is the turn's current step: its group's
                // header shows it, so it stays when thinking text is hidden.
                entry.rows.removeAll { $0.item?.type == "reasoning" && $0.activitySummary?.isRunning != true }
                guard !entry.rows.isEmpty else { return }
            }
            if entry.isTimelineMarker, let row = entry.rows.first {
                // Keep the marker in the single lazy timeline regardless of
                // disclosure state. Its identity is the App Server item ID.
                result.append(Self(id: row.id, entryID: entry.id, content: .compaction(row)))
                return
            }
            // A turn's error that its outcome states is not repeated inside
            // Working; work that was only that error has no group of its own.
            let onlyAbsorbed = entry.isActivity && !absorbed.isEmpty && entry.rows.allSatisfy { absorbed.contains($0.id) }
            if onlyAbsorbed, !expanded.contains(entry.id) { return }
            if detail == .conversation, entry.isActivity {
                // Only the pictures, video and audio its tools made; the agent's
                // commentary is work too. The same node IDs as when expanded.
                var seen: Set<String> = []
                for row in entry.rows {
                    for file in row.shownFiles where seen.insert(file.id).inserted {
                        result.append(Self(id: "file:" + entry.id + ":" + file.id, entryID: entry.id, content: .file(file)))
                    }
                }
                return
            }
            result.append(Self(id: entry.id, entryID: entry.id, content: .entry(entry)))
            let isActivity = entry.isActivity
            var seen: Set<String> = []
            // Tool details belong to the Working disclosure; the pictures, video
            // and audio the tools made show directly, expanded or not, under the
            // same node IDs. Files of a visible message always show.
            guard !isActivity || expanded.contains(entry.id) else {
                for row in entry.rows {
                    for file in row.shownFiles where seen.insert(file.id).inserted {
                        result.append(Self(id: "file:" + entry.id + ":" + file.id, entryID: entry.id, content: .file(file)))
                    }
                }
                return
            }
            // The group's header already names the running step with its ticking
            // time; its own row below would say the same thing twice.
            let currentStepID = isActivity ? entry.rows.last(where: { $0.activitySummary?.isRunning == true })
                .flatMap { detail == .thinking && $0.thinkingSummary != nil ? nil : $0.id } : nil
            func append(_ row: ReadRow) {
                if isActivity && row.id != currentStepID && !row.isOnlyItsMedia && (onlyAbsorbed || !absorbed.contains(row.id)) {
                    if detail == .thinking, row.thinkingSummary != nil {
                        result.append(Self(id: "activity:" + row.id, entryID: entry.id, content: .thinking(row)))
                    } else if row.isCommentary || row.item?.type != "reasoning" || row.thinkingTiming != nil {
                        result.append(Self(id: "activity:" + row.id, entryID: entry.id, content: .activity(row)))
                    }
                }
                for file in row.toolFiles where seen.insert(file.id).inserted {
                    result.append(Self(id: "file:" + entry.id + ":" + file.id, entryID: entry.id, content: .file(file)))
                }
            }
            var run: [ReadRow] = []
            func endRun() {
                defer { run = [] }
                guard run.count > 1 else { run.forEach(append); return }
                let id = "commands:" + run[0].id
                result.append(Self(id: id, entryID: entry.id, content: .commands(CommandRun(rows: run))))
                if expanded.contains(id) || run.contains(where: { openDetails.contains($0.id) }) { run.forEach(append); return }
                for row in run {
                    for file in row.shownFiles where seen.insert(file.id).inserted {
                        result.append(Self(id: "file:" + entry.id + ":" + file.id, entryID: entry.id, content: .file(file)))
                    }
                }
            }
            for row in entry.rows {
                if isActivity, CommandRun.joins(row) {
                    run.append(row)
                } else if isActivity, !run.isEmpty, row.item?.type == "reasoning", row.thinkingTiming == nil,
                          detail != .thinking || row.thinkingSummary == nil {
                    // A thought that shows nothing at this level does not split a run.
                    continue
                } else {
                    endRun()
                    append(row)
                }
            }
            endRun()
        }
        for entry in entries {
            emit(entry)
            for question in questions[entry.id] ?? [] {
                result.append(Self(id: "question:" + question.id, entryID: entry.id, content: .question(question)))
            }
            for outcome in outcomesAfter[entry.id] ?? [] {
                result.append(Self(id: outcome.nodeID, entryID: entry.id, content: .outcome(outcome)))
            }
        }
        return result
    }
}

/// How a turn ended when it did not finish: it failed or was stopped. One
/// per turn, with the reason the agent's runtime gave, shown outside the
/// Working disclosure so Conversation detail keeps it too.
public struct TurnOutcome: Equatable, Sendable {
    public enum Kind: Sendable { case failed, stopped }
    public let turnID: String
    public let kind: Kind
    /// The first line of the turn's last error, bounded; nil when none was
    /// recorded or it is not plain text.
    public let reason: String?
    /// The turn's error rows that say exactly `reason`; Working omits them.
    public let errorRowIDs: Set<String>

    public var nodeID: String { Self.nodeID(turnID) }
    public static func nodeID(_ turnID: String) -> String { "outcome:" + turnID }

    public func label(isProjectConversation: Bool) -> String {
        switch kind {
        // A desktop-owned Project may already be running newer work; name
        // the recorded response, as its Working header does.
        case .failed: isProjectConversation ? "This response couldn’t finish" : "This reply couldn’t finish"
        case .stopped: isProjectConversation ? "Earlier response interrupted" : "Reply stopped"
        }
    }
    public var symbol: String { kind == .failed ? "exclamationmark.triangle" : "stop.circle" }

    /// Prepared once per timeline revision from its settled rows. A running
    /// turn, a completed one and a local placeholder have no outcome.
    public static func prepare(rows: [ReadRow], turns: [String: ReadTurn], activeTurnIDs: Set<String>) -> [String: Self] {
        guard turns.values.contains(where: { ["failed", "interrupted"].contains($0.status) }) else { return [:] }
        var errors: [String: [ReadRow]] = [:]
        var present: Set<String> = []
        for row in rows {
            guard let turn = row.turnId else { continue }
            present.insert(turn)
            if !row.isUser, row.item?.type == "error" { errors[turn, default: []].append(row) }
        }
        var result: [String: Self] = [:]
        for turnID in present where !activeTurnIDs.contains(turnID) && !turnID.hasPrefix("local:") {
            let kind: Kind
            switch turns[turnID]?.status {
            case "failed": kind = .failed
            case "interrupted": kind = .stopped
            default: continue
            }
            let failures = errors[turnID] ?? []
            let reason = failures.reversed().lazy.compactMap { reasonText($0.item?.text ?? $0.text) }.first
            let absorbed = reason.map { reason in Set(failures.filter { reasonText($0.item?.text ?? $0.text) == reason }.map(\.id)) } ?? []
            result[turnID] = Self(turnID: turnID, kind: kind, reason: reason, errorRowIDs: absorbed)
        }
        return result
    }

    static func reasonText(_ text: String?) -> String? {
        guard let line = text?.split(whereSeparator: \.isNewline).lazy
            .map({ $0.trimmingCharacters(in: .whitespaces) }).first(where: { !$0.isEmpty }),
              let first = line.first, first != "{", first != "[" else { return nil }
        return line.count > 240 ? String(line.prefix(239)) + "…" : line
    }
}

/// Where each row and turn sits in the grouped entries, built once per
/// timeline revision, so placing the settled questions on every update is one
/// lookup per question rather than a scan of the chat per visible row.
public struct ChatFeedIndex: Sendable {
    private let entryOfRow: [String: String]
    private let lastEntryOfTurn: [String: String]

    public init(entries: [ChatFeedEntry]) {
        var rows: [String: String] = [:], turns: [String: String] = [:]
        for entry in entries {
            for row in entry.rows {
                rows[row.id] = entry.id
                if let turn = row.turnId { turns[turn] = entry.id }
            }
        }
        entryOfRow = rows
        lastEntryOfTurn = turns
    }

    /// The entry a request or question belongs to: its own row's, else the
    /// last entry of its turn. Nil when that part of the chat is not loaded.
    public func entry(row: String?, turn: String?) -> String? {
        row.flatMap { entryOfRow[$0] } ?? turn.flatMap { lastEntryOfTurn[$0] }
    }

    /// Questions that can no longer be answered, by the entry they follow:
    /// their own row's, else the last entry of their turn (history removed
    /// the item). Answered questions whose turn is not loaded at all come
    /// back as `outside`. Pending questions stay in the attention flow.
    /// Call with the current questions and time: state, response and
    /// expiry change without a new timeline revision.
    public func placing(_ questions: [AsyncQuestion], now: UInt64) -> (byEntry: [String: [AsyncQuestion]], outside: [AsyncQuestion]) {
        var byEntry: [String: [AsyncQuestion]] = [:], outside: [AsyncQuestion] = []
        for question in questions where !question.canAnswer(now: now) {
            if let entry = entryOfRow[question.rowId] ?? lastEntryOfTurn[question.turnId] {
                byEntry[entry, default: []].append(question)
            } else if question.state == "answered" {
                outside.append(question)
            }
        }
        return (byEntry, outside)
    }
}

/// Two or more finished commands one after another, shown as one row until
/// opened.
public struct CommandRun: Sendable {
    public let rows: [ReadRow]
    /// "Ran 6 commands", with how many failed or were stopped.
    public let label: String

    init(rows: [ReadRow]) {
        self.rows = rows
        let stopped = rows.filter { $0.item?.state == "interrupted" }.count
        let failed = rows.filter { $0.item?.state != "interrupted" && ($0.commandSummary?.failed ?? ($0.activitySummary?.failed == true)) }.count
        var parts = ["Ran \(rows.count) commands"]
        if failed > 0 { parts.append("\(failed) failed") }
        if stopped > 0 { parts.append("\(stopped) stopped") }
        label = parts.joined(separator: " · ")
    }

    /// A command that is no longer running. A running one, or one waiting
    /// for approval, stays its own row so it can be followed.
    static func joins(_ row: ReadRow) -> Bool {
        row.item?.type == "commandExecution" && row.activitySummary?.isRunning == false && !row.isOnlyItsMedia
    }
}

public struct ActivityPresentation: Sendable {
    public let title: String
    /// Prepared once, also used by collapsed groups without reparsing names.
    public let toolName: String?
    public let symbol: String
    public let state: ActivityState
    public let details: [ActivityDetail]
    public let kind: ActivityKind
    public let actions: [ActivityAction]
    public let accessibilityLabel: String
    /// A command's panel text ("$ command", a blank line, its output), built
    /// once with the details so an open panel compares the same string.
    public let commandPanelText: String?
    public var isRunning: Bool { state.isRunning }
    public var failed: Bool { state == .failed }
    public var usesCommandLabel: Bool { kind == .commandExecution && actions.allSatisfy { $0.kind == .command } }
    public func detail(_ kind: ActivityDetail.Kind) -> String? { details.first { $0.kind == kind }?.text }
    public var status: String {
        switch state {
        case .started, .streaming: "Running"
        case .completed: "Completed"
        case .failed: "Failed"
        case .interrupted: "Stopped"
        case .waiting: "Waiting"
        default: "Status unavailable"
        }
    }
}

/// Cheap, product-facing data for one command disclosure row. The daemon has
/// already sanitized and bounded the command payload; this type only removes
/// an unambiguous shell launch prefix for the collapsed label. It never runs,
/// expands, or evaluates shell syntax.
public struct CommandSummary: Equatable, Sendable {
    public let rawCommand: String
    public let displayCommand: String
    public let duration: String?
    public let state: String
    public let exitCode: Int64?
    public let failed: Bool
    public let stopped: Bool
    /// What VoiceOver reads: "Ran swift test for 5 seconds", without the
    /// Markdown backticks and abbreviated units of `label(includeDuration:)`.
    public let accessibilityLabel: String

    private init(rawCommand: String, displayCommand: String, duration: String?, spokenDuration: String?,
                 state: String, exitCode: Int64?, failed: Bool, stopped: Bool) {
        self.rawCommand = rawCommand
        self.displayCommand = displayCommand
        self.duration = duration
        self.state = state
        self.exitCode = exitCode
        self.failed = failed
        self.stopped = stopped
        let prefix = Self.prefix(for: state, failed: failed, stopped: stopped)
        let spoken = prefix + " " + displayCommand
        self.accessibilityLabel = spokenDuration.map { spoken + " for " + $0 } ?? spoken
    }

    public var prefix: String {
        Self.prefix(for: state, failed: failed, stopped: stopped)
    }

    /// The Markdown export line: "Ran `swift test` for 5s".
    public func label(includeDuration: Bool) -> String {
        Self.makeLabel(prefix: prefix, command: displayCommand, duration: includeDuration ? duration : nil)
    }

    /// The collapsed row shows one line; a longer command ends in "…" there
    /// and stays complete in the expanded details (`rawCommand`).
    public static let displayLimit = 200

    public static func prepare(item: ReadItem) -> Self? {
        guard item.type == "commandExecution",
              let command = item.payload?["command"]?.string else { return nil }

        let rawCommand = command
        let source = trimmed(command)
        guard !source.isEmpty else { return nil }
        let unwrapped = shellCommand(from: source) ?? source
        let displayCommand = singleLine(unwrapped, limit: displayLimit)
        guard !displayCommand.isEmpty else { return nil }

        let exitCode = integer(item.payload?["exitCode"])
        let stopped = item.state == "interrupted"
        let running = ["started", "streaming", "waiting"].contains(item.state)
        let resultIsError: Bool
        if case .object(let result) = item.payload?["result"] {
            resultIsError = result["isError"]?.bool == true
        } else {
            resultIsError = false
        }
        let hasError = item.payload?["error"].map { $0 != .null && $0.string != "" } == true
        let failed = item.state == "failed"
            || (!stopped && !running && (exitCode.map { $0 != 0 } == true || item.payload?["success"]?.bool == false || resultIsError || hasError))

        return Self(rawCommand: rawCommand, displayCommand: displayCommand,
                    duration: compactDuration(item.payload?["durationMs"]),
                    spokenDuration: spokenDuration(item.payload?["durationMs"]), state: item.state,
                    exitCode: exitCode, failed: failed, stopped: stopped)
    }

    private static func prefix(for state: String, failed: Bool, stopped: Bool) -> String {
        if stopped { return "Stopped" }
        if failed { return "Failed" }
        switch state {
        case "started", "streaming", "waiting": return "Running"
        case "completed": return "Ran"
        default: return "Command"
        }
    }

    private static func makeLabel(prefix: String, command: String, duration: String?) -> String {
        let base = prefix + " `" + command + "`"
        return duration.map { base + " for " + $0 } ?? base
    }

    private struct LexicalToken {
        let value: String
        let raw: String
    }

    private static func shellCommand(from source: String) -> String? {
        // Most rows are ordinary commands. Read only their first lexical word
        // before invoking the bounded scanner used for recognized wrappers.
        let firstWord = String(Substring(source.unicodeScalars.prefix { !isSpace($0) }))
        guard recognizedShells.contains(basename(firstWord)) || basename(firstWord) == "env" else { return nil }
        guard let tokens = tokenize(source), !tokens.isEmpty else { return nil }
        var shellIndex = 0
        if basename(tokens[0].value) == "env" {
            shellIndex = 1
            while shellIndex < tokens.count && isEnvironmentAssignment(tokens[shellIndex].value) {
                shellIndex += 1
            }
        }
        guard shellIndex < tokens.count,
              recognizedShells.contains(basename(tokens[shellIndex].value)) else { return nil }

        let commandOptions = ["-c", "-lc", "-ic", "-ilc"]
        let optionStart = shellIndex + 1
        guard optionStart < tokens.count else { return nil }
        var optionIndex = optionStart
        while optionIndex < tokens.count {
            let option = tokens[optionIndex].value
            if commandOptions.contains(option) {
                let commandIndex = optionIndex + 1
                guard commandIndex == tokens.count - 1 else { return nil }
                return commandText(from: tokens[commandIndex])
            }
            guard shellFlags.contains(option) else { return nil }
            optionIndex += 1
        }
        return nil
    }

    private static func commandText(from token: LexicalToken) -> String? {
        guard !token.value.isEmpty else { return nil }
        let scalars = token.raw.unicodeScalars
        if let first = scalars.first, let last = scalars.last, scalars.count >= 2,
           first == "'" || first == "\"", last == first {
            let inner = scalars.dropFirst().dropLast()
            guard !inner.contains(first) else { return nil }
            return String(Substring(inner))
        }
        guard !scalars.contains(where: { "'\"\\;$|&<>()`".unicodeScalars.contains($0) || isSpace($0) }) else { return nil }
        // Keep an unquoted command argument byte-for-byte. In particular, do
        // not reinterpret backslashes or substitutions while preparing UI.
        return token.raw
    }

    /// Scans Unicode scalars, not grapheme clusters: every character it
    /// recognizes is ASCII, and a long command scans in linear time.
    private static func tokenize(_ source: String) -> [LexicalToken]? {
        var result: [LexicalToken] = []
        let scalars = source.unicodeScalars
        var wordStart: String.Index?
        var value = String.UnicodeScalarView()
        var quote: Unicode.Scalar?
        var escaped = false

        func finish(_ end: String.Index) {
            guard let start = wordStart else { return }
            result.append(LexicalToken(value: String(value), raw: String(Substring(scalars[start..<end]))))
            wordStart = nil
            value = String.UnicodeScalarView()
        }

        var index = scalars.startIndex
        while index < scalars.endIndex {
            let scalar = scalars[index]
            if escaped {
                value.append(scalar)
                escaped = false
                index = scalars.index(after: index)
                continue
            }
            if scalar == "\\" && quote != "'" {
                if wordStart == nil { wordStart = index }
                escaped = true
                index = scalars.index(after: index)
                continue
            }
            if let open = quote {
                if scalar == open { quote = nil }
                else { value.append(scalar) }
                index = scalars.index(after: index)
                continue
            }
            if scalar == "'" || scalar == "\"" {
                if wordStart == nil { wordStart = index }
                quote = scalar
                index = scalars.index(after: index)
                continue
            }
            // Operators outside the sole quoted command argument belong to
            // the caller's shell, not to a wrapper we can safely remove.
            if operators.contains(scalar) { return nil }
            if isSpace(scalar) {
                finish(index)
            } else {
                if wordStart == nil { wordStart = index }
                value.append(scalar)
            }
            index = scalars.index(after: index)
        }
        guard quote == nil, !escaped else { return nil }
        finish(scalars.endIndex)
        return result
    }

    private static let operators = Set(";$|&<>()`".unicodeScalars)

    private static func isSpace(_ scalar: Unicode.Scalar) -> Bool {
        scalar.isASCII ? scalar == " " || (0x09...0x0D).contains(scalar.value) : scalar.properties.isWhitespace
    }

    private static func trimmed(_ value: String) -> String {
        let scalars = value.unicodeScalars
        guard let start = scalars.firstIndex(where: { !isSpace($0) }),
              let end = scalars.lastIndex(where: { !isSpace($0) }) else { return "" }
        if start == scalars.startIndex && scalars.index(after: end) == scalars.endIndex { return value }
        return String(Substring(scalars[start...end]))
    }

    private static let recognizedShells: Set<String> = ["sh", "bash", "zsh"]
    private static let shellFlags: Set<String> = ["-l", "-i", "--login", "--interactive", "--noprofile", "--norc", "--no-profile", "--no-rcs"]

    private static func basename(_ value: String) -> String {
        value.split(separator: "/").last.map(String.init) ?? value
    }

    private static func isEnvironmentAssignment(_ value: String) -> Bool {
        guard let equals = value.firstIndex(of: "="), equals > value.startIndex else { return false }
        let name = value[..<equals]
        guard let first = name.first, first == "_" || first.isASCII && first.isLetter else { return false }
        return name.dropFirst().allSatisfy { $0 == "_" || $0.isASCII && $0.isLetter || $0.isASCII && $0.isNumber }
    }

    private static func integer(_ value: ThreadValue?) -> Int64? {
        guard let value = value?.number, value.isFinite, value.rounded() == value,
              value >= -9_223_372_036_854_775_808.0, value < 9_223_372_036_854_775_808.0 else { return nil }
        return Int64(value)
    }

    private static func validMilliseconds(_ value: ThreadValue?) -> Int64? {
        guard let rawMilliseconds = value?.number, rawMilliseconds.isFinite, rawMilliseconds >= 0,
              rawMilliseconds.rounded() == rawMilliseconds, rawMilliseconds <= 9_007_199_254_740_991 else { return nil }
        return Int64(rawMilliseconds)
    }

    private static func compactDuration(_ value: ThreadValue?) -> String? {
        guard let milliseconds = validMilliseconds(value) else { return nil }
        if milliseconds < 1_000 { return "\(milliseconds)ms" }
        if milliseconds < 60_000 {
            if milliseconds % 1_000 == 0 { return "\(milliseconds / 1_000)s" }
            let seconds = milliseconds / 1_000
            let tenths = (milliseconds % 1_000 + 50) / 100
            if tenths == 10 { return "\(seconds + 1)s" }
            return "\(seconds).\(tenths)s"
        }
        let seconds = milliseconds / 1_000
        let minutes = seconds / 60
        let remainder = seconds % 60
        if minutes < 60 { return remainder == 0 ? "\(minutes)m" : "\(minutes)m \(remainder)s" }
        let hours = minutes / 60
        let remainingMinutes = minutes % 60
        return remainingMinutes == 0 ? "\(hours)h" : "\(hours)h \(remainingMinutes)m"
    }

    /// `compactDuration` in words, with the same rounding: "5.3 seconds".
    private static func spokenDuration(_ value: ThreadValue?) -> String? {
        guard let milliseconds = validMilliseconds(value) else { return nil }
        func unit(_ count: Int64, _ name: String) -> String { "\(count) " + name + (count == 1 ? "" : "s") }
        if milliseconds < 1_000 { return unit(milliseconds, "millisecond") }
        if milliseconds < 60_000 {
            let seconds = milliseconds / 1_000
            let tenths = (milliseconds % 1_000 + 50) / 100
            if tenths == 0 { return unit(seconds, "second") }
            if tenths == 10 { return unit(seconds + 1, "second") }
            return "\(seconds).\(tenths) seconds"
        }
        let seconds = milliseconds / 1_000
        let minutes = seconds / 60
        let remainder = seconds % 60
        if minutes < 60 { return remainder == 0 ? unit(minutes, "minute") : unit(minutes, "minute") + " " + unit(remainder, "second") }
        let hours = minutes / 60
        let remainingMinutes = minutes % 60
        return remainingMinutes == 0 ? unit(hours, "hour") : unit(hours, "hour") + " " + unit(remainingMinutes, "minute")
    }

    /// Whitespace runs become one space; past `limit` characters the line
    /// ends in "…" without reading the rest of the command.
    private static func singleLine(_ value: String, limit: Int) -> String {
        var result = String.UnicodeScalarView()
        var pendingSpace = false
        var truncated = false, scanned = 0
        for scalar in value.unicodeScalars {
            if isSpace(scalar) {
                if !result.isEmpty { pendingSpace = true }
                continue
            }
            // Scalars bound the scan; characters (fewer or equal) bound the text.
            if scanned > limit * 2 { truncated = true; break }
            if pendingSpace { result.append(" "); pendingSpace = false }
            result.append(scalar); scanned += 1
        }
        let line = String(result)
        guard truncated || line.count > limit else { return line }
        return String(line.prefix(limit - 1)) + "…"
    }
}

public struct ContextCompactionPresentation: Equatable, Sendable {
    public let label: String
    public let symbol: String
    public let isRunning: Bool
    public init(label: String, symbol: String, isRunning: Bool) { self.label = label; self.symbol = symbol; self.isRunning = isRunning }

    public static func forState(_ state: String, isProjectConversation: Bool = false) -> Self {
        switch state.lowercased() {
        case "started", "streaming", "waiting":
            return Self(label: "Compacting context…", symbol: "arrow.down.right.and.arrow.up.left", isRunning: true)
        case "completed":
            return Self(label: "Context compacted", symbol: "text.badge.checkmark", isRunning: false)
        case "interrupted":
            if isProjectConversation {
                return Self(label: "Context compaction interrupted", symbol: "exclamationmark.circle", isRunning: false)
            }
            return Self(label: "Context compaction stopped", symbol: "stop.circle", isRunning: false)
        case "failed":
            return Self(label: "Context compaction failed", symbol: "exclamationmark.triangle", isRunning: false)
        default:
            return Self(label: "Context compaction status unavailable", symbol: "questionmark.circle", isRunning: false)
        }
    }
}

extension ReadRow {
    /// A plan the agent proposed. It is the answer the owner reviews, so the
    /// timeline shows it as its own card instead of inside the Working group.
    public var isPlan: Bool { !isUser && item?.type == "plan" }

    /// Markdown body of a plan; hosts put it in the item text.
    public var planText: String? {
        guard isPlan else { return nil }
        let text = item?.text ?? item?.payload?["text"]?.string ?? item?.payload?["plan"]?.string
        guard let trimmed = text?.trimmingCharacters(in: .whitespacesAndNewlines), !trimmed.isEmpty else { return nil }
        return trimmed
    }

    /// The plan an owner can implement: the newest one, once it is complete.
    /// Earlier plans were superseded, and a streaming plan may still change.
    public static func implementablePlanID(in rows: [ReadRow]) -> String? {
        guard let latest = rows.last(where: \.isPlan), latest.planText != nil,
              !["started", "streaming", "waiting", "unknown"].contains(latest.item?.state ?? "") else { return nil }
        return latest.id
    }

    public var activity: ActivityPresentation? { presentation(includeDetails: true) }
    public var isContextCompaction: Bool { !isUser && item?.type == "contextCompaction" }
    /// The host writes "Switched to Claude Sonnet · Codex history was handed
    /// over …"; the conversation shows only the switch, not how history moved.
    public var isProviderSwitch: Bool { !isUser && item?.type == "providerSwitch" }
    /// A background task's report that woke the agent into a run of its own.
    /// The host writes the row's words (`Command "Build" finished`); the run's
    /// tools fold into their own "Worked" row after it, then its reply.
    public var isTaskNotification: Bool { !isUser && item?.type == "taskNotification" }
    public var taskNotificationText: String? {
        guard isTaskNotification else { return nil }
        let text = (item?.text ?? text).trimmingCharacters(in: .whitespacesAndNewlines)
        return text.isEmpty ? "Background task finished" : text
    }
    /// The glyph of what reported back: a command's terminal, a monitor's eye,
    /// an agent's person. A report of failure or a stop says so in its words.
    public var taskNotificationSymbol: String {
        switch item?.payload?["kind"]?.string {
        case "agent": return "person.crop.circle"
        case "monitor": return "eye"
        default: return "terminal"
        }
    }
    public var providerSwitchText: String? {
        guard isProviderSwitch, let text = item?.text else { return nil }
        let headline = (text.components(separatedBy: " · ").first ?? text).trimmingCharacters(in: .whitespacesAndNewlines)
        return headline.isEmpty ? nil : headline
    }
    public var contextCompactionPresentation: ContextCompactionPresentation? {
        guard isContextCompaction, let item else { return nil }
        return .forState(item.state)
    }

    func presentation(includeDetails: Bool) -> ActivityPresentation? {
        guard !isUser, let item else { return nil }
        let kind = ActivityKind(rawValue: item.type) ?? .unknown
        guard kind != .userMessage, kind != .agentMessage, kind != .approval else { return nil }
        let state = ActivityState.prepare(item)
        let actions = ActivityAction.prepare(item, kind: kind)
        let payload = item.payload ?? [:]
        var details: [ActivityDetail] = []
        func add(_ title: String, _ makeText: @autoclosure () -> String?, kind: ActivityDetail.Kind = .other) {
            guard includeDetails, let text = makeText(), !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
            details.append(ActivityDetail(title: title, text: text, kind: kind))
        }
        func value(_ key: String) -> String? {
            guard let value = payload[key], value != .null else { return nil }
            if let text = value.string { return text }
            let encoder = JSONEncoder(); encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
            return (try? encoder.encode(value)).flatMap { String(data: $0, encoding: .utf8) }
        }
        // Errors the agent's code checker reported for an edit that succeeded;
        // they never turn the edit into a failure.
        let codeErrors: [String] = kind == .fileChange ? (payload["diagnostics"]?.array ?? []).prefix(10).compactMap { entry in
            guard case .object(let fields) = entry, let path = fields["path"]?.string, let message = fields["message"]?.string,
                  let line = fields["line"]?.number, let column = fields["column"]?.number,
                  (1...1e9).contains(line), (1...1e9).contains(column) else { return nil }
            return "\(path) line \(Int(line)), column \(Int(column)): \(message)"
        } : []
        let title: String
        let symbol: String
        switch kind {
        case .commandExecution:
            title = "Run command"; symbol = "terminal"
            add("Command", value("command"), kind: .command); add("Folder", value("cwd"))
            add("Output", value("output") ?? value("aggregatedOutput"), kind: .output)
            add("Exit code", value("exitCode"), kind: .exitCode)
        case .webSearch:
            var action: [String: ThreadValue] = [:]
            if case .object(let fields) = payload["action"] { action = fields }
            switch action["type"]?.string {
            case "openPage": title = "Open web page"
            case "findInPage": title = "Find in page"
            default: title = "Search the web"
            }
            symbol = "globe"
            add("Search", value("query") ?? action["query"]?.string
                ?? action["queries"]?.array?.compactMap(\.string).joined(separator: "\n"))
            add("Page", action["url"]?.string); add("Find", action["pattern"]?.string)
            add("Results", value("resultCount"))
        case .fileChange:
            title = fileChangeSummary?.title ?? "Edited a file"; symbol = "pencil"
            add("Files", payload["paths"]?.array?.compactMap(\.string).map(FileChangeSummary.filename).joined(separator: "\n"), kind: .files)
            add("Added lines", value("additions"), kind: .additions); add("Removed lines", value("deletions"), kind: .deletions)
            for change in includeDetails ? (payload["diffs"]?.array ?? []) : [] {
                if case .object(let fields) = change, let diff = fields["diff"]?.string, !diff.isEmpty {
                    details.append(ActivityDetail(title: fields["path"]?.string.map(FileChangeSummary.filename) ?? "Changes", text: diff, isDiff: true, filePath: FileChangeSummary.relativePath(fields["path"]?.string)))
                }
            }
            add("Changes", value("detail"))
            add("Code errors", codeErrors.isEmpty ? nil : ([(payload["diagnosticsReporter"]?.string ?? "The agent") + " reported these after the edit:"] + codeErrors).joined(separator: "\n"))
        case .collabAgentToolCall:
            title = "Agent"; symbol = "sparkles.rectangle.stack"
            add("Details", value("result") ?? "An agent task update.")
            add("Input", value("arguments"))
        case .mcpToolCall, .dynamicToolCall:
            title = "Tool"; symbol = "puzzlepiece.extension"
            add("Input", value("arguments")); add("Result", (payload["result"] ?? payload["contentItems"])?.toolOutputText)
        case .imageView:
            title = "Image"; symbol = "photo"
            add("Result", payload["result"]?.toolOutputText)
        case .imageGeneration:
            title = "Generate image"; symbol = "wand.and.stars"
            // The Mac turns the image into a verified file; raw image bytes in
            // the result never reach the detail view.
            if payload["result"]?.string != nil {
                add("Result", "Preview unavailable.")
            } else {
                add("Result", payload["result"]?.toolOutputText)
            }
            add("Error", value("failure"))
        case .functionCallOutput:
            title = "Tool result"; symbol = "doc.plaintext"
            add("Result", payload["output"]?.toolOutputText ?? item.text)
        case .error:
            title = "Couldn’t complete an action"; symbol = "exclamationmark.triangle"
            add("Error", item.text)
        case .reasoning: title = "Thinking"; symbol = "brain"
        case .plan:
            title = "Plan"; symbol = "list.bullet.clipboard"; add("Plan", item.text)
        case .hookPrompt:
            title = "Additional instructions"; symbol = "text.quote"
            if includeDetails {
                let fragments = payload["fragments"]?.array?.compactMap { fragment -> String? in
                    guard case .object(let fields) = fragment,
                          let text = fields["text"]?.string else { return nil }
                    let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
                    return trimmed.isEmpty ? nil : trimmed
                } ?? []
                add("Instructions", fragments.isEmpty ? nil : fragments.joined(separator: "\n\n"))
            }
        case .subAgentActivity:
            symbol = "sparkles.rectangle.stack"
            let name = payload["agentNickname"]?.string ?? payload["agentRole"]?.string ?? "Agent"
            switch payload["kind"]?.string {
            case "started": title = name + " started"
            case "interacted": title = name + " received an update"
            case "interrupted": title = name + " stopped"
            case "completed": title = name + " finished"
            default: title = name
            }
            switch payload["kind"]?.string {
            case "started":
                add("Details", "Another agent was started.")
            case "interacted":
                add("Details", "Another agent received an update.")
            case "interrupted":
                add("Details", "Another agent was stopped.")
            case "completed":
                add("Details", "Another agent finished its work.")
            default:
                add("Details", "An agent task update.")
            }
        case .sleep:
            title = "Wait"; symbol = "clock"
            add("Requested duration", Self.durationLabel(payload["durationMs"]))
        case .enteredReviewMode:
            title = "Start review"; symbol = "checkmark.shield"
            add("Review", payload["review"]?.string)
        case .exitedReviewMode:
            title = "Finish review"; symbol = "checkmark.shield.fill"
            add("Review", payload["review"]?.string)
        case .providerSwitch:
            title = "Switched provider"; symbol = "arrow.left.arrow.right"
        case .taskNotification:
            title = taskNotificationText ?? "Background task finished"; symbol = taskNotificationSymbol
        case .contextCompaction:
            let marker = ContextCompactionPresentation.forState(item.state)
            title = "Context compaction"; symbol = marker.symbol
            // The runtime item may carry a private summary or instruction
            // body. Compaction is represented only by the structural marker.
        default:
            title = "New activity"; symbol = "ellipsis.circle"
            add("Details", "This activity isn’t supported in this version of Cupola, so its details are unavailable.")
        }
        if includeDetails, item.type != "error" && item.type != "contextCompaction" {
            var message = value("error")
            if case .object(let error) = payload["error"], error.count == 1, let text = error["message"]?.string {
                message = text
            }
            if state == .interrupted, message == "Action did not return a confirmed result." {
                message = "Action was cancelled."
            }
            add("Error", message, kind: .error)
        }
        let actionTitle = actions.prefix(3).map { $0.label(state: state) }.joined(separator: " · ")
        let preparedTitle = kind == .fileChange ? fileChangeSummary?.title : nil
        var displayTitle = preparedTitle ?? (actionTitle.isEmpty ? title : actionTitle)
        var toolName: String?
        if [.mcpToolCall, .dynamicToolCall].contains(kind), actions.count == 1,
           actions[0].kind == .tool, actions[0].target == nil,
           let name = ToolDisplayName.title(server: payload["server"]?.string, tool: payload["tool"]?.string) {
            toolName = name
            switch state {
            case .failed: displayTitle = "Couldn’t complete · " + name
            case .interrupted: displayTitle = "Cancelled · " + name
            default: displayTitle = name
            }
        }
        let accessibilityLabel: String
        if kind == .commandExecution, actions.allSatisfy({ $0.kind == .command }), let commandSummary {
            accessibilityLabel = commandSummary.accessibilityLabel
        } else {
            let label = fileChangeSummary?.accessibilityLabel ?? displayTitle
            switch codeErrors.count {
            case 0: accessibilityLabel = label
            case 1: accessibilityLabel = label + ", 1 error in the code"
            default: accessibilityLabel = label + ", \(codeErrors.count) errors in the code"
            }
        }
        var commandPanelText: String?
        if includeDetails, kind == .commandExecution {
            let command = details.first { $0.kind == .command }?.text ?? ""
            let output = details.first { $0.kind == .output }?.text
            commandPanelText = "$ " + command + (output.map { "\n\n" + $0 } ?? "")
        }
        return ActivityPresentation(title: displayTitle, toolName: toolName, symbol: actions.first?.symbol ?? symbol,
                                    state: state, details: details, kind: kind, actions: actions, accessibilityLabel: accessibilityLabel,
                                    commandPanelText: commandPanelText)
    }

    /// Convert the schema's nonnegative integer milliseconds into a short,
    /// human-readable duration. Invalid or unrepresentable values stay hidden.
    /// A Double can represent integers exactly only through 2^53 - 1; values
    /// above that boundary are not safe to present as a claimed duration.
    private static func durationLabel(_ value: ThreadValue?) -> String? {
        guard let rawMilliseconds = value?.number,
              rawMilliseconds.isFinite,
              rawMilliseconds >= 0,
              rawMilliseconds.rounded() == rawMilliseconds,
              rawMilliseconds <= 9_007_199_254_740_991 else { return nil }
        let milliseconds = Int64(rawMilliseconds)
        if milliseconds < 1_000 {
            return quantity(milliseconds, singular: "millisecond")
        }

        var seconds = milliseconds / 1_000
        let remainder = milliseconds % 1_000
        if seconds < 60 {
            if remainder == 0 { return quantity(seconds, singular: "second") }
            let tenths = (remainder + 50) / 100
            if tenths == 10 {
                seconds += 1
                return seconds == 60 ? "1 minute" : quantity(seconds, singular: "second")
            }
            return "\(seconds).\(tenths) seconds"
        }

        let minutes = seconds / 60
        seconds %= 60
        if minutes < 60 {
            return [quantity(minutes, singular: "minute"), seconds > 0 ? quantity(seconds, singular: "second") : nil]
                .compactMap { $0 }.joined(separator: " ")
        }

        let hours = minutes / 60
        let remainingMinutes = minutes % 60
        if hours < 24 {
            return [quantity(hours, singular: "hour"), remainingMinutes > 0 ? quantity(remainingMinutes, singular: "minute") : nil]
                .compactMap { $0 }.joined(separator: " ")
        }

        let days = hours / 24
        let remainingHours = hours % 24
        return [quantity(days, singular: "day"), remainingHours > 0 ? quantity(remainingHours, singular: "hour") : nil]
            .compactMap { $0 }.joined(separator: " ")
    }

    private static func quantity(_ value: Int64, singular: String) -> String {
        "\(value) \(value == 1 ? singular : singular + "s")"
    }
}

// Tool content is a union, not necessarily a printable JSON object. Keep media
// inert until the attachment renderer can validate and display it.
extension ThreadValue {
    var toolOutputText: String {
        switch self {
        case .string(let text): return text
        case .null: return ""
        case .array(let values): return values.map(\.toolOutputText).filter { !$0.isEmpty }.joined(separator: "\n\n")
        case .object(let fields):
            switch fields["type"]?.string {
            case "inputText", "input_text", "output_text", "text": return fields["text"]?.string ?? ""
            case "wonderArtifact": return ""
            case "inputImage", "input_image", "image": return "Image result — preview unavailable"
            case "inputAudio", "input_audio", "audio": return "Audio result — playback unavailable"
            case "resource", "resource_link": return "Resource result — preview unavailable"
            default: break
            }
            if let content = fields["content"] {
                let text = content.toolOutputText
                if !text.isEmpty { return text }
                if let structured = fields["structuredContent"] { return structured.toolOutputText }
                return ""
            }
            return formattedJSON
        default: return formattedJSON
        }
    }
    private var formattedJSON: String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return (try? encoder.encode(self)).flatMap { String(data: $0, encoding: .utf8) } ?? ""
    }
}

extension ReadRow {
    func makeToolFiles() -> [ConversationFile] {
        guard let payload = item?.payload else { return [] }
        return ["result", "contentItems", "output"].flatMap { payload[$0]?.toolFiles ?? [] } + (publishedPage.map { [$0] } ?? [])
    }
    /// The verified page a tool published for the reader (T3 Code's
    /// `html_render`, recognized on the Mac), drawn natively above the reply.
    /// Cupola's own `wonder_show_visual` publishes the same way, as a page
    /// or as a wonder-ui document.
    var publishedPage: ConversationFile? {
        guard case .object(let page) = item?.payload?["htmlRender"], let value = page["file"],
              let bytes = try? JSONEncoder().encode(value),
              let file = try? JSONDecoder().decode(ConversationFile.self, from: bytes),
              ConversationFile.publishedPageTypes.contains(file.mimeType?.lowercased() ?? "") else { return nil }
        return file
    }
    /// The visual this row's visual updates (its file ID), if any.
    var replacedPage: String? {
        guard case .object(let page) = item?.payload?["htmlRender"] else { return nil }
        return page["replaces"]?.string
    }
    /// What a tool made that shows in the conversation without opening its
    /// details: its pictures, video and audio, and a page it published. A
    /// page preview's screenshot is how the agent checked its work, so it
    /// stays in the details (`mediaInDetails`, set on the Mac).
    func makeShownFiles() -> [ConversationFile] {
        if item?.payload?["mediaInDetails"]?.bool == true { return [] }
        let page = publishedPage?.id
        return toolFiles.filter { $0.isVisualMedia || $0.id == page }
    }

    /// A picture or recording a tool showed or made that is in this history
    /// only as the provider recorded it, with no verified file the Mac saved:
    /// what an agent task's transcript holds, since it is read straight from
    /// its provider. Nil when the row has a verified file, failed or made
    /// nothing. Nothing here can be downloaded; it says what there was.
    public var unsavedMedia: UnsavedToolMedia? {
        guard let item, toolFiles.isEmpty, item.state != "failed",
              item.payload?["success"]?.bool != false else { return nil }
        let payload = item.payload ?? [:]
        switch item.type {
        case "imageView":
            return UnsavedToolMedia(kind: .picture, count: 1, name: payload["path"]?.string.flatMap(UnsavedToolMedia.fileName))
        case "imageGeneration":
            guard item.state == "completed" else { return nil }
            return UnsavedToolMedia(kind: .picture, count: 1, name: payload["savedPath"]?.string.flatMap(UnsavedToolMedia.fileName))
        default:
            let parts = ["result", "contentItems", "output"].flatMap { payload[$0]?.mediaPartTypes ?? [] }
            let pictures = parts.filter { ["image", "inputImage", "input_image"].contains($0) }.count
            if pictures > 0 { return UnsavedToolMedia(kind: .picture, count: pictures, name: nil) }
            let recordings = parts.filter { ["audio", "inputAudio", "input_audio"].contains($0) }.count
            if recordings > 0 { return UnsavedToolMedia(kind: .recording, count: recordings, name: nil) }
            return nil
        }
    }
}

/// See `ReadRow.unsavedMedia`.
public struct UnsavedToolMedia: Equatable, Sendable {
    public enum Kind: Sendable { case picture, recording }
    public let kind: Kind
    public let count: Int
    /// The file's name when the provider gives its path on the Mac.
    public let name: String?

    /// The last part of a workspace-relative path. The Mac replaces a path
    /// outside the workspace with a bracketed placeholder, which is no name.
    static func fileName(_ path: String) -> String? {
        let name = path.split(separator: "/").last.map(String.init)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return name.isEmpty || name.hasPrefix("[") || name == "." ? nil : name
    }
}

extension ThreadValue {
    /// The types of the picture and audio parts of a tool result, wherever
    /// they nest in its content.
    var mediaPartTypes: [String] {
        switch self {
        case .array(let values): return values.flatMap(\.mediaPartTypes)
        case .object(let fields):
            if let type = fields["type"]?.string,
               ["image", "inputImage", "input_image", "audio", "inputAudio", "input_audio"].contains(type) { return [type] }
            return ["content", "contentItems"].flatMap { fields[$0]?.mediaPartTypes ?? [] }
        default: return []
        }
    }
}
extension ThreadValue {
    var toolFiles: [ConversationFile] {
        switch self {
        case .array(let values): return values.flatMap(\.toolFiles)
        case .object(let fields):
            if fields["type"]?.string == "wonderArtifact", let value = fields["file"],
               let bytes = try? JSONEncoder().encode(value),
               let file = try? JSONDecoder().decode(ConversationFile.self, from: bytes) { return [file] }
            return ["content", "contentItems"].flatMap { fields[$0]?.toolFiles ?? [] }
        default: return []
        }
    }
}

/// Prepared observed lifecycle time, never turn time or private thinking text.
/// A thought shorter than a second has no time to show.
public struct ThinkingTiming: Sendable {
    public let startedAt: Date
    public let completedLabel: String?
    static let maximumMilliseconds: Double = 86_400_000

    static func prepare(_ item: ReadItem, hostEpoch: String?) -> Self? {
        guard item.type == "reasoning", case .object(let value) = item.payload?["thinkingTiming"],
              let start = value["startedAtMs"]?.number, start.isFinite, (1...9_007_199_254_740_991).contains(start) else { return nil }
        if item.state == "completed", value["finished"]?.bool == true,
           let end = value["completedAtMs"]?.number, let seconds = seconds(end - start) {
            guard seconds > 0 else { return nil }
            return Self(startedAt: Date(timeIntervalSince1970: start / 1000), completedLabel: "Thought for " + duration(seconds))
        }
        guard ActivityState.prepare(item).isRunning, value["finished"]?.bool != true,
              let hostEpoch, value["hostEpoch"]?.string == hostEpoch else { return nil }
        return Self(startedAt: Date(timeIntervalSince1970: start / 1000), completedLabel: nil)
    }
    public func activeLabel(at date: Date) -> String {
        guard completedLabel == nil, let elapsed = Self.seconds(date.timeIntervalSince(startedAt) * 1000), elapsed > 0 else { return "Thinking" }
        return "Thinking · " + Self.duration(elapsed)
    }
    private static func seconds(_ milliseconds: Double) -> Int? {
        guard milliseconds.isFinite, (0...maximumMilliseconds).contains(milliseconds) else { return nil }
        return Int(milliseconds / 1000)
    }
    private static func duration(_ seconds: Int) -> String {
        seconds >= 60 ? "\(seconds / 60)m \(seconds % 60)s" : "\(seconds)s"
    }

    static func observing(_ previous: ThreadValue?, state: String, at event: ReplayEvent) -> ThreadValue {
        let unknown = ThreadValue.object(["finished": .bool(true)])
        let old: [String: ThreadValue]
        if case .object(let value) = previous { old = value } else { old = [:] }
        if old["finished"]?.bool == true { return previous ?? unknown }
        let milliseconds = Int64(event.occurredAt).map(Double.init)
            ?? ReadTimestamp.seconds(event.occurredAt).map { ($0 * 1000).rounded(.down) }
        guard let at = milliseconds, at.isFinite, (1...9_007_199_254_740_991).contains(at),
              event.sequence <= 9_007_199_254_740_991 else { return unknown }
        if state == "started" {
            return previous ?? .object(["startedAtMs": .number(at), "hostEpoch": .string(event.hostEpoch),
                                        "startSequence": .number(Double(event.sequence))])
        }
        guard state == "completed", let start = old["startedAtMs"]?.number,
              old["hostEpoch"]?.string == event.hostEpoch,
              let sequence = old["startSequence"]?.number, sequence < Double(event.sequence),
              seconds(at - start) != nil else { return unknown }
        var result = old
        result["completedAtMs"] = .number(at); result["finished"] = .bool(true)
        return .object(result)
    }
}

/// The complete text of a command or diff panel, a page at a time, for the
/// output reader. Each page is one accessibility element of bounded size, so
/// VoiceOver can read all of it without a row carrying thousands of lines.
/// Pages end after a line where one fits; joined, they are the whole text.
public enum ToolOutputPages {
    /// About 200 lines of typical output; a 64 KB capped output is 7 pages.
    public static let characterLimit = 10_000

    public static func split(_ text: String, limit: Int = characterLimit) -> [String] {
        let limit = max(1, limit)
        var pages: [String] = []
        var start = text.startIndex
        while start < text.endIndex {
            guard let hardEnd = text.index(start, offsetBy: limit, limitedBy: text.endIndex),
                  hardEnd < text.endIndex else {
                pages.append(String(text[start...]))
                break
            }
            let end = text[start..<hardEnd].lastIndex(where: \.isNewline).map { text.index(after: $0) } ?? hardEnd
            pages.append(String(text[start..<end]))
            start = end
        }
        return pages.isEmpty ? [""] : pages
    }
}

/// How an open tool panel takes new text without losing the reader's place:
/// output that only grew restyles from its last line on; anything else is a
/// replacement that keeps a clamped selection and scroll offset.
public enum ToolTextChange: Equatable, Sendable {
    case unchanged
    /// Restyle from this UTF-16 offset: the start of the old text's last line.
    case append(from: Int)
    case replace

    public static func between(_ old: String?, _ new: String, sameStyle: Bool) -> Self {
        guard sameStyle, let old else { return .replace }
        let oldUnits = old.utf16, newUnits = new.utf16
        if oldUnits.count == newUnits.count, oldUnits.elementsEqual(newUnits) { return .unchanged }
        guard newUnits.count > oldUnits.count, newUnits.starts(with: oldUnits) else { return .replace }
        let newline = UInt16(UInt8(ascii: "\n"))
        let lastLine = oldUnits.lastIndex(of: newline).map { oldUnits.distance(from: oldUnits.startIndex, to: $0) + 1 } ?? 0
        return .append(from: lastLine)
    }

    /// A selection that still fits the new text, or nil when nothing was selected.
    public static func clamped(_ selection: NSRange, toLength length: Int) -> NSRange? {
        guard selection.location != NSNotFound, selection.length > 0, selection.location < length else { return nil }
        return NSRange(location: selection.location, length: min(selection.length, length - selection.location))
    }

    /// A vertical offset that still shows text after the content shrank.
    public static func clampedOffset(_ offset: Double, contentHeight: Double, viewportHeight: Double,
                                     topInset: Double = 0, bottomInset: Double = 0) -> Double {
        let maximum = max(-topInset, contentHeight + bottomInset - viewportHeight)
        return min(max(offset, -topInset), maximum)
    }
}
