import XCTest
@testable import WonderPairing

final class FileChangeSummaryTests: XCTestCase {
    private func item(_ payload: [String: ThreadValue], state: String = "completed") -> ReadItem {
        ReadItem(id: "edit", type: "fileChange", state: state, text: nil, createdAt: "1000", payload: payload)
    }

    func testLegacyDiffCountsIgnoreHeadersAndDoNotInventAnOutsideWorkspaceWarning() {
        let legacy = item(["diffs": .array([.object(["path": .string("[path outside workspace]"), "diff": .string("--- a/test.swift\n+++ b/test.swift\n@@ -1 +1 @@\n-old\n+new")])])])
        let summary = FileChangeSummary.prepare(legacy)
        XCTAssertEqual(summary?.title, "Edited files")
        XCTAssertEqual(summary?.additions, 1)
        XCTAssertEqual(summary?.deletions, 1)
        let unknown = FileChangeSummary.prepare(item([:]))
        XCTAssertNil(unknown?.additions)
        XCTAssertNil(unknown?.deletions)
    }

    func testLinksRetainRelativePathsAndRejectRedactedOrEscapingPaths() {
        for invalid in ["", "/workspace/File.swift", "../File.swift", "A/../File.swift", "A//File.swift", "./File.swift", "[path outside workspace]", "file\0.swift"] {
            XCTAssertNil(FileChangeSummary.relativePath(invalid))
            XCTAssertNil(FileChangeSummary.prepare(item(["paths": .array([.string(invalid)])]))?.path)
        }
        XCTAssertEqual(FileChangeSummary.relativePath("Sources/My File.swift"), "Sources/My File.swift")
        XCTAssertNil(FileChangeSummary.prepare(item(["paths": .array([.string("A.swift"), .string("B.swift")])]))?.path)
    }

    func testResponseFooterReadsMultiplePatchPathsWhenLegacyPathsAreEmpty() throws {
        let value = item(["paths": .array([]), "diffs": .array([
            .object(["path": .string("A.swift"), "diff": .string("--- a/A.swift\n+++ b/A.swift\n-old\n+new")]),
            .object(["path": .string("B.swift"), "diff": .string("+one\n+two")])])])
        let row = ReadRow(id: "saved", author: "Agent", text: "", isUser: false, timestamp: "1000", turnId: "turn", item: value)
        let files = try XCTUnwrap(ResponseEditedFiles.footers(entries: ChatFeedEntry.grouping([row]), activeTurnIDs: [])["saved"]).files
        XCTAssertEqual(files.map(\.path), ["A.swift", "B.swift"])
        XCTAssertEqual(files.map(\.additions), [1, 2])
        XCTAssertEqual(files.map(\.deletions), [1, 0])
    }

    func testChangedFilesNameEveryChangeEvenWhenTheEventHasNoUsablePath() {
        func change(_ path: String?, _ diff: String) -> ThreadValue {
            var fields: [String: ThreadValue] = ["diff": .string(diff)]
            if let path { fields["path"] = .string(path) }
            return .object(fields)
        }
        let files = ResponseEditedFiles.editedFiles(in: [
            "paths": .array([.string("[path outside workspace]"), .string("A.swift")]),
            "diffs": .array([
                change("A.swift", "--- a/A.swift\n+++ b/A.swift\n@@ -1 +1 @@\n-old\n+new"),
                // The host's placeholder and a missing field both fall back to the patch header.
                change("[path outside workspace]", "diff --git a/Docs/My notes.md b/Docs/My notes.md\n--- a/Docs/My notes.md\n+++ b/Docs/My notes.md\n@@ -1 +1,2 @@\n-a\n+b\n+c"),
                change(nil, "--- /dev/null\n+++ b/new/File.rs\n@@ -0,0 +1 @@\n+fn main() {}"),
                change(nil, "@@ -1 +1 @@\n-x\n+y")])
        ], singlePathCounts: nil)
        XCTAssertEqual(files.map(\.path), ["A.swift", "Docs/My notes.md", "new/File.rs", "File 4"])
        XCTAssertEqual(files.map(\.name), ["A.swift", "My notes.md", "File.rs", "File 4"])
        XCTAssertEqual(files.map(\.additions), [1, 2, 1, 1])
        XCTAssertEqual(files.map(\.deletions), [1, 1, 0, 1])
    }

    func testLegacyPathsOnlyPayloadKeepsARowForAWithheldPath() {
        let files = ResponseEditedFiles.editedFiles(in: [
            "paths": .array([.string("[path outside workspace]"), .string("A.swift")])
        ], singlePathCounts: nil)
        XCTAssertEqual(files.map(\.path), ["A.swift", "File 2"])
    }

    func testPatchPathReadsTheHeaderAndRejectsEscapes() {
        XCTAssertEqual(FileChangeSummary.patchPath("--- a/x/y.txt\n+++ b/x/y.txt\n@@"), "x/y.txt")
        XCTAssertEqual(FileChangeSummary.patchPath("diff --git a/Docs/My notes.md b/Docs/My notes.md\n@@"), "Docs/My notes.md")
        XCTAssertEqual(FileChangeSummary.patchPath("--- a/gone.txt\n+++ /dev/null\n@@"), "gone.txt")
        XCTAssertNil(FileChangeSummary.patchPath("--- a/../secret\n+++ b/../secret\n@@"))
        XCTAssertNil(FileChangeSummary.patchPath("+++ b//etc/passwd\n@@"))
        XCTAssertNil(FileChangeSummary.patchPath(nil))
    }

