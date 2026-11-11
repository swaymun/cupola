import Foundation
import CryptoKit

public struct SendRequest: Codable, Sendable {
    public let deviceId: String
    public let clientMessageId: String
    public let body: String
    public let attachmentIds: [String]
    public var codexSkills: [CodexSkillSelection]? = nil
    public var expectedTurnId: String?
    /// A Project thread's model for this message alone; see `ProjectMessageModel`.
    public var projectModel: ProjectMessageModel? = nil
    /// Send now on a thread another app on the computer has open: the computer
    /// stops that app's process for the thread instead of waiting for it.
    public var takeOver: Bool? = nil
}

public struct SendReceipt: Codable, Sendable {
    public let clientMessageId: String
    public let wonderMessageId: String
    public let bodySha256: String
    public let conversationId: String
    public let deliveryState: String
    /// Whether the message waits in the conversation's queue rather than being
    /// sent now; absent from Macs that do not decide it.
    public var queued: Bool? = nil
}

public enum SendFailure: Error { case empty, tooLarge, pending, receiptMismatch }

public struct PendingSend: Codable, Sendable {
    public let request: SendRequest
    public let createdAt: String
    public var receipt: SendReceipt?
    public var rejected: Bool?
    /// Promoted from the thread's outbox: a refusal returns it there, with
    /// its reason, instead of into the composer draft.
    public var fromOutbox: Bool? = nil
}

/// Reconcile an IME commit with text appended while the marked candidate was
/// still being edited. Never restore already-sent base text after a draft clear.
public enum ComposerComposition {
    /// Keep the original input until the model confirms it accepted a write.
    /// Each retry merges against the latest draft, including a Send/reset clear.
    public struct PendingCommit {
        private let base: String
        private let committed: String
        public init(base: String, committed: String) { self.base = base; self.committed = committed }
        public func replacingCommittedText(_ text: String) -> Self { Self(base: base, committed: text) }
        public func apply(to currentDraft: String, save: (String) -> String) -> String? {
            let merged = ComposerComposition.merge(base: base, committed: committed, currentDraft: currentDraft)
            return save(merged) == merged ? merged : nil
        }
    }
    public static func merge(base: String, committed: String, currentDraft: String) -> String {
        if currentDraft == base { return committed }
        if committed == base { return currentDraft }
        if currentDraft.hasPrefix(base) {
            return join(committed, String(currentDraft.dropFirst(base.count)))
        }
        // A send/restore replaced the base while composition was active. Retain
        // the current draft and only the user's changed span, not the stale base.
        let before = Array(base), after = Array(committed)
        var prefix = 0
        while prefix < min(before.count, after.count), before[prefix] == after[prefix] { prefix += 1 }
        var suffix = 0
        while suffix < min(before.count, after.count) - prefix,
            before[before.count - suffix - 1] == after[after.count - suffix - 1] { suffix += 1 }
        return join(currentDraft, String(after[prefix..<(after.count - suffix)]))
    }
    private static func join(_ first: String, _ second: String) -> String {
        let separated = first.isEmpty || second.isEmpty || first.last?.isWhitespace == true || second.first?.isWhitespace == true
        return first + (separated ? "" : " ") + second
    }
}

