import Foundation

/// Edit from here: what the phone asks the Mac to rewind
/// (`POST /api/v1/conversations/{id}/rewind`). The owner's message that opened
/// `turnId`, and everything after it, leave the provider's history.
public struct RewindConversationRequest: Encodable, Equatable, Sendable {
    public let turnId: String
    /// Also restore files the agent edited from that message on. Sent only
    /// where the provider can (`ProviderCapabilities.rewindFiles`) and the owner
    /// turned its backups on.
    public let restoreFiles: Bool
    public init(turnId: String, restoreFiles: Bool) { self.turnId = turnId; self.restoreFiles = restoreFiles }
    public func body() throws -> Data { try JSONEncoder().encode(self) }
}

/// What the Mac did. `filesRestored` is true only when the provider restored files.
public struct RewindConversationResponse: Decodable, Equatable, Sendable {
    public let historyRevision: UInt64
    public let removedTurns: Int
    public let filesRestored: Bool
    public let filesChanged: Int?
    public let filesSkipped: Int?
}

/// Settings > Providers: per provider that can restore files on rewind, whether
/// it keeps the backups that needs (`GET /api/v1/settings/file-restore`).
public struct FileRestoreSettings: Decodable, Equatable, Sendable {
    public struct Provider: Decodable, Equatable, Sendable, Identifiable {
        public let family: AgentFamily
        public let enabled: Bool
        public var id: String { family.rawValue }
        public init(family: AgentFamily, enabled: Bool) { self.family = family; self.enabled = enabled }
    }
    public let providers: [Provider]
    public init(providers: [Provider]) { self.providers = providers }
    public func enabled(_ family: AgentFamily) -> Bool { providers.first { $0.family == family }?.enabled == true }
}

/// Saved with the chat's draft before an Edit from here is sent, so a reply the
/// phone never gets cannot lose the owner's message: what was selected, and the
/// history it was selected in. Cleared once history shows the edit happened (the
/// message returns to the draft, once) or did not.
public struct RewindRecovery: Codable, Equatable, Sendable {
    public let id: String
    public let turnID: String
    public let body: String
    public let attachmentIDs: [String]
    public let restoreFiles: Bool
    /// The chat's history revision when asked; a rewind moves it on.
    public let historyRevision: UInt64
    public let askedAt: TimeInterval
    /// The Mac answered, so it is no longer working on this request.
    public var hostAnswered: Bool
    public init(id: String = UUID().uuidString, turnID: String, body: String, attachmentIDs: [String], restoreFiles: Bool,
                historyRevision: UInt64, askedAt: TimeInterval = Date().timeIntervalSince1970, hostAnswered: Bool = false) {
        self.id = id; self.turnID = turnID; self.body = body; self.attachmentIDs = attachmentIDs
        self.restoreFiles = restoreFiles; self.historyRevision = historyRevision; self.askedAt = askedAt
        self.hostAnswered = hostAnswered
    }
}

/// Whether the provider keeps the backups that restoring files needs, as far as
/// the phone knows. A read that failed is not the same as backups being off.
public enum FileBackups: Equatable, Sendable {
    case loading, enabled, disabled, failed
    public init(settings: FileRestoreSettings, family: AgentFamily) { self = settings.enabled(family) ? .enabled : .disabled }
}

public enum ConversationRewind {
    /// The turn the owner's own sent message opened, if Edit from here can start
    /// at it: not a message another thread wrote, not one still sending or
    /// undelivered, and only once that turn has a native identity.
    public static func turnID(of row: ReadRow, isPending: Bool) -> String? {
        guard row.isUser, !isPending, row.source == nil, row.undelivered == nil, row.commandBlock == nil,
              let turn = row.turnId, !turn.isEmpty else { return nil }
        return turn
    }

    /// Whether the conversation offers Edit from here at all: the provider
    /// rewinds natively (`ProviderCapabilities.rewind`), the thread has started,
    /// nothing runs and it is not archived.
    public static func canRewind(rewind: Bool, hasNativeSession: Bool, hasActiveTurn: Bool, isArchived: Bool) -> Bool {
        rewind && hasNativeSession && !hasActiveTurn && !isArchived
    }

