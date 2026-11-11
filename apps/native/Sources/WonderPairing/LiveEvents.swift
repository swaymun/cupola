import Foundation

/// Applying streamed events to an open conversation.
///
/// A batching host (see `ReplayEvent.frame`) journals each assistant delta in
/// the same transaction as the text change it describes, and builds snapshot
/// items from the same journaled item events. So a snapshot read at sequence S
/// already contains every such event at or below S and none above it: applying
/// the events above S, in order, gives the snapshot the host would return now.
/// Anything this file cannot reproduce (delivery states, approvals, settings,
/// host-wide changes) still marks the conversation stale for an authoritative
/// read, which also reconciles everything when a turn ends.
///
/// The open conversation is updated in place: a snapshot finds the message or
/// item an event names through its `LiveIndex` (interned IDs, no scan), changes
/// that one element, and says what it changed (`LiveChange`) so the rows built
/// from it refresh just that part. Applying an event costs the same in a
/// 3,000-item chat as in an empty one, provided the snapshot's storage is not
/// shared; the model drops its other copies of the open chat before applying.
public struct LiveChange: Equatable, Sendable {
    public enum Kind: Equatable, Sendable {
        /// Only the sequence moved; nothing a row shows changed.
        case sequence
        /// `assistantMessages[slot]` grew, finished or was added.
        case reply(slot: Int, added: Bool)
        /// `thread.turns[turn].items[slot]` was updated or added.
        case item(turn: Int, slot: Int, added: Bool)
        /// Anything wider (a new turn, record labels): rebuild the rows.
        case structural
    }
    public let kind: Kind
    /// How much the conversation's resident size estimate moved.
    public let residentBytes: Int
    /// Whether the event changed what is saved for the conversation.
    public var changesContent: Bool { kind != .sequence }
    /// Whether it changed a thread item, which activity summaries read.
    public var touchesItems: Bool {
        switch kind {
        case .item, .structural: true
        case .sequence, .reply: false
        }
    }
}

/// Finds messages, turns and items by the IDs events name. IDs are interned
/// once into small integers, so a lookup hashes two integers instead of
/// comparing strings through arrays, and the first match wins exactly as the
/// scans it replaces did.
struct LiveIndex: Sendable {
    private var symbols: [String: UInt32] = [:]
    private var replies: [UInt64: Int] = [:]
    private var turns: [UInt32: Int] = [:]
    private var items: [UInt64: Int] = [:]

    init(_ snapshot: ConversationSnapshot) {
        for (slot, message) in snapshot.assistantMessages.enumerated() {
            guard let turn = message.codexTurnId, let item = message.itemId else { continue }
            addReply(turn: turn, item: item, slot: slot)
        }
        for (slot, turn) in (snapshot.thread.turns ?? []).enumerated() {
            guard addTurn(turn.id, slot: slot) else { continue }
            for (position, item) in turn.items.enumerated() { addItem(turn: turn.id, item: item.id, slot: position) }
        }
    }

    private mutating func symbol(_ id: String) -> UInt32 {
        if let known = symbols[id] { return known }
        let next = UInt32(truncatingIfNeeded: symbols.count)
        symbols[id] = next
        return next
    }
    private static func pair(_ first: UInt32, _ second: UInt32) -> UInt64 { UInt64(first) << 32 | UInt64(second) }
    private func pair(_ first: String, _ second: String) -> UInt64? {
        guard let first = symbols[first], let second = symbols[second] else { return nil }
        return Self.pair(first, second)
    }

    func reply(turn: String, item: String) -> Int? { pair(turn, item).flatMap { replies[$0] } }
    func turnSlot(_ turn: String) -> Int? { symbols[turn].flatMap { turns[$0] } }
    func itemSlot(turn: String, item: String) -> Int? { pair(turn, item).flatMap { items[$0] } }

    mutating func addReply(turn: String, item: String, slot: Int) {
        let key = Self.pair(symbol(turn), symbol(item))
        if replies[key] == nil { replies[key] = slot }
    }
    /// False for a repeated turn ID: the first turn with an ID owns its items.
    @discardableResult mutating func addTurn(_ turn: String, slot: Int) -> Bool {
        let key = symbol(turn)
        guard turns[key] == nil else { return false }
        turns[key] = slot
        return true
    }
    mutating func addItem(turn: String, item: String, slot: Int) {
        let key = Self.pair(symbol(turn), symbol(item))
        if items[key] == nil { items[key] = slot }
    }
}