/// Saved atomically before networking. A retry always uses the original bytes and ID.
public struct ComposerIntent: Codable, Sendable {
    /// Transient identity for attachment/send state, excluded from Codable.
    /// Text has a small durable overlay and can change during preparation.
    public private(set) var nonTextMutationID = UUID()
    private enum CodingKeys: String, CodingKey {
        case stagedFiles, draftAttachmentIds, codexSkills, draft, lastDictationRequestID, pending, recoveredPending, outbox, removedUploadIds, rewindRecovery
    }
    public var stagedFiles: [StagedFile]? { didSet { nonTextMutationID = UUID() } }
    public var draftAttachmentIds: [String]? { didSet { nonTextMutationID = UUID() } }
    public var codexSkills: [CodexSkillSelection]? { didSet { if oldValue != codexSkills { nonTextMutationID = UUID() } } }
    public var draft = "" { didSet { codexSkills = CodexSkillSelection.retaining(codexSkills, in: draft) } }
    public private(set) var lastDictationRequestID: String? { didSet { nonTextMutationID = UUID() } }
    public internal(set) var pending: PendingSend? { didSet { nonTextMutationID = UUID() } }
    /// Requests made by a previous pairing remain immutable and readable, but
    /// cannot block a new draft or be retried under the replacement identity.
    public private(set) var recoveredPending: [PendingSend]? { didSet { nonTextMutationID = UUID() } }
    /// Messages written while the Mac was unreachable, oldest first; see `OutboxMessage`.
    public internal(set) var outbox: [OutboxMessage]? { didSet { nonTextMutationID = UUID() } }
    /// An Edit from here the Mac may have done: the selected message, kept until
    /// history shows whether it was rewound (`RewindRecovery`).
    public internal(set) var rewindRecovery: RewindRecovery? { didSet { nonTextMutationID = UUID() } }
    public init() {}
    @discardableResult public mutating func preservePreviousIdentityPending(currentDevice: String) -> Bool {
        guard let pending, pending.request.deviceId != currentDevice else { return false }
        var recovered = recoveredPending ?? []
        if !recovered.contains(where: { $0.request.clientMessageId == pending.request.clientMessageId }) { recovered.append(pending) }
        recoveredPending = recovered
        self.pending = nil
        return true
    }
    /// The durable draft can contain both local files waiting for upload and
    /// server files restored after a rejected or interrupted send. Keep their
    /// count and identity in one place so attachment limits and removal agree.
    public var attachmentIDs: [String] {
        var result: [String] = []
        for id in (stagedFiles ?? []).map(\.id) + (draftAttachmentIds ?? []) where !result.contains(id) {
            result.append(id)
        }
        return result
    }
    public var attachmentCount: Int { attachmentIDs.count }
    /// Copies on the Mac of files removed from this draft after an earlier,
    /// unsent attempt uploaded them. The Mac forgets each (it refuses a file a
    /// sent message carries); one it could not reach waits for the next try.
    public private(set) var removedUploadIds: [String]?

