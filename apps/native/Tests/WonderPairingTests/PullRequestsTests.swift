import XCTest
@testable import WonderPairing

// Contract: the app decodes the same pull request fixture the Mac's schema
// test validates, shows the pill only for a signed-in, understood, non-empty
// list, and opens only github.com pull request pages.
final class PullRequestsTests: XCTestCase {
    private func fixture() throws -> Data {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/project-pull-requests-v1.json"))
    }

    func testSharedFixtureDecodes() throws {
        let list = try JSONDecoder().decode(ThreadPullRequests.self, from: fixture())
        XCTAssertTrue(list.showsPill)
        XCTAssertEqual(list.pullRequests.map(\.number), [12, 9, 3])
        XCTAssertEqual(list.pullRequests.map(\.state), [.draft, .merged, .closed])
        XCTAssertEqual(list.pullRequests.map(\.checks.summary), ["2 of 5 checks running", "1 of 5 checks failed", nil])
        XCTAssertEqual(list.detail, "Some pull requests could not be loaded.")
        XCTAssertEqual(list.total, 5, "the thread opened more than the newest listed")
        let details = try XCTUnwrap(list.pullRequests.first?.details)
        XCTAssertEqual(details.checks?.map(\.name), ["Unit tests", "Package tests", "Documentation", "iOS build", "Privacy audit"])
        XCTAssertEqual(details.checks?.map(\.resultTitle), ["Passed", "Passed", "Skipped", "Running", "Pending"])
        XCTAssertEqual(details.checks?.map(\.isNeutral), [false, false, true, false, false])
        XCTAssertEqual(details.reviewRequests?.map(\.title), ["alex", "example/mobile (team)"])
        XCTAssertEqual(details.reviewTitle, "Review required")
        XCTAssertEqual(details.changedFiles, 7)
        XCTAssertEqual(details.checks?.first?.workflow, "CI")
        XCTAssertEqual(details.mergeSummary(state: .draft), "Draft — not ready for review")
        XCTAssertEqual(details.mergeSummary(state: .open), "GitHub reports this merge is blocked. Open GitHub for the requirements.")
        XCTAssertNil(list.pullRequests[1].details, "older Mac payloads still decode")
    }

