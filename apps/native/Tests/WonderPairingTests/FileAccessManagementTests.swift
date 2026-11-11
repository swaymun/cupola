import XCTest
@testable import WonderPairing

final class FileAccessManagementTests: XCTestCase {
    func testDirectoryPageDecodesNamesKindsAndPagination() throws {
        let page = try JSONDecoder().decode(MacLocationPage.self, from: Data(#"{"path":"/Users/owner","parentPath":"/Users","entries":[{"name":"Project","path":"/Users/owner/Project","isDirectory":true},{"name":"Notes.md","path":"/Users/owner/Notes.md","isDirectory":false}],"nextOffset":200}"#.utf8))
        XCTAssertEqual(page.entries.map(\.isDirectory), [true, false])
        XCTAssertEqual(page.parentPath, "/Users")
        XCTAssertEqual(page.nextOffset, 200)
    }
    func testAliasRowsKeepDistinctIdentityWhenCanonicalPathsMatch() throws {
        let page = try JSONDecoder().decode(MacLocationPage.self, from: Data(#"{"path":"/Users/owner","parentPath":"/Users","entries":[{"name":"Project","path":"/Shared/project","isDirectory":true},{"name":"Project alias","path":"/Shared/project","isDirectory":true}],"nextOffset":null}"#.utf8))
        XCTAssertEqual(page.entries[0].path, page.entries[1].path)
        XCTAssertNotEqual(page.entries[0].id, page.entries[1].id)
    }

}
