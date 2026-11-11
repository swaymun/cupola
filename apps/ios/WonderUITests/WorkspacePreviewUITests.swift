import XCTest

@MainActor final class WorkspacePreviewUITests: XCTestCase {
    private let appBundleIdentifier = "com.swaymun.wonder"

    // Keep the visible state with every failure; a failed assertion alone
    // rarely shows which control was covered or missing.
    override func record(_ issue: XCTIssue) {
        var issue = issue
        let screen = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        screen.name = "Failure screen"; screen.lifetime = .keepAlways
        issue.add(screen)
        let tree = XCTAttachment(string: XCUIApplication(bundleIdentifier: appBundleIdentifier).debugDescription)
        tree.name = "Failure hierarchy"; tree.lifetime = .keepAlways
        issue.add(tree)
        super.record(issue)
    }

    private func launchProjectFiles(revision: Bool = false, html: Bool = false,
                                    hostileHTML: Bool = false) -> XCUIApplication {
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-read-preview", "-send-preview", "-files-preview",
                               "-project-files-conversation-preview", "-workspace-document-preview"]
        if revision { app.launchArguments.append("-artifact-revision-preview") }
        if html { app.launchArguments.append("-workspace-html-scroll-preview") }
        if hostileHTML { app.launchArguments.append("-workspace-html-script-preview") }
        app.launch()
        XCTAssertTrue(app.buttons["conversation-files-pill"].waitForExistence(timeout: 10))
        return app
    }

    private func open(_ name: String, in app: XCUIApplication) {
        app.buttons["conversation-files-pill"].tap()
        let list = app.descendants(matching: .any)["workspace-file-list"]
        XCTAssertTrue(list.waitForExistence(timeout: 10), "Files should replace the conversation")
        let file = app.buttons["workspace-file-entry:\(name)"]
        let composerTop = app.buttons["computer-status-pill"].frame.minY
        let visibleTop = list.frame.minY + 8
        let visibleBottom = min(list.frame.maxY, composerTop) - 8
        func nudge(_ up: Bool) {
            let start = list.coordinate(withNormalizedOffset: .zero)
                .withOffset(CGVector(dx: list.frame.width / 2,
                                     dy: (up ? visibleBottom - 24 : visibleTop + 24) - list.frame.minY))
            let end = start.withOffset(CGVector(dx: 0, dy: up ? -110 : 110))
            start.press(forDuration: 0.05, thenDragTo: end)
        }
        for _ in 0..<15 {
            if file.exists { break }
            nudge(true)
        }
        XCTAssertTrue(file.waitForExistence(timeout: 5), "Missing \(name) in Files")
        for _ in 0..<10 {
            if file.isHittable && file.frame.minY >= visibleTop && file.frame.maxY <= visibleBottom { break }
            nudge(file.frame.maxY > visibleBottom)
        }
        XCTAssertTrue(file.isHittable, "\(name) must be reachable on this device")
        XCTAssertGreaterThanOrEqual(file.frame.minY, visibleTop)
        XCTAssertLessThanOrEqual(file.frame.maxY, visibleBottom,
                                 "\(name) must be visible above the composer before tapping")
        file.tap()
    }

    private func capture(_ app: XCUIApplication, name: String) {
        let image = XCTAttachment(screenshot: app.screenshot())
        image.name = name
        image.lifetime = .keepAlways
        add(image)
    }

    func testHTMLPreviewDoesNotRunEmbeddedScript() {
        continueAfterFailure = false
        let app = launchProjectFiles(hostileHTML: true)
        open("reader.html", in: app)
        let safe = app.webViews.staticTexts["Safe content remains"]
        XCTAssertTrue(safe.waitForExistence(timeout: 10),
                      "HTML content should remain readable with scripts disabled")
        XCTAssertFalse(app.webViews.staticTexts["SCRIPT EXECUTED"].exists,
                       "A workspace HTML preview must not execute source scripts")
        app.buttons["workspace-preview-expand"].tap()
        XCTAssertTrue(app.buttons["workspace-preview-collapse"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.webViews.staticTexts["Safe content remains"].exists)
        XCTAssertFalse(app.webViews.staticTexts["SCRIPT EXECUTED"].exists)
    }
}
