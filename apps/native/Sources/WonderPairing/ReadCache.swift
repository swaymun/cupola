import Foundation
import CryptoKit

public struct ChatSummary: Codable, Hashable, Identifiable, Sendable {
    public let conversationId: String
    public let title: String
    public let lastMessagePreview: String?
    public let lastMessageAt: String?
    public let messageCount: Int
    public let deliveryState: String?
    public let hasUnread: Bool
    public let isArchived: Bool
    public let isPinned: Bool
    public var id: String { conversationId }
    public init(conversationId: String, title: String, lastMessagePreview: String?, lastMessageAt: String?,
                messageCount: Int, deliveryState: String?, hasUnread: Bool, isArchived: Bool, isPinned: Bool) {
        self.conversationId = conversationId; self.title = title
        self.lastMessagePreview = lastMessagePreview; self.lastMessageAt = lastMessageAt; self.messageCount = messageCount
        self.deliveryState = deliveryState; self.hasUnread = hasUnread; self.isArchived = isArchived; self.isPinned = isPinned
    }
}

/// Who wrote a message when another thread, an automation, or Wonder
/// reporting finished delegated tasks, did instead of the owner.
public struct MessageSource: Codable, Equatable, Sendable {
    /// `thread`, `delegation`, `automation` or `wake`.
    public let kind: String
    public let sourceConversationId: String?
    public let sourceTitle: String
    public init(kind: String, sourceConversationId: String? = nil, sourceTitle: String) {
        self.kind = kind; self.sourceConversationId = sourceConversationId; self.sourceTitle = sourceTitle
    }
    /// A short, user-facing label for a message another thread wrote.
    public var fromLabel: String? {
        switch kind {
        case "wake": nil
        // The Mac titles a deleted automation's prompts "Automation".
        case "automation": sourceTitle == "Automation" ? sourceTitle : "Automation: " + sourceTitle
        default: "From " + sourceTitle
        }
    }
    /// The label's symbol: a clock for a scheduled prompt.
    public var symbolName: String { isAutomation ? "clock" : "arrow.turn.down.right" }
    public var isAutomation: Bool { kind == "automation" }
    /// Wonder's own report that delegated tasks finished; shown as a system row.
    public var isWake: Bool { kind == "wake" }
}

public struct ConversationMessage: Codable, Equatable, Identifiable, Sendable {
    public var clientMessageId: String?
    public var codexTurnId: String?
    public var codexThreadId: String?
    public var originalBodySha256: String?
    public let messageId: String
    public let body: String
    public let state: String
    public let createdAt: String
    public let attachmentIds: [String]
    public var source: MessageSource?
    /// Why the Mac could not deliver it, in plain language.
    public var deliveryError: String?
    public var id: String { messageId }
    /// The message never reached the agent: the Mac refused or could not send it.
    public var undelivered: UndeliveredMessage? {
        guard ["safe_to_retry", "failed"].contains(state), codexTurnId?.isEmpty != false else { return nil }
        return UndeliveredMessage(messageId: messageId, reason: deliveryError)
    }
    /// Removing queued work cancels the local message before any runtime saw it.
    public var wasCancelledBeforeDispatch: Bool {
        state == "interrupted" && (codexTurnId?.isEmpty ?? true) && (codexThreadId?.isEmpty ?? true)
    }
}

/// A message of the owner's that the Mac did not deliver, shown on its bubble
/// with the reason and a Retry.
public struct UndeliveredMessage: Equatable, Sendable {
    public let messageId: String
    public let reason: String?
    public init(messageId: String, reason: String?) { self.messageId = messageId; self.reason = reason }
    /// The computer's reason, if it gave one; "Not sent" alone says the rest.
    public var explanation: String? {
        guard let reason = reason?.trimmingCharacters(in: .whitespacesAndNewlines), !reason.isEmpty else { return nil }
        return reason
    }
}

public struct AssistantMessage: Codable, Equatable, Identifiable, Sendable {
    public var itemId: String?
    public var codexTurnId: String?
    public let messageId: String
    /// Live events grow and settle a message in place (`ConversationSnapshot.applyLive`).
    public internal(set) var text: String
    public internal(set) var state: String
    public let createdAt: String
    public internal(set) var updatedAt: String
    public var id: String { messageId }
    public var rowId: String {
        if let codexTurnId, let itemId { return codexTurnId + "/" + itemId }
        return "assistant-" + messageId
    }
}

public struct ConversationSnapshot: Codable, Equatable, Sendable, ThreadJSONDecodable {
    public init(
        conversationId: String,
        hostEpoch: String,
        lastSequence: UInt64,
        historyRevision: UInt64? = nil,
        messages: [ConversationMessage],
        assistantMessages: [AssistantMessage],
        thread: ThreadProjection,
        // Required: a copy that forgot the records would split a T3 Code
        // message into two bubbles on the next merge or live update.
        messageRecords: [MessageRecord]?
    ) {
        self.conversationId = conversationId
        self.hostEpoch = hostEpoch
        self.lastSequence = lastSequence
        self.historyRevision = historyRevision
        self.messages = messages
        self.assistantMessages = assistantMessages
        self.thread = thread
        self.messageRecords = messageRecords
    }


