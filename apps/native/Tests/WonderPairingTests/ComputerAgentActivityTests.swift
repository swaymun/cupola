import XCTest
@testable import WonderPairing

/// Native tool results own app identity. Only an explicit window selection can
/// turn that identity into a capture target, and recent use expires per app.
final class ComputerAgentActivityTests: XCTestCase {

    func testWindowMenuRetainsLastGoodListUntilThreeConsecutiveFailures() {
        let source = ComputerSource(id: "app:editor#41", name: "Notes", kind: "window", appId: "editor")
        var discovery = ComputerWindowMenuDiscovery()
        discovery.recordFailure()
        XCTAssertNil(discovery.result, "Failure cannot invent a capture capability")
        discovery.receive(ComputerAvailableSources(available: true, sources: [source]))
        for failures in 1...2 {
            discovery.recordFailure()
            XCTAssertEqual(discovery.consecutiveFailures, failures)
            XCTAssertEqual(discovery.result?.windowMenuItems.map(\.id), ["app:editor#41"])
            XCTAssertTrue(discovery.result?.windowMenuItems.first?.isSelected(sourceID: source.id) == true)
        }
        discovery.receive(ComputerAvailableSources(available: true, sources: [source]))
        XCTAssertEqual(discovery.consecutiveFailures, 0, "A good poll restarts the failure budget")
        for _ in 1...3 { discovery.recordFailure() }
        XCTAssertNil(discovery.result)
        XCTAssertTrue(discovery.canShowComputer(isCurrent: true), "Expiring stale window rows cannot remove the screen being viewed")
        XCTAssertFalse(discovery.canShowComputer(isCurrent: false))
        discovery.receive(ComputerAvailableSources(available: true, sources: [source]))
        XCTAssertTrue(discovery.result?.canShowComputer == true)
        discovery.receive(ComputerAvailableSources(available: false, sources: []))
        XCTAssertFalse(discovery.result?.canShowComputer == true, "Explicit no-capture is authoritative on the first reply")
    }

    func testWindowMenuReopeningRetainsDiscoveryButNewPairingCannotInheritIt() {
        var discovery = ComputerWindowMenuDiscovery()
        discovery.begin(identity: "computer:phone")
        discovery.receive(ComputerAvailableSources(available: true, sources: []))
        discovery.recordFailure()
        discovery.begin(identity: "computer:phone")
        XCTAssertTrue(discovery.result?.canShowComputer == true)
        XCTAssertEqual(discovery.consecutiveFailures, 1, "Reopening cannot restart an outage's budget")
        for replacement in ["computer:new-phone", "other-computer:phone"] {
            discovery.begin(identity: replacement)
            XCTAssertNil(discovery.result)
            XCTAssertEqual(discovery.consecutiveFailures, 0)
            discovery.receive(ComputerAvailableSources(available: true, sources: []))
        }
    }

    private func call(_ app: String? = nil, state: String = "completed", time: String = "1000",
                      code: String = "await selectedApp.getState()", server: String = "cua_repl") -> ReadItem {
        var payload: [String: ThreadValue] = ["server": .string(server), "tool": .string("js"),
                                            "arguments": .object(["code": .string(code)])]
        if let app {
            payload["result"] = .object(["_meta": .object(["codex/toolSurface": .object([
                "kind": .string("computerUse"), "app": .object([
                    "kind": .string("appId"), "appId": .string(app)])])])])
        }
        return ReadItem(id: UUID().uuidString, type: "mcpToolCall", state: state, text: nil,
                        createdAt: time, payload: payload)
    }

    func testCodeAndPrintedExamplesCannotEstablishIdentity() {
        let code = #"cua.getApp('Safari'); sky.click({window: {app: 'com.apple.TextEdit', id: 41}})"#
        XCTAssertNil(ComputerAgentActivity.target(in: call(code: code)))
        XCTAssertNil(ComputerAgentActivity.target(in: call("bad#id")))
        XCTAssertNil(ComputerAgentActivity.target(in: call("/Applications/Notes.app")))
        XCTAssertNil(ComputerAgentActivity.target(in: call("Notes app")))
        XCTAssertNil(ComputerAgentWindowTarget(app: String(repeating: "a", count: 151)))
        XCTAssertNil(ComputerAgentWindowTarget(app: "Text\u{0007}Edit"))
    }

    /// The viewport border follows a computer call in flight, not the whole running turn.
    func testActingOnlyWhileAComputerCallIsInFlight() {
        func call(_ id: String, state: String) -> ReadItem {
            ReadItem(id: id, type: "mcpToolCall", state: state, text: nil, createdAt: "1000",
                     payload: ["server": .string("cua_repl"), "tool": .string("click")])
        }
        func activity(_ states: [String]) -> ComputerAgentActivity {
            ComputerAgentActivity.latest(in: ReadTurn(id: "t", items: states.enumerated().map { call("c\($0.offset)", state: $0.element) },
                                                       status: "inProgress"))
        }
        let between = activity(["completed"])
        XCTAssertTrue(between.isUsingComputer)
        XCTAssertFalse(between.isActing)
        XCTAssertTrue(activity(["completed", "inProgress"]).isActing)
    }

    func testClaudeCuaDriverCallsCountAsComputerUseAndNameTheLaunchedApp() {
        func driver(_ tool: String, _ arguments: [String: ThreadValue], state: String = "completed") -> ReadItem {
            ReadItem(id: UUID().uuidString, type: "mcpToolCall", state: state, text: nil, createdAt: "1000",
                     payload: ["server": .string("cua_driver"), "tool": .string("mcp__cua_driver__" + tool),
                               "arguments": .object(arguments)])
        }
        let launched = ComputerAgentActivity.latest(in: ReadTurn(
            id: "t", items: [driver("launch_app", ["bundle_id": .string("com.apple.TextEdit")]),
                             driver("type_text", ["pid": .number(93235)], state: "inProgress")], status: "inProgress"))
        XCTAssertTrue(launched.isUsingComputer)
        XCTAssertTrue(launched.isActing)
        XCTAssertEqual(launched.targets.map(\.app), ["com.apple.TextEdit"])
        let pidOnly = ComputerAgentActivity.latest(in: ReadTurn(
            id: "t", items: [driver("press_key", ["pid": .number(93235)])], status: "inProgress"))
        XCTAssertTrue(pidOnly.isUsingComputer)
        XCTAssertTrue(pidOnly.targets.isEmpty)
    }

    func testWindowSelectionBindsAppAndPositiveWindowNumber() {
        let source = ComputerSource(id: "app:com.apple.TextEdit#41", name: "TextEdit — Rear document", kind: "window", appId: "com.apple.TextEdit")
        let target = ComputerAgentWindowTarget(source: source)
        XCTAssertEqual(target?.windowID, 41)
        XCTAssertEqual(target?.sourceID, source.id)
        XCTAssertEqual(target?.displayName, source.name)
        for id in ["app:com.apple.TextEdit", "app:com.apple.TextEdit#0", "app:com.apple.TextEdit#4294967296", "app:com.apple.TextEdit#+41", "app:com.other.TextEdit#41"] {
            XCTAssertNil(ComputerAgentWindowTarget(source: ComputerSource(id: id, kind: "window", appId: "com.apple.TextEdit")), id)
        }
    }

}
