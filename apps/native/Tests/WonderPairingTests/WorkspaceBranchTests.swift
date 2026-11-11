import XCTest
@testable import WonderPairing

// Contract: the app decodes the branch summary and change list fixtures the
// Mac's schema test validates, hides the chip outside a repository, and words
// the chip, comparison and rows for people rather than Git.
final class WorkspaceBranchTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/\(name)"))
    }

    func testSharedFixturesDecode() throws {
        let summary = try JSONDecoder().decode(WorkspaceGitSummary.self, from: fixture("workspace-git-summary-v1.json"))
        XCTAssertTrue(summary.showsChip)
        XCTAssertEqual(summary.shortTitle, "branch-chip")
        XCTAssertEqual(WorkspaceGitSummary(repository: true, branch: "agent/a-very-long-branch-name-for-review").shortTitle,
                       "a-very-long…r-review")
        XCTAssertEqual(summary.comparisonTitle, "agent/branch-chip → origin/main")
        XCTAssertEqual(summary.aheadBehindText, "3 commits ahead, 1 behind")
        XCTAssertEqual(summary.accessibilityText,
                       "Branch agent/branch-chip, worktree, 4 uncommitted files, 24 added lines, 6 removed lines")
        let changes = try JSONDecoder().decode(WorkspaceGitChanges.self, from: fixture("workspace-git-changes-v1.json"))
        XCTAssertEqual(changes.files.map(\.statusTitle), ["Modified", "Renamed", "New", "Deleted"])
        XCTAssertTrue(changes.files[0].binary)
        XCTAssertEqual(changes.files[1].accessibilityText,
                       "Sources/Branch.swift, Renamed, from Sources/Chip.swift, 12 added lines, 4 removed lines")
        XCTAssertEqual(changes.files[2].accessibilityText, "Sources/New.swift, New, 10 added lines")
        XCTAssertEqual(changes.files[1].folder, "Sources")
        XCTAssertEqual(summary.accessibilityText(uncommittedWork: false), "Branch agent/branch-chip, worktree")
        // This thread: the thread's own files carry their hunks; a binary one none.
        let thread = try JSONDecoder().decode(WorkspaceGitChanges.self, from: fixture("workspace-thread-changes-v1.json"))
        XCTAssertEqual(thread.scope, "thread")
        XCTAssertEqual(thread.files.map(\.statusTitle), ["Added", "Modified", "Deleted"])
        XCTAssertNil(thread.files[0].diff)
        XCTAssertTrue(thread.files[1].diff?.hasPrefix("@@ -1,2 +1,3 @@") == true)
        XCTAssertEqual(ThreadChangeCount(thread), ThreadChangeCount(totals: .init(files: 3, additions: 3, deletions: 3)))
    }

}
