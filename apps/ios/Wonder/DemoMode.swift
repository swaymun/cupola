import CryptoKit
import SwiftUI
import Foundation
import UIKit
import WonderPairing

/// Demo mode: a local, made-up computer with two Projects and four threads, for
/// someone who has no computer paired yet (an App Store reviewer, or anyone trying the
/// app first). It ships in Release on purpose and holds only content: nothing here
/// records, uploads or depends on Diagnostics tooling.
///
/// The demo computer is served by `DemoURLProtocol` inside the app. It never opens
/// a network connection, starts model work, registers for notifications or Live
/// Activities, or writes the pairing Keychain. `ConnectionLibrary` keeps it out of
/// its persisted computers and removes it when the demo is left or a real computer is
/// paired. Entry points call `ConnectionLibrary.startDemo()`; see there.
enum DemoMode {
    static let hostID = "wonder-demo-computer"
    static let deviceID = "wonder-demo-device"
    static let origin = "https://demo.wonder.invalid"
    static let computerName = "Demo computer"
    static let hostEpoch = "demo"

    static func isDemo(_ connection: SavedConnection) -> Bool { connection.credential.hostInstallationId == hostID }
    static func isDemo(host: String?) -> Bool { host == hostID }

    /// The demo computer as a saved connection: in memory only, never stored.
    static func connection() -> SavedConnection {
        // Built through Codable so the demo uses the same type as a pairing
        // without a public memberwise initializer for credentials.
        let credential = try! JSONDecoder().decode(Credential.self, from: Data("""
        {"sessionToken":"demo","deviceId":"\(deviceID)","csrfToken":"demo","hostInstallationId":"\(hostID)","expiresAtMs":null}
        """.utf8))
        return SavedConnection(origin: origin, credential: credential, hostName: computerName)
    }

    /// A model for the demo computer. Its chats are stored in a temporary folder
    /// (removed by `ConnectionLibrary.leaveDemo()`), its requests answered by
    /// `DemoURLProtocol`, and its signing key exists only in memory.
    @MainActor static func makeModel(storeRoot: URL) -> ConnectionModel {
        DemoServer.shared.reset()
        let configuration = sessionConfiguration()
        let key = P256.Signing.PrivateKey()
        let signing = SigningIdentity(read: { key.rawRepresentation }, save: { _ in },
            restore: { _ in EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation,
                                                      sign: { try key.signature(for: $0) }) },
            create: { EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation,
                                                sign: { try key.signature(for: $0) }) })
        return ConnectionModel(demoStoreRoot: storeRoot, saved: connection(),
                               api: PairingAPI(configuration: configuration), signingIdentity: signing)
    }

    /// The demo's only transport: `DemoURLProtocol` takes every request, so none
    /// reaches the network, and nothing is cached or stored in cookies.
    static func sessionConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [DemoURLProtocol.self]
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        return configuration
    }

    static func newStoreRoot() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("wonder-demo-" + UUID().uuidString, isDirectory: true)
    }

    /// Removes what an earlier demo could have left behind if Cupola was closed
    /// while it ran: its Project catalog, drafts and temporary chat folders, and
    /// what the demo computer holds in memory (sent messages, attached files).
    @MainActor static func removeLeftovers() {
        DemoServer.shared.reset()
        let host = ProjectLibrary.cacheKeySuffix(connection())
        UserDefaults.standard.removeObject(forKey: "wonder.projects." + host)
        UserDefaults.standard.removeObject(forKey: "wonder.projects.recent." + host)
        NewChatDraftStore.remove(host: hostID)
        ManagementDraftStore(host: hostID).removeAll()
        let temporary = FileManager.default.temporaryDirectory
        if let entries = try? FileManager.default.contentsOfDirectory(at: temporary, includingPropertiesForKeys: nil) {
            for entry in entries where entry.lastPathComponent.hasPrefix("wonder-demo-") {
                try? FileManager.default.removeItem(at: entry)
            }
        }
    }

    // MARK: Scripted replies

    /// A reply the demo streams: an optional tool step, then text in parts, and
    /// in Plan mode a plan to implement.
    struct Script: Sendable {
        let command: String?
        let parts: [String]
        var plan: String? = nil
        var text: String { parts.joined() }
    }

    static func replyScript(family: AgentFamily, to body: String, planMode: Bool = false) -> Script {
        let agent = family == .claude ? "Claude" : "Codex" // provider identity
        if planMode {
            return Script(command: nil, parts: [
                "Plan mode is on, so I wrote a plan instead of changing anything. ",
                "Tap **Implement plan** to go ahead.\n\n", "This is a demo reply: on your computer, \(agent) would plan from your project.",
            ], plan: "1. Read how the app refreshes its session after waking up\n2. Share one refresh between requests that arrive together\n3. Add a test with ten requests at once\n4. Run the full test suite")
        }
        let command = family == .claude ? "npm test" : "swift test --filter TokenRefresherTests" // provider identity
        return Script(command: command, parts: [
            "This is a demo, so nothing ran ", "and your message stayed on this device. ",
            "On your own computer, \(agent) would read the project, ", "run commands like the one above ",
            "and stream its reply here as it writes.\n\n",
            "It asks before anything risky, ", "and each reply can show the files it changed. ",
            "To try it for real, leave the demo and add your computer.",
        ])
    }
}

/// The demo computer's state: what was sent and answered during this demo. Requests
/// arrive on URL loading threads, so every access takes the lock.
final class DemoServer: @unchecked Sendable {
    static let shared = DemoServer()
    private let lock = NSLock()

    /// A turn started on the phone: a message sent, or a request answered.
    struct LiveTurn {
        let id: String
        let conversation: String
        /// The sent message, if the turn started with one.
        var message: (client: String, body: String, createdAt: String, attachments: [String])?
        var script: DemoMode.Script
        /// The message moved the thread to another agent: (from, to model's name).
        var switched: (from: AgentFamily, toModel: String)? = nil
        /// The streamed reply finished.
        var finished = false
        var finishedAt = ""
        /// Stopped from the phone before the reply finished.
        var interrupted = false
    }
    /// A thread's composer choices (model, speed, access, Plan), changed from the phone.
    struct Choices {
        var model: String
        var effort: String?
        var serviceTier: String?
        var accessMode = "workspace"
        var planMode = false
        var claudeApproval: String?
    }
    private var turns: [LiveTurn] = []
    private var resolved: [String: String] = [:]
    private var read: Set<String> = []
    private var choices: [String: Choices] = [:]
    /// Renames, pins and read marks made from Chats.
    private var titles: [String: String] = [:], pins: [String: Bool] = [:], unread: [String: Bool] = [:]
    private var projectFlags: [String: [String: Bool]] = [:]
    /// Chat detail per provider. Codex starts at Thinking so its summaries show.
    private var chatDetail = DemoContent.initialChatDetail
    private var chatDetailOverrides: [String: String] = [:]
    private var goal: DemoContent.Goal? = DemoContent.initialGoal
    private var automations = DemoContent.initialAutomations()
    private var runs = DemoContent.initialRuns()
    /// Files attached from the phone, kept in memory until the demo is left.
    private var uploads: [(conversation: String, record: [String: Any], data: Data)] = []
    /// Threads a message moved to the other agent.
    private var families: [String: AgentFamily] = [:]
    private(set) var sequence: UInt64 = 1
    /// Every request path served, for tests: proves which routes the demo used.
    private var served: [String] = []
    private var started = Date()

    func reset() {
        lock.withLock {
            turns = []; resolved = [:]; read = []; choices = [:]; chatDetail = DemoContent.initialChatDetail; chatDetailOverrides = [:]
            titles = [:]; pins = [:]; unread = [:]; projectFlags = [:]; uploads = []; families = [:]
            goal = DemoContent.initialGoal; automations = DemoContent.initialAutomations(); runs = DemoContent.initialRuns()
            sequence = 1; served = []; started = Date()
        }
    }

    /// The agent a thread runs on now: its own, or the one a message moved it to.
    func family(_ thread: DemoContent.Thread) -> AgentFamily { lock.withLock { families[thread.id] ?? thread.family } }

    /// Keeps an attached file for this demo and answers with its record.
    fileprivate func upload(_ conversation: String, name: String, mimeType: String, data: Data) -> [String: Any] {
        lock.withLock {
            let stamp = DemoContent.iso(Date())
            let record: [String: Any] = ["id": "demo-upload-\(uploads.count + 1)", "kind": "attachment", "name": name, "mimeType": mimeType,
                                         "byteSize": data.count, "sha256": ConversationFile.digest(data), "state": "available",
                                         "createdAt": stamp, "updatedAt": stamp]
            uploads.append((conversation, record, data))
            return record
        }
    }
    fileprivate func uploads(_ conversation: String) -> [[String: Any]] {
        lock.withLock { uploads.filter { $0.conversation == conversation }.map(\.record) }
    }
    fileprivate func uploadData(_ conversation: String, id: String) -> (data: Data, mimeType: String)? {
        lock.withLock {
            uploads.first { $0.conversation == conversation && $0.record["id"] as? String == id }
                .map { ($0.data, $0.record["mimeType"] as? String ?? "application/octet-stream") }
        }
    }

    func choices(_ thread: DemoContent.Thread) -> Choices {
        lock.withLock { choices[thread.id] ?? DemoContent.initialChoices(families[thread.id] ?? thread.family) }
    }

    /// Saves what a PATCH sent: only the fields it names change.
    fileprivate func update(_ thread: DemoContent.Thread, with fields: [String: Any]) {
        lock.withLock {
            var next = choices[thread.id] ?? DemoContent.initialChoices(families[thread.id] ?? thread.family)
            if let model = fields["model"] as? String { next.model = model }
            if fields.keys.contains("effort") { next.effort = fields["effort"] as? String }
            if fields.keys.contains("serviceTier") { next.serviceTier = fields["serviceTier"] as? String }
            if let access = fields["accessMode"] as? String { next.accessMode = access }
            if let plan = fields["planMode"] as? Bool { next.planMode = plan }
            if fields.keys.contains("claudeApproval") { next.claudeApproval = fields["claudeApproval"] as? String }
            choices[thread.id] = next
        }
    }

    func title(_ thread: DemoContent.Thread) -> String { lock.withLock { titles[thread.id] ?? thread.title } }
    func isPinned(_ thread: DemoContent.Thread) -> Bool { lock.withLock { pins[thread.id] ?? thread.isPinned } }
    /// A conversation's title, pin and read mark, as Chats changed them.
    fileprivate func mark(_ conversation: String, with fields: [String: Any]) {
        lock.withLock {
            if let title = fields["title"] as? String, !title.isEmpty { titles[conversation] = title }
            if let pinned = fields["isPinned"] as? Bool { pins[conversation] = pinned }
            if let value = fields["hasUnread"] as? Bool { unread[conversation] = value; if !value { read.insert(conversation) } }
        }
    }
    func projectFlag(_ project: String, _ name: String, default value: Bool) -> Bool { lock.withLock { projectFlags[project]?[name] ?? value } }
    fileprivate func setProjectFlags(_ project: String, _ fields: [String: Any]) {
        lock.withLock {
            for name in ["isPinned", "isIncluded"] { if let value = fields[name] as? Bool { projectFlags[project, default: [:]][name] = value } }
        }
    }

    var chatDetailLevels: [String: String] { lock.withLock { chatDetail } }
    fileprivate func setChatDetail(_ family: String, _ level: String) { lock.withLock { chatDetail[family] = level } }
    var chatDetailChats: [String: String] { lock.withLock { chatDetailOverrides } }
    fileprivate func setChatDetail(chat: String, _ level: String?) { lock.withLock { chatDetailOverrides[chat] = level } }

    var currentGoal: DemoContent.Goal? { lock.withLock { goal } }
    fileprivate func setGoal(_ next: DemoContent.Goal?) { lock.withLock { goal = next } }

    var automationList: [[String: Any]] { lock.withLock { automations } }

