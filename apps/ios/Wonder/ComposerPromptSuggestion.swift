import WonderPairing

extension ConnectionModel {
    func eligiblePromptSuggestion(_ suggestion: PromptSuggestion?, chat: ChatSummary) -> PromptSuggestion? {
        guard let suggestion, let draft = composers[chat.id], isProject(chat), !accessEnded, !chat.isArchived,
              visibleChat?.id == chat.id, snapshots[chat.id]?.thread.turns?.last?.id == suggestion.turnID,
              snapshots[chat.id]?.messages.last(where: { !$0.wasCancelledBeforeDispatch }).map({ $0.codexTurnId == suggestion.turnID }) ?? true,
              suggestion.isEligible(family: agentFamily(chat), capabilities: capabilities(chat), draft: draft,
                busy: conversationWorking(chat.id) || sending.contains(chat.id) || preparingSends.contains(chat.id)
                    || uploading.contains(chat.id) || loadingPhotos.contains(chat.id) || composerErrors[chat.id] != nil
                    || savingComposerSettings.contains(chat.id)
                    || nativeOpenElsewhere.contains(chat.id) || !(queues[chat.id] ?? []).isEmpty) else { return nil }
        return suggestion
    }

    /// Revalidate the captured presentation and pairing at the moment of the
    /// tap. This only saves text; Send remains the owner's separate action.
    func acceptPromptSuggestion(_ suggestion: PromptSuggestion, chat: ChatSummary, scope: String) -> Bool {
        guard scope == assignmentScope,
              timeline(for: chat, focusedRowID: nil).promptSuggestion == suggestion,
              eligiblePromptSuggestion(suggestion, chat: chat) != nil else { return false }
        return fillEmptyDraft(suggestion.text, chat: chat.id) == .saved
    }
}
