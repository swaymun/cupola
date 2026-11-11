import XCTest
import CryptoKit
@testable import WonderPairing

final class SendIntentTests: XCTestCase {
    // Contract: maximum-size intent bodies become durable together, while
    // preparation leaves the original draft readable until the final commit.
    @MainActor func testMaximumAttachmentsPrepareOffMainAndCommitAtomically() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "benchmark", device: "phone")
        let worker = ComposerPersistenceWorker()
        var original = ComposerIntent(); original.draft = "Prior durable draft"
        try store.saveComposer(original, conversation: "chat")
        var bytes = Data(repeating: 65, count: 8 * 1024 * 1024)
        bytes.replaceSubrange(0..<3, with: [0xff, 0xd8, 0xff])
        var intent = ComposerIntent(); intent.draft = "Maximum supported attachment bodies"
        intent.stagedFiles = try (0..<4).map {
            try StagedFile(id: "photo-\($0)", name: "Photo-\($0).jpg", mimeType: "image/jpeg", data: bytes)
        }
        let samples = ProcessInfo.processInfo.environment["WONDER_PHOTO_BENCHMARK"] == "1" ? 3 : 1
        for sample in 0..<samples {
            let start = Date()
            let revision = try store.intentRevision(conversation: "chat")
            let setup = Date().timeIntervalSince(start)
            let prepared = try await worker.prepare(intent, store: store, conversation: "chat", expectedRevision: revision)
            let preparedAt = Date()
            if sample == 0 { XCTAssertEqual(try store.loadComposer(conversation: "chat").draft, original.draft) }
            let commitStart = Date()
            try prepared.commit()
            let commit = Date().timeIntervalSince(commitStart)
            print("PHOTO_AFTER sample=\(sample) input_bytes=\(4 * bytes.count) prepare_wall_ms=\(preparedAt.timeIntervalSince(start) * 1000) commit_ms=\(commit * 1000) main_actor_ms=\((setup + commit) * 1000)")
        }
        if samples == 3 {
            // Separately instrument the same codec and prepared-file primitive;
            // the production-worker wall samples above have no instrumentation.
            for sample in 0..<samples {
                let phase = try await worker.measurePreparationForTest(intent, store: store,
                    revision: store.intentRevision(conversation: "chat"))
                let commitStart = Date()
                try phase.file.commit()
                let commit = Date().timeIntervalSince(commitStart)
                print("PHOTO_PHASE sample=\(sample) json_bytes=\(phase.bytes) encoding_ms=\(phase.encoding * 1000) write_ms=\(phase.write * 1000) main_commit_ms=\(commit * 1000)")
            }
        }
        let restarted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restarted.draft, intent.draft)
        XCTAssertEqual(restarted.stagedFiles?.map(\.id), intent.stagedFiles?.map(\.id))
        XCTAssertEqual(restarted.stagedFiles?.map(\.data), Array(repeating: bytes, count: 4))
    }

    // Contract: a draft's staged bodies are written once beside its intent.
    // Upload results, removals and outbox moves rewrite only small metadata,
    // never the megabytes already saved, and the bodies survive a restart.
    func testStagedBodiesAreWrittenOnceAndDraftSavesStaySmall() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        let bodies = (0..<4).map { index -> Data in
            var bytes = Data(repeating: 65, count: 8 * 1024 * 1024)
            bytes.replaceSubrange(0..<3, with: [0xff, 0xd8, 0xff])
            bytes[bytes.count - 1] = UInt8(index)
            return bytes
        }
        var intent = ComposerIntent(); intent.draft = "Four large photos"
        intent.stagedFiles = try bodies.enumerated().map {
            try StagedFile(id: "photo-\($0.offset)", name: "Photo-\($0.offset).jpg", mimeType: "image/jpeg", data: $0.element)
        }
        try store.saveComposer(intent, conversation: "chat")
        XCTAssertLessThan(try XCTUnwrap(store.loadIntent(conversation: "chat")).count, 8 * 1024,
                          "The intent names its bodies instead of carrying 32 MB of base64")
        let directory = store.stagedBodies(conversation: "chat").directory
        func identities() throws -> [String: NSNumber] {
            var result: [String: NSNumber] = [:]
            for name in try FileManager.default.contentsOfDirectory(atPath: directory.path) {
                let attributes = try FileManager.default.attributesOfItem(atPath: directory.appendingPathComponent(name).path)
                result[name] = attributes[.systemFileNumber] as? NSNumber
            }
            return result
        }
        let written = try identities()
        XCTAssertEqual(Set(written.keys), Set(intent.stagedFiles!.map { $0.sha256 + ".body" }))

        // An upload result is metadata: the saved bodies keep their file identity.
        let uploaded = ConversationFile(id: "uploaded-0", name: "Photo-0.jpg", mimeType: "image/jpeg", byteSize: bodies[0].count,
            sha256: ConversationFile.digest(bodies[0]), state: "available", updatedAt: "now")
        intent.stagedFiles?[0].uploaded = uploaded
        try store.saveComposer(intent, conversation: "chat")
        XCTAssertEqual(try identities(), written)

        // Removing a file drops only its body, after the intent without it is durable.
        intent.removeAttachment(id: "photo-3")
        try store.saveComposer(intent, conversation: "chat")
        var remaining = written; remaining.removeValue(forKey: ConversationFile.digest(bodies[3]) + ".body")
        XCTAssertEqual(try identities(), remaining)

        let restarted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restarted.draft, intent.draft)
        XCTAssertEqual(restarted.stagedFiles?.map(\.data), Array(bodies.prefix(3)))
        XCTAssertEqual(restarted.stagedFiles?.first?.uploaded?.id, "uploaded-0")
        XCTAssertNoThrow(try restarted.stagedFiles?[1].uploadBody(), "A body read back from its sidecar passes its hash check")

        try store.removeComposer(conversation: "chat")
        XCTAssertEqual(try identities(), [:])
    }

    // Contract: a draft saved with inline bodies (before sidecars) still opens
    // with the same bytes and is moved out of the intent once.
    func testInlineBodiesFromAnEarlierVersionMigrateOnLoad() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        let data = Data(repeating: 66, count: 512 * 1024)
        var legacy = ComposerIntent(); legacy.draft = "Saved by the previous version"
        legacy.stagedFiles = [try StagedFile(id: "legacy", name: "notes.txt", mimeType: "text/plain", data: data)]
        // The previous encoding: no store context, so the body is inline.
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(legacy)) as? [String: Any])
        var files = try XCTUnwrap(object["stagedFiles"] as? [[String: Any]])
        XCTAssertNotNil(files[0]["data"])
        files[0].removeValue(forKey: "sha256"); object["stagedFiles"] = files
        try store.saveIntent(JSONSerialization.data(withJSONObject: object), conversation: "chat")
        try store.saveComposerDraft("Typed after the update", conversation: "chat")

        let loaded = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(loaded.stagedFiles?.first?.data, data)
        XCTAssertEqual(loaded.stagedFiles?.first?.sha256, ConversationFile.digest(data))
        XCTAssertEqual(loaded.draft, "Typed after the update")
        XCTAssertLessThan(try XCTUnwrap(store.loadIntent(conversation: "chat")).count, 4 * 1024)
        let reloaded = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(reloaded.stagedFiles?.first?.data, data)
        XCTAssertEqual(reloaded.draft, "Typed after the update")
    }

    // Contract: a body is only accepted under the hash and size its intent
    // names; a missing, short or altered body never reaches the Mac, and a
    // saved name can never point outside the draft's body folder.
    func testStagedBodyReferencesAreVerified() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        let data = Data("Original attachment text".utf8)
        var intent = ComposerIntent()
        intent.stagedFiles = [try StagedFile(id: "file", name: "notes.txt", mimeType: "text/plain", data: data)]
        try store.saveComposer(intent, conversation: "chat")
        let body = store.stagedBodies(conversation: "chat").directory
            .appendingPathComponent(ConversationFile.digest(data) + ".body")

        try Data("Altered attachment text!".utf8.prefix(data.count)).write(to: body)
        let altered = try store.loadComposer(conversation: "chat")
        XCTAssertThrowsError(try altered.stagedFiles?.first?.uploadBody()) { XCTAssertEqual($0 as? FileFailure, .integrity) }

        try Data("short".utf8).write(to: body)
        XCTAssertThrowsError(try store.loadComposer(conversation: "chat"))
        try FileManager.default.removeItem(at: body)
        XCTAssertThrowsError(try store.loadComposer(conversation: "chat"))

        let saved = try XCTUnwrap(store.loadIntent(conversation: "chat"))
        let escaping = String(decoding: saved, as: UTF8.self)
            .replacingOccurrences(of: ConversationFile.digest(data), with: "../../intent")
        try store.saveIntent(Data(escaping.utf8), conversation: "chat")
        XCTAssertThrowsError(try store.loadComposer(conversation: "chat")) { XCTAssertEqual($0 as? FileFailure, .integrity) }
    }

    // Contract: bodies written by a preparation that loses to a newer commit
    // are not left referenced, and a stale preparation cannot remove bodies
    // the newer committed intent still needs.
    @MainActor func testStalePreparationNeverRemovesBodiesOfTheCommittedIntent() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        let kept = try StagedFile(id: "kept", name: "kept.txt", mimeType: "text/plain", data: Data("kept body".utf8))
        let added = try StagedFile(id: "added", name: "added.txt", mimeType: "text/plain", data: Data("added body".utf8))
        var original = ComposerIntent(); original.stagedFiles = [kept]
        try store.saveComposer(original, conversation: "chat")
        var candidate = original; candidate.stagedFiles?.append(added)
        let prepared = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
            expectedRevision: store.intentRevision(conversation: "chat"))
        var newer = original; newer.draft = "Committed first"
        try store.saveComposer(newer, conversation: "chat")
        XCTAssertThrowsError(try prepared.commit()) { XCTAssertEqual($0 as? FileFailure, .stale) }
        let restarted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restarted.draft, "Committed first")
        XCTAssertEqual(restarted.stagedFiles?.map(\.data), [kept.data])
        let names = try FileManager.default.contentsOfDirectory(atPath: store.stagedBodies(conversation: "chat").directory.path)
        XCTAssertEqual(names, [kept.sha256 + ".body"])

        let retry = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
            expectedRevision: store.intentRevision(conversation: "chat"))
        try retry.commit()
        XCTAssertEqual(try store.loadComposer(conversation: "chat").stagedFiles?.map(\.data), [kept.data, added.data])
    }

    @MainActor func testPreparedComposerRejectsStaleCommitAndPreservesNewerOverlayAndFiles() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        var original = ComposerIntent(); original.draft = "Original"
        original.stagedFiles = [try StagedFile(id: "old", name: "old.txt", mimeType: "text/plain", data: Data("old file".utf8))]
        for change in ["remove", "send", "outbox"] {
            var baseline = original
            let fileData = baseline.stagedFiles![0].data
            baseline.stagedFiles?[0].uploaded = ConversationFile(id: "uploaded-original", name: "old.txt", mimeType: "text/plain",
                byteSize: fileData.count, sha256: ConversationFile.digest(fileData),
                state: "available", updatedAt: "now")
            try store.saveComposer(baseline, conversation: "chat")
            var candidate = baseline
            candidate.stagedFiles?.append(try StagedFile(name: "new.txt", mimeType: "text/plain", data: Data("new file".utf8)))
            let prepared = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
                expectedRevision: store.intentRevision(conversation: "chat"))
            var newer = baseline
            switch change {
            case "remove": newer.removeAttachment(id: "old")
            case "send": try newer.begin(device: "phone", clientMessageID: "original-send")
            default: try newer.enqueue(device: "phone", clientMessageID: "original-outbox")
            }
            try store.saveComposer(newer, conversation: "chat")
            try store.saveComposerDraft("Typed after \(change)", conversation: "chat")
            let saved = try store.loadIntent(conversation: "chat")
            XCTAssertThrowsError(try prepared.commit()) { XCTAssertEqual($0 as? FileFailure, .stale) }
            XCTAssertEqual(try store.loadIntent(conversation: "chat"), saved)
            let restarted = try store.loadComposer(conversation: "chat")
            XCTAssertEqual(restarted.draft, "Typed after \(change)")
            XCTAssertEqual(restarted.attachmentCount, 0)
            if change == "send" {
                XCTAssertEqual(restarted.pending?.request.clientMessageId, "original-send")
                XCTAssertEqual(restarted.pending?.request.attachmentIds, ["uploaded-original"])
            }
            if change == "outbox" {
                XCTAssertEqual(restarted.outbox?.first?.id, "original-outbox")
                XCTAssertEqual(restarted.outbox?.first?.stagedFiles?.first?.uploaded?.id, "uploaded-original")
            }
        }
    }

    // The overlay is valid on both sides of the rename. A missing prepared
    // file simulates interruption after overlay binding but before publication.
    @MainActor func testPreparedComposerRebasesLatestTextAcrossCommitFailureAndRestartWithoutResurrectingSentDraft() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        func file(_ id: String) throws -> StagedFile {
            let data = Data(id.utf8)
            return try StagedFile(id: id, name: id + ".txt", mimeType: "text/plain", data: data,
                uploaded: ConversationFile(id: "uploaded-" + id, name: id + ".txt", mimeType: "text/plain",
                    byteSize: data.count, sha256: ConversationFile.digest(data), state: "available", updatedAt: "now"))
        }
        var original = ComposerIntent(); original.draft = "Captured text"; original.stagedFiles = [try file("old")]
        try store.saveComposer(original, conversation: "chat")
        let before = try store.loadIntent(conversation: "chat")
        var candidate = original; candidate.stagedFiles?.append(try file("new"))
        let failed = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
            expectedRevision: store.intentRevision(conversation: "chat"))
        let temporary = try XCTUnwrap(FileManager.default.contentsOfDirectory(at: store.directory,
            includingPropertiesForKeys: nil).first { $0.lastPathComponent.hasPrefix(".prepared-intent-") })
        try FileManager.default.removeItem(at: temporary)
        XCTAssertThrowsError(try failed.commit(draft: "Typed before interrupted commit"))
        XCTAssertEqual(try store.loadIntent(conversation: "chat"), before)
        XCTAssertEqual(try store.loadComposer(conversation: "chat").draft, "Typed before interrupted commit")
        XCTAssertEqual(try store.loadComposer(conversation: "chat").attachmentIDs, ["old"])

        let prepared = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
            expectedRevision: store.intentRevision(conversation: "chat"))
        try prepared.commit(draft: "Latest typed text")
        var restarted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restarted.draft, "Latest typed text")
        XCTAssertEqual(restarted.attachmentIDs, ["old", "new"])
        XCTAssertEqual(restarted.stagedFiles?.map(\.data), [Data("old".utf8), Data("new".utf8)])
        let overlayKey = "composer-draft-v1:chat"
        let transition = try XCTUnwrap(store.loadIntent(conversation: overlayKey))
        try restarted.begin(device: "phone", clientMessageID: "saved-send")
        try store.saveComposer(restarted, conversation: "chat")
        // A process may stop before best-effort overlay cleanup finishes.
        try store.saveIntent(transition, conversation: overlayKey)
        let afterSend = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(afterSend.pending?.request.clientMessageId, "saved-send")
        XCTAssertEqual(afterSend.pending?.request.body, "Latest typed text")
        XCTAssertEqual(afterSend.pending?.request.attachmentIds, ["uploaded-old", "uploaded-new"])
        XCTAssertEqual(afterSend.draft, "")
        XCTAssertEqual(afterSend.attachmentCount, 0)
    }

    @MainActor func testMainActorHeartbeatWhileComposerWorkerIsHeldAndCancellationPreservesDraft() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        var original = ComposerIntent(); original.draft = "Keep this"
        original.stagedFiles = [try StagedFile(name: "old.txt", mimeType: "text/plain", data: Data("original bytes".utf8))]
        try store.saveComposer(original, conversation: "chat")
        let before = try store.loadIntent(conversation: "chat")
        let worker = ComposerPersistenceWorker()
        let gate = DispatchSemaphore(value: 0)
        defer { gate.signal() }
        let started = expectation(description: "Worker held off main")
        let hold = Task { await worker.holdPersistenceForTest(gate, started: started) }
        await fulfillment(of: [started], timeout: 5)
        var completed = false
        let preparing = Task {
            defer { completed = true }
            return try await worker.prepare(original, store: store, conversation: "chat",
                expectedRevision: store.intentRevision(conversation: "chat"))
        }
        var heartbeat = 0
        await Task { @MainActor in heartbeat += 1 }.value
        XCTAssertEqual(heartbeat, 1)
        XCTAssertFalse(completed, "Persistence is still held when the main actor services the heartbeat")
        preparing.cancel()
        gate.signal()
        await hold.value
        do { _ = try await preparing.value; XCTFail("Cancelled preparation succeeded") }
        catch { XCTAssertTrue(error is CancellationError) }
        XCTAssertEqual(try store.loadIntent(conversation: "chat"), before)
        XCTAssertEqual(try store.loadComposer(conversation: "chat").stagedFiles?.first?.data, original.stagedFiles?.first?.data)
    }

    @MainActor func testCancellationAfterPreparationCannotPublishAndWriteFailureKeepsPriorBytes() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        var original = ComposerIntent(); original.draft = "Keep this"
        try store.saveComposer(original, conversation: "chat")
        try store.saveComposerDraft("Overlay stays", conversation: "chat")
        let before = try store.loadIntent(conversation: "chat")
        var candidate = original; candidate.draft = "Never publish"
        let prepared = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
            expectedRevision: store.intentRevision(conversation: "chat"))
        let cancelled = await Task { @MainActor in
            withUnsafeCurrentTask { $0?.cancel() }
            do { try prepared.commit(); return false }
            catch { return error is CancellationError }
        }.value
        XCTAssertTrue(cancelled)
        XCTAssertEqual(try store.loadIntent(conversation: "chat"), before)

        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: store.directory.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path) }
        XCTAssertThrowsError(try prepared.commit(), "A failed atomic rename must preserve both the prior intent and overlay")
        do {
            _ = try await ComposerPersistenceWorker.shared.prepare(candidate, store: store, conversation: "chat",
                expectedRevision: store.intentRevision(conversation: "chat"))
            XCTFail("A read-only directory accepted a prepared write")
        } catch { XCTAssertFalse(error is CancellationError) }
        XCTAssertEqual(try store.loadIntent(conversation: "chat"), before)
        XCTAssertEqual(try store.loadComposer(conversation: "chat").draft, "Overlay stays")
    }

    func testRejectedStaleAnnotationRestoresBodyAndUploadedAttachmentAfterRestart() throws {
        let source = Data("one\ntwo\n".utf8)
        let annotation = try ArtifactAnnotation(projectId: "project", conversationId: "chat",
            rootId: "workspace", path: "Plan.md", source: source,
            startLine: 2, endLine: 2, note: "Check this line")
        var staged = try annotation.stagedFile()
        staged.uploaded = ConversationFile(id: "uploaded-annotation", name: staged.name,
            mimeType: staged.mimeType, byteSize: staged.data.count,
            sha256: ConversationFile.digest(staged.data), state: "available", updatedAt: "now")
        var intent = ComposerIntent()
        intent.draft = "Please review this"
        intent.stagedFiles = [staged]
        try intent.begin(device: "phone", clientMessageID: "stale-send")
        XCTAssertEqual(intent.pending?.request.attachmentIds, ["uploaded-annotation"])
        intent.markRejected()
        try intent.restoreRejected()
        XCTAssertEqual(intent.draft, "Please review this")
        XCTAssertEqual(intent.draftAttachmentIds, ["uploaded-annotation"])
        XCTAssertNil(intent.pending)
        XCTAssertEqual(source, Data("one\ntwo\n".utf8))

        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        try store.saveComposer(intent, conversation: "chat")
        let restarted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restarted.draft, intent.draft)
        XCTAssertEqual(restarted.draftAttachmentIds, ["uploaded-annotation"])
    }
    func receipt(_ intent: ComposerIntent, conversation: String = "chat", state: String = "accepted_by_wonder") -> SendReceipt {
        let request = intent.pending!.request
        return SendReceipt(clientMessageId: request.clientMessageId, wonderMessageId: "server-id",
            bodySha256: SHA256.hash(data: Data(request.body.utf8)).map { String(format: "%02x", $0) }.joined(),
            conversationId: conversation, deliveryState: state)
    }

    func testLostResponseAndRestartReuseExactRequestWhileNewDraftSurvives() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "Mac", device: "phone")
        var intent = ComposerIntent(); intent.draft = "  Hello 🌍\nKeep my whitespace.  "
        try store.saveComposer(intent, conversation: "chat")
        XCTAssertEqual(try store.loadComposer(conversation: "chat").draft, intent.draft)
        let creationID = UUID().uuidString
        try intent.begin(device: "phone", clientMessageID: creationID)
        XCTAssertEqual(intent.pending?.request.clientMessageId, creationID)
        let original = intent.pending!.request
        try store.saveComposer(intent, conversation: "chat")
        // The Mac accepts, but the response is lost and iOS exits.
        var recovered = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(recovered.pending?.request.clientMessageId, original.clientMessageId)
        XCTAssertEqual(recovered.pending?.request.body, original.body)
        recovered.draft = "A different follow-up"
        XCTAssertThrowsError(try recovered.begin(device: "phone"))
        XCTAssertEqual(recovered.draft, "A different follow-up")
        try recovered.accept(receipt(intent), conversation: "chat")
        try store.saveComposer(recovered, conversation: "chat")
        try store.save(ProjectionState())
        let restarted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restarted.pending?.receipt?.wonderMessageId, "server-id")
        XCTAssertEqual(restarted.draft, "A different follow-up")
        XCTAssertNil(try ReadStore(root: root, host: "other", device: "phone").loadComposer(conversation: "chat").pending)
    }

    func testWrongReceiptCannotClearIntentAndSnapshotMustIncludeMessage() throws {
        var intent = ComposerIntent(); intent.draft = "hello"
        try intent.begin(device: "phone")
        XCTAssertThrowsError(try intent.accept(receipt(intent, conversation: "other"), conversation: "chat"))
        XCTAssertNil(intent.pending?.receipt)
        try intent.accept(receipt(intent, state: "uncertain"), conversation: "chat")
        let empty = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
            messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true))
        intent.reconcile(empty)
        XCTAssertNotNil(intent.pending)
        let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [ConversationMessage(messageId: "server-id", body: "hello", state: "uncertain", createdAt: "1000", attachmentIds: [])],
            assistantMessages: [], thread: empty.thread)
        intent.reconcile(snapshot)
        XCTAssertNil(intent.pending)
        XCTAssertEqual(snapshot.messages.first?.state, "uncertain")
    }

    func testUTF8LimitsAndWhitespace() throws {
        var intent = ComposerIntent(); intent.draft = " \n "
        XCTAssertThrowsError(try intent.begin(device: "phone"))
        intent.draft = String(repeating: "🌍", count: 16385)
        XCTAssertThrowsError(try intent.begin(device: "phone"))
        XCTAssertFalse(intent.draft.isEmpty)
        intent.draft = String(repeating: "🌍", count: 16384)
        try intent.begin(device: "phone")
        XCTAssertEqual(intent.pending?.request.body.utf8.count, 65536)
    }

    func testSnapshotRecoversLostReceiptAndKeepsLocalRowIdentity() throws {
        var intent = ComposerIntent(); intent.draft = "One request"
        try intent.begin(device: "phone")
        let client = intent.pending!.request.clientMessageId
        intent.draft = "Next draft"
        let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [ConversationMessage(clientMessageId: client, messageId: "server-id", body: "One request", state: "streaming", createdAt: "1000", attachmentIds: [])],
            assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true,
                turns: [ReadTurn(id: "turn", items: [ReadItem(id: "server-id", type: "userMessage", state: "streaming", text: "One request", createdAt: "1000")])]))
        intent.reconcile(snapshot)
        XCTAssertNil(intent.pending)
        XCTAssertEqual(intent.draft, "Next draft")
        XCTAssertEqual(snapshot.rows(author: "Ada").map(\.id), ["user-" + client])
    }

    func testLiveTextOverridesStaleThreadAndAcceptedMessageIsVisible() throws {
        let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 2,
            messages: [ConversationMessage(messageId: "new", body: "New request", state: "accepted_by_wonder", createdAt: "3000", attachmentIds: [])],
            assistantMessages: [AssistantMessage(itemId: "item", messageId: "assistant:turn:item", text: "Hello world", state: "streaming", createdAt: "2000", updatedAt: "3000")],
            thread: ThreadProjection(nextCursor: nil, hydrated: true, turns: [ReadTurn(id: "turn", items: [ReadItem(id: "item", type: "agentMessage", state: "streaming", text: "Hello", createdAt: "2000")])]))
        let rows = snapshot.rows(author: "Ada")
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows.first?.text, "Hello world")
        XCTAssertEqual(rows.first?.id, "turn/item")
        XCTAssertEqual(rows.last?.id, "user-new")
    }
    func testLostReceiptReconcilesAfterAnotherDeviceEditsAcceptedQueueItem() throws {
        var intent = ComposerIntent(); intent.draft = "Original"
        try intent.begin(device: "phone")
        let id = intent.pending!.request.clientMessageId
        let snapshot = ConversationSnapshot(conversationId: "chat",hostEpoch:"host",lastSequence:3,
            messages:[ConversationMessage(clientMessageId:id,originalBodySha256:ConversationFile.digest(Data("Original".utf8)),messageId:"server",body:"Edited on Mac",state:"accepted_by_wonder",createdAt:"1000",attachmentIds:[])],assistantMessages:[],thread:ThreadProjection(nextCursor:nil,hydrated:true))
        intent.reconcile(snapshot)
        XCTAssertNil(intent.pending)
    }

    func testRemovingOneRestoredAttachmentPersistsOtherAttachment() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "Mac", device: "phone")
        var intent = ComposerIntent()
        intent.draft = "Review these files"
        intent.draftAttachmentIds = ["server-photo", "missing-file"]
        let localData = Data("local".utf8)
        let uploaded = ConversationFile(id: "uploaded-local", name: "local.txt", mimeType: "text/plain",
            byteSize: localData.count, sha256: ConversationFile.digest(localData), state: "available", updatedAt: "now")
        let local = try StagedFile(id: "local-file", name: "local.txt", mimeType: "text/plain", data: localData, uploaded: uploaded)
        intent.stagedFiles = [local]
        try store.saveComposer(intent, conversation: "chat")

        var restored = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restored.attachmentIDs, [local.id, "server-photo", "missing-file"])
        XCTAssertEqual(restored.stagedFiles?.first?.uploaded?.id, "uploaded-local")
        restored.removeAttachment(id: "server-photo")
        try store.saveComposer(restored, conversation: "chat")

        let persisted = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(persisted.attachmentIDs, [local.id, "missing-file"])
        XCTAssertEqual(persisted.draft, "Review these files")
    }

    func testAttachmentIdentityIsUniqueAndTheFourthIsAllowedButFifthIsRejected() throws {
        var intent = ComposerIntent()
        intent.draftAttachmentIds = ["one", "two", "three", "four"]
        XCTAssertEqual(intent.attachmentIDs, ["one", "two", "three", "four"])
        XCTAssertEqual(intent.attachmentCount, 4)
        try intent.begin(device: "phone")
        XCTAssertEqual(intent.pending?.request.attachmentIds, ["one", "two", "three", "four"])

        var tooMany = ComposerIntent()
        tooMany.draftAttachmentIds = ["one", "two", "three", "four", "five"]
        XCTAssertEqual(tooMany.attachmentCount, 5)
        XCTAssertThrowsError(try tooMany.begin(device: "phone")) { error in
            XCTAssertTrue(error is FileFailure)
        }
        XCTAssertNil(tooMany.pending)
    }

    func testUploadedStagedAndRestoredAttachmentsKeepTheirDistinctSendIdentities() throws {
        let bytes = Data([255, 216, 255])
        let uploaded = ConversationFile(id: "server-photo", name: "photo.jpg", mimeType: "image/jpeg",
            byteSize: bytes.count, sha256: ConversationFile.digest(bytes), state: "available", updatedAt: "now")
        let staged = try StagedFile(id: "local-photo", name: "photo.jpg", mimeType: "image/jpeg", data: bytes, uploaded: uploaded)
        var intent = ComposerIntent()
        intent.draft = "Review both"
        intent.stagedFiles = [staged]
        intent.draftAttachmentIds = ["restored-document"]
        XCTAssertEqual(intent.attachmentIDs, ["local-photo", "restored-document"])
        XCTAssertEqual(intent.attachmentCount, 2)

        try intent.begin(device: "phone")
        XCTAssertEqual(intent.pending?.request.attachmentIds, ["server-photo", "restored-document"])
    }

    /// Removing a sketch or file uploaded by an earlier, unsent attempt asks
    /// the Mac to forget its copy, durably until it answers; a file never
    /// uploaded, or attached again since, is not forgotten.
    func testRemovingAnUploadedAttachmentQueuesItsCopyOnTheMacUntilForgotten() throws {
        let bytes = Data([137, 80, 78, 71, 13, 10, 26, 10])
        let uploaded = ConversationFile(id: "server-sketch", name: "Sketch.png", mimeType: "image/png",
            byteSize: bytes.count, sha256: ConversationFile.digest(bytes), state: "available", updatedAt: "now")
        var intent = ComposerIntent()
        intent.stagedFiles = [try StagedFile(id: "sketch", name: "Sketch.png", mimeType: "image/png", data: bytes, uploaded: uploaded),
                              try StagedFile(id: "local", name: "Local.png", mimeType: "image/png", data: bytes)]
        intent.draftAttachmentIds = ["restored", "kept"]
        intent.removeAttachment(id: "local")
        XCTAssertEqual(intent.uploadsToForget(), [], "a file only on the phone has no copy to forget")
        intent.removeAttachment(id: "sketch")
        intent.removeAttachment(id: "restored")
        intent.removeAttachment(id: "restored")
        XCTAssertEqual(intent.attachmentIDs, ["kept"])

        let reloaded = try JSONDecoder().decode(ComposerIntent.self, from: JSONEncoder().encode(intent))
        var relaunched = reloaded
        XCTAssertEqual(relaunched.uploadsToForget(), ["server-sketch", "restored"], "survives a relaunch")
        relaunched.draftAttachmentIds = ["kept", "restored"]
        XCTAssertEqual(relaunched.uploadsToForget(), ["server-sketch"], "attached again: the Mac keeps it")
        relaunched.forgotUpload("server-sketch")
        XCTAssertEqual(relaunched.uploadsToForget(), [])
        XCTAssertNil(relaunched.removedUploadIds)
    }

    func testAcceptedPendingAttachmentSnapshotStaysImmutableWhileFollowUpDraftChanges() throws {
        var intent = ComposerIntent()
        intent.draft = "Send these photos"
        intent.draftAttachmentIds = ["photo-1", "photo-2"]
        try intent.begin(device: "phone")
        let pendingID = try XCTUnwrap(intent.pending?.request.clientMessageId)
        XCTAssertEqual(intent.pending?.request.attachmentIds, ["photo-1", "photo-2"])
        XCTAssertEqual(intent.attachmentCount, 0)

        intent.draft = "Follow up"
        intent.draftAttachmentIds = ["photo-3"]
        intent.removeAttachment(id: "photo-1")
        XCTAssertEqual(intent.pending?.request.clientMessageId, pendingID)
        XCTAssertEqual(intent.pending?.request.attachmentIds, ["photo-1", "photo-2"])
        XCTAssertEqual(intent.attachmentIDs, ["photo-3"])

        let snapshot = ConversationSnapshot(conversationId: "chat", hostEpoch: "epoch", lastSequence: 1,
            messages: [ConversationMessage(clientMessageId: pendingID, messageId: "server-id", body: "Send these photos",
                state: "accepted_by_wonder", createdAt: "1000", attachmentIds: ["photo-1", "photo-2"])],
            assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true))
        intent.reconcile(snapshot)
        XCTAssertNil(intent.pending)
        XCTAssertEqual(intent.draft, "Follow up")
        XCTAssertEqual(intent.attachmentIDs, ["photo-3"])
    }

    func testAttachmentOnlyProjectRetryRetainsUploadAndMessageIdentityAcrossRestart() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let bytes = Data("Bounded Project attachment".utf8)
        var staged = try StagedFile(name: "notes.txt", mimeType: "text/plain", data: bytes)
        var draft = ComposerIntent(); draft.stagedFiles = [staged]
        try store.saveComposer(draft, conversation: "project-thread")
        var restored = try store.loadComposer(conversation: "project-thread")
        let restoredUpload = try JSONSerialization.jsonObject(with: XCTUnwrap(restored.stagedFiles?.first).uploadBody()) as? [String: String]
        let originalUpload = try JSONSerialization.jsonObject(with: staged.uploadBody()) as? [String: String]
        XCTAssertEqual(restoredUpload, originalUpload)
        XCTAssertThrowsError(try restored.begin(device: "phone"))
        staged.uploaded = ConversationFile(id: "canonical-file", name: "notes.txt", mimeType: "text/plain", byteSize: bytes.count, sha256: ConversationFile.digest(bytes), state: "available", updatedAt: "1000")
        try staged.uploaded?.verify(bytes, mime: staged.mimeType)
        restored.stagedFiles = [staged]
        try restored.begin(device: "phone")
        let request = restored.pending!.request
        XCTAssertEqual(request.body, "")
        XCTAssertEqual(request.attachmentIds, ["canonical-file"])
        try store.saveComposer(restored, conversation: "project-thread")
        var retry = try store.loadComposer(conversation: "project-thread")
        XCTAssertEqual(retry.pending?.request.clientMessageId, request.clientMessageId)
        XCTAssertEqual(retry.pending?.request.attachmentIds, request.attachmentIds)
        XCTAssertNil(try ReadStore(root: root, host: "other-mac", device: "phone").loadComposer(conversation: "project-thread").pending)
        XCTAssertNil(try ReadStore(root: root, host: "mac", device: "other-phone").loadComposer(conversation: "project-thread").pending)
        retry.draft = "Next message remains here"
        let snapshot = ConversationSnapshot(conversationId: "project-thread", hostEpoch: "epoch", lastSequence: 1,
            messages: [ConversationMessage(clientMessageId: request.clientMessageId, messageId: "canonical-message", body: "",
                state: "completed", createdAt: "1000", attachmentIds: ["canonical-file"])],
            assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true))
        retry.reconcile(snapshot)
        XCTAssertNil(retry.pending)
        XCTAssertEqual(retry.draft, "Next message remains here")
    }
}