    /// Creates (`id` nil) or changes an automation from the editor's fields; nil if it is gone.
    fileprivate func saveAutomation(_ id: String?, _ fields: [String: Any]) -> [String: Any]? {
        lock.withLock {
            let editable = ["name", "prompt", "rrule", "timezone", "notificationPolicy", "status"]
            var item: [String: Any]
            if let id {
                guard let index = automations.firstIndex(where: { $0["id"] as? String == id }) else { return nil }
                item = automations[index]
                item["revision"] = (item["revision"] as? Int ?? 0) + 1
            } else {
                guard let scope = fields["scopeId"] as? String, DemoContent.thread(scope) != nil else { return nil }
                item = ["id": "demo-automation-" + UUID().uuidString.prefix(8).lowercased(), "revision": 1, "scopeType": "project_thread",
                        "scopeId": scope, "lastRunAt": NSNull(), "lastAttemptAt": NSNull()]
            }
            for key in editable { if let value = fields[key] as? String { item[key] = value } }
            item["nextRunAt"] = item["status"] as? String == "paused" ? NSNull() : DemoContent.iso(Date().addingTimeInterval(20 * 60 * 60))
            if let index = automations.firstIndex(where: { $0["id"] as? String == item["id"] as? String }) { automations[index] = item }
            else { automations.append(item) }
            return item
        }
    }

    fileprivate func deleteAutomation(_ id: String) -> Bool {
        lock.withLock {
            guard let index = automations.firstIndex(where: { $0["id"] as? String == id }) else { return false }
            automations.remove(at: index)
            runs[id] = nil
            return true
        }
    }

    /// Run now: the demo records a run that finishes at once.
    fileprivate func runAutomation(_ id: String) -> [String: Any]? {
        lock.withLock {
            guard let index = automations.firstIndex(where: { $0["id"] as? String == id }) else { return nil }
            let started = DemoContent.iso(Date())
            automations[index]["lastRunAt"] = started
            automations[index]["lastAttemptAt"] = started
            let run = DemoContent.run(id: "\(id)-run-\((runs[id]?.count ?? 0) + 1)", automation: id, at: Date())
            runs[id, default: []].insert(run, at: 0)
            var running = run; running["status"] = "running"; running["finishedAt"] = NSNull()
            return running
        }
    }

    fileprivate func automationRuns(_ id: String) -> [[String: Any]] { lock.withLock { runs[id] ?? [] } }

    var servedPaths: [String] { lock.withLock { served } }
    var currentSequence: UInt64 { lock.withLock { sequence } }
    func advance(to value: UInt64) { lock.withLock { sequence = max(sequence, value) } }

    /// The reply still to stream in a turn the phone started, if any.
    func pendingScript(conversation: String, turn: String) -> DemoMode.Script? {
        lock.withLock { turns.first { $0.id == turn && $0.conversation == conversation && !$0.finished }?.script }
    }

    func finish(conversation: String, turn: String) {
        lock.withLock {
            guard let index = turns.firstIndex(where: { $0.id == turn && $0.conversation == conversation }) else { return }
            turns[index].finished = true
            turns[index].finishedAt = DemoContent.now()
        }
    }

    /// Stop on the phone: the turn ends without its reply.
    fileprivate func interrupt(conversation: String, turn: String) -> Bool {
        lock.withLock {
            guard let index = turns.firstIndex(where: { $0.id == turn && $0.conversation == conversation && !$0.finished }) else {
                // A turn waiting on a request: stopping it dismisses the request.
                // The goal thread's running turn just stops.
                if let request = DemoContent.requests.first(where: { $0.turn == turn && $0.conversation == conversation }) {
                    guard resolved[request.id] == nil else { return false }
                    resolved[request.id] = "cancel"
                } else {
                    guard DemoContent.runningTurns[conversation] == turn,
                          !turns.contains(where: { $0.id == turn && $0.conversation == conversation }) else { return false }
                }
                var stopped = LiveTurn(id: turn, conversation: conversation, message: nil, script: DemoMode.Script(command: nil, parts: []))
                stopped.finished = true; stopped.interrupted = true; stopped.finishedAt = DemoContent.now()
                turns.append(stopped)
                return true
            }
            turns[index].finished = true
            turns[index].interrupted = true
            turns[index].finishedAt = DemoContent.now()
            return true
        }
    }

    fileprivate func record(_ path: String) { lock.withLock { served.append(path) } }

    /// A message from the phone. A model of the other agent (the composer's
    /// picker) moves the thread to that agent before it replies, as a computer does.
    fileprivate func send(conversation: String, client: String, body: String, attachments: [String],
                          model next: (family: AgentFamily, model: String, effort: String?)?) {
        lock.withLock {
            guard let thread = DemoContent.thread(conversation) else { return }
            var family = families[thread.id] ?? thread.family
            var switched: (from: AgentFamily, toModel: String)?
            if let next, next.family != family, [.codex, .claude].contains(next.family) { // provider identity: the demo's two agents
                switched = (family, DemoContent.modelName(next.model))
                family = next.family
                families[thread.id] = family
                var moved = DemoContent.initialChoices(family)
                moved.model = next.model; moved.effort = next.effort ?? moved.effort
                moved.planMode = choices[thread.id]?.planMode ?? false
                choices[thread.id] = moved
            }
            let planMode = choices[thread.id]?.planMode ?? false
            turns.append(LiveTurn(id: "demo-sent-\(turns.count + 1)", conversation: conversation,
                                  message: (client, body, DemoContent.now(), attachments),
                                  script: DemoMode.replyScript(family: family, to: body, planMode: planMode), switched: switched))
        }
    }

    fileprivate func resolve(_ approval: String, decision: String, answer: String?) -> Bool {
        lock.withLock {
            guard resolved[approval] == nil, let request = DemoContent.requests.first(where: { $0.id == approval }) else { return false }
            resolved[approval] = decision
            let script = DemoContent.continuation(for: request, decision: decision, answer: answer)
            turns.append(LiveTurn(id: request.turn, conversation: request.conversation, message: nil, script: script))
            return true
        }
    }

    fileprivate func markRead(_ conversation: String) { lock.withLock { read.insert(conversation); unread[conversation] = false } }

    fileprivate func pendingRequests() -> [DemoContent.Request] {
        lock.withLock { DemoContent.requests.filter { resolved[$0.id] == nil } }
    }

    fileprivate func liveTurns(_ conversation: String) -> [LiveTurn] {
        lock.withLock { turns.filter { $0.conversation == conversation } }
    }

    fileprivate func isWorking(_ conversation: String) -> Bool {
        lock.withLock {
            turns.contains { $0.conversation == conversation && !$0.finished }
                || DemoContent.requests.contains { $0.conversation == conversation && resolved[$0.id] == nil }
                || DemoContent.runningTurns[conversation].map { turn in
                    !turns.contains { $0.id == turn && $0.conversation == conversation }
                } == true
        }
    }

    fileprivate func hasUnread(_ conversation: String) -> Bool {
        lock.withLock { unread[conversation] ?? (!read.contains(conversation) && DemoContent.unreadAtStart.contains(conversation)) }
    }

    fileprivate var startedAt: Date { lock.withLock { started } }
}

/// The demo's Projects, threads and requests, shaped as a computer's Cupola answers.
enum DemoContent {
    struct Thread {
        let id: String
        let reference: String
        let project: String
        let title: String
        let family: AgentFamily
        let isPinned: Bool
        let minutesAgo: Int
    }
    struct Request {
        let id: String
        let conversation: String
        let turn: String
        let isQuestion: Bool
    }

    static let weather = (id: "demo-weather", name: "Weather", folder: "/Users/demo/Code/Weather")
    static let website = (id: "demo-website", name: "Website", folder: "/Users/demo/Code/Website")
    static let signOut = Thread(id: "demo-codex-sign-out", reference: "codex:demo-sign-out", project: weather.id,
                                title: "Fix sign-out after wake", family: .codex, isPinned: true, minutesAgo: 4)
    static let release = Thread(id: "demo-codex-release", reference: "codex:demo-release", project: weather.id,
                                title: "Push the beta branch", family: .codex, isPinned: false, minutesAgo: 9)
    static let darkMode = Thread(id: "demo-claude-dark-mode", reference: "claude:demo-dark-mode", project: website.id,
                                 title: "Add a dark mode toggle", family: .claude, isPinned: false, minutesAgo: 2)
    /// A goal thread: Codex keeps working toward the goal, with two agent tasks.
    static let speed = Thread(id: "demo-codex-speed", reference: "codex:demo-speed", project: weather.id,
                              title: "Open the forecast in under a second", family: .codex, isPinned: false, minutesAgo: 1)
    static let threads = [signOut, speed, release, darkMode]
    static let unreadAtStart: Set<String> = [darkMode.id]

    static let requests = [
        Request(id: "demo-push-approval", conversation: release.id, turn: "demo-release-turn-2", isQuestion: false),
        Request(id: "demo-toggle-question", conversation: darkMode.id, turn: "demo-dark-turn-2", isQuestion: true),
    ]
    /// Turns still running when the demo opens, waiting on nothing but the agent.
    static let runningTurns = [speed.id: "demo-speed-turn-2"]

    static func thread(_ id: String) -> Thread? { threads.first { $0.id == id } }
    static func now() -> String { String(Int(Date().timeIntervalSince1970 * 1000)) }

    // MARK: Choices, chat detail and the goal

    static let codexModel = "gpt-6.1-sol", claudeModel = "claude-opus-5-5" // provider identity
    static func initialChoices(_ family: AgentFamily) -> DemoServer.Choices {
        family == .claude // provider identity: each agent's own demo model
            ? DemoServer.Choices(model: claudeModel, effort: "high", claudeApproval: "ask")
            : DemoServer.Choices(model: codexModel, effort: "medium", serviceTier: "default")
    }
    static let initialChatDetail = ["codex": "thinking", "claude": "everything"]

    /// The models each agent offers, as `/api/v1/agent-options` lists them.
    static func agentOptions() -> [String: Any] {
        func efforts(_ ids: [String]) -> [[String: Any]] {
            ids.map { ["id": $0, "label": $0 == "xhigh" ? "Extra high" : $0.prefix(1).uppercased() + $0.dropFirst()] }
        }
        let tiers: [[String: Any]] = [["id": "default", "label": "Standard", "description": "Standard speed"],
                                      ["id": "fast", "label": "Fast", "description": "Faster replies, uses more of your plan"]]
        return ["models": [
            ["id": codexModel, "displayName": "GPT-6.1 Sol", "hidden": false, "agentFamily": "codex", "isDefault": true,
             "defaultReasoningEffort": "medium", "reasoningEfforts": efforts(["low", "medium", "high", "xhigh"]),
             "serviceTiers": tiers, "defaultServiceTier": "default"],
            ["id": "gpt-6.1-mini", "displayName": "GPT-6.1 Mini", "hidden": false, "agentFamily": "codex", "isDefault": false,
             "defaultReasoningEffort": "medium", "reasoningEfforts": efforts(["low", "medium", "high"])],
            ["id": claudeModel, "displayName": "Claude Opus 5.5", "hidden": false, "agentFamily": "claude", "isDefault": true,
             "defaultReasoningEffort": "high", "reasoningEfforts": efforts(["low", "medium", "high"])],
            ["id": "claude-sonnet-5-5", "displayName": "Claude Sonnet 5.5", "hidden": false, "agentFamily": "claude", "isDefault": false,
             "defaultReasoningEffort": "medium", "reasoningEfforts": efforts(["low", "medium", "high"])],
        ] as [[String: Any]], "allowedApprovalPolicies": [Any](), "timezone": TimeZone.current.identifier]
    }

    static func chatDetail() -> [String: Any] {
        ["providers": DemoServer.shared.chatDetailLevels.sorted { $0.key < $1.key }.map { ["family": $0.key, "detail": $0.value] },
         "chats": DemoServer.shared.chatDetailChats]
    }

    struct Goal {
        var objective: String
        var status: String
        var tokenBudget: Int?
        var timeBudgetSeconds: Int?
    }
    static let initialGoal: Goal? = Goal(objective: "Open the forecast screen in under one second on a cold start.",
                                         status: "active", tokenBudget: 400_000, timeBudgetSeconds: 2 * 60 * 60)

    static func goal() -> [String: Any] {
        guard let goal = DemoServer.shared.currentGoal else { return ["goal": NSNull()] }
        return ["goal": ["objective": goal.objective, "status": goal.status,
                         "createdAt": Int(DemoServer.shared.startedAt.timeIntervalSince1970) - 24 * 60,
                         "tokenBudget": goal.tokenBudget.map { $0 as Any } ?? NSNull(), "tokensUsed": 126_400,
                         "timeBudgetSeconds": goal.timeBudgetSeconds.map { $0 as Any } ?? NSNull(), "timeUsedSeconds": 22 * 60]]
    }