    /// What the owner chooses between, in the order shown.
    public enum Choice: Hashable, Sendable {
        /// Rewind the conversation; files stay as they are.
        case conversationOnly
        /// Rewind and restore the files the agent changed since.
        case restoreFiles
        /// Read the backup setting again after it could not be read.
        case checkBackups
    }

    /// The confirmation, which says exactly what changes.
    public struct Confirmation: Equatable, Sendable {
        public let title: String
        public let message: String
        public let choices: [Choice]
    }

    /// `canRestoreFiles`: the provider restores files (`rewindFiles`).
    /// `backups`: whether the owner turned its backups on in Settings; only a
    /// read that said so counts as off.
    public static func confirmation(agent: String, canRestoreFiles: Bool, backups: FileBackups) -> Confirmation {
        var message = "This message and everything after it are removed from \(agent)’s conversation, and the message returns to your draft so you can change it and send it again."
        let choices: [Choice]
        switch (canRestoreFiles, backups) {
        case (false, _):
            message += " Files on your computer stay as they are."
            choices = [.conversationOnly]
        case (true, .enabled):
            message += " You can also put back the files \(agent) changed since then, from the backups it kept. Otherwise your files stay as they are."
            choices = [.restoreFiles, .conversationOnly]
        case (true, .disabled):
            message += " Files on your computer stay as they are. To restore files too, turn on file backups for \(agent) in Settings; they apply to replies after that."
            choices = [.conversationOnly]
        case (true, .loading):
            message += " Checking whether \(agent) kept file backups. Until then, files on your computer stay as they are."
            choices = [.conversationOnly]
        case (true, .failed):
            message += " Couldn’t check whether \(agent) kept file backups, so files can’t be restored yet. Check again, or rewind the conversation and leave files as they are."
            choices = [.checkBackups, .conversationOnly]
        }
        return Confirmation(title: "Edit from this message?", message: message, choices: choices)
    }

    public static func label(_ choice: Choice) -> String {
        switch choice {
        case .conversationOnly: "Rewind conversation"
        case .restoreFiles: "Rewind and restore files"
        case .checkBackups: "Check file backups again"
        }
    }

    /// What the owner reads once the Mac rewound.
    public static func outcome(_ response: RewindConversationResponse) -> String {
        guard response.filesRestored else { return "Rewound. Your message is back in the draft; files are unchanged." }
        var text = "Rewound. Your message is back in the draft"
        if let changed = response.filesChanged {
            text += changed == 0 ? "; no files needed restoring." : "; \(changed) file\(changed == 1 ? "" : "s") restored."
        } else {
            text += "; files restored."
        }
        if let skipped = response.filesSkipped, skipped > 0 {
            text += " \(skipped) file\(skipped == 1 ? " was" : "s were") left as is because a link was in the way."
        }
        return text
    }

    /// What an error from the rewind request proves.
    public enum Disposition: Equatable, Sendable {
        /// The Mac answered that it did not rewind: nothing changed.
        case refused
        /// No answer, or an answer that cannot say whether the rewind happened.
        case unconfirmed
    }

    /// Only a Mac refusal before it started (a bad request, access, a missing,
    /// busy or unsupported chat) shows nothing changed. A lost or timed-out
    /// reply and a server error can follow a rewind that already happened.
    public static func disposition(status: Int?) -> Disposition {
        switch status {
        case 400, 401, 403, 404, 409, 422: .refused
        default: .unconfirmed
        }
    }

    /// What the owner reads when the Mac refused. The Mac's own words win when
    /// it sent them.
    public static func failureMessage(status: Int?, detail: String?, computer: String) -> String {
        if let detail, !detail.isEmpty { return detail }
        switch status {
        case 409: return "This chat is busy or hasn’t started. Edit it when the agent has finished replying."
        case 422: return "This kind of chat can’t be edited from an earlier message."
        case 404: return "This chat is no longer on \(computer)."
        case 401, 403: return "Access has ended. Reconnect to your computer in Settings."
        default: return "\(computer) didn’t rewind this chat. Your chat and draft are unchanged."
        }
    }