    /// Local placeholders retain queued/unsent messages, but cannot supersede
    /// the newest actual runtime turn after Guide, queue cancellation, or replay.
    private var newestRuntimeTurn: ReadTurn? {
        thread.turns?.last(where: { !$0.id.hasPrefix("local:") })
    }
    /// Whether this projection already ends with the newest native turn the
    /// Mac reports, settled, so a send needs no full history re-read. Any
    /// doubt (a different, missing or running turn) means re-read first.
    public func showsLatestNativeTurn(id: String?, status: String, runningElsewhere: Bool) -> Bool {
        guard let id, let shown = newestRuntimeTurn, shown.id == id, !runningElsewhere,
              ["completed", "failed", "interrupted"].contains(status) else { return false }
        return shown.status == status
    }
    public var activeTurnIDs: Set<String> {
        activeTurnID.map { [$0] } ?? []
    }
    public var activeTurnID: String? {
        guard let newest = newestRuntimeTurn, newest.isInProgress else { return nil }
        return newest.id
    }
    /// The active turn is running in a desktop or terminal app on the Mac.
    public var activeTurnRunsElsewhere: Bool {
        activeTurnID != nil && newestRuntimeTurn?.runningElsewhere == true
    }
    /// The Mac can hand a message to Codex's queue while an earlier reply is
    /// still running. Keep its explanation until every uncertain message has
    /// a turn, even if a stale Project detail still carries the notice.
    public func pendingDeliveryNotice(_ notice: String?) -> String? {
        guard let notice = notice?.trimmingCharacters(in: .whitespacesAndNewlines), !notice.isEmpty,
              messages.contains(where: {
                  $0.state == "uncertain" && ($0.codexTurnId?.isEmpty ?? true)
              }) else { return nil }
        return notice
    }
    /// The Mac's line for a conversation whose agent hasn't started yet (a
    /// thread starting in T3 Code: "Starting in T3 Code…"): shown while it
    /// has no session and no reply, so a stale detail can't outlast the reply.
    public static func startingNotice(_ notice: String?, hasNativeSession: Bool, snapshot: ConversationSnapshot?) -> String? {
        guard !hasNativeSession, snapshot?.newestRuntimeTurn == nil,
              let notice = notice?.trimmingCharacters(in: .whitespacesAndNewlines), !notice.isEmpty else { return nil }
        return notice
    }
    /// A short line under the conversation when the last message did not get
    /// its reply. A stopped reply says so in its own activity row, and a
    /// message the Mac never delivered says so on its bubble with Retry
    /// (`undelivered`), so neither has one.
    public var latestRequestIssue: String? {
        guard activeTurnID == nil, !hasUnassignedPreTurnWork,
              let message = messages.last(where: { !$0.wasCancelledBeforeDispatch }) else { return nil }
        if message.codexTurnId == newestRuntimeTurn?.id, newestRuntimeTurn?.status == "completed" { return nil }
        if message.state == "uncertain" {
            // The Mac lost the agent mid-reply: the turn was delivered and failed.
            if message.codexTurnId == newestRuntimeTurn?.id, newestRuntimeTurn?.status == "failed" {
                return "The last reply couldn’t finish."
            }
            return "Your last message may not have been delivered. Check this thread on your computer before sending it again."
        }
        guard ["safe_to_retry", "failed"].contains(message.state), message.undelivered == nil else { return nil }
        return "The last reply couldn’t finish."
    }
    /// A message can be accepted before the daemon assigns it to a turn. It
    /// still occupies the composer until dispatch resolves, but it cannot
    /// identify a turn for Guide or Stop.
    public var hasUnassignedPreTurnWork: Bool {
        messages.contains { message in
            (message.codexTurnId == nil || message.codexTurnId?.isEmpty == true)
                && ["accepted_by_wonder", "dispatching_to_codex"].contains(message.state)
        }
    }
    /// Merge an older canonical page, or a cached projection during refresh.
    /// Turn statuses and items remain historical data; actionable state is
    /// derived separately from the newest chronological turn. The newer
    /// read's message records label the other's saved items too, so a
    /// record saved before the Mac knew it as a sent message joins that
    /// message's bubble on the next read, also outside its page.
    public func mergingOlder(_ older: Self) throws -> Self {
        guard conversationId == older.conversationId, hostEpoch == older.hostEpoch else { throw ReadFailure.resync }
        guard (historyRevision ?? 0) >= (older.historyRevision ?? 0) else { throw ReadFailure.resync }
        if (historyRevision ?? 0) != (older.historyRevision ?? 0) { return self }
        let records = (older.lastSequence > lastSequence ? older.messageRecords : nil) ?? messageRecords ?? older.messageRecords
        return Self(conversationId: conversationId, hostEpoch: hostEpoch, lastSequence: lastSequence, historyRevision: historyRevision,
                    messages: Self.unique(older.messages, messages, key: \.messageId),
                    assistantMessages: Self.unique(older.assistantMessages, assistantMessages, key: \.messageId),
                    thread: thread.mergingOlder(older.thread).labeling(records ?? []), messageRecords: records)
    }
    /// The newest page over the saved chat, published before its unfinished
    /// older items are re-read. It keeps this page's cursor, so a failed
    /// older read can't hide the unfilled gap.
    public func mergingOlderKeepingCursor(_ cached: Self) throws -> Self {
        try mergingOlder(cached).replacingCursor(thread.nextCursor)
    }
    /// This newest page over the saved chat. The saved copy's paging cursor
    /// is kept only where this page reaches back into it (they share a turn);
    /// otherwise the turns between them were never read, and this page's
    /// cursor still leads to them, so paging older fills the gap.
    public func mergingSaved(_ saved: Self) throws -> Self {
        let merged = try mergingOlder(saved)
        guard let cursor = thread.nextCursor, let oldest = thread.turns?.first?.id,
              !(saved.thread.turns ?? []).contains(where: { $0.id == oldest }) else { return merged }
        return merged.replacingCursor(cursor)
    }
    /// Turns this copy already holds, updated from `repaired`: older pages
    /// re-read out of order to settle unfinished items. Turns it does not
    /// hold, turn order and the paging cursor stay this copy's.
    public func updatingKnownTurns(from repaired: Self) -> Self {
        guard conversationId == repaired.conversationId, hostEpoch == repaired.hostEpoch,
              (historyRevision ?? 0) == (repaired.historyRevision ?? 0), let turns = thread.turns else { return self }
        let fresh = Dictionary((repaired.thread.turns ?? []).map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
        guard turns.contains(where: { fresh[$0.id] != nil }) else { return self }
        let updated = turns.map { turn -> ReadTurn in
            guard let newer = fresh[turn.id] else { return turn }
            let replacements = Dictionary(newer.items.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
            let known = Set(turn.items.map(\.id))
            let items = turn.items.map { replacements[$0.id] ?? $0 } + newer.items.filter { !known.contains($0.id) }
            return ReadTurn(id: turn.id, items: items, startedAt: newer.startedAt ?? turn.startedAt,
                            completedAt: newer.completedAt ?? turn.completedAt, status: newer.status,
                            runningElsewhere: newer.runningElsewhere, editedFiles: newer.editedFiles ?? turn.editedFiles)
        }
        return Self(conversationId: conversationId, hostEpoch: hostEpoch, lastSequence: lastSequence, historyRevision: historyRevision,
                    messages: messages, assistantMessages: assistantMessages,
                    thread: ThreadProjection(threadId: thread.threadId, nextCursor: thread.nextCursor, hydrated: thread.hydrated,
                                             turns: updated).labeling(messageRecords ?? []),
                    messageRecords: messageRecords)
    }
    private func replacingCursor(_ cursor: String?) -> Self {
        Self(conversationId: conversationId, hostEpoch: hostEpoch, lastSequence: lastSequence,
             historyRevision: historyRevision, messages: messages, assistantMessages: assistantMessages,
             thread: ThreadProjection(threadId: thread.threadId, nextCursor: cursor, hydrated: thread.hydrated, turns: thread.turns),
             messageRecords: messageRecords)
    }
    private static func unique<T>(_ older: [T], _ newer: [T], key: KeyPath<T, String>) -> [T] {
        let ids = Set(newer.map { $0[keyPath: key] })
        return older.filter { !ids.contains($0[keyPath: key]) } + newer
    }
    /// Saved items still shown as running ("turn/item") that a refresh should
    /// settle. A finished turn's too: the turn no longer runs, but the item
    /// still lacks its final state and output. The walk that fetches them is
    /// bounded per refresh and goes on across refreshes.
    public var unsettledItemIDs: Set<String> {
        Set((thread.turns ?? []).flatMap { turn in
            turn.items.filter { ["started", "streaming", "waiting"].contains($0.state) }.map { turn.id + "/" + $0.id }
        })
    }

    public func rows(author: String) -> [ReadRow] {
        var reuse = PreparedRowReuse()
        return rows(author: author, reusing: &reuse)
    }
    /// The same rows, taking each one `reuse` prepared from identical inputs
    /// instead of preparing it again (command summaries, files, activity).
    func rows(author: String, reusing reuse: inout PreparedRowReuse) -> [ReadRow] {
        defer { reuse.finish() }
        if let turns = thread.turns, !turns.isEmpty {
            let canonical = CanonicalMessages(messages)
            let rows: [ReadRow] = turns.flatMap { (turn: ReadTurn) -> [ReadRow] in
                let userCount = turn.items.filter { $0.type == "userMessage" }.count
                return turn.items.compactMap { (item: ReadItem) -> ReadRow? in
                    // An approval shows as its own card while it waits; once
                    // answered it is not a message in the conversation.
                    if item.type == "approval" || item.type == "promptSuggestion" { return nil }
                    let user = item.type == "userMessage"
                    let message = user || item.type == "agentMessage"
                    let text: String
                    switch item.type {
                    case "agentMessage" where turn.isInProgress && ["started", "streaming"].contains(item.state):
                        // A started reply can reach the phone before its first token.
                        text = item.text ?? ""
                    case "userMessage", "agentMessage": text = item.text ?? "Message content unavailable"
                    default: text = Self.activityText(item)
                    }
                    let durable = user ? canonical.message(for: item, turnID: turn.id, userCount: userCount) : nil
                    if durable?.wasCancelledBeforeDispatch == true { return nil }
                    let clientId = durable?.clientMessageId ?? item.payload?["clientId"]?.string
                    let identity = user ? "user-" + (clientId ?? durable?.messageId ?? item.id) : turn.id + "/" + item.id
                    let speaker: String = user ? "You" : (message ? author : "Activity")
                    // A prompt written on the Mac has no Wonder message; the
                    // host lists the images it attached on the item itself.
                    let attachments = durable?.attachmentIds
                        ?? (user ? item.payload?["attachmentIds"]?.array?.compactMap(\.string) : nil) ?? []
                    // Keyed by turn as well: providers can repeat item IDs across turns.
                    return reuse.row(turn.id + "\u{1F}" + identity, PreparedRowReuse.Inputs(author: speaker,
                                   text: durable?.body ?? text, isUser: user, timestamp: durable?.createdAt ?? item.createdAt,
                                   turnId: turn.id, item: item, attachmentIds: attachments, source: durable?.source,
                                   undelivered: durable?.undelivered, hostEpoch: hostEpoch), id: identity)
                }
            }
            // Live projections can precede thread hydration. Merge by identity so
            // accepted sends and in-progress assistant text remain visible. A
            // delivered message keeps its turn (Edit from here rewinds to it).
            let userIDs = Set(rows.filter(\.isUser).map(\.id))
            let missingUsers = messages.filter { !$0.wasCancelledBeforeDispatch && !userIDs.contains("user-" + ($0.clientMessageId ?? $0.messageId)) }.map {
                ReadRow(id: "user-" + ($0.clientMessageId ?? $0.messageId), author: "You", text: $0.body, isUser: true, timestamp: $0.createdAt, turnId: $0.codexTurnId, attachmentIds: $0.attachmentIds, source: $0.source, undelivered: $0.undelivered)
            }
            var merged = rows
            var assistantRows = AssistantRowIndices(rows)
            for message in assistantMessages {
                if let index = assistantRows.index(for: message) {
                    let old = merged[index]
                    merged[index] = reuse.row("reply\u{1F}" + old.id, PreparedRowReuse.Inputs(author: author, text: message.text,
                        isUser: false, timestamp: old.timestamp, turnId: old.turnId, item: old.item), id: old.id)
                } else {
                    assistantRows.insert(id: message.rowId, at: merged.count)
                    merged.append(ReadRow(id: message.rowId, author: author, text: message.text, isUser: false, timestamp: message.createdAt, turnId: message.codexTurnId))
                }
            }
            var seen = Set<String>()
            let visible = (merged + missingUsers).filter { row in
                let emptyReply = !row.isUser && row.nativeQuestion == nil
                    && (row.item == nil || row.item?.type == "agentMessage")
                    && row.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    && row.toolFiles.isEmpty
                return !emptyReply && seen.insert(row.id).inserted
            }.sorted { $0.time < $1.time }
            return ReadRow.identifyingSpeakers(ReadRow.reconcilingQuestions(visible), fallback: author)
        }
        return (messages.filter { !$0.wasCancelledBeforeDispatch }.map { ReadRow(id: "user-" + ($0.clientMessageId ?? $0.messageId), author: "You", text: $0.body, isUser: true, timestamp: $0.createdAt, turnId: $0.codexTurnId, attachmentIds: $0.attachmentIds, source: $0.source, undelivered: $0.undelivered) }
         + assistantMessages.filter { !$0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }.map { ReadRow(id: $0.rowId, author: author, text: $0.text, isUser: false, timestamp: $0.createdAt) })
        .sorted { $0.time < $1.time }
    }

    /// Items that show as activity rows: everything but messages, approvals,
    /// suggestions and provider switches (which also set who speaks next).
    static func isActivityItem(_ item: ReadItem) -> Bool {
        !["userMessage", "agentMessage", "approval", "promptSuggestion", "providerSwitch"].contains(item.type)
    }
    static func activityText(_ item: ReadItem) -> String {
        switch item.type {
        case "error": item.text ?? "This work could not be completed."
        case "unknown": "This item needs a newer version of Cupola."
        default: "Activity · " + (item.state == "completed" ? "Completed" : "Saved")
        }
    }

    /// Preserve the first canonical match and the legacy unambiguous-turn
    /// fallback, without searching all durable messages for every user item.
    private struct CanonicalMessages {
        var clients: [String: ConversationMessage] = [:]
        var ids: [String: ConversationMessage] = [:]
        var turns: [String: [ConversationMessage]] = [:]
        init(_ messages: [ConversationMessage]) {
            for message in messages {
                if let client = message.clientMessageId, clients[client] == nil { clients[client] = message }
                if ids[message.messageId] == nil { ids[message.messageId] = message }
                if let turn = message.codexTurnId { turns[turn, default: []].append(message) }
            }
        }
        func message(for item: ReadItem, turnID: String, userCount: Int) -> ConversationMessage? {
            if let client = item.payload?["clientId"]?.string, let message = clients[client] { return message }
            if let message = ids[item.id] { return message }
            let candidates = turns[turnID] ?? []
            if candidates.count == 1, userCount == 1, item.payload?["clientId"]?.string == nil { return candidates[0] }
            return nil
        }
    }

    /// Both exact identity and old hosts' item-only suffix matching choose the
    /// first non-user row, including rows appended by earlier assistant messages.
    private struct AssistantRowIndices {
        var ids: [String: Int] = [:]
        var suffixes: [String: Int] = [:]
        init(_ rows: [ReadRow]) {
            for (index, row) in rows.enumerated() where !row.isUser { insert(id: row.id, at: index) }
        }
        mutating func insert(id: String, at index: Int) {
            if ids[id] == nil { ids[id] = index }
            for slash in id.indices where id[slash] == "/" {
                let suffix = String(id[id.index(after: slash)...])
                if suffixes[suffix] == nil { suffixes[suffix] = index }
            }
        }
        func index(for message: AssistantMessage) -> Int? {
            guard let item = message.itemId else { return nil }
            let exact = ids[message.rowId]
            guard message.codexTurnId == nil, let suffix = suffixes[item] else { return exact }
            return min(exact ?? suffix, suffix)
        }
    }

    public let conversationId: String
    public let hostEpoch: String
    /// Live events move the sequence and change one element in place (see `LiveEvents.swift`).
    public internal(set) var lastSequence: UInt64
    public let historyRevision: UInt64?
    public let messages: [ConversationMessage]
    public internal(set) var assistantMessages: [AssistantMessage]
    public internal(set) var thread: ThreadProjection
    /// Every agent record of a message sent from here that T3 Code ran, as
    /// the Mac knew at this read, also outside its page, plus those named by
    /// later reads and live news (absent from older Macs). Every merge and
    /// live update labels the saved items with them, so a page or update
    /// that brings a record unlabelled can't split its bubble again.
    public internal(set) var messageRecords: [MessageRecord]?
    /// What follows this value but is not part of it: which snapshot it descends
    /// from, how many live events it has taken, and where they look things up.
    var live = LiveMeta()

    private enum CodingKeys: String, CodingKey {
        case conversationId, hostEpoch, lastSequence, historyRevision, messages, assistantMessages, thread, messageRecords
    }
}

/// Identity and lookup state for the live path. Never compared or saved: two
/// snapshots with the same content are equal whatever their history.
struct LiveMeta: Equatable, Sendable {
    /// Shared by a snapshot and the copies that live events change; a new value
    /// (a read, a merge) starts its own.
    var lineage = UInt64.random(in: .min ... .max)
    /// Live changes applied since the value was made. A row projection that
    /// holds revision N and is told of K changes may take a snapshot at N+K.
    var revision: UInt64 = 0
    var index: LiveIndex?
    static func == (_: Self, _: Self) -> Bool { true }
}

/// An agent's record of a message sent from here that T3 Code ran: the
/// `userMessage` item `itemId` of turn `turnId` is the message `clientId`'s
/// bubble.
public struct MessageRecord: Codable, Equatable, Sendable {
    public let turnId: String
    public let itemId: String
    public let clientId: String
    public init(turnId: String, itemId: String, clientId: String) {
        self.turnId = turnId; self.itemId = itemId; self.clientId = clientId
    }
    /// Both lists, one entry per record: `newer`'s where they disagree.
    static func merging(_ newer: [Self]?, over older: [Self]?) -> [Self]? {
        guard let older, !older.isEmpty else { return newer }
        guard let newer, !newer.isEmpty else { return older }
        let named = Set(newer.map { [$0.turnId, $0.itemId] })
        return older.filter { !named.contains([$0.turnId, $0.itemId]) } + newer
    }
}

/// Rows prepared by the previous projection, by identity and with the inputs
/// they were prepared from. A projection takes a row whose inputs are equal
/// instead of preparing it again, so an event that changes one item prepares
/// one row, not the whole history. It keeps only the last projection's rows.
struct PreparedRowReuse: Sendable {
    struct Inputs: Equatable, Sendable {
        let author: String
        let text: String
        let isUser: Bool
        let timestamp: String
        var turnId: String? = nil
        var item: ReadItem? = nil
        var attachmentIds: [String] = []
        var source: MessageSource? = nil
        var undelivered: UndeliveredMessage? = nil
        var hostEpoch: String? = nil
    }
    private var previous: [String: (inputs: Inputs, row: ReadRow)] = [:]
    private var current: [String: (inputs: Inputs, row: ReadRow)] = [:]
    private var prepared = 0
    /// Rows the last projection prepared rather than reused.
    private(set) var lastPrepared = 0

    mutating func row(_ key: String, _ inputs: Inputs, id: String? = nil) -> ReadRow {
        if let kept = current[key] ?? previous[key], kept.inputs == inputs {
            current[key] = kept
            return kept.row
        }
        prepared += 1
        let row = ReadRow(id: id ?? key, author: inputs.author, text: inputs.text, isUser: inputs.isUser,
                          timestamp: inputs.timestamp, turnId: inputs.turnId, item: inputs.item,
                          attachmentIds: inputs.attachmentIds, source: inputs.source,
                          undelivered: inputs.undelivered, hostEpoch: inputs.hostEpoch)
        current[key] = (inputs, row)
        return row
    }
    /// Ends a projection: its rows are the ones the next projection may take.
    mutating func finish() {
        previous = current
        current = [:]
        lastPrepared = prepared
        prepared = 0
    }
}

/// One conversation's prepared rows, bounded by input entries and estimated input bytes.
///
/// A live event changes one message or item, and says so (`LiveChange`). When
/// the snapshot is exactly the retained one plus those changes, only the rows
/// they name are prepared again, in place: a reply's text, an activity row that
/// keeps its place in time, or an activity row after the last one. Anything
/// else (a new turn, a user message, a question, equal timestamps, the first
/// token of a reply) uses the complete projection, which also does the sorting
/// and the questions. Both give the same rows.
public struct ConversationRowProjection: Sendable {
    private struct Retained: Sendable {
        let conversationId: String
        let hostEpoch: String
        let historyRevision: UInt64?
        let lineage: UInt64
        var revision: UInt64
        let author: String
        var rows: [ReadRow]
        /// Row ID to position, for the rows an assistant message may overlay.
        var slots: [String: Int]
        var entries: Int
        var inputBytes: Int

        /// Whether `snapshot` is this snapshot's history, perhaps with live changes since.
        func continues(_ snapshot: ConversationSnapshot, author: String) -> Bool {
            self.author == author && conversationId == snapshot.conversationId && hostEpoch == snapshot.hostEpoch
                && historyRevision == snapshot.historyRevision && lineage == snapshot.live.lineage
        }
    }
    private var retained: Retained?
    /// The retained input size, in this cache's own measure; a fresh projection of the same snapshot gives the same number.
    var retainedInputBytes: Int? { retained?.inputBytes }
    private var reuse = PreparedRowReuse()
    private var livePrepared: Int?
    /// Rows the last projection prepared rather than reused.
    public var lastPreparedRowCount: Int { livePrepared ?? reuse.lastPrepared }
    private let maximumEntries: Int
    private let maximumInputBytes: Int

    public init(maximumEntries: Int = 20_000, maximumInputBytes: Int = 16 * 1_024 * 1_024) {
        self.maximumEntries = max(0, maximumEntries)
        self.maximumInputBytes = max(0, maximumInputBytes)
    }

    /// The rows for `snapshot`. `changes` are the live changes applied to it
    /// since the last call, in order; without them (a read, a merge) only an
    /// identical snapshot keeps its rows.
    public mutating func rows(for snapshot: ConversationSnapshot, author: String, changes: [LiveChange] = []) -> [ReadRow] {
        // Asked of `retained` in place: a second reference to its rows would
        // make the first in-place refresh copy all of them.
        if retained?.continues(snapshot, author: author) == true {
            if retained!.revision == snapshot.live.revision, changes.isEmpty { livePrepared = 0; return retained!.rows }
            if !changes.isEmpty, retained!.revision &+ UInt64(changes.count) == snapshot.live.revision,
               refresh(changes, from: snapshot) {
                return retained!.rows
            }
        }
        livePrepared = nil
        let rows = snapshot.rows(author: author, reusing: &reuse)
        let size = Self.size(snapshot, author: author, rowCount: rows.count)
        guard size.entries <= maximumEntries, size.bytes <= maximumInputBytes else {
            retained = nil
            reuse = PreparedRowReuse()
            return rows
        }
        var slots = [String: Int](minimumCapacity: rows.count)
        for (position, row) in rows.enumerated() where !row.isUser && slots[row.id] == nil { slots[row.id] = position }
        retained = Retained(conversationId: snapshot.conversationId, hostEpoch: snapshot.hostEpoch,
            historyRevision: snapshot.historyRevision, lineage: snapshot.live.lineage, revision: snapshot.live.revision,
            author: author, rows: rows, slots: slots, entries: size.entries, inputBytes: size.bytes)
        return rows
    }

    /// Prepares again the rows `changes` name, in the retained rows. False when
    /// some change needs the complete projection; the retained rows are then
    /// partly updated and must be replaced by it.
    private mutating func refresh(_ changes: [LiveChange], from snapshot: ConversationSnapshot) -> Bool {
        guard retained != nil else { return false }
        var prepared = 0
        for change in changes {
            switch change.kind {
            case .sequence: continue
            case .structural: return false
            case .reply(let slot, _):
                guard snapshot.assistantMessages.indices.contains(slot) else { return false }
                let message = snapshot.assistantMessages[slot]
                // Only an exact turn and item identity: legacy suffix matches keep the general path.
                guard message.itemId != nil, message.codexTurnId != nil, let at = retained!.slots[message.rowId] else { return false }
                let row = retained!.rows[at]
                guard !row.isUser, row.nativeQuestion == nil, row.item == nil || row.item?.type == "agentMessage" else { return false }
                let replacement = ReadRow(id: row.id, author: row.author, text: message.text, isUser: false,
                                          timestamp: row.timestamp, turnId: row.turnId, item: row.item)
                // This cache's own measure (`size`), not the snapshot's resident estimate: only the text moves.
                retained!.inputBytes += message.text.utf8.count - row.text.utf8.count
                guard retained!.inputBytes <= maximumInputBytes else { return false }
                // First-token/empty transitions may insert/remove a row and change
                // stable timestamp ties, so the complete projection orders them.
                guard !replacement.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !replacement.toolFiles.isEmpty else { return false }
                retained!.rows[at] = replacement
                prepared += 1
            case .item(let turnSlot, let slot, _):
                guard let turns = snapshot.thread.turns, turns.indices.contains(turnSlot), turns[turnSlot].items.indices.contains(slot) else { return false }
                let turn = turns[turnSlot], item = turn.items[slot]
                guard ConversationSnapshot.isActivityItem(item) else { return false }
                let identity = turn.id + "/" + item.id
                let row = ReadRow(id: identity, author: "Activity", text: ConversationSnapshot.activityText(item), isUser: false,
                                  timestamp: item.createdAt, turnId: turn.id, item: item, hostEpoch: snapshot.hostEpoch)
                guard row.nativeQuestion == nil else { return false }
                if let at = retained!.slots[identity] {
                    // Same place in time: the sort puts it where it was.
                    guard retained!.rows[at].nativeQuestion == nil, retained!.rows[at].time == row.time else { return false }
                    retained!.inputBytes += Self.bytes(item) - (retained!.rows[at].item.map(Self.bytes) ?? 0)
                    guard retained!.inputBytes <= maximumInputBytes else { return false }
                    retained!.rows[at] = row
                } else {
                    // Later than every row: the sort puts it last, whatever the order of ties.
                    guard let last = retained!.rows.last, last.time < row.time else { return false }
                    retained!.rows.append(row)
                    retained!.slots[identity] = retained!.rows.count - 1
                    retained!.entries += 1
                    retained!.inputBytes += Self.bytes(item)
                    guard retained!.entries <= maximumEntries, retained!.inputBytes <= maximumInputBytes else { return false }
                }
                prepared += 1
            }
        }
        retained!.revision = snapshot.live.revision
        livePrepared = prepared
        return true
    }

    private static func bytes(_ values: String?...) -> Int { values.reduce(0) { $0 + 16 + ($1?.utf8.count ?? 0) } }
    private static func bytes(_ message: AssistantMessage) -> Int {
        bytes(message.itemId, message.codexTurnId, message.messageId, message.text,
              message.state, message.createdAt, message.updatedAt)
    }
    private static func bytes(_ item: ReadItem) -> Int {
        bytes(item.id, item.type, item.state, item.text, item.createdAt) + bytes(item.payload)
    }
    private static func bytes(_ object: [String: ThreadValue]?) -> Int {
        (object ?? [:]).reduce(16) { $0 + 32 + $1.key.utf8.count + bytes($1.value) }
    }
    private static func bytes(_ value: ThreadValue) -> Int {
        switch value {
        case .string(let text): 16 + text.utf8.count
        case .array(let values): values.reduce(16) { $0 + bytes($1) }
        case .object(let object): bytes(object)
        default: 16
        }
    }
    private static func size(_ snapshot: ConversationSnapshot, author: String, rowCount: Int) -> (entries: Int, bytes: Int) {
        var entries = rowCount + snapshot.messages.count + snapshot.assistantMessages.count
        var count = bytes(author, snapshot.conversationId, snapshot.hostEpoch, snapshot.thread.threadId, snapshot.thread.nextCursor)
        for message in snapshot.messages {
            count += bytes(message.clientMessageId, message.codexTurnId, message.codexThreadId, message.originalBodySha256,
                message.messageId, message.body, message.state, message.createdAt, message.deliveryError,
                message.source?.kind, message.source?.sourceConversationId, message.source?.sourceTitle)
            count += message.attachmentIds.reduce(0) { $0 + 16 + $1.utf8.count }
        }
        count += snapshot.assistantMessages.reduce(0) { $0 + bytes($1) }
        for turn in snapshot.thread.turns ?? [] {
            entries += 1 + turn.items.count
            count += bytes(turn.id, turn.status, turn.startedAt, turn.completedAt) + bytes(turn.editedFiles)
            for item in turn.items { count += bytes(item) }
        }
        return (entries, count)
    }
}

public struct ThreadProjection: Codable, Equatable, Sendable {
    public let threadId: String?
    public let nextCursor: String?
    public let hydrated: Bool
    public internal(set) var turns: [ReadTurn]?
    public init(threadId: String? = nil, nextCursor: String?, hydrated: Bool, turns: [ReadTurn]? = nil) {
        self.threadId = threadId; self.nextCursor = nextCursor; self.hydrated = hydrated; self.turns = turns
    }
    func mergingOlder(_ older: Self) -> Self {
        let order = (older.turns ?? []).map(\.id) + (turns ?? []).map(\.id).filter { id in !(older.turns ?? []).contains { $0.id == id } }
        var merged = Dictionary((older.turns ?? []).map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
        for turn in turns ?? [] {
            let newest = Set(turn.items.map(\.id))
            let items = (merged[turn.id]?.items ?? []).filter { !newest.contains($0.id) } + turn.items
            merged[turn.id] = ReadTurn(id: turn.id, items: items,
                                       startedAt: turn.startedAt ?? merged[turn.id]?.startedAt,
                                       completedAt: turn.completedAt ?? merged[turn.id]?.completedAt,
                                       status: turn.status, runningElsewhere: turn.runningElsewhere,
                                       editedFiles: turn.editedFiles ?? merged[turn.id]?.editedFiles)
        }
        return Self(threadId: threadId ?? older.threadId, nextCursor: older.nextCursor, hydrated: hydrated, turns: merged.isEmpty ? nil : order.compactMap { merged[$0] })
    }
    /// This thread with each saved item `records` names carrying its
    /// message's client ID, so the two show as one bubble. Items not saved
    /// here are named by the reads that bring them.
    /// `onlyTurn` limits the work to the one turn a live item update changed;
    /// the others were labelled when they arrived. Unchanged turns and items
    /// are returned as they are, so a live update copies nothing it did not touch.
    func labeling(_ records: [MessageRecord], onlyTurn: String? = nil) -> Self {
        guard !records.isEmpty, var turns else { return self }
        var named = [String: [String: String]]()
        for record in records where onlyTurn == nil || record.turnId == onlyTurn {
            named[record.turnId, default: [:]][record.itemId] = record.clientId
        }
        guard !named.isEmpty else { return self }
        var changed = false
        for index in turns.indices {
            let turn = turns[index]
            guard let clients = named[turn.id] else { continue }
            var items = turn.items
            var labelled = false
            for position in items.indices {
                let item = items[position]
                guard item.type == "userMessage", let client = clients[item.id],
                      item.payload?["clientId"]?.string != client else { continue }
                var copy = item
                copy.payload = (item.payload ?? [:]).merging(["clientId": .string(client)]) { _, named in named }
                items[position] = copy
                labelled = true
            }
            guard labelled else { continue }
            turns[index] = ReadTurn(id: turn.id, items: items, startedAt: turn.startedAt, completedAt: turn.completedAt,
                                    status: turn.status, runningElsewhere: turn.runningElsewhere, editedFiles: turn.editedFiles)
            changed = true
        }
        return changed ? Self(threadId: threadId, nextCursor: nextCursor, hydrated: hydrated, turns: turns) : self
    }
}

public struct ReadTurn: Codable, Equatable, Sendable {
    public let id: String
    public internal(set) var items: [ReadItem]
    public var startedAt: String? = nil
    public var completedAt: String? = nil
    /// The App Server turn lifecycle. This is the authority for whether work
    /// is active; item or message delivery states can arrive late during a
    /// steer, stop, reconnect, or replay. Only an authoritative read or a live
    /// event that observes the turn begin (`LiveEvents.write`) assigns it; the
    /// host's turn end arrives as `message_state`, which forces a read.
    public let status: String
    /// Running in a desktop or terminal app on the Mac. Wonder shows it but
    /// cannot steer, stop, or add a turn until it finishes.
    public var runningElsewhere = false
    /// Every file the turn changed, from the agent's whole-turn diff, shaped
    /// like a `fileChange` payload. Absent when the Mac did not observe one.
    public var editedFiles: [String: ThreadValue]? = nil
    public init(id: String, items: [ReadItem], startedAt: String? = nil, completedAt: String? = nil, status: String = "unknown",
                runningElsewhere: Bool = false, editedFiles: [String: ThreadValue]? = nil) {
        self.id = id
        self.items = items
        self.startedAt = startedAt
        self.completedAt = completedAt
        self.status = status
        self.runningElsewhere = runningElsewhere
        self.editedFiles = editedFiles
    }
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        items = try container.decode([ReadItem].self, forKey: .items)
        startedAt = try container.decodeIfPresent(String.self, forKey: .startedAt)
        completedAt = try container.decodeIfPresent(String.self, forKey: .completedAt)
        // Pre-status caches remain readable, but are deliberately treated as
        // unknown instead of being inferred from stale delivery receipts.
        status = try container.decodeIfPresent(String.self, forKey: .status) ?? "unknown"
        runningElsewhere = try container.decodeIfPresent(Bool.self, forKey: .runningElsewhere) ?? false
        editedFiles = try container.decodeIfPresent([String: ThreadValue].self, forKey: .editedFiles)
    }
    private enum CodingKeys: String, CodingKey { case id, items, startedAt, completedAt, status, runningElsewhere, editedFiles }
    public var isInProgress: Bool { status == "inProgress" }
    public var terminalLabel: String? {
        switch status {
        case "completed": return workedLabel
        case "interrupted": return "Stopped"
        case "failed": return "Couldn’t finish"
        default: return nil
        }
    }
    /// Under a second reads as plain "Worked": a turn the Mac read in one
    /// batch can carry the same start and end, and "0s" says nothing.
    public var workedLabel: String {
        guard let startedAt, let completedAt,
              let start = Self.seconds(startedAt), let end = Self.seconds(completedAt),
              end - start >= 1, end - start < Double(Int.max) else { return "Worked" }
        let seconds = Int(end - start)
        return seconds >= 60 ? "Worked for \(seconds / 60)m \(seconds % 60)s" : "Worked for \(seconds)s"
    }
    public var startedDate: Date? {
        startedAt.flatMap(Self.seconds).map { Date(timeIntervalSince1970: $0) }
    }
    private static func seconds(_ value: String) -> Double? {
        if let milliseconds = Double(value), milliseconds.isFinite { return milliseconds / 1000 }
        return ReadTimestamp.seconds(value)
    }
}
public struct ReadItem: Codable, Equatable, Sendable {
    public let id: String; public let type: String; public let state: String
    public let text: String?; public let createdAt: String
    public var payload: [String: ThreadValue]? = nil
}

extension ReadItem {
    private enum CodingKeys: String, CodingKey { case id, type, state, text, createdAt, payload }
    /// Hosts attach each computer-use screenshot inline as a multi-megabyte
    /// base64 data URL. Native clients never render that field, so drop it
    /// before it reaches row projection, the timeline or the on-device cache.
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        type = try container.decode(String.self, forKey: .type)
        state = try container.decode(String.self, forKey: .state)
        text = try container.decodeIfPresent(String.self, forKey: .text)
        createdAt = try container.decode(String.self, forKey: .createdAt)
        payload = try container.decodeIfPresent([String: ThreadValue].self, forKey: .payload)
        if type == "imageView", payload?["imageUrl"]?.string?.hasPrefix("data:") == true {
            payload?["imageUrl"] = nil
        }
    }
}

/// Retains the daemon's sanitized structured payload through disk cache/replay.
/// Unknown tool fields survive without making the whole conversation undecodable.
public enum ThreadValue: Codable, Equatable, Sendable {
    case string(String), number(Double), bool(Bool), array([ThreadValue]), object([String: ThreadValue]), null
    public var string: String? { if case .string(let value) = self { return value }; return nil }
    public var number: Double? { if case .number(let value) = self { return value }; return nil }
    public var bool: Bool? { if case .bool(let value) = self { return value }; return nil }
    public var array: [ThreadValue]? { if case .array(let value) = self { return value }; return nil }
    public init(from decoder: Decoder) throws {
        // `ThreadJSON` already knows the node's type: no guessing, no failed attempts.
        if let tree = decoder as? TreeDecoder { self.init(tree: tree.value); return }
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let decoded = try? value.decode(Bool.self) { self = .bool(decoded) }
        else if let decoded = try? value.decode(String.self) { self = .string(decoded) }
        else if let decoded = try? value.decode(Double.self) { self = .number(decoded) }
        else if let decoded = try? value.decode([ThreadValue].self) { self = .array(decoded) }
        else { self = .object(try value.decode([String: ThreadValue].self)) }
    }
    public func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        switch self {
        case .string(let item): try value.encode(item)
        case .number(let item): try value.encode(item)
        case .bool(let item): try value.encode(item)
        case .array(let item): try value.encode(item)
        case .object(let item): try value.encode(item)
        case .null: try value.encodeNil()
        }
    }
}

