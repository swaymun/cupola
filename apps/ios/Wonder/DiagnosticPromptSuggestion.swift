#if WONDER_DIAGNOSTICS
import Foundation

extension DiagnosticSubagentFixture {
    static var promptSuggestionFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-prompt-suggestion") }
    static func promptSuggestionSnapshot() -> [String: Any] {
        ["conversationId": parentID, "hostEpoch": "fixture", "lastSequence": 2,
         "messages": [], "assistantMessages": [], "thread": ["threadId": parentThreadID, "hydrated": true, "turns": [[
            "id": "suggestion-turn", "status": "completed", "items": [
                ["id": "suggestion-user", "type": "userMessage", "state": "completed", "text": "Review the layout.", "createdAt": "1000"],
                ["id": "suggestion-answer", "type": "agentMessage", "state": "completed", "text": "The layout keeps the composer available.", "createdAt": "2000"],
                ["id": "suggestion-hint", "type": "promptSuggestion", "state": "completed", "text": "Check the keyboard layout", "createdAt": "2001",
                 "payload": ["agentFamily": "claude"]]
            ]]]]]
    }
}
#endif