extension ConversationSnapshot {
    /// This snapshot with `event` applied, or nil when the event needs an
    /// authoritative read instead.
    public func applyingLive(_ event: ReplayEvent) -> ConversationSnapshot? {
        var next = self
        return next.applyLive(event) == nil ? nil : next
    }

    /// Applies `event` in place and says what changed. Returns nil, leaving
    /// the snapshot as it was, when the event needs an authoritative read.
    @discardableResult
    public mutating func applyLive(_ event: ReplayEvent) -> LiveChange? {
        guard event.hostEpoch == hostEpoch, event.sequence > lastSequence,
              event.conversationId == conversationId else { return nil }
        switch event.event.type {
        case "assistant_delta":
            guard let turn = event.turnId, let item = event.itemId, let text = event.event.text, !text.isEmpty else { return nil }
            return write(reply: turn, item: item, at: event, completed: false, text: text)
        case "assistant_completed":
            guard let turn = event.turnId, let item = event.itemId, let text = event.event.text else { return nil }
            return write(reply: turn, item: item, at: event, completed: true, text: text)
        case "activity" where event.event.category == "projection_changed":
            // The stored text of an assistant message changed. Its delta or
            // completion event, which carries the change, follows; nothing
            // else writes that text without one.
            guard Self.invalidatedResource(event.event.detail) == "assistant_messages" else { return nil }
            return advance(to: event.sequence)
        case "activity" where event.event.category == "native_history" || event.event.category == "native_status":
            // Another app wrote this chat on the Mac, or its work there started,
            // waits or ended: a wake-up for the open chat's history check (see
            // RunningElsewhereRefresh) and the sidebar, not a change to this snapshot.
            return advance(to: event.sequence)
        case "activity" where event.event.category == "thread_item_upsert":
            guard let upsert = LiveItemUpsert(event.event.detail), thread.turns != nil else { return nil }
            return write(upsert.item(createdAt: event.occurredAt), turn: upsert.turnId, at: event,
                         observedLifecycle: upsert.lifecycleAuthority == 2)
        case "activity" where event.event.category == "message_records":
            // The Mac now knows records the phone may have saved as sent
            // messages' (a newer read that leaves them out keeps them).
            struct Records: Decodable { let records: [MessageRecord] }
            guard let data = event.event.detail?.data(using: .utf8),
                  let records = try? JSONDecoder().decode(Records.self, from: data).records else { return nil }
            let kept = MessageRecord.merging(records, over: messageRecords)
            if thread.turns != nil { thread = thread.labeling(kept ?? []) }
            messageRecords = kept
            lastSequence = event.sequence
            live.revision &+= 1
            return LiveChange(kind: .structural, residentBytes: 0)
        default:
            return nil
        }
    }

    static func invalidatedResource(_ detail: String?) -> String? {
        struct Invalidation: Decodable { let resource: String? }
        guard let data = detail?.data(using: .utf8) else { return nil }
        return (try? JSONDecoder().decode(Invalidation.self, from: data))?.resource
    }

    /// Moves the sequence on. Message records stay what they were, normalised
    /// the way every live update has always left them.
    private mutating func advance(to sequence: UInt64, bytes: Int = 0, _ kind: LiveChange.Kind = .sequence) -> LiveChange {
        messageRecords = MessageRecord.merging(nil, over: messageRecords)
        lastSequence = sequence
        live.revision &+= 1
        return LiveChange(kind: kind, residentBytes: bytes)
    }

