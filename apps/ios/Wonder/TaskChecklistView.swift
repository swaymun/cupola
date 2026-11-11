import SwiftUI
import WonderPairing

/// Native checklists stay separate from proposed plans and their Implement action.
struct TaskChecklistView: View {
    let checklist: TaskChecklist
    let rowID: String
    @State private var expanded = true
    @State private var historyExpanded = false
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button { expanded.toggle() } label: {
                HStack(spacing: 8) {
                    Image(systemName: "checklist")
                    Text("Tasks").fontWeight(.medium)
                    count(checklist.current)
                    Spacer(minLength: 0)
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .font(.caption.weight(.semibold))
                }
                .frame(minHeight: 44)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Tasks, " + checklist.current.accessibilityLabel)
            .accessibilityValue(expanded ? "Expanded" : "Collapsed")
            .accessibilityIdentifier("tasks-" + rowID)
            if expanded {
                entries(checklist.current)
                if !checklist.history.isEmpty {
                    Button { historyExpanded.toggle() } label: {
                        Label("Updates", systemImage: historyExpanded ? "chevron.down" : "chevron.right")
                            .font(.subheadline)
                            .frame(minHeight: 44)
                    }
                    .buttonStyle(.plain)
                    .accessibilityValue(historyExpanded ? "Expanded" : "Collapsed")
                    .accessibilityIdentifier("task-updates-" + rowID)
                    if historyExpanded {
                        ForEach(checklist.history.indices, id: \.self) { index in
                            VStack(alignment: .leading, spacing: 8) {
                                HStack {
                                    Text("Update \(index + 1)").font(.caption.weight(.medium))
                                    count(checklist.history[index])
                                }
                                entries(checklist.history[index])
                            }
                            .padding(.bottom, 8)
                        }
                    }
                }
            }
        }
        .foregroundStyle(theme.text)
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
        // Keep disclosure labels beside their content while the row changes height.
        .transaction { transaction in
            transaction.animation = nil
            transaction.disablesAnimations = true
        }
    }

    private func count(_ snapshot: TaskChecklist.Snapshot) -> some View {
        Text(snapshot.countLabel)
            .font(.caption.monospacedDigit().weight(.medium))
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(theme.text.opacity(0.08), in: Capsule())
            .accessibilityLabel(snapshot.accessibilityLabel)
    }

    private func entries(_ snapshot: TaskChecklist.Snapshot) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            if snapshot.total == 0 {
                Text("No tasks").font(.subheadline)
            }
            ForEach(snapshot.entries.indices, id: \.self) { index in
                let entry = snapshot.entries[index]
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Image(systemName: entry.status.symbol)
                        .accessibilityHidden(true)
                    Text(entry.text)
                        .fontWeight(entry.status == .inProgress ? .medium : .regular)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .font(.subheadline)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(entry.status.label + ": " + entry.text)
            }
            if snapshot.truncated {
                Text("Some task details omitted").font(.caption)
            }
        }
    }
}
