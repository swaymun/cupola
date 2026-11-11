import XCTest
@testable import WonderPairing

final class ConversationRewindTests: XCTestCase {
    func testRequestCarriesTheTurnAndTheFileChoice() throws {
        let body = try RewindConversationRequest(turnId: "turn-2", restoreFiles: false).body()
        let object = try JSONSerialization.jsonObject(with: body) as? [String: Any]
        XCTAssertEqual(object?["turnId"] as? String, "turn-2")
        XCTAssertEqual(object?["restoreFiles"] as? Bool, false)
    }

    // Only the owner's own delivered message, with a turn, starts Edit from here.
    func testOnlyTheOwnersSentMessageOffersEditFromHere() {
        XCTAssertEqual(ConversationRewind.turnID(of: row(), isPending: false), "turn-1")
        XCTAssertNil(ConversationRewind.turnID(of: row(), isPending: true), "still sending")
        XCTAssertNil(ConversationRewind.turnID(of: row(isUser: false), isPending: false), "an agent reply")
        XCTAssertNil(ConversationRewind.turnID(of: row(turnId: nil), isPending: false), "no turn yet")
        XCTAssertNil(ConversationRewind.turnID(of: row(source: MessageSource(kind: "thread", sourceTitle: "Other")), isPending: false),
                     "another thread wrote it")
    }

    // The rewound message and its attachments come back without losing a draft.
    func testRewoundMessageReturnsToTheDraftAfterAnyTextAlreadyThere() {
        var empty = ComposerIntent()
        empty.restoreRewound(body: "Fix the build", attachmentIds: ["file-1"])
        XCTAssertEqual(empty.draft, "Fix the build")
        XCTAssertEqual(empty.draftAttachmentIds, ["file-1"])
        var drafting = ComposerIntent()
        drafting.draft = "Also check tests"
        drafting.draftAttachmentIds = ["file-1"]
        drafting.restoreRewound(body: "Fix the build", attachmentIds: ["file-1", "file-2"])
        XCTAssertEqual(drafting.draft, "Also check tests\n\nFix the build")
        XCTAssertEqual(drafting.draftAttachmentIds, ["file-1", "file-2"])
        var plain = ComposerIntent()
        plain.restoreRewound(body: "Hi", attachmentIds: [])
        XCTAssertNil(plain.draftAttachmentIds)
    }

    // A delivered message the host lists only in `messages` (its turn not
    // hydrated as a userMessage item) keeps its turn, so it can be edited.
    func testDeliveredMessageRowsKeepTheirTurn() throws {
        let json = #"{"conversationId":"c","hostEpoch":"e","lastSequence":1,"messages":[{"messageId":"m1","body":"Please continue","state":"completed","createdAt":"13000","codexTurnId":"turn-13","codexThreadId":"t","attachmentIds":[]}],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"turn-13","status":"completed","createdAt":"13000","updatedAt":"13900","items":[{"id":"reply","type":"agentMessage","state":"completed","createdAt":"13900","text":"Done"}]}]},"events":[]}"#
        let snapshot = try JSONDecoder().decode(ConversationSnapshot.self, from: Data(json.utf8))
        let user = try XCTUnwrap(snapshot.rows(author: "Codex").first { $0.isUser })
        XCTAssertEqual(ConversationRewind.turnID(of: user, isPending: false), "turn-13")
    }