public struct ReplayEvent: Codable, Sendable {
    public let eventId: String
    public let hostEpoch: String
    public let sequence: UInt64
    public let occurredAt: String
    public let conversationId: String?
    public let event: EventBody
    /// Where a streamed change belongs, for applying it to an open conversation.
    public var turnId: String? = nil
    public var itemId: String? = nil
    public var messageId: String? = nil
    public init(eventId: String, hostEpoch: String, sequence: UInt64, occurredAt: String, conversationId: String?, event: EventBody,
                turnId: String? = nil, itemId: String? = nil, messageId: String? = nil) {
        self.eventId = eventId; self.hostEpoch = hostEpoch; self.sequence = sequence; self.occurredAt = occurredAt
        self.conversationId = conversationId; self.event = event; self.turnId = turnId; self.itemId = itemId; self.messageId = messageId
    }

    /// One socket frame: a single event, or a JSON array of events from a host
    /// that coalesces them (requested with `batch` in the subscription). Only a
    /// batching host journals each streamed delta beside its text change, so
    /// only its events may be applied to a snapshot directly.
    public static func frame(_ data: Data) throws -> (events: [ReplayEvent], batched: Bool) {
        let first = data.first { !$0.isWhitespace }
        if first == UInt8(ascii: "[") { return (try JSONDecoder().decode([ReplayEvent].self, from: data), true) }
        return ([try JSONDecoder().decode(ReplayEvent.self, from: data)], false)
    }
}