    public mutating func removeAttachment(id: String) {
        let uploaded = stagedFiles?.first(where: { $0.id == id })?.uploaded?.id
            ?? ((draftAttachmentIds ?? []).contains(id) ? id : nil)
        stagedFiles?.removeAll { $0.id == id }
        draftAttachmentIds?.removeAll { $0 == id }
        if let uploaded, !(removedUploadIds ?? []).contains(uploaded) { removedUploadIds = (removedUploadIds ?? []) + [uploaded] }
    }
    /// Removed uploads the Mac should forget now; one attached again since
    /// (or carried by the pending send) is kept and leaves the list.
    public mutating func uploadsToForget() -> [String] {
        let attached = Set((stagedFiles ?? []).compactMap(\.uploaded?.id) + (draftAttachmentIds ?? [])
            + (pending?.request.attachmentIds ?? []))
        removedUploadIds?.removeAll { attached.contains($0) }
        if removedUploadIds?.isEmpty == true { removedUploadIds = nil }
        return removedUploadIds ?? []
    }
    public mutating func forgotUpload(_ id: String) {
        removedUploadIds?.removeAll { $0 == id }
        if removedUploadIds?.isEmpty == true { removedUploadIds = nil }
    }
    public mutating func begin(device: String, clientMessageID: String = UUID().uuidString, expectedTurnId: String? = nil, projectModel: ProjectMessageModel? = nil, takeOver: Bool = false) throws {
        guard pending == nil else { throw SendFailure.pending }
        guard !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !(stagedFiles ?? []).isEmpty || !(draftAttachmentIds ?? []).isEmpty else { throw SendFailure.empty }
        guard draft.utf8.count <= 65536 else { throw SendFailure.tooLarge }
        guard attachmentCount <= 4 else { throw FileFailure.tooLarge }
        guard (stagedFiles ?? []).allSatisfy({ $0.uploaded != nil }) else { throw FileFailure.notUploaded }
        pending = PendingSend(request: SendRequest(deviceId: device, clientMessageId: clientMessageID,
            body: draft, attachmentIds: (stagedFiles ?? []).compactMap { $0.uploaded?.id } + (draftAttachmentIds ?? []), codexSkills: CodexSkillSelection.retaining(codexSkills, in: draft), expectedTurnId: expectedTurnId, projectModel: projectModel, takeOver: takeOver ? true : nil), createdAt: ISO8601DateFormatter().string(from: Date()))
        draft = ""
        stagedFiles = nil
        draftAttachmentIds = nil
    }
    public mutating func markRejected() { pending?.rejected = true }
    /// The Mac definitively refused the pending send. A message from the
    /// outbox goes back to the front of it with `reason` (its thread's outbox
    /// stops there); a composer send returns to an empty draft as before.
    public mutating func refuse(reason: String) {
        guard let pending, pending.receipt == nil else { return }
        if pending.fromOutbox == true {
            let request = pending.request
            var returned = OutboxMessage(id: request.clientMessageId, deviceId: request.deviceId, body: request.body,
                createdAt: pending.createdAt, codexSkills: request.codexSkills, projectModel: request.projectModel)
            returned.remoteAttachmentIds = request.attachmentIds.isEmpty ? nil : request.attachmentIds
            returned.refusal = reason
            outbox = [returned] + (outbox ?? [])
            self.pending = nil
            return
        }
        markRejected()
        if draft.isEmpty { try? restoreRejected() }
    }
    public mutating func restoreRejected() throws {
        guard let pending, pending.rejected == true, pending.receipt == nil,
              draft.isEmpty, (stagedFiles ?? []).isEmpty else { throw SendFailure.pending }
        draft = pending.request.body
        draftAttachmentIds = pending.request.attachmentIds
        codexSkills = pending.request.codexSkills
        self.pending = nil
    }
    public mutating func accept(_ receipt: SendReceipt, conversation: String) throws {
        guard let pending, receipt.clientMessageId == pending.request.clientMessageId,
              receipt.conversationId == conversation, !receipt.wonderMessageId.isEmpty,
              receipt.bodySha256 == SHA256.hash(data: Data(pending.request.body.utf8)).map({ String(format: "%02x", $0) }).joined()
        else { throw SendFailure.receiptMismatch }
        self.pending?.receipt = receipt
    }
    public mutating func reconcile(_ snapshot: ConversationSnapshot) {
        // Remove the local echo only after the authoritative projection includes it.
        let matches: (PendingSend) -> Bool = { pending in
            if let receipt = pending.receipt, receipt.conversationId != snapshot.conversationId { return false }
            return snapshot.messages.contains {
                ($0.messageId == pending.receipt?.wonderMessageId || $0.clientMessageId == pending.request.clientMessageId)
                    && ($0.body == pending.request.body || $0.originalBodySha256 == ConversationFile.digest(Data(pending.request.body.utf8)))
            }
        }
        if let pending, matches(pending) { self.pending = nil }
        recoveredPending?.removeAll(where: matches)
    }
}