    /// What the context ring shows: how full each thread's context is.
    static func activity(_ thread: Thread) -> [String: Any] {
        let latest = (snapshot(thread.id)?["thread"] as? [String: Any])?["turns"] as? [[String: Any]]
        let used = [signOut.id: 61_800, speed.id: 148_300, release.id: 23_900, darkMode.id: 92_400][thread.id] ?? 20_000
        return ["latestTurnId": latest?.last?["id"] ?? NSNull(), "latestTurnStatus": latest?.last?["status"] as? String ?? "completed",
                "latestTurnItems": (latest?.last?["items"] as? [Any])?.count ?? 0,
                "runningElsewhere": false, "openElsewhere": false, "tasksOnMac": false,
                "contextUsage": ["usedTokens": used, "windowTokens": DemoServer.shared.family(thread) == .claude ? 200_000 : 258_400]] // provider identity
    }

    /// Plan limits behind the context ring.
    static func usage(family: String) -> [String: Any] {
        let start = DemoServer.shared.startedAt.timeIntervalSince1970
        return ["agentFamily": family, "checkedAtMs": UInt64(start * 1000), "planType": family == "claude" ? "max" : "pro", // provider identity
                "windows": [["id": "primary", "label": "5 hours", "usedPercent": 18, "remainingPercent": 82,
                             "windowDurationMins": 300, "resetsAt": UInt64(start) + 3 * 60 * 60],
                            ["id": "secondary", "label": "Week", "usedPercent": 41, "remainingPercent": 59,
                             "windowDurationMins": 7 * 24 * 60, "resetsAt": UInt64(start) + 4 * 24 * 60 * 60]]]
    }

    /// Times relative to when the demo started, so the threads read as recent.
    private static func time(_ minutesAgo: Int, plus seconds: Int = 0) -> String {
        let start = DemoServer.shared.startedAt.timeIntervalSince1970
        return String(Int((start - Double(minutesAgo * 60) + Double(seconds)) * 1000))
    }

    // MARK: Catalog

    static func projects() -> [String: Any] {
        func project(_ value: (id: String, name: String, folder: String)) -> [String: Any] {
            let server = DemoServer.shared
            return ["id": value.id, "name": value.name, "isIncluded": server.projectFlag(value.id, "isIncluded", default: true),
             "isPinned": server.projectFlag(value.id, "isPinned", default: false), "rootsRevision": 1, "createdAt": "demo",
             "folders": [["id": value.id + "-folder", "path": value.folder, "name": URL(fileURLWithPath: value.folder).lastPathComponent,
                          "isPrimary": true, "isAvailable": true]]]
        }
        // No archiveVersion: archiving needs the computer's own history, so the demo leaves it out.
        return ["skillsVersion": 1, "modesVersion": 1,
                "projects": [project(weather), project(website)],
                "families": [AgentFamily.codex, .claude].map { ["family": $0.rawValue, "available": true, "capabilities": capabilities($0)] },
                "pinned": threads.filter { DemoServer.shared.isPinned($0) }.map { ["projectId": $0.project, "thread": summary($0)] }]
    }

    /// What each agent offers on the demo computer: a computer's record, less
    /// what the demo cannot answer (Send now, Fork, skills, connected apps,
    /// sessions started on a computer), so those controls do not show.
    static func capabilities(_ family: AgentFamily) -> Any {
        var record = ProviderCapabilities.fixtureMac(family)
        record.steer = false; record.agentSteer = false; record.nativeFork = false; record.rewind = false
        record.nativeSessions = false; record.desktopProjectSync = false; record.threadTools = false
        record.skills = false; record.slashCommands = false; record.fileMentions = false; record.promptSuggestions = false
        record.imageGeneration = false; record.connectedApps = false; record.wonderSkills = false; record.accounts = false
        return (try? JSONEncoder().encode(record)).flatMap { try? JSONSerialization.jsonObject(with: $0) } ?? NSNull()
    }

    static func modelName(_ id: String) -> String {
        let models = agentOptions()["models"] as? [[String: Any]] ?? []
        return models.first { $0["id"] as? String == id }?["displayName"] as? String ?? id
    }

    /// Thread > Continue on computer: the command that resumes the thread in a terminal.
    static func continuation(_ thread: Thread) -> [String: Any] {
        let folder = (thread.project == website.id ? website : weather).folder
        let session = thread.reference.split(separator: ":").last.map(String.init) ?? thread.id
        let option: [String: Any] = DemoServer.shared.family(thread) == .claude // provider identity
            ? ["id": "claude-code", "title": "Continue in Claude Code", "detail": "Resumes this exact session in Terminal.",
               "command": "cd \(folder) && claude --resume \(session)"]
            : ["id": "codex-cli", "title": "Continue in Codex CLI", "detail": "Resumes this exact thread in Terminal.",
               "command": "cd \(folder) && codex resume \(session)"]
        return ["options": [option], "notes": [String]()]
    }

    static func threads(project: String) -> [String: Any] {
        ["threads": threads.filter { $0.project == project }.map(summary), "nextCursor": NSNull(), "partial": [Any]()]
    }

    static func summary(_ thread: Thread) -> [String: Any] {
        let server = DemoServer.shared
        let latest = server.liveTurns(thread.id).count
        return ["reference": thread.reference, "conversationId": thread.id, "title": server.title(thread), "family": server.family(thread).rawValue,
                "updatedAt": Int(server.startedAt.timeIntervalSince1970) - thread.minutesAgo * 60 + latest,
                "isPinned": server.isPinned(thread), "hasUnread": server.hasUnread(thread.id), "isWorking": server.isWorking(thread.id)]
    }

    static func detail(_ thread: Thread) -> [String: Any] {
        let project = thread.project == weather.id ? weather : website
        let chosen = DemoServer.shared.choices(thread)
        func value(_ text: String?) -> Any { text.map { $0 as Any } ?? NSNull() }
        return ["conversationId": thread.id, "projectId": project.id, "projectName": project.name, "title": DemoServer.shared.title(thread),
                "family": DemoServer.shared.family(thread).rawValue, "model": chosen.model, "effort": value(chosen.effort),
                "serviceTier": value(chosen.serviceTier), "accessMode": chosen.accessMode,
                "workingFolder": project.folder, "workingFolderName": URL(fileURLWithPath: project.folder).lastPathComponent,
                "isPinned": DemoServer.shared.isPinned(thread), "hasUnread": DemoServer.shared.hasUnread(thread.id),
                // No native session: the demo has no provider history to re-read before a send.
                "hasNativeSession": false, "folderInProject": true, "claudeApproval": value(chosen.claudeApproval),
                "planMode": chosen.planMode, "isArchived": false]
    }

    // MARK: Requests

    static func attention() -> [[String: Any]] {
        DemoServer.shared.pendingRequests().map { request in
            if request.isQuestion {
                return ["approvalId": request.id, "conversationId": request.conversation, "method": "item/tool/requestUserInput",
                        "actionNonce": request.id + "-nonce",
                        "params": ["threadId": request.conversation + "-thread", "turnId": request.turn, "isBlocking": false,
                                   "questions": [["id": "placement", "question": "Where should the dark mode toggle go?",
                                                  "options": [["label": "Header"], ["label": "Settings page"]]]]]]
            }
            return ["approvalId": request.id, "conversationId": request.conversation, "method": "item/commandExecution/requestApproval",
                    "actionNonce": request.id + "-nonce",
                    "params": ["threadId": request.conversation + "-thread", "turnId": request.turn,
                               "command": "git push -u origin fix/token-refresh",
                               "reason": "Push the branch so the beta build can start.",
                               "availableDecisions": ["accept", "decline"]]]
        }
    }

    /// How the agent continues once a request is answered on the phone.
    static func continuation(for request: Request, decision: String, answer: String?) -> DemoMode.Script {
        let accepted = !["decline", "cancel", "skip"].contains(decision)
        if request.isQuestion {
            let place = answer.flatMap { $0.isEmpty ? nil : $0 } ?? "header"
            return DemoMode.Script(command: accepted ? "npm run build" : nil, parts: accepted ? [
                "Thanks. The toggle goes in the ", "\(place.lowercased()), ", "and it remembers the choice between visits. ",
                "The build passes.\n\n", "This is a demo reply: on your computer, Claude would make the change now.",
            ] : ["Skipped. I'll leave the toggle out for now. ", "This is a demo reply."])
        }
        return accepted
            ? DemoMode.Script(command: "git push -u origin fix/token-refresh", parts: [
                "Pushed `fix/token-refresh`. ", "The beta build starts from this branch.\n\n",
                "This is a demo reply: nothing was pushed anywhere."])
            : DemoMode.Script(command: nil, parts: ["OK, I won't push. ", "The branch stays on your computer. ", "This is a demo reply."])
    }

    // MARK: Conversations

    // MARK: Automations

    static func iso(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.string(from: date)
    }
    static func run(id: String, automation: String, at date: Date) -> [String: Any] {
        ["id": id, "automationId": automation, "scheduledFor": "manual", "status": "completed", "startedAt": iso(date),
         "finishedAt": iso(date.addingTimeInterval(95)), "error": NSNull(), "messageId": NSNull(),
         "conversationId": automation == "demo-automation-tests" ? signOut.id : darkMode.id]
    }
    /// Two automations: a daily test run (it ran this morning) and a paused weekly check.
    static func initialAutomations() -> [[String: Any]] {
        let morning = Calendar.current.date(bySettingHour: 9, minute: 0, second: 0, of: Date()) ?? Date()
        let lastRun = morning > Date() ? morning.addingTimeInterval(-24 * 60 * 60) : morning
        return [
            ["id": "demo-automation-tests", "revision": 1, "name": "Morning test run", "scopeType": "project_thread",
             "scopeId": signOut.id, "prompt": "Run the full test suite and tell me about anything that fails.",
             "rrule": "FREQ=DAILY;BYHOUR=9;BYMINUTE=0", "timezone": TimeZone.current.identifier, "notificationPolicy": "failed_runs_only",
             "status": "active", "nextRunAt": iso(lastRun.addingTimeInterval(24 * 60 * 60)), "lastRunAt": iso(lastRun),
             "lastAttemptAt": iso(lastRun)],
            ["id": "demo-automation-packages", "revision": 1, "name": "Weekly package check", "scopeType": "project_thread",
             "scopeId": darkMode.id, "prompt": "Check for outdated npm packages and list the updates that are safe to take.",
             "rrule": "FREQ=WEEKLY;BYDAY=MO;BYHOUR=8;BYMINUTE=30", "timezone": TimeZone.current.identifier,
             "notificationPolicy": "all_runs", "status": "paused", "nextRunAt": NSNull(), "lastRunAt": NSNull(), "lastAttemptAt": NSNull()],
        ]
    }
    static func initialRuns() -> [String: [[String: Any]]] {
        let morning = Calendar.current.date(bySettingHour: 9, minute: 0, second: 0, of: Date()) ?? Date()
        let lastRun = morning > Date() ? morning.addingTimeInterval(-24 * 60 * 60) : morning
        return ["demo-automation-tests": [run(id: "demo-automation-tests-run-1", automation: "demo-automation-tests", at: lastRun)]]
    }

    // MARK: Search

    /// `text` around its first match of `query`, the match marked as search snippets mark it.
    private static func snippet(_ text: String, _ query: String) -> String? {
        let flat = text.replacingOccurrences(of: "\n", with: " ")
        guard let match = flat.range(of: query, options: [.caseInsensitive, .diacriticInsensitive]) else { return nil }
        let start = flat.index(match.lowerBound, offsetBy: -40, limitedBy: flat.startIndex) ?? flat.startIndex
        let end = flat.index(match.upperBound, offsetBy: 60, limitedBy: flat.endIndex) ?? flat.endIndex
        func clean(_ part: Substring) -> String { part.replacingOccurrences(of: "<", with: "‹").replacingOccurrences(of: ">", with: "›") }
        return (start > flat.startIndex ? "…" : "") + clean(flat[start..<match.lowerBound]) + "<mark>" + clean(flat[match])
            + "</mark>" + clean(flat[match.upperBound..<end]) + (end < flat.endIndex ? "…" : "")
    }