private extension UInt8 {
    var isWhitespace: Bool { self == 0x20 || self == 0x0A || self == 0x0D || self == 0x09 }
}

public struct EventBody: Codable, Sendable {
    public let type: String
    /// Activity category/state, read to recognize host-wide runtime notices
    /// and live item updates. Other payload shapes decode as nil rather than
    /// failing replay.
    public let category: String?
    public let state: String?
    /// Activity detail (JSON text for item upserts and invalidations).
    public var detail: String? = nil
    /// Assistant text: the appended part of a delta, or a completed message.
    public var text: String? = nil
    public init(type: String, category: String? = nil, state: String? = nil, detail: String? = nil, text: String? = nil) {
        self.type = type; self.category = category; self.state = state; self.detail = detail; self.text = text
    }
    private enum CodingKeys: String, CodingKey { case type, data }
    private struct Activity: Codable { let category: String?; let state: String?; var detail: String?; var text: String? }
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        type = try container.decode(String.self, forKey: .type)
        let activity = (try? container.decodeIfPresent(Activity.self, forKey: .data)) ?? nil
        category = activity?.category; state = activity?.state; detail = activity?.detail; text = activity?.text
    }
    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(type, forKey: .type)
        if category != nil || state != nil || detail != nil || text != nil {
            try container.encode(Activity(category: category, state: state, detail: detail, text: text), forKey: .data)
        }
    }
    /// Runtime status the host journals without a conversation (MCP startup,
    /// rate limits, app lists, warnings). It changes no conversation data.
    public var isHostRuntimeNotice: Bool { type == "activity" && ((category == "App Server" && state == "unhandled") || (category == "codex_skills" && state == "changed")) }
}

public struct ProjectionState: Codable, Sendable {
    public var dirty: Set<String> = []
    /// The newest replay sequence that invalidated each dirty conversation. A
    /// conversation without an entry needs a snapshot at the global cursor.
    /// Optional so caches written before scoped invalidation still decode.
    public var invalidatedThrough: [String: UInt64]?
    public var listDirty = true
    public var positions: [String: String] = [:]
    public init() { hostEpoch = ""; lastSequence = 0; resident = [:] }
    private enum CodingKeys: String, CodingKey {
        case dirty, invalidatedThrough, listDirty, positions, hostEpoch, lastSequence, resident = "snapshots", bodyRecords
    }
    /// A snapshot read at `sequence` covers this conversation's invalidations
    /// through that sequence. Later events for other conversations do not make
    /// it stale; the host's read fence uses the same per-conversation rule.
    public func covers(_ conversation: String, through sequence: UInt64) -> Bool {
        sequence >= (invalidatedThrough?[conversation] ?? lastSequence)
    }
    public mutating func markClean(_ conversation: String) {
        dirty.remove(conversation)
        invalidatedThrough?[conversation] = nil
    }
    /// Marks every cached conversation stale as of the current cursor, for a
    /// cold load, resnapshot, epoch change or host-wide event.
    public mutating func invalidateAll() {
        var marks = invalidatedThrough ?? [:]
        for conversation in knownConversationIDs {
            dirty.insert(conversation)
            marks[conversation] = max(marks[conversation] ?? 0, lastSequence)
        }
        invalidatedThrough = marks
        listDirty = true
    }
    // A snapshot covers one chat, never the rest of the host.
    public mutating func install(_ snapshot: ConversationSnapshot) {
        if let existing = resident[snapshot.conversationId], existing.hostEpoch == snapshot.hostEpoch {
            if (existing.historyRevision ?? 0) > (snapshot.historyRevision ?? 0) { return }
            if existing.lastSequence > snapshot.lastSequence { return }
            if existing.lastSequence == snapshot.lastSequence,
               (existing.historyRevision ?? 0) == (snapshot.historyRevision ?? 0),
               existing.messages.count + existing.assistantMessages.count > snapshot.messages.count + snapshot.assistantMessages.count { return }
        }
        if hostEpoch != snapshot.hostEpoch {
            hostEpoch = snapshot.hostEpoch
            lastSequence = snapshot.lastSequence
            invalidatedThrough = nil
            invalidateAll()
        }
        store(snapshot)
        liveChanges[snapshot.conversationId] = nil
        if covers(snapshot.conversationId, through: snapshot.lastSequence) { markClean(snapshot.conversationId) }
    }
    public mutating func consume(_ event: ReplayEvent) throws {
        guard event.event.type != "resync_required", event.hostEpoch == hostEpoch else { throw ReadFailure.resync }
        if event.sequence <= lastSequence { return }
        guard lastSequence < UInt64.max, event.sequence == lastSequence + 1 else { throw ReadFailure.resync }
        lastSequence = event.sequence
        let conversation = event.conversationId.flatMap { $0.isEmpty ? nil : $0 }
        if conversation == nil && event.event.isHostRuntimeNotice { return }
        // Streamed text and its journal notice change no list summary. A clean
        // GET ahead of replay contains them; they need only the contiguous cursor.
        // Lifecycle, approval and item events still reach the list below.
        if let conversation, snapshotCovers(conversation, event: event),
           event.event.type == "assistant_delta" || (event.event.type == "activity" && event.event.category == "projection_changed"
                && ConversationSnapshot.invalidatedResource(event.event.detail) == "assistant_messages") { return }
        listDirty = true
        if let conversation {
            // A same-epoch read can be ahead of replay. Consume the event for
            // the list/cursor, but never make already-covered content stale.
            guard !snapshotCovers(conversation, event: event) else { return }
            dirty.insert(conversation)
            invalidatedThrough = (invalidatedThrough ?? [:]).merging([conversation: event.sequence]) { _, new in new }
        } else {
            // Unknown ownership stays conservative for every cache that the
            // event is newer than; a clean snapshot ahead of replay covers it.
            for id in knownConversationIDs where !snapshotCovers(id, event: event) {
                dirty.insert(id)
                invalidatedThrough = (invalidatedThrough ?? [:]).merging([id: event.sequence]) { _, new in new }
            }
        }
    }

    func snapshotCovers(_ conversation: String, event: ReplayEvent) -> Bool {
        guard !dirty.contains(conversation), let snapshot = resident[conversation] else { return false }
        return snapshot.hostEpoch == event.hostEpoch && snapshot.lastSequence >= event.sequence
    }