    private mutating func write(reply turn: String, item: String, at event: ReplayEvent, completed: Bool, text: String) -> LiveChange {
        if live.index == nil { live.index = LiveIndex(self) }
        let state = completed ? "completed" : "streaming"
        guard let slot = live.index!.reply(turn: turn, item: item) else {
            // The host's own identity for this message, so a later read replaces it.
            let message = AssistantMessage(itemId: item, codexTurnId: turn, messageId: "assistant:\(turn):\(item)", text: text,
                                           state: state, createdAt: event.occurredAt, updatedAt: event.occurredAt)
            assistantMessages.append(message)
            live.index!.addReply(turn: turn, item: item, slot: assistantMessages.count - 1)
            return advance(to: event.sequence, bytes: Self.residentCost(of: message), .reply(slot: assistantMessages.count - 1, added: true))
        }
        let old = assistantMessages[slot]
        var bytes = 0
        if completed {
            bytes = (text.utf8.count - old.text.utf8.count) * 2
            assistantMessages[slot].text = text
        } else if old.state != "completed" {
            // A completed message keeps its final text; a late delta cannot reopen it.
            bytes = text.utf8.count * 2
            assistantMessages[slot].text.append(text)
        }
        assistantMessages[slot].state = old.state == "completed" ? "completed" : state
        assistantMessages[slot].updatedAt = event.occurredAt
        return advance(to: event.sequence, bytes: bytes, .reply(slot: slot, added: false))
    }

    private mutating func write(_ item: ReadItem, turn turnID: String, at event: ReplayEvent, observedLifecycle: Bool) -> LiveChange {
        if live.index == nil { live.index = LiveIndex(self) }
        var item = item
        let turnSlot = live.index!.turnSlot(turnID)
        let itemSlot = turnSlot.flatMap { _ in live.index!.itemSlot(turn: turnID, item: item.id) }
        if item.type == "reasoning", observedLifecycle {
            let old = turnSlot.flatMap { turn in itemSlot.map { thread.turns![turn].items[$0] } }
            item.payload?["thinkingTiming"] = ThinkingTiming.observing(old?.payload?["thinkingTiming"], state: item.state, at: event)
        }
        guard let turn = turnSlot else {
            // An item still starting, waiting or streaming means the turn is running; its
            // end arrives as a lifecycle event that needs a read. A finished first item
            // says nothing about the turn, so that stays unknown until the next read.
            let running = (1..<4).contains(Self.rank(item.state))
            thread.turns!.append(ReadTurn(id: turnID, items: [item], startedAt: event.occurredAt,
                                          status: running ? "inProgress" : "unknown"))
            let slot = thread.turns!.count - 1
            live.index!.addTurn(turnID, slot: slot)
            live.index!.addItem(turn: turnID, item: item.id, slot: 0)
            let change = advance(to: event.sequence, bytes: 256 + Self.residentCost(of: item), .structural)
            labelTurn(slot)
            return change
        }
        let change: LiveChange
        if let existing = itemSlot {
            let old = thread.turns![turn].items[existing]
            // Item lifecycles only move forward, as in the host's projection.
            guard Self.rank(item.state) >= Self.rank(old.state) else { return advance(to: event.sequence) }
            var payload = item.payload
            if payload?["phase"] == nil, let phase = old.payload?["phase"] { payload?["phase"] = phase }
            if item.type == "reasoning", payload?["thinkingTiming"] == nil {
                payload?["thinkingTiming"] = old.payload?["thinkingTiming"]
            }
            let merged = ReadItem(id: item.id, type: item.type, state: item.state, text: item.text ?? old.text,
                                  createdAt: old.createdAt, payload: payload)
            thread.turns![turn].items[existing] = merged
            change = advance(to: event.sequence, bytes: Self.residentCost(of: merged) - Self.residentCost(of: old),
                             .item(turn: turn, slot: existing, added: false))
        } else {
            thread.turns![turn].items.append(item)
            let slot = thread.turns![turn].items.count - 1
            live.index!.addItem(turn: turnID, item: item.id, slot: slot)
            change = advance(to: event.sequence, bytes: Self.residentCost(of: item), .item(turn: turn, slot: slot, added: true))
        }
        labelTurn(turn)
        return change
    }

    /// Labels the one turn a live item update changed with the message
    /// records kept so far, so an update re-sending a record keeps its bubble.
    /// New records label every loaded turn they name (`message_records`).
    private mutating func labelTurn(_ slot: Int) {
        guard let records = messageRecords, !records.isEmpty else { return }
        let turnID = thread.turns![slot].id
        var clients = [String: String]()
        for record in records where record.turnId == turnID { clients[record.itemId] = record.clientId }
        guard !clients.isEmpty else { return }
        for position in thread.turns![slot].items.indices {
            let item = thread.turns![slot].items[position]
            guard item.type == "userMessage", let client = clients[item.id],
                  item.payload?["clientId"]?.string != client else { continue }
            var copy = item
            copy.payload = (item.payload ?? [:]).merging(["clientId": .string(client)]) { _, named in named }
            thread.turns![slot].items[position] = copy
        }
    }

