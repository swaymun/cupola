import Foundation

// A voice session's limits and what the phone reports about it to the Mac
// (`crates/wonderd/src/voice.rs`): connected time and OpenAI's usage for the
// daily budget, and the transcript (text only) when the owner keeps them.

/// When this phone ends a session on its own, before the Mac's limit and
/// before OpenAI closes a Realtime session at 60 minutes.
public enum VoiceSessionLimit {
    public enum Reason: Equatable, Sendable {
        /// The owner's session limit on the Mac.
        case sessionLimit(minutes: Int)
        /// Today's minutes on the Mac run out first.
        case dailyLimit
        /// OpenAI's own cap on one Realtime connection.
        case connectionCap
    }

    /// OpenAI ends a Realtime session after 60 minutes.
    public static let connectionCap: TimeInterval = 60 * 60
    /// End this much earlier, so the session closes on the phone's terms.
    public static let margin: TimeInterval = 5

    /// The deadline for a session whose socket opened at `connectedAt`.
    public static func deadline(grant: VoiceSessionGrant, connectedAt: Date) -> (date: Date, reason: Reason) {
        let cap = (connectedAt.addingTimeInterval(connectionCap - margin), Reason.connectionCap)
        guard let endsAt = grant.sessionEndsAt else { return cap }
        let mac = Date(timeIntervalSince1970: TimeInterval(endsAt) - margin)
        guard mac < cap.0 else { return cap }
        let reason: Reason = grant.sessionEndsFor == "dailyLimit"
            ? .dailyLimit : .sessionLimit(minutes: max(1, (grant.sessionLimitSeconds ?? 0) / 60))
        return (mac, reason)
    }

    /// What the panel says when the session ended at its deadline.
    public static func message(_ reason: Reason, macName: String) -> String {
        switch reason {
        case .sessionLimit(let minutes):
            "Voice reached its \(minutes)-minute session limit. Start voice again to keep talking."
        case .dailyLimit:
            "Today’s voice limit on \(macName) is used up. It resets at midnight, or you can raise it in Voice settings."
        case .connectionCap:
            "OpenAI ends a voice session after an hour. Start voice again to keep talking."
        }
    }

    /// Whether Try again can reconnect in the same panel; after a limit the
    /// Mac refuses that panel, so voice starts a new one.
    public static func panelEnded(_ reason: Reason) -> Bool {
        switch reason {
        case .sessionLimit, .dailyLimit: true
        case .connectionCap: false
        }
    }
}

/// A random voice panel ID the Mac accepts: 8–64 of `[A-Za-z0-9_-]`.
public func makeVoicePanelID() -> String {
    "panel-" + UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(16)
}

/// `GET`/`PUT /api/v1/voice/settings`.
public struct VoiceLimits: Codable, Equatable, Sendable {
    public struct Today: Codable, Equatable, Sendable {
        public var spokenSeconds: Int
        public var estimatedDollars: Double
        public init(spokenSeconds: Int, estimatedDollars: Double) {
            self.spokenSeconds = spokenSeconds; self.estimatedDollars = estimatedDollars
        }
    }
    public var sessionLimitMinutes: Int
    public var sessionLimitMaxMinutes: Int
    public var dailyMinutes: Int?
    public var dailyDollars: Double?
    public var keepTranscripts: Bool
    public var today: Today?
}

/// What the session said and used, reported to the Mac in small batches.
/// Usage events go once each; transcript entries go when they are new and
/// keep their ID, so a repeat replaces rather than duplicates.
public struct VoiceSessionLog: Sendable {
    public struct Entry: Codable, Equatable, Sendable {
        public let id: String
        /// `owner` or `voice`.
        public let speaker: String
        public let text: String
        public let atMs: Int64
    }
    struct UsageEvent: Encodable, Sendable {
        let source: String
        let id: String
        let usage: VoiceJSON
    }

    /// The Mac accepts at most this many usage events or entries per post.
    public static let batchLimit = 200
    private let prefix: String
    private var entries = 0
    private var reportedUsage = Set<String>()
    private var unsentUsage: [UsageEvent] = []
    private var unsentEntries: [Entry] = []
    private var connectedSince: Date?
    private var connectedBefore: TimeInterval = 0

    public init(prefix: String = String(UUID().uuidString.prefix(8))) { self.prefix = prefix }

    public mutating func connected(at date: Date) { if connectedSince == nil { connectedSince = date } }

    public mutating func disconnected(at date: Date) {
        guard let since = connectedSince else { return }
        connectedBefore += max(0, date.timeIntervalSince(since))
        connectedSince = nil
    }

    public func connectedSeconds(at date: Date) -> Int {
        Int(connectedBefore + (connectedSince.map { max(0, date.timeIntervalSince($0)) } ?? 0))
    }

    public var hasUnsentTranscript: Bool { !unsentEntries.isEmpty }

    public mutating func record(_ event: VoiceRealtimeEvent, at date: Date) {
        switch event {
        case .usage(let source, let id, let json):
            guard reportedUsage.insert(source + ":" + id).inserted,
                  let value = try? JSONDecoder().decode(VoiceJSON.self, from: Data(json.utf8)) else { return }
            unsentUsage.append(UsageEvent(source: source, id: id, usage: value))
        case .userTranscript(let text, true): append(speaker: "owner", text: text, at: date)
        case .assistantTranscript(let text, true): append(speaker: "voice", text: text, at: date)
        default: break
        }
    }

    private mutating func append(speaker: String, text: String, at date: Date) {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        entries += 1
        unsentEntries.append(Entry(id: "\(prefix)-\(entries)", speaker: speaker, text: String(text.prefix(8_000)),
                                   atMs: Int64(date.timeIntervalSince1970 * 1000)))
    }

