import XCTest
@testable import WonderPairing
final class FileTests: XCTestCase {
    func testVideoLimitsRejectOversizedDiskFileBeforeReadingItsBody() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: url) }
        XCTAssertTrue(FileManager.default.createFile(atPath: url.path, contents: nil))
        let handle = try FileHandle(forWritingTo: url)
        try handle.truncate(atOffset: UInt64(AttachmentLimits.maximumVideoBytes + 1))
        try handle.close()
        let size = try XCTUnwrap(url.resourceValues(forKeys: [.fileSizeKey]).fileSize)
        XCTAssertThrowsError(try AttachmentLimits.validate(byteSize: size, mimeType: "video/mp4"))
        XCTAssertThrowsError(try StagedFile.importingVideo(url, name: "Too big.mp4", mimeType: "video/mp4")) {
            XCTAssertEqual($0 as? FileFailure, .tooLarge)
        }
        XCTAssertNoThrow(try AttachmentLimits.validate(byteSize: size - 1, mimeType: "video/mp4"))
        XCTAssertThrowsError(try AttachmentLimits.validate(byteSize: 8 * 1024 * 1024 + 1, mimeType: "image/png"))
        XCTAssertThrowsError(try AttachmentLimits.validate(byteSize: 100, mimeType: "video/unknown"))
        XCTAssertThrowsError(try AttachmentLimits.validate(byteSize: 0, mimeType: "video/mp4"))
    }

    func testComposerPreparationReclaimsOnlyOldTemporaryFilesAndKeepsDurableIntent() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        var intent = ComposerIntent(); intent.draft = "Durable message"
        try store.saveComposer(intent, conversation: "chat")
        let before = try store.loadIntent(conversation: "chat")
        let old = store.directory.appendingPathComponent(".prepared-intent-abandoned")
        let recent = store.directory.appendingPathComponent(".prepared-intent-recent")
        try Data("abandoned bytes".utf8).write(to: old)
        try Data("recent bytes".utf8).write(to: recent)
        try FileManager.default.setAttributes([.modificationDate: Date.distantPast], ofItemAtPath: old.path)
        let prepared = try store.prepareIntent(Data("candidate".utf8), conversation: "chat",
            expectedRevision: store.intentRevision(conversation: "chat"))
        withExtendedLifetime(prepared) {
            XCTAssertFalse(FileManager.default.fileExists(atPath: old.path))
            XCTAssertEqual(try? Data(contentsOf: recent), Data("recent bytes".utf8))
            XCTAssertEqual(try? store.loadIntent(conversation: "chat"), before)
        }
    }

    // Contract: separate old/new diff notes persist through the existing composer
    // owner with exact patch identity; malformed or partial ranges cannot be sent.
    func testDiffNotesKeepOldAndNewSidesThroughDurableComposer() throws {
        let patch = "diff --git a/a.swift b/a.swift\n--- a/a.swift\n+++ b/a.swift\n@@ -1,2 +1,3 @@\n keep\n-old\n+new\n+another\n"
        let source = try DiffAnnotationSource(patch: patch)
        XCTAssertEqual(source.lines.map(\.id), [5, 6, 7, 8])
        XCTAssertEqual(source.lines[1].oldLine, 2)
        XCTAssertNil(source.lines[1].newLine)
        XCTAssertEqual(source.lines[2].newLine, 2)
        XCTAssertNil(source.lines[2].oldLine)
        let removed = try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "workspace", path: "a.swift",
            patch: source, scope: .uncommitted, originalPath: nil, startLine: 6, endLine: 6, note: "Keep the old behavior")
        let added = try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "workspace", path: "a.swift",
            patch: source, scope: .uncommitted, originalPath: nil, startLine: 7, endLine: 8, note: "Explain these additions")
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var intent = ComposerIntent(); intent.draft = "Review changes"
        intent.stagedFiles = try [removed.stagedFile(), added.stagedFile()]
        try store.saveComposer(intent, conversation: "c")
        let recovered = try store.loadComposer(conversation: "c")
        let notes = try XCTUnwrap(recovered.stagedFiles).map { try ArtifactAnnotation.read($0.data) }
        XCTAssertEqual(notes, [removed, added])
        XCTAssertEqual(notes[0].sourceSha256, ConversationFile.digest(Data(patch.utf8)))
        XCTAssertEqual(try notes[1].replacingNote("A revised note").anchor, added.anchor)
        XCTAssertFalse(source.validSelection(4...6))
        XCTAssertFalse(source.validSelection(8...9))
        XCTAssertThrowsError(try DiffAnnotationSource(patch: patch.replacingOccurrences(of: "+another\n", with: "")))
        XCTAssertThrowsError(try DiffAnnotationSource(patch: "Binary files a/x and b/x differ\n"))
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "workspace", path: "a.swift",
            patch: source, scope: .branch, originalPath: "../private", startLine: 6, endLine: 6, note: "Review"))
        let oldHost = try JSONDecoder().decode(WorkspaceDiffResponse.self, from: Data("{\"path\":\"a\",\"staged\":false,\"diff\":\"\"}".utf8))
        XCTAssertNil(oldHost.annotationVersion)
    }

    func testTextPreviewAnnotationPersistsExactBindingAndEditableNoteWithoutChangingSource() throws {
        let source = Data("first\nsecond\nthird\n".utf8)
        let annotation = try ArtifactAnnotation(projectId: "project", conversationId: "chat",
            rootId: "workspace", path: "Sources/Plan.md", source: source,
            startLine: 2, endLine: 3, note: "Check this decision")
        let staged = try annotation.stagedFile()
        XCTAssertEqual(staged.mimeType, ArtifactAnnotation.mimeType)
        let decoded = try ArtifactAnnotation.read(staged.data)
        XCTAssertEqual(decoded.sourceSha256, ConversationFile.digest(source))
        XCTAssertEqual(decoded.anchor, .textLines(startLine: 2, endLine: 3))
        XCTAssertEqual(decoded.note, "Check this decision")
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: staged.data) as? [String: Any])
        XCTAssertEqual(json["version"] as? Int, 1)
        XCTAssertEqual(json["projectId"] as? String, "project")
        XCTAssertEqual(json["conversationId"] as? String, "chat")
        XCTAssertEqual(json["rootId"] as? String, "workspace")
        XCTAssertEqual(json["path"] as? String, "Sources/Plan.md")
        XCTAssertEqual((json["anchor"] as? [String: Any])?["kind"] as? String, "textLines")
        let edited = try decoded.replacingNote("A revised note")
        XCTAssertEqual(edited.sourceSha256, decoded.sourceSha256)
        XCTAssertEqual(edited.anchor, decoded.anchor)
        XCTAssertEqual(source, Data("first\nsecond\nthird\n".utf8))

        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        var intent = ComposerIntent()
        intent.draft = "Review this section"
        intent.stagedFiles = [staged]
        try store.saveComposer(intent, conversation: "chat")
        let restored = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(try ArtifactAnnotation.read(XCTUnwrap(restored.stagedFiles?.first?.data)), annotation)
    }

    func testTextPreviewAnnotationRejectsInvalidRegionPathAndNote() throws {
        let source = Data("one\ntwo".utf8)
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "../secret", source: source, startLine: 1, endLine: 1, note: "Review"))
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "a.txt", source: source, startLine: 2, endLine: 3, note: "Review"))
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "a.txt", source: source, startLine: 1, endLine: 1, note: "  "))
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "a.txt", source: Data([0xff]), startLine: 1, endLine: 1, note: "Review"))
    }

    func testSelectedTextRangeUsesExclusiveUTF8BytesAndRejectsBrokenSelections() throws {
        let source = Data("Aé🦊Z\n".utf8)
        let annotation = try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "workspace",
            path: "notes.md", source: source, mimeType: "text/markdown",
            startByte: 1, endByte: 7, note: "Explain this selection")
        XCTAssertEqual(annotation.sourceSha256, ConversationFile.digest(source))
        XCTAssertEqual(annotation.anchor, .textRange(startByte: 1, endByte: 7))
        let staged = try annotation.stagedFile()
        XCTAssertEqual(try ArtifactAnnotation.read(staged.data), annotation)
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: staged.data) as? [String: Any])
        let anchor = try XCTUnwrap(json["anchor"] as? [String: Any])
        XCTAssertEqual(anchor["kind"] as? String, "textRange")
        XCTAssertEqual(anchor["startByte"] as? Int, 1)
        XCTAssertEqual(anchor["endByte"] as? Int, 7)
        for (mime, start, end) in [("text/markdown", 2, 7), ("text/markdown", 1, 6),
                                   ("text/markdown", 7, 7), ("text/markdown", 1, 99),
                                   ("text/html", 1, 7), ("image/png", 1, 7)] {
            XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "workspace",
                path: "notes.md", source: source, mimeType: mime,
                startByte: start, endByte: end, note: "Review"))
        }
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "workspace",
            path: "notes.md", source: Data([0xff, 0xfe]), mimeType: "text/plain",
            startByte: 0, endByte: 2, note: "Review"))
    }

    func testImageAndPDFRegionsEncodeExactNormalizedAnchorsAndRejectInvalidBounds() throws {
        let image = Data([137, 80, 78, 71, 13, 10, 26, 10, 0])
        let imageNote = try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "images/diagram.png", source: image, imageX: 0.125, y: 0.25,
            width: 0.5, height: 0.5, note: "Inspect this diagram")
        XCTAssertEqual(try ArtifactAnnotation.read(imageNote.stagedFile().data).anchor,
                       .imageRegion(x: 0.125, y: 0.25, width: 0.5, height: 0.5))
        let imageJSON = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(imageNote)) as? [String: Any])
        XCTAssertEqual((imageJSON["anchor"] as? [String: Any])?["kind"] as? String, "imageRegion")
        XCTAssertEqual(imageNote.sourceSha256, ConversationFile.digest(image))

        let pdf = Data("%PDF-1.7\nfixture".utf8)
        let pdfNote = try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "documents/report.pdf", source: pdf, pdfPage: 2, x: 0.2, y: 0.1,
            width: 0.7, height: 0.8, note: "Review this chart")
        XCTAssertEqual(try ArtifactAnnotation.read(pdfNote.stagedFile().data).anchor,
                       .pdfRegion(page: 2, x: 0.2, y: 0.1, width: 0.7, height: 0.8))
        let pdfJSON = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(pdfNote)) as? [String: Any])
        XCTAssertEqual((pdfJSON["anchor"] as? [String: Any])?["kind"] as? String, "pdfRegion")
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "a.png", source: image, imageX: 0.8, y: 0.1, width: 0.3, height: 0.2, note: "Bad"))
        XCTAssertThrowsError(try ArtifactAnnotation(projectId: "p", conversationId: "c", rootId: "r",
            path: "a.pdf", source: pdf, pdfPage: 0, x: 0, y: 0, width: 1, height: 1, note: "Bad"))
    }

    func testStagedBytesSurviveCacheReplacementAndSendRequiresVerifiedUpload() throws {
        let data = Data("Actual text file 🌍".utf8)
        let staged = try StagedFile(name: "notes.txt", mimeType: "text/plain", data: data)
        var intent = ComposerIntent(); intent.stagedFiles = [staged]
        XCTAssertThrowsError(try intent.begin(device: "phone"))
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "host", device: "phone")
        try store.saveComposer(intent, conversation: "chat"); try store.save(ProjectionState())
        var restored = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restored.stagedFiles?.first?.data, data)
        let file = ConversationFile(id: "file", name: "notes.txt", mimeType: "text/plain", byteSize: data.count, sha256: ConversationFile.digest(data), state: "available", updatedAt: "now")
        try file.verify(data, mime: "text/plain")
        XCTAssertThrowsError(try file.verify(Data("corrupt".utf8), mime: "text/plain"))
        XCTAssertThrowsError(try file.verify(data, mime: "text/html"))
        restored.stagedFiles?[0].uploaded = file
        try restored.begin(device: "phone")
        XCTAssertEqual(restored.pending?.request.attachmentIds, ["file"])
        XCTAssertNil(restored.stagedFiles)

        // The file-provider callback can end before previews or uploads start.
        // The staged owner retains one disk source; restored drafts reuse their sidecar.
        let video = Data([0, 0, 0, 24] + Array("ftypmp42".utf8) + Array(repeating: UInt8(0), count: 12))
        let providerURL = root.appendingPathComponent("provider.mp4")
        try video.write(to: providerURL)
        var movie: StagedFile? = try StagedFile.importingVideo(providerURL, name: "Movie.mp4", mimeType: "video/mp4")
        let sourceURL = try XCTUnwrap(movie?.source?.url)
        try FileManager.default.removeItem(at: providerURL)
        XCTAssertEqual(try Data(contentsOf: sourceURL), video)
        intent.stagedFiles = [try XCTUnwrap(movie)]
        try store.saveComposer(intent, conversation: "video")
        var copy = movie
        movie = nil
        intent.stagedFiles = nil
        XCTAssertTrue(FileManager.default.fileExists(atPath: sourceURL.path))
        withExtendedLifetime(copy) {}
        copy = nil
        XCTAssertFalse(FileManager.default.fileExists(atPath: sourceURL.path))
        let recovered = try XCTUnwrap(store.loadComposer(conversation: "video").stagedFiles?.first)
        let recoveredURL = try XCTUnwrap(recovered.source?.url)
        XCTAssertEqual(recoveredURL.pathExtension, "body")
        XCTAssertEqual(try Data(contentsOf: recoveredURL), video)
    }

    func testLargeStagedDraftEditsStayDurableWithoutRewritingFileOrRestoringSentText() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "mac", device: "phone")
        let data = Data(repeating: 65, count: 1024 * 1024)
        let staged = try StagedFile(name: "large.txt", mimeType: "text/plain", data: data)
        var intent = ComposerIntent()
        intent.stagedFiles = [staged]
        try store.saveComposer(intent, conversation: "chat")
        let original = try XCTUnwrap(store.loadIntent(conversation: "chat"))

        try store.saveComposerDraft("First", conversation: "chat")
        try store.saveComposerDraft("Final edit", conversation: "chat")
        let oldOverlay = try XCTUnwrap(store.loadIntent(conversation: "composer-draft-v1:chat"))
        XCTAssertLessThan(oldOverlay.count, 1024)
        XCTAssertEqual(try store.loadIntent(conversation: "chat"), original)
        var restored = try store.loadComposer(conversation: "chat")
        XCTAssertEqual(restored.draft, "Final edit")
        XCTAssertEqual(restored.stagedFiles?.first?.data, data)

        let uploaded = ConversationFile(id: "uploaded", name: "large.txt", mimeType: "text/plain",
            byteSize: data.count, sha256: ConversationFile.digest(data), state: "available", updatedAt: "now")
        restored.stagedFiles?[0].uploaded = uploaded
        try restored.begin(device: "phone")
        try store.saveComposer(restored, conversation: "chat")
        // Simulate a crash before stale-overlay cleanup after the atomic intent write.
        try store.saveIntent(oldOverlay, conversation: "composer-draft-v1:chat")
        let sent = try store.loadComposer(conversation: "chat")
        XCTAssertTrue(sent.draft.isEmpty)
        XCTAssertEqual(sent.pending?.request.body, "Final edit")
        XCTAssertNil(sent.stagedFiles)

        try store.saveComposerDraft("Unsaved conversation", conversation: "new-chat")
        try store.removeComposer(conversation: "new-chat")
        XCTAssertTrue(try store.loadComposer(conversation: "new-chat").draft.isEmpty)
    }
    func testSpoofedMIMEAndOversizeAreRejected() throws {
        XCTAssertThrowsError(try StagedFile(name: "fake.pdf", mimeType: "application/pdf", data: Data("not a PDF".utf8)))
        XCTAssertThrowsError(try StagedFile(name: "big", mimeType: "application/octet-stream", data: Data(count: 8 * 1024 * 1024 + 1)))
    }

    func testPhotoSignaturesAreValidatedBeforeStaging() throws {
        let png = Data([137, 80, 78, 71, 13, 10, 26, 10, 0])
        let jpeg = Data([255, 216, 255, 224, 0])
        XCTAssertNoThrow(try StagedFile(name: "photo.png", mimeType: "image/png", data: png))
        XCTAssertNoThrow(try StagedFile(name: "photo.jpg", mimeType: "image/jpeg", data: jpeg))
        XCTAssertThrowsError(try StagedFile(name: "wrong.png", mimeType: "image/png", data: jpeg))
        XCTAssertThrowsError(try StagedFile(name: "wrong.jpg", mimeType: "image/jpeg", data: png))
    }

}
