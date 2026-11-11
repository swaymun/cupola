import SwiftUI
import WonderPairing

struct ActivityGroupView: View {
    let rows: [ReadRow]
    let turn: ReadTurn?
    let isLatestSegmentForTurn: Bool
    let isLatestActiveSegment: Bool
    let expanded: Bool
    let toggle: () -> Void
    private var lifecycleLabel: String {
        if turn?.isInProgress == true, isLatestActiveSegment,
           let running = rows.last(where: { $0.activitySummary?.isRunning == true }),
           running.item?.type == "reasoning" {
            return "Thinking"
        }
        return ChatFeedEntry.lifecycleLabel(rows: rows, turn: turn, isLatestSegmentForTurn: isLatestSegmentForTurn,
                                     isLatestActiveSegment: isLatestActiveSegment)
    }
    private var showsSpinner: Bool {
        turn?.isInProgress == true && isLatestActiveSegment
    }
    /// The actions a finished turn took, after its "Worked for" time.
    private var detail: String? {
        ChatFeedEntry.lifecycleDetail(rows: rows, turn: turn, isLatestSegmentForTurn: isLatestSegmentForTurn)
    }
    /// Start of the current step, shown as a ticking time on this row only.
    private var stepStart: Date? {
        guard showsSpinner else { return nil }
        return ChatFeedEntry.currentStepStart(rows: rows, turn: turn)
    }
    @Environment(\.scenePhase) private var scenePhase
    @State private var visible = false
    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 8) {
                if showsSpinner {
                    ProgressView().controlSize(.small)
                        .accessibilityIdentifier("activity-progress:" + (rows.first?.id ?? "empty"))
                }
                title
                Spacer()
                Image(systemName: expanded ? "chevron.down" : "chevron.right").font(.caption.weight(.semibold))
            }.font(.subheadline).foregroundStyle(.primary).frame(minHeight: 44)
                .contentShape(Rectangle())
        }.buttonStyle(.plain)
            .onAppear { visible = true }
            .onDisappear { visible = false }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(accessibilityText(at: .now))
            .accessibilityValue(expanded ? "Expanded" : "Collapsed")
            .accessibilityIdentifier("activity-group:" + (rows.first?.id ?? "empty"))
    }
    @ViewBuilder private var title: some View {
        if let start = stepStart, visible, scenePhase == .active {
            TimelineView(.periodic(from: .now, by: 1)) { context in
                Text(timed(at: context.date, since: start)).monospacedDigit().lineLimit(1)
            }
        } else if let start = stepStart {
            Text(timed(at: .now, since: start)).monospacedDigit().lineLimit(1)
        } else {
            Text(lifecycleLabel).lineLimit(1)
            if let detail { Text(detail).foregroundStyle(.secondary).lineLimit(1) }
        }
    }
    private func timed(at date: Date, since start: Date) -> String {
        let seconds = Int(date.timeIntervalSince(start))
        guard seconds >= 1, seconds < 86_400 else { return lifecycleLabel }
        return lifecycleLabel + " · " + (seconds >= 60 ? "\(seconds / 60)m \(seconds % 60)s" : "\(seconds)s")
    }
    private func accessibilityText(at date: Date) -> String {
        if let start = stepStart { return timed(at: date, since: start) }
        return [lifecycleLabel, detail].compactMap { $0 }.joined(separator: ", ")
    }
}

/// Finished commands one after another inside an open Working group, as one
/// "Ran 6 commands" row. Opening it lists each command under it, as nodes of
/// the conversation's one lazy stack.
struct CommandRunView: View {
    let run: CommandRun
    let expanded: Bool
    let toggle: () -> Void
    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 8) {
                Label(run.label, systemImage: "terminal").font(.subheadline.weight(.medium))
                Spacer()
                Image(systemName: expanded ? "chevron.down" : "chevron.right").font(.caption.weight(.semibold))
            }
            .foregroundStyle(.primary).frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(run.label)
        .accessibilityValue(expanded ? "Expanded" : "Collapsed")
        .accessibilityIdentifier("command-run:" + (run.rows.first?.id ?? "empty"))
    }
}

/// A thought inside an expanded Working group: "Thought for 12s", or the
/// elapsed time while it runs. Only this visible label ticks; history and the
/// rest of the conversation stay still. At the Thinking level the summary the
/// agent wrote follows the label.
struct ThinkingActivityView: View {
    let row: ReadRow
    let isRunning: Bool
    var summary: String? = nil
    @Environment(\.scenePhase) private var scenePhase
    @State private var visible = false
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Group {
                if isRunning, let timing = row.thinkingTiming, visible, scenePhase == .active {
                    TimelineView(.periodic(from: .now, by: 1)) { context in
                        label(timing.activeLabel(at: context.date))
                    }
                } else {
                    label(row.thinkingTiming?.completedLabel ?? (isRunning ? "Thinking" : "Thought"))
                }
            }
            .accessibilityIdentifier("thinking-status:" + row.id)
            if let summary {
                AgentMessageText(text: summary)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: 640, alignment: .leading)
                    .accessibilityIdentifier("thinking-summary:" + row.id)
            }
        }
        .onAppear { visible = true }
        .onDisappear { visible = false }
    }
    private func label(_ text: String) -> some View {
        HStack(spacing: 8) {
            if isRunning { ProgressView().controlSize(.small).accessibilityHidden(true) }
            Text(text).monospacedDigit().fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .font(.subheadline)
        .foregroundStyle(.secondary)
        .frame(minHeight: 44)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(text)
    }
}
