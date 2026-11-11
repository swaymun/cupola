import CoreGraphics
import XCTest
@testable import WonderComputerUseCore

/// Fixtures follow what Chrome reported on the owner's Mac on 2026-10-14 (titles
/// replaced): two maximized windows with identical AppleScript bounds
/// {0, 33, 1512, 982}, only one of them on screen as 1512x949 at 0,33, and a
/// window-server title Chrome shortened in the middle with "…".
final class BrowserTabWindowTests: XCTestCase {
    private let maximized = CGRect(x: 0, y: 33, width: 1512, height: 949)

    private func script(_ lines: [String]) -> [BrowserTabWindow.ScriptWindow] {
        BrowserTabWindow.windows(fromScript: lines.joined(separator: "\n") + "\n")
    }

    func testParsesTheScriptOutput() {
        let windows = script(["true\t0\t33\t1512\t982\tfalse\tTrip notes - Docs", "false\t10\t20\t30\tbad\tfalse\tx", "junk"])
        XCTAssertEqual(windows, [.init(holdsTab: true, bounds: maximized, minimized: false, activeTitle: "Trip notes - Docs")])
    }

    func testASingleWindowWithTheTabsBoundsIsExact() {
        let windows = script(["false\t100\t100\t900\t700\tfalse\tOther", "true\t0\t33\t1512\t982\tfalse\tTrip notes"])
        let screen = [BrowserTabWindow.ScreenWindow(id: 31510, frame: maximized, title: "Trip notes"),
                      .init(id: 31511, frame: CGRect(x: 100, y: 100, width: 800, height: 600), title: "Other"),
                      .init(id: 31634, frame: CGRect(x: 0, y: 33, width: 1512, height: 115), title: nil)]
        XCTAssertEqual(BrowserTabWindow.match(windows, screen: screen), .exact(31510))
    }

    func testMaximizedTwinsAreToldApartByTheShortenedTitle() {
        let long = "Release checklist - Q4 launch planning for the whole team, with notes?? - My Drive"
        let shortened = "Release checklist - Q4 launch planning… with notes?? - My Drive"
        let screen = [BrowserTabWindow.ScreenWindow(id: 31510, frame: maximized, title: shortened)]
        // The tab is in the on-screen window.
        XCTAssertEqual(BrowserTabWindow.match(script(["true\t0\t33\t1512\t982\tfalse\t\(long)", "false\t0\t33\t1512\t982\tfalse\tInbox"]), screen: screen),
                       .exact(31510))
        // The tab is in the twin on another Space: the visible window is not it.
        XCTAssertEqual(BrowserTabWindow.match(script(["false\t0\t33\t1512\t982\tfalse\t\(long)", "true\t0\t33\t1512\t982\tfalse\tInbox"]), screen: screen),
                       .notFound)
    }

    func testTwinsWithTheSameTitleOrUnreadableTitlesAreAmbiguous() {
        let twins = script(["true\t0\t33\t1512\t982\tfalse\tNew Tab", "false\t0\t33\t1512\t982\tfalse\tNew Tab"])
        let screen = [BrowserTabWindow.ScreenWindow(id: 1, frame: maximized, title: "New Tab"),
                      .init(id: 2, frame: maximized, title: "New Tab")]
        XCTAssertEqual(BrowserTabWindow.match(twins, screen: screen), .ambiguous)
        let distinct = script(["true\t0\t33\t1512\t982\tfalse\tA", "false\t0\t33\t1512\t982\tfalse\tB"])
        let untitled = [BrowserTabWindow.ScreenWindow(id: 1, frame: maximized, title: nil), .init(id: 2, frame: maximized, title: nil)]
        XCTAssertEqual(BrowserTabWindow.match(distinct, screen: untitled), .notFound)
        let twoOnScreenSameTitle = [BrowserTabWindow.ScreenWindow(id: 1, frame: maximized, title: "A"), .init(id: 2, frame: maximized, title: "A")]
        XCTAssertEqual(BrowserTabWindow.match(distinct, screen: twoOnScreenSameTitle), .ambiguous)
    }

    func testMissingOrMinimizedWindowsAreNotFound() {
        let screen = [BrowserTabWindow.ScreenWindow(id: 1, frame: maximized, title: "A")]
        XCTAssertEqual(BrowserTabWindow.match(script(["false\t0\t33\t1512\t982\tfalse\tA"]), screen: screen), .notFound)
        XCTAssertEqual(BrowserTabWindow.match(script(["true\t0\t33\t1512\t982\ttrue\tA"]), screen: screen), .notFound)
        XCTAssertEqual(BrowserTabWindow.match(script(["true\t0\t33\t1512\t982\tfalse\tA"]), screen: []), .notFound)
    }

    func testTitleMatchingHonoursChromesMiddleEllipsis() {
        XCTAssertTrue(BrowserTabWindow.titleMatches("Docs", tabTitle: "Docs"))
        XCTAssertFalse(BrowserTabWindow.titleMatches("Docs", tabTitle: "Docs 2"))
        XCTAssertTrue(BrowserTabWindow.titleMatches("Long… end", tabTitle: "Long middle end"))
        XCTAssertFalse(BrowserTabWindow.titleMatches("Long… end", tabTitle: "Long end"))
        XCTAssertFalse(BrowserTabWindow.titleMatches("", tabTitle: ""))
    }
}