extension SendIntentTests {
    func testTypedCodexSkillsSurviveFirstDraftSendGuideAndRejectedRestart() throws {
        let skill = CodexSkillSelection(name: "review", path: "/native/review/SKILL.md", projectId: "project", folderId: "folder", rootsRevision: 3)
        var draft = NewChatDraft(destination: .project(id: "project"), text: "Please $review ", family: .codex)
        draft.codexSkills = [skill]; draft.freeze()
        let restoredDraft = try JSONDecoder().decode(NewChatDraft.self, from: JSONEncoder().encode(draft))
        XCTAssertEqual(restoredDraft.codexSkills, [skill])
        XCTAssertEqual(restoredDraft.submittedBody, "Please $review ")
        for turn in [nil, "active-turn"] {
            var intent = ComposerIntent(); intent.draft = restoredDraft.text; intent.codexSkills = restoredDraft.codexSkills
            try intent.begin(device: "phone", clientMessageID: "request", expectedTurnId: turn)
            XCTAssertNil(intent.codexSkills)
            var restarted = try JSONDecoder().decode(ComposerIntent.self, from: JSONEncoder().encode(intent))
            XCTAssertEqual(restarted.pending?.request.codexSkills, [skill])
            XCTAssertEqual(restarted.pending?.request.expectedTurnId, turn)
            restarted.markRejected(); try restarted.restoreRejected()
            XCTAssertEqual(restarted.codexSkills, [skill]); XCTAssertEqual(restarted.draft, restoredDraft.text)
            restarted.draft = "Please $reviewer"
            XCTAssertNil(restarted.codexSkills)
            restarted.draft = "Please $review"
            XCTAssertNil(restarted.codexSkills, "Typing a token must not recreate an explicit selection")
        }
        var changed = restoredDraft; changed.rejectSubmission(); changed.chooseFamily(.claude, capabilities: .published(.claude))
        XCTAssertEqual(changed.codexSkills, [skill], "Retain the selection for visible review; never silently convert its input")
        XCTAssertNil(NewChatDraft.switching(from: changed, toSaved: nil).codexSkills)
    }
    func testSameNameSkillPathsRemainDistinctAndTokensRequireBoundaries() {
        let a = CodexSkillSelection(name: "review", path: "/a/SKILL.md", projectId: "p", folderId: "f", rootsRevision: 1)
        let b = CodexSkillSelection(name: "review", path: "/b/SKILL.md", projectId: "p", folderId: "f", rootsRevision: 1)
        var text = "$review ", skills: [CodexSkillSelection]? = [a, b]
        CodexSkillSelection.remove(path: a.path, text: &text, skills: &skills)
        XCTAssertEqual(text, "$review "); XCTAssertEqual(skills, [b])
        CodexSkillSelection.remove(path: b.path, text: &text, skills: &skills)
        XCTAssertEqual(text, " "); XCTAssertNil(skills)
        for value in ["$review", "Use $review.", "($review)", "$review\nnext"] { XCTAssertTrue(a.isPresent(in: value), value) }
        for value in ["$reviewer", "$review-extra", "x$review", "$$review", "$review_2", "$reviewé"] { XCTAssertFalse(a.isPresent(in: value), value) }
    }