    /// Sidebar search: the demo threads' messages and replies that contain the words.
    static func search(_ query: String) -> [String: Any] {
        let words = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard words.count >= 2 else { return ["results": [Any](), "nextCursor": NSNull(), "partial": false] }
        var results: [[String: Any]] = []
        for thread in threads {
            guard let snapshot = snapshot(thread.id) else { continue }
            for message in snapshot["messages"] as? [[String: Any]] ?? [] {
                guard let body = message["body"] as? String, let found = snippet(body, words) else { continue }
                results.append(["kind": "message", "id": message["messageId"] ?? "", "title": DemoServer.shared.title(thread), "snippet": found,
                                "conversationId": thread.id, "updatedAt": message["createdAt"] ?? now()])
            }
            let turns = (snapshot["thread"] as? [String: Any])?["turns"] as? [[String: Any]] ?? []
            for item in turns.flatMap({ $0["items"] as? [[String: Any]] ?? [] }) where item["type"] as? String == "agentMessage" {
                guard let text = item["text"] as? String, let found = snippet(text, words) else { continue }
                results.append(["kind": "native_message", "id": item["id"] ?? "", "title": DemoServer.shared.title(thread), "snippet": found,
                                "conversationId": thread.id, "updatedAt": item["createdAt"] ?? now(), "projectId": thread.project,
                                "reference": thread.reference, "family": DemoServer.shared.family(thread).rawValue, "itemId": item["id"] ?? ""])
            }
        }
        return ["results": Array(results.prefix(20)), "nextCursor": NSNull(), "partial": false]
    }

    static func occurrences(_ conversation: String, _ query: String) -> [String: Any] {
        let turns = (snapshot(conversation)?["thread"] as? [String: Any])?["turns"] as? [[String: Any]] ?? []
        let found: [[String: Any]] = turns.flatMap { turn in
            (turn["items"] as? [[String: Any]] ?? []).compactMap { item in
                guard let text = item["text"] as? String, let match = snippet(text, query) else { return nil }
                return ["itemId": item["id"] ?? "", "turnId": turn["id"] ?? "", "snippet": match]
            }
        }
        return ["supported": true, "occurrences": found]
    }

    // MARK: Agent tasks

    static let scout = (thread: "demo-speed-scout", name: "Scout", role: "research")
    static let builder = (thread: "demo-speed-builder", name: "Builder", role: "implementation")

    static func subagent(_ task: (thread: String, name: String, role: String)) -> [String: Any] {
        let running = task.thread == scout.thread && DemoServer.shared.isWorking(speed.id)
        var row: [String: Any] = ["parentConversationId": speed.id, "threadId": task.thread, "title": task.name,
                                  "agentNickname": task.name, "agentRole": task.role, "status": running ? "running" : "completed",
                                  "isArchived": false, "canAcceptDirectInput": false, "namedSubagents": true,
                                  "updatedAt": time(task.thread == scout.thread ? 0 : 10)]
        if running { row["progressSummary"] = "Profiling the forecast request" }
        return row
    }

    static func subagents(_ conversation: String) -> [String: Any] {
        ["available": true, "detail": NSNull(), "subagents": conversation == speed.id ? [subagent(scout), subagent(builder)] : [],
         "nextCurrentCursor": NSNull(), "nextArchivedCursor": NSNull()]
    }

    static func transcript(_ conversation: String, task id: String) -> [String: Any]? {
        guard conversation == speed.id, let task = [scout, builder].first(where: { $0.thread == id }) else { return nil }
        let ago = speed.minutesAgo + 23
        let items: [[String: Any]] = task.thread == scout.thread ? [
            command("demo-scout-c1", "rg -n \"forecast(for:\" Sources", at: time(ago, plus: 10),
                    output: "Sources/Forecast/ForecastAPI.swift:31:    func forecast(for location: Location) async throws -> Forecast"),
            reply("demo-scout-r1", "The forecast request waits for a fresh location fix first. Starting with the last known location saves about 600 ms.", at: time(ago, plus: 400)),
        ] : [
            command("demo-builder-c1", "swift test --filter ForecastStoreTests", at: time(ago, plus: 700),
                    output: "Executed 12 tests, with 0 failures (0 unexpected) in 0.9 seconds"),
            reply("demo-builder-r1", "`ForecastStore` now keeps the last forecast on disk and shows it while a fresh one loads. 12 tests pass.", at: time(ago, plus: 820)),
        ]
        let running = task.thread == scout.thread && DemoServer.shared.isWorking(speed.id)
        return ["subagent": subagent(task), "snapshot": [
            "conversationId": "project-agent:\(speed.id):\(task.thread)", "hostEpoch": DemoMode.hostEpoch, "lastSequence": 0,
            "messages": [Any](), "assistantMessages": [Any](),
            "thread": ["threadId": task.thread, "nextCursor": NSNull(), "hydrated": true, "turns": [[
                "id": task.thread + "-turn", "status": running ? "inProgress" : "completed", "createdAt": time(ago), "items": items]]]]]
    }

    static let screenshotID = "demo-sign-in-screenshot"
    /// An iPhone screen showing the sign-out bug, drawn once per launch.
    static let screenshot: Data = UIGraphicsImageRenderer(size: CGSize(width: 300, height: 520), format: {
        let format = UIGraphicsImageRendererFormat(); format.scale = 2; return format
    }()).pngData { context in
        let bounds = CGRect(x: 0, y: 0, width: 300, height: 520)
        UIColor(red: 0.09, green: 0.32, blue: 0.62, alpha: 1).setFill(); context.fill(bounds)
        UIColor.white.withAlphaComponent(0.15).setFill()
        UIBezierPath(roundedRect: CGRect(x: 24, y: 150, width: 252, height: 200), cornerRadius: 18).fill()
        let title: [NSAttributedString.Key: Any] = [.font: UIFont.systemFont(ofSize: 22, weight: .semibold), .foregroundColor: UIColor.white]
        let body: [NSAttributedString.Key: Any] = [.font: UIFont.systemFont(ofSize: 15), .foregroundColor: UIColor.white.withAlphaComponent(0.9)]
        ("Weather" as NSString).draw(at: CGPoint(x: 24, y: 60), withAttributes: title)
        ("Session expired" as NSString).draw(at: CGPoint(x: 44, y: 176), withAttributes: title)
        ("You were signed out.\nSign in again to see\nyour forecast." as NSString)
            .draw(in: CGRect(x: 44, y: 216, width: 220, height: 80), withAttributes: body)
        UIColor.white.setFill()
        UIBezierPath(roundedRect: CGRect(x: 44, y: 300, width: 212, height: 34), cornerRadius: 17).fill()
        ("Sign in" as NSString).draw(at: CGPoint(x: 122, y: 307), withAttributes: [.font: UIFont.systemFont(ofSize: 16, weight: .semibold),
                                                                                  .foregroundColor: UIColor(red: 0.09, green: 0.32, blue: 0.62, alpha: 1)])
    }

    /// The thread's files: the sign-out screenshot, then anything attached during the demo.
    static func files(_ conversation: String) -> [[String: Any]] {
        let attached = DemoServer.shared.uploads(conversation)
        guard conversation == signOut.id else { return attached }
        return [["id": screenshotID, "kind": "attachment", "name": "Signed out.png", "mimeType": "image/png",
                 "byteSize": screenshot.count, "sha256": ConversationFile.digest(screenshot), "state": "available",
                 "createdAt": "2026-10-15T09:00:00Z", "updatedAt": "2026-10-15T09:00:00Z"]] + attached
    }

    private static func command(_ id: String, _ command: String, at time: String, output: String = "") -> [String: Any] {
        ["id": id, "type": "commandExecution", "state": "completed", "createdAt": time,
         "payload": ["command": command, "exitCode": 0, "output": output]]
    }
    private static func reply(_ id: String, _ text: String, at time: String) -> [String: Any] {
        ["id": id, "type": "agentMessage", "state": "completed", "createdAt": time, "text": text, "payload": ["phase": "final_answer"]]
    }
    /// A thinking summary. Finished ones read "Thought for Ns"; a running one
    /// (`seconds` nil) counts up live.
    private static func thinking(_ id: String, _ text: String, at time: String, seconds: Int?) -> [String: Any] {
        let start = Int(time) ?? 0
        var timing: [String: Any] = ["startedAtMs": start, "hostEpoch": DemoMode.hostEpoch]
        if let seconds { timing["finished"] = true; timing["completedAtMs"] = start + seconds * 1000 }
        return ["id": id, "type": "reasoning", "state": seconds == nil ? "started" : "completed", "createdAt": time, "text": text,
                "payload": ["thinkingTiming": timing]]
    }
    private static func search(_ id: String, _ query: String, results: Int, at time: String) -> [String: Any] {
        ["id": id, "type": "webSearch", "state": "completed", "createdAt": time, "payload": ["query": query, "resultCount": results]]
    }
    private static func agentTask(_ id: String, _ task: (thread: String, name: String, role: String), kind: String, at time: String) -> [String: Any] {
        ["id": id, "type": "subAgentActivity", "state": kind == "started" ? "started" : "completed", "createdAt": time,
         "payload": ["agentThreadId": task.thread, "agentNickname": task.name, "agentRole": task.role, "kind": kind,
                     "status": kind == "started" ? "running" : "completed"]]
    }
    private static func diff(_ path: String, _ hunk: String) -> [String: Any] {
        let lines = hunk.split(separator: "\n")
        return ["path": path, "kind": "update", "diff": hunk,
                "additions": lines.filter { $0.hasPrefix("+") }.count, "deletions": lines.filter { $0.hasPrefix("-") }.count]
    }
    private static func message(_ id: String, _ body: String, turn: String, conversation: String, at time: String,
                                attachments: [String] = []) -> [String: Any] {
        ["messageId": id, "body": body, "state": "completed", "codexTurnId": turn, "codexThreadId": conversation + "-thread",
         "createdAt": time, "attachmentIds": attachments]
    }

    static func snapshot(_ conversation: String) -> [String: Any]? {
        guard let thread = thread(conversation) else { return nil }
        var (messages, turns) = history(thread)
        let assistant: [[String: Any]] = []
        for live in DemoServer.shared.liveTurns(conversation) {
            if let sent = live.message {
                messages.append(["messageId": "message-" + live.id, "clientMessageId": sent.client, "body": sent.body,
                                 "state": live.finished ? "completed" : "streaming", "codexTurnId": live.id,
                                 "codexThreadId": conversation + "-thread", "createdAt": sent.createdAt, "attachmentIds": sent.attachments])
            }
            if let switched = live.switched, let sent = live.message {
                // The move to the other agent, as its own row before the reply.
                let id = "demo-switch-" + live.id
                turns.append(["id": id, "status": "completed", "createdAt": sent.createdAt, "items": [[
                    "id": id, "type": "providerSwitch", "state": "completed", "createdAt": sent.createdAt,
                    "text": "Switched to \(switched.toModel) · \(switched.from.title) history was handed over",
                    "payload": ["fromFamily": switched.from.rawValue, "toFamily": DemoServer.shared.family(thread).rawValue]]]])
            }
            let existing = turns.first { ($0["id"] as? String) == live.id }
            var items = existing?["items"] as? [[String: Any]] ?? []
            if live.interrupted {
                // Stopped: what was still running (a thinking timer) stops too.
                items = items.map { item in
                    guard item["state"] as? String == "started" else { return item }
                    var stopped = item; stopped["state"] = "interrupted"
                    return stopped
                }
            }
            if live.finished && !live.interrupted {
                if let command = live.script.command {
                    items.append(Self.command(live.id + "-command", command, at: live.finishedAt))
                }
                if let plan = live.script.plan {
                    items.append(["id": live.id + "-plan", "type": "plan", "state": "completed", "createdAt": live.finishedAt, "text": plan])
                }
                items.append(reply(live.id + "-reply", live.script.text, at: live.finishedAt))
            }
            let turn: [String: Any] = ["id": live.id, "status": live.interrupted ? "interrupted" : live.finished ? "completed" : "inProgress",
                                       "createdAt": existing?["createdAt"] ?? live.message?.createdAt ?? now(), "items": items]
            if let index = turns.firstIndex(where: { ($0["id"] as? String) == live.id }) { turns[index] = turn }
            else { turns.append(turn) }
        }
        return ["conversationId": conversation, "hostEpoch": DemoMode.hostEpoch, "lastSequence": DemoServer.shared.currentSequence,
                "messages": messages, "assistantMessages": assistant,
                "thread": ["threadId": conversation + "-thread", "hydrated": true, "turns": turns]]
    }

