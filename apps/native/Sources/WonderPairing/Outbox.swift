import Foundation

/// A message written while Wonder on the Mac was unreachable. It waits in its
/// thread's outbox, saved with the thread's `ComposerIntent` (the durable draft
/// store, never a cache), and is sent in order once the Mac is back.
///
/// Its `id` is the client message ID it is sent with. The Mac keeps one
/// message per device and client message ID, so a send whose answer was lost
/// can be repeated but never accepted twice. `deviceId` is the pairing that
/// wrote it: a later pairing never sends it.
public struct OutboxMessage: Codable, Identifiable, Sendable {
    public let id: String
    public let deviceId: String
    public let body: String
    public let createdAt: String
    public var codexSkills: [CodexSkillSelection]?
    /// The model chosen for this message alone when it was written.
    public var projectModel: ProjectMessageModel?
    /// Attachment bytes kept on this iPhone until the outbox drains; each
    /// records its upload so a retried drain does not upload it again.
    public var stagedFiles: [StagedFile]?
    /// Files already on the Mac (from an earlier refused send).
    public var remoteAttachmentIds: [String]?
    /// Why the Mac refused it, in plain words. A refused message stops its
    /// thread's outbox until Retry, Edit or Cancel.
    public var refusal: String?

    public init(id: String, deviceId: String, body: String, createdAt: String,
                codexSkills: [CodexSkillSelection]? = nil, projectModel: ProjectMessageModel? = nil) {
        self.id = id; self.deviceId = deviceId; self.body = body; self.createdAt = createdAt
        self.codexSkills = codexSkills; self.projectModel = projectModel
    }

    public var attachmentCount: Int { (stagedFiles?.count ?? 0) + (remoteAttachmentIds?.count ?? 0) }
    var localBytes: Int { (stagedFiles ?? []).reduce(0) { $0 + $1.data.count } }
}

public enum Outbox {
    /// Messages one thread can hold while the Mac is unreachable.
    public static let messageLimit = 10
    /// Attachment bytes one thread's outbox keeps on this iPhone, saved
    /// beside the thread's draft (see `StagedFileBodies`).
    public static let byteLimit = 16 * 1024 * 1024

    public enum Failure: Error, Equatable { case full, tooLarge, draftInUse, notFound }

    public static func explanation(_ failure: Failure) -> String {
        switch failure {
        case .full: "\(messageLimit) messages are already waiting for your computer. Send this one once it’s back, or cancel a waiting message."
        case .tooLarge: "Messages waiting for your computer can hold up to 16 MB of attachments. Remove an attachment, or send it once your computer is back."
        case .draftInUse: "Send or clear the message you’re writing first, then edit this one."
        case .notFound: "This message already left the outbox."
        }
    }

    /// What a failed outbox send means for the message.
    public enum Outcome: Equatable {
        /// The Mac refused it: it stays with this reason and its thread stops.
        case refused(String)
        /// Offline, busy or unanswered: try again later. A pending send that
        /// may have reached the Mac is checked against the thread first and
        /// repeated only under its own client message ID.
        case retryLater
    }

    public static func outcome(of error: Error) -> Outcome {
        switch error {
        case PairingFailure.annotationRejected(_, let detail), PairingFailure.skillRejected(_, let detail):
            return .refused(detail)
        case PairingFailure.hostMessage(let code, let detail) where refuses(code):
            return .refused(detail)
        case PairingFailure.response(let code) where refuses(code):
            return .refused(reason(code))
        default:
            return .retryLater
        }
    }

    /// A 4xx is the Mac's answer about this message. Sign-in (401), timeouts
    /// (408, 425) and rate limits (429) can pass on their own.
    static func refuses(_ code: Int) -> Bool { (400..<500).contains(code) && ![401, 408, 425, 429].contains(code) }

    static func reason(_ code: Int) -> String {
        switch code {
        case 403: "Your computer no longer accepts messages from this device. Check this device with cupola devices on your computer."
        case 404, 410: "This thread is no longer on your computer."
        case 409: "Your computer couldn’t take this message. The thread may be archived; check it on your computer."
        case 412: "This thread’s model settings changed. Check the model."
        case 413: "This message is too large for your computer."
        default: "Your computer couldn’t use this message. Edit it, then try again."
        }
    }

    /// Wait before the next drain after a failure that may pass: 1, 2, 4, 8,
    /// then 16 seconds.
    public static func retryDelay(attempt: Int) -> Duration {
        .seconds(1 << min(max(attempt - 1, 0), 4))
    }
}

public extension ComposerIntent {
    /// Local attachment bytes held by the outbox.
    var outboxBytes: Int { (outbox ?? []).reduce(0) { $0 + $1.localBytes } }