    /// `POST /api/v1/voice/panels/{id}/usage`. Taken events are gone from the
    /// log; `restore` puts them back when the post fails.
    public mutating func takeUsage(at date: Date) -> (body: Data, sent: VoiceSessionLogBatch) {
        let events = Array(unsentUsage.prefix(Self.batchLimit))
        unsentUsage.removeFirst(events.count)
        struct Report: Encodable { let connectedSeconds: Int; let events: [UsageEvent] }
        let body = (try? JSONEncoder().encode(Report(connectedSeconds: connectedSeconds(at: date), events: events))) ?? Data()
        return (body, VoiceSessionLogBatch(usage: events, entries: []))
    }

    /// `POST /api/v1/voice/panels/{id}/transcript`.
    public mutating func takeTranscript(threadIDs: [String]) -> (body: Data, sent: VoiceSessionLogBatch) {
        let batch = Array(unsentEntries.prefix(Self.batchLimit))
        unsentEntries.removeFirst(batch.count)
        struct Report: Encodable { let entries: [Entry]; let threadIds: [String] }
        let body = (try? JSONEncoder().encode(Report(entries: batch, threadIds: Array(threadIDs.prefix(50))))) ?? Data()
        return (body, VoiceSessionLogBatch(usage: [], entries: batch))
    }

    public mutating func restore(_ batch: VoiceSessionLogBatch) {
        unsentUsage = batch.usage + unsentUsage
        unsentEntries = batch.entries + unsentEntries
    }
}

/// What one post carried, to put back if it failed.
public struct VoiceSessionLogBatch: Sendable {
    let usage: [VoiceSessionLog.UsageEvent]
    let entries: [VoiceSessionLog.Entry]
}

/// OpenAI's usage object, passed to the Mac unchanged.
enum VoiceJSON: Codable, Sendable, Equatable {
    case null, bool(Bool), number(Double), string(String), array([VoiceJSON]), object([String: VoiceJSON])

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let bool = try? value.decode(Bool.self) { self = .bool(bool) }
        else if let number = try? value.decode(Double.self) { self = .number(number) }
        else if let string = try? value.decode(String.self) { self = .string(string) }
        else if let array = try? value.decode([VoiceJSON].self) { self = .array(array) }
        else { self = .object(try value.decode([String: VoiceJSON].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        switch self {
        case .null: try value.encodeNil()
        case .bool(let bool): try value.encode(bool)
        case .number(let number):
            if number.rounded() == number, abs(number) < 1e15 { try value.encode(Int64(number)) } else { try value.encode(number) }
        case .string(let string): try value.encode(string)
        case .array(let array): try value.encode(array)
        case .object(let object): try value.encode(object)
        }
    }
}

/// Where a connected voice conversation is, kept apart from whether the
/// microphone may run. A call or another app (`interrupted`), or a stopped
/// microphone the owner resumes from the orb (`paused`), holds capture and
/// playback; speech, tool and reply events still move the turn along under
/// the hold, but the screen keeps saying Paused until `resumed` once the
/// microphone runs again.
public struct VoiceTurn: Equatable, Sendable {
    public enum Stage: Equatable, Sendable { case listening, hearing, working, speaking }
    public enum Hold: Equatable, Sendable {
        /// A call, Siri or another app has the audio.
        case interrupted
        /// The microphone stopped and voice waits for a tap on the orb.
        case paused

        public var message: String {
            switch self {
            case .interrupted: "Paused for a call or another app"
            case .paused: "Paused. Tap the orb to resume."
            }
        }
    }
    /// What the panel shows.
    public enum Display: Equatable, Sendable {
        case stage(Stage)
        case held(Hold)
    }

    public private(set) var stage: Stage = .listening
    public private(set) var hold: Hold?
    private var playing = false
    private var activeTools = 0
    private var awaitingToolReply = false
    private var completedResponses = 0
    private var lastToolResponse = 0

    public init() {}

    /// The microphone may capture, route changes may restart it, and the
    /// voice's audio may play.
    public var live: Bool { hold == nil }

    public var display: Display { hold.map(Display.held) ?? .stage(stage) }

    public var toolsRunning: Bool { activeTools > 0 }

    public mutating func apply(_ event: VoiceRealtimeEvent) {
        switch event {
        case .userSpeechStarted: stage = .hearing
        case .userSpeechStopped: stage = .working
        case .userSpeechDiscarded: if stage == .hearing { stage = settled }
        case .assistantTranscript(_, true):
            if completedResponses + 1 > lastToolResponse { awaitingToolReply = false }
        case .toolStarted:
            activeTools += 1; awaitingToolReply = true
            lastToolResponse = completedResponses + 1
            stage = .working
        case .toolFinished:
            activeTools = max(0, activeTools - 1)
        case .responseDone:
            completedResponses += 1
            if !playing, !awaitingToolReply, activeTools == 0, stage != .hearing { stage = .listening }
        default: break
        }
    }

    /// The voice's audio started or finished playing. Nothing plays while
    /// held, so a late start is ignored.
    public mutating func playbackChanged(_ playing: Bool) {
        if playing {
            guard live else { return }
            self.playing = true; stage = .speaking
        } else {
            self.playing = false
            if stage == .speaking { stage = settled }
        }
    }

    /// Capture and playback stopped; queued audio was dropped.
    public mutating func hold(_ reason: Hold) {
        hold = reason; playing = false
    }

    /// The microphone runs again. The owner wasn't heard while it was off,
    /// and what the voice was saying was dropped.
    public mutating func resumed() {
        guard hold != nil else { return }
        hold = nil; playing = false; stage = settled
    }

    private var settled: Stage { awaitingToolReply || activeTools > 0 ? .working : .listening }
}
