import Foundation

/// Filters only the state the host actually reported. Idle and unread alone do
/// not prove that the latest turn completed successfully.
public enum SidebarSessionFilter: String, CaseIterable, Identifiable, Sendable {
    case all, working, needsInput
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .all: "All"
        case .working: "Working"
        case .needsInput: "Needs input"
        }
    }

    public func includes(_ thread: ProjectThreadSummary, needsInput: Bool) -> Bool {
        switch self {
        case .all: true
        case .working: thread.isRunning && !needsInput
        case .needsInput: needsInput
        }
    }

    /// The verified conversation mapping is shared by opened and unopened rows.
    /// An expired question cannot keep a session in Needs input.
    public static func awaitsInput(conversation: String?, requests: [AttentionRequest], questions: [AsyncQuestion], now: UInt64) -> Bool {
        guard let conversation, !conversation.isEmpty else { return false }
        return requests.contains { $0.belongs(to: conversation) && ($0.params.expiresAtMs.map { now < $0 } ?? true) }
            || questions.contains { $0.conversationId == conversation && $0.canAnswer(now: now) }
    }

    public static func attentionKey(host: String, thread: String) -> String { host + "\n" + thread }
}

/// One actionable address; identities include the computer, never just a request ID.
public struct AttentionInboxItem: Identifiable, Equatable, Sendable {
    public enum Kind: String, Sendable { case approval, question, failedReply, finished, failed }
    public let host: String
    public let conversation: String
    public let request: String
    public let kind: Kind
    public let reason: String
    public var id: String { host + "\n" + conversation + "\n" + request }
}

public enum AttentionInboxProjection {
    /// Hook for U-FINISHED-FILTER-H. Only populate from an authoritative host
    /// outcome awaiting review, never from idle/unread or a cached history guess.
    public struct ReviewOutcome: Sendable {
        public let conversation: String
        public let item: String
        public let failed: Bool
        public init(conversation: String, item: String, failed: Bool) {
            self.conversation = conversation; self.item = item; self.failed = failed
        }
    }
    public struct Computer: Sendable {
        public let host: String
        public let requests: [AttentionRequest]
        public let questions: [AsyncQuestion]
        public let failedReplies: Set<String>
        public let outcomes: [ReviewOutcome]
        public init(host: String, requests: [AttentionRequest], questions: [AsyncQuestion],
                    failedReplies: Set<String> = [], outcomes: [ReviewOutcome] = []) {
            self.host = host; self.requests = requests; self.questions = questions
            self.failedReplies = failedReplies; self.outcomes = outcomes
        }
    }
    public static func items(_ computers: [Computer], now: UInt64) -> [AttentionInboxItem] {
        var seen = Set<String>()
        return computers.flatMap { computer -> [AttentionInboxItem] in
            var result = computer.requests.compactMap { request -> AttentionInboxItem? in
                guard let chat = request.conversationId, !chat.isEmpty,
                      request.params.expiresAtMs.map({ now < $0 }) ?? true else { return nil }
                let failed = computer.failedReplies.contains(request.id)
                return AttentionInboxItem(host: computer.host, conversation: chat, request: request.id,
                    kind: failed ? .failedReply : request.isQuestion ? .question : .approval,
                    reason: failed ? "Your reply wasn’t confirmed" : request.isQuestion ? "Answer a question" : request.isSecretRequest ? "Enter a secret" : "Permission requested")
            }
            result += computer.questions.compactMap { question in
                let failed = computer.failedReplies.contains(question.id)
                guard !question.conversationId.isEmpty, question.canAnswer(now: now) || failed else { return nil }
                return AttentionInboxItem(host: computer.host, conversation: question.conversationId, request: question.id,
                    kind: failed ? .failedReply : .question, reason: failed ? "Your reply wasn’t confirmed" : "Answer a question")
            }
            result += computer.outcomes.map {
                AttentionInboxItem(host: computer.host, conversation: $0.conversation, request: $0.item,
                    kind: $0.failed ? .failed : .finished, reason: $0.failed ? "The reply failed" : "Finished work to review")
            }
            return result
        }.filter { seen.insert($0.id).inserted }
        .sorted { $0.id < $1.id }
    }
}
