import Foundation

/// How full a Project thread's context window is, as its provider last
/// reported it (Codex token usage, or Claude's last request through the Agent
/// SDK). The Mac sends it only when known, so there is no estimate here.
public struct ContextUsage: Codable, Equatable, Sendable {
    public let usedTokens: Int
    /// 0 when the provider gave the request's tokens but not its window
    /// (the host sends 0; a null from another host also reads as 0): the
    /// tokens show without a fill.
    public let windowTokens: Int
    /// True when the Mac filled `windowTokens` in from what the model
    /// typically has because the provider gave none; false when reported.
    public let windowIsTypical: Bool
    /// When the provider reported it (ms since 1970); absent from an older Mac.
    public let atMs: Int64?

    public init(usedTokens: Int, windowTokens: Int, windowIsTypical: Bool = false, atMs: Int64? = nil) {
        self.usedTokens = usedTokens
        self.windowTokens = windowTokens
        self.windowIsTypical = windowIsTypical
        self.atMs = atMs
    }

    private enum CodingKeys: String, CodingKey { case usedTokens, windowTokens, windowIsTypical, atMs }
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        usedTokens = try values.decode(Int.self, forKey: .usedTokens)
        windowTokens = try values.decodeIfPresent(Int.self, forKey: .windowTokens) ?? 0
        windowIsTypical = try values.decodeIfPresent(Bool.self, forKey: .windowIsTypical) ?? false
        atMs = try values.decodeIfPresent(Int64.self, forKey: .atMs)
    }

    /// A reading older than two minutes reads dimmed with its age; views re-check
    /// it each minute, so a reading dims as it ages and a new one shows undimmed.
    public func isStale(now: Date = Date()) -> Bool {
        guard let atMs else { return false }
        return now.timeIntervalSince1970 * 1000 - Double(atMs) > 120_000
    }
    /// "3 h ago" for a stale reading.
    public func age(now: Date = Date()) -> String? {
        guard isStale(now: now), let atMs else { return nil }
        return RelativeDateTimeFormatter().localizedString(for: Date(timeIntervalSince1970: Double(atMs) / 1000), relativeTo: now)
    }

    /// Nil for a report that cannot be shown honestly.
    public var known: ContextUsage? { usedTokens > 0 ? self : nil }
    /// Whether the provider reported how large the window is.
    public var hasWindow: Bool { windowTokens > 0 }
    public var fraction: Double { hasWindow ? min(1, Double(usedTokens) / Double(windowTokens)) : 0 }
    /// Whole percent, never 0 for a window that holds something.
    public var percent: Int { max(1, Int((fraction * 100).rounded())) }
    public var summary: String {
        hasWindow ? "\(Self.compact(usedTokens)) of \(Self.compact(windowTokens))" : "\(Self.compact(usedTokens)) tokens"
    }
    public var accessibilityValue: String {
        hasWindow ? "\(percent) percent used, \(usedTokens.formatted()) of \(windowTokens.formatted()) tokens\(windowIsTypical ? ", typical window for this model" : "")"
            : "\(usedTokens.formatted()) tokens used, window size unknown"
    }

    /// 950, 84K, 1.2M (one decimal only below 10M).
    static func compact(_ tokens: Int) -> String {
        if tokens >= 1_000_000 {
            let millions = Double(tokens) / 1_000_000
            return millions < 10 && millions.rounded() != millions
                ? String(format: "%.1fM", millions) : "\(Int(millions.rounded()))M"
        }
        if tokens >= 1_000 { return "\(Int((Double(tokens) / 1_000).rounded()))K" }
        return "\(tokens)"
    }
}

/// The last reading per chat, kept on the phone so an opening chat shows it at
/// once, before (or without) the computer answering. One small atomic file per
/// pairing; the host's newer reading replaces it.
public extension ReadStore {
    func loadContextUsage() throws -> [String: ContextUsage] {
        guard let data = try loadIntent(conversation: contextUsageKey) else { return [:] }
        return try JSONDecoder().decode([String: ContextUsage].self, from: data)
    }
    func saveContextUsage(_ readings: [String: ContextUsage]) throws {
        if readings.isEmpty { try removeIntent(conversation: contextUsageKey) }
        else { try saveIntent(JSONEncoder().encode(readings), conversation: contextUsageKey) }
    }
}

public extension IntentWriter {
    /// Saved with the other intents: ordered, newest per key, held while the
    /// phone is locked, and dropped by `discard()` so a Forget in flight never
    /// has its removed partition recreated by a late reading.
    func saveContextUsage(_ readings: [String: ContextUsage]) {
        guard let data = try? JSONEncoder().encode(readings) else { return }
        save(data, key: contextUsageKey)
    }
}

private let contextUsageKey = "context-usage-v1"

public extension Dictionary where Key == String, Value == ContextUsage {
    /// Keeps the newest 128 readings.
    func trimmedToNewest(_ limit: Int = 128) -> [String: ContextUsage] {
        guard count > limit else { return self }
        let newest = sorted { ($0.value.atMs ?? 0) > ($1.value.atMs ?? 0) }.prefix(limit)
        return Dictionary(uniqueKeysWithValues: newest.map { ($0.key, $0.value) })
    }
}
