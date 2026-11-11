import SwiftUI
import WonderPairing

/// Skills already stored on a draft, queued message or retry stay visible
/// and removable; nothing in the app adds new ones.
struct SelectedCodexSkills: View {
    let skills: [CodexSkillSelection]
    /// The next reply's provider takes skills; otherwise they block sending.
    let takesSkills: Bool
    let disabled: Bool
    let remove: (String) -> Void
    @Environment(\.wonderTheme) private var theme
    var body: some View {
        if !skills.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                ScrollView(.horizontal) {
                    HStack(spacing: 8) {
                        ForEach(skills) { skill in
                            Button { remove(skill.path) } label: {
                                Label("$" + skill.name, systemImage: "xmark.circle.fill").font(.caption)
                                    .padding(.horizontal, 8).padding(.vertical, 6)
                                    .background(theme.surface, in: Capsule())
                            }.disabled(disabled)
                            .accessibilityLabel("Remove selected skill \(skill.name)")
                            .accessibilityHint(skill.path)
                            .accessibilityIdentifier("selected-skill-" + ConversationFile.digest(Data(skill.path.utf8)))
                        }
                    }
                }.scrollIndicators(.hidden)
                if !takesSkills { Text("Use Codex or remove the selected skills before sending.").font(.caption).foregroundStyle(.secondary) }
            }.padding(.horizontal, 8).padding(.top, 4)
        }
    }
}