    /// Messages a previous pairing wrote. They are never sent; the owner can
    /// copy or discard them.
    func outboxFromPreviousPairing(device: String) -> [OutboxMessage] {
        (outbox ?? []).filter { $0.deviceId != device }
    }

    /// Send while the Mac is unreachable: the draft, its attachments and
    /// skills become the newest outbox message. Refuses past the bounds.
    @discardableResult
    mutating func enqueue(device: String, clientMessageID: String = UUID().uuidString,
                          projectModel: ProjectMessageModel? = nil, now: Date = Date()) throws -> OutboxMessage {
        guard !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || attachmentCount > 0 else { throw SendFailure.empty }
        guard draft.utf8.count <= 65536 else { throw SendFailure.tooLarge }
        guard attachmentCount <= 4 else { throw FileFailure.tooLarge }
        guard (outbox ?? []).count < Outbox.messageLimit else { throw Outbox.Failure.full }
        let bytes = (stagedFiles ?? []).reduce(0) { $0 + $1.data.count }
        guard outboxBytes + bytes <= Outbox.byteLimit else { throw Outbox.Failure.tooLarge }
        var message = OutboxMessage(id: clientMessageID, deviceId: device, body: draft,
            createdAt: ISO8601DateFormatter().string(from: now),
            codexSkills: CodexSkillSelection.retaining(codexSkills, in: draft), projectModel: projectModel)
        message.stagedFiles = (stagedFiles ?? []).isEmpty ? nil : stagedFiles
        message.remoteAttachmentIds = (draftAttachmentIds ?? []).isEmpty ? nil : draftAttachmentIds
        outbox = (outbox ?? []) + [message]
        draft = ""
        stagedFiles = nil
        draftAttachmentIds = nil
        return message
    }

    /// The message this pairing sends next: the oldest one it wrote, once no
    /// earlier send of this thread is still pending. nil while that message
    /// is refused, so nothing behind it overtakes it.
    func nextOutboxMessage(device: String) -> OutboxMessage? {
        guard pending == nil, let first = (outbox ?? []).first(where: { $0.deviceId == device }),
              first.refusal == nil else { return nil }
        return first
    }

    /// Records an attachment upload made while the message is still waiting.
    mutating func noteOutboxUpload(message: String, file: String, uploaded: ConversationFile) {
        guard let index = outbox?.firstIndex(where: { $0.id == message }),
              let fileIndex = outbox?[index].stagedFiles?.firstIndex(where: { $0.id == file }) else { return }
        outbox?[index].stagedFiles?[fileIndex].uploaded = uploaded
    }

    /// Makes the next message the thread's pending send under its own ID, in
    /// the same saved state that removes it from the outbox.
    @discardableResult
    mutating func promoteOutbox(device: String) throws -> PendingSend {
        guard let next = nextOutboxMessage(device: device) else { throw Outbox.Failure.notFound }
        guard (next.stagedFiles ?? []).allSatisfy({ $0.uploaded != nil }) else { throw FileFailure.notUploaded }
        let attachments = (next.stagedFiles ?? []).compactMap { $0.uploaded?.id } + (next.remoteAttachmentIds ?? [])
        let promoted = PendingSend(request: SendRequest(deviceId: device, clientMessageId: next.id, body: next.body,
            attachmentIds: attachments, codexSkills: next.codexSkills, expectedTurnId: nil, projectModel: next.projectModel),
            createdAt: next.createdAt, fromOutbox: true)
        outbox?.removeAll { $0.id == next.id }
        if outbox?.isEmpty == true { outbox = nil }
        pending = promoted
        return promoted
    }

    /// Refused before it was sent (the thread is archived or gone, or it
    /// carries what the thread cannot take).
    mutating func markOutboxRefused(id: String, reason: String) {
        guard let index = outbox?.firstIndex(where: { $0.id == id }) else { return }
        outbox?[index].refusal = reason
    }

    /// A New chat's first message, sent by the outbox: a refusal leaves it
    /// in the new thread's outbox with the reason instead of in the draft.
    mutating func markPendingFromOutbox() { pending?.fromOutbox = true }

    /// Retry a refused message: its thread's outbox moves again.
    mutating func retryOutbox(id: String) throws {
        guard let index = outbox?.firstIndex(where: { $0.id == id }) else { throw Outbox.Failure.notFound }
        outbox?[index].refusal = nil
    }

    /// Cancel or discard: the only way a message leaves the outbox unsent.
    @discardableResult
    mutating func removeFromOutbox(id: String) -> OutboxMessage? {
        guard let index = outbox?.firstIndex(where: { $0.id == id }) else { return nil }
        let removed = outbox?.remove(at: index)
        if outbox?.isEmpty == true { outbox = nil }
        return removed
    }

