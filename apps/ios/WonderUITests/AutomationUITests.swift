import XCTest

@MainActor final class AutomationUITests: XCTestCase {
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

    private func keep(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name; shot.lifetime = .keepAlways
        add(shot)
    }

    func testUncertainCreateWithEditedDraftRequiresNewRequest() {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-diagnostics-automations-fixture", "-diagnostics-automations-create-lost-response"]
        app.launch()
        let create = app.buttons["project-automation-new"]
        XCTAssertTrue(create.waitForExistence(timeout: 15))
        create.tap()
        XCTAssertTrue(app.buttons["project-automation-thread"].waitForExistence(timeout: 15))
        let name = app.descendants(matching: .any)["project-automation-name"]
        let prompt = app.descendants(matching: .any)["project-automation-prompt"]
        name.tap(); name.typeText("Daily review")
        prompt.tap(); prompt.typeText("Review the plan.")
        let save = app.buttons["project-automation-save"]
        save.tap()
        let failure = app.staticTexts["project-automation-save-failure"]
        XCTAssertTrue(failure.waitForExistence(timeout: 10))
        prompt.tap(); prompt.typeText(" Include risks.")
        save.tap()
        XCTAssertTrue(app.buttons["project-automation-new-request"].waitForExistence(timeout: 10))
        XCTAssertTrue((prompt.value as? String)?.contains("Include risks.") == true)
        app.buttons["project-automation-new-request"].tap()
        XCTAssertFalse(failure.exists)
        XCTAssertTrue(save.isEnabled)
        XCTAssertTrue((prompt.value as? String)?.contains("Include risks.") == true)
    }

    func testRevisionConflictKeepsAutomationEditsVisible() {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: appBundleIdentifier)
        app.launchArguments = ["-diagnostics-automations-fixture", "-diagnostics-automations-existing-project",
                               "-diagnostics-automations-revision-conflict"]
        app.launch()

        let morning = app.buttons["automation-toggle:fixture-morning-schedule"]
        XCTAssertTrue(morning.waitForExistence(timeout: 15))
        XCTAssertEqual(morning.label, "Pause")
        morning.tap()
        let toggleFailure = app.staticTexts["automation-change-failure:fixture-morning-schedule"]
        XCTAssertTrue(toggleFailure.waitForExistence(timeout: 10))
        XCTAssertTrue(toggleFailure.label.contains("Refresh and try again"))
        XCTAssertEqual(morning.label, "Pause", "A stale toggle must retain the server-confirmed status")

        let edit = app.buttons["automation-edit:fixture-project-schedule"]
        XCTAssertTrue(edit.isHittable)
        edit.tap()
        let prompt = app.descendants(matching: .any)["project-automation-prompt"]
        XCTAssertTrue(prompt.waitForExistence(timeout: 10))
        prompt.tap(); prompt.typeText(" Check the current plan.")
        app.buttons["project-automation-save"].tap()
        let editFailure = app.staticTexts["project-automation-save-failure"]
        XCTAssertTrue(editFailure.waitForExistence(timeout: 10))
        XCTAssertTrue(editFailure.label.contains("Reload the latest version"))
        XCTAssertTrue((prompt.value as? String)?.contains("Check the current plan.") == true,
                      "A stale edit must remain visible so the user can recover it")
        let reload = app.buttons["project-automation-reload"]
        XCTAssertTrue(reload.isHittable)
        reload.tap()
        XCTAssertFalse(editFailure.waitForExistence(timeout: 3))
        let row = app.descendants(matching: .any)["automation-row:fixture-project-schedule"]
        XCTAssertTrue(row.staticTexts["Project updated on Mac"].waitForExistence(timeout: 10))
        XCTAssertTrue(edit.waitForExistence(timeout: 10))
        edit.tap()
        XCTAssertTrue(prompt.waitForExistence(timeout: 10))
        XCTAssertEqual(prompt.value as? String, "Updated on Mac.",
                       "Reload should show the latest server version before another edit")
    }

}