    /// The thread as it stood when the demo opened.
    private static func history(_ thread: Thread) -> ([[String: Any]], [[String: Any]]) {
        let id = thread.id
        switch id {
        case signOut.id:
            let ago = signOut.minutesAgo + 20
            let first = message("demo-sign-out-1", "Users get signed out at random after the app wakes up. This is what they see. Can you find out why?",
                                turn: "demo-sign-out-turn-1", conversation: id, at: time(ago), attachments: [screenshotID])
            let second = message("demo-sign-out-2", "Nice. Add a test for it and make sure the suite passes.",
                                 turn: "demo-sign-out-turn-2", conversation: id, at: time(ago, plus: 600))
            let fix: [String: Any] = ["id": "demo-sign-out-edit-1", "type": "fileChange", "state": "completed", "createdAt": time(ago, plus: 90),
                "payload": ["diffs": [
                    diff("Sources/Networking/TokenRefresher.swift", "@@ -18,9 +18,17 @@ actor TokenRefresher {\n     private let session: AuthSession\n+    private var refreshTask: Task<Token, Error>?\n \n     func validToken() async throws -> Token {\n-        if session.token.isExpired {\n-            return try await session.refresh()\n-        }\n-        return session.token\n+        guard session.token.isExpired else { return session.token }\n+        if let running = refreshTask {\n+            return try await running.value\n+        }\n+        let task = Task { try await session.refresh() }\n+        refreshTask = task\n+        defer { refreshTask = nil }\n+        return try await task.value\n     }"),
                    diff("Sources/Networking/APIClient.swift", "@@ -41,7 +41,7 @@ final class APIClient {\n     func send<T: Decodable>(_ request: Request<T>) async throws -> T {\n-        let token = try await session.refreshIfNeeded()\n+        let token = try await refresher.validToken()\n         var urlRequest = request.urlRequest"),
                ]]]
            let test: [String: Any] = ["id": "demo-sign-out-edit-2", "type": "fileChange", "state": "completed", "createdAt": time(ago, plus: 660),
                "payload": ["diffs": [
                    diff("Tests/NetworkingTests/TokenRefresherTests.swift", "@@ -0,0 +1,14 @@\n+import XCTest\n+@testable import Networking\n+\n+final class TokenRefresherTests: XCTestCase {\n+    func testConcurrentRefreshesShareOneRequest() async throws {\n+        let server = MockAuthServer(token: .expired)\n+        let refresher = TokenRefresher(session: server.session)\n+        try await withThrowingTaskGroup(of: Token.self) { group in\n+            for _ in 0..<10 { group.addTask { try await refresher.validToken() } }\n+            for try await token in group { XCTAssertFalse(token.isExpired) }\n+        }\n+        XCTAssertEqual(server.refreshCount, 1)\n+    }\n+}"),
                ]]]
            let turns: [[String: Any]] = [
                ["id": "demo-sign-out-turn-1", "status": "completed", "createdAt": time(ago), "items": [
                    thinking("demo-sign-out-t1", "**Tracing the sign-out**\n\nThe screenshot shows a session that expired right after the app woke. If two requests refresh the token at once, the second one uses a token the first already replaced. I'll check how `TokenRefresher` handles callers that arrive together.",
                             at: time(ago, plus: 5), seconds: 9),
                    search("demo-sign-out-s1", "OAuth refresh token rotation concurrent requests", results: 6, at: time(ago, plus: 15)),
                    command("demo-sign-out-c1", "rg -n \"refresh\" Sources/Networking", at: time(ago, plus: 20),
                            output: "Sources/Networking/TokenRefresher.swift:21:    func validToken() async throws -> Token {\nSources/Networking/APIClient.swift:44:        let token = try await session.refreshIfNeeded()"),
                    command("demo-sign-out-c2", "git log -5 --oneline -- Sources/Networking", at: time(ago, plus: 40)),
                    fix,
                    reply("demo-sign-out-r1", "Found it. When the app wakes up, two requests can both see an expired token and refresh it at the same time. The second refresh uses a token the first one already replaced, so the server signs the user out.\n\nThe fix lets one refresh run at a time; every other request waits for its result:\n\n```swift\nif let running = refreshTask {\n  return try await running.value\n}\nlet task = Task { try await session.refresh() }\nrefreshTask = task\ndefer { refreshTask = nil }\nreturn try await task.value\n```\n\n**Changed:** `TokenRefresher.swift` and `APIClient.swift`.", at: time(ago, plus: 120)),
                ]],
                ["id": "demo-sign-out-turn-2", "status": "completed", "createdAt": time(ago, plus: 600), "items": [
                    thinking("demo-sign-out-t2", "**Planning the test**\n\nTen requests with an expired token, started together, should cause exactly one refresh.",
                             at: time(ago, plus: 605), seconds: 4),
                    test,
                    command("demo-sign-out-c3", "swift test", at: time(ago, plus: 700), output: "Executed 42 tests, with 0 failures (0 unexpected) in 3.8 seconds"),
                    reply("demo-sign-out-r2", "Added `testConcurrentRefreshesShareOneRequest`, which starts ten requests with an expired token and checks the server sees a single refresh.\n\n- **42 tests passed** in 3.8 s\n- No other test changed\n\nReady for review on `fix/token-refresh`.\n\n[Swift book: actors](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/concurrency/) explains why a shared task is needed: an actor can take a second call while the first is waiting.", at: time(ago, plus: 720)),
                ]],
            ]
            return ([first, second], turns)
        case release.id:
            let ago = release.minutesAgo
            let first = message("demo-release-1", "Get the token fix ready for the beta.", turn: "demo-release-turn-1", conversation: id, at: time(ago + 3))
            let second = message("demo-release-2", "Looks good. Push it.", turn: "demo-release-turn-2", conversation: id, at: time(ago))
            let turns: [[String: Any]] = [
                ["id": "demo-release-turn-1", "status": "completed", "createdAt": time(ago + 3), "items": [
                    command("demo-release-c1", "git switch -c fix/token-refresh", at: time(ago + 3, plus: 10)),
                    command("demo-release-c2", "git commit -am \"Share one token refresh between requests\"", at: time(ago + 3, plus: 20),
                            output: "[fix/token-refresh 4f2a9c1] Share one token refresh between requests\n 3 files changed, 31 insertions(+), 5 deletions(-)"),
                    reply("demo-release-r1", "Committed the fix on `fix/token-refresh`. Say the word and I'll push it so the beta build can start.", at: time(ago + 3, plus: 30)),
                ]],
                ["id": "demo-release-turn-2", "status": "inProgress", "createdAt": time(ago), "items": [
                    command("demo-release-c3", "git status --short --branch", at: time(ago, plus: 5), output: "## fix/token-refresh"),
                ]],
            ]
            return ([first, second], turns)
        case speed.id:
            let ago = speed.minutesAgo + 24
            let first = message("demo-speed-1", "Set a goal: open the forecast screen in under one second on a cold start. Use agent tasks for the research and the caching.",
                                turn: "demo-speed-turn-1", conversation: id, at: time(ago))
            let second = message("demo-speed-2", "Good. Keep going until it's under a second.", turn: "demo-speed-turn-2", conversation: id, at: time(ago, plus: 1200))
            let cache: [String: Any] = ["id": "demo-speed-edit", "type": "fileChange", "state": "completed", "createdAt": time(ago, plus: 900),
                "payload": ["diffs": [diff("Sources/Forecast/ForecastStore.swift", "@@ -12,6 +12,12 @@ final class ForecastStore {\n     func load() async throws -> Forecast {\n+        if let cached = try? cache.read(), !cached.isStale {\n+            Task { try? await refresh() }\n+            return cached\n+        }\n         let forecast = try await api.forecast(for: location)\n+        try? cache.write(forecast)\n         return forecast\n     }")]]]
            let turns: [[String: Any]] = [
                ["id": "demo-speed-turn-1", "status": "completed", "createdAt": time(ago), "items": [
                    thinking("demo-speed-t1", "**Splitting the work**\n\nFirst measure where the time goes, then cache the last forecast so the screen can show it at once.",
                             at: time(ago, plus: 4), seconds: 6),
                    command("demo-speed-c1", "xcrun xctrace record --template 'App Launch' --launch Weather.app", at: time(ago, plus: 30),
                            output: "Cold start: 2.4 s\nFirst forecast on screen: 1.7 s after launch"),
                    agentTask("demo-speed-a1", scout, kind: "started", at: time(ago, plus: 60)),
                    agentTask("demo-speed-a2", builder, kind: "started", at: time(ago, plus: 62)),
                    agentTask("demo-speed-a3", builder, kind: "completed", at: time(ago, plus: 880)),
                    cache,
                    reply("demo-speed-r1", "Cold start is **2.4 s** today; the forecast request alone takes 1.7 s.\n\n- **Builder** cached the last forecast, so the screen shows it at once and refreshes behind it.\n- **Scout** is still profiling the forecast request.\n\nI'll keep working toward the goal.", at: time(ago, plus: 960)),
                ]],
                ["id": "demo-speed-turn-2", "status": "inProgress", "createdAt": time(ago, plus: 1200), "items": [
                    command("demo-speed-c2", "xcrun xctrace record --template 'App Launch' --launch Weather.app", at: time(ago, plus: 1210),
                            output: "Cold start: 1.3 s\nFirst forecast on screen: 0.4 s after launch"),
                    thinking("demo-speed-t2", "**Finding the last 300 ms**\n\nThe cached forecast shows at 0.4 s now. What is left is the app's own startup: fonts and the map view load before the first screen.",
                             at: time(ago, plus: 1220), seconds: nil),
                ]],
            ]
            return ([first, second], turns)
        default:
            let ago = darkMode.minutesAgo
            let first = message("demo-dark-1", "Add a dark mode toggle to the website.", turn: "demo-dark-turn-1", conversation: id, at: time(ago + 6))
            let second = message("demo-dark-2", "Go ahead with the toggle.", turn: "demo-dark-turn-2", conversation: id, at: time(ago))
            func checklist(_ states: [String]) -> [String: Any] {
                let texts = ["Add dark color tokens", "Build the toggle", "Remember the choice", "Run the tests"]
                return ["entries": zip(texts, states).map { ["text": $0.0, "status": $0.1] },
                        "total": states.count, "completed": states.filter { $0 == "completed" }.count]
            }
            let before = checklist(["pending", "pending", "pending", "pending"])
            var current = checklist(["completed", "inProgress", "pending", "pending"])
            current["history"] = [before, current]
            let todos = [["content": "Add dark color tokens", "status": "completed", "activeForm": "Adding dark color tokens"],
                         ["content": "Build the toggle", "status": "in_progress", "activeForm": "Building the toggle"],
                         ["content": "Remember the choice", "status": "pending", "activeForm": "Remembering the choice"],
                         ["content": "Run the tests", "status": "pending", "activeForm": "Running the tests"]]
            let tasks: [String: Any] = ["id": "demo-dark-tasks", "type": "mcpToolCall", "state": "completed", "createdAt": time(ago + 6, plus: 10),
                "payload": ["server": "Claude", "tool": "TodoWrite", "arguments": ["todos": todos], "toolAction": ["kind": "updateTasks"],
                            "status": "completed", "success": true, "taskChecklist": current,
                            "result": ["content": "Todos have been modified successfully."]]]
            let tokens: [String: Any] = ["id": "demo-dark-edit", "type": "fileChange", "state": "completed", "createdAt": time(ago + 6, plus: 40),
                "payload": ["diffs": [diff("src/styles/colors.css", "@@ -1,6 +1,14 @@\n :root {\n   --background: #ffffff;\n   --text: #1d1d1f;\n }\n+\n+@media (prefers-color-scheme: dark) {\n+  :root {\n+    --background: #111114;\n+    --text: #f5f5f7;\n+  }\n+}")]]]
            let turns: [[String: Any]] = [
                ["id": "demo-dark-turn-1", "status": "completed", "createdAt": time(ago + 6), "items": [
                    tasks,
                    command("demo-dark-c1", "rg -n \"--background\" src", at: time(ago + 6, plus: 20), output: "src/styles/colors.css:2:  --background: #ffffff;"),
                    tokens,
                    reply("demo-dark-r1", "The dark colors are in place and follow the system setting. Next is the toggle itself, so visitors can choose.", at: time(ago + 6, plus: 60)),
                ]],
                ["id": "demo-dark-turn-2", "status": "inProgress", "createdAt": time(ago), "items": [
                    command("demo-dark-c2", "ls src/components", at: time(ago, plus: 5), output: "Footer.tsx\nHeader.tsx\nSettings.tsx"),
                ]],
            ]
            return ([first, second], turns)
        }
    }
}

/// The demo Projects' folders: a few files each, their Git state and what
/// each thread changed, for Code changes and Files. Read-only.
enum DemoWorkspace {
    struct File { let path: String; let text: String; var status: String? = nil; var committed = false }