    public var hostEpoch: String
    public var lastSequence: UInt64
    /// The resident histories. Held apart from `snapshots` so a live event can
    /// change the open chat in place: a property observer would keep the old
    /// dictionary alive to compare against, and with it a second owner of
    /// every array in the chat.
    var resident: [String: ConversationSnapshot]
    public var snapshots: [String: ConversationSnapshot] {
        get { resident }
        set {
            let old = resident
            resident = newValue
            var records = bodyRecords ?? [:]
            // Resident absence never deletes disk ownership. Deletion has
            // one explicit path, forgetConversation(_:).
            for (id, snapshot) in newValue where old[id] != snapshot {
                records[id] = Self.record(snapshot, replacing: old[id], keeping: records[id])
            }
            bodyRecords = records
        }
    }
    /// The saved body for a changed history: the same body when only
    /// its sequence moved, a new one when any content did.
    private static func record(_ snapshot: ConversationSnapshot, replacing old: ConversationSnapshot?,
                               keeping record: CachedConversationBody?) -> CachedConversationBody {
        let sameBody = old?.messages == snapshot.messages && old?.assistantMessages == snapshot.assistantMessages
            && old?.thread.turns == snapshot.thread.turns && old?.messageRecords == snapshot.messageRecords
        return CachedConversationBody(snapshot, reusing: sameBody ? record : nil)
    }
    /// Stores one history, as assigning it through `snapshots` would.
    mutating func store(_ snapshot: ConversationSnapshot) {
        let old = resident[snapshot.conversationId]
        resident[snapshot.conversationId] = snapshot
        if old != snapshot {
            let record = Self.record(snapshot, replacing: old, keeping: bodyRecords?[snapshot.conversationId])
            bodyRecords?[snapshot.conversationId] = record
        }
    }
    /// The changes live events made to each conversation since they were last
    /// taken, in order; a projection built from the conversation replays them.
    /// Not saved. Bounded: past `liveChangeLimit` one structural change stands
    /// for them all.
    var liveChanges: [String: [LiveChange]] = [:]
    static let liveChangeLimit = 512
    public mutating func takeLiveChanges(_ conversation: String) -> [LiveChange] {
        liveChanges.removeValue(forKey: conversation) ?? []
    }
    public internal(set) var bodyRecords: [String: CachedConversationBody]? = [:]
    public var knownConversationIDs: Set<String> { Set((bodyRecords ?? [:]).keys).union(resident.keys) }
    public static let residentByteLimit = 32 * 1_024 * 1_024
    /// Conservative allocation accounting, not a measurement of process RSS.
    public var residentBytes: Int {
        resident.keys.reduce(0) { $0 + (bodyRecords?[$1]?.residentBytes ?? 0) }
    }
    public mutating func touchConversation(_ id: String) {
        bodyRecords?[id]?.lastAccess = Date().timeIntervalSince1970
    }
    public func metadataOnly() -> Self {
        var copy = self
        copy.resident = [:]
        return copy
    }
    /// Restoring disk history is not an authoritative read: retain dirty marks,
    /// replay fences and the global cursor, even when the disk page was clean.
    @discardableResult public mutating func restoreConversation(_ snapshot: ConversationSnapshot,
                                                               matching record: CachedConversationBody) -> Bool {
        let id = snapshot.conversationId
        guard resident[id] == nil, let current = bodyRecords?[id], current.matches(record) else { return false }
        resident[id] = snapshot
        bodyRecords?[id] = current
        touchConversation(id)
        return true
    }
    /// Absence from snapshots means non-resident, never deletion. Only an
    /// explicit forget removes the disk index entry at the next atomic save.
    public mutating func forgetConversation(_ id: String) {
        resident[id] = nil
        liveChanges[id] = nil
        bodyRecords?[id] = nil
        positions[id] = nil
        markClean(id)
    }
    @discardableResult public mutating func evictResidentBodies(
        keeping protected: Set<String>, durable: [String: UUID], byteLimit: Int = residentByteLimit
    ) -> Set<String> {
        var bytes = residentBytes
        let candidates = resident.keys.filter { !protected.contains($0) }.sorted {
            let left = bodyRecords?[$0]?.lastAccess ?? 0, right = bodyRecords?[$1]?.lastAccess ?? 0
            return left == right ? $0 < $1 : left < right
        }
        let records = bodyRecords
        var evicted = Set<String>()
        for id in candidates where bytes > max(0, byteLimit) {
            guard let record = records?[id], durable[id] == record.bodyID else { continue }
            bytes -= record.residentBytes
            resident[id] = nil
            evicted.insert(id)
        }
        bodyRecords = records
        return evicted
    }
}

/// Body identity and cheap paging/replay headers for every saved conversation.
/// Payload arrays and message-record indexes live solely in its body file.
public struct CachedConversationBody: Codable, Equatable, Sendable {
    public let bodyID: UUID
    public let residentBytes: Int
    public fileprivate(set) var lastAccess: TimeInterval
    private let hostEpoch: String
    private let lastSequence: UInt64
    private let historyRevision: UInt64?
    private let threadId: String?
    private let nextCursor: String?
    private let hydrated: Bool
    init(_ snapshot: ConversationSnapshot, reusing old: Self?) {
        bodyID = old?.bodyID ?? UUID()
        residentBytes = old?.residentBytes ?? snapshot.residentByteCost
        lastAccess = Date().timeIntervalSince1970
        hostEpoch = snapshot.hostEpoch; lastSequence = snapshot.lastSequence
        historyRevision = snapshot.historyRevision; threadId = snapshot.thread.threadId
        nextCursor = snapshot.thread.nextCursor; hydrated = snapshot.thread.hydrated
    }
    /// The record after a live event changed the conversation: a new body when
    /// the event changed what is saved, the size estimate moved by `change`.
    init(live snapshot: ConversationSnapshot, after change: LiveChange, from old: Self?) {
        bodyID = change.changesContent ? UUID() : old?.bodyID ?? UUID()
        residentBytes = old.map { max(0, $0.residentBytes + change.residentBytes) } ?? snapshot.residentByteCost
        lastAccess = Date().timeIntervalSince1970
        hostEpoch = snapshot.hostEpoch; lastSequence = snapshot.lastSequence
        historyRevision = snapshot.historyRevision; threadId = snapshot.thread.threadId
        nextCursor = snapshot.thread.nextCursor; hydrated = snapshot.thread.hydrated
    }
    fileprivate func matches(_ other: Self) -> Bool {
        var copy = other
        // Navigation can touch the same record while its body is decoding.
        // Access order is not a new page and must not strand an offline open.
        copy.lastAccess = lastAccess
        return self == copy
    }
    fileprivate func applying(conversation: String, messages: [ConversationMessage], assistantMessages: [AssistantMessage],
                              turns: [ReadTurn]?, messageRecords: [MessageRecord]?) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: conversation, hostEpoch: hostEpoch, lastSequence: lastSequence,
            historyRevision: historyRevision, messages: messages, assistantMessages: assistantMessages,
            thread: ThreadProjection(threadId: threadId, nextCursor: nextCursor, hydrated: hydrated, turns: turns),
            messageRecords: messageRecords)
    }
}

extension ConversationSnapshot {
    private static func strings(_ values: String?...) -> Int { values.reduce(0) { $0 + ($1?.utf8.count ?? 0) * 2 + 32 } }
    private static func payload(_ values: [String: ThreadValue]?) -> Int {
        (values ?? [:]).reduce(0) { $0 + 96 + $1.key.utf8.count * 2 + $1.value.residentByteCost }
    }
    static func residentCost(of m: AssistantMessage) -> Int {
        192 + strings(m.itemId, m.codexTurnId, m.messageId, m.text, m.state, m.createdAt, m.updatedAt)
    }
    static func residentCost(of item: ReadItem) -> Int {
        192 + strings(item.id, item.type, item.state, item.text, item.createdAt) + payload(item.payload)
    }
    /// Account all variable-sized content, including nested tool payloads. A
    /// two-byte-per-UTF8-byte allowance covers UTF16 storage; fixed allowances
    /// cover array slots, dictionary entries and small value fields. Shared CoW
    /// storage is deliberately counted for each owner. No JSON encoding on UI.
    /// Live events move it by the same terms, per element (`LiveChange.residentBytes`).
    fileprivate var residentByteCost: Int {
        var bytes = 512 + Self.strings(conversationId, hostEpoch, thread.threadId, thread.nextCursor)
        for m in messages {
            bytes += 256 + Self.strings(m.clientMessageId, m.codexTurnId, m.codexThreadId, m.originalBodySha256,
                m.messageId, m.body, m.state, m.createdAt, m.deliveryError,
                m.source?.kind, m.source?.sourceConversationId, m.source?.sourceTitle)
            for id in m.attachmentIds { bytes += Self.strings(id) }
        }
        for m in assistantMessages { bytes += Self.residentCost(of: m) }
        for turn in thread.turns ?? [] {
            bytes += 256 + Self.strings(turn.id, turn.startedAt, turn.completedAt, turn.status) + Self.payload(turn.editedFiles)
            for item in turn.items { bytes += Self.residentCost(of: item) }
        }
        for record in messageRecords ?? [] { bytes += 96 + Self.strings(record.turnId, record.itemId, record.clientId) }
        return bytes
    }
}
private extension ThreadValue {
    var residentByteCost: Int {
        switch self {
        case .string(let value): return 32 + value.utf8.count * 2
        case .array(let values): return 32 + values.reduce(0) { $0 + 32 + $1.residentByteCost }
        case .object(let values): return 32 + values.reduce(0) { $0 + 96 + $1.key.utf8.count * 2 + $1.value.residentByteCost }
        default: return 32
        }
    }
}


public enum ReadFailure: Error { case resync, wrongConversation }

enum ReadTimestamp {
    // FormatStyle is a Sendable value; Foundation caches its parsing machinery.
    // Do not construct an ISO8601DateFormatter for every row on each view update.
    static let fractional = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
    static let whole = Date.ISO8601FormatStyle(includingFractionalSeconds: false)
    static func seconds(_ value: String) -> Double? {
        ((try? fractional.parse(value)) ?? (try? whole.parse(value)))?.timeIntervalSince1970
    }
}

public struct ReadRow: Identifiable, Sendable {
    /// The name of a provider as T3 Code records it in a timeline item's ID.
    public static func providerName(_ provider: String) -> String {
        switch provider {
        case "claudeAgent": "Claude"
        case "opencode": "OpenCode" // provider identity
        case "cursor": "Cursor"
        case "codex": "Codex" // provider identity
        default: provider.prefix(1).uppercased() + provider.dropFirst()
        }
    }
    public let attachmentIds: [String]
    /// Set on the owner's message when the Mac did not deliver it.
    public let undelivered: UndeliveredMessage?
    /// Set when another thread, or Wonder reporting finished tasks, wrote this.
    public let source: MessageSource?
    /// Prepared while projecting the cached row so command wrapper scanning
    /// never runs from a SwiftUI body update.
    public let commandSummary: CommandSummary?
    public let fileChangeSummary: FileChangeSummary?
    public let thinkingTiming: ThinkingTiming?
    /// The summary a provider wrote for a thought (Codex). The host sends it
    /// only while the provider's chat detail is Thinking.
    public let thinkingSummary: String?
    public let taskChecklist: TaskChecklist?
    /// A shell or slash command the owner ran in Claude Code, shown as a command card.
    public let commandBlock: ClaudeCommandBlock?
    public let id: String
    public private(set) var author: String
    public let authorId: String?
    public let text: String
    public let isUser: Bool
    public let timestamp: String
    public let time: Double
    public let turnId: String?
    public let item: ReadItem?
    /// Prepared once per projected row. Timeline grouping reads these for every
    /// row; recomputing them decoded tool JSON on every SwiftUI update.
    public private(set) var activitySummary: ActivityPresentation?
    public private(set) var toolFiles: [ConversationFile] = []
    /// The part of `toolFiles` the conversation shows outside tool details.
    public private(set) var shownFiles: [ConversationFile] = []
    public private(set) var nativeQuestion: NativeQuestionPresentation?
    private let nativeQuestionReplies: [NativeQuestionReply]?
    /// Commentary is visible agent speech, never private reasoning or a tool result.
    /// A finished turn's role (`settlingFinishedTurns`) wins over the provider's phase.
    public var isCommentary: Bool {
        if let settledCommentary { return settledCommentary }
        return !isUser && item?.type == "agentMessage" && item?.payload?["phase"]?.string == "commentary"
    }
    /// Set only by `settlingFinishedTurns`: whether this agent text is work
    /// (true) or the turn's reply (false). Nil leaves the provider's phase.
    public internal(set) var settledCommentary: Bool?
    /// Agent text the owner reads as a reply or an update: not the owner's own
    /// words, a question form, another thread's report or an empty row.
    var isAgentSpeech: Bool {
        !isUser && source == nil && nativeQuestion == nil && (item == nil || item?.type == "agentMessage")
            && (!text.isEmpty || !attachmentIds.isEmpty)
    }
    /// A finished image view or generation whose output is its pictures: the
    /// pictures show by themselves, without a "Viewed image" row around them.
    /// A failed one keeps its row, so its error stays readable.
    public var isOnlyItsMedia: Bool {
        guard let type = item?.type, type == "imageView" || type == "imageGeneration",
              item?.state == "completed", item?.payload?["success"]?.bool != false,
              !toolFiles.isEmpty else { return false }
        return toolFiles.allSatisfy(\.isVisualMedia)
    }
    /// A Claude TaskCreate/TaskUpdate call that did not fail. Its change shows in
    /// the turn's task checklist (`<turn>:tasks`), so the call itself is not listed.
    public var isClaudeTaskChange: Bool {
        guard item?.type == "mcpToolCall", item?.payload?["server"]?.string == "Claude",
              let tool = item?.payload?["tool"]?.string, tool == "TaskCreate" || tool == "TaskUpdate" else { return false }
        return item?.payload?["success"]?.bool != false && item?.state != "failed"
    }
    public init(id: String, author: String, text: String, isUser: Bool, timestamp: String, authorId: String? = nil,
                turnId: String? = nil, item: ReadItem? = nil, attachmentIds: [String] = [],
                source: MessageSource? = nil, undelivered: UndeliveredMessage? = nil, hostEpoch: String? = nil) {
        self.attachmentIds = attachmentIds
        self.undelivered = undelivered
        self.source = source
        self.turnId = turnId; self.item = item
        self.commandSummary = item.flatMap(CommandSummary.prepare)
        self.fileChangeSummary = item.flatMap(FileChangeSummary.prepare)
        self.thinkingTiming = item.flatMap { ThinkingTiming.prepare($0, hostEpoch: hostEpoch) }
        self.thinkingSummary = item.flatMap { item in
            guard item.type == "reasoning", let text = item.text?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !text.isEmpty else { return nil }
            return text
        }
        self.taskChecklist = item.flatMap(TaskChecklist.prepare)
        nativeQuestion = item.flatMap(NativeQuestionPresentation.prepare)
        nativeQuestionReplies = isUser ? NativeQuestionReply.parse(text) : nil
        commandBlock = isUser && nativeQuestionReplies == nil ? ClaudeCommandBlock.parse(text) : nil
        self.id = id; self.author = author; self.authorId = authorId
        self.text = nativeQuestionReplies?.map { $0.question + "\n" + $0.answer }.joined(separator: "\n\n")
            ?? commandBlock?.plainText
            ?? (!isUser && (item == nil || item?.type == "agentMessage") ? AssistantDisplayText.render(text)
                : isUser && (item == nil || item?.type == "userMessage") ? OwnerDisplayText.render(text) : text)
        self.isUser = isUser; self.timestamp = timestamp
        time = Double(timestamp).map { $0 / 1000 } ?? ReadTimestamp.seconds(timestamp) ?? 0
        activitySummary = presentation(includeDetails: false)
        toolFiles = makeToolFiles()
        shownFiles = makeShownFiles()
    }