    /// The Mac may or may not have rewound. Says only that.
    public static func unconfirmedMessage(computer: String) -> String {
        "Couldn’t confirm the edit with \(computer). Your message is kept here while the chat is checked."
    }

    public enum Reconciliation: Equatable, Sendable {
        /// History moved on without the message: the edit happened.
        case rewound
        /// History did not move: the edit did not happen.
        case unchanged
        /// Not enough evidence yet: the Mac may still be working on it.
        case undetermined
    }

    /// Reads a refreshed history against the edit that was asked for. A rewind
    /// advances the history revision and removes the selected message; neither
    /// alone is taken as proof.
    public static func reconcile(_ recovery: RewindRecovery, snapshot: ConversationSnapshot,
                                 now: TimeInterval = Date().timeIntervalSince1970) -> Reconciliation {
        let revision = snapshot.historyRevision ?? 0
        if revision > recovery.historyRevision {
            return containsTurn(snapshot, recovery.turnID) ? .unchanged : .rewound
        }
        // The Mac has not moved the history. Its own answer, or time for a
        // request it may still be working on, says it will not.
        return recovery.hostAnswered || now - recovery.askedAt >= unansweredGrace ? .unchanged : .undetermined
    }

    /// A request the Mac never answered may still be running; its own limit is
    /// 30 s, plus the time it takes to fence history.
    public static let unansweredGrace: TimeInterval = 60

    static func containsTurn(_ snapshot: ConversationSnapshot, _ turn: String) -> Bool {
        snapshot.messages.contains { $0.codexTurnId == turn } || (snapshot.thread.turns ?? []).contains { $0.id == turn }
    }

    /// What the owner reads once history settles an unconfirmed edit. Files are
    /// reported restored only when the Mac said so, which it did not here.
    public static func resolvedMessage(_ recovery: RewindRecovery, _ result: Reconciliation, computer: String) -> String {
        switch result {
        case .rewound:
            recovery.restoreFiles
                ? "Rewound. Your message is back in the draft. \(computer) didn’t confirm whether files were restored, so check them before continuing."
                : "Rewound. Your message is back in the draft; files are unchanged."
        case .unchanged:
            "The edit didn’t go through. \(computer) still has this chat as it was."
        case .undetermined:
            unconfirmedMessage(computer: computer)
        }
    }
}

extension ComposerIntent {
    /// Puts a rewound message back in the draft with its attachments. Text
    /// already in the draft stays first, so nothing the owner wrote is lost.
    public mutating func restoreRewound(body: String, attachmentIds: [String]) {
        let current = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        draft = current.isEmpty ? body : draft + "\n\n" + body
        var ids = draftAttachmentIds ?? []
        for id in attachmentIds where !ids.contains(id) { ids.append(id) }
        draftAttachmentIds = ids.isEmpty ? nil : ids
    }

    /// Keeps the selected message before Edit from here is sent. False while an
    /// earlier edit is still unresolved: it is checked first, never repeated.
    public mutating func beginRewind(_ recovery: RewindRecovery) -> Bool {
        guard rewindRecovery == nil else { return false }
        rewindRecovery = recovery
        return true
    }

    /// The Mac rewound (or history shows it did): the saved message joins the
    /// draft and the recovery ends in the same write, so a second call, or a
    /// second reconciliation, adds nothing.
    @discardableResult public mutating func finishRewind(id: String) -> Bool {
        guard let recovery = rewindRecovery, recovery.id == id else { return false }
        restoreRewound(body: recovery.body, attachmentIds: recovery.attachmentIDs)
        rewindRecovery = nil
        return true
    }

    /// The Mac did not rewind: the draft stays as it was.
    @discardableResult public mutating func abandonRewind(id: String) -> Bool {
        guard rewindRecovery?.id == id else { return false }
        rewindRecovery = nil
        return true
    }

    /// The Mac answered without saying whether it rewound.
    public mutating func markRewindAnswered(id: String) {
        guard rewindRecovery?.id == id else { return }
        rewindRecovery?.hostAnswered = true
    }
}
