import XCTest
@testable import WonderPairing

/// Observable contract: a share saved before its first request comes back
/// after a restart with the same identity, answers and bytes, so finishing it
/// cannot send a second copy; a new share of the same content is a new item.
final class ShareRecoveryTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("share-recovery-\(UUID().uuidString)", isDirectory: true)
    }

    override func tearDownWithError() throws { try? FileManager.default.removeItem(at: directory) }

    private func file(_ text: String) throws -> StagedFile {
        try StagedFile(name: "notes.txt", mimeType: "text/plain", data: Data(text.utf8))
    }

    private func item(_ files: [StagedFile], body: String = "Look at this", createdAt: Date = Date()) -> PendingShare {
        PendingShare(host: "studio", deviceID: "phone", target: "thread:codex:a",
                     destination: .existing(targetID: "thread:codex:a", projectID: "p1", reference: "codex:a", conversationID: "c-1", attach: false),
                     body: body, files: files, title: "Share sheet", createdAt: createdAt)
    }

    func testEachLostReceiptIsRecoveredWithTheSameIdentityAfterARestart() throws {
        let files = [try file("first"), try file("second")]
        var pending = item(files)
        try ShareJournal(directory: directory).begin(pending, files: files)

        // Lost create/attach answer: nothing recorded yet beyond the item itself.
        var restored = try XCTUnwrap(ShareJournal(directory: directory).load().first)
        XCTAssertEqual(restored.id, pending.id)
        XCTAssertEqual(restored.conversationID, "c-1", "a known thread needs no attach")
        XCTAssertTrue(restored.uploads.isEmpty)

        // Lost answer after the first upload: the verified upload is kept.
        let uploaded = try JSONDecoder().decode(ConversationFile.self, from: JSONSerialization.data(withJSONObject: [
            "id": "file-1", "name": "notes.txt", "mimeType": "text/plain", "byteSize": 5,
            "sha256": ConversationFile.digest(Data("first".utf8)), "state": "available", "updatedAt": "now"]))
        pending.uploads[files[0].id] = uploaded
        try ShareJournal(directory: directory).update(pending)
        restored = try XCTUnwrap(ShareJournal(directory: directory).load().first)
        XCTAssertEqual(restored.uploads[files[0].id]?.id, "file-1")
        XCTAssertNil(restored.uploads[files[1].id])

        // Lost send receipt: the exact message request is reused.
        var intent = ComposerIntent()
        intent.draft = pending.body
        intent.draftAttachmentIds = ["file-1", "file-2"]
        try intent.begin(device: pending.deviceID, clientMessageID: pending.id)
        pending.request = try XCTUnwrap(intent.pending?.request)
        try ShareJournal(directory: directory).update(pending)
        restored = try XCTUnwrap(ShareJournal(directory: directory).load().first)
        XCTAssertEqual(restored.request?.clientMessageId, pending.id)
        XCTAssertEqual(restored.request?.attachmentIds, ["file-1", "file-2"])

        // The bytes come back exactly as shared, with their IDs.
        let reread = try ShareJournal(directory: directory).files(for: restored)
        XCTAssertEqual(reread.map(\.id), files.map(\.id))
        XCTAssertEqual(reread.map(\.data), files.map(\.data))

        // A confirmed receipt retires it.
        ShareJournal(directory: directory).remove(restored.id)
        XCTAssertTrue(ShareJournal(directory: directory).load().isEmpty)
    }

    func testANewShareOfTheSameContentIsANewItemNotARecovery() throws {
        let first = try file("same")
        let pending = item([first])
        // The same staged files on the same sheet are a retry.
        XCTAssertTrue(pending.isRetry(host: "studio", target: "thread:codex:a", body: "Look at this", fileIDs: [first.id]))
        // Shared again (new staged files) or changed, it is another share.
        let again = try file("same")
        XCTAssertFalse(pending.isRetry(host: "studio", target: "thread:codex:a", body: "Look at this", fileIDs: [again.id]))
        XCTAssertFalse(pending.isRetry(host: "studio", target: "thread:codex:a", body: "Look", fileIDs: [first.id]))
        XCTAssertFalse(pending.isRetry(host: "laptop", target: "thread:codex:a", body: "Look at this", fileIDs: [first.id]))
        XCTAssertNotEqual(item([again]).id, pending.id)
    }

    func testChangedBytesAreNeverSentInPlaceOfTheSharedOnes() throws {
        let shared = try file("original")
        let pending = item([shared])
        let journal = ShareJournal(directory: directory)
        try journal.begin(pending, files: [shared])
        try Data("tampered".utf8).write(to: directory.appendingPathComponent(pending.id).appendingPathComponent("file-0"))
        XCTAssertThrowsError(try journal.files(for: pending)) { XCTAssertEqual($0 as? FileFailure, .integrity) }
    }

    func testOldAndExcessItemsAreDroppedAndAnItemExistsOnlyWithItsRecord() throws {
        let journal = ShareJournal(directory: directory)
        let old = item([], createdAt: Date().addingTimeInterval(-ShareJournal.maximumAge - 60))
        try journal.begin(old, files: [])
        var kept: [PendingShare] = []
        for index in 0..<(ShareJournal.maximumItems + 2) {
            let next = item([], createdAt: Date().addingTimeInterval(TimeInterval(index)))
            try journal.begin(next, files: [])
            kept.append(next)
        }
        // A folder without its record (bytes written, record never saved) is not an item.
        try FileManager.default.createDirectory(at: directory.appendingPathComponent("orphan"), withIntermediateDirectories: true)
        let loaded = journal.load()
        XCTAssertEqual(loaded.map(\.id), kept.suffix(ShareJournal.maximumItems).map(\.id))
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appendingPathComponent("orphan").path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appendingPathComponent(old.id).path))
    }

}