    /// Resolve speaker identity after live overlays and ordering, preserving
    /// prepared row content such as reconciled question answers.
    static func identifyingSpeakers(_ rows: [Self], fallback: String) -> [Self] {
        let firstSwitch = rows.first { $0.item?.type == "providerSwitch" }
        var speaker = firstSwitch?.item?.payload?["fromFamily"]?.string
            .flatMap(AgentFamily.init(rawValue:))?.title ?? fallback
        return rows.map { original in
            var row = original
            if row.item?.type == "providerSwitch",
               let raw = row.item?.payload?["toFamily"]?.string,
               let family = AgentFamily(rawValue: raw) {
                speaker = family.title
            }
            if !row.isUser && (row.item == nil || row.item?.type == "agentMessage") {
                // A reply T3 Code recorded a provider for is that provider's, not the chat's family.
                row.author = row.item?.payload?["t3Provider"]?.string.flatMap { $0.isEmpty ? nil : Self.providerName($0) } ?? speaker
            }
            return row
        }
    }

    static func reconcilingQuestions(_ rows: [Self]) -> [Self] {
        var rows = rows
        let questions = Dictionary(rows.indices.compactMap { index -> (String, Int)? in
            guard rows[index].nativeQuestion != nil, let id = rows[index].item?.id else { return nil }
            return (id, index)
        }, uniquingKeysWith: { first, _ in first })
        var answeredReplyIDs = Set<String>()
        for row in rows {
            guard let replies = row.nativeQuestionReplies else { continue }
            var matched = 0
            for reply in replies {
                guard let target = reply.target, let index = questions[target.itemID],
                      let form = rows[index].nativeQuestion, form.questions.indices.contains(target.index),
                      form.questions[target.index].title == reply.question else { continue }
                rows[index].nativeQuestion?.answers[target.index] = reply.answer
                matched += 1
            }
            // A paged answer stays readable until all its questions are loaded.
            // Attachments remain visible even when the form owns the reply text.
            if matched == replies.count, row.attachmentIds.isEmpty { answeredReplyIDs.insert(row.id) }
        }
        return rows.filter { !answeredReplyIDs.contains($0.id) }
    }
}

/// Where a chat's walk to settle items still shown running continues on its
/// next refresh, in one history. Saved beside the cache so a relaunch carries
/// on instead of reading the same older pages again; another host epoch or
/// history revision ignores it.
public struct SettleRepair: Codable, Equatable, Sendable {
    public let hostEpoch: String
    public let historyRevision: UInt64
    public var cursor: String?
    /// Items a walk to the start of history did not find. They still show as
    /// running (absence does not prove they finished); once `recheckAfter`
    /// has passed a walk looks for them again.
    public var unreachable: Set<String> = []
    public var recordedAt: Date
    public static let maximumUnreachable = 256
    public static let recheckAfter: TimeInterval = 24 * 60 * 60
    public init(hostEpoch: String, historyRevision: UInt64, cursor: String? = nil, unreachable: Set<String> = [], recordedAt: Date = Date()) {
        self.hostEpoch = hostEpoch; self.historyRevision = historyRevision
        self.cursor = cursor; self.unreachable = unreachable; self.recordedAt = recordedAt
    }
    /// This record if it still describes the history the Mac just returned.
    public func current(epoch: String, revision: UInt64, now: Date = Date()) -> SettleRepair? {
        guard hostEpoch == epoch, historyRevision == revision else { return nil }
        guard unreachable.isEmpty || now.timeIntervalSince(recordedAt) < Self.recheckAfter else { return nil }
        return self
    }
    /// The record after one walk. `next` is where the walk stopped (nil at the
    /// start of history). A walk the Mac refused (`placeRefused`) proves
    /// nothing about the items it looked for: it only forgets its place.
    public func afterWalk(missing: Set<String>, next: String?, pagesRead: Int, placeRefused: Bool, now: Date = Date()) -> SettleRepair {
        var record = self
        if missing.isEmpty || placeRefused { record.cursor = nil }
        else if let next { if pagesRead > 0 { record.cursor = next } }
        else if pagesRead > 0 {
            // The walk reached the start: what it did not find is not there.
            record.unreachable.formUnion(missing); record.cursor = nil; record.recordedAt = now
        }
        return record
    }
    /// Kept only for items the saved chat still shows running, and bounded.
    /// Nil when nothing is left to remember.
    public func bounded(to unsettled: Set<String>) -> SettleRepair? {
        var next = self
        next.unreachable = Set(unreachable.intersection(unsettled).sorted().prefix(Self.maximumUnreachable))
        return next.cursor == nil && next.unreachable.isEmpty ? nil : next
    }
}

extension ReadStore {
    private static func settleRepairKey(_ conversation: String) -> String { "settle-repair-v1:" + conversation }
    /// Losing the record only repeats a bounded walk, so an unreadable one is nil.
    public func loadSettleRepair(conversation: String) -> SettleRepair? {
        guard let data = try? loadIntent(conversation: Self.settleRepairKey(conversation)) else { return nil }
        return try? JSONDecoder().decode(SettleRepair.self, from: data)
    }
    public func saveSettleRepair(_ repair: SettleRepair?, conversation: String) throws {
        if let repair { try saveIntent(JSONEncoder().encode(repair), conversation: Self.settleRepairKey(conversation)) }
        else { try removeIntent(conversation: Self.settleRepairKey(conversation)) }
    }
}

/// Saved approval decisions ("decision-" + id) and answer drafts ("answer-" +
/// id) are intent files named by a hash, so an index of the approval IDs that
/// may have one lets them be removed once the Mac no longer lists them. Each ID
/// carries the order it was first noted in, so a read that started before an
/// approval was known cannot remove its files.
extension ReadStore {
    private static let attentionIndexKey = "attention-intents-v2"
    private struct AttentionIndex: Codable {
        var next: UInt64 = 0
        var noted: [String: UInt64] = [:]
    }
    private func attentionIndex() -> AttentionIndex {
        guard let data = try? loadIntent(conversation: Self.attentionIndexKey) else { return AttentionIndex() }
        return (try? JSONDecoder().decode(AttentionIndex.self, from: data)) ?? AttentionIndex()
    }
    private func saveAttentionIndex(_ index: AttentionIndex) throws {
        if index.noted.isEmpty { try removeIntent(conversation: Self.attentionIndexKey) }
        else { try saveIntent(JSONEncoder().encode(index), conversation: Self.attentionIndexKey) }
    }
    public func attentionIntentIDs() -> Set<String> { Set(attentionIndex().noted.keys) }
    /// Taken before reading the pending approvals and passed to
    /// `pruneAttentionIntents`: only approvals noted before it can be removed.
    public func attentionReadMark() -> UInt64 { attentionIndex().next }
    /// Records approvals that may have a saved decision or answer.
    public func noteAttentionIntents(_ ids: Set<String>) throws {
        var index = attentionIndex()
        let added = ids.subtracting(index.noted.keys)
        guard !added.isEmpty else { return }
        for id in added { index.noted[id] = index.next }
        index.next += 1
        try saveAttentionIndex(index)
    }
    /// Removes an approval's saved decision and answer draft.
    public func removeAttentionIntents(_ ids: Set<String>) throws {
        guard !ids.isEmpty else { return }
        for id in ids {
            try removeIntent(conversation: "decision-" + id)
            try removeIntent(conversation: "answer-" + id)
        }
        var index = attentionIndex()
        for id in ids { index.noted[id] = nil }
        try saveAttentionIndex(index)
    }
    /// After a successful read of every pending approval: removes saved
    /// decisions and drafts for approvals no longer pending (answered here or
    /// elsewhere, or expired). Call only with a complete list; after a failed
    /// read nothing is known, and an uncertain signed decision must keep its
    /// retry identity. `keeping` holds approvals with a reply in flight.
    /// `readMark` is `attentionReadMark()` from before the read: an approval
    /// first noted later (a newer read listed it while this one was in flight)
    /// is absent from this list only because the list is older.
    @discardableResult
    public func pruneAttentionIntents(pending: Set<String>, keeping: Set<String> = [], readMark: UInt64) throws -> Set<String> {
        let gone = Set(attentionIndex().noted.filter { $0.value < readMark }.keys).subtracting(pending).subtracting(keeping)
        try removeAttentionIntents(gone)
        return gone
    }
}

// An atomic metadata index names an immutable manifest per conversation, and
// each manifest names immutable per-turn segment files (HistorySegments.swift).
// Neither launch nor ProjectionWriter keeps a decoded checkpoint of all saved
// history. Intent, pins and reading positions are independent of this
// disposable cache.
public struct ReadStore: Sendable {
    public let directory: URL
    public init(root: URL, host: String, device: String) {
        let partition = SHA256.hash(data: Data((host + "\n" + device).utf8)).map { String(format: "%02x", $0) }.joined()
        directory = root.appendingPathComponent(partition, isDirectory: true)
    }
    private struct Index: Codable {
        let version: Int
        let state: ProjectionState
    }
    private static let indexVersion = 4
    private var indexURL: URL { directory.appendingPathComponent("read-cache-v4.json") }
    private var history: HistoryIndex { HistoryIndex.at(directory) }
    /// Metadata only. Old cache versions are intentionally discarded on first
    /// launch; the owner's pre-launch rule permits refetching their history.
    public func load() throws -> ProjectionState {
        if !FileManager.default.fileExists(atPath: indexURL.path) {
            for name in ["read-cache-v2.json", "read-cache-v3.json", "read-cache-overlay-v1.json"] {
                let url = directory.appendingPathComponent(name)
                if FileManager.default.fileExists(atPath: url.path) { try FileManager.default.removeItem(at: url) }
            }
            return ProjectionState()
        }
        let index = try JSONDecoder().decode(Index.self, from: Data(contentsOf: indexURL))
        guard index.version == Self.indexVersion, index.state.snapshots.isEmpty else { throw ReadFailure.resync }
        return index.state
    }
    /// Read on the writer queue, then restore only if the caller still owns
    /// this exact record. A newer network page must win a racing disk read.
    public func loadConversation(_ conversation: String, record: CachedConversationBody) throws -> ConversationSnapshot {
        let manifest = try readManifest(record)
        let loaded = try manifest.segmentNames.map {
            try ThreadJSON.decode(HistorySegment.self, from: Data(contentsOf: directory.appendingPathComponent($0)))
        }
        guard manifest.conversationId == conversation, let body = HistoryPlan(manifest: manifest, loaded: loaded).body() else {
            throw ReadFailure.wrongConversation
        }
        let snapshot = record.applying(conversation: conversation, messages: body.messages, assistantMessages: body.assistant,
                                       turns: body.turns, messageRecords: body.records)
        let history = self.history
        history.lock.withLock {
            // Remember the segments the next save compares against.
            if history.primed, history.entries[conversation]?.bodyID == record.bodyID {
                history.entries[conversation]?.plan = HistoryPlan(snapshot)
            }
        }
        return snapshot
    }
    private func readManifest(_ record: CachedConversationBody) throws -> HistoryManifest {
        try JSONDecoder().decode(HistoryManifest.self,
            from: Data(contentsOf: directory.appendingPathComponent(HistoryManifest.manifestName(record))))
    }
    fileprivate func durableBodyIDs() throws -> [String: UUID] {
        let state = try load()
        var result: [String: UUID] = [:]
        for (id, record) in state.bodyRecords ?? [:] {
            var isDirectory: ObjCBool = false
            if FileManager.default.fileExists(atPath: directory.appendingPathComponent(HistoryManifest.manifestName(record)).path,
                                              isDirectory: &isDirectory), !isDirectory.boolValue {
                result[id] = record.bodyID
            }
        }
        return result
    }
    /// Returns bytes actually written: the segments whose content changed, the
    /// manifests naming them, and the small index. Publish the index after
    /// every segment and manifest is durable, then reclaim unreferenced files.
    /// A failed index leaves the old index and its immutable files readable.
    @discardableResult public func save(_ state: ProjectionState) throws -> Int {
        let history = self.history
        return try history.lock.withLock { try save(state, history) }
    }

