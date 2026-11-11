import XCTest

/// Runs one `.wctl` scenario on a physical device. `scripts/device-runner` translates the file
/// into JSON steps (only the verbs listed in `run`) and passes them in WONDER_SCENARIO_STEPS;
/// without it the test skips. Simulator lanes drive the same scenarios through AXe instead.
@MainActor final class ScenarioDeviceUITests: XCTestCase {
    private let bundleID = "com.swaymun.wonder"
    private var app: XCUIApplication!

    private struct Step: Decodable {
        var verb: String
        var line: Int
        var launch: [String]?
        var id: String?
        var label: String?
        var contains: String?
        var type: String?
        var nth: Int?
        var gone: Bool?
        var never: Double?
        var timeout: Double?
        var text: String?
        var name: String?
    }

    func testScenario() throws {
        continueAfterFailure = false
        guard let raw = ProcessInfo.processInfo.environment["WONDER_SCENARIO_STEPS"] else {
            throw XCTSkip("WONDER_SCENARIO_STEPS not set; run through scripts/device-runner")
        }
        let steps = try JSONDecoder().decode([Step].self, from: Data(raw.utf8))
        for step in steps { try run(step) }
    }

    private func run(_ step: Step) throws {
        let at = "line \(step.line) \(step.verb)"
        switch step.verb {
        case "launch":
            app = XCUIApplication(bundleIdentifier: bundleID)
            app.launchArguments = step.launch ?? []
            app.launch()
        case "activate":
            app.activate()
        case "terminate":
            app.terminate()
        case "wait":
            let query = try element(step, in: app)
            if let never = step.never {
                // Stays put for `never` seconds; fails if the element shows up meanwhile.
                let deadline = Date().addingTimeInterval(never)
                while !query.exists && Date() < deadline { Thread.sleep(forTimeInterval: 0.2) }
                XCTAssertFalse(query.exists, "\(at): appeared")
            } else if step.gone == true {
                let deadline = Date().addingTimeInterval(step.timeout ?? 10)
                while query.exists && Date() < deadline { Thread.sleep(forTimeInterval: 0.2) }
                XCTAssertFalse(query.exists, "\(at): still present")
            } else {
                XCTAssertTrue(query.waitForExistence(timeout: step.timeout ?? 10), "\(at): not found within \(step.timeout ?? 10)s")
            }
        case "tap":
            let target = try element(step, in: app)
            XCTAssertTrue(target.waitForExistence(timeout: step.timeout ?? 10), "\(at): not found")
            target.tap()
        case "type":
            app.typeText(step.text ?? "")
        case "sidebar":
            try sidebar(open: step.text == "open", at: at)
        case "shot":
            let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
            shot.name = step.name ?? "shot"
            shot.lifetime = .keepAlways
            add(shot)
        default:
            XCTFail("\(at): verb not supported on device")
        }
    }

    private func element(_ step: Step, in app: XCUIApplication) throws -> XCUIElement {
        var parts: [NSPredicate] = []
        if let id = step.id { parts.append(NSPredicate(format: "identifier == %@", id)) }
        if let label = step.label { parts.append(NSPredicate(format: "label == %@", label)) }
        if let text = step.contains {
            parts.append(NSPredicate(format: "label CONTAINS %@ OR value CONTAINS %@ OR placeholderValue CONTAINS %@", text, text, text))
        }
        guard !parts.isEmpty else { throw XCTSkip("selector missing") }
        var query = app.descendants(matching: .any).matching(NSCompoundPredicate(andPredicateWithSubpredicates: parts))
        if let type = step.type, let kind = Self.types[type.lowercased()] {
            query = app.descendants(matching: kind).matching(NSCompoundPredicate(andPredicateWithSubpredicates: parts))
        }
        return query.element(boundBy: step.nth ?? 0)
    }

    private static let types: [String: XCUIElement.ElementType] = [
        "button": .button, "textarea": .textView, "textfield": .textField, "statictext": .staticText,
        "switch": .switch, "image": .image, "cell": .cell, "link": .link,
    ]

    /// Opens or closes the sidebar through its header button (compact) or the scrim.
    private func sidebar(open: Bool, at: String) throws {
        let shown = { self.app.descendants(matching: .any)["sidebar-search"].exists }
        if shown() == open { return }
        let toggle = app.buttons["open-sidebar"]
        XCTAssertTrue(toggle.waitForExistence(timeout: 10), "\(at): no sidebar control")
        toggle.tap()
        let deadline = Date().addingTimeInterval(10)
        while shown() != open && Date() < deadline { Thread.sleep(forTimeInterval: 0.2) }
        XCTAssertEqual(shown(), open, "\(at): sidebar did not become \(open ? "open" : "closed")")
    }
}