    static func files(project: String) -> [File] { project == DemoContent.website.id ? website : weather }

    private static let weather: [File] = [
        File(path: "README.md", text: "# Weather\n\nA forecast app for iPhone and iPad.\n\nRun the tests with `swift test`.\n"),
        File(path: "Package.swift", text: "// swift-tools-version: 6.0\nimport PackageDescription\n\nlet package = Package(\n    name: \"Weather\",\n    targets: [\n        .target(name: \"Networking\"),\n        .target(name: \"Forecast\", dependencies: [\"Networking\"]),\n        .testTarget(name: \"NetworkingTests\", dependencies: [\"Networking\"]),\n    ]\n)\n"),
        File(path: "Sources/Networking/TokenRefresher.swift", text: "actor TokenRefresher {\n    private let session: AuthSession\n    private var refreshTask: Task<Token, Error>?\n\n    init(session: AuthSession) { self.session = session }\n\n    func validToken() async throws -> Token {\n        guard session.token.isExpired else { return session.token }\n        if let running = refreshTask {\n            return try await running.value\n        }\n        let task = Task { try await session.refresh() }\n        refreshTask = task\n        defer { refreshTask = nil }\n        return try await task.value\n    }\n}\n", status: "modified", committed: true),
        File(path: "Sources/Networking/APIClient.swift", text: "final class APIClient {\n    private let refresher: TokenRefresher\n\n    init(refresher: TokenRefresher) { self.refresher = refresher }\n\n    func send<T: Decodable>(_ request: Request<T>) async throws -> T {\n        let token = try await refresher.validToken()\n        var urlRequest = request.urlRequest\n        urlRequest.setValue(\"Bearer \\(token.value)\", forHTTPHeaderField: \"Authorization\")\n        let (data, _) = try await URLSession.shared.data(for: urlRequest)\n        return try JSONDecoder().decode(T.self, from: data)\n    }\n}\n", status: "modified", committed: true),
        File(path: "Sources/Forecast/ForecastStore.swift", text: "final class ForecastStore {\n    private let api: ForecastAPI\n    private let cache: ForecastCache\n    var location: Location\n\n    func load() async throws -> Forecast {\n        if let cached = try? cache.read(), !cached.isStale {\n            Task { try? await refresh() }\n            return cached\n        }\n        let forecast = try await api.forecast(for: location)\n        try? cache.write(forecast)\n        return forecast\n    }\n}\n", status: "modified"),
        File(path: "Tests/NetworkingTests/TokenRefresherTests.swift", text: "import XCTest\n@testable import Networking\n\nfinal class TokenRefresherTests: XCTestCase {\n    func testConcurrentRefreshesShareOneRequest() async throws {\n        let server = MockAuthServer(token: .expired)\n        let refresher = TokenRefresher(session: server.session)\n        try await withThrowingTaskGroup(of: Token.self) { group in\n            for _ in 0..<10 { group.addTask { try await refresher.validToken() } }\n            for try await token in group { XCTAssertFalse(token.isExpired) }\n        }\n        XCTAssertEqual(server.refreshCount, 1)\n    }\n}\n", status: "added", committed: true),
    ]

    private static let website: [File] = [
        File(path: "README.md", text: "# Website\n\nThe marketing site. `npm run dev` starts it on port 3000.\n"),
        File(path: "package.json", text: "{\n  \"name\": \"website\",\n  \"private\": true,\n  \"scripts\": { \"dev\": \"next dev\", \"build\": \"next build\", \"test\": \"vitest run\" }\n}\n"),
        File(path: "src/styles/colors.css", text: ":root {\n  --background: #ffffff;\n  --text: #1d1d1f;\n}\n\n@media (prefers-color-scheme: dark) {\n  :root {\n    --background: #111114;\n    --text: #f5f5f7;\n  }\n}\n", status: "modified"),
        File(path: "src/components/Header.tsx", text: "export function Header() {\n  return (\n    <header className=\"header\">\n      <a href=\"/\">Home</a>\n    </header>\n  );\n}\n"),
        File(path: "src/components/Footer.tsx", text: "export function Footer() {\n  return <footer className=\"footer\">© 2026</footer>;\n}\n"),
        File(path: "src/components/Settings.tsx", text: "export function Settings() {\n  return <section className=\"settings\"><h1>Settings</h1></section>;\n}\n"),
    ]

    /// Each thread's own edits, from its turns' file changes.
    static func threadDiffs(_ conversation: String) -> [(path: String, diff: String, additions: Int, deletions: Int)] {
        guard let turns = (DemoContent.snapshot(conversation)?["thread"] as? [String: Any])?["turns"] as? [[String: Any]] else { return [] }
        return turns.flatMap { $0["items"] as? [[String: Any]] ?? [] }
            .filter { $0["type"] as? String == "fileChange" }
            .flatMap { (($0["payload"] as? [String: Any])?["diffs"] as? [[String: Any]]) ?? [] }
            .compactMap { diff in
                guard let path = diff["path"] as? String, let text = diff["diff"] as? String else { return nil }
                return (path, text, diff["additions"] as? Int ?? 0, diff["deletions"] as? Int ?? 0)
            }
    }

    private static func project(_ conversation: String) -> (id: String, name: String, folder: String) {
        DemoContent.thread(conversation)?.project == DemoContent.website.id ? DemoContent.website : DemoContent.weather
    }
    private static func branch(_ conversation: String) -> String {
        project(conversation).id == DemoContent.website.id ? "dark-mode" : "fix/token-refresh"
    }
    /// A file's whole text as one added hunk, for a scope with no recorded edit.
    private static func hunk(_ file: File) -> String {
        let lines = file.text.split(separator: "\n", omittingEmptySubsequences: false).dropLast()
        return "@@ -0,0 +1,\(lines.count) @@\n" + lines.map { "+" + $0 }.joined(separator: "\n")
    }
    private static func counts(_ diff: String) -> (Int, Int) {
        let lines = diff.split(separator: "\n")
        return (lines.filter { $0.hasPrefix("+") }.count, lines.filter { $0.hasPrefix("-") }.count)
    }
    /// The edit a scope shows for a file: the thread's own hunk when it made one.
    private static func diff(_ file: File, project: String) -> String {
        let recorded = DemoContent.threads.filter { $0.project == project }.flatMap { threadDiffs($0.id) }
        return recorded.first { $0.path == file.path }?.diff ?? hunk(file)
    }
    private static func changed(_ conversation: String, scope: String) -> [File] {
        files(project: project(conversation).id).filter { $0.status != nil && (scope == "branch" || !$0.committed) }
    }

    static func summary(_ conversation: String) -> [String: Any] {
        func totals(_ list: [File]) -> [String: Any] {
            let all = list.map { counts(diff($0, project: project(conversation).id)) }
            return ["files": list.count, "additions": all.map(\.0).reduce(0, +), "deletions": all.map(\.1).reduce(0, +)]
        }
        return ["available": true, "detail": NSNull(), "repository": true, "branch": branch(conversation), "detached": false,
                "head": "4f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39", "worktree": false, "baseRef": "origin/main",
                "mergeBase": "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b", "ahead": 1, "behind": 0,
                "uncommitted": totals(changed(conversation, scope: "uncommitted")), "branchChanges": totals(changed(conversation, scope: "branch"))]
    }

    /// `thread` lists the thread's own edits with their hunks; `uncommitted` and `branch` the folder's.
    static func changes(_ conversation: String, scope: String) -> [String: Any] {
        var rows: [[String: Any]]
        if scope == "thread" {
            var merged: [String: (diff: String, additions: Int, deletions: Int)] = [:], order: [String] = []
            for edit in threadDiffs(conversation) {
                if let earlier = merged[edit.path] {
                    merged[edit.path] = (earlier.diff + "\n" + edit.diff, earlier.additions + edit.additions, earlier.deletions + edit.deletions)
                } else { merged[edit.path] = (edit.diff, edit.additions, edit.deletions); order.append(edit.path) }
            }
            rows = order.map { path in
                let edit = merged[path]!
                return ["path": path, "originalPath": NSNull(), "status": edit.deletions == 0 && edit.diff.hasPrefix("@@ -0,0") ? "added" : "modified",
                        "binary": false, "additions": edit.additions, "deletions": edit.deletions, "diff": edit.diff]
            }
        } else {
            rows = changed(conversation, scope: scope).map { file in
                let (additions, deletions) = counts(diff(file, project: project(conversation).id))
                return ["path": file.path, "originalPath": NSNull(), "status": file.status ?? "modified", "binary": false,
                        "additions": additions, "deletions": deletions]
            }
        }
        return ["scope": scope, "available": true, "detail": NSNull(), "branch": branch(conversation), "baseRef": "origin/main",
                "files": rows, "truncated": false,
                "totals": ["files": rows.count, "additions": rows.compactMap { $0["additions"] as? Int }.reduce(0, +),
                           "deletions": rows.compactMap { $0["deletions"] as? Int }.reduce(0, +)]]
    }

    static func fileDiff(_ conversation: String, path: String) -> [String: Any]? {
        guard let file = files(project: project(conversation).id).first(where: { $0.path == path }) else { return nil }
        return ["path": path, "staged": false, "diff": diff(file, project: project(conversation).id)]
    }

    // MARK: Files

    static func roots(_ conversation: String) -> [String: Any] {
        let folder = project(conversation)
        return ["available": true, "detail": NSNull(),
                "roots": [["id": "workspace", "label": folder.name, "path": folder.folder, "isDirectory": true,
                           "kind": "workingDirectory", "readOnly": true]],
                "attachments": DemoContent.files(conversation)]
    }

    static func directory(_ conversation: String, path: String) -> [String: Any] {
        let prefix = path.isEmpty ? "" : path + "/"
        var names: [String: Bool] = [:]
        for file in files(project: project(conversation).id) where file.path.hasPrefix(prefix) {
            let rest = file.path.dropFirst(prefix.count)
            if let slash = rest.firstIndex(of: "/") { names[String(rest[..<slash]), default: true] = true }
            else { names[String(rest)] = false }
        }
        let entries: [[String: Any]] = names.sorted { ($0.value ? 0 : 1, $0.key) < ($1.value ? 0 : 1, $1.key) }.map { name, isDirectory in
            let full = prefix + name
            let text = files(project: project(conversation).id).first { $0.path == full }?.text
            return ["name": name, "path": full, "isDirectory": isDirectory,
                    "byteSize": text.map { $0.utf8.count as Any } ?? NSNull(), "mimeType": isDirectory ? NSNull() : "text/plain"]
        }
        let parent = path.isEmpty ? nil : (path.contains("/") ? String(path[..<path.lastIndex(of: "/")!]) : "")
        return ["rootId": "workspace", "path": path, "parentPath": parent.map { $0 as Any } ?? NSNull(), "entries": entries, "nextOffset": NSNull()]
    }

    static func text(_ conversation: String, path: String) -> Data? {
        files(project: project(conversation).id).first { $0.path == path }.map { Data($0.text.utf8) }
    }

    static func status(_ conversation: String) -> [String: Any] {
        ["available": true, "detail": NSNull(), "repositoryPath": project(conversation).folder,
         "changes": changed(conversation, scope: "uncommitted").map { file in
             ["path": file.path, "originalPath": NSNull(), "state": file.status == "added" ? "untracked" : "modified",
              "indexStatus": file.status == "added" ? "?" : " ", "worktreeStatus": file.status == "added" ? "?" : "M"]
         }]
    }
}

