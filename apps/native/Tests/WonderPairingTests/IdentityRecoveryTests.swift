import XCTest
@testable import WonderPairing

final class IdentityRecoveryTests: XCTestCase {
    private func connection(_ host: String, device: String, origin: String? = nil) throws -> SavedConnection {
        let data = try JSONSerialization.data(withJSONObject: ["origin": origin ?? "https://\(host).invalid", "hostName": host,
            "credential": ["hostInstallationId": host, "deviceId": device, "sessionToken": "session", "csrfToken": "csrf"]])
        return try JSONDecoder().decode(SavedConnection.self, from: data)
    }

    func testLegacyConnectionDefaultsAndSharedInvalidationSurvivePersistence() throws {
        let old = try connection("studio", device: "original")
        XCTAssertFalse(old.requiresPairing)
        XCTAssertEqual(old.storageDeviceId, "original")
        var library = SavedConnections(legacy: old)
        library.save(try connection("laptop", device: "other"))
        try library.select("studio")
        library.requirePairing()
        var restored = try JSONDecoder().decode(SavedConnections.self, from: JSONEncoder().encode(library))
        XCTAssertTrue(restored.connections.allSatisfy(\.requiresPairing))
        restored.save(try connection("studio", device: "replacement"))
        XCTAssertFalse(restored.connections[0].requiresPairing)
        XCTAssertEqual(restored.connections[0].credential.deviceId, "replacement")
        XCTAssertEqual(restored.connections[0].storageDeviceId, "original")
        XCTAssertTrue(restored.connections[1].requiresPairing)
        XCTAssertEqual(restored.selectedHostIDs, ["studio"])
    }

    func testSameHostRepairKeepsDraftFilesAndReadingPositionWithoutCrossingHosts() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let old = try connection("studio", device: "original", origin: "https://studio.tail.ts.net:8443")
        let originalStore = ReadStore(root: root, host: "studio", device: old.storageDeviceId)
        var composer = ComposerIntent()
        composer.draft = "Keep this unsent text"
        composer.stagedFiles = [try StagedFile(id: "file", name: "notes.txt", mimeType: "text/plain", data: Data("notes".utf8))]
        try originalStore.saveComposer(composer, conversation: "chat")
        try originalStore.savePosition("older-message", conversation: "chat")
        let repaired = try connection("studio", device: "replacement", origin: "https://studio.tail.ts.net:8444").preservingStorage(from: old)
        XCTAssertEqual(repaired.origin, "https://studio.tail.ts.net:8444")
        XCTAssertEqual(repaired.credential.deviceId, "replacement")
        let newStore = ReadStore(root: root, host: "studio", device: repaired.storageDeviceId)
        XCTAssertEqual(newStore.directory, originalStore.directory)
        XCTAssertEqual(try newStore.loadComposer(conversation: "chat").draft, composer.draft)
        XCTAssertEqual(try newStore.loadComposer(conversation: "chat").stagedFiles?.first?.data, Data("notes".utf8))
        XCTAssertEqual(try newStore.loadPosition(conversation: "chat"), "older-message")
        let different = try connection("different-installation", device: "new-device").preservingStorage(from: old)
        XCTAssertEqual(different.storageDeviceId, "new-device")
        XCTAssertTrue(try ReadStore(root: root, host: "different-installation", device: different.storageDeviceId).loadComposer(conversation: "chat").draft.isEmpty)
    }

    func testUncertainOldSendIsImmutableAndDoesNotBlockNewDraft() throws {
        var intent = ComposerIntent()
        intent.draft = "Possibly already sent"
        try intent.begin(device: "old-phone")
        let pending = try XCTUnwrap(intent.pending)
        let encoder = JSONEncoder(); encoder.outputFormatting = .sortedKeys
        let original = try encoder.encode(pending)
        intent.draft = "New follow-up draft"
        intent.stagedFiles = [try StagedFile(id: "file", name: "notes.txt", mimeType: "text/plain", data: Data("notes".utf8))]
        XCTAssertFalse(intent.preservePreviousIdentityPending(currentDevice: "old-phone"))
        XCTAssertTrue(intent.preservePreviousIdentityPending(currentDevice: "new-phone"))
        XCTAssertFalse(intent.preservePreviousIdentityPending(currentDevice: "new-phone"))
        var restored = try JSONDecoder().decode(ComposerIntent.self, from: encoder.encode(intent))
        XCTAssertNil(restored.pending)
        XCTAssertEqual(restored.draft, "New follow-up draft")
        XCTAssertEqual(restored.stagedFiles?.first?.data, Data("notes".utf8))
        XCTAssertEqual(try encoder.encode(XCTUnwrap(restored.recoveredPending?.first)), original)
        restored.stagedFiles = nil
        try restored.begin(device: "new-phone")
        XCTAssertEqual(restored.pending?.request.deviceId, "new-phone")
        XCTAssertNotEqual(restored.pending?.request.clientMessageId, pending.request.clientMessageId)
        XCTAssertEqual(try encoder.encode(XCTUnwrap(restored.recoveredPending?.first)), original)
    }

    func testRecoveredSendNeedsMatchingAuthoritativeHistory() throws {
        var intent = ComposerIntent(); intent.draft = "Unconfirmed"
        try intent.begin(device: "old-phone")
        let original = try XCTUnwrap(intent.pending)
        intent.preservePreviousIdentityPending(currentDevice: "replacement")
        let empty = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
            messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true))
        intent.reconcile(empty)
        XCTAssertEqual(intent.recoveredPending?.count, 1)
        let mismatched = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [ConversationMessage(clientMessageId: original.request.clientMessageId, messageId: "message", body: "different", state: "completed", createdAt: "1000", attachmentIds: [])], assistantMessages: [], thread: empty.thread)
        intent.reconcile(mismatched)
        XCTAssertEqual(intent.recoveredPending?.count, 1)
        let confirmed = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 3,
            messages: [ConversationMessage(clientMessageId: original.request.clientMessageId, messageId: "message", body: "Unconfirmed", state: "completed", createdAt: "1000", attachmentIds: [])], assistantMessages: [], thread: empty.thread)
        intent.reconcile(confirmed)
        XCTAssertTrue(intent.recoveredPending?.isEmpty == true)
    }
}