    func testResponseFooterMarksPatchCappedByHostAsPartial() throws {
        let value = item(["diffs": .array([
            .object(["path": .string("Large.swift"), "diff": .string("@@ -1,2 +1,2 @@\n-old\n+new"), "additions": .number(900), "deletions": .number(1)]),
            .object(["path": .string("Small.swift"), "diff": .string("@@ -1 +1 @@\n-a\n+b"), "additions": .number(1), "deletions": .number(1)])])])
        let row = ReadRow(id: "saved", author: "Agent", text: "", isUser: false, timestamp: "1000", turnId: "turn", item: value)
        let files = try XCTUnwrap(ResponseEditedFiles.footers(entries: ChatFeedEntry.grouping([row]), activeTurnIDs: [])["saved"]).files
        XCTAssertEqual(files.map(\.partial), [true, false])
        XCTAssertEqual(files[0].additions, 900)
    }
}

extension FileChangeSummaryTests {
    func testTurnDiffReplacesEditRowsIncludingFilesTheRowsMissed() throws {
        let edit = ReadRow(id: "edit", author: "Agent", text: "", isUser: false, timestamp: "1000", turnId: "turn",
            item: item(["diffs": .array([.object(["path": .string("app.js"), "diff": .string("-a\n+b")])])]))
        let reply = ReadRow(id: "reply", author: "Agent", text: "Done", isUser: false, timestamp: "2000", turnId: "turn")
        let entries = ChatFeedEntry.grouping([edit, reply])
        // The shape the Mac sends, saved and reloaded with the thread.
        let json = #"{"id":"turn","items":[],"status":"completed","editedFiles":{"paths":["app.js","comic.html"],"additions":4,"deletions":1,"diffs":[{"path":"app.js","kind":"update","diff":"@@ -1 +1 @@\n-a\n+b","additions":1,"deletions":1},{"path":"comic.html","kind":"add","diff":"@@ -0,0 +1,3 @@\n+<html>\n+<body>\n+</html>","additions":3,"deletions":0}]}}"#
        let decoded = try JSONDecoder().decode(ReadTurn.self, from: Data(json.utf8))
        let turn = try JSONDecoder().decode(ReadTurn.self, from: JSONEncoder().encode(decoded))
        let all = try XCTUnwrap(ResponseEditedFiles.latest(entries: entries, activeTurnIDs: [], turns: ["turn": turn]).edits)
        XCTAssertEqual(all.files.map(\.path), ["app.js", "comic.html"])
        XCTAssertEqual(all.files.map(\.additions), [1, 3])
        XCTAssertEqual(all.files.map(\.deletions), [1, 0])
        XCTAssertEqual(all.files[1].patches, ["@@ -0,0 +1,3 @@\n+<html>\n+<body>\n+</html>"])
        // A turn whose diff is empty edited nothing, whatever its rows said.
        let empty = ReadTurn(id: "turn", items: [], status: "completed", editedFiles: ["paths": .array([]), "diffs": .array([])])
        XCTAssertNil(ResponseEditedFiles.latest(entries: entries, activeTurnIDs: [], turns: ["turn": empty]).edits)
        // Pages merged from the cache keep the diff.
        let older = ThreadProjection(nextCursor: nil, hydrated: true, turns: [turn])
        let newer = ThreadProjection(nextCursor: nil, hydrated: true, turns: [ReadTurn(id: "turn", items: [], status: "completed")])
        XCTAssertNotNil(newer.mergingOlder(older).turns?.first?.editedFiles)
    }

}

final class DiffLineTests: XCTestCase {
    func testUnifiedDiffNumbersLinesFromHunkHeaders() {
        let lines = DiffLine.parse("--- a/A.swift\n+++ b/A.swift\n@@ -10,3 +10,3 @@\n keep\n-old\n+new\n tail\n")
        XCTAssertEqual(lines.map(\.kind), [.note, .note, .hunk, .context, .removed, .added, .context])
        XCTAssertEqual(lines[3].oldNumber, 10); XCTAssertEqual(lines[3].newNumber, 10)
        XCTAssertEqual(lines[4].oldNumber, 11); XCTAssertNil(lines[4].newNumber)
        XCTAssertEqual(lines[5].newNumber, 11); XCTAssertEqual(lines[5].text, "new")
        XCTAssertEqual(lines[6].oldNumber, 12); XCTAssertEqual(lines[6].newNumber, 12)
    }

    func testHeadersWithoutNumbersAndRawAdditionsLeaveNumbersUnknown() {
        let lines = DiffLine.parse("@@ @@\n-a\n+b\n+c")
        XCTAssertEqual(lines.map(\.kind), [.hunk, .removed, .added, .added])
        XCTAssertTrue(lines.allSatisfy { $0.oldNumber == nil && $0.newNumber == nil })
    }

    func testSplitRowsPairReplacementsAndKeepContextOnBothSides() {
        let rows = SplitDiffRow.pair(DiffLine.parse("@@ -1,3 +1,2 @@\n same\n-one\n-two\n+uno\n same2"))
        XCTAssertEqual(rows.count, 5)
        XCTAssertEqual(rows[2].left?.text, "one"); XCTAssertEqual(rows[2].right?.text, "uno")
        XCTAssertEqual(rows[3].left?.text, "two"); XCTAssertNil(rows[3].right)
        XCTAssertEqual(rows[4].left?.text, "same2"); XCTAssertEqual(rows[4].right?.text, "same2")
    }
}