/// Answers the demo computer's requests inside the app. Any request that is not for
/// the demo computer fails as if offline, so a demo session can never reach the network.
final class DemoURLProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        guard let url = request.url, url.scheme == "https", url.host == URL(string: DemoMode.origin)?.host else {
            client?.urlProtocol(self, didFailWithError: URLError(.notConnectedToInternet))
            return
        }
        let method = request.httpMethod ?? "GET"
        let path = url.path
        DemoServer.shared.record(path)
        let body = Self.body(of: request)
        let fields = body.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
        let query = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func value(_ name: String) -> String? { query.first { $0.name == name }?.value }
        let server = DemoServer.shared
        let conversations = "/api/v1/conversations/"
        let projectConversations = "/api/v1/project-conversations/"
        switch path {
        case "/api/v1/devices":
            return finish(json: [["id": DemoMode.deviceID, "revokedAt": NSNull()]])
        case "/api/v1/host/status":
            return finish(json: ["hostInstallationId": DemoMode.hostID, "hostName": DemoMode.computerName, "platform": "macos"])
        case "/api/v1/projects" where method == "GET":
            return finish(json: DemoContent.projects())
        case "/api/v1/conversations" where method == "GET":
            return finish(json: [Any]())
        case "/api/v1/automations" where method == "GET":
            return finish(json: server.automationList)
        case "/api/v1/automations" where method == "POST":
            return server.saveAutomation(nil, fields).map { finish(status: 201, json: $0) } ?? finish(status: 400)
        case "/api/v1/approvals":
            return finish(json: DemoContent.attention())
        case "/api/v1/agent-options":
            return finish(json: DemoContent.agentOptions())
        case ChatDetailSettings.path:
            return finish(json: DemoContent.chatDetail())
        case "/api/v1/providers":
            return finish(json: ["providers": [
                ["id": "codex", "running": true, "starting": false, "incompatible": false, "updatePending": false, "detail": NSNull(),
                 "state": "signedIn", "version": "0.163.0", "account": "ChatGPT Pro", "working": false],
                ["id": "claude", "running": true, "starting": false, "incompatible": false, "updatePending": false, "detail": NSNull(),
                 "state": "signedIn", "version": "2.4.1", "account": "Claude Max", "working": false]]])
        case "/api/v1/search":
            return finish(json: DemoContent.search(value("q") ?? ""))
        case "/api/v1/account/usage":
            return finish(json: DemoContent.usage(family: value("agentFamily") ?? "codex"))
        default: break
        }
        if path.hasPrefix("/api/v1/automations/") {
            let rest = path.dropFirst("/api/v1/automations/".count).split(separator: "/").map(String.init)
            switch (method, rest.count > 1 ? rest[1] : "") {
            case ("PATCH", ""):
                return server.saveAutomation(rest[0], fields).map { finish(json: $0) } ?? finish(status: 404)
            case ("DELETE", ""):
                return server.deleteAutomation(rest[0]) ? finish(status: 204) : finish(status: 404)
            case ("POST", "run"):
                return server.runAutomation(rest[0]).map { finish(status: 202, json: $0) } ?? finish(status: 404)
            case ("GET", "runs"):
                return finish(json: server.automationRuns(rest[0]))
            default:
                return unavailable(method)
            }
        }
        if path.hasPrefix("/api/v1/conversations/"), path.hasSuffix("/chat-detail"), method == "PUT" {
            let chat = String(path.dropFirst("/api/v1/conversations/".count).dropLast("/chat-detail".count))
            let level = fields["detail"] as? String
            guard fields["detail"] is NSNull || level.flatMap({ ChatDetailLevel(rawValue: $0) }) != nil else { return finish(status: 400) }
            server.setChatDetail(chat: chat, level)
            return finish(json: DemoContent.chatDetail())
        }
        if path.hasPrefix(ChatDetailSettings.path + "/"), method == "PUT" {
            let family = String(path.dropFirst(ChatDetailSettings.path.count + 1))
            guard let level = fields["detail"] as? String, ChatDetailLevel(rawValue: level) != nil,
                  AgentFamily(rawValue: family) != nil else { return finish(status: 400) }
            server.setChatDetail(family, level)
            return finish(json: DemoContent.chatDetail())
        }
        if path.hasPrefix("/api/v1/projects/"), method == "PATCH" {
            let id = String(path.dropFirst("/api/v1/projects/".count))
            guard let project = (DemoContent.projects()["projects"] as? [[String: Any]])?.first(where: { $0["id"] as? String == id }) else {
                return finish(status: 404)
            }
            server.setProjectFlags(id, fields)
            var updated = project
            for name in ["isPinned", "isIncluded"] { updated[name] = server.projectFlag(id, name, default: project[name] as? Bool ?? false) }
            return finish(json: updated)
        }
        if path.hasPrefix("/api/v1/projects/"), path.hasSuffix("/threads/archived") {
            return finish(json: ["threads": [Any](), "partial": [Any]()])
        }
        if path.hasPrefix("/api/v1/projects/"), path.hasSuffix("/threads") {
            let project = path.dropFirst("/api/v1/projects/".count).dropLast("/threads".count)
            return finish(json: DemoContent.threads(project: String(project)))
        }
        if path.hasPrefix("/api/v1/approvals/"), path.hasSuffix("/resolve"), method == "POST" {
            let id = String(path.dropFirst("/api/v1/approvals/".count).dropLast("/resolve".count))
            let decision = fields["decision"] as? String ?? "accept"
            let answer = (fields["responseJson"] as? String).flatMap(Self.firstAnswer)
            return server.resolve(id, decision: decision, answer: answer) ? finish(json: [String: Any]()) : finish(status: 409)
        }
        if path.hasPrefix(projectConversations) {
            let rest = path.dropFirst(projectConversations.count).split(separator: "/").map(String.init)
            guard let id = rest.first, let thread = DemoContent.thread(id) else { return finish(status: 404) }
            switch (method, Array(rest.dropFirst())) {
            case ("GET", ["subagents"]):
                return finish(json: DemoContent.subagents(id))
            case ("GET", let task) where task.count == 3 && task[0] == "subagents" && task[2] == "transcript":
                return DemoContent.transcript(id, task: task[1]).map { finish(json: $0) } ?? finish(status: 404)
            case ("PATCH", []):
                server.mark(id, with: fields)
                // Model, speed, access and Plan choices hold for the rest of the demo.
                server.update(thread, with: fields)
                return finish(json: DemoContent.detail(thread))
            case ("GET", []):
                return finish(json: DemoContent.detail(thread))
            default:
                return unavailable(method)
            }
        }
        if path.hasPrefix(conversations) {
            let rest = path.dropFirst(conversations.count).split(separator: "/", maxSplits: 1).map(String.init)
            let id = rest.first ?? ""
            let suffix = rest.count > 1 ? rest[1] : ""
            guard DemoContent.thread(id) != nil else { return finish(status: 404) }
            switch (method, suffix) {
            case ("GET", ""):
                return finish(json: DemoContent.snapshot(id) ?? [:])
            case ("PATCH", ""):
                server.markRead(id)
                return finish(status: 204)
            case ("GET", "files"):
                return finish(json: DemoContent.files(id))
            case ("POST", "files"):
                // An attachment stays in memory for this demo; nothing is uploaded anywhere.
                guard let name = fields["name"] as? String, let mime = fields["mimeType"] as? String,
                      let data = (fields["contentBase64"] as? String).flatMap({ Data(base64Encoded: $0) }) else { return finish(status: 400) }
                return finish(status: 201, json: server.upload(id, name: name, mimeType: mime, data: data))
            case ("GET", "files/" + DemoContent.screenshotID) where id == DemoContent.signOut.id:
                return finish(status: 200, body: DemoContent.screenshot, contentType: "image/png")
            case ("GET", let file) where file.hasPrefix("files/"):
                return server.uploadData(id, id: String(file.dropFirst("files/".count)))
                    .map { finish(status: 200, body: $0.data, contentType: $0.mimeType) } ?? finish(status: 404)
            case ("GET", "desktop-continuation"):
                return finish(json: DemoContent.continuation(DemoContent.thread(id)!))
            case ("POST", let stop) where stop.hasPrefix("turns/") && stop.hasSuffix("/interrupt"):
                let turn = String(stop.dropFirst("turns/".count).dropLast("/interrupt".count))
                return server.interrupt(conversation: id, turn: turn)
                    ? finish(json: ["stopConfirmed": true]) : finish(status: 204)
            case ("GET", "queue"), ("GET", "questions"):
                return finish(json: [Any]())
            case ("GET", "search/occurrences"):
                return finish(json: DemoContent.occurrences(id, value("q") ?? ""))
            case ("GET", "workspace/git/summary"):
                return finish(json: DemoWorkspace.summary(id))
            case ("GET", "workspace/git/thread-changes"):
                return finish(json: DemoWorkspace.changes(id, scope: "thread"))
            case ("GET", "workspace/git/changes"):
                return finish(json: DemoWorkspace.changes(id, scope: value("scope") ?? "uncommitted"))
            case ("GET", "workspace/git/diff"):
                return DemoWorkspace.fileDiff(id, path: value("path") ?? "").map { finish(json: $0) } ?? finish(status: 404)
            case ("GET", "workspace/git/status"):
                return finish(json: DemoWorkspace.status(id))
            case ("GET", "workspace/roots"):
                return finish(json: DemoWorkspace.roots(id))
            case ("GET", "workspace/directory"):
                return finish(json: DemoWorkspace.directory(id, path: value("path") ?? ""))
            case ("GET", "workspace/file"):
                return DemoWorkspace.text(id, path: value("path") ?? "").map { finish(status: 200, body: $0, contentType: "text/plain") }
                    ?? finish(status: 404)
            case ("GET", "history/activity"):
                return finish(json: DemoContent.activity(DemoContent.thread(id)!))
            case ("GET", "goal"):
                let none: [String: Any] = ["goal": NSNull()]
                return finish(json: id == DemoContent.speed.id ? DemoContent.goal() : none)
            case ("PUT", "goal") where id == DemoContent.speed.id:
                var goal = server.currentGoal ?? DemoContent.Goal(objective: "", status: "active")
                if let status = fields["status"] as? String { goal.status = status }
                if let objective = fields["objective"] as? String, !objective.isEmpty {
                    goal.objective = objective
                    goal.tokenBudget = fields["tokenBudget"] as? Int
                    goal.timeBudgetSeconds = fields["timeBudgetSeconds"] as? Int
                }
                server.setGoal(goal)
                return finish(json: DemoContent.goal())
            case ("DELETE", "goal") where id == DemoContent.speed.id:
                server.setGoal(nil)
                return finish(json: [String: Any]())
            case ("POST", "messages"):
                guard let body, let request = try? JSONSerialization.jsonObject(with: body) as? [String: Any],
                      let client = request["clientMessageId"] as? String, let text = request["body"] as? String else {
                    return finish(status: 400)
                }
                let next = (request["projectModel"] as? [String: Any]).flatMap { picked -> (family: AgentFamily, model: String, effort: String?)? in
                    guard let family = (picked["family"] as? String).flatMap(AgentFamily.init(rawValue:)), let model = picked["model"] as? String else { return nil }
                    return (family, model, picked["effort"] as? String)
                }
                server.send(conversation: id, client: client, body: text, attachments: request["attachmentIds"] as? [String] ?? [], model: next)
                return finish(status: 202, json: ["clientMessageId": client, "wonderMessageId": "demo-" + client, "conversationId": id,
                                                  "bodySha256": ConversationFile.digest(Data(text.utf8)),
                                                  "deliveryState": "accepted_by_wonder", "queued": false])
            default:
                break
            }
        }
        unavailable(method)
    }

    /// Anything else is a feature the demo does not have; the app treats it as
    /// one this computer does not offer.
    private func unavailable(_ method: String) {
        finish(status: method == "GET" ? 404 : 501, body: Data("Not available in the demo.".utf8), contentType: "text/plain")
    }

    private static func body(of request: URLRequest) -> Data? {
        if let data = request.httpBody { return data }
        guard let stream = request.httpBodyStream else { return nil }
        stream.open(); defer { stream.close() }
        var data = Data(), buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            guard count > 0 else { break }
            data.append(contentsOf: buffer.prefix(count))
        }
        return data
    }

    /// The first chosen option of a question reply (`{"answers":{"id":{"answers":[…]}}}`).
    private static func firstAnswer(_ response: String) -> String? {
        guard let data = response.data(using: .utf8),
              let answers = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["answers"] as? [String: Any] else { return nil }
        return answers.values.compactMap { ($0 as? [String: Any])?["answers"] as? [String] }.first?.first
    }

    private func finish(status: Int = 200, json value: Any) {
        let data = (try? JSONSerialization.data(withJSONObject: value, options: [.withoutEscapingSlashes])) ?? Data("{}".utf8)
        finish(status: status, body: data)
    }

    private func finish(status: Int, body: Data = Data(), contentType: String = "application/json") {
        guard let url = request.url,
              let response = HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1", headerFields: ["Content-Type": contentType]) else { return }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
}

// MARK: Views

/// Marks the demo computer wherever its chats show, so it never passes for a real computer.
struct DemoBadge: View {
    var body: some View {
        Text("Demo")
            .font(.caption.weight(.semibold))
            .foregroundStyle(Color(uiColor: .systemOrange))
            .padding(.horizontal, 6).padding(.vertical, 1)
            .background(Color(uiColor: .systemOrange).opacity(0.16), in: Capsule())
            .lineLimit(1).fixedSize()
            .accessibilityIdentifier("demo-badge")
    }
}

