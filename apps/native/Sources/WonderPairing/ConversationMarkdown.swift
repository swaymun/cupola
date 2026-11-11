import Foundation

/// The loaded conversation as Markdown: who spoke, what they said, and one line
/// for each thing the agent did. It is built from projected rows, not from a
/// view, so the same text comes out whatever is on screen.
public enum ConversationMarkdown {
    public struct Output: Equatable, Sendable {
        public let markdown: String
        /// Messages written: the owner's and the agent's words, and plans.
        public let messageCount: Int
        /// Earlier history exists on the Mac that is not loaded on this device.
        public let isPartial: Bool

        /// What the reader is told after copying.
        public var confirmation: String {
            let noun = messageCount == 1 ? "message" : "messages"
            return isPartial ? "Copied \(messageCount) \(noun). Earlier messages are not loaded yet."
                : "Copied \(messageCount) \(noun)"
        }
    }

    public static func render(title: String, agentName: String, rows: [ReadRow], isPartial: Bool) -> Output {
        var lines = ["# " + singleLine(title, fallback: "Conversation")]
        if isPartial { lines.append("_Earlier messages are not included._") }
        var speaker: String?
        // Actions are headed by the provider in effect at that point, which a
        // switch changes; their projected author is only "Activity".
        var provider = rows.first(where: \.isProviderSwitch).flatMap { family($0, "fromFamily") } ?? agentName
        var activity: [String] = []
        var count = 0
        func flushActivity() {
            guard !activity.isEmpty else { return }
            lines.append(activity.map { "- " + $0 }.joined(separator: "\n"))
            activity = []
        }
        func heading(_ name: String) {
            guard speaker != name else { return }
            lines.append("## " + singleLine(name, fallback: "Agent"))
            speaker = name
        }
        func speak(_ name: String, _ text: String) {
            flushActivity()
            heading(name)
            lines.append(text)
            count += 1
        }
        func name(_ row: ReadRow) -> String {
            let author = row.author.trimmingCharacters(in: .whitespaces)
            let isMessage = row.item == nil || row.item?.type == "agentMessage"
            return isMessage && !author.isEmpty ? author : provider
        }
        for row in rows {
            if row.isProviderSwitch, let next = family(row, "toFamily") { provider = next }
            if row.item?.type == "reasoning" || row.isContextCompaction || row.isProviderSwitch || row.isTaskNotification { continue }
            let text = (row.isPlan ? row.planText : row.text)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            if row.isUser {
                if !text.isEmpty { speak("You", text) }
            } else if let tasks = row.taskChecklist {
                flushActivity()
                heading(name(row))
                lines.append("Tasks · " + tasks.current.countLabel + "\n" + tasks.current.entries.map {
                    "- " + $0.status.label + ": " + $0.text
                }.joined(separator: "\n"))
            } else if row.isPlan {
                if !text.isEmpty { speak(name(row), text) }
            } else if let summary = summary(row) {
                heading(name(row))
                activity.append(summary)
            } else if !text.isEmpty {
                speak(name(row), text)
            }
        }
        flushActivity()
        return Output(markdown: lines.joined(separator: "\n\n") + "\n", messageCount: count, isPartial: isPartial)
    }

    private static func family(_ row: ReadRow, _ key: String) -> String? {
        row.item?.payload?[key]?.string.flatMap(AgentFamily.init(rawValue:))?.title
    }

    /// One line per action. Never tool output and never identifiers.
    private static func summary(_ row: ReadRow) -> String? {
        if let command = row.commandSummary { return command.label(includeDuration: true) }
        if let change = row.fileChangeSummary { return change.title }
        guard let activity = row.activitySummary else { return nil }
        return activity.state == .completed ? activity.title : "\(activity.title) (\(activity.status.lowercased()))"
    }

    private static func singleLine(_ text: String, fallback: String) -> String {
        let value = text.split(whereSeparator: \.isNewline).joined(separator: " ").trimmingCharacters(in: .whitespaces)
        return value.isEmpty ? fallback : value
    }
}