    /// Lock held.
    private func save(_ state: ProjectionState, _ history: HistoryIndex) throws -> Int {
        try prime(history)
        var written = 0
        var entries = history.entries
        for (id, record) in state.bodyRecords ?? [:] {
            if entries[id]?.bodyID == record.bodyID { continue }
            guard let snapshot = state.snapshots[id] else {
                // Never replace the index with a reference we cannot reload.
                guard let manifest = try? readManifest(record) else { throw ReadFailure.resync }
                entries[id] = HistoryIndex.Entry(bodyID: record.bodyID, manifest: manifest, plan: nil)
                continue
            }
            let plan = HistoryPlan(snapshot)
            let old = entries[id]
            // Retained segments by key (same position in the old manifest) and
            // every file the old manifest already names.
            let oldPosition = old?.plan.map { Dictionary(uniqueKeysWithValues: $0.keys.enumerated().map { ($1, $0) }) }
            let oldNames = Set(old?.manifest.files ?? [])
            var files: [String] = []
            for key in plan.keys {
                guard let segment = plan.segments[key] else { continue }
                if let position = oldPosition?[key], let oldFiles = old?.manifest.files, oldFiles.indices.contains(position),
                   old?.plan?.segments[key] == segment {
                    files.append(oldFiles[position]); continue
                }
                // Sorted keys: equal content must give equal bytes, in any process.
                let encoder = JSONEncoder(); encoder.outputFormatting = .sortedKeys
                let data = try encoder.encode(segment)
                let digest = HistoryManifest.digest(data)
                files.append(digest)
                let name = HistoryManifest.segmentName(digest)
                if oldNames.contains(digest) { continue }
                // The name is the content: an existing file is the same bytes.
                if FileManager.default.fileExists(atPath: directory.appendingPathComponent(name).path) { continue }
                try write(data, name: name)
                written += data.count
            }
            let manifest = plan.manifest(conversationId: id, files: files)
            let data = try JSONEncoder().encode(manifest)
            try write(data, name: HistoryManifest.manifestName(record))
            written += data.count
            entries[id] = HistoryIndex.Entry(bodyID: record.bodyID, manifest: manifest, plan: plan)
        }
        let data = try JSONEncoder().encode(Index(version: Self.indexVersion, state: state.metadataOnly()))
        try write(data, name: indexURL.lastPathComponent)
        written += data.count
        history.indexRevision = try? fileRevision(at: indexURL)
        // Published: what the index names is now what memory knows.
        var kept = Set<String>()
        var complete = true
        for (id, record) in state.bodyRecords ?? [:] {
            guard let entry = entries[id], entry.bodyID == record.bodyID else { complete = false; continue }
            kept.insert(HistoryManifest.manifestName(record)); kept.formUnion(entry.manifest.segmentNames)
        }
        history.entries = entries.filter { state.bodyRecords?[$0.key]?.bodyID == $0.value.bodyID }
        for id in history.entries.keys where state.snapshots[id] == nil { history.entries[id]?.plan = nil }
        if complete { removeUnreferencedFiles(keeping: kept) }
        return written
    }

    /// Lock held. Learns what is on disk once per process, or again if the index
    /// file changed under this process (a removal, another owner).
    private func prime(_ history: HistoryIndex) throws {
        let revision = try? fileRevision(at: indexURL)
        if history.primed, history.indexRevision == revision { return }
        history.reset()
        history.indexRevision = revision
        history.primed = true
        guard let index = try? load() else { return }
        for (id, record) in index.bodyRecords ?? [:] {
            if let manifest = try? readManifest(record) {
                history.entries[id] = HistoryIndex.Entry(bodyID: record.bodyID, manifest: manifest, plan: nil)
            }
        }
    }

    private func removeUnreferencedFiles(keeping kept: Set<String>) {
        for url in (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? [] {
            if HistoryManifest.owns(url.lastPathComponent), !kept.contains(url.lastPathComponent) {
                try? FileManager.default.removeItem(at: url)
            }
        }
    }
    public func savePosition(_ position: String, conversation: String) throws {
        try saveIntent(JSONEncoder().encode(position), conversation: Self.positionKey(conversation))
    }
    public func loadPosition(conversation: String) throws -> String? {
        guard let data = try loadIntent(conversation: Self.positionKey(conversation)) else { return nil }
        return try JSONDecoder().decode(String.self, from: data)
    }
    public func remove() throws {
        history.lock.withLock { history.reset() }
        HistoryIndex.forget(directory)
        if FileManager.default.fileExists(atPath: directory.path) {
            try FileManager.default.removeItem(at: directory)
        }
    }
    public func saveIntent(_ data: Data, conversation: String) throws {
        try write(data, name: intentURL(conversation: conversation).lastPathComponent)
    }
    /// Prepare a sibling without replacing durable intent. The owner must
    /// validate its in-memory snapshot before publishing this file.
    func prepareIntent(_ data: Data, conversation: String, expectedRevision: String) throws -> PreparedIntent {
        // A process killed during preparation cannot run deinit. Reclaim only
        // old preparation files, off the UI actor; never durable intent files.
        let manager = FileManager.default
        let cutoff = Date().addingTimeInterval(-24 * 60 * 60)
        let abandoned = (try? manager.contentsOfDirectory(at: directory,
            includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
        for url in abandoned where url.lastPathComponent.hasPrefix(".prepared-intent-") {
            if let modified = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
               modified < cutoff { try? manager.removeItem(at: url) }
        }
        let name = ".prepared-intent-" + UUID().uuidString
        let url = directory.appendingPathComponent(name)
        do {
            try write(data, name: name)
            return PreparedIntent(store: self, conversation: conversation, expectedRevision: expectedRevision,
                                  preparedRevision: try fileRevision(at: url), url: url)
        }
        catch { try? FileManager.default.removeItem(at: url); throw error }
    }
    final class PreparedIntent: Sendable {
        private let store: ReadStore
        private let conversation: String
        let expectedRevision: String
        let preparedRevision: String
        private let url: URL
        init(store: ReadStore, conversation: String, expectedRevision: String, preparedRevision: String, url: URL) {
            self.store = store; self.conversation = conversation
            self.expectedRevision = expectedRevision; self.preparedRevision = preparedRevision; self.url = url
        }
        @MainActor func validateRevision() throws {
            guard try store.intentRevision(conversation: conversation) == expectedRevision else { throw FileFailure.stale }
        }
        /// No encoding or body write here: one atomic rename is the commit.
        /// A failed or abandoned preparation leaves the prior file untouched.
        @MainActor func commit() throws {
            try validateRevision()
            let destination = store.intentURL(conversation: conversation)
            guard rename(url.path, destination.path) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        }
        deinit {
            let abandoned = url
            DispatchQueue.global(qos: .utility).async { try? FileManager.default.removeItem(at: abandoned) }
        }
    }
    public func removeIntent(conversation: String) throws {
        let url = intentURL(conversation: conversation)
        if FileManager.default.fileExists(atPath: url.path) { try FileManager.default.removeItem(at: url) }
    }
    /// Renames one intent file to another key without reading it, so it needs
    /// no free space. A missing file is not an error.
    func moveIntent(from conversation: String, to other: String) throws {
        try move(intentURL(conversation: conversation), to: intentURL(conversation: other))
    }
    /// Renames a conversation's staged attachment bodies to another key.
    func moveStagedBodies(from conversation: String, to other: String) throws {
        try move(stagedBodies(conversation: conversation).directory, to: stagedBodies(conversation: other).directory)
    }
    private func move(_ source: URL, to destination: URL) throws {
        guard FileManager.default.fileExists(atPath: source.path) else { return }
        guard rename(source.path, destination.path) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
    }
    public func loadIntent(conversation: String) throws -> Data? {
        let url = intentURL(conversation: conversation)
        guard FileManager.default.fileExists(atPath: url.path) else { return nil }
        return try Data(contentsOf: url)
    }
    /// An atomic intent replacement gets a new file identity. A small draft
    /// overlay can use this token to avoid restoring text from an older intent
    /// after Send or another full composer mutation has committed.
    public func intentRevision(conversation: String) throws -> String {
        try fileRevision(at: intentURL(conversation: conversation))
    }
    private func fileRevision(at url: URL) throws -> String {
        guard FileManager.default.fileExists(atPath: url.path) else { return "missing" }
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        if let number = attributes[.systemFileNumber] as? NSNumber,
           let modified = attributes[.modificationDate] as? Date,
           let size = attributes[.size] as? NSNumber {
            return "\(number):\(modified.timeIntervalSince1970.bitPattern):\(size)"
        }
        // File identity is unavailable on some test filesystems. Keep the
        // ordering guarantee there even if checking the token costs more.
        return SHA256.hash(data: try Data(contentsOf: url)).map { String(format: "%02x", $0) }.joined()
    }
    private func intentURL(conversation: String) -> URL {
        directory.appendingPathComponent("intent-" + intentName(conversation) + ".json")
    }
    private func intentName(_ conversation: String) -> String {
        SHA256.hash(data: Data(conversation.utf8)).map { String(format: "%02x", $0) }.joined()
    }
    /// Where this conversation's staged attachment bodies live, beside its intent.
    public func stagedBodies(conversation: String) -> StagedFileBodies {
        StagedFileBodies(directory: directory.appendingPathComponent("staged-" + intentName(conversation), isDirectory: true))
    }
    private func write(_ data: Data, name: String) throws {
        try prepareDirectory()
        try writeFile(data, name: name)
    }
    private func prepareDirectory() throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var url = directory
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try url.setResourceValues(values)
    }
    /// `IntentWriter` prepares the directory once. If it disappears later the
    /// partition was removed, so a queued small write must not recreate it.
    fileprivate func writeIntent(_ data: Data, key: String, prepared: inout Bool) throws {
        if !prepared { try prepareDirectory(); prepared = true }
        try writeFile(data, name: intentURL(conversation: key).lastPathComponent)
    }
    fileprivate static func positionKey(_ conversation: String) -> String { "reading-position-v1:" + conversation }
    private func writeFile(_ data: Data, name: String) throws {
        #if os(iOS)
        try data.write(to: directory.appendingPathComponent(name), options: [.atomic, .completeFileProtection])
        #else
        try data.write(to: directory.appendingPathComponent(name), options: .atomic)
        #endif
    }
}

/// Complete file protection makes the saved chats unreadable and unwritable
/// while the phone is locked, for example while voice talks on the lock screen.
/// The iOS app reports availability here. Writers check it before encoding,
/// keep only their newest state while it is unavailable, and write that state,
/// in the order they paused, once it is available again.
public final class ProtectedDataGate: @unchecked Sendable {
    public static let shared = ProtectedDataGate()
    private let lock = NSLock()
    private var available: Bool
    private var waiting: [@Sendable () -> Void] = []
    /// Counts losses of availability, so a write that failed while the phone
    /// locked is deferred rather than reported as a storage failure.
    private var losses: UInt64 = 0
    public init(available: Bool = true) { self.available = available }
    public var isAvailable: Bool { lock.withLock { available } }
    var epoch: UInt64 { lock.withLock { losses } }
    public func update(available: Bool) {
        let resume: [@Sendable () -> Void] = lock.withLock {
            if self.available, !available { losses &+= 1 }
            self.available = available
            guard available else { return [] }
            defer { waiting = [] }
            return waiting
        }
        for action in resume { action() }
    }
    /// Keeps `action` until data is available and returns true; returns false
    /// without keeping it when data is available now.
    func waitIfUnavailable(_ action: @escaping @Sendable () -> Void) -> Bool {
        lock.withLock {
            guard !available else { return false }
            waiting.append(action)
            return true
        }
    }
    /// Whether a write that started at `epoch` may have met locked data.
    func interrupted(since epoch: UInt64) -> Bool {
        lock.withLock { !available || losses != epoch }
    }
}

/// Writes the read projection away from the caller's thread. Replay/refresh can
/// replace the state several times a second, so writes are coalesced to the
/// newest pending state. A write costs the size of what changed (the changed
/// turns, their manifests and the index), so nothing throttles it by size. Each
/// commit is one atomic replacement of an index after its changed segments are
/// durable. The queue guarantees an old model's final write precedes a
/// replacement's load. Writes serialize off MainActor.
public final class ProjectionWriter: @unchecked Sendable {
    public let store: ReadStore
    // Replacement models can own the same partition. Serialize their loads,
    // final writes and removals without blocking the UI during ownership changes.
    private static let worker = DispatchQueue(label: "wonder.read-cache.writer", qos: .utility)
    private var queue: DispatchQueue { Self.worker }
    private let lock = NSLock()
    private let delay: DispatchTimeInterval
    private let gate: ProtectedDataGate
    private let completion: @Sendable (Result<Int, Error>, TimeInterval) -> Void
    private var pending: ProjectionState?
    private var scheduled = false
    private var retired = false
    /// A resume is registered with the gate; it writes the newest pending state.
    private var deferred = false
    /// Only the timer armed last may drain; a flush cancels it.
    private var timer: UInt64 = 0

    /// `completion` runs on the writer queue after each write with the encoded
    /// size (or error) and the elapsed write time. A write deferred because the
    /// phone locked is not reported; it is retried when data is available.
    public init(store: ReadStore, delay: DispatchTimeInterval = .milliseconds(750), gate: ProtectedDataGate = .shared,
                completion: @escaping @Sendable (Result<Int, Error>, TimeInterval) -> Void = { _, _ in }) {
        self.store = store
        self.delay = delay
        self.gate = gate
        self.completion = completion
    }

    public func schedule(_ state: ProjectionState) {
        lock.lock(); defer { lock.unlock() }
        guard !retired else { return }
        pending = state
        guard !scheduled else { return }
        scheduled = true
        arm(.now() + delay)
    }

    /// Lock held.
    private func arm(_ deadline: DispatchTime) {
        timer &+= 1
        let armed = timer
        queue.asyncAfter(deadline: deadline) { [self] in fire(armed) }
    }

    private func fire(_ armed: UInt64) {
        lock.lock()
        guard armed == timer, scheduled else { lock.unlock(); return }
        lock.unlock()
        drain()
    }

    /// Persists the newest pending state now, without waiting for the delay.
    /// While protected data is unavailable it returns at once and the state
    /// is written when data is available again.
    public func flush(then done: @escaping @Sendable () -> Void = {}) {
        queue.async { [self] in drain(); done() }
    }

    public func flush() async {
        await withCheckedContinuation { continuation in
            flush(then: { continuation.resume() })
        }
    }

    public static func load(_ store: ReadStore) async throws -> ProjectionState {
        try await withCheckedThrowingContinuation { continuation in
            worker.async { continuation.resume(with: Result { try store.load() }) }
        }
    }

    public static func loadConversation(_ store: ReadStore, conversation: String,
                                        record: CachedConversationBody) async throws -> ConversationSnapshot {
        try await withCheckedThrowingContinuation { continuation in
            worker.async { continuation.resume(with: Result { try store.loadConversation(conversation, record: record) }) }
        }
    }

    /// Flush before evicting. If locked or a write failed, only versions that
    /// really reached disk may leave memory. No checkpoint holds their bodies.
    public func durableBodies() async -> [String: UUID] {
        await withCheckedContinuation { continuation in
            queue.async { [self] in
                drain()
                // A cold offline owner may not have written anything yet.
                // Consult the current disk index, never a body-holding checkpoint
                // or stale success from before another owner removed a file.
                continuation.resume(returning: (try? store.durableBodyIDs()) ?? [:])
            }
        }
    }

    /// Stops accepting writes, drops pending ones and removes the cache
    /// directory after any write already in progress.
    public func remove() async throws {
        lock.withLock { retired = true; pending = nil }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            queue.async { [self] in continuation.resume(with: Result { try store.remove() }) }
        }
    }

    /// Stops accepting writes and queues the newest accepted state for saving.
    public func retire() {
        lock.withLock { retired = true }
        flush(then: {})
    }

    private func resume() {
        queue.async { [self] in
            lock.withLock { deferred = false }
            drain()
        }
    }

    /// Worker only.
    private func drain() {
        lock.lock()
        scheduled = false
        timer &+= 1
        guard let state = pending, !deferred else { lock.unlock(); return }
        // Pause before encoding: locked data cannot be written, and only the
        // newest state is kept until it can.
        if gate.waitIfUnavailable({ [self] in resume() }) { deferred = true; lock.unlock(); return }
        pending = nil
        let epoch = gate.epoch
        lock.unlock()
        let start = Date()
        let result = Result { try store.save(state) }
        let elapsed = Date().timeIntervalSince(start)
        var report = true
        lock.withLock {
            switch result {
            case .success: break
            case .failure:
                // A failed atomic write preserves the prior cache. Keep the newest
                // accepted state available for an explicit flush/retry, unless a
                // newer state or removal already superseded it.
                if pending == nil, !retired { pending = state }
                // The phone locked during the write: persistence is deferred,
                // not a storage failure.
                if gate.interrupted(since: epoch) {
                    report = false
                    if !deferred, pending != nil {
                        if gate.waitIfUnavailable({ [self] in resume() }) { deferred = true }
                        else { queue.async { [self] in drain() } }
                    }
                }
            }
        }
        if report { completion(result, elapsed) }
    }
}

/// Writes small intent files, such as reading positions, away from the
/// caller's thread, in order on one serial queue. Each key keeps only its
/// newest value, so an older queued value never lands after a newer one, and
/// `flush` returns after every value accepted before it was written or
/// deferred. This is separate from `ProjectionWriter`, whose queue owns the
/// disposable cache. While protected data is unavailable values wait, newest
/// per key, and are written when it is available again.
public final class IntentWriter: @unchecked Sendable {
    public let store: ReadStore
    private static let serial = DispatchQueue(label: "wonder.intent.writer", qos: .utility)
    private let gate: ProtectedDataGate
    private let completion: @Sendable (String, Result<Void, Error>) -> Void
    private let lock = NSLock()
    private var pending: [String: (revision: UInt64, data: Data)] = [:]
    private var revision: UInt64 = 0
    private var draining = false
    private var deferred = false
    private var discarded = false
    // Accessed only on the serial queue.
    private var prepared = false