    private static func rank(_ state: String) -> Int {
        switch state {
        case "started": return 1
        case "waiting": return 2
        case "streaming": return 3
        case "completed", "failed", "interrupted": return 4
        default: return 0
        }
    }
}

/// A live item event's detail: the item as the host stored it, plus the type
/// and display text a snapshot would give it.
struct LiveItemUpsert: Decodable {
    let turnId: String
    let itemId: String
    let state: String
    let lifecycleAuthority: Int?
    let historyRefresh: Bool?
    let itemType: String?
    let text: String?
    let item: [String: ThreadValue]

    /// History imports need a read: their event time is the refresh time,
    /// while the store preserves when each item first appeared in the chat.
    init?(_ detail: String?) {
        guard let data = detail?.data(using: .utf8),
              let decoded = try? JSONDecoder().decode(Self.self, from: data),
              decoded.historyRefresh != true,
              (decoded.lifecycleAuthority ?? 0) > 0, decoded.itemType != nil else { return nil }
        self = decoded
    }

    func item(createdAt fallback: String) -> ReadItem {
        let type = itemType ?? "unknown"
        var payload = item
        if type == "reasoning" { payload = payload.filter { ["id", "type", "status", "state"].contains($0.key) } }
        if type == "imageView", payload["imageUrl"]?.string?.hasPrefix("data:") == true { payload["imageUrl"] = nil }
        if type == "unknown" { payload = ["originalType": item["type"] ?? .null, "raw": .object(payload)] }
        let created = item["createdAt"]?.string.flatMap { $0.isEmpty ? nil : $0 } ?? fallback
        return ReadItem(id: itemId, type: type, state: state, text: text, createdAt: created, payload: payload)
    }
}

/// Events for one watched conversation since a known sequence, kept so a
/// snapshot that lands after newer events can catch up instead of being read
/// again (during a streamed reply every read is otherwise already stale).
///
/// Bounded by count and by payload bytes: a tool's large output must not keep
/// megabytes of already-applied events alive. Whatever is dropped moves the
/// coverage floor (`since`) past it, so a read older than the floor is never
/// caught up from an incomplete log; it stays stale and is read again.
public struct LiveEventLog: Sendable {
    public private(set) var conversation: String?
    /// Every event for `conversation` (and every host-wide event) after this
    /// sequence is recorded.
    private var since: UInt64 = 0
    private var events: [ReplayEvent] = []
    private var sizes: [Int] = []
    public private(set) var bytes = 0
    private let limit: Int
    private let byteLimit: Int
    public init(limit: Int = 2048, byteLimit: Int = 4 << 20) {
        self.limit = max(2, limit); self.byteLimit = max(1, byteLimit)
    }

    public var count: Int { events.count }

    public mutating func watch(_ conversation: String?, from sequence: UInt64) {
        guard conversation != self.conversation else { return }
        self.conversation = conversation; since = sequence; events = []; sizes = []; bytes = 0
    }

    public mutating func record(_ event: ReplayEvent) {
        guard conversation != nil else { return }
        let target = event.conversationId.flatMap { $0.isEmpty ? nil : $0 }
        // A host-wide event stays as a barrier: it cannot be applied, so a
        // catch-up across it leaves the conversation for a fresh read.
        guard target == conversation || (target == nil && !event.event.isHostRuntimeNotice) else { return }
        let size = Self.size(of: event)
        guard size <= byteLimit else {
            // Too large to keep: nothing at or before it can be replayed.
            release(through: event.sequence)
            return
        }
        events.append(event); sizes.append(size); bytes += size
        if events.count > limit { drop(events.count - limit / 2) }
        if bytes > byteLimit {
            // Trim to half the budget, as the count overflow trims to half the count.
            var drop = 0, kept = bytes
            while drop < events.count, kept > byteLimit / 2 { kept -= sizes[drop]; drop += 1 }
            self.drop(drop)
        }
    }

    /// Forgets every event through `sequence`: a read at or after it no longer
    /// needs them, and an older read can no longer catch up (it is read again).
    /// Used once a read is installed and current, and on memory pressure.
    public mutating func release(through sequence: UInt64) {
        guard sequence > since else { return }
        since = sequence
        let drop = events.firstIndex { $0.sequence > sequence } ?? events.count
        events.removeFirst(drop)
        bytes -= sizes.prefix(drop).reduce(0, +)
        sizes.removeFirst(drop)
    }

