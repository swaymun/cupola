#if WONDER_DIAGNOSTICS
import Foundation
import WonderPairing

/// `-diagnostics-project-rewind` (with project-actions): the Mac offers Edit from
/// here. Codex and Claude publish `rewind`, and Claude also file restore, whose
/// backups are off until Settings turns them on. A rewind waits for
/// `wonderctl signal rewind`; afterwards the thread ends before the chosen
/// message. `-diagnostics-project-rewind-conflict`: the Mac refuses in its own
/// words. `-diagnostics-project-rewind-lost`: the Mac rewinds, then its answer
/// is the plain "recovering" state instead of the result. History cannot be read
/// until `signal history` (the phone must say it cannot confirm), then shows the
/// rewind; a second request finds the message gone, as the Mac says.
/// `-diagnostics-project-rewind-backups-slow` / `-backups-failing`: Claude's
/// backups are on, but the setting is read late (held for `signal rewind`) or
/// fails once before it answers. Nothing leaves the phone.
extension DiagnosticSubagentFixture {
    static var rewindFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-project-rewind") }
    static var rewindConflictFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-project-rewind-conflict") }
    static var rewindLostFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-project-rewind-lost") }
    static var rewindBackupsSlowFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-project-rewind-backups-slow") }
    static var rewindBackupsFailingFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-project-rewind-backups-failing") }
    static let rewindRecovering = "Cupola is recovering this edit. History and new messages are paused. If this continues, restart Cupola on this computer."
    static let rewindGone = "That message is no longer in this chat. Reopen it and try again."
    static let rewindRefusal = "This conversation is open in Claude on your Mac. Close it there, then edit it here."

    /// Codex rewinds; Claude rewinds and can restore files.
    static func rewindFamilies() -> [[String: Any]] {
        [AgentFamily.codex, .claude].map { family in
            var record = ProviderCapabilities.fixtureMac(family)
            record.rewind = true
            record.rewindFiles = family == .claude // provider identity: the fixture's own record
            let capabilities = (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(record))) ?? [:]
            return ["family": family.rawValue, "available": true, "capabilities": capabilities]
        }
    }
}

final class DiagnosticRewindState: @unchecked Sendable {
    static let shared = DiagnosticRewindState()
    private let lock = NSLock()
    private var rewoundTurn: String?
    private var backups = DiagnosticSubagentFixture.rewindBackupsSlowFixture || DiagnosticSubagentFixture.rewindBackupsFailingFixture
    /// Rewind POSTs the Mac received, and file-backup reads it answered.
    private var rewindRequests = 0
    private var backupReads = 0
    var counts: (rewinds: Int, backupReads: Int) { lock.withLock { (rewindRequests, backupReads) } }

    struct Reply { let status: Int; let body: Data; let held: Bool }

    /// The reply to a rewind or file-backup request, or nil when the path is not one.
    func respond(method: String, path: String, body: Data?, conversation: String) -> Reply? {
        let json = { (value: Any) in (try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])) ?? Data("{}".utf8) }
        lock.lock(); defer { lock.unlock() }
        if path == "/api/v1/settings/file-restore", method == "GET" {
            backupReads += 1
            if DiagnosticSubagentFixture.rewindBackupsFailingFixture, backupReads == 1 {
                return Reply(status: 503, body: Data("Cupola couldn’t read its settings.".utf8), held: false)
            }
            return Reply(status: 200, body: json(["providers": [["family": "claude", "enabled": backups]]]),
                         held: DiagnosticSubagentFixture.rewindBackupsSlowFixture)
        }
        if path == "/api/v1/settings/file-restore/claude", method == "PUT" {
            let request = body.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
            backups = request["enabled"] as? Bool ?? backups
            return Reply(status: 200, body: json(["providers": [["family": "claude", "enabled": backups]]]), held: false)
        }
        guard path == "/api/v1/conversations/\(conversation)/rewind", method == "POST" else { return nil }
        rewindRequests += 1
        if DiagnosticSubagentFixture.rewindConflictFixture {
            return Reply(status: 409, body: Data(DiagnosticSubagentFixture.rewindRefusal.utf8), held: true)
        }
        let request = body.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
        guard let turn = request["turnId"] as? String, let restoreFiles = request["restoreFiles"] as? Bool else {
            return Reply(status: 400, body: Data("Choose one of your messages.".utf8), held: false)
        }
        if rewoundTurn != nil {
            return Reply(status: 409, body: Data(DiagnosticSubagentFixture.rewindGone.utf8), held: false)
        }
        rewoundTurn = turn
        if DiagnosticSubagentFixture.rewindLostFixture {
            return Reply(status: 503, body: Data(DiagnosticSubagentFixture.rewindRecovering.utf8), held: true)
        }
        var result: [String: Any] = ["historyRevision": 1, "removedTurns": 2, "filesRestored": restoreFiles]
        if restoreFiles { result["filesChanged"] = 2 }
        return Reply(status: 200, body: json(result), held: true)
    }

    /// The conversation as the Mac serves it after a rewind: everything from the
    /// rewound message on is gone, at the next history revision.
    func trim(_ snapshot: [String: Any]) -> [String: Any] {
        guard let turn = lock.withLock({ rewoundTurn }) else { return snapshot }
        // Not readable yet: a snapshot of another conversation fails the phone's read.
        if DiagnosticSubagentFixture.rewindLostFixture, !DiagnosticSubagentFixture.signalled("history") {
            var unreadable = snapshot
            unreadable["conversationId"] = "unreadable-while-recovering"
            return unreadable
        }
        let time = { (value: Any?) in Double(value as? String ?? "") ?? 0 }
        let messages = snapshot["messages"] as? [[String: Any]] ?? []
        guard let cut = messages.first(where: { $0["codexTurnId"] as? String == turn }).map({ time($0["createdAt"]) }) else { return snapshot }
        var trimmed = snapshot
        trimmed["historyRevision"] = 1
        trimmed["messages"] = messages.filter { time($0["createdAt"]) < cut }
        trimmed["assistantMessages"] = (snapshot["assistantMessages"] as? [[String: Any]] ?? []).filter { time($0["createdAt"]) < cut }
        if var thread = snapshot["thread"] as? [String: Any] {
            thread["turns"] = (thread["turns"] as? [[String: Any]] ?? []).filter { time($0["createdAt"]) < cut }
            trimmed["thread"] = thread
        }
        return trimmed
    }
}
#endif
