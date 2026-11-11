import XCTest
@testable import WonderPairing

// Contract: each Git action control shows its progress while the Mac works,
// then the Mac's success or refusal in its own words, kept until the sheet
// closes; a push to the default branch waits for confirmation; the menu says
// why an action is unavailable; pull request tasks only fill the composer;
// Merge and Merge when ready are pinned to the commit the owner reviewed and
// refuse without one.
@MainActor
final class GitActionsTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/\(name)"))
    }

    private let pr = URL(string: "https://github.com/example/wonder/pull/12")!

    private func repository(branch: String? = "agent/x", upstream: String? = "origin/agent/x", ahead: Int? = 0,
                            behind: Int? = 0, changes: ThreadRepositories.Changes? = .init(staged: 0, unstaged: 2, untracked: 1),
                            pullRequests: [ThreadRepositories.PullRequest] = [], writable: Bool = true) -> ThreadRepositories.Repository {
        .init(id: "local:4f1c2b7a9d3e5f60", name: "wonder", roles: [.workspace], location: .init(rootId: "workspace", path: ""),
              writable: writable, defaultBranch: "main", remote: "example/wonder", branch: branch, upstream: upstream,
              ahead: ahead, behind: behind, changes: changes, pullRequests: pullRequests)
    }

    /// A stand-in Mac that holds each action until the test answers it.
    private final class HeldMac {
        var requests: [GitActionRequest] = []
        private var waiting: [CheckedContinuation<GitActionOutcome, Error>] = []
        @MainActor func perform(_ request: GitActionRequest) async throws -> GitActionOutcome {
            requests.append(request)
            return try await withCheckedThrowingContinuation { waiting.append($0) }
        }
        func answer(_ outcome: GitActionOutcome) { waiting.removeFirst().resume(returning: outcome) }
        func fail(_ error: Error) { waiting.removeFirst().resume(throwing: error) }
        var pending: Int { waiting.count }
    }

    private func settle() async { for _ in 0..<20 { await Task.yield() } }

    private func until(_ condition: @autoclosure () -> Bool) async {
        for _ in 0..<200 where !condition() { await Task.yield() }
    }

    func testPushToTheDefaultBranchWaitsForConfirmation() async throws {
        let mac = HeldMac()
        let model = GitActionsModel(perform: mac.perform)
        let main = repository(branch: "main", upstream: "origin/main", ahead: 1)
        XCTAssertTrue(GitActionAvailability(main, githubAvailable: true, busy: false).pushesToDefaultBranch)
        let first = model.run(GitActionRequest(repository: main, action: .push)!, key: "repo")
        await until(mac.pending == 1)
        mac.answer(.init(action: .push, ok: false, reason: "confirmDefaultBranch",
                         message: "This pushes straight to main, the default branch, without a pull request."))
        await first?.value
        XCTAssertNil(model.status("repo"), "no failure shown while asking")
        XCTAssertEqual(model.confirmation?.message, "This pushes straight to main, the default branch, without a pull request.")
        let second = model.confirm()
        await until(mac.pending == 1)
        XCTAssertEqual(mac.requests.last?.confirmDefaultBranch, true)
        mac.answer(.init(action: .push, ok: true, message: "Pushed 1 commit to origin/main."))
        await second?.value
        XCTAssertEqual(model.status("repo")?.phase, .succeeded)
    }

    func testClosingTheSheetCancelsWork() async {
        let model = GitActionsModel(perform: { _ in
            try await Task.sleep(for: .seconds(30))
            return .init(action: .commit, ok: true, message: "late")
        })
        model.run(.commit(repository(), message: "m", includeUnstaged: false)!, key: "repo")
        XCTAssertTrue(model.isRunning("repo"))
        model.cancelAll()
        await settle()
        XCTAssertNil(model.status("repo"))
    }

    func testRequestsEncodeOnlyTheirFieldsAndSharedFixturesDecode() throws {
        let request = GitActionRequest.commit(repository(), message: "Fix", includeUnstaged: true)!
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: AnyHashable]
        XCTAssertEqual(json, ["repositoryId": "local:4f1c2b7a9d3e5f60", "root": "workspace", "repo": "", "action": "commit",
                              "message": "Fix", "includeUnstaged": true])
        XCTAssertEqual(try GitActionRequest.path(conversationId: "c-1"), "/api/v1/conversations/c%2D1/repos/actions")
        XCTAssertEqual(try PullRequestMergeState.path(conversationId: "c-1", repository: repository(), url: pr),
                       "/api/v1/conversations/c%2D1/repos/pull-request?repositoryId=local:4f1c2b7a9d3e5f60&root=workspace&repo=&url=https://github.com/example/wonder/pull/12")

        struct Shared: Decodable { let outcomes: [GitActionOutcome]; let mergeState: PullRequestMergeState }
        let shared = try JSONDecoder().decode(Shared.self, from: fixture("git-actions-v1.json"))
        XCTAssertEqual(shared.outcomes.map(\.ok), [true, false])
        XCTAssertEqual(shared.outcomes[0].pullRequest?.number, 41)
        XCTAssertTrue(shared.outcomes[1].needsDefaultBranchConfirmation)
        XCTAssertEqual(shared.mergeState.methods, [.squash, .rebase], "an unknown method is skipped")
        XCTAssertTrue(shared.mergeState.conflicting)
        XCTAssertFalse(shared.mergeState.canAutoMerge)

        let index = try JSONDecoder().decode(ThreadRepositories.self, from: fixture("thread-repositories-v1.json"))
        XCTAssertEqual(index.repositories.map(\.actionsAvailable), [true, true, false])
        XCTAssertEqual(index.repositories[0].defaultBranch, "main")
    }

    // MARK: Reviewed head

    private let headA = "0123456789abcdef0123456789abcdef01234567"
    private let headB = "fedcba9876543210fedcba9876543210fedcba98"

    private func mergeState(head: String?? = .some(nil), extra: String = "") throws -> PullRequestMergeState {
        let headField: String
        switch head {
        case .none: headField = ""
        case .some(.none): headField = #""headOid": null,"#
        case .some(.some(let oid)): headField = #""headOid": "\#(oid)","#
        }
        let json = """
        {"number": 12, "url": "https://github.com/example/wonder/pull/12", "state": "open", \(headField) \(extra)
         "checks": "passing", "conflicting": false, "autoMerge": false, "methods": ["squash"], "defaultMethod": "squash"}
        """
        return try JSONDecoder().decode(PullRequestMergeState.self, from: Data(json.utf8))
    }

    private func body(_ request: GitActionRequest?) throws -> [String: AnyHashable] {
        let request = try XCTUnwrap(request)
        return try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: AnyHashable])
    }

    func testSharedFixtureHeadReachesMergeAndAutoMergeRequests() throws {
        struct Shared: Decodable { let mergeState: PullRequestMergeState }
        let state = try JSONDecoder().decode(Shared.self, from: fixture("git-actions-v1.json")).mergeState
        XCTAssertEqual(state.headOid, headA)
        XCTAssertEqual(state.reviewedHead, headA)
        let merge = try body(.merge(repository(), pullRequest: pr, method: .squash, reviewedHead: state.reviewedHead))
        XCTAssertEqual(merge["expectedHeadOid"], headA as AnyHashable)
        XCTAssertEqual(merge["action"], "merge")
        let auto = try body(.autoMerge(repository(), pullRequest: pr, enabled: true, method: .rebase, reviewedHead: state.reviewedHead))
        XCTAssertEqual(auto["expectedHeadOid"], headA as AnyHashable)
        XCTAssertEqual(auto["enabled"], true)
    }

    func testMergeAndEnablingAutoMergeRefuseWithoutAKnownHead() throws {
        for head in [nil, "", "   ", "not-a-commit", String(repeating: "a", count: 39), String(repeating: "g", count: 40)] as [String?] {
            XCTAssertNil(GitActionRequest.merge(repository(), pullRequest: pr, method: .squash, reviewedHead: head), "merge with \(head ?? "nil")")
            XCTAssertNil(GitActionRequest.autoMerge(repository(), pullRequest: pr, enabled: true, method: .squash, reviewedHead: head))
        }
        for state in [try mergeState(), try mergeState(head: .none), try mergeState(head: .some("")), try mergeState(head: .some("zz"))] {
            XCTAssertNil(state.reviewedHead)
            XCTAssertFalse(state.canMerge)
            XCTAssertFalse(state.canAutoMerge)
            XCTAssertNotNil(state.headNote)
            XCTAssertNil(MergeApproval(method: .squash, state: state))
        }
        // Turning it off needs no commit, and sends none.
        let off = try body(.autoMerge(repository(), pullRequest: pr, enabled: false, method: nil, reviewedHead: nil))
        XCTAssertEqual(off["enabled"], false)
        XCTAssertNil(off["expectedHeadOid"])
        XCTAssertNotNil(GitActionRequest.autoMerge(repository(), pullRequest: pr, enabled: false, method: nil))
    }

    func testAnApprovalHoldsOnlyForTheCommitTheOwnerReviewed() throws {
        let reviewed = try mergeState(head: .some(headA.uppercased()))
        XCTAssertEqual(reviewed.reviewedHead, headA, "normalized the way the Mac compares")
        let approval = try XCTUnwrap(MergeApproval(method: .rebase, state: reviewed))
        XCTAssertTrue(approval.holds(for: try mergeState(head: .some(headA))))
        XCTAssertFalse(approval.holds(for: try mergeState(head: .some(headB))), "a later refresh can't switch the approved commit")
        XCTAssertFalse(approval.holds(for: try mergeState(head: .some(nil))))
        let request = try body(approval.request(repository(), pullRequest: pr))
        XCTAssertEqual(request["expectedHeadOid"], headA as AnyHashable)
        XCTAssertEqual(request["method"], "rebase")
    }

    func testMergeStateToleratesAdditiveFieldsAndAnUnusableHead() throws {
        let extra = try mergeState(head: .some(headA), extra: #""futureField": {"a": [1, 2]}, "mergeQueue": true,"#)
        XCTAssertEqual(extra.reviewedHead, headA)
        let wrongType = try JSONDecoder().decode(PullRequestMergeState.self, from: Data("""
        {"number": 12, "url": "https://github.com/example/wonder/pull/12", "state": "open", "headOid": 7,
         "checks": "passing", "conflicting": false, "autoMerge": false, "methods": ["squash"]}
        """.utf8))
        XCTAssertNil(wrongType.reviewedHead, "a head of another type is unknown, not a decoding failure")
        XCTAssertEqual(wrongType.number, 12)
    }
}
