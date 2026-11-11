import XCTest
@testable import WonderPairing

// Contract: the app decodes the repository index fixture the Mac's schema test
// validates, names each repository's roles in plain words, and switches Code
// changes to one section per repository only when there is more than one.
final class ThreadRepositoriesTests: XCTestCase {
    private func fixture() throws -> Data {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/thread-repositories-v1.json"))
    }

    func testSharedFixtureDecodesThreeRepositoriesInPlainWords() throws {
        let index = try JSONDecoder().decode(ThreadRepositories.self, from: fixture())
        XCTAssertTrue(index.showsSections)
        XCTAssertEqual(index.repositories.map(\.name), ["wonder", "parser-kit", "example/compiler"])
        XCTAssertEqual(index.repositories.map { $0.roles.map(\.title) },
                       [["Project folder", "Edited"], ["Created in this chat"], ["Reviewing PR"]])
        let (workspace, created, reviewed) = (index.repositories[0], index.repositories[1], index.repositories[2])
        XCTAssertEqual(workspace.branchTitle, "agent/thread-repos → origin/main")
        XCTAssertEqual(workspace.syncText, "2 commits to push")
        XCTAssertEqual(workspace.changes?.summary, "1 staged · 2 unstaged · 1 untracked")
        XCTAssertEqual(workspace.pullRequests.map(\.relation), [.opened])
        XCTAssertEqual(workspace.pullRequests.first?.state, .draft)
        XCTAssertTrue(workspace.opensChanges)
        XCTAssertEqual(created.location, .init(rootId: "workspace", path: "parser-kit"))
        XCTAssertEqual(created.syncText, "No commits yet")
        XCTAssertEqual(created.changes?.summary, "3 untracked")
        XCTAssertFalse(reviewed.local)
        XCTAssertFalse(reviewed.opensChanges)
        XCTAssertNil(reviewed.branchTitle)
        XCTAssertEqual(reviewed.pullRequests.first?.accessibilityText,
                       "Pull request 7, Fix the parser's error recovery, Open, Reviewing, example/compiler")
        XCTAssertEqual(reviewed.accessibilityText, "Repository example/compiler, Reviewing PR, not on your computer")
        XCTAssertEqual(created.accessibilityText,
                       "Repository parser-kit, Created in this chat, branch main, No commits yet, 3 untracked")
    }

    func testOneRepositoryKeepsTheSingleLayoutAndNewerFieldsAreTolerated() throws {
        let one = ThreadRepositories(repositories: [.init(id: "local:0000000000000000", name: "a", roles: [.workspace])])
        XCTAssertFalse(one.showsSections)
        XCTAssertTrue(CodeChanges(git: nil, pullRequests: nil, replyEdits: nil, lastReplyChanged: false, repositories: one).repositories.isEmpty)
        let json = #"{"version":1,"githubAvailable":false,"truncated":false,"detail":null,"repositories":[{"id":"local:1","name":"a","roles":["workspace","futureRole"],"local":true,"location":null,"remote":null,"branch":null,"detached":true,"noCommits":false,"baseRef":null,"upstream":null,"ahead":null,"behind":null,"changes":null,"pullRequests":[{"number":3,"repository":"o/r","url":"https://example.com/o/r/pull/3","relation":"opened","title":null,"state":null,"checks":null}],"detail":null}]}"#
        let index = try JSONDecoder().decode(ThreadRepositories.self, from: Data(json.utf8))
        XCTAssertEqual(index.repositories[0].roles, [.workspace])
        XCTAssertTrue(index.repositories[0].pullRequests.isEmpty, "only github.com pages open")
        XCTAssertEqual(index.repositories[0].branchTitle, "Detached")
        XCTAssertNil(index.repositories[0].syncText)
    }

}