    // Contract: work kept from a vanished chat can never send on its own (a
    // pending send is refused, the outbox stops), keeps its unsent identities
    // and bytes, and round-trips through its own store file.
    func testOrphanedDraftFencesSendsAndKeepsItsWork() throws {
        XCTAssertFalse(ComposerIntent().holdsUnsentWork)
        var blank = ComposerIntent(); blank.draft = " \n"
        XCTAssertFalse(blank.holdsUnsentWork)

        var intent = ComposerIntent()
        intent.draft = "Sent before it vanished"
        try intent.begin(device: "phone", clientMessageID: "pending-send")
        intent.draft = "Queued offline"
        intent.stagedFiles = [try StagedFile(id: "note", name: "Note.txt", mimeType: "text/plain", data: Data("Note".utf8))]
        intent.draftAttachmentIds = ["on-the-mac"]
        try intent.enqueue(device: "phone", clientMessageID: "queued")
        intent.draft = "Still typing"
        XCTAssertTrue(intent.holdsUnsentWork)

        let kept = OrphanedDraft(conversationId: "gone", title: "Gone", intent: intent, reason: "No longer on your computer.")
        XCTAssertEqual(kept.intent.pending?.rejected, true, "The pending send is refused, not retried")
        XCTAssertEqual(kept.intent.pending?.request.clientMessageId, "pending-send")
        XCTAssertEqual(kept.intent.outbox?.map(\.id), ["queued"])
        XCTAssertEqual(kept.intent.outbox?.first?.refusal, "No longer on your computer.")
        XCTAssertNil(kept.intent.nextOutboxMessage(device: "phone"))
        XCTAssertEqual(kept.text, "Sent before it vanished\n\nQueued offline\n\nStill typing")
        XCTAssertEqual(kept.localFiles.map(\.data), [Data("Note".utf8)])
        XCTAssertNotEqual(kept.localFiles.first?.id, "note", "Moved files upload as new attachments")
        XCTAssertEqual(kept.computerOnlyFileCount, 1)

        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "Mac", device: "phone")
        XCTAssertEqual(try store.loadOrphanedDrafts().count, 0)
        try store.saveOrphanedDrafts([kept])
        let loaded = try store.loadOrphanedDrafts()
        XCTAssertEqual(loaded.map(\.id), [kept.id])
        XCTAssertEqual(loaded.first?.text, kept.text)
        XCTAssertEqual(loaded.first?.intent.outbox?.first?.refusal, "No longer on your computer.")
        try store.saveOrphanedDrafts([])
        XCTAssertEqual(try store.loadOrphanedDrafts().count, 0)

