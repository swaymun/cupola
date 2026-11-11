import ActivityKit
import SwiftUI
import WidgetKit

/// Lock Screen and Dynamic Island for agent work on the phone's computers: a
/// status row per thread (title, Project, status) with the counts above. The
/// Mac pushes every change and the end; no message, command or file text
/// appears here.
struct AgentLiveActivityWidget: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: AgentActivityAttributes.self) { context in
            AgentActivityLockScreen(context: context)
                .activityBackgroundTint(nil)
                .widgetURL(AgentActivityLink.open(context))
        } dynamicIsland: { context in
            let shown = AgentActivityPresentation(attributes: context.attributes, state: context.state, isStale: context.isStale)
            return DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    AgentActivityPhaseIcon(phase: context.state.phase, symbol: shown.symbol, size: 36)
                        .padding(.leading, 4)
                }
                DynamicIslandExpandedRegion(.center) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(shown.headline).font(.headline).lineLimit(1).privacySensitive()
                        Text(shown.detail).font(.subheadline).lineLimit(2).foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    AgentActivityRows(context: context, shown: shown, limit: 3)
                }
            } compactLeading: {
                // The one VoiceOver element for compact, in every phase: the
                // icon itself is hidden, and the trailing count may be empty.
                AgentActivityPhaseIcon(phase: context.state.phase, symbol: shown.symbol, size: 22)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(shown.accessibilityLabel)
            } compactTrailing: {
                Text(shown.shortLabel)
                    .accessibilityHidden(true)
                    .font(.caption.monospacedDigit().weight(.semibold))
                    .lineLimit(1)
                    .minimumScaleFactor(0.7)
                    .frame(maxWidth: 64, alignment: .trailing)
                    .foregroundStyle(AgentActivityPhaseIcon.tint(context.state.phase))
            } minimal: {
                Group {
                    if shown.shortLabel.isEmpty {
                        // Finished: the icon alone, as in compact.
                        AgentActivityPhaseIcon(phase: context.state.phase, symbol: shown.symbol, size: 22)
                    } else {
                        Text(shown.shortLabel)
                            .font(.caption.monospacedDigit().weight(.semibold))
                            .lineLimit(1)
                            .minimumScaleFactor(0.7)
                            .frame(width: 22, height: 22)
                            .foregroundStyle(AgentActivityPhaseIcon.tint(context.state.phase))
                    }
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(shown.accessibilityLabel)
            }
            .widgetURL(AgentActivityLink.open(context))
            .keylineTint(AgentActivityPhaseIcon.tint(context.state.phase))
        }
    }
}

private struct AgentActivityLockScreen: View {
    let context: ActivityViewContext<AgentActivityAttributes>

    var body: some View {
        let shown = AgentActivityPresentation(attributes: context.attributes, state: context.state, isStale: context.isStale)
        let hidden = max(0, shown.rows.count - 4) + shown.more
        // The Lock Screen gives a Live Activity about 160 pt: a one-line
        // header and one-line rows keep four threads inside it.
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .center, spacing: 10) {
                AgentActivityPhaseIcon(phase: context.state.phase, symbol: shown.symbol, size: shown.rows.isEmpty ? 40 : 28)
                if shown.rows.isEmpty {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(shown.headline).font(.headline).lineLimit(1).privacySensitive()
                        Text(shown.detail).font(.subheadline).lineLimit(2).foregroundStyle(.secondary)
                    }
                } else {
                    Text(shown.detail).font(.subheadline.weight(.semibold)).lineLimit(1).minimumScaleFactor(0.8)
                }
                Spacer(minLength: 8)
                if hidden > 0 { Text("+\(hidden) more").font(.caption).foregroundStyle(.secondary) }
            }
            AgentActivityRows(context: context, shown: shown, limit: 4, showsMore: false)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }
}

/// The per-thread rows; a tap opens that thread. Elapsed time is a system
/// timer, so a quiet thread needs no push to keep counting.
private struct AgentActivityRows: View {
    let context: ActivityViewContext<AgentActivityAttributes>
    let shown: AgentActivityPresentation
    let limit: Int
    var showsMore = true

    var body: some View {
        let rows = Array(shown.rows.prefix(limit))
        let hidden = shown.rows.count - rows.count
        if !rows.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                    let content = AgentActivityRow(row: row)
                    if let url = AgentActivityLink.url(attributes: context.attributes, row: row, bundleIdentifier: Bundle.main.bundleIdentifier) {
                        Link(destination: url) { content }
                    } else {
                        content
                    }
                }
                let extra = max(0, hidden) + shown.more
                if showsMore, extra > 0 {
                    Text("+\(extra) more").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}

private struct AgentActivityRow: View {
    let row: AgentActivityAttributes.ThreadRow

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: row.status.symbol)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(AgentActivityPhaseIcon.tint(row.status))
                .frame(width: 18)
            Text(row.title.isEmpty ? "Chat" : row.title)
                .font(.subheadline).lineLimit(1).privacySensitive()
                .layoutPriority(1)
            if !row.project.isEmpty {
                Text(row.project).font(.caption).foregroundStyle(.secondary).lineLimit(1).privacySensitive()
            }
            Spacer(minLength: 6)
            status
                .font(.caption.monospacedDigit().weight(.medium))
                .foregroundStyle(AgentActivityPhaseIcon.tint(row.status))
                .lineLimit(1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(row.accessibilityLabel)
    }

    @ViewBuilder private var status: some View {
        switch row.status {
        case .running: Text(Date(timeIntervalSince1970: row.startedAt), style: .timer).multilineTextAlignment(.trailing).frame(maxWidth: 64, alignment: .trailing)
        case .needsApproval: Text("Approval")
        case .needsInput: Text("Input")
        case .done: Text("Done")
        case .failed: Text("Failed")
        case .stopped: Text("Stopped")
        }
    }
}

struct AgentActivityPhaseIcon: View {
    let phase: AgentActivityAttributes.Phase
    let symbol: String
    let size: CGFloat

    static func tint(_ phase: AgentActivityAttributes.Phase) -> Color {
        switch phase {
        case .needsApproval, .needsInput: .orange
        case .done: .green
        case .failed: .red
        case .stopped: .secondary
        case .running: .cupolaAccent
        }
    }

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: size * 0.5, weight: .semibold))
            .foregroundStyle(Self.tint(phase))
            .frame(width: size, height: size)
            .background(Self.tint(phase).opacity(0.18), in: Circle())
            .accessibilityHidden(true)
    }
}

extension AgentActivityLink {
    static func open(_ context: ActivityViewContext<AgentActivityAttributes>) -> URL? {
        url(attributes: context.attributes, state: context.state, bundleIdentifier: Bundle.main.bundleIdentifier)
    }
}