    func testFileRestoreSettingsDecodePerProvider() throws {
        let settings = try JSONDecoder().decode(FileRestoreSettings.self, from: Data(#"{"providers":[{"family":"claude","enabled":true}]}"#.utf8))
        XCTAssertTrue(settings.enabled(.claude))
        XCTAssertFalse(settings.enabled(.codex))
    }

    // honesty.md #3: a backup read that has not finished, or failed, is never "off".
    func testBackupStatesAreSeparateInTheConfirmation() {
        let states: [FileBackups] = [.loading, .enabled, .disabled, .failed]
        let results = states.map { ConversationRewind.confirmation(agent: "Claude", canRestoreFiles: true, backups: $0) }
        XCTAssertEqual(results.map(\.choices), [[.conversationOnly], [.restoreFiles, .conversationOnly],
                                                [.conversationOnly], [.checkBackups, .conversationOnly]])
        XCTAssertEqual(Set(results.map(\.message)).count, 4, "each state says something different")
        for state in [FileBackups.loading, .failed] {
            let text = ConversationRewind.confirmation(agent: "Claude", canRestoreFiles: true, backups: state).message
            XCTAssertFalse(text.contains("turn on file backups"), "\(state) must not tell the owner to turn backups on")
        }
        XCTAssertTrue(ConversationRewind.confirmation(agent: "Claude", canRestoreFiles: true, backups: .disabled).message.contains("turn on file backups"))
        // A provider that cannot restore files never waits on, or offers, backups.
        for state in states {
            XCTAssertEqual(ConversationRewind.confirmation(agent: "Codex", canRestoreFiles: false, backups: state).choices, [.conversationOnly])
        }
        XCTAssertEqual(FileBackups(settings: FileRestoreSettings(providers: [.init(family: .claude, enabled: true)]), family: .claude), .enabled)
        XCTAssertEqual(FileBackups(settings: FileRestoreSettings(providers: []), family: .claude), .disabled)
    }

    // honesty.md #1: only a refusal before the Mac started shows nothing changed.
    func testOnlyARefusalProvesNothingChanged() {
        for status in [400, 401, 403, 404, 409, 422] { XCTAssertEqual(ConversationRewind.disposition(status: status), .refused, "\(status)") }
        for status in [500, 502, 503, 504, 408, 429, nil] { XCTAssertEqual(ConversationRewind.disposition(status: status), .unconfirmed, "\(String(describing: status))") }
        for status in [400, 404, 409, 422, 500, 502, 503, nil] as [Int?] {
            XCTAssertFalse(ConversationRewind.failureMessage(status: status, detail: nil, computer: "Mac").contains("Nothing changed"))
        }
        XCTAssertFalse(ConversationRewind.unconfirmedMessage(computer: "Mac").contains("Nothing changed"))
        XCTAssertTrue(ConversationRewind.unconfirmedMessage(computer: "Mac").contains("Couldn’t confirm"))
    }

    // The saved message returns to the draft once, whoever reconciles and however often.
    func testRecoveryRestoresOnceAndNeverOverwritesAnotherDraft() {
        var intent = ComposerIntent()
        intent.draft = "Unrelated draft"
        intent.draftAttachmentIds = ["keep"]
        let recovery = RewindRecovery(turnID: "turn-2", body: "Fix the build", attachmentIDs: ["file-1"], restoreFiles: false, historyRevision: 3)
        XCTAssertTrue(intent.beginRewind(recovery))
        XCTAssertEqual(intent.draft, "Unrelated draft", "saving the recovery leaves the draft alone")
        XCTAssertFalse(intent.beginRewind(RewindRecovery(turnID: "turn-3", body: "Other", attachmentIDs: [], restoreFiles: false, historyRevision: 3)),
                       "an unresolved edit is checked, never repeated")
        XCTAssertEqual(intent.rewindRecovery, recovery)
        XCTAssertTrue(intent.holdsUnsentWork)
        XCTAssertTrue(intent.finishRewind(id: recovery.id))
        XCTAssertEqual(intent.draft, "Unrelated draft\n\nFix the build")
        XCTAssertEqual(intent.draftAttachmentIds, ["keep", "file-1"])
        XCTAssertNil(intent.rewindRecovery)
        XCTAssertFalse(intent.finishRewind(id: recovery.id), "a second reconciliation adds nothing")
        XCTAssertFalse(intent.abandonRewind(id: recovery.id))
        XCTAssertEqual(intent.draft, "Unrelated draft\n\nFix the build")
        var refused = ComposerIntent()
        refused.draft = "Mine"
        XCTAssertTrue(refused.beginRewind(recovery))
        XCTAssertFalse(refused.abandonRewind(id: "someone-else"))
        XCTAssertTrue(refused.abandonRewind(id: recovery.id))
        XCTAssertFalse(refused.finishRewind(id: recovery.id), "a refused edit cannot append the message")
        XCTAssertEqual(refused.draft, "Mine")
    }

    // The recovery survives a restart with the draft, and an old save without one still loads.
    func testRecoveryIsSavedWithTheDraftAndSurvivesARestart() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        XCTAssertNil(try store.loadComposer(conversation: "chat").rewindRecovery)
        var intent = ComposerIntent()
        intent.draft = "Typed since"
        let recovery = RewindRecovery(turnID: "turn-2", body: "Fix the build", attachmentIDs: ["file-1", "file-2"], restoreFiles: true, historyRevision: 4)
        XCTAssertTrue(intent.beginRewind(recovery))
        try store.saveComposer(intent, conversation: "chat")
        let restarted = ReadStore(root: root, host: "host", device: "phone")
        var loaded = try restarted.loadComposer(conversation: "chat")
        XCTAssertEqual(loaded.rewindRecovery, recovery)
        XCTAssertEqual(loaded.draft, "Typed since")
        XCTAssertNil(try restarted.loadComposer(conversation: "other").rewindRecovery, "scoped to its conversation")
        XCTAssertTrue(loaded.finishRewind(id: recovery.id))
        try restarted.saveComposer(loaded, conversation: "chat")
        let after = try ReadStore(root: root, host: "host", device: "phone").loadComposer(conversation: "chat")
        XCTAssertNil(after.rewindRecovery)
        XCTAssertEqual(after.draft, "Typed since\n\nFix the build")
        XCTAssertEqual(after.draftAttachmentIds, ["file-1", "file-2"])
    }

    // History decides: the revision moved on without the message, or it did not move.
    func testReconcileReadsRefreshedHistory() throws {
        let recovery = RewindRecovery(turnID: "turn-13", body: "Please continue", attachmentIDs: [], restoreFiles: false,
                                      historyRevision: 0, askedAt: 1000)
        let present = try snapshot(revision: 0, turns: ["turn-12", "turn-13"])
        let gone = try snapshot(revision: 1, turns: ["turn-12"])
        let movedButPresent = try snapshot(revision: 1, turns: ["turn-12", "turn-13"])
        XCTAssertEqual(ConversationRewind.reconcile(recovery, snapshot: gone, now: 1001), .rewound)
        XCTAssertEqual(ConversationRewind.reconcile(recovery, snapshot: movedButPresent, now: 1001), .unchanged)
        // Unmoved history: only the Mac's own answer, or enough time, says it will not move.
        XCTAssertEqual(ConversationRewind.reconcile(recovery, snapshot: present, now: 1001), .undetermined)
        XCTAssertEqual(ConversationRewind.reconcile(recovery, snapshot: present, now: 1000 + ConversationRewind.unansweredGrace), .unchanged)
        var answered = recovery
        answered.hostAnswered = true
        XCTAssertEqual(ConversationRewind.reconcile(answered, snapshot: present, now: 1001), .unchanged)
        // Files are never reported restored without the Mac's word.
        var restoring = recovery
        restoring = RewindRecovery(id: restoring.id, turnID: "turn-13", body: "Please continue", attachmentIDs: [], restoreFiles: true, historyRevision: 0, askedAt: 1000)
        let text = ConversationRewind.resolvedMessage(restoring, .rewound, computer: "Mac")
        XCTAssertFalse(text.contains("files restored"))
        XCTAssertTrue(text.contains("didn’t confirm whether files were restored"))
        XCTAssertTrue(ConversationRewind.resolvedMessage(recovery, .rewound, computer: "Mac").contains("files are unchanged"))
    }

    private func snapshot(revision: UInt64, turns: [String]) throws -> ConversationSnapshot {
        let items = turns.enumerated().map { index, id in
            #"{"id":"\#(id)","status":"completed","createdAt":"\#(1000 + index)","updatedAt":"\#(1000 + index)","items":[]}"#
        }.joined(separator: ",")
        let json = #"{"conversationId":"c","hostEpoch":"e","lastSequence":1,"historyRevision":\#(revision),"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[\#(items)]},"events":[]}"#
        return try JSONDecoder().decode(ConversationSnapshot.self, from: Data(json.utf8))
    }

    private func row(isUser: Bool = true, turnId: String? = "turn-1", source: MessageSource? = nil) -> ReadRow {
        ReadRow(id: "row", author: "You", text: "Fix the build", isUser: isUser, timestamp: "1000", turnId: turnId,
                item: ReadItem(id: "row", type: isUser ? "userMessage" : "agentMessage", state: "completed", text: "Fix the build",
                               createdAt: "1000", payload: [:]),
                source: source)
    }
}
