import ActivityKit
import AppIntents
import SwiftUI
import WidgetKit

/// Lock Screen and Dynamic Island while Cupola Voice talks: the computer,
/// whether the microphone is muted, the time left, and Mute and End.
struct VoiceLiveActivityWidget: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: VoiceActivityAttributes.self) { context in
            VStack(alignment: .leading, spacing: 12) {
                VoiceActivitySummary(context: context, iconSize: 40)
                VoiceActivityButtons(muted: context.state.muted)
            }
            .padding(16)
            .activityBackgroundTint(nil)
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.center) {
                    VoiceActivitySummary(context: context, iconSize: 36)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    VoiceActivityButtons(muted: context.state.muted)
                }
            } compactLeading: {
                VoiceActivityIcon(muted: context.state.muted, size: 22)
            } compactTrailing: {
                VoiceTimeLeft(endsAt: context.state.endsAt)
                    .font(.caption.monospacedDigit().weight(.semibold))
                    .frame(maxWidth: 52, alignment: .trailing)
            } minimal: {
                VoiceActivityIcon(muted: context.state.muted, size: 22)
            }
            .keylineTint(.cupolaAccent)
        }
    }
}

private struct VoiceActivitySummary: View {
    let context: ActivityViewContext<VoiceActivityAttributes>
    let iconSize: CGFloat

    var body: some View {
        HStack(spacing: 12) {
            VoiceActivityIcon(muted: context.state.muted, size: iconSize)
            VStack(alignment: .leading, spacing: 2) {
                Text("Cupola Voice").font(.headline).lineLimit(1)
                Text(context.state.muted ? "Muted · \(context.attributes.computer)" : context.attributes.computer)
                    .font(.subheadline).foregroundStyle(.secondary).lineLimit(1).privacySensitive()
            }
            Spacer(minLength: 8)
            VoiceTimeLeft(endsAt: context.state.endsAt)
                .font(.subheadline.monospacedDigit())
                .foregroundStyle(.secondary)
        }
    }
}

/// Counts down by itself; nothing is pushed per second.
private struct VoiceTimeLeft: View {
    let endsAt: Date
    var body: some View {
        Text(timerInterval: Date.now...max(Date.now, endsAt), countsDown: true)
            .multilineTextAlignment(.trailing)
            .accessibilityLabel("Time left in this voice session")
    }
}

private struct VoiceActivityButtons: View {
    let muted: Bool
    var body: some View {
        HStack(spacing: 12) {
            Button(intent: VoiceMuteIntent()) {
                Label(muted ? "Unmute" : "Mute", systemImage: muted ? "mic.slash.fill" : "mic.fill")
                    .frame(maxWidth: .infinity)
            }
            .tint(muted ? .orange : .cupolaAccent)
            Button(intent: VoiceEndIntent()) {
                Label("End", systemImage: "xmark").frame(maxWidth: .infinity)
            }
            .tint(.red)
        }
        .buttonStyle(.borderedProminent)
        .font(.subheadline.weight(.semibold))
    }
}

private struct VoiceActivityIcon: View {
    let muted: Bool
    let size: CGFloat
    var body: some View {
        Image(systemName: muted ? "mic.slash.fill" : "waveform")
            .font(.system(size: size * 0.5, weight: .semibold))
            .foregroundStyle(muted ? Color.orange : Color.cupolaAccent)
            .frame(width: size, height: size)
            .background((muted ? Color.orange : Color.cupolaAccent).opacity(0.18), in: Circle())
            .accessibilityLabel(muted ? "Cupola Voice, muted" : "Cupola Voice, listening")
    }
}
