import Foundation
import CryptoKit

// A saved conversation is a small manifest naming immutable, content-addressed
// segment files: one per turn, holding that turn with the messages, assistant
// messages and message records that belong to it. A streaming save changes one
// turn, so it writes that segment, the manifest and the index, never the rest
// of the history. Unchanged segments keep their file name, and a segment whose
// content already exists on disk is not written again.

/// One turn of a saved conversation and what belongs to it. Messages that name
/// no loaded turn share a segment per turn id (the empty id for unassigned ones).
struct HistorySegment: Codable, Equatable, Sendable {
    var turn: ReadTurn?
    var messages: [ConversationMessage] = []
    var assistantMessages: [AssistantMessage] = []
    var records: [MessageRecord] = []
}

/// A snapshot split into segments, plus the run lengths that restore the
/// original order of each list across them.
struct HistoryPlan: Sendable {
    private(set) var keys: [String] = []
    private(set) var segments: [String: HistorySegment] = [:]
    private(set) var hasTurns = false
    private(set) var hasRecords = false
    /// Flat (segment index, count) pairs in list order.
    private(set) var messageRuns: [Int] = []
    private(set) var assistantRuns: [Int] = []
    private(set) var recordRuns: [Int] = []

    init(_ snapshot: ConversationSnapshot) {
        var index: [String: Int] = [:]
        var turnKeys: [String: String] = [:]
        func segment(_ key: String) -> Int {
            if let known = index[key] { return known }
            index[key] = keys.count; keys.append(key); segments[key] = HistorySegment()
            return keys.count - 1
        }
        if let turns = snapshot.thread.turns {
            hasTurns = true
            for (position, turn) in turns.enumerated() {
                let key = turnKeys[turn.id] == nil ? "t:" + turn.id : "t:" + turn.id + "#\(position)"
                if turnKeys[turn.id] == nil { turnKeys[turn.id] = key }
                _ = segment(key)
                segments[key]?.turn = turn
            }
        }
        func owner(_ turnID: String?) -> Int {
            let id = turnID ?? ""
            return segment(turnKeys[id] ?? "o:" + id)
        }
        var runs: (messages: [Int], assistant: [Int], records: [Int]) = ([], [], [])
        func note(_ runs: inout [Int], _ position: Int) {
            if runs.count >= 2, runs[runs.count - 2] == position { runs[runs.count - 1] += 1 }
            else { runs.append(position); runs.append(1) }
        }
        for message in snapshot.messages {
            let position = owner(message.codexTurnId); note(&runs.messages, position)
            segments[keys[position]]?.messages.append(message)
        }
        for message in snapshot.assistantMessages {
            let position = owner(message.codexTurnId); note(&runs.assistant, position)
            segments[keys[position]]?.assistantMessages.append(message)
        }
        if let records = snapshot.messageRecords {
            hasRecords = true
            for record in records {
                let position = owner(record.turnId); note(&runs.records, position)
                segments[keys[position]]?.records.append(record)
            }
        }
        messageRuns = runs.messages; assistantRuns = runs.assistant; recordRuns = runs.records
    }

    /// Segments read back in manifest order (keys are positions here).
    init(manifest: HistoryManifest, loaded: [HistorySegment]) {
        keys = loaded.indices.map(String.init)
        segments = Dictionary(uniqueKeysWithValues: zip(keys, loaded))
        hasTurns = manifest.hasTurns; hasRecords = manifest.hasRecords
        messageRuns = manifest.messageRuns; assistantRuns = manifest.assistantRuns; recordRuns = manifest.recordRuns
    }

    func manifest(conversationId: String, files: [String]) -> HistoryManifest {
        HistoryManifest(conversationId: conversationId, files: files, hasTurns: hasTurns, hasRecords: hasRecords,
                        messageRuns: messageRuns, assistantRuns: assistantRuns, recordRuns: recordRuns)
    }

    /// Nil when the segments do not add up to the runs the manifest recorded.
    func body() -> (messages: [ConversationMessage], assistant: [AssistantMessage],
                    turns: [ReadTurn]?, records: [MessageRecord]?)? {
        func interleave<T>(_ runs: [Int], _ pick: (HistorySegment) -> [T]) -> [T]? {
            var cursors = [Int](repeating: 0, count: keys.count)
            var result: [T] = []
            var position = 0
            while position + 1 < runs.count {
                let (segment, count) = (runs[position], runs[position + 1]); position += 2
                guard keys.indices.contains(segment), let body = segments[keys[segment]] else { return nil }
                let items = pick(body)
                // Runs come from a manifest on disk; a torn one must read as uncached, never trap.
                guard count >= 0, count <= items.count - cursors[segment] else { return nil }
                result += items[cursors[segment]..<cursors[segment] + count]
                cursors[segment] += count
            }
            return runs.count.isMultiple(of: 2) ? result : nil
        }
        guard let messages = interleave(messageRuns, { $0.messages }),
              let assistant = interleave(assistantRuns, { $0.assistantMessages }),
              let records = interleave(recordRuns, { $0.records }) else { return nil }
        let turns = hasTurns ? keys.compactMap { segments[$0]?.turn } : nil
        return (messages, assistant, turns, hasRecords ? records : nil)
    }
}

/// Names a conversation's segments in order. Small: one digest per segment.
struct HistoryManifest: Codable, Equatable, Sendable {
    let conversationId: String
    /// Segment content digests, in segment order; `segmentName` makes the file name.
    let files: [String]
    let hasTurns: Bool
    let hasRecords: Bool
    let messageRuns: [Int]
    let assistantRuns: [Int]
    let recordRuns: [Int]

    static func manifestName(_ record: CachedConversationBody) -> String { "history-" + record.bodyID.uuidString + ".json" }
    static func digest(_ data: Data) -> String {
        SHA256.hash(data: data).prefix(16).map { String(format: "%02x", $0) }.joined()
    }
    static func segmentName(_ digest: String) -> String { "segment-" + digest + ".json" }
    var segmentNames: [String] { files.map(Self.segmentName) }
    static func owns(_ name: String) -> Bool {
        (name.hasPrefix("history-") || name.hasPrefix("segment-")) && name.hasSuffix(".json")
    }
}

/// What this process knows about the files on disk, so a save compares against
/// memory instead of reading the index back or encoding unchanged history. The
/// entry for a conversation is valid only while its index record still names the
/// same body; a changed index file on disk (another owner, a removal) discards
/// everything and is read again.
final class HistoryIndex: @unchecked Sendable {
    struct Entry {
        let bodyID: UUID
        let manifest: HistoryManifest
        /// The segments last written or loaded for a resident conversation, to
        /// find the changed ones. Dropped once the conversation is not resident.
        var plan: HistoryPlan?
    }
    let lock = NSLock()
    var indexRevision: String?
    var entries: [String: Entry] = [:]
    var primed = false

    private final class Registry: @unchecked Sendable {
        let lock = NSLock()
        var indexes: [String: HistoryIndex] = [:]
    }
    private static let registry = Registry()
    static func at(_ directory: URL) -> HistoryIndex {
        registry.lock.withLock {
            if let known = registry.indexes[directory.path] { return known }
            let created = HistoryIndex()
            registry.indexes[directory.path] = created
            return created
        }
    }
    static func forget(_ directory: URL) { registry.lock.withLock { registry.indexes[directory.path] = nil } }
    func reset() { indexRevision = nil; entries = [:]; primed = false }
}