    /// Edit: the message returns to the composer, which must be empty, with
    /// its attachments and skills. Sending it again makes a new message.
    mutating func editOutbox(id: String) throws {
        guard let message = (outbox ?? []).first(where: { $0.id == id }) else { throw Outbox.Failure.notFound }
        guard draft.isEmpty, attachmentCount == 0 else { throw Outbox.Failure.draftInUse }
        removeFromOutbox(id: id)
        draft = message.body
        codexSkills = message.codexSkills
        stagedFiles = message.stagedFiles
        draftAttachmentIds = message.remoteAttachmentIds
    }
}

public extension ReadStore {
    /// Conversations that may have outbox messages or a pending send, oldest
    /// first. Saved before the message itself, so it is never missing one; a
    /// listed conversation with nothing left to send is dropped when it drains.
    func loadOutboxIndex() throws -> [String] {
        guard let data = try loadIntent(conversation: Self.outboxIndexKey) else { return [] }
        return try JSONDecoder().decode([String].self, from: data)
    }
    func saveOutboxIndex(_ conversations: [String]) throws {
        if conversations.isEmpty { try removeIntent(conversation: Self.outboxIndexKey) }
        else { try saveIntent(JSONEncoder().encode(conversations), conversation: Self.outboxIndexKey) }
    }
    /// Lists a conversation before its message is saved (see `loadOutboxIndex`).
    func indexOutbox(_ conversation: String) throws {
        var index = try loadOutboxIndex()
        guard !index.contains(conversation) else { return }
        index.append(conversation)
        try saveOutboxIndex(index)
    }
    private static var outboxIndexKey: String { "outbox-index-v1" }
}

// MARK: - New chat outbox

public extension Outbox {
    /// Attachment bytes waiting New chats keep on this iPhone. Their files
    /// already live beside the drafts, not inside them.
    static let newChatByteLimit = 32 * 1024 * 1024

    /// Why another New chat cannot wait for the Mac, or nil when it can.
    static func newChatRefusal(adding draft: NewChatDraft, to queued: [NewChatDraft]) -> Failure? {
        guard queued.count < messageLimit else { return .full }
        let bytes = { (item: NewChatDraft) in (item.attachments ?? []).reduce(0) { $0 + $1.byteSize } }
        return queued.reduce(0) { $0 + bytes($1) } + bytes(draft) <= newChatByteLimit ? nil : .tooLarge
    }
}

public extension NewChatDraft {
    var isQueued: Bool { queuedAt != nil }

    /// Send while the Mac is unreachable: freeze this request for the device
    /// that wrote it. A different pairing never sends it.
    mutating func queue(device: String, now: Date = Date()) {
        if isSubmitted { outboxAttempted = true } else { submittedDeviceID = device; freeze() }
        if submittedDeviceID == nil { submittedDeviceID = device }
        queuedAt = ISO8601DateFormatter().string(from: now)
        outboxRefusal = nil; outboxRetryable = nil
    }

    /// Edit is safe when the Mac never received the request or refused it:
    /// it becomes an editable draft under a new request ID.
    var canEditQueued: Bool {
        preparedConversationID == nil && (outboxAttempted != true || outboxRefusal != nil)
    }

    mutating func unqueue() {
        queuedAt = nil; outboxAttempted = nil; outboxRefusal = nil; outboxRetryable = nil
        rejectSubmission()
    }

    mutating func refuseQueued(_ reason: String, retryable: Bool) {
        outboxRefusal = reason; outboxRetryable = retryable
    }

    /// The Mac's answer to creating a waiting New chat, or nil for a failure
    /// that may pass. Mirrors New chat's own notices.
    static func queuedRefusal(_ error: Error) -> (reason: String, retryable: Bool)? {
        switch error {
        case PairingFailure.response(412):
            return ("The project’s folders changed. Edit the message to review the working folder.", false)
        case PairingFailure.response(422):
            return ("This project, model or access choice can’t be used. Edit the message to change them.", false)
        case PairingFailure.response(409):
            return ("This request conflicts with the saved thread or its folder. Check the project on your computer.", true)
        case PairingFailure.response(404):
            return ("This project is no longer on your computer. Edit the message to choose another.", false)
        case PairingFailure.hostMessage(let code, let detail) where Outbox.refuses(code):
            return (detail, true)
        case PairingFailure.response(let code) where Outbox.refuses(code):
            return (Outbox.reason(code), true)
        default:
            return nil
        }
    }
}