/// Above the demo's Chats: what the demo is, and the ways out of it.
struct DemoChatsNotice: View {
    let addComputer: () -> Void
    let leave: () -> Void
    @Environment(\.dynamicTypeSize) private var typeSize
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("These chats are a demo. Replies are scripted, and nothing you send leaves this device.")
                .font(.footnote).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            let layout = typeSize.isAccessibilitySize ? AnyLayout(VStackLayout(alignment: .leading, spacing: 4)) : AnyLayout(HStackLayout(spacing: 16))
            layout {
                Button("Add your computer", action: addComputer)
                    .font(.footnote.weight(.semibold))
                    .accessibilityIdentifier("demo-add-computer")
                Button("Leave demo", action: leave)
                    .font(.footnote)
                    .accessibilityIdentifier("demo-leave")
            }
            .buttonStyle(.borderless)
        }
        .padding(.vertical, 6)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("demo-notice")
    }
}

/// New chat on the demo computer: it has no folders to start one in.
struct DemoNewChatNotice: View {
    let showChats: () -> Void
    let addComputer: () -> Void
    var body: some View {
        ScrollView {
            VStack(spacing: 14) {
                DemoBadge()
                Text("New chats need your own computer")
                    .font(.title3.weight(.semibold)).multilineTextAlignment(.center)
                    .accessibilityAddTraits(.isHeader)
                Text("In the demo, open a thread from Chats and send a message to see how a reply arrives.")
                    .multilineTextAlignment(.center).foregroundStyle(.secondary)
                Button(action: showChats) { Label("Show Chats", systemImage: "list.bullet").frame(maxWidth: 280) }
                    .buttonStyle(.wonderProminent).controlSize(.large)
                    .accessibilityIdentifier("demo-show-chats")
                Button("Add your computer", systemImage: "plus", action: addComputer)
                    .accessibilityIdentifier("demo-new-chat-add-computer")
            }
            .frame(maxWidth: 420)
            .padding(.horizontal, 24).padding(.vertical, 32)
            .frame(maxWidth: .infinity)
        }
        .defaultScrollAnchor(.center)
        .accessibilityIdentifier("demo-new-chat")
    }
}

/// Computer view on the demo computer: a still picture of a made-up screen,
/// drawn in the app. Nothing is captured or streamed.
struct DemoComputerView: View {
    private let dismiss = ToolDismiss()
    @Environment(\.wonderTheme) private var theme
    var body: some View {
        VStack(spacing: 12) {
            Image(uiImage: Self.frame)
                .resizable().aspectRatio(contentMode: .fit)
                .clipShape(RoundedRectangle(cornerRadius: 8))
                .accessibilityLabel("A picture of the demo computer's screen: the Weather project open in an editor, with a terminal running tests")
                .accessibilityIdentifier("demo-computer-frame")
            Text("This is a picture for the demo. On your own computer, this shows its live screen: watch an agent work, then tap, type and scroll to take over.")
                .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityIdentifier("demo-computer-note")
        }
        .padding(16)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(theme.computerBackground.ignoresSafeArea())
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("demo-computer")
        .navigationTitle(DemoMode.computerName)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                HStack(spacing: 6) { Text(DemoMode.computerName).font(.headline); DemoBadge() }
            }
            ToolbarItem(placement: .confirmationAction) {
                HStack {
                    CompanionExpandButton()
                    ToolCloseControl { Button("Done") { dismiss() }.accessibilityIdentifier("demo-computer-done") }
                }
            }
        }
    }

    /// The made-up screen, drawn once: an editor with the sign-out fix and a terminal.
    static let frame: UIImage = UIGraphicsImageRenderer(size: CGSize(width: 1280, height: 800), format: {
        let format = UIGraphicsImageRendererFormat(); format.scale = 1; return format
    }()).image { context in
        let bounds = CGRect(x: 0, y: 0, width: 1280, height: 800)
        UIColor(red: 0.16, green: 0.24, blue: 0.38, alpha: 1).setFill(); context.fill(bounds)
        UIColor(white: 1, alpha: 0.12).setFill(); context.fill(CGRect(x: 0, y: 0, width: 1280, height: 28))
        let mono = UIFont.monospacedSystemFont(ofSize: 17, weight: .regular)
        func window(_ frame: CGRect, title: String, fill: UIColor) {
            fill.setFill(); UIBezierPath(roundedRect: frame, cornerRadius: 12).fill()
            UIColor(white: 1, alpha: 0.08).setFill()
            UIBezierPath(roundedRect: CGRect(x: frame.minX, y: frame.minY, width: frame.width, height: 36), cornerRadius: 12).fill()
            for (index, color) in [UIColor.systemRed, .systemYellow, .systemGreen].enumerated() {
                color.setFill(); UIBezierPath(ovalIn: CGRect(x: frame.minX + 16 + CGFloat(index) * 22, y: frame.minY + 12, width: 12, height: 12)).fill()
            }
            (title as NSString).draw(at: CGPoint(x: frame.minX + 90, y: frame.minY + 8),
                                     withAttributes: [.font: UIFont.systemFont(ofSize: 15, weight: .semibold), .foregroundColor: UIColor(white: 0.85, alpha: 1)])
        }
        func lines(_ text: [(String, UIColor)], at origin: CGPoint) {
            for (index, line) in text.enumerated() {
                (line.0 as NSString).draw(at: CGPoint(x: origin.x, y: origin.y + CGFloat(index) * 26), withAttributes: [.font: mono, .foregroundColor: line.1])
            }
        }
        let plain = UIColor(white: 0.88, alpha: 1), keyword = UIColor.systemPink, added = UIColor.systemGreen, comment = UIColor.systemGray
        window(CGRect(x: 60, y: 70, width: 760, height: 620), title: "TokenRefresher.swift — Weather", fill: UIColor(white: 0.11, alpha: 1))
        lines([("actor TokenRefresher {", keyword), ("    private let session: AuthSession", plain),
               ("    private var refreshTask: Task<Token, Error>?", added), ("", plain),
               ("    func validToken() async throws -> Token {", plain),
               ("        guard session.token.isExpired else {", plain), ("            return session.token", plain), ("        }", plain),
               ("        if let running = refreshTask {", added), ("            return try await running.value", added), ("        }", added),
               ("        let task = Task { try await session.refresh() }", added), ("        refreshTask = task", added),
               ("        defer { refreshTask = nil }", added), ("        return try await task.value", added), ("    }", plain), ("}", plain),
               ("", plain), ("// One refresh at a time; the others wait for it.", comment)],
              at: CGPoint(x: 84, y: 124))
        window(CGRect(x: 760, y: 380, width: 460, height: 330), title: "Terminal", fill: UIColor(white: 0.05, alpha: 1))
        lines([("$ swift test", plain), ("Building for debugging...", comment), ("Test Suite 'All tests' started", comment),
               ("TokenRefresherTests passed", added), ("Executed 42 tests, 0 failures", added), ("$ ", plain)],
              at: CGPoint(x: 780, y: 432))
    }
}

/// Settings > Demo computer: what a computer's page holds, for the demo. The
/// provider pages work (status, usage, chat detail); the rest needs a real computer.
struct DemoComputerSettings: View {
    @ObservedObject var model: ConnectionModel
    let addComputer: () -> Void
    let leave: () -> Void
    var body: some View {
        WonderForm {
            Section {
                HStack(spacing: 8) { Text(DemoMode.computerName); DemoBadge() }
                Text("A made-up computer for trying Cupola. Its chats and replies are scripted, and nothing you do here leaves this device.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            Section {
                ForEach([AgentFamily.codex, .claude]) { family in // provider identity: the demo has both agents
                    NavigationLink { ProviderSettingsView(model: model, family: family) } label: {
                        HStack(spacing: 12) {
                            ProviderIcon(family: family, size: 22).frame(width: 28).accessibilityHidden(true)
                            Text(family.title)
                        }
                        .frame(minHeight: 36)
                    }
                    .accessibilityIdentifier("settings-provider:" + family.rawValue)
                }
            } header: {
                Text("Providers")
            } footer: {
                Text("Choose how much of each agent's work its chats show. On your own computer this page also has its Projects, connected apps and connection.")
            }
            Section {
                Button("Add your computer", systemImage: "plus", action: addComputer)
                    .accessibilityIdentifier("demo-settings-add-computer")
                Button("Leave demo", role: .destructive, action: leave)
                    .accessibilityIdentifier("settings-leave-demo")
            }
        }
        .navigationTitle(DemoMode.computerName).navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("demo-computer-settings")
    }
}

/// A Project page (add, edit) opened on the demo computer: Projects come from a real computer's folders.
struct DemoPageNotice: View {
    @ObservedObject var library: ConnectionLibrary
    @State private var pairing = false
    var body: some View {
        VStack(spacing: 14) {
            DemoBadge()
            Text("Projects come from your computer")
                .font(.title3.weight(.semibold)).multilineTextAlignment(.center)
                .accessibilityAddTraits(.isHeader)
            Text("The demo's two Projects are made up. Add your computer to add Projects from its folders and change their settings.")
                .multilineTextAlignment(.center).foregroundStyle(.secondary)
            Button("Add your computer", systemImage: "plus") { pairing = true }
                .buttonStyle(.wonderProminent).controlSize(.large)
                .accessibilityIdentifier("demo-page-add-computer")
        }
        .frame(maxWidth: 420)
        .padding(.horizontal, 24).padding(.vertical, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityIdentifier("demo-page-notice")
        .sheet(isPresented: $pairing) { PairComputerView(model: library.pairingModel()) }
    }
}

extension ProviderCapabilities {
    /// The record the demo and Diagnostics computers publish for `family`: a
    /// Mac's record without the endpoints those fixtures do not serve (the
    /// composer lists, Cupola's skills and rewind) and, for OpenCode
    /// and Cursor, without thread tools. A fixture that serves one turns it on.
    static func fixtureMac(_ family: AgentFamily) -> Self {
        switch family {
        case .codex: // provider identity: the fixture Mac's own record
            Self(interrupt: true, steer: true, agentSteer: true, queue: true, modelSwitch: true,
                 terminalStatus: .strong, nativeFork: true, rewind: false, nativeSessions: true,
                 archive: .provider, attachments: true, slashCommands: false, fileMentions: false,
                 skills: true, promptSuggestions: false, accessChoices: true, structuredQuestions: true,
                 denyWithInstruction: false, plan: .toggle, agentTasks: .nativeThreads, namedSubagents: true,
                 threadTools: true, goals: true, imageGeneration: true, usageLimits: true, connectedApps: true,
                 desktopProjectSync: true, approvalModes: ["ask"])
        case .claude: // provider identity: the fixture Mac's own record
            Self(interrupt: true, steer: true, agentSteer: false, queue: true, modelSwitch: true,
                 terminalStatus: .strong, nativeFork: true, rewind: false, nativeSessions: true,
                 archive: .wonder, attachments: true, slashCommands: false, fileMentions: false,
                 skills: false, promptSuggestions: true, accessChoices: true, structuredQuestions: true,
                 denyWithInstruction: true, plan: .permissionMode, agentTasks: .transcript, namedSubagents: false,
                 threadTools: true, goals: false, imageGeneration: false, usageLimits: true, connectedApps: true,
                 desktopProjectSync: false, approvalModes: ["ask", "accept_edits", "auto"])
        case .opencode, .cursor: // provider identity: the fixture Mac's own record
            Self(interrupt: true, steer: false, agentSteer: false, queue: true, modelSwitch: true,
                 terminalStatus: .weak, nativeFork: false, rewind: false, nativeSessions: false,
                 archive: .wonder, attachments: true, slashCommands: false, fileMentions: false,
                 skills: false, promptSuggestions: false, accessChoices: false, structuredQuestions: false,
                 denyWithInstruction: false, plan: .toggle, agentTasks: .none, namedSubagents: false,
                 threadTools: false, goals: false, imageGeneration: false, usageLimits: false, connectedApps: false,
                 desktopProjectSync: false, approvalModes: ["ask"])
        }
    }

    /// `fixtureMac` as the JSON a host sends in `families[i].capabilities`.
    static func fixtureMacJSON(_ family: AgentFamily) -> Any {
        (try? JSONEncoder().encode(fixtureMac(family))).flatMap { try? JSONSerialization.jsonObject(with: $0) } ?? [:]
    }
}