    private mutating func drop(_ count: Int) {
        guard count > 0 else { return }
        since = max(since, events[count - 1].sequence)
        events.removeFirst(count)
        bytes -= sizes.prefix(count).reduce(0, +)
        sizes.removeFirst(count)
    }

    /// Payload bytes an event keeps alive; fixed fields count as a small constant.
    static func size(of event: ReplayEvent) -> Int {
        let fields: [String?] = [event.eventId, event.conversationId, event.turnId, event.itemId, event.event.detail, event.event.text]
        return fields.reduce(128) { $0 + ($1?.utf8.count ?? 0) }
    }

    /// The recorded events after `sequence`, or nil if some may be missing.
    public func events(after sequence: UInt64) -> [ReplayEvent]? {
        guard sequence >= since else { return nil }
        return events.filter { $0.sequence > sequence }
    }
}

public enum LiveOutcome: Equatable, Sendable {
    /// Already seen, or a host runtime notice that changes no chat.
    case skipped
    /// Applied to this conversation's snapshot, which stays current.
    case applied(String)
    /// Marked stale for an authoritative read (nil: every scope).
    case invalidated(String?)
}

extension ProjectionState {
    /// Consumes one event from a batching host, applying it to a current
    /// snapshot when possible instead of marking that conversation stale.
    public mutating func consumeLive(_ event: ReplayEvent) throws -> LiveOutcome {
        guard event.event.type != "resync_required", event.hostEpoch == hostEpoch else { throw ReadFailure.resync }
        if event.sequence <= lastSequence { return .skipped }
        guard lastSequence < UInt64.max, event.sequence == lastSequence + 1 else { throw ReadFailure.resync }
        let conversation = event.conversationId.flatMap { $0.isEmpty ? nil : $0 }
        if let conversation, snapshotCovers(conversation, event: event) {
            // Still consume list/cursor effects. Attention and other readers
            // process the event independently of snapshot content.
            try consume(event)
            return .applied(conversation)
        }
        if let conversation, !dirty.contains(conversation), let change = resident[conversation]?.applyLive(event) {
            lastSequence = event.sequence
            noteLive(change, in: conversation)
            // A Chats row shows the thread's current step, which a tool item
            // changes; it follows on the list's own cadence. Streamed text
            // changes nothing a row shows, so it reads no list.
            if event.event.type == "activity", event.event.category == "thread_item_upsert" { listDirty = true }
            return .applied(conversation)
        }
        try consume(event)
        if conversation == nil && event.event.isHostRuntimeNotice { return .skipped }
        return .invalidated(conversation)
    }

    /// Installs an authoritative snapshot, then applies the watched events
    /// that arrived while it was being read, so it is current on arrival.
    public mutating func install(_ snapshot: ConversationSnapshot, catchingUp log: LiveEventLog) {
        install(snapshot)
        let id = snapshot.conversationId
        guard log.conversation == id, dirty.contains(id), var current = resident[id],
              current.lastSequence == snapshot.lastSequence, let pending = log.events(after: current.lastSequence) else { return }
        var changedContent = false, bytes = 0
        for event in pending {
            guard let change = current.applyLive(event) else { return }
            changedContent = changedContent || change.changesContent
            bytes += change.residentBytes
        }
        resident[id] = current
        // The read's saved body stays the body unless the events changed content;
        // either way its header follows the sequence, and the size estimate moves by the events'.
        let applied = LiveChange(kind: changedContent ? .structural : .sequence, residentBytes: bytes)
        let saved = bodyRecords?[id]
        bodyRecords?[id] = CachedConversationBody(live: current, after: applied, from: saved)
        liveChanges[id] = nil
        if covers(id, through: current.lastSequence) { markClean(id) }
    }

    /// Records what a live event did to a conversation: the changes its
    /// projection will replay, and the saved body they invalidate.
    private mutating func noteLive(_ change: LiveChange, in conversation: String) {
        var pending = liveChanges[conversation] ?? []
        if pending.count >= Self.liveChangeLimit { pending = [LiveChange(kind: .structural, residentBytes: 0)] }
        else { pending.append(change) }
        liveChanges[conversation] = pending
        guard let snapshot = resident[conversation] else { return }
        let record = CachedConversationBody(live: snapshot, after: change, from: bodyRecords?[conversation])
        bodyRecords?[conversation] = record
    }
}

