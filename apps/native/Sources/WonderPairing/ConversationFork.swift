import Foundation

/// What the phone asks the Mac to do when the owner forks a Project thread.
public struct ForkConversationRequest: Encodable, Equatable, Sendable {
    /// The reply to keep through. Nil forks the whole conversation.
    public let lastTurnId: String?
    public init(lastTurnId: String?) { self.lastTurnId = lastTurnId }

    /// The body the host accepts: the turn is named only when there is one.
    public func body() throws -> Data { try JSONEncoder().encode(self) }
}

public enum ConversationFork {
    /// The turn an agent reply can be forked at: a finished reply, never the
    /// owner's own message, commentary, or anything still being written.
    public static func turnID(of row: ReadRow, turnFinished: Bool) -> String? {
        guard turnFinished, !row.isUser, !row.isCommentary, !row.isPlan,
              row.item?.type == "agentMessage", row.item?.state == "completed" else { return nil }
        return row.turnId
    }

    /// Whether the header can offer Fork: the provider forks natively
    /// (`ProviderCapabilities.nativeFork`), the owner's thread has started and
    /// no turn is running.
    public static func canForkLatest(nativeFork: Bool, hasNativeSession: Bool, hasActiveTurn: Bool, isArchived: Bool) -> Bool {
        nativeFork && hasNativeSession && !hasActiveTurn && !isArchived
    }

    /// What the owner reads when the Mac cannot fork, by the host's status.
    public static func failureMessage(status: Int?, computer: String) -> String {
        switch status {
        case 409: "This chat is busy or hasn’t started. Fork it when the agent has finished replying."
        case 422: "This kind of chat can’t be forked yet."
        case 404: "This chat is no longer on \(computer)."
        case 401, 403: "Access has ended. Reconnect to your computer in Settings."
        default: "\(computer) couldn’t fork this chat. Check it and try again."
        }
    }
}
