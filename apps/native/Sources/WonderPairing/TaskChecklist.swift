import Foundation

/// Provider-authored task snapshots, prepared with the row rather than in a view.
public struct TaskChecklist: Equatable, Sendable {
    public struct Entry: Equatable, Sendable {
        public enum Status: String, Sendable {
            case pending, inProgress, completed, unknown
            public var label: String {
                switch self {
                case .pending: "Pending"
                case .inProgress: "In progress"
                case .completed: "Completed"
                case .unknown: "Status unavailable"
                }
            }
            public var symbol: String {
                switch self {
                case .pending: "circle"
                case .inProgress: "circle.dotted.circle"
                case .completed: "checkmark.circle.fill"
                case .unknown: "questionmark.circle"
                }
            }
        }
        public let text: String
        public let status: Status
    }
    public struct Snapshot: Equatable, Sendable {
        public let entries: [Entry]
        public let total: Int
        public let completed: Int
        public let truncated: Bool
        public let countLabel: String
        public let accessibilityLabel: String
    }
    public let current: Snapshot
    /// Earlier distinct native updates, oldest first; current is shown separately.
    public let history: [Snapshot]

    public static func prepare(_ item: ReadItem) -> Self? {
        guard item.type == "taskChecklist" || (item.type == "mcpToolCall" && item.payload?["server"]?.string == "Claude" && item.payload?["tool"]?.string == "TodoWrite"),
              ActivityState.prepare(item) == .completed,
              case .object(let fields) = item.payload?["taskChecklist"],
              let current = snapshot(fields) else { return nil }
        var history: [Snapshot] = []
        for value in (fields["history"]?.array ?? []).suffix(8) {
            guard case .object(let fields) = value, let entry = snapshot(fields), history.last != entry else { continue }
            history.append(entry)
        }
        if history.last == current { history.removeLast() }
        return Self(current: current, history: history)
    }

    private static func snapshot(_ fields: [String: ThreadValue]) -> Snapshot? {
        guard let values = fields["entries"]?.array else { return nil }
        let entries = values.prefix(32).map { value -> Entry in
            guard case .object(let entry) = value else { return Entry(text: "Task unavailable", status: .unknown) }
            let text = String((entry["text"]?.string ?? "").prefix(256)).trimmingCharacters(in: .whitespacesAndNewlines)
            return Entry(text: text.isEmpty ? "Task unavailable" : text,
                         status: Entry.Status(rawValue: entry["status"]?.string ?? "") ?? .unknown)
        }
        let total = boundedCount(fields["total"]) ?? entries.count
        let completed = min(total, boundedCount(fields["completed"]) ?? entries.filter { $0.status == .completed }.count)
        guard total >= entries.count else { return nil }
        return Snapshot(entries: entries, total: total, completed: completed,
                        truncated: fields["truncated"]?.bool == true || total > entries.count || values.count > 32,
                        countLabel: "\(completed)/\(total)", accessibilityLabel: "\(completed) of \(total) tasks completed")
    }
    private static func boundedCount(_ value: ThreadValue?) -> Int? {
        guard let number = value?.number, number.isFinite, number >= 0, number <= 1_000_000,
              number.rounded() == number else { return nil }
        return Int(number)
    }
}
