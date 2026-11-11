import Foundation

/// One conversation on one Mac: where an in-app alert leads.
public struct InAppAlertAddress: Hashable, Sendable {
    public let host: String
    public let conversation: String
    public init(host: String, conversation: String) { self.host = host; self.conversation = conversation }
}

/// What happened in another chat. Ordered by how much it needs the user.
public enum InAppAlertKind: Int, Comparable, Sendable {
    /// A notification for a chat the live stream does not cover (a thread run
    /// by Claude Code or the Codex app on the Mac).
    case update
    case finished
    case failed
    case question
    case approval

    /// A request the agent is waiting on: tapping opens the request, and the
    /// chat's later replies do not replace its alert.
    public var isRequest: Bool { self == .question || self == .approval }
    public static func < (lhs: Self, rhs: Self) -> Bool { lhs.rawValue < rhs.rawValue }
}

public struct InAppAlert: Identifiable, Equatable, Sendable {
    /// Stable through updates: one per pending request, otherwise one per chat.
    public let id: String
    public let address: InAppAlertAddress
    public var kind: InAppAlertKind
    /// A line from the event itself (the question, the command, the reply).
    /// Nil uses the chat's latest preview.
    public var summary: String?
    /// The pending request behind an approval or question.
    public var attentionID: String?
    public var arrivedAt: Date
}

/// Routes the same events that produce notifications (finished and failed
/// replies, pending permission requests and questions) into in-app alerts for
/// chats other than the ones on screen. Pure value logic: the app feeds it
/// live-stream frames, the pending-request list after each event-driven read,
/// and notifications that arrive while Wonder is open.
public struct InAppAlertQueue: Sendable {
    /// A replayed event older than this (catch-up after the app was away)
    /// already produced a notification; it does not alert again.
    public static let staleEventAge: TimeInterval = 120
    /// A notification for a chat that alerted from the live stream within
    /// this window is the same event arriving twice.
    public static let duplicateWindow: TimeInterval = 90
    static let memory = 256

    /// Newest first.
    public private(set) var alerts: [InAppAlert] = []
    private var seenEvents: [String] = []
    private var seenEventSet: Set<String> = []
    /// Pending request IDs per Mac, seen at least once. Requests already
    /// pending when Wonder first reads a Mac's list do not alert.
    private var knownAttention: [String: Set<String>] = [:]
    private var raisedAt: [InAppAlertAddress: Date] = [:]
    private var replies: [InAppAlertAddress: String] = [:]

    public init() {}

    // MARK: Inputs

    /// Live-stream events from one Mac. Returns whether alerts changed.
    @discardableResult
    public mutating func receive(_ events: [ReplayEvent], host: String, onScreen: Set<InAppAlertAddress>, now: Date) -> Bool {
        var changed = false
        for event in events {
            guard let conversation = event.conversationId else { continue }
            let address = InAppAlertAddress(host: host, conversation: conversation)
            if event.event.type == "assistant_completed", let text = event.event.text, !text.isEmpty {
                if replies.count >= Self.memory { replies.removeAll() }
                replies[address] = text
                continue
            }
            guard event.event.type == "message_state", let state = event.event.state,
                  state == "completed" || state == "failed" else { continue }
            let key = host + "\n" + conversation + "\n" + (event.messageId ?? event.turnId ?? event.eventId) + "\n" + state
            guard remember(key) else { continue }
            if let occurred = Self.date(event.occurredAt), now.timeIntervalSince(occurred) > Self.staleEventAge { continue }
            guard !onScreen.contains(address) else { replies[address] = nil; continue }
            // A chat still waiting on a request keeps that alert; a reply
            // finishing around it is not news.
            guard !alerts.contains(where: { $0.address == address && $0.kind.isRequest }) else { continue }
            let kind: InAppAlertKind = state == "failed" ? .failed : .finished
            let reply = kind == .finished ? replies[address] : nil
            replies[address] = nil
            raise(InAppAlert(id: "chat\n" + host + "\n" + conversation, address: address, kind: kind,
                             summary: reply.map { Self.oneLine($0) }, attentionID: nil, arrivedAt: now))
            changed = true
        }
        return changed
    }

    /// The Mac's pending requests after a read. New ones alert (except on the
    /// first read); ones no longer pending lose their alert.
    @discardableResult
    public mutating func receiveAttention(_ requests: [AttentionRequest], host: String, onScreen: Set<InAppAlertAddress>, now: Date) -> Bool {
        let pending = Set(requests.map(\.id))
        let before = alerts
        alerts.removeAll { $0.address.host == host && $0.attentionID.map { !pending.contains($0) } == true }
        guard let known = knownAttention[host] else {
            knownAttention[host] = pending
            return alerts != before
        }
        for request in requests where !known.contains(request.id) {
            guard let conversation = request.conversationId else { continue }
            let address = InAppAlertAddress(host: host, conversation: conversation)
            guard !onScreen.contains(address) else { continue }
            // The request supersedes a finished/failed alert for that chat.
            alerts.removeAll { $0.address == address && !$0.kind.isRequest }
            raise(InAppAlert(id: "attention\n" + host + "\n" + request.id, address: address,
                             kind: request.isQuestion ? .question : .approval,
                             summary: Self.oneLine(request.inAppSummary), attentionID: request.id, arrivedAt: now))
        }
        // Remember only what is pending now, so the set stays bounded; a
        // resolved request never comes back.
        knownAttention[host] = pending
        return alerts != before
    }

