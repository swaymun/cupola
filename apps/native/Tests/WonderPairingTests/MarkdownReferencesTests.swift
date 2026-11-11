import XCTest
@testable import WonderPairing

final class MarkdownReferencesTests: XCTestCase {
    func testReferencesResolveInsideTheRootOnly() {
        func resolve(_ raw: String, from doc: String = "README.md") -> MarkdownReference { .resolve(raw, documentPath: doc) }
        XCTAssertEqual(resolve("./docs/screenshot.png"), .local("docs/screenshot.png"))
        XCTAssertEqual(resolve("docs/My%20Shot.png?raw=true"), .local("docs/My Shot.png"))
        XCTAssertEqual(resolve("../assets/logo.png", from: "docs/guide/README.md"), .local("docs/assets/logo.png"))
        XCTAssertEqual(resolve("/docs/setup.md#install", from: "a/b/README.md"), .local("docs/setup.md"))
        XCTAssertEqual(resolve("<docs/with space.md>"), .local("docs/with space.md"))
        XCTAssertEqual(resolve("https://example.com/a.png"), .web(URL(string: "https://example.com/a.png")!))
        XCTAssertEqual(resolve("#usage"), .anchor)
        // Never outside the project root, never another scheme or the home folder.
        for raw in ["../secret.png", "docs/../../secret.png", "%2E%2E/secret.png", "..%2Fsecret.png", "~/notes.md",
                    "file:///etc/passwd", "javascript:alert(1)", "data:image/png;base64,AAAA", "docs\\..\\..\\x.png", ".", "/", ""] {
            XCTAssertEqual(resolve(raw), .blocked, raw)
        }
    }

}
