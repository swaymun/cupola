import Foundation

/// An optional native SDK hint owned by the latest completed turn of a
/// provider that offers prompt suggestions. Prepared at the timeline boundary,
/// never parsed by the editor or its view.
public struct PromptSuggestion: Equatable, Sendable {
    public let id: String
    public let turnID: String
    public let text: String
    /// The provider that wrote the hint, as the host stamped it.
    public let family: AgentFamily

    public static func latest(in turns: [ReadTurn]) -> Self? {
        guard let turn = turns.last, turn.status == "completed", !turn.runningElsewhere,
              !turn.items.contains(where: { $0.type == "providerSwitch" }),
              let item = turn.items.last, item.type == "promptSuggestion", item.state == "completed",
              let family = item.payload?["agentFamily"]?.string.flatMap(AgentFamily.init(rawValue:)),
              let text = item.text ?? item.payload?["text"]?.string,
              !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, text.utf16.count <= 2000 else { return nil }
        return Self(id: item.id, turnID: turn.id, text: text, family: family)
    }

    /// Shown only in a thread whose provider wrote it and offers suggestions
    /// (`ProviderCapabilities.promptSuggestions`).
    public func isEligible(family: AgentFamily?, capabilities: ProviderCapabilities?, draft: ComposerIntent, busy: Bool) -> Bool {
        family == self.family && capabilities?.promptSuggestions == true && !busy && draft.draft.isEmpty && draft.pending == nil
            && (draft.stagedFiles ?? []).isEmpty && (draft.draftAttachmentIds ?? []).isEmpty
    }
}
