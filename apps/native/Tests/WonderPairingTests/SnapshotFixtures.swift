@testable import WonderPairing

// Fixtures without T3 Code message records. Production code has no such
// initializer: every copy there must say which records it carries.
extension ConversationSnapshot {
    init(conversationId: String, hostEpoch: String, lastSequence: UInt64, historyRevision: UInt64? = nil,
         messages: [ConversationMessage], assistantMessages: [AssistantMessage], thread: ThreadProjection) {
        self.init(conversationId: conversationId, hostEpoch: hostEpoch, lastSequence: lastSequence, historyRevision: historyRevision,
                  messages: messages, assistantMessages: assistantMessages, thread: thread, messageRecords: nil)
    }
}