public extension ReadStore {
    func loadComposer(conversation: String) throws -> ComposerIntent {
        let data = try loadIntent(conversation: conversation)
        let bodies = stagedBodies(conversation: conversation)
        var intent = try data.map { try composerDecoder(bodies).decode(ComposerIntent.self, from: $0) } ?? ComposerIntent()
        if let overlay = try loadIntent(conversation: draftOverlayKey(conversation)) {
            let saved = try JSONDecoder().decode(ComposerDraftOverlay.self, from: overlay)
            let revision = try intentRevision(conversation: conversation)
            if saved.baseRevision == revision || saved.previousRevision == revision { intent.draft = saved.draft }
            else { try? removeIntent(conversation: draftOverlayKey(conversation)) }
        }
        // A draft saved before bodies moved beside the intent is rewritten
        // once; if that fails the original stays readable.
        if bodies.decodedInlineBodies { try? saveComposer(intent, conversation: conversation) }
        return intent
    }
    /// Encodes only references to staged bodies: a body is written once,
    /// before the first intent naming it, so the save is small however many
    /// megabytes are attached.
    func saveComposer(_ intent: ComposerIntent, conversation: String) throws {
        let bodies = stagedBodies(conversation: conversation)
        try saveIntent(composerEncoder(bodies).encode(intent), conversation: conversation)
        bodies.retain(bodies.referencedDigests)
        // If cleanup is interrupted, the overlay's old file identity still
        // prevents it from replacing this committed intent on restart.
        try? removeIntent(conversation: draftOverlayKey(conversation))
    }
    func saveComposerDraft(_ draft: String, conversation: String) throws {
        let overlay = ComposerDraftOverlay(baseRevision: try intentRevision(conversation: conversation), draft: draft)
        try saveIntent(JSONEncoder().encode(overlay), conversation: draftOverlayKey(conversation))
    }
    func removeComposer(conversation: String) throws {
        try removeIntent(conversation: draftOverlayKey(conversation))
        try removeIntent(conversation: conversation)
        stagedBodies(conversation: conversation).retain([])
    }
    /// Keeps an unreadable composer before the owner starts over: its intent,
    /// typed-text overlay and staged attachment bodies move, unread, to a
    /// backup key. Nothing is deleted. The intent moves last, so a failed
    /// move leaves it in place and the chat still reports it unreadable.
    @discardableResult func setAsideComposer(conversation: String, at date: Date = Date()) throws -> String {
        let backup = "composer-set-aside:\(Int(date.timeIntervalSince1970 * 1000)):" + conversation
        try moveStagedBodies(from: conversation, to: backup)
        try moveIntent(from: draftOverlayKey(conversation), to: draftOverlayKey(backup))
        try moveIntent(from: conversation, to: backup)
        return backup
    }
}

public extension ComposerIntent {
    /// Anything the user wrote that has not reached a thread for sure. Only a
    /// composer without any of it may be deleted with its chat.
    var holdsUnsentWork: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !(stagedFiles ?? []).isEmpty
            || !(draftAttachmentIds ?? []).isEmpty || pending != nil || !(recoveredPending ?? []).isEmpty
            || !(outbox ?? []).isEmpty || rewindRecovery != nil
    }
    /// The thread is gone: nothing here may be sent automatically again. A
    /// pending send without a receipt is refused (an outbox one goes back to
    /// the outbox front), and every waiting message carries `reason`, as a
    /// refusal does in the outbox drain.
    mutating func fenceAfterRemoval(reason: String) {
        // A composer send stays the pending record, refused: restoring it into
        // an empty draft would drop its identity and its place before the outbox.
        if let pending, pending.receipt == nil {
            if pending.fromOutbox == true { refuse(reason: reason) } else { markRejected() }
        }
        for message in outbox ?? [] where message.refusal == nil { markOutboxRefused(id: message.id, reason: reason) }
    }
}

/// Unsent work kept after its chat disappeared from the computer (a history
/// read answered "not found"). It is stored apart from every chat's draft, so
/// nothing sends it on its own; the user copies, moves or discards it.
public struct OrphanedDraft: Codable, Identifiable, Sendable {
    public let id: String
    public let conversationId: String
    public let title: String
    public let removedAt: String
    public let intent: ComposerIntent

    public init(conversationId: String, title: String, intent: ComposerIntent, reason: String,
                id: String = UUID().uuidString, removedAt: Date = Date()) {
        var fenced = intent
        fenced.fenceAfterRemoval(reason: reason)
        self.id = id; self.conversationId = conversationId; self.title = title
        self.removedAt = ISO8601DateFormatter().string(from: removedAt); self.intent = fenced
    }

