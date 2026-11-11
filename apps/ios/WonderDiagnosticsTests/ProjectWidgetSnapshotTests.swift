import XCTest
import AppIntents
import Combine
@testable import Wonder
import WonderPairing

final class ProjectWidgetSnapshotTests: XCTestCase {

    @MainActor func testSnapshotCannotReloadAfterFailedPersistenceOrOwnerRelease() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let saved = try shortcutConnection(host: "widget-owner", device: "phone")
        let data = try JSONEncoder().encode(SavedConnections(legacy: saved))
        var reloads = 0
        let client = ProjectWidgetSnapshotPublisher.Client(available: { true },
            load: { ProjectWidgetSnapshotStore.load(from: directory) },
            save: { ProjectWidgetSnapshotStore.save($0, in: directory) }, reload: { reloads += 1 })
        var library: ConnectionLibrary? = ConnectionLibrary(storage: .init(read: { $0 == "connections-v1" ? data : nil },
            save: { _, _ in }, forgetLegacy: {}), widgetPublisher: {
                ProjectWidgetSnapshotPublisher.publish(from: $0, refreshIfUnchanged: $1, client: client)
            })
        XCTAssertFalse(try XCTUnwrap(library).publishWidgetSnapshotNow())
        XCTAssertEqual(reloads, 0, "A failed atomic save cannot request a stale reload")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let model = try XCTUnwrap(library).model(for: saved)
        model.snapshots = [:] // schedules through the real list-revision callback
        weak var owner = library
        library = nil
        XCTAssertNil(owner)
        try await Task.sleep(for: .milliseconds(250))
        XCTAssertEqual(reloads, 0, "Pending publication belongs to the released library")
    }

    // Widget links must enter the exact paired host. A scheme or ID mix-up
    // could open the wrong app or turn a shared label into a URL path.
    func testAttentionDeepLinkIsStrictAndRoundTrips() throws {
        let url = try XCTUnwrap(WonderDeepLink.attention.url(scheme: "wonder"))
        XCTAssertEqual(url.absoluteString, "wonder://v1/attention")
        XCTAssertEqual(WonderDeepLink.parse(url, scheme: "wonder"), .attention)
        for value in ["wonder://v1/attention/", "wonder://v1/attention/x", "wonder://v1//attention",
                      "wonder://v1/attention?q=x", "wonder://v1/attention#x", "wonder://user@v1/attention",
                      "wonder://v1:42/attention", "wonder://v1/%61ttention", "other://v1/attention"] {
            XCTAssertNil(WonderDeepLink.parse(try XCTUnwrap(URL(string: value)), scheme: "wonder"), value)
        }
    }

    func testExactLinksRejectUnsafeIdentifiers() throws {
        let production = try XCTUnwrap(ProjectWidgetIdentity(bundleIdentifier: "com.swaymun.wonder"))
        let widget = try XCTUnwrap(ProjectWidgetIdentity(bundleIdentifier: "com.swaymun.wonder.Widget"))
        XCTAssertEqual(widget.appGroupID, "group.com.swaymun.wonder")
        XCTAssertNil(ProjectWidgetIdentity(bundleIdentifier: "com.saimun.wonder.native"))
        XCTAssertEqual(ProjectWidgetLink.newChat(hostID: "mac_1", projectID: "project-2", identity: production)?.absoluteString,
                       "wonder://v1/hosts/mac_1/projects/project-2/new")
        XCTAssertEqual(ProjectWidgetLink.chat(hostID: "mac_1", chatID: "chat_3", identity: widget)?.absoluteString,
                       "wonder://v1/hosts/mac_1/chats/chat_3")
        XCTAssertNil(ProjectWidgetLink.chat(hostID: "mac/other", chatID: "chat_3", identity: widget))
        XCTAssertNil(ProjectWidgetLink.newChat(hostID: "mac_1", projectID: "%2Fsecret", identity: production))
    }

    /// B-093/B-453: while iOS redacts the widget, VoiceOver speaks no thread,
    /// Project or computer name; the small widget says when it is out of date.
    func testWidgetSpeechHidesNamesWhenRedactedAndSaysWhenStale() {
        let title = "Merger plan", project = "Acquisition", host = "Studio"
        let shown = ProjectWidgetSpeech.destination(title: title, detail: project, redacted: false)
        XCTAssertEqual(shown, "Merger plan, Acquisition")
        XCTAssertEqual(ProjectWidgetSpeech.small(destination: shown, hostName: host, stale: false, redacted: false),
                       "Merger plan, Acquisition on Studio")
        XCTAssertEqual(ProjectWidgetSpeech.small(destination: nil, hostName: host, stale: true, redacted: false),
                       "Computer on Studio. Out of date. Open Cupola to refresh.")
        let hidden = ProjectWidgetSpeech.destination(title: title, detail: project, redacted: true)
        let labels = [hidden,
                      ProjectWidgetSpeech.small(destination: hidden, hostName: host, stale: true, redacted: true),
                      ProjectWidgetSpeech.small(destination: nil, hostName: host, stale: false, redacted: true),
                      ProjectWidgetSpeech.computer(hostName: host, redacted: true)]
        for label in labels {
            for name in [title, project, host] { XCTAssertFalse(label.contains(name), label) }
        }
        XCTAssertTrue(labels[1].hasSuffix(ProjectWidgetSpeech.stale), "Staleness is not private")
    }

    // An atomic valid rewrite recovers a corrupt or oversized snapshot, and
    // names are stored as the widget shows them.
    func testSnapshotStoreRecoversCorruption() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("wonder-widget-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let file = directory.appendingPathComponent("project-widget-snapshot-v3.json")
        let project = ProjectWidgetSnapshot.Project(id: "project", name: "Roadmap")
        let snapshot = ProjectWidgetSnapshot(savedAt: Date(timeIntervalSince1970: 1_000_000), hostID: "mac",
                                             hostName: "Studio", projects: [project])
        XCTAssertTrue(ProjectWidgetSnapshotStore.save(snapshot, in: directory))
        XCTAssertEqual(ProjectWidgetSnapshotStore.load(from: directory)?.projects.first?.name, "Roadmap")
        XCTAssertEqual(ProjectWidgetSnapshotStore.load(from: directory)?.hostName, "Studio")
        try Data("{".utf8).write(to: file, options: .atomic)
        XCTAssertNil(ProjectWidgetSnapshotStore.load(from: directory))
        try Data(repeating: 0, count: ProjectWidgetSnapshot.maxBytes + 1).write(to: file, options: .atomic)
        XCTAssertNil(ProjectWidgetSnapshotStore.load(from: directory))
        XCTAssertTrue(ProjectWidgetSnapshotStore.save(snapshot, in: directory))
        XCTAssertEqual(ProjectWidgetSnapshotStore.load(from: directory)?.projects.first?.name, "Roadmap")
    }

    // An intent may finish before the cold-launched shell exists. Exactly one
    // active scene receives its destination, and the pairing is checked again.
    @MainActor func testShortcutLaunchWaitsForSceneAndRejectsReplacedPairing() async throws {
        let fixture = try shortcutFixture()
        defer { UserDefaults.standard.removeObject(forKey: fixture.key) }
        let library = ConnectionLibrary(diagnosticModels: [fixture.model])
        let project = try XCTUnwrap(library.shortcutProjects().first)
        var intent = NewProjectThreadIntent()
        intent.library = library
        intent.project = project
        _ = try await intent.perform()
        let scene = UUID(), other = UUID()
        XCTAssertNil(try library.takeShortcut(scene: scene))
        XCTAssertNotNil(library.shortcutNavigation)
        library.setScene(scene, phase: .active)
        XCTAssertNil(try library.takeShortcut(scene: other))
        let request = try XCTUnwrap(library.takeShortcut(scene: scene))
        XCTAssertEqual(request.target, .newProjectChat(host: fixture.host, project: "project-0"))
        XCTAssertNil(try library.takeShortcut(scene: scene))
        try library.requestShortcut(request.address)
        fixture.model.connection = try shortcutConnection(host: fixture.host, device: "replacement")
        XCTAssertThrowsError(try library.takeShortcut(scene: scene))
        XCTAssertNil(library.shortcutNavigation)
        // An inactive owner keeps transport; the window in front opens the shortcut.
        fixture.model.connection = try shortcutConnection(host: fixture.host, device: "phone")
        library.setScene(other, phase: .active)
        library.setScene(scene, phase: .inactive)
        XCTAssertEqual(library.foregroundOwner, scene)
        try library.requestShortcut(request.address)
        XCTAssertNil(try library.takeShortcut(scene: scene))
        XCTAssertNotNil(try library.takeShortcut(scene: other))
    }

    // Fabricated or stale entities cannot switch Project or provider. Hidden
    // Projects and known archived threads stay outside the Shortcuts picker.
    @MainActor func testShortcutQueriesRejectHiddenArchivedAndForgedDestinations() async throws {
        let fixture = try shortcutFixture()
        defer { UserDefaults.standard.removeObject(forKey: fixture.key) }
        let library = ConnectionLibrary(diagnosticModels: [fixture.model])
        let project = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "hidden")
        XCTAssertThrowsError(try library.shortcutTarget(project))
        let missing = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "project-0", family: .codex, reference: "codex:missing")
        XCTAssertThrowsError(try library.shortcutTarget(missing))
        let mismatched = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "project-0", family: .claude, reference: "codex:thread")
        XCTAssertThrowsError(try library.shortcutTarget(mismatched))
        XCTAssertFalse(try library.shortcutThreads().contains { ["Archived", "Unsupported", "Invalid native"].contains($0.title) })
        let unsupported = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "project-0", family: .opencode, reference: "opencode:foreign")
        XCTAssertThrowsError(try library.shortcutTarget(unsupported))
        let archived = try XCTUnwrap(fixture.model.projects.details["archived"])
        let address = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "project-0", family: .codex, reference: "codex:archived")
        var detail = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(archived)) as? [String: Any])
        detail["isArchived"] = false
        let valid = try JSONDecoder().decode(ProjectConversationDetail.self, from: JSONSerialization.data(withJSONObject: detail))
        XCTAssertTrue(address.matches(valid, conversation: "archived"))
        for (key, value) in [("conversationId", "other"), ("projectId", "other"), ("family", "claude")] {
            var changed = detail
            changed[key] = value
            let moved = try JSONDecoder().decode(ProjectConversationDetail.self, from: JSONSerialization.data(withJSONObject: changed))
            XCTAssertFalse(address.matches(moved, conversation: "archived"))
        }
        XCTAssertFalse(address.matches(archived, conversation: "archived"))
        let query = WonderThreadQuery()
        query.library = library
        let opencode = try await query.entities(matching: "OpenCode")
        XCTAssertEqual(opencode.count, 1)
        var intent = OpenProjectThreadIntent()
        intent.library = library
        intent.target = try XCTUnwrap(opencode.first)
        _ = try await intent.perform()
        XCTAssertEqual(library.shortcutNavigation?.target,
                       .chat(host: fixture.host, id: "opencode-conversation"))
    }

    @MainActor func testAskShortcutRetainsUnconfirmedRequestAndRejectsChangedPairing() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let fixture = try shortcutFixture(root: root)
        defer {
            MessageRecoveryURLProtocol.reset(); UserDefaults.standard.removeObject(forKey: fixture.key)
            NewChatDraftStore.remove(host: fixture.host); try? FileManager.default.removeItem(at: root)
        }
        let library = ConnectionLibrary(diagnosticModels: [fixture.model])
        let address = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "project-0")
        for text in [" ", String(repeating: "é", count: 32769)] {
            do { _ = try await library.askShortcut(address, family: .codex, message: text); XCTFail("Invalid message sent") }
            catch { }
        }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects", includingEmpty: true).isEmpty)
        try enqueueAskShortcut(fixture.model, family: .codex, creates: false)
        MessageRecoveryURLProtocol.fail(path: "/api/v1/projects/project-0/threads", error: .networkConnectionLost)
        do { _ = try await library.askShortcut(address, family: .codex, message: "Keep this request"); XCTFail("Unconfirmed creation reported sent") }
        catch { }
        let pending = try XCTUnwrap(NewChatDraftStore.savedMessages(host: fixture.host).first)
        XCTAssertEqual(pending.submittedBody, "Keep this request")
        XCTAssertEqual(pending.submittedDeviceID, "phone")
        XCTAssertNil(pending.preparedConversationID)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/recovery-chat/messages").isEmpty)
        XCTAssertNil(library.shortcutNavigation)

        MessageRecoveryURLProtocol.reset()
        try enqueueAskShortcut(fixture.model, family: .codex)
        MessageRecoveryURLProtocol.hold(path: "/api/v1/projects")
        do {
            _ = try await library.askShortcut(address, family: .codex, message: "Timeout must not send", timeout: .milliseconds(40))
            XCTFail("Timed out request reported sent")
        } catch { }
        MessageRecoveryURLProtocol.releaseHeld()
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects/project-0/threads").isEmpty)
        XCTAssertEqual(NewChatDraftStore.savedMessages(host: fixture.host).map(\.requestID), [pending.requestID])

        MessageRecoveryURLProtocol.reset()
        try enqueueAskShortcut(fixture.model, family: .codex)
        do { _ = try await library.askShortcut(address, family: .claude, message: "Unavailable agent"); XCTFail("Unavailable agent sent") }
        catch { }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects/project-0/threads").isEmpty)

        MessageRecoveryURLProtocol.reset()
        try enqueueAskShortcut(fixture.model, family: .codex)
        MessageRecoveryURLProtocol.hold(path: "/api/v1/projects")
        let run = Task { @MainActor in try await library.askShortcut(address, family: .codex, message: "Do not send") }
        for _ in 0..<100 {
            if !MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects", includingEmpty: true).isEmpty { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        fixture.model.connection = try shortcutConnection(host: fixture.host, device: "replacement")
        MessageRecoveryURLProtocol.releaseHeld()
        do { _ = try await run.value; XCTFail("Replaced pairing sent") } catch { }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects/project-0/threads").isEmpty)
        XCTAssertEqual(NewChatDraftStore.savedMessages(host: fixture.host).map(\.requestID), [pending.requestID])
    }

    @MainActor private func enqueueAskShortcut(_ model: ConnectionModel, family: AgentFamily, creates: Bool = true) throws {
        let project: [String: Any] = ["id":"project-0", "name":"Current project", "isIncluded":true,
            "isPinned":false, "rootsRevision":7, "createdAt":"x", "folders":[[
                "id":"primary", "path":"/fixture", "name":"Fixture", "isPrimary":true, "isAvailable":true]]]
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", method: "GET", body: try JSONSerialization.data(withJSONObject:
            ["projects":[project], "families":[["family":family.rawValue, "available":true,
                "capabilities":ProviderCapabilities.fixtureMacJSON(family)]], "modesVersion":1]))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/agent-options", method: "GET", body: try JSONSerialization.data(withJSONObject:
            ["models":[["id":"fixture-default", "displayName":"Fixture", "agentFamily":family.rawValue, "hidden":false,
                "isDefault":true, "defaultReasoningEffort":"low", "reasoningEfforts":[["id":"low", "label":"Low"]]]],
             "allowedApprovalPolicies":[String]()]))
        let thread: [String: Any] = ["reference":"wonder:recovery-chat", "conversationId":"recovery-chat", "title":"Review",
            "family":family.rawValue, "updatedAt":1, "isPinned":false, "hasUnread":false, "isWorking":false]
        if creates {
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects/project-0/threads", method: "POST",
                body: try JSONSerialization.data(withJSONObject: ["conversation":thread]))
        }
        var detail = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(try XCTUnwrap(model.projects.details["archived"]))) as? [String: Any])
        detail["conversationId"] = "recovery-chat"; detail["family"] = family.rawValue; detail["isArchived"] = false
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", method: "GET",
            body: try JSONSerialization.data(withJSONObject: detail))
    }

    @MainActor func testProjectStatusShortcutRejectsIncompleteCatalogAndChangedPairing() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let fixture = try shortcutFixture(root: root)
        defer {
            MessageRecoveryURLProtocol.reset(); UserDefaults.standard.removeObject(forKey: fixture.key)
            try? FileManager.default.removeItem(at: root)
        }
        let library = ConnectionLibrary(diagnosticModels: [fixture.model])
        let address = ProjectShortcutAddress(host: fixture.host, device: "phone", project: "project-0")
        let path = "/api/v1/projects/project-0/threads"
        let partial = try JSONDecoder().decode(ProjectPartialFailure.self, from: Data(#"{"family":"codex","detail":"Unavailable"}"#.utf8))
        let row = ProjectThreadSummary(reference: "wonder:missing", conversationId: "missing", title: "Unavailable chat", family: .codex, updatedAt: 1)
        let oversized = (0..<301).map {
            ProjectThreadSummary(reference: "wonder:row-\($0)", conversationId: "row-\($0)", title: "Work", family: .codex, updatedAt: 1)
        }
        // A missing exact read is unknown, never idle. An empty complete Project
        // is distinct from an incomplete, looping or oversized catalog.
        for (pages, expected) in [
            ([ProjectThreadsPage(threads: [row], nextCursor: nil, partial: [])], "Current project: 0 working, 0 need input, 0 idle.\nStatus incomplete for 1 chat.\nUnavailable chat: Status unavailable" as String?),
            ([ProjectThreadsPage(threads: [], nextCursor: nil, partial: [])], "Current project: 0 working, 0 need input, 0 idle."),
            ([ProjectThreadsPage(threads: [], nextCursor: nil, partial: [partial])], nil),
            ([ProjectThreadsPage(threads: oversized, nextCursor: nil, partial: [])], nil),
            (Array(repeating: ProjectThreadsPage(threads: [], nextCursor: "repeat", partial: []), count: 2), nil),
            ((0..<10).map { ProjectThreadsPage(threads: [], nextCursor: "page-\($0)", partial: []) }, nil)
        ] {
            MessageRecoveryURLProtocol.reset()
            try enqueueProjectStatusCatalog(fixture.model)
            for page in pages { MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(page)) }
            if let expected {
                let result = try await library.shortcutProjectStatus(address)
                XCTAssertEqual(result, expected)
            }
            else {
                do { _ = try await library.shortcutProjectStatus(address); XCTFail("Incomplete catalogs cannot report all-clear") }
                catch ProjectShortcutFailure.projectStatus { }
            }
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, pages.count)
        }
        MessageRecoveryURLProtocol.reset()
        try enqueueProjectStatusCatalog(fixture.model)
        MessageRecoveryURLProtocol.hold(path: path)
        let start = ContinuousClock.now
        do { _ = try await library.shortcutProjectStatus(address, timeout: .milliseconds(30)); XCTFail("Must time out") }
        catch ProjectShortcutFailure.timedOut { }
        XCTAssertLessThan(start.duration(to: .now), .seconds(1))
        MessageRecoveryURLProtocol.reset()
        try enqueueProjectStatusCatalog(fixture.model)
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(ProjectThreadsPage(threads: [], nextCursor: nil, partial: [])))
        MessageRecoveryURLProtocol.hold(path: path)
        let run = Task { try await library.shortcutProjectStatus(address) }
        for _ in 0..<100 where MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).isEmpty {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 1)
        fixture.model.connection = try shortcutConnection(host: fixture.host, device: "replacement")
        MessageRecoveryURLProtocol.releaseHeld()
        do { _ = try await run.value; XCTFail("A replaced pairing cannot return status") } catch { }
        XCTAssertNil(library.shortcutNavigation)
    }

    @MainActor private func enqueueProjectStatusCatalog(_ model: ConnectionModel) throws {
        var project = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(try XCTUnwrap(model.projects.projects.first))) as? [String: Any])
        project["name"] = "Current project"
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: try JSONSerialization.data(withJSONObject:
            ["projects":[project], "families":[]]))
    }

    @MainActor private func shortcutConnection(host: String, device: String, origin: String = "https://camera-unit.invalid",
                                               session: String = "synthetic") throws -> SavedConnection {
        let value: [String: Any] = ["origin": origin, "hostName": "Studio",
            "credential": ["sessionToken": session, "csrfToken": "synthetic", "deviceId": device, "hostInstallationId": host]]
        return try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: value))
    }

    @MainActor private func shortcutModel(host: String, device: String, root: URL? = nil) throws -> ConnectionModel {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MessageRecoveryURLProtocol.self]
        if let root {
            return ConnectionModel(cameraFixtureStoreRoot: root, saved: try shortcutConnection(host: host, device: device),
                                   api: PairingAPI(configuration: configuration), replayEnabled: false)
        }
        return ConnectionModel(saved: try shortcutConnection(host: host, device: device), persistConnection: { _ in },
                               api: PairingAPI(configuration: configuration))
    }

    // Contract: the Project cache keeps each provider's record, so a cold
    // launch offers the same controls (Stop, attachments, access) before the
    // Mac answers; a provider the cache never saw offers nothing.
    @MainActor func testColdLaunchKeepsTheMacsProviderRecords() throws {
        let host = "records-" + UUID().uuidString
        let key = "wonder.projects." + Data((host + ":phone").utf8).base64EncodedString()
        defer { UserDefaults.standard.removeObject(forKey: key) }
        var claude = ProviderCapabilities.fixtureMac(.claude); claude.rewind = true
        let families: [[String: Any]] = [["family": "claude", "available": true,
            "capabilities": try JSONSerialization.jsonObject(with: JSONEncoder().encode(claude))]]
        UserDefaults.standard.set(try JSONSerialization.data(withJSONObject: ["projects": [], "threads": [:], "families": families]), forKey: key)
        let model = try shortcutModel(host: host, device: "phone")
        XCTAssertEqual(model.projects.capabilities(.claude), claude)
        XCTAssertEqual(model.projects.capabilities(.codex), .unoffered)
    }

    @MainActor private func shortcutFixture(root: URL? = nil) throws -> (model: ConnectionModel, key: String, host: String) {
        let host = "shortcut-" + UUID().uuidString
        let key = "wonder.projects." + Data((host + ":phone").utf8).base64EncodedString()
        var projects: [[String: Any]] = (0..<10).map { index in
            ["id": "project-\(index)", "name": "Project \(index)", "isIncluded": true, "isPinned": false,
             "rootsRevision": 1, "folders": [], "createdAt": "x"]
        }
        projects.append(["id": "hidden", "name": "Hidden", "isIncluded": false, "isPinned": false,
                         "rootsRevision": 1, "folders": [], "createdAt": "x"])
        var rows: [[String: Any]] = AgentFamily.allCases.map { family in
            ["reference": family.rawValue + ":thread", "conversationId": family.rawValue + "-conversation", "title": "Work", "family": family.rawValue, "updatedAt": 1, "isPinned": false, "hasUnread": false, "isWorking": false]
        }
        for family in [AgentFamily.codex, .claude] {
            rows.append(["reference": family.rawValue + ":abcd-1234", "title": "Native", "family": family.rawValue,
                         "updatedAt": 1, "isPinned": false, "hasUnread": false, "isWorking": false])
        }
        rows.append(["reference": "codex:not-a-session", "title": "Invalid native", "family": "codex", "updatedAt": 1,
                     "isPinned": false, "hasUnread": false, "isWorking": false])
        rows.append(["reference": "opencode:foreign", "title": "Unsupported", "family": "opencode", "updatedAt": 1,
                     "isPinned": false, "hasUnread": false, "isWorking": false])
        rows.append(["reference": "codex:archived", "conversationId": "archived", "title": "Archived", "family": "codex", "updatedAt": 1, "isPinned": false, "hasUnread": false, "isWorking": false])
        let detail: [String: Any] = ["conversationId": "archived", "projectId": "project-0", "projectName": "Project 0",
            "title": "Archived", "family": "codex", "accessMode": "workspace", "workingFolder": "/fixture",
            "workingFolderName": "Fixture", "isPinned": false, "hasUnread": false, "hasNativeSession": true,
            "isArchived": true, "folderInProject": true]
        let families = AgentFamily.allCases.map { ["family": $0.rawValue, "available": true, "capabilities": ProviderCapabilities.fixtureMacJSON($0)] }
        let cache: [String: Any] = ["projects": projects, "threads": ["project-0": rows], "details": ["archived": detail],
                                    "supportsProjects": true, "families": families]
        UserDefaults.standard.set(try JSONSerialization.data(withJSONObject: cache), forKey: key)
        return (try shortcutModel(host: host, device: "phone", root: root), key, host)
    }

}
