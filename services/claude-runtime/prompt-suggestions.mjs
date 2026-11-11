// The SDK hint has no turn ID. One process owns this fence alongside its one
// retained iterator; nothing waits for a hint and no extra request is made.
export class PromptSuggestions {
  pending = null;
  ambiguous = false;

  begin() {
    // Native absence is normal. Once another prompt overlaps an unresolved
    // result we cannot distinguish either turn's hint, even after its result.
    if (this.pending) this.ambiguous = true;
    this.pending = null;
  }
  complete(turnId, sessionId, result) {
    if (!this.ambiguous && result?.status === "completed") this.pending = { turnId, sessionId };
  }
  invalidate() { this.ambiguous = true; this.pending = null; }
  take(message, latestTurnId) {
    const source = this.pending;
    if (!source || message.session_id !== source.sessionId || message.parent_tool_use_id) return null;
    this.pending = null;
    if (source.turnId !== latestTurnId || typeof message.uuid !== "string" || !message.uuid || message.uuid.length > 200
      || typeof message.suggestion !== "string" || !message.suggestion.trim() || message.suggestion.length > 2000) return null;
    return { turnId: source.turnId, item: { type: "promptSuggestion", id: `suggestion:${message.uuid}`,
      status: "completed", text: message.suggestion, agentFamily: "claude" } };
  }
}