    /// Every message body kept, oldest send first, then the draft being written.
    public var text: String {
        let sends = (intent.recoveredPending ?? []).map(\.request.body) + [intent.pending?.request.body].compactMap { $0 }
        let parts = sends + (intent.outbox ?? []).map(\.body) + [intent.draft]
        return parts.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            .joined(separator: "\n\n")
    }
    /// Files whose bytes are still on this device, as new attachments.
    public var localFiles: [StagedFile] {
        ((intent.outbox ?? []).flatMap { $0.stagedFiles ?? [] } + (intent.stagedFiles ?? [])).compactMap {
            try? StagedFile(name: $0.name, mimeType: $0.mimeType, data: $0.data)
        }
    }
    /// Files that were only on the computer and left with the chat.
    public var computerOnlyFileCount: Int {
        let sends = (intent.recoveredPending ?? []) + [intent.pending].compactMap { $0 }
        return (intent.draftAttachmentIds ?? []).count + sends.reduce(0) { $0 + $1.request.attachmentIds.count }
            + (intent.outbox ?? []).reduce(0) { $0 + ($1.remoteAttachmentIds?.count ?? 0) }
    }
}

public extension ReadStore {
    func loadOrphanedDrafts() throws -> [OrphanedDraft] {
        guard let data = try loadIntent(conversation: orphanedDraftsKey) else { return [] }
        return try JSONDecoder().decode([OrphanedDraft].self, from: data)
    }
    /// One atomic file holds every orphaned draft; an empty list removes it.
    func saveOrphanedDrafts(_ drafts: [OrphanedDraft]) throws {
        if drafts.isEmpty { try removeIntent(conversation: orphanedDraftsKey) }
        else { try saveIntent(JSONEncoder().encode(drafts), conversation: orphanedDraftsKey) }
    }
}

private let orphanedDraftsKey = "orphaned-drafts-v1"

func composerEncoder(_ bodies: StagedFileBodies) -> JSONEncoder {
    let encoder = JSONEncoder()
    encoder.userInfo[StagedFileBodies.key] = bodies
    return encoder
}

func composerDecoder(_ bodies: StagedFileBodies) -> JSONDecoder {
    let decoder = JSONDecoder()
    decoder.userInfo[StagedFileBodies.key] = bodies
    return decoder
}

private struct ComposerDraftOverlay: Codable {
    let baseRevision: String
    let draft: String
    /// During a prepared replacement the same text must survive either side
    /// of the rename, including interruption between overlay write and commit.
    var previousRevision: String? = nil
}

private func draftOverlayKey(_ conversation: String) -> String { "composer-draft-v1:" + conversation }

/// Writing new attachment bodies, encoding the intent and writing its
/// prepared file run on this actor's executor, never on the calling UI actor.
/// Nothing becomes durable intent until the caller validates its ownership
/// and commits the result.
public actor ComposerPersistenceWorker {
    public static let shared = ComposerPersistenceWorker()
    public init() {}
    public func prepare(_ intent: ComposerIntent, store: ReadStore, conversation: String,
                        expectedRevision: String) throws -> PreparedComposer {
        try Task.checkCancellation()
        let bodies = store.stagedBodies(conversation: conversation)
        let data = try composerEncoder(bodies).encode(intent)
        try Task.checkCancellation()
        let file = try store.prepareIntent(data, conversation: conversation, expectedRevision: expectedRevision)
        try Task.checkCancellation()
        return PreparedComposer(file: file, store: store, conversation: conversation, bodies: bodies)
    }
}

public final class PreparedComposer: Sendable {
    private let file: ReadStore.PreparedIntent
    private let store: ReadStore
    private let conversation: String
    private let bodies: StagedFileBodies
    init(file: ReadStore.PreparedIntent, store: ReadStore, conversation: String, bodies: StagedFileBodies) {
        self.file = file; self.store = store; self.conversation = conversation; self.bodies = bodies
    }
    @MainActor public func commit(draft: String? = nil) throws {
        try Task.checkCancellation()
        if let draft {
            try file.validateRevision()
            let overlay = ComposerDraftOverlay(baseRevision: file.preparedRevision, draft: draft,
                                               previousRevision: file.expectedRevision)
            try store.saveIntent(JSONEncoder().encode(overlay), conversation: draftOverlayKey(conversation))
            try Task.checkCancellation()
        }
        try file.commit()
        bodies.retain(bodies.referencedDigests)
        // Changed text stays bound to this committed file. Otherwise an old
        // overlay cannot apply to the new identity, even if cleanup fails.
        if draft == nil { try? store.removeIntent(conversation: draftOverlayKey(conversation)) }
    }
}