        // With nothing typed after them, the pending send keeps its identity and its place.
        var quiet = ComposerIntent()
        quiet.draft = "Pending first"
        try quiet.begin(device: "phone", clientMessageID: "pending-quiet")
        quiet.draft = "Queued second"
        try quiet.enqueue(device: "phone", clientMessageID: "queued-quiet")
        let fenced = OrphanedDraft(conversationId: "gone", title: "Gone", intent: quiet, reason: "Gone.")
        XCTAssertEqual(fenced.intent.pending?.request.clientMessageId, "pending-quiet")
        XCTAssertEqual(fenced.intent.pending?.rejected, true)
        XCTAssertEqual(fenced.intent.draft, "")
        XCTAssertEqual(fenced.text, "Pending first\n\nQueued second")
    }

    // Contract: messages written while the Mac is unreachable wait in the
    // thread's outbox, survive a relaunch in order with their attachment
    // bytes, and become the pending send one at a time under their own
    // client message IDs, each only after the previous one was accepted.
    func testOutboxKeepsOrderAcrossRelaunchAndSendsOneAtATime() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "Mac", device: "phone")
        var intent = ComposerIntent()
        intent.draft = "First"
        try intent.enqueue(device: "phone", clientMessageID: "one")
        intent.draft = "Second"
        intent.stagedFiles = [try StagedFile(id: "file", name: "note.txt", mimeType: "text/plain", data: Data("keep".utf8))]
        try intent.enqueue(device: "phone", clientMessageID: "two")
        XCTAssertEqual(intent.draft, "")
        XCTAssertNil(intent.stagedFiles)
        try store.saveComposer(intent, conversation: "chat")

        var relaunched = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(relaunched.outbox?.map(\.id), ["one", "two"])
        XCTAssertEqual(relaunched.outbox?.last?.stagedFiles?.first?.data, Data("keep".utf8))
        let first = try relaunched.promoteOutbox(device: "phone")
        XCTAssertEqual(first.request.clientMessageId, "one")
        XCTAssertEqual(first.request.body, "First")
        XCTAssertEqual(first.fromOutbox, true)
        XCTAssertNil(relaunched.nextOutboxMessage(device: "phone"), "The next message waits for this one")
        XCTAssertThrowsError(try relaunched.promoteOutbox(device: "phone"))
        try relaunched.accept(receipt(relaunched), conversation: "chat")
        relaunched.reconcile(try JSONDecoder().decode(ConversationSnapshot.self, from: Data(#"{"conversationId":"chat","hostEpoch":"epoch","lastSequence":2,"messages":[{"messageId":"server-id","clientMessageId":"one","body":"First","state":"accepted_by_wonder","createdAt":"1","attachmentIds":[]}],"assistantMessages":[],"thread":{"hydrated":true,"turns":[]}}"#.utf8)))
        XCTAssertNil(relaunched.pending)
        XCTAssertThrowsError(try relaunched.promoteOutbox(device: "phone"), "Its attachment uploads first")
        relaunched.noteOutboxUpload(message: "two", file: "file", uploaded: ConversationFile(id: "uploaded", name: "note.txt",
            mimeType: "text/plain", byteSize: 4, sha256: ConversationFile.digest(Data("keep".utf8)), state: "available", updatedAt: "now"))
        try store.saveComposer(relaunched, conversation: "chat")
        relaunched = try store.loadComposer(conversation: "chat")
        let second = try relaunched.promoteOutbox(device: "phone")
        XCTAssertEqual(second.request.clientMessageId, "two")
        XCTAssertEqual(second.request.attachmentIds, ["uploaded"])
        XCTAssertNil(relaunched.outbox)
    }

    // Contract: a refused outbox message goes back to the front of its
    // outbox with the reason, never into the draft being typed, and nothing
    // behind it is sent until Retry; Edit needs an empty composer.
    func testRefusedOutboxMessageHoldsItsThreadUntilRetryEditOrCancel() throws {
        var intent = ComposerIntent()
        intent.draft = "Archived?"
        try intent.enqueue(device: "phone", clientMessageID: "one")
        intent.draft = "Behind it"
        try intent.enqueue(device: "phone", clientMessageID: "two")
        try intent.promoteOutbox(device: "phone")
        intent.draft = "Typing meanwhile"
        intent.refuse(reason: "This thread is archived.")
        XCTAssertNil(intent.pending)
        XCTAssertEqual(intent.draft, "Typing meanwhile")
        XCTAssertEqual(intent.outbox?.map(\.id), ["one", "two"])
        XCTAssertEqual(intent.outbox?.first?.refusal, "This thread is archived.")
        XCTAssertNil(intent.nextOutboxMessage(device: "phone"), "Nothing overtakes a refused message")
        try intent.retryOutbox(id: "one")
        XCTAssertEqual(intent.nextOutboxMessage(device: "phone")?.id, "one")
        XCTAssertThrowsError(try intent.editOutbox(id: "one")) { XCTAssertEqual($0 as? Outbox.Failure, .draftInUse) }
        intent.draft = ""
        try intent.editOutbox(id: "one")
        XCTAssertEqual(intent.draft, "Archived?")
        XCTAssertEqual(intent.removeFromOutbox(id: "two")?.body, "Behind it")
        XCTAssertNil(intent.outbox)

        var composer = ComposerIntent()
        composer.draft = "Sent online"
        try composer.begin(device: "phone")
        composer.refuse(reason: "Refused")
        XCTAssertEqual(composer.draft, "Sent online", "A composer send still returns to an empty draft")
        XCTAssertNil(composer.outbox)
    }

    // Contract: a message belongs to the pairing that wrote it. A later
    // pairing never sends it (thread or New chat) but can still read it.
    func testAnotherPairingNeverSendsAWaitingMessage() throws {
        var intent = ComposerIntent()
        intent.draft = "Written before pairing again"
        try intent.enqueue(device: "old-phone", clientMessageID: "old")
        XCTAssertNil(intent.nextOutboxMessage(device: "new-phone"))
        XCTAssertThrowsError(try intent.promoteOutbox(device: "new-phone"))
        XCTAssertEqual(intent.outboxFromPreviousPairing(device: "new-phone").map(\.id), ["old"])
        intent.draft = "Written now"
        try intent.enqueue(device: "new-phone", clientMessageID: "new")
        XCTAssertEqual(try intent.promoteOutbox(device: "new-phone").request.clientMessageId, "new")
        XCTAssertEqual(intent.outbox?.map(\.id), ["old"])

        var draft = NewChatDraft(destination: .project(id: "project"), text: "Hello")
        draft.queue(device: "old-phone")
        XCTAssertTrue(draft.isQueued)
        XCTAssertEqual(draft.submittedDeviceID, "old-phone")
        XCTAssertEqual(draft.submittedBody, "Hello")
        XCTAssertTrue(draft.canEditQueued)
        draft.outboxAttempted = true
        XCTAssertFalse(draft.canEditQueued, "An unanswered creation keeps its request ID")
        draft.refuseQueued("Refused", retryable: false)
        XCTAssertTrue(draft.canEditQueued)
        let request = draft.requestID
        draft.unqueue()
        XCTAssertFalse(draft.isQueued)
        XCTAssertNotEqual(draft.requestID, request)
        XCTAssertEqual(draft.text, "Hello")
        XCTAssertNil(draft.submittedDeviceID)
    }

    // Contract: at most ten messages wait per thread or as New chats; past
    // that bound Send refuses with a plain reason and keeps the draft.
    // Pin the user limit independently so changing the constant cannot grow it.
    func testOutboxRefusesPastItsBoundsAndKeepsTheDraft() throws {
        var intent = ComposerIntent()
        for index in 0..<10 {
            intent.draft = "Message \(index)"
            try intent.enqueue(device: "phone")
        }
        intent.draft = "One too many"
        XCTAssertThrowsError(try intent.enqueue(device: "phone")) { XCTAssertEqual($0 as? Outbox.Failure, .full) }
        XCTAssertEqual(intent.draft, "One too many")
        XCTAssertFalse(Outbox.explanation(.full).isEmpty)

        let large = Data(repeating: 65, count: 6 * 1024 * 1024)
        var heavy = ComposerIntent()
        heavy.draft = "Two files"
        heavy.stagedFiles = [try StagedFile(name: "a.txt", mimeType: "text/plain", data: large),
                             try StagedFile(name: "b.txt", mimeType: "text/plain", data: large)]
        try heavy.enqueue(device: "phone")
        heavy.draft = "Over the limit"
        heavy.stagedFiles = [try StagedFile(name: "c.txt", mimeType: "text/plain", data: large)]
        XCTAssertThrowsError(try heavy.enqueue(device: "phone")) { XCTAssertEqual($0 as? Outbox.Failure, .tooLarge) }
        XCTAssertEqual(heavy.stagedFiles?.count, 1)
        XCTAssertEqual(heavy.draft, "Over the limit")

        XCTAssertEqual(Outbox.newChatRefusal(adding: NewChatDraft(), to: Array(repeating: NewChatDraft(), count: 10)), .full)
        XCTAssertNil(Outbox.newChatRefusal(adding: NewChatDraft(), to: []))
    }

    // Contract: offline Send validates the same content bounds as online Send
    // before moving the draft. Empty, >64 KiB UTF-8 and five-file drafts stay
    // editable; exactly 64 KiB or four attachments (without text) can wait.
    func testOutboxValidatesContentBeforeMovingTheDraft() throws {
        var intent = ComposerIntent()
        intent.draft = " \n\t "
        XCTAssertThrowsError(try intent.enqueue(device: "phone")) {
            guard case SendFailure.empty = $0 else { return XCTFail("Expected empty, got \($0)") }
        }
        XCTAssertEqual(intent.draft, " \n\t ")
        XCTAssertNil(intent.outbox)

        let oversized = String(repeating: "🌍", count: 16385)
        intent.draft = oversized
        XCTAssertThrowsError(try intent.enqueue(device: "phone")) {
            guard case SendFailure.tooLarge = $0 else { return XCTFail("Expected tooLarge, got \($0)") }
        }
        XCTAssertEqual(intent.draft, oversized)
        XCTAssertNil(intent.outbox)
        intent.draft = String(repeating: "🌍", count: 16384)
        XCTAssertEqual(try intent.enqueue(device: "phone").body.utf8.count, 65536)

        intent.draftAttachmentIds = ["one", "two", "three", "four", "five"]
        XCTAssertThrowsError(try intent.enqueue(device: "phone")) {
            guard case FileFailure.tooLarge = $0 else { return XCTFail("Expected file tooLarge, got \($0)") }
        }
        XCTAssertEqual(intent.draftAttachmentIds, ["one", "two", "three", "four", "five"])
        XCTAssertEqual(intent.outbox?.count, 1)
        intent.draftAttachmentIds?.removeLast()
        let mediaOnly = try intent.enqueue(device: "phone")
        XCTAssertEqual(mediaOnly.body, "")
        XCTAssertEqual(mediaOnly.remoteAttachmentIds, ["one", "two", "three", "four"])
    }

    // Contract: waiting New chats share a 32 MiB attachment budget. Exactly
    // the budget is accepted; one more byte in another draft is refused.
    func testNewChatOutboxBoundsAttachmentBytesAcrossDrafts() throws {
        let data = Data(repeating: 65, count: 8 * 1024 * 1024)
        let queued = try (0..<4).map { index in
            var draft = NewChatDraft()
            draft.attachments = [NewChatAttachment(try StagedFile(id: "file-\(index)", name: "file.txt",
                mimeType: "text/plain", data: data))]
            draft.queue(device: "phone")
            return draft
        }
        XCTAssertNil(Outbox.newChatRefusal(adding: queued[3], to: Array(queued.prefix(3))))
        var extra = NewChatDraft()
        extra.attachments = [NewChatAttachment(try StagedFile(name: "extra.txt", mimeType: "text/plain", data: Data([65])))]
        XCTAssertEqual(Outbox.newChatRefusal(adding: extra, to: queued), .tooLarge)
    }

    // Contract: the Mac's answer about a message stops its thread; offline,
    // busy, sign-in and unanswered failures retry with bounded backoff.
    func testOutboxSeparatesRefusalsFromFailuresThatMayPass() {
        XCTAssertEqual(Outbox.outcome(of: PairingFailure.hostMessage(409, "The Mac said no.")), .refused("The Mac said no."))
        XCTAssertEqual(Outbox.outcome(of: PairingFailure.skillRejected(422, "Skill missing.")), .refused("Skill missing."))
        XCTAssertEqual(Outbox.outcome(of: PairingFailure.annotationRejected(422, "Source changed.")), .refused("Source changed."))
        if case .refused(let reason) = Outbox.outcome(of: PairingFailure.response(409)) { XCTAssertTrue(reason.contains("archived")) }
        else { XCTFail("409 refuses") }
        for code in [400, 403, 404, 409, 410, 412, 413, 422, 499] {
            guard case .refused = Outbox.outcome(of: PairingFailure.response(code)) else {
                XCTFail("\(code) refuses this message"); continue
            }
            XCTAssertEqual(Outbox.outcome(of: PairingFailure.hostMessage(code, "Reason \(code)")), .refused("Reason \(code)"))
        }
        if case .refused(let reason) = Outbox.outcome(of: PairingFailure.response(403)) {
            XCTAssertTrue(reason.contains("no longer accepts"), "403 explains that this device lost access")
        } else { XCTFail("403 refuses") }
        XCTAssertEqual(Outbox.outcome(of: PairingFailure.response(410)), Outbox.outcome(of: PairingFailure.response(404)),
            "Both a missing and a gone thread explain that it is no longer on the computer")
        for code in [401, 408, 425, 429, 500, 503, 599] {
            XCTAssertEqual(Outbox.outcome(of: PairingFailure.response(code)), .retryLater, "\(code)")
            XCTAssertEqual(Outbox.outcome(of: PairingFailure.hostMessage(code, "Temporary failure")), .retryLater, "\(code)")
        }
        for error: Error in [URLError(.notConnectedToInternet), URLError(.networkConnectionLost), CancellationError()] {
            XCTAssertEqual(Outbox.outcome(of: error), .retryLater, "\(error)")
        }
        XCTAssertEqual(NewChatDraft.queuedRefusal(PairingFailure.response(422))?.retryable, false,
            "An invalid project/model/access choice needs editing before New chat can retry")
        XCTAssertNil(NewChatDraft.queuedRefusal(PairingFailure.response(503)))
        XCTAssertEqual(Outbox.retryDelay(attempt: 0), .seconds(1), "An unset attempt still waits before retrying")
        XCTAssertEqual(Outbox.retryDelay(attempt: 1), .seconds(1))
        XCTAssertEqual(Outbox.retryDelay(attempt: 3), .seconds(4))
        XCTAssertEqual(Outbox.retryDelay(attempt: 9), .seconds(16))
    }
}

private extension ComposerPersistenceWorker {
    func measurePreparationForTest(_ intent: ComposerIntent, store: ReadStore, revision: String) throws
        -> (file: ReadStore.PreparedIntent, bytes: Int, encoding: TimeInterval, write: TimeInterval) {
        XCTAssertFalse(Thread.isMainThread)
        let start = Date()
        let data = try composerEncoder(store.stagedBodies(conversation: "chat")).encode(intent)
        let encodedAt = Date()
        let file = try store.prepareIntent(data, conversation: "chat", expectedRevision: revision)
        return (file, data.count, encodedAt.timeIntervalSince(start), Date().timeIntervalSince(encodedAt))
    }

    func holdPersistenceForTest(_ gate: DispatchSemaphore, started: XCTestExpectation) {
        XCTAssertFalse(Thread.isMainThread)
        started.fulfill()
        gate.wait()
    }
}