    /// A notification that arrived while Wonder is open, resolved to its chat.
    /// It alerts only when the live stream did not already cover that chat.
    @discardableResult
    public mutating func receiveNotification(_ address: InAppAlertAddress, body: String?, completed: Bool?, onScreen: Set<InAppAlertAddress>, now: Date) -> Bool {
        guard !onScreen.contains(address), !alerts.contains(where: { $0.address == address }) else { return false }
        if let last = raisedAt[address], now.timeIntervalSince(last) < Self.duplicateWindow { return false }
        raise(InAppAlert(id: "chat\n" + address.host + "\n" + address.conversation, address: address,
                         kind: completed == true ? .finished : .update,
                         summary: body.map { Self.oneLine($0) }, attentionID: nil, arrivedAt: now))
        return true
    }

    // MARK: User and lifecycle

    /// A chat came on screen: its alerts are seen.
    public mutating func opened(_ address: InAppAlertAddress) {
        alerts.removeAll { $0.address == address }
        replies[address] = nil
    }
    public mutating func dismiss(_ id: String) { alerts.removeAll { $0.id == id } }
    public mutating func dismissAll() { alerts.removeAll() }
    /// A Mac was unpaired or its access ended.
    public mutating func forget(host: String) {
        alerts.removeAll { $0.address.host == host }
        knownAttention[host] = nil
        raisedAt = raisedAt.filter { $0.key.host != host }
        replies = replies.filter { $0.key.host != host }
    }

    /// Every alert is a heads-up: it leaves once `lifetime` has passed, a
    /// request too (the chat's request pill and the Chats list still show it).
    @discardableResult
    public mutating func expire(now: Date, lifetime: TimeInterval) -> Bool {
        let before = alerts.count
        alerts.removeAll { now.timeIntervalSince($0.arrivedAt) >= lifetime }
        return alerts.count != before
    }
    /// When the next alert expires, for one timer.
    public func nextExpiry(lifetime: TimeInterval) -> Date? {
        alerts.map { $0.arrivedAt.addingTimeInterval(lifetime) }.min()
    }

    // MARK: Helpers

    private mutating func raise(_ alert: InAppAlert) {
        alerts.removeAll { $0.id == alert.id }
        alerts.insert(alert, at: 0)
        if raisedAt.count >= Self.memory { raisedAt.removeAll() }
        raisedAt[alert.address] = alert.arrivedAt
    }
    private mutating func remember(_ key: String) -> Bool {
        guard seenEventSet.insert(key).inserted else { return false }
        seenEvents.append(key)
        if seenEvents.count > Self.memory { seenEventSet.remove(seenEvents.removeFirst()) }
        return true
    }
    static func date(_ value: String) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: value) { return date }
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: value)
    }
    /// One readable line: whitespace and line breaks collapse, long text ends
    /// with an ellipsis.
    public static func oneLine(_ text: String, limit: Int = 160) -> String {
        let collapsed = text.split(whereSeparator: { $0.isWhitespace || $0.isNewline }).joined(separator: " ")
        guard collapsed.count > limit else { return collapsed }
        return String(collapsed.prefix(limit - 1)).trimmingCharacters(in: .whitespaces) + "…"
    }
}

public extension AttentionRequest {
    /// What the agent is asking for, in a few words, without decision details.
    var inAppSummary: String {
        if isQuestion, let question = params.questions?.first?.question, !question.isEmpty { return question }
        if computerAction != nil { return "Use your computer" }
        if let command = params.command, !command.isEmpty { return "Run " + command }
        if method == "item/fileChange/requestApproval" {
            if let reason = params.reason, !reason.isEmpty { return reason }
            if let root = params.grantRoot { return "Write to " + root }
            return "Change files"
        }
        if let message = params.message, !message.isEmpty { return message }
        if let detail = permissionDetails.first { return detail }
        if isSecretRequest, let label = params.secretRequest?.label { return "Enter " + label }
        return isQuestion ? "Answer a question" : "Approve a request"
    }
}

/// The alert's second line: who, and what happened.
public enum InAppAlertText {
    public static func line(kind: InAppAlertKind, agent: String, summary: String?) -> String {
        let summary = summary.map { InAppAlertQueue.oneLine($0) }.flatMap { $0.isEmpty ? nil : $0 }
        switch kind {
        case .approval: return "\(agent) needs permission" + (summary.map { ": " + $0 } ?? "")
        case .question: return "\(agent) asks" + (summary.map { ": " + $0 } ?? " a question")
        case .finished: return summary.map { "\(agent): " + $0 } ?? "\(agent) finished replying"
        case .failed: return "\(agent) couldn’t finish the reply"
        case .update: return summary ?? "New activity from \(agent)"
        }
    }
    /// What the alert asks VoiceOver to announce.
    public static func announcement(kind: InAppAlertKind, chat: String, agent: String, summary: String?) -> String {
        "In \(chat): " + line(kind: kind, agent: agent, summary: summary)
    }
}