/// One owner per connection. Reads in the same scope share a task; event
/// bursts retain one trailing operation. Independent attention reads never
/// wait behind a throttled list or a slow history import.
@MainActor public final class RefreshCoordinator {
    public enum Scope: Hashable, Sendable {
        case list, attention
        case conversation(String), detail(String), nativeHistory(String), subagents(String), queue(String), questions(String)
    }
    private final class Lane {
        var read: Task<Void, Error>?
        var reader: UUID?
        var pump: Task<Void, Never>?
        var sleep: Task<Void, Error>?
        var pending: (@MainActor () async -> Void)?
        var lastStarted: ContinuousClock.Instant?
        var cadence: Duration = .zero
        var due: ContinuousClock.Instant = .now
        var urgent = false
    }
    private var lanes: [Scope: Lane] = [:]
    public init() {}
    private func lane(_ scope: Scope) -> Lane {
        if let lane = lanes[scope] { return lane }
        // Keep timing only while it can affect cadence; idle chats do not
        // accumulate an unbounded connection-lifetime scheduler cache.
        let now = ContinuousClock.now
        lanes = lanes.filter { _, lane in
            lane.read != nil || lane.pump != nil || lane.lastStarted.map { now < $0 + lane.cadence } == true
        }
        let lane = Lane(); lanes[scope] = lane; return lane
    }
    /// `afterCurrent` is for a canonical GET after native import: a GET that
    /// started before import completed cannot satisfy it, even if still running.
    public func read(_ scope: Scope, afterCurrent: Bool = false, operation: @escaping @MainActor () async throws -> Void) async throws {
        try Task.checkCancellation()
        let lane = lane(scope)
        if let read = lane.read {
            if afterCurrent {
                _ = try? await read.value
                try Task.checkCancellation()
                try await self.read(scope, operation: operation)
            } else { try await read.value }
            return
        }
        let id = UUID()
        lane.reader = id
        lane.lastStarted = .now
        let task = Task {
            defer { if lane.reader == id { lane.read = nil; lane.reader = nil } }
            try Task.checkCancellation()
            try await operation()
        }
        lane.read = task
        try await task.value
    }
    public func request(_ scope: Scope, cadence: Duration, delay: Duration = .milliseconds(300),
                        urgent: Bool = false, operation: @escaping @MainActor () async -> Void) {
        guard !Task.isCancelled else { return }
        let lane = lane(scope)
        let alreadyPending = lane.pending != nil
        lane.pending = operation
        lane.cadence = cadence
        if !alreadyPending { lane.due = .now + delay }
        lane.urgent = lane.urgent || urgent
        if urgent { lane.due = .now; lane.sleep?.cancel() }
        guard lane.pump == nil else { return }
        lane.pump = Task {
            defer { lane.pump = nil; lane.sleep = nil }
            while lane.pending != nil, !Task.isCancelled {
                // An event that arrived after a read started belongs to the
                // trailing pass, unless the handler finds that read covered it.
                if let read = lane.read { _ = try? await read.value }
                guard !Task.isCancelled, lane.pending != nil else { return }
                let spaced = lane.lastStarted.map { $0 + lane.cadence } ?? lane.due
                let due = lane.urgent ? lane.due : max(lane.due, spaced)
                if ContinuousClock.now < due {
                    let sleep = Task { try await Task.sleep(until: due, clock: .continuous) }
                    lane.sleep = sleep
                    try? await sleep.value
                    lane.sleep = nil
                    continue // An urgent request may have changed the deadline.
                }
                let operation = lane.pending
                lane.pending = nil; lane.urgent = false
                await operation?()
            }
        }
    }
    /// A screen stopped watching: forget its queued progress work. An import
    /// already reading still finishes its canonical GET for other callers.
    public func discardRequests(_ scope: Scope) {
        guard let lane = lanes[scope] else { return }
        lane.pending = nil
        lane.sleep?.cancel()
    }
    public func cancel() {
        for lane in lanes.values {
            lane.pending = nil; lane.sleep?.cancel(); lane.pump?.cancel(); lane.read?.cancel()
        }
        lanes.removeAll()
    }
}