    func testSharedFixtureCarriesBranchCountsAndHeads() throws {
        let list = try JSONDecoder().decode(ThreadPullRequests.self, from: fixture())
        XCTAssertEqual(list.counts, .init(open: 0, draft: 1, merged: 1, closed: 1))
        XCTAssertFalse(list.complete)
        XCTAssertEqual(list.branch?.name, "agent/thread-pull-requests")
        XCTAssertEqual(list.branch?.pushed, true)
        XCTAssertEqual(list.branch?.pullRequest?.number, 12)
        let pr = try XCTUnwrap(list.pullRequests.first)
        XCTAssertEqual([pr.headRef, pr.baseRef, pr.author], ["agent/thread-pull-requests", "main", "alex"])
        XCTAssertEqual(pr.headOid, "0123456789abcdef0123456789abcdef01234567")
        XCTAssertEqual([pr.additions, pr.deletions], [120, 14])
        let older = try JSONDecoder().decode(ThreadPullRequests.self, from: Data(
            #"{"version":1,"available":true,"total":4,"pullRequests":[{"number":1,"repository":"o/r","title":"A","state":"open","checks":{"state":"none","passed":0,"failed":0,"pending":0},"url":"https://github.com/o/r/pull/1"}]}"#.utf8))
        XCTAssertEqual(older.counts, .init(open: 1), "an older Mac's counts come from its list")
        XCTAssertFalse(older.complete, "one of four listed")
        XCTAssertNil(older.branch)
        XCTAssertNil(older.pullRequests.first?.headRef)
    }

    private func pr(_ number: Int, _ state: ThreadPullRequests.PullRequest.State, checks: ThreadPullRequests.PullRequest.Checks.State = .passing,
                    head: String? = nil, details: [String: Any]? = nil) throws -> ThreadPullRequests.PullRequest {
        try ThreadPullRequests.PullRequest(number: number, repository: "o/r", title: "PR \(number)", state: state,
            checks: .init(state: checks, passed: 1), url: XCTUnwrap(URL(string: "https://github.com/o/r/pull/\(number)")),
            details: details.map { try JSONDecoder().decode(PullRequestDetails.self, from: JSONSerialization.data(withJSONObject: $0)) },
            headRef: head)
    }

    func testUnknownStatesAndForeignLinksAreContained() throws {
        let json = #"{"version":1,"available":true,"detail":null,"pullRequests":[{"number":1,"repository":"o/r","title":"A","state":"queued","checks":{"state":"later","passed":0,"failed":0,"pending":0},"url":"https://github.com/o/r/pull/1"},{"number":2,"repository":"o/r","title":"B","state":"open","checks":{"state":"none","passed":0,"failed":0,"pending":0},"url":"https://evil.example/o/r/pull/2"}]}"#
        let list = try JSONDecoder().decode(ThreadPullRequests.self, from: Data(json.utf8))
        XCTAssertEqual(list.pullRequests.map(\.number), [1])
        XCTAssertEqual(list.pullRequests.first?.state, .unknown)
        XCTAssertEqual(list.pullRequests.first?.checks.state, ThreadPullRequests.PullRequest.Checks.State.unknown)
    }

    func testUnknownAndPartialNativeFactsNeverBecomeMergePermission() throws {
        func details(_ fields: [String: Any]) throws -> PullRequestDetails {
            try JSONDecoder().decode(PullRequestDetails.self, from: JSONSerialization.data(withJSONObject: fields))
        }
        let unknown = try details(["checks": [["name": "External CI", "state": "future", "result": "FUTURE"]],
                                   "mergeStateStatus": "UNKNOWN", "mergeable": "MERGEABLE"])
        XCTAssertEqual(unknown.checks?.first?.state, .unknown)
        XCTAssertEqual(unknown.checks?.first?.resultTitle, "Unknown")
        XCTAssertEqual(unknown.mergeSummary(state: .open), "Merge status unavailable")
        XCTAssertEqual(unknown.reviewTitle, "No review decision available")
        XCTAssertNil(unknown.reviewRequests)
        XCTAssertNil(unknown.changedFiles)
        let complete = try details(["mergeStateStatus": "CLEAN", "mergeable": "MERGEABLE"])
        XCTAssertEqual(complete.mergeSummary(state: .open), "GitHub reports a clean merge and passing checks")
        let conflicts = try details(["mergeStateStatus": "UNKNOWN", "mergeable": "CONFLICTING"])
        XCTAssertEqual(conflicts.mergeSummary(state: .open), "Merge conflicts must be resolved")
        XCTAssertEqual(conflicts.mergeSummary(state: .merged), "Merged")
        XCTAssertEqual(conflicts.mergeSummary(state: .closed), "Closed without merging")
        let partial = try details(["checks": Array(repeating: ["name": "CI", "state": "passing", "result": "SUCCESS"], count: 100),
                                   "reviewRequests": Array(repeating: ["name": "alex", "kind": "user"], count: 100)])
        XCTAssertTrue(partial.reviewersMayBeLimited)
        XCTAssertEqual(ThreadPullRequests.PullRequest.Checks(state: .failing, passed: 98, failed: 2, limited: true).summary,
                       "2 reported checks failed")
        XCTAssertEqual(ThreadPullRequests.PullRequest.Checks(state: .pending, passed: 99, pending: 1, limited: true).summary,
                       "1 reported check running")
        XCTAssertEqual(ThreadPullRequests.PullRequest.Checks(state: .failing, failed: 1, unknown: 1).summary, "1 of 2 checks failed")
        XCTAssertEqual(ThreadPullRequests.PullRequest.Checks(state: .unknown).summary, "Some check results are unavailable")
    }

    func testPathEscapesTheConversation() throws {
        XCTAssertEqual(try ThreadPullRequests.path(conversationId: "a/b"), "/api/v1/project-conversations/a%2Fb/pull-requests")
        XCTAssertEqual(try ThreadPullRequests.path(conversationId: "c", refresh: true), "/api/v1/project-conversations/c/pull-requests?refresh=true")
        XCTAssertThrowsError(try ThreadPullRequests.path(conversationId: ""))
    }
}