    /// `completion` runs on the writer queue with the key of each finished
    /// write. A write deferred because the phone locked is not reported.
    public init(store: ReadStore, gate: ProtectedDataGate = .shared,
                completion: @escaping @Sendable (String, Result<Void, Error>) -> Void = { _, _ in }) {
        self.store = store
        self.gate = gate
        self.completion = completion
    }

    public func save(_ data: Data, key: String) {
        lock.withLock {
            guard !discarded else { return }
            revision &+= 1
            pending[key] = (revision, data)
            startDrain()
        }
    }

    /// Reports completion with `positionKey(conversation:)`.
    public func savePosition(_ position: String, conversation: String) {
        guard let data = try? JSONEncoder().encode(position) else { return }
        save(data, key: ReadStore.positionKey(conversation))
    }
    public static func positionKey(conversation: String) -> String { ReadStore.positionKey(conversation) }

    public func flush() async {
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            Self.serial.async { continuation.resume() }
        }
    }

    /// Drops values not yet written and returns after any write in progress,
    /// so the partition can be removed without a late write recreating it.
    public func discard() async {
        lock.withLock { discarded = true; pending = [:] }
        await flush()
    }

    /// Lock held.
    private func startDrain() {
        guard !draining, !deferred else { return }
        draining = true
        Self.serial.async { [self] in drain() }
    }

    private func resume() {
        lock.withLock { deferred = false; startDrain() }
    }

    /// Serial queue only.
    private func drain() {
        while true {
            lock.lock()
            guard let next = pending.min(by: { $0.value.revision < $1.value.revision }), !deferred else {
                draining = false; lock.unlock(); return
            }
            let key = next.key, entry = next.value
            if gate.waitIfUnavailable({ [self] in resume() }) {
                deferred = true; draining = false; lock.unlock(); return
            }
            pending[key] = nil
            let epoch = gate.epoch
            lock.unlock()
            let result = Result { try store.writeIntent(entry.data, key: key, prepared: &prepared) }
            let report: Bool = lock.withLock {
                guard case .failure = result, gate.interrupted(since: epoch) else { return true }
                // Locked during the write: retry this value unless a newer one
                // or a discard replaced it.
                if pending[key] == nil, !discarded { pending[key] = entry }
                return false
            }
            if report { completion(key, result) }
        }
    }
}

/// The provider's current process state, read separately from durable turn outcomes.
/// Missing, unloaded, and unfamiliar native states remain unknown.
public struct NativeThreadStatus: Codable, Equatable, Sendable {
    public let type: String
    public let activeFlags: [String]?
    public var isWorking: Bool? {
        switch type {
        case "active": true
        case "idle": false
        default: nil
        }
    }
}

/// What a chat is doing, derived in one place from every source that can say so.
/// The sources are the open history's active turn (kept current by streamed
/// events and re-reads), a message accepted but not yet assigned a turn, and the
/// Mac's own answer to the activity check. An answer is only as good as its age,
/// so it expires instead of lingering after the check that fed it stopped, and a
/// failed check keeps the last answer until it expires rather than flickering.
public enum ThreadActivity: Equatable, Sendable {
    case idle
    case working(Source, since: Date?)
    case needsInput(since: Date?)
    /// The activity check failed and no other source says work is running.
    case unknown(since: Date)

    public enum Source: Equatable, Sendable {
        /// The history's newest turn is in progress.
        case turn
        /// A message was accepted and has no turn yet.
        case pendingMessage
        /// The Mac reports the thread active (a turn started there or by a task).
        case computer
        /// Only the Chats list's row says so (the Mac's list read); the watchdog
        /// re-reads such rows until they agree with the thread.
        case row
    }

    /// The Mac's last answer for one chat. `status` is nil when the check said nothing usable.
    public struct HostReading: Equatable, Sendable {
        public let status: NativeThreadStatus?
        public let answeredAt: Date
        /// When the check began failing after `answeredAt`; nil while it answers.
        public let failedSince: Date?
        public init(status: NativeThreadStatus?, answeredAt: Date, failedSince: Date? = nil) {
            self.status = status; self.answeredAt = answeredAt; self.failedSince = failedSince
        }
        /// This reading after one more failed check: it keeps its answer and its age.
        public func failing(at now: Date) -> HostReading {
            HostReading(status: status, answeredAt: answeredAt, failedSince: failedSince ?? now)
        }
        public func expiry() -> Date { answeredAt.addingTimeInterval(ThreadActivity.hostReadingLifetime) }
        public func isFresh(at now: Date) -> Bool { now < expiry() }
    }

    /// Longer than the slowest check cadence (15 s with a live stream) plus a
    /// slow answer, so a healthy check never lets it lapse, and short enough that
    /// a check that stopped (chat closed, phone backgrounded) can't leave "Working" up.
    public static let hostReadingLifetime: TimeInterval = 45

    public var isBusy: Bool {
        switch self {
        case .working, .needsInput: true
        case .idle, .unknown: false
        }
    }
    public var needsInput: Bool {
        if case .needsInput = self { return true }
        return false
    }

    /// The one place that decides. `turnActive`/`pendingMessage` come from the
    /// history; `host` is the activity check's last reading.
    public static func derive(turnActive: Bool, turnStartedAt: Date? = nil, pendingMessage: Bool,
                              host: HostReading?, rowWorking: Bool = false, now: Date) -> ThreadActivity {
        let fresh = host.flatMap { $0.isFresh(at: now) ? $0 : nil }
        let hostWorking = fresh?.status?.isWorking == true
        let waiting = hostWorking && fresh?.status?.activeFlags?.contains(where: {
            ["waitingOnApproval", "waitingOnUserInput"].contains($0)
        }) == true
        if waiting { return .needsInput(since: turnStartedAt) }
        if turnActive { return .working(.turn, since: turnStartedAt) }
        if pendingMessage { return .working(.pendingMessage, since: nil) }
        if hostWorking { return .working(.computer, since: nil) }
        if rowWorking { return .working(.row, since: nil) }
        // Nothing says work is running. A check that is failing, or whose last
        // answer lapsed while failing, is unknown rather than idle.
        if let failed = host?.failedSince { return .unknown(since: failed) }
        return .idle
    }
}
