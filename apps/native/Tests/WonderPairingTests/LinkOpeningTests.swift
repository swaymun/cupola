import XCTest
@testable import WonderPairing

// Contract: a web link opens where Settings says (in the app by default,
// Safari, or the default browser); other links always go to the system as they were.
final class LinkOpeningTests: XCTestCase {
    private let page = URL(string: "https://example.com/guide?section=2#install")!

    func testOnlyWebPagesFollowTheChoice() {
        for raw in ["mailto:owner@example.com", "tel:+15555550100", "app-settings:", "wonder-preview://port-5173/", "docs/USAGE.md"] {
            let url = URL(string: raw)!
            for choice in LinkOpening.allCases {
                XCTAssertEqual(choice.destination(for: url), .system(url), "\(raw) with \(choice)")
            }
        }
    }
}
