import XCTest
@testable import WonderPairing

private final class ProjectSubagentResponseProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host == "project-agents.invalid"
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let roster = #"{"available":true,"detail":null,"subagents":[{"parentConversationId":"project parent","threadId":"child/thread","title":"Scout","agentNickname":"Scout","agentRole":"research","status":"active","isArchived":false,"canAcceptDirectInput":false}]}"#
        let transcript = #"{"subagent":{"parentConversationId":"project parent","threadId":"child/thread","title":"Scout","agentNickname":"Scout","agentRole":"research","status":"completed","isArchived":false,"canAcceptDirectInput":false},"snapshot":{"conversationId":"project-agent:project parent:child/thread","hostEpoch":"epoch","lastSequence":0,"messages":[],"assistantMessages":[],"thread":{"threadId":"child/thread","turns":[{"id":"turn","status":"completed","startedAt":null,"completedAt":null,"items":[{"id":"answer","type":"agentMessage","state":"completed","text":"Found the issue","createdAt":"2026-10-02T00:00:00Z","payload":{}}]}],"nextCursor":null,"hydrated":false},"events":[]}}"#
        let value: String?
        switch request.url?.absoluteString {
        case "https://project-agents.invalid/api/v1/project-conversations/project%20parent/subagents":
            value = roster
        case "https://project-agents.invalid/api/v1/project-conversations/project%20parent/subagents/child%2Fthread/transcript?cursor=older%2Bpage":
            value = transcript
        default:
            value = nil
        }
        let response = HTTPURLResponse(url: request.url!, statusCode: value == nil ? 404 : 200,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        if let value { client?.urlProtocol(self, didLoad: Data(value.utf8)) }
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

final class ProjectsTests: XCTestCase {
    func testOlderHostsOmitWorktreeLabelsAndSettingsRoundTrip() throws {
        let old = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"codex:old","conversationId":null,"title":"Old host","family":"codex","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":false}"#.utf8))
        XCTAssertNil(old.worktreeLabel)
        XCTAssertNil(old.branch)
        XCTAssertNil(old.locationLabel)
        for enabled in [true, false] {
            let saved = ProjectDiscoverySettings(includeWorktrees: enabled)
            XCTAssertEqual(try JSONDecoder().decode(ProjectDiscoverySettings.self, from: JSONEncoder().encode(saved)), saved)
        }
    }

    // Contract: the auto-archive rule reads from the computer's Project
    // settings and a Project summary (absent on older hosts and on Projects
    // following the computer), and saving it sends only the rule, or null to
    // return a Project to the computer's rule.
    func testAutoArchiveRuleWireShape() throws {
        let host = try JSONDecoder().decode(ProjectDiscoverySettings.self, from: Data(#"{"includeWorktrees":true,"autoArchive":{"idleDays":3,"onMerge":true}}"#.utf8))
        XCTAssertEqual(host.autoArchive, AutoArchiveRule(idleDays: 3, onMerge: true))
        XCTAssertNil(try JSONDecoder().decode(ProjectDiscoverySettings.self, from: Data(#"{"includeWorktrees":false}"#.utf8)).autoArchive)
        let body = try JSONSerialization.jsonObject(with: JSONEncoder().encode(AutoArchiveSettingsUpdate(AutoArchiveRule(idleDays: 0, onMerge: false)))) as? NSDictionary
        XCTAssertEqual(body, ["autoArchive": ["idleDays": 0, "onMerge": false]])
        let project = #"{"id":"p","name":"P","isIncluded":true,"isPinned":false,"rootsRevision":1,"folders":[{"id":"r","path":"/w","name":"w","isPrimary":true,"isAvailable":true}],"lastFamily":null,"lastUsedAt":null,"createdAt":"now""#
        XCTAssertNil(try JSONDecoder().decode(ProjectSummary.self, from: Data((project + "}").utf8)).autoArchive)
        XCTAssertEqual(try JSONDecoder().decode(ProjectSummary.self, from: Data((project + #","autoArchive":{"idleDays":14,"onMerge":false}}"#).utf8)).autoArchive,
                       AutoArchiveRule(idleDays: 14, onMerge: false))
        XCTAssertTrue(AutoArchiveRule.projectField(nil) is NSNull)
        XCTAssertEqual(AutoArchiveRule.projectField(AutoArchiveRule(idleDays: 7, onMerge: true)) as? NSDictionary, ["idleDays": 7, "onMerge": true])
        XCTAssertEqual(AutoArchiveRule.dayChoices.map(AutoArchiveRule.daysLabel),
                       ["Never", "After 1 day", "After 3 days", "After 7 days", "After 14 days", "After 30 days"])
    }

    // Contract: the computer's continuation choices and a thread's scheduled
    // resume decode from the host.
    func testContinuationWireShape() throws {
        let host = try JSONDecoder().decode(ProjectDiscoverySettings.self, from: Data(#"{"includeWorktrees":true,"resumeAfterLimit":true,"continueAfterRestart":false}"#.utf8))
        XCTAssertEqual(host.continuation, ContinuationSettings(resumeAfterLimit: true, continueAfterRestart: false))
        XCTAssertNil(try JSONDecoder().decode(ProjectDiscoverySettings.self, from: Data(#"{"includeWorktrees":true}"#.utf8)).continuation)
        let body = try JSONSerialization.jsonObject(with: JSONEncoder().encode(ContinuationSettings(resumeAfterLimit: false, continueAfterRestart: true))) as? NSDictionary
        XCTAssertEqual(body, ["resumeAfterLimit": false, "continueAfterRestart": true])

        let row = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"codex:t","conversationId":"c","title":"T","family":"codex","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":false,"resumesAt":"2026-10-20T15:40:00Z"}"#.utf8))
        let at = try XCTUnwrap(UsageResume.date(row.resumesAt))
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "UTC")!
        let locale = Locale(identifier: "en_US")
        let morning = Date(timeIntervalSince1970: at.timeIntervalSince1970 - 3600 * 5)
        XCTAssertEqual(UsageResume.label(at, now: morning, calendar: calendar, locale: locale), "Resumes at 3:40\u{202F}PM")
        XCTAssertEqual(UsageResume.label(at, now: morning.addingTimeInterval(-86_400), calendar: calendar, locale: locale), "Resumes tomorrow at 3:40\u{202F}PM")
        XCTAssertEqual(UsageResume.label(at, now: morning.addingTimeInterval(-3 * 86_400), calendar: calendar, locale: locale), "Resumes Tue at 3:40\u{202F}PM")
        // The row shows it while the thread rests, and drops it once it is past.
        XCTAssertEqual(row.rowDetail(project: "P", waitingLabel: nil, macConnected: true, now: morning).status, row.resumeLabel(now: morning))
        XCTAssertNotNil(row.resumeLabel(now: morning))
        XCTAssertNil(row.resumeLabel(now: at.addingTimeInterval(1)))
        XCTAssertEqual(row.updatingDetails(title: "T", isPinned: true, hasUnread: false).resumesAt, row.resumesAt)
    }

    // Contract: the app decodes each family's capability record from the
    // shared fixture the Mac's published record is tested against, and a
    // family without a readable record offers nothing beyond sending, so
    // Projects never fail on it and no control promises an unoffered feature.
    private func capabilitiesFixture() throws -> Data {
        try Data(contentsOf: PublishedCapabilities.fixtureURL)
    }

    func testProviderCapabilitiesDecodeFromTheSharedFixture() throws {
        let response = try JSONDecoder().decode(ProjectsResponse.self, from: capabilitiesFixture())
        XCTAssertEqual(response.capabilitiesVersion, 1)
        XCTAssertEqual(response.families.map(\.family), [.codex, .claude, .opencode, .cursor])
        let claude = response.families.capabilities(for: .claude)
        XCTAssertTrue(claude.steer)
        XCTAssertFalse(claude.agentSteer)
        XCTAssertEqual(claude.plan, .permissionMode)
        XCTAssertEqual(claude.agentTasks, .transcript)
        XCTAssertTrue(claude.allowsApprovalChoice)
        let opencode = response.families.capabilities(for: .opencode)
        XCTAssertFalse(opencode.steer)
        XCTAssertFalse(opencode.accessChoices)
        XCTAssertEqual(opencode.terminalStatus, .weak)
        XCTAssertEqual(response.families.capabilities(for: .codex).archive, .provider)
        // The fields remain wire-compatible, but every provider uses this
        // computer's own sign-in and offers no added-account controls.
        for family in response.families {
            XCTAssertFalse(family.capabilities.accounts, "\(family.family)")
        }
        XCTAssertTrue(claude.slashCommands); XCTAssertTrue(claude.fileMentions)
        XCTAssertTrue(opencode.slashCommands); XCTAssertFalse(opencode.fileMentions)
        XCTAssertTrue(response.families.capabilities(for: .codex).slashCommands)
        XCTAssertTrue(response.families.capabilities(for: .codex).fileMentions)
        XCTAssertTrue(response.families.capabilities(for: .codex).wonderSkills)
        XCTAssertTrue(claude.wonderSkills)
        XCTAssertFalse(opencode.wonderSkills)
        // Edit from here: Codex and Claude rewind; only Claude restores files.
        XCTAssertTrue(response.families.capabilities(for: .codex).rewind)
        XCTAssertFalse(response.families.capabilities(for: .codex).restoresFilesOnRewind)
        XCTAssertTrue(claude.rewind); XCTAssertTrue(claude.restoresFilesOnRewind)
        XCTAssertFalse(opencode.rewind); XCTAssertFalse(opencode.restoresFilesOnRewind)
        // Claude in Chrome is Claude's own browser control.
        XCTAssertEqual(claude.browserUse, .nativeChrome)
        XCTAssertEqual(response.families.capabilities(for: .codex).browserUse, ProviderCapabilities.BrowserUse.none)
        XCTAssertEqual(opencode.browserUse, ProviderCapabilities.BrowserUse.none)
    }

    private func project(_ id: String, pinned: Bool = false, included: Bool = true, family: AgentFamily? = nil) -> ProjectSummary {
        ProjectSummary(id: id, name: id.capitalized, isIncluded: included, isPinned: pinned,
                       folders: [ProjectFolder(id: id + "-root", path: "/work/" + id, name: id, isPrimary: true, isAvailable: true)],
                       lastFamily: family)
    }
    private func thread(_ reference: String, conversation: String? = nil, pinned: Bool = false, title: String? = nil, family: AgentFamily = .codex) -> ProjectThreadSummary {
        ProjectThreadSummary(reference: reference, conversationId: conversation, title: title ?? reference, family: family,
                             updatedAt: 1, isPinned: pinned)
    }

    // Contract: optional producer evidence survives decoding, cache, local
    // mutations and the shared pin/Project/All Chats projection without inferring
    // origin from provider identity or a native reference.
    func testComputerEvidenceSurvivesSidebarDecodingAndProjection() throws {
        for family in [AgentFamily.codex, .claude, .opencode] {
            for flag in [nil, false, true] as [Bool?] {
                for pinned in [false, true] {
                    let reference = family.rawValue + ":native"
                    var payload: [String: Any] = ["reference": reference, "conversationId": "conversation", "title": "A long synthetic title",
                        "family": family.rawValue, "updatedAt": 1, "isPinned": pinned, "hasUnread": true,
                        "isWorking": true, "needsInputOnMac": true]
                    payload["worktreeLabel"] = "agent/worker"
                    if let flag { payload["isComputerSession"] = flag }
                    let decoded = try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONSerialization.data(withJSONObject: payload))
                    let cached = try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONEncoder().encode(decoded))
                    XCTAssertEqual(cached.isComputerSession, flag)
                    XCTAssertEqual(cached.computerSessionLabel != nil, flag == true)
                    XCTAssertEqual(cached.settingPinned(!pinned).isComputerSession, flag)
                    XCTAssertEqual(cached.settingUnread(false).isComputerSession, flag)
                    let details = cached.updatingDetails(title: "Renamed", isPinned: !pinned, hasUnread: false)
                    XCTAssertEqual(details.worktreeLabel, "agent/worker")
                    XCTAssertEqual(cached.worktreeLabel, "agent/worker")
                    XCTAssertEqual(details.isComputerSession, flag)
                    XCTAssertEqual(details.title, "Renamed")
                    XCTAssertEqual(details.id, cached.id)
                    XCTAssertEqual(details.family, family)
                    XCTAssertTrue(details.isWorking && details.waitsOnMac)
                    XCTAssertFalse(details.hasUnread)
                    XCTAssertEqual(details.isPinned, !pinned)
                    var state = ProjectThreadsState()
                    state.apply(ProjectThreadsPage(threads: [cached], nextCursor: nil, partial: []), replacing: true)
                    let host = SidebarHostProjects(hostID: "mac", name: "Mac", isOnline: true, supportsProjects: true,
                        projects: [project("app")], threads: ["app": state],
                        pinned: pinned ? [PinnedProjectThread(projectId: "app", thread: cached)] : [])
                    for layout in [SidebarLayout.byProject, .allChats] {
                        let rows = SidebarProjection.rows(hosts: [host],
                            expanded: [SidebarProjection.expansionKey(host: "mac", project: "app")],
                            selectedConversation: nil, layout: layout)
                        let summaries = rows.compactMap { row -> ProjectThreadSummary? in
                            switch row {
                            case .pinned(_, _, _, _, let thread, _), .thread(_, _, let thread, _), .chat(_, _, _, let thread, _): return thread
                            default: return nil
                            }
                        }
                        XCTAssertEqual(summaries.count, 1)
                        let projected = try XCTUnwrap(summaries.first)
                        XCTAssertEqual(projected.isComputerSession, flag)
                        XCTAssertEqual(projected.id, reference)
                        XCTAssertEqual(projected.title, cached.title)
                        XCTAssertEqual(projected.family, family)
                        XCTAssertTrue(projected.isWorking && projected.hasUnread && projected.waitsOnMac)
                        XCTAssertTrue(rows.contains { $0.id == "thread:mac:" + reference })
                    }
                }
            }
        }
    }

    func testCurrentActivitySurvivesThreadListDecoding() throws {
        let data = Data(#"{"reference":"codex:t","conversationId":"c","title":"Example","family":"codex","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":true,"currentActivity":{"id":"read","type":"commandExecution","state":"started","createdAt":"now","payload":{"commandActions":[{"type":"read","path":"Sources/AppShell.swift"}]}}}"#.utf8)
        let thread = try JSONDecoder().decode(ProjectThreadSummary.self, from: data)
        let saved = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(thread)) as? [String: Any])
        XCTAssertNotNil(saved["currentActivity"], "The current action must survive the list/cache boundary.")
        XCTAssertEqual(thread.currentStep, "Reading AppShell.swift")
        XCTAssertEqual(thread.settingPinned(true).currentStep, thread.currentStep)
        XCTAssertEqual(thread.settingUnread(true).currentStep, thread.currentStep)
        XCTAssertEqual(thread.updatingDetails(title: "Renamed", isPinned: true, hasUnread: false).currentStep, thread.currentStep)
        let settled = ProjectThreadSummary(reference: "codex:t", conversationId: "c", title: "Example", family: .codex, updatedAt: 1,
            isWorking: false, currentActivity: thread.currentActivity)
        XCTAssertNil(settled.currentStep)
        // Waiting on the Mac survives decoding, the cache and pin/read edits;
        // an older Mac that omits it is not waiting.
        XCTAssertFalse(thread.waitsOnMac)
        let onMac = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"claude:s","conversationId":"c","title":"Example","family":"claude","updatedAt":1,"isPinned":true,"hasUnread":false,"isWorking":true,"needsInputOnMac":true}"#.utf8))
        let cached = try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONEncoder().encode(onMac))
        XCTAssertTrue(cached.waitsOnMac && cached.settingPinned(false).waitsOnMac && cached.settingUnread(true).waitsOnMac)
        XCTAssertFalse(cached.waitsOnMacPermission)
        // T3 Code reports a permission wait for this specific chat. Preserve it
        // through decoding, caching and edits independently of needsInputOnMac.
        let dialog = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"codex:t3-session","conversationId":"c","title":"Example","family":"codex","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":true,"waitingOnMacPermission":true}"#.utf8))
        let kept = try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONEncoder().encode(dialog)).settingPinned(true).settingUnread(true)
        XCTAssertTrue(kept.waitsOnMacPermission)
        XCTAssertTrue(kept.waitsOnMac)
        XCTAssertTrue(kept.isWorking, "A permission wait preserves the provider's running state")
        // Background work after the reply ended keeps the row running through
        // the cache and pin/read edits; an unreported copy and older Macs are not running.
        let background = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"claude:b","conversationId":"c","title":"Example","family":"claude","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":false,"backgroundWork":true}"#.utf8))
        let carried = try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONEncoder().encode(background)).settingPinned(true).settingUnread(true)
        XCTAssertTrue(carried.isRunning && !carried.isWorking)
        XCTAssertFalse(carried.saved.isRunning)
        XCTAssertFalse(onMac.saved.isRunning || ProjectThreadSummary(reference: "codex:t", conversationId: nil, title: "Old", family: .codex, updatedAt: 1).isRunning)
        // An automated thread keeps its clock, and a failed run its reason,
        // through the cache, pin/read edits and an unreported copy.
        let automated = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"codex:a","conversationId":"c","title":"Example","family":"codex","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":false,"lastAutomationAt":"2026-10-20T08:00:00.000Z","automationRun":{"automationId":"nightly","name":"Nightly","status":"failed","startedAt":"2026-10-20T08:00:00.000Z","finishedAt":"2026-10-20T08:00:01.000Z","error":"This automation needs a paired phone or iPad. Pair one again with cupola pair on your computer."}}"#.utf8))
        for copy in [automated, try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONEncoder().encode(automated)).settingPinned(true).settingUnread(true).saved] {
            let run = try XCTUnwrap(copy.automationRun)
            XCTAssertEqual(run.symbolName, "clock.badge.exclamationmark")
            XCTAssertEqual(run.label, "Automation Nightly failed: This automation needs a paired phone or iPad. Pair one again with cupola pair on your computer.")
        }
        let completed = ThreadAutomationRun(automationId: "gone", name: nil, status: "completed", startedAt: "t")
        XCTAssertEqual([completed.symbolName, completed.label], ["clock", "An automation posted here"])
        XCTAssertNil(background.automationRun)
    }

    // Contract: responses captured from a real host decode without loss, so
    // the phone and Mac agree on the published HTTP schema.
    func testHostResponsesDecode() throws {
        let projects = try JSONDecoder().decode(ProjectsResponse.self, from: Data(#"{"projects":[{"id":"p","name":"Codex P0","isIncluded":true,"isPinned":false,"rootsRevision":1,"folders":[{"id":"f","path":"/Users/owner/app","name":"app","isPrimary":true,"isAvailable":true}],"lastFamily":null,"lastUsedAt":null,"createdAt":"2026-09-30T02:18:38.447Z"}],"families":[{"family":"codex","available":true},{"family":"claude","available":true}]}"#.utf8))
        XCTAssertEqual(projects.projects.first?.primaryFolder?.path, "/Users/owner/app")
        XCTAssertNil(projects.archiveVersion, "Older hosts do not offer Archive")
        let page = try JSONDecoder().decode(ProjectThreadsPage.self, from: Data(#"{"threads":[{"reference":"claude:a0b0","conversationId":"f861","title":"Wonder-p0","family":"claude","updatedAt":1790735086,"isPinned":false,"hasUnread":true,"isWorking":false}],"nextCursor":null,"partial":[]}"#.utf8))
        XCTAssertEqual(page.threads.first?.family, .claude)
        let detail = try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(#"{"conversationId":"c","projectId":"p","projectName":"Codex P0","title":"t","family":"codex","model":"gpt-5.6-luna","effort":"low","serviceTier":"fast","accessMode":"read_only","workingFolder":"/w","workingFolderName":"w","isPinned":false,"hasUnread":false,"hasNativeSession":true,"folderInProject":true,"notice":null}"#.utf8))
        XCTAssertEqual(detail.accessMode, .readOnly)
        XCTAssertEqual(detail.serviceTier, "fast")
        XCTAssertNil(detail.isArchived)
        let encoded = try JSONEncoder().encode(detail)
        var archived = try XCTUnwrap(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
        archived["isArchived"] = true
        XCTAssertEqual(try JSONDecoder().decode(ProjectConversationDetail.self,
            from: JSONSerialization.data(withJSONObject: archived)).isArchived, true)

        // The Mac's delivery explanation survives both the wire read and the
        // saved Project cache, including a host that supplies no notice.
        for notice in [nil, "Queued in Codex on your Mac.", "Check this thread in Codex on your Mac."] as [String?] {
            archived["notice"] = notice
            let received = try JSONDecoder().decode(ProjectConversationDetail.self,
                from: JSONSerialization.data(withJSONObject: archived))
            let cached = try JSONDecoder().decode(ProjectConversationDetail.self, from: JSONEncoder().encode(received))
            XCTAssertEqual(cached.notice, notice)
        }
    }

    // Contract: a Project helper stays bound to its parent and exact provider
    // thread; the read-only transcript route cannot be confused with a writable Project thread.
    func testProjectSubagentWireAndRequestPaths() async throws {
        let roster = try JSONDecoder().decode(ProjectSubagentList.self, from: Data(#"{"available":true,"detail":null,"subagents":[{"parentConversationId":"project parent","threadId":"child/thread","title":"Scout","agentNickname":"Scout","agentRole":"research","status":"active","isArchived":false,"canAcceptDirectInput":false}]}"#.utf8))
        XCTAssertNil(roster.nextCurrentCursor, "older hosts omit pagination fields")
        let child = try XCTUnwrap(roster.subagents.first)
        XCTAssertEqual(child.statusLabel, "Running")
        XCTAssertEqual(child.statusLabel(available: false), "Last known: Running")
        XCTAssertFalse(child.canAcceptDirectInput)
        // The host cannot infer live desktop-owned work from a separate App
        // Server's notLoaded status; archiving also cannot imply completion.
        for (status, archived, expected) in [
            ("notLoaded", false, "Status unknown"),
            ("unknown", false, "Status unknown"),
            ("interrupted", true, "Archived · Stopped"),
            ("failed", true, "Archived · Failed"),
            ("completed", true, "Archived · Completed"),
            ("notLoaded", true, "Archived · Status unknown"),
        ] {
            let data = try JSONSerialization.data(withJSONObject: [
                "parentConversationId": "project parent", "threadId": "child/thread",
                "title": "Scout", "agentNickname": "Scout", "agentRole": "research",
                "status": status, "isArchived": archived, "canAcceptDirectInput": false,
            ])
            XCTAssertEqual(try JSONDecoder().decode(ProjectSubagentSummary.self, from: data).statusLabel,
                           expected, "\(status), archived=\(archived)")
        }
        XCTAssertEqual(try ProjectSubagentPaths.roster(parentConversationId: child.parentConversationId),
                       "/api/v1/project-conversations/project%20parent/subagents")
        XCTAssertEqual(try ProjectSubagentPaths.roster(parentConversationId: child.parentConversationId,
            archived: true, cursor: "older+page"),
            "/api/v1/project-conversations/project%20parent/subagents?archived=true&cursor=older%2Bpage")
        XCTAssertThrowsError(try ProjectSubagentPaths.roster(parentConversationId: child.parentConversationId,
            cursor: "older+page"))
        XCTAssertEqual(try ProjectSubagentPaths.transcript(parentConversationId: child.parentConversationId,
            threadId: child.threadId, cursor: "older+page"),
            "/api/v1/project-conversations/project%20parent/subagents/child%2Fthread/transcript?cursor=older%2Bpage")
        XCTAssertEqual(try ProjectSubagentPaths.transcript(parentConversationId: child.parentConversationId,
            threadId: child.threadId, kind: "t3Task"),
            "/api/v1/project-conversations/project%20parent/subagents/child%2Fthread/transcript?kind=t3Task")
        XCTAssertThrowsError(try ProjectSubagentPaths.transcript(parentConversationId: "", threadId: child.threadId))

        let read = try JSONDecoder().decode(ProjectSubagentTranscript.self, from: Data(#"{"subagent":{"parentConversationId":"project parent","threadId":"child/thread","title":"Scout","agentNickname":"Scout","agentRole":"research","status":"completed","isArchived":false,"canAcceptDirectInput":false},"snapshot":{"conversationId":"project-agent:project parent:child/thread","hostEpoch":"epoch","lastSequence":0,"messages":[],"assistantMessages":[],"thread":{"threadId":"child/thread","turns":[{"id":"turn","status":"completed","startedAt":null,"completedAt":null,"items":[{"id":"answer","type":"agentMessage","state":"completed","text":"Found the issue","createdAt":"2026-10-02T00:00:00Z","payload":{}}]}],"nextCursor":null,"hydrated":false},"events":[]}}"#.utf8))
        XCTAssertEqual(read.subagent.parentConversationId, child.parentConversationId)
        XCTAssertEqual(read.snapshot.rows(author: read.subagent.title).first?.text, "Found the issue")

        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ProjectSubagentResponseProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let fetchedRoster: ProjectSubagentList = try await api.request(
            ProjectSubagentPaths.roster(parentConversationId: child.parentConversationId),
            origin: "https://project-agents.invalid")
        XCTAssertEqual(fetchedRoster.subagents.first?.threadId, child.threadId)
        let fetchedRead: ProjectSubagentTranscript = try await api.request(
            ProjectSubagentPaths.transcript(parentConversationId: child.parentConversationId,
                                            threadId: child.threadId, cursor: "older+page"),
            origin: "https://project-agents.invalid")
        XCTAssertEqual(fetchedRead.snapshot.rows(author: child.title).first?.text, "Found the issue")
    }

    // Contract: changing Macs never keeps a destination, provider or settings
    // that belong to the previous Mac, and never overwrites a saved draft.
    func testSwitchingConnectionsCarriesTextOnly() {
        var outgoing = NewChatDraft(text: "Refactor the parser")
        outgoing.choose(.project(id: "mac-a-project"), project: project("mac-a-project", family: .claude), capabilities: ProviderCapabilities.published)
        let fresh = NewChatDraft.switching(from: outgoing, toSaved: nil)
        XCTAssertEqual(fresh.text, "Refactor the parser")
        XCTAssertNil(fresh.destination)
        XCTAssertNil(fresh.family)
        var saved = NewChatDraft(text: "Existing words")
        saved.choose(.project(id: "saved-project"), capabilities: ProviderCapabilities.published)
        XCTAssertEqual(NewChatDraft.switching(from: outgoing, toSaved: saved), saved)
    }

    // Contract: a submitted creation keeps its exact request identity and body
    // until confirmed, so a retry cannot create a second thread.
    func testSubmittedDraftIsFrozenUntilConfirmed() {
        var draft = NewChatDraft(text: "First message")
        draft.choose(.project(id: "p"), project: project("p", family: .claude), capabilities: ProviderCapabilities.published)
        XCTAssertEqual(draft.family, .claude)
        let request = draft.requestID
        draft.freeze()
        draft.chooseFamily(.codex, capabilities: .published(.codex))
        draft.choose(.project(id: "another-project"), capabilities: ProviderCapabilities.published)
        XCTAssertEqual(draft.family, .claude)
        XCTAssertEqual(draft.destination, .project(id: "p"))
        XCTAssertEqual(draft.requestID, request)
        XCTAssertNil(NewChatDraft.switching(from: draft, toSaved: nil).submittedBody)
        draft.completeSubmission()
        XCTAssertNotEqual(draft.requestID, request)
        XCTAssertEqual(draft.text, "")
        draft.text = "Rejected request"
        draft.freeze()
        let rejectedID = draft.requestID
        draft.rejectSubmission()
        XCTAssertFalse(draft.isSubmitted)
        XCTAssertNotEqual(draft.requestID, rejectedID)
        XCTAssertEqual(draft.text, "Rejected request")
    }

    // Contract: a New Chat note survives a draft restart and creation retry,
    // then binds to exactly the prepared conversation without crossing Project,
    // folder or Mac boundaries.
    func testNewChatPreviewNoteBindsOnlyAfterPreparedConversation() throws {
        var draft = NewChatDraft(destination: .project(id: "project"), text: "Keep this unsent note")
        let originalRequest = draft.requestID
        let source = Data("one\ntwo\n".utf8)
        let annotation = try ArtifactAnnotation(projectId: "project",
            conversationId: "new-chat:" + originalRequest, rootId: "workspace",
            path: "notes.md", source: source, startLine: 2, endLine: 2,
            note: "Check this line")
        let pending = try PendingNewChatAnnotation(annotation: annotation,
                                                   draftID: originalRequest, folderID: "folder", rootsRevision: 7)
        try draft.addAnnotation(pending)
        XCTAssertEqual(draft.folderId, "folder")
        XCTAssertEqual(draft.attachmentCount, 1)
        let restored = try JSONDecoder().decode(NewChatDraft.self, from: JSONEncoder().encode(draft))
        XCTAssertEqual(restored.annotations, [pending])
        // Pre-removal drafts may name an added account. Retiring that choice
        // must preserve the text, note, destination and creation retry identity.
        var legacy = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(draft)) as? [String: Any])
        legacy["accountId"] = "retired-work-sign-in"
        let migrated = try JSONDecoder().decode(NewChatDraft.self, from: JSONSerialization.data(withJSONObject: legacy))
        XCTAssertEqual(migrated, restored)
        XCTAssertNil(NewChatDraft.switching(from: restored, toSaved: nil).annotations)
        try draft.updateAnnotation(id: pending.id, note: "Check this line again")
        XCTAssertEqual(draft.annotations?.first?.id, pending.id,
                       "Editing the comment must keep its stable upload identity")

        draft.freeze()
        XCTAssertThrowsError(try draft.addAnnotation(pending))
        draft.rejectSubmission()
        XCTAssertNotEqual(draft.requestID, originalRequest)
        let file = try XCTUnwrap(draft.annotations?.first).file(
            conversationID: "real-conversation", projectID: "project",
            folderID: "folder", draftID: originalRequest, rootsRevision: 7)
        XCTAssertEqual(file.id, pending.id)
        XCTAssertEqual(file.data, try XCTUnwrap(draft.annotations?.first).file(
            conversationID: "real-conversation", projectID: "project",
            folderID: "folder", draftID: originalRequest, rootsRevision: 7).data,
            "A retry must upload the same note bytes under its stable ID")
        let bound = try ArtifactAnnotation.read(file.data)
        XCTAssertEqual(bound.conversationId, "real-conversation")
        XCTAssertEqual(bound.sourceSha256, ConversationFile.digest(source))
        XCTAssertEqual(bound.anchor, annotation.anchor)
        XCTAssertEqual(bound.note, "Check this line again")
        XCTAssertThrowsError(try pending.file(conversationID: "real-conversation",
            projectID: "other", folderID: "folder", draftID: originalRequest, rootsRevision: 7))
        XCTAssertThrowsError(try pending.file(conversationID: "real-conversation",
            projectID: "project", folderID: "other", draftID: originalRequest, rootsRevision: 7))
        XCTAssertThrowsError(try pending.file(conversationID: "real-conversation",
            projectID: "project", folderID: "folder", draftID: originalRequest, rootsRevision: 8),
            "Repointing the same folder ID with identical file bytes must not rebind the note")
        XCTAssertThrowsError(try pending.file(conversationID: "new-chat:placeholder",
            projectID: "project", folderID: "folder", draftID: originalRequest, rootsRevision: 7))
        draft.noteWorkspaceRevision(folderID: "other", draftID: originalRequest,
                                    rootID: "workspace", path: "notes.md", sha256: "changed")
        XCTAssertFalse(try XCTUnwrap(draft.annotations?.first).sourceChanged,
                       "A late refresh from another folder cannot change this note")
        draft.noteWorkspaceRevision(folderID: "folder", draftID: originalRequest,
                                    rootID: "workspace", path: "notes.md", sha256: "changed")
        XCTAssertTrue(try XCTUnwrap(draft.annotations?.first).sourceChanged)
        XCTAssertThrowsError(try XCTUnwrap(draft.annotations?.first).file(
            conversationID: "real-conversation", projectID: "project",
            folderID: "folder", draftID: originalRequest, rootsRevision: 7))
        draft.chooseFolder("other")
        XCTAssertNil(draft.annotations)
    }

    // Contract: an import completing after a note was saved cannot exceed the
    // same four-slot cap that applies to adding a note after an import.
    func testNewChatAttachmentAndNoteInterleavingKeepsFourSlotLimit() throws {
        var draft = NewChatDraft(destination: .project(id: "project"))
        let attachments = try (1...4).map { number in
            NewChatAttachment(try StagedFile(name: "\(number).txt", mimeType: "text/plain",
                                             data: Data("\(number)".utf8)))
        }
        try draft.addAttachments(Array(attachments.prefix(3)))
        let annotation = try ArtifactAnnotation(projectId: "project",
            conversationId: "new-chat:" + draft.requestID, rootId: "workspace",
            path: "notes.md", source: Data("one\n".utf8), startLine: 1, endLine: 1, note: "Check")
        try draft.addAnnotation(PendingNewChatAnnotation(annotation: annotation,
            draftID: draft.requestID, folderID: "folder", rootsRevision: 1))
        XCTAssertThrowsError(try draft.addAttachments([attachments[3]]))
        XCTAssertEqual(draft.attachmentCount, 4)
    }

    // Contract: the tree is one flat list with stable IDs; paging appends
    // without reordering, pinned threads leave the project list, and
    // collapsed projects load nothing.
    func testNativeSidebarMoveOffsetsRespectSectionsAndProjectSubtrees() {
        let rows: [SidebarRow] = [
            .pinnedHeader,
            .pinned(hostID: "a", projectID: "p", projectName: "P", hostName: nil, thread: thread("codex:pin-a", pinned: true), isSelected: false),
            .pinned(hostID: "b", projectID: "p", projectName: "P", hostName: nil, thread: thread("codex:pin-b", pinned: true), isSelected: false),
            .automations,
            .project(hostID: "a", project: project("p"), isExpanded: true, isSelected: false),
            .thread(hostID: "a", projectID: "p", thread: thread("codex:one"), isSelected: false),
            .thread(hostID: "a", projectID: "p", thread: thread("codex:two"), isSelected: false),
            .project(hostID: "a", project: project("q"), isExpanded: true, isSelected: false),
            .thread(hostID: "a", projectID: "q", thread: thread("codex:three"), isSelected: false),
            .moreThreads(hostID: "a", projectID: "q", isLoading: false),
            .project(hostID: "b", project: project("p"), isExpanded: false, isSelected: false)
        ]
        let pin = SidebarProjection.moveTarget(rows: rows, source: 1, destination: 3)
        XCTAssertEqual(pin?.target.id, rows[2].id); XCTAssertEqual(pin?.after, true)
        XCTAssertNil(SidebarProjection.moveTarget(rows: rows, source: 1, destination: 4))
        // Projects of every computer are one group: a Project can cross to another computer's.
        let block = SidebarProjection.moveTarget(rows: rows, source: 4, destination: 9)
        XCTAssertEqual(block?.target.id, rows[10].id); XCTAssertEqual(block?.after, false)
        XCTAssertNil(SidebarProjection.moveTarget(rows: rows, source: 4, destination: 12))
        let before = SidebarProjection.moveTarget(rows: rows, source: 7, destination: 4)
        XCTAssertEqual(before?.target.id, rows[4].id); XCTAssertEqual(before?.after, false)
        let thread = SidebarProjection.moveTarget(rows: rows, source: 5, destination: 7)
        XCTAssertEqual(thread?.target.id, rows[6].id); XCTAssertEqual(thread?.after, true)
        XCTAssertNil(SidebarProjection.moveTarget(rows: rows, source: 5, destination: 8))
        XCTAssertNil(SidebarProjection.moveTarget(rows: rows, source: 0, destination: 1))
        XCTAssertNil(SidebarProjection.moveTarget(rows: rows, source: 99, destination: 1))
    }

    func testUntouchedSidebarSectionsFollowActivityUntilTheirOwnFirstMove() throws {
        let a = SidebarOrderID(host: "mac", item: "a"), b = SidebarOrderID(host: "mac", item: "b")
        var order = SidebarOrder()
        for section in SidebarOrder.Section.allCases {
            order.observe([a, b], in: section)
            XCTAssertEqual(order.sorted([b, a], in: section, id: { $0 }), [b, a])
            XCTAssertFalse(order.isManual(section))
        }
        XCTAssertFalse(order.move(a, relativeTo: b, after: false, visible: [a, b], in: .chats))
        XCTAssertFalse(order.isManual(.chats), "Dropping back in the same slot is not a manual arrangement")
        var legacy = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(order)) as? [String: Any])
        legacy.removeValue(forKey: "manualSections")
        let restoredLegacy = try JSONDecoder().decode(SidebarOrder.self, from: JSONSerialization.data(withJSONObject: legacy))
        XCTAssertEqual(restoredLegacy.ids(in: .chats), [], "Unmarked pre-launch recency snapshots have no saved user intent")
        XCTAssertEqual(restoredLegacy.sorted([b, a], in: .chats, id: { $0 }), [b, a])
        order.move(a, relativeTo: b, after: true, visible: [a, b], in: .chats)
        order = try JSONDecoder().decode(SidebarOrder.self, from: JSONEncoder().encode(order))
        XCTAssertEqual(order.sorted([a, b], in: .chats, id: { $0 }), [b, a])
        for section in [SidebarOrder.Section.projects, .pinned] {
            XCTAssertEqual(order.sorted([b, a], in: section, id: { $0 }), [b, a])
            XCTAssertEqual(order.sorted([a, b], in: section, id: { $0 }), [a, b])
        }
    }

    func testRemoteRemovalRequiresCompleteCatalogAndSurvivesRelaunch() throws {
        let a = SidebarOrderID(host: "mac", item: "codex:a"), b = SidebarOrderID(host: "mac", item: "codex:b")
        let offline = SidebarOrderID(host: "offline", item: "codex:a")
        var order = SidebarOrder()
        order.observe([a, b, offline], in: .chats)
        order.move(b, relativeTo: a, after: false, visible: [a, b], in: .chats)
        order.reconcileThreads(host: "mac", project: "p", threads: [thread("codex:a"), thread("codex:b")], complete: true)
        order.reconcileThreads(host: "mac", project: "p", threads: [thread("codex:b")], complete: false)
        XCTAssertEqual(order.ids(in: .chats), [b, a, offline])
        order = try JSONDecoder().decode(SidebarOrder.self, from: JSONEncoder().encode(order))
        order.reconcileThreads(host: "mac", project: "p", threads: [thread("codex:b")], complete: true)
        XCTAssertEqual(order.ids(in: .chats), [b, offline])
    }

    func testRemoteUnpinAndProjectDeletionPruneOnlyTheirComputer() {
        let a = SidebarOrderID(host: "mac", item: "codex:a"), b = SidebarOrderID(host: "mac", item: "codex:b")
        let offline = SidebarOrderID(host: "offline", item: "codex:a")
        var order = SidebarOrder()
        for section in [SidebarOrder.Section.chats, .pinned] {
            order.observe([a, b, offline], in: section)
            order.move(b, relativeTo: a, after: false, visible: [a, b], in: section)
        }
        order.reconcileThreads(host: "mac", project: "p", threads: [thread("codex:a"), thread("codex:b")], complete: true)
        order.reconcilePinned(host: "mac", threads: [thread("codex:b")])
        XCTAssertEqual(order.ids(in: .pinned), [b, offline])
        XCTAssertEqual(order.ids(in: .chats), [b, a, offline], "Unpinning keeps the chat's manual slot")
        order.removeProjects(host: "mac", ids: ["p"])
        XCTAssertEqual(order.ids(in: .chats), [offline])
        XCTAssertEqual(order.ids(in: .pinned), [offline])
    }

    func testSidebarOrderInsertsNewRowsAtTopAndIgnoresActivityChanges() {
        let a = SidebarOrderID(host: "a", item: "1"), b = SidebarOrderID(host: "a", item: "2")
        let new = SidebarOrderID(host: "a", item: "new")
        var order = SidebarOrder()
        XCTAssertTrue(order.move(a, relativeTo: b, after: false, visible: [b, a], in: .chats))
        XCTAssertFalse(order.observe([b, a], in: .chats))
        XCTAssertEqual(order.sorted([b, a], in: .chats, id: { $0 }), [a, b])
        XCTAssertTrue(order.observe([new], in: .chats))
        XCTAssertFalse(order.observe([b, a, new], in: .chats), "a new local chat keeps first place even when computer clocks disagree")
        XCTAssertEqual(order.ids(in: .chats), [new, a, b])
    }

    func testSidebarOrderPagingAppendsOlderRowsWithoutMovingTheOwnersRows() {
        let a = SidebarOrderID(host: "mac", item: "a"), b = SidebarOrderID(host: "mac", item: "b")
        let older = SidebarOrderID(host: "mac", item: "older"), new = SidebarOrderID(host: "mac", item: "new")
        var order = SidebarOrder()
        order.observe([a, b], in: .chats)
        order.move(b, relativeTo: a, after: false, visible: [a, b], in: .chats)
        order.observe([a, b, older], in: .chats)
        XCTAssertEqual(order.ids(in: .chats), [b, a, older], "Show more must append older rows, preserving the manual arrangement")
        order.observe([new, a, b, older], in: .chats)
        XCTAssertEqual(order.ids(in: .chats), [new, b, a, older], "a genuinely new leading chat still goes first")
        // Another computer can have a faster clock and lead the combined input.
        // It must not make this computer's genuinely new first row look paged.
        let ahead = SidebarOrderID(host: "ahead", item: "known")
        let newest = SidebarOrderID(host: "mac", item: "newest")
        order.observe([ahead, new, a, b, older], in: .chats)
        order.observe([ahead, newest, new, a, b, older], in: .chats)
        XCTAssertEqual(order.sorted([a, b, older, new, newest], in: .chats, id: { $0 }), [newest, new, b, a, older])
    }

    func testSidebarOrderMoveKeepsDisconnectedSlotsAndScopesDuplicateIDsByComputer() throws {
        let a = SidebarOrderID(host: "a", item: "same"), offline = SidebarOrderID(host: "b", item: "same")
        let c = SidebarOrderID(host: "a", item: "c")
        var order = SidebarOrder()
        order.observe([a, offline, c], in: .pinned)
        XCTAssertTrue(order.move(c, relativeTo: a, after: false, visible: [a, c], in: .pinned))
        XCTAssertEqual(order.ids(in: .pinned), [c, offline, a])
        let restored = try JSONDecoder().decode(SidebarOrder.self, from: JSONEncoder().encode(order))
        XCTAssertEqual(restored.sorted([a, offline, c], in: .pinned, id: { $0 }), [c, offline, a])
        XCTAssertEqual(restored.ids(in: .chats), [], "sections remain independent")
    }

    func testSidebarOrderSurvivesAttachmentAndPreparedThreadReferenceChanges() throws {
        let first = SidebarOrderID(host: "mac", item: "codex:first")
        let prepared = SidebarOrderID(host: "mac", item: "wonder:draft")
        let native = SidebarOrderID(host: "mac", item: "claude:session")
        var order = SidebarOrder()
        order.move(first, relativeTo: prepared, after: false, visible: [prepared, first], in: .chats)
        order.move(prepared, relativeTo: first, after: false, visible: [first, prepared], in: .pinned)
        // Decode an earlier document without aliases, retaining its arrangement.
        var legacy = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(order)) as? [String: Any])
        legacy.removeValue(forKey: "threadAliases")
        order = try JSONDecoder().decode(SidebarOrder.self, from: JSONSerialization.data(withJSONObject: legacy))
        XCTAssertTrue(order.recordThread(host: "mac", thread: thread("wonder:draft", conversation: "draft")))
        order = try JSONDecoder().decode(SidebarOrder.self, from: JSONEncoder().encode(order))
        XCTAssertTrue(order.recordThread(host: "mac", thread: thread("claude:session", conversation: "draft", family: .claude)))
        XCTAssertFalse(order.observe([native, first], in: .chats), "joining is the same chat, not a new insertion")
        XCTAssertEqual(order.sorted([native, first], in: .chats, id: { $0 }), [first, native])
        XCTAssertEqual(order.sorted([first, native], in: .pinned, id: { $0 }), [native, first])
        XCTAssertTrue(order.move(native, relativeTo: first, after: false, visible: [first, native], in: .chats))
        XCTAssertEqual(order.sorted([first, prepared], in: .chats, id: { $0 }), [prepared, first], "a stale cached reference resolves to the same slot")
        XCTAssertTrue(order.recordThread(host: "mac", thread: thread("codex:first", conversation: "attached")))
        XCTAssertEqual(order.sorted([first, native], in: .chats, id: { $0 }), [native, first], "attaching an imported native thread preserves its position")
        let other = SidebarOrderID(host: "other", item: native.item)
        XCTAssertTrue(order.observe([other], in: .chats))
        XCTAssertEqual(order.sorted([first, native, other], in: .chats, id: { $0 }), [other, native, first])
        XCTAssertTrue(order.remove([native], from: .chats))
        XCTAssertEqual(order.sorted([first, other], in: .chats, id: { $0 }), [other, first])
    }

    func testSidebarOrderUnknownIDsAndDeletionNeverInventRows() {
        let a = SidebarOrderID(host: "a", item: "a"), b = SidebarOrderID(host: "a", item: "b")
        let unknown = SidebarOrderID(host: "a", item: "unknown")
        var order = SidebarOrder()
        order.observe([a, b], in: .projects)
        XCTAssertFalse(order.move(unknown, relativeTo: a, after: true, visible: [a, b], in: .projects))
        XCTAssertFalse(order.move(a, relativeTo: unknown, after: true, visible: [a, b], in: .projects))
        XCTAssertFalse(order.move(a, relativeTo: b, after: false, visible: [a, a, b], in: .projects))
        XCTAssertEqual(order.sorted([unknown, b], in: .projects, id: { $0 }), [unknown, b])
        XCTAssertEqual(order.sorted([b], in: .projects, id: { $0 }), [b])
        XCTAssertFalse(order.observe([b], in: .projects))
        XCTAssertEqual(order.ids(in: .projects), [a, b], "a missing page or hidden item is not a deletion")
        XCTAssertTrue(order.remove([a], from: .projects))
        XCTAssertEqual(order.ids(in: .projects), [b])
        XCTAssertFalse(order.remove([a], from: .projects))
    }

    func testSidebarOrderMovesBothDirectionsWithinVisibleSubset() {
        let ids = (0..<5).map { SidebarOrderID(host: "mac", item: String($0)) }
        var order = SidebarOrder()
        order.observe(ids, in: .chats)
        XCTAssertTrue(order.move(ids[0], relativeTo: ids[4], after: true, visible: ids, in: .chats))
        XCTAssertEqual(order.ids(in: .chats), Array(ids.dropFirst()) + [ids[0]])
        XCTAssertTrue(order.move(ids[0], relativeTo: ids[1], after: false, visible: ids, in: .chats))
        XCTAssertEqual(order.ids(in: .chats), ids)
        XCTAssertFalse(order.move(ids[0], relativeTo: ids[1], after: false, visible: ids, in: .chats))
    }

    func testSidebarManualOrderProjectsMoveAsBlocksAndThreadsSurviveLayoutChanges() {
        var state = ProjectThreadsState()
        state.apply(ProjectThreadsPage(threads: [thread("codex:1"), thread("codex:2")], nextCursor: nil, partial: []), replacing: true)
        let host = SidebarHostProjects(hostID: "mac", name: "Mac", isOnline: true, supportsProjects: true,
                                      projects: [project("a"), project("b")], threads: ["a": state])
        var order = SidebarOrder()
        order.move(SidebarOrderID(host: "mac", item: "b"), relativeTo: SidebarOrderID(host: "mac", item: "a"), after: false, visible: ["a", "b"].map { SidebarOrderID(host: "mac", item: $0) }, in: .projects)
        order.move(SidebarOrderID(host: "mac", item: "codex:2"), relativeTo: SidebarOrderID(host: "mac", item: "codex:1"), after: false, visible: ["codex:1", "codex:2"].map { SidebarOrderID(host: "mac", item: $0) }, in: .chats)
        let rows = SidebarProjection.rows(hosts: [host], expanded: ["mac/a"], selectedConversation: nil, order: order)
        XCTAssertEqual(rows.map(\.id), ["automations", "project:mac:b", "project:mac:a", "thread:mac:codex:2", "thread:mac:codex:1"])
        let chats = SidebarProjection.rows(hosts: [host], expanded: [], selectedConversation: nil, layout: .allChats, order: order)
        XCTAssertEqual(chats.compactMap(\.orderItem).map(\.item), ["codex:2", "codex:1"])
        XCTAssertEqual(Set(rows.map(\.id)).intersection(chats.map(\.id)).count, 3, "thread identities survive grouping")
    }

    func testSidebarManualPinnedOrderSpansComputersAndRetainsOfflineRows() {
        let a = SidebarHostProjects(hostID: "a", name: "A", isOnline: true, supportsProjects: true,
            projects: [project("p")], threads: [:], pinned: [PinnedProjectThread(projectId: "p", thread: thread("codex:1", pinned: true))])
        let b = SidebarHostProjects(hostID: "b", name: "B", isOnline: false, supportsProjects: true,
            projects: [project("p")], threads: [:], pinned: [PinnedProjectThread(projectId: "p", thread: thread("codex:1", pinned: true))])
        var order = SidebarOrder()
        order.move(SidebarOrderID(host: "b", item: "codex:1"), relativeTo: SidebarOrderID(host: "a", item: "codex:1"), after: false, visible: [SidebarOrderID(host: "a", item: "codex:1"), SidebarOrderID(host: "b", item: "codex:1")], in: .pinned)
        let rows = SidebarProjection.pinnedRows(hosts: [a, b], selectedConversation: nil, order: order)
        XCTAssertEqual(rows.compactMap(\.orderItem).map(\.host), ["b", "a"])
        XCTAssertEqual(Set(rows.map(\.id)).count, rows.count)
        XCTAssertEqual(SidebarProjection.pinnedRows(hosts: [a], selectedConversation: nil, order: order).compactMap(\.orderItem).map(\.host), ["a"])
    }

    func testSidebarRowsAreStableAcrossPaging() {
        var state = ProjectThreadsState()
        state.apply(ProjectThreadsPage(threads: [thread("codex:1", conversation: "c1"), thread("claude:2", pinned: true, family: .claude)], nextCursor: "next", partial: []), replacing: true)
        let host = SidebarHostProjects(hostID: "mac", name: "MacBook Pro", isOnline: true, supportsProjects: true,
                                       projects: [project("b"), project("a", pinned: true), project("hidden", included: false)], threads: ["a": state])
        let key = SidebarProjection.expansionKey(host: "mac", project: "a")
        let first = SidebarProjection.projectRows(hosts: [host], expanded: [key], selectedConversation: "c1", selectedProject: nil)
        XCTAssertEqual(first.map(\.id), ["automations", "project:mac:a", "thread:mac:codex:1", "more:mac:a", "project:mac:b"])
        if case .thread(_, _, _, let selected) = first[2] { XCTAssertTrue(selected) } else { XCTFail() }
        state.apply(ProjectThreadsPage(threads: [thread("codex:1", conversation: "c1"), thread("codex:3")], nextCursor: nil, partial: []), replacing: false)
        let paged = SidebarProjection.projectRows(hosts: [SidebarHostProjects(hostID: "mac", name: "MacBook Pro", isOnline: true, supportsProjects: true, projects: host.projects, threads: ["a": state])], expanded: [key], selectedConversation: nil, selectedProject: nil)
        XCTAssertEqual(Array(paged.map(\.id).prefix(4)), ["automations", "project:mac:a", "thread:mac:codex:1", "thread:mac:codex:3"])
        XCTAssertFalse(paged.map(\.id).contains("more:mac:a"))
        state.apply(ProjectThreadsPage(threads: [], nextCursor: nil, partial: [ProjectPartialFailure(family: .codex, detail: "Try again")]), replacing: true)
        XCTAssertEqual(state.threads.map(\.reference), ["codex:1", "codex:3"], "A provider timeout must retain its cached conversations")
    }

    // Contract: a thread archived elsewhere (the Mac, another device, a
    // script) leaves Chats with the first complete answer that no longer
    // lists it, wherever the phone held it: page 1, a Show more page, or
    // the offline cache; in By Project and All Chats. A still-reading answer
    // proves nothing missing and keeps it. Owner report 2026-10-22: 243 chats
    // archived from the Mac stayed in the phone's sidebar.
    func testExternallyArchivedRowsLeaveOnACompleteAnswer() {
        func dated(_ reference: String, _ at: Int) -> ProjectThreadSummary {
            ProjectThreadSummary(reference: reference, conversationId: reference, title: "Chat " + reference, family: .codex, updatedAt: at)
        }
        let all = (1...40).map { dated("t\($0)", 1000 - $0) }
        // Page 1 (30 rows) plus a Show more page.
        var shown = ProjectThreadsState()
        shown.apply(ProjectThreadsPage(threads: Array(all[0..<30]), nextCursor: "p2", partial: []), replacing: true)
        shown.apply(ProjectThreadsPage(threads: Array(all[30..<40]), nextCursor: nil, partial: []), replacing: false)
        XCTAssertEqual(shown.refreshLimit, ProjectThreadsState.maxPage)
        // t2 (page 1) and t35 (Show more page) are archived on the Mac.
        let remaining = all.filter { !["t2", "t35"].contains($0.reference) }
        func refreshed(_ start: ProjectThreadsState, loading: Bool) -> ProjectThreadsState {
            var next = start
            let rows = loading ? [] : Array(remaining.prefix(start.refreshLimit))
            next.apply(ProjectThreadsPage(threads: rows, nextCursor: "c", partial: [], loading: loading), replacing: true)
            return next
        }
        func ids(_ state: ProjectThreadsState) -> Set<String> {
            let host = SidebarHostProjects(hostID: "mac", name: "Mac", isOnline: true, supportsProjects: true,
                                           projects: [project("a")], threads: ["a": state])
            let key = SidebarProjection.expansionKey(host: "mac", project: "a")
            let byProject = Set(SidebarProjection.projectRows(hosts: [host], expanded: [key], selectedConversation: nil, selectedProject: nil).map(\.id))
            let allChats = Set(SidebarProjection.chatRows(hosts: [host], selectedConversation: nil).map(\.id))
            XCTAssertEqual(byProject.filter { $0.hasPrefix("thread:") }, allChats.filter { $0.hasPrefix("thread:") },
                           "By Project and All Chats agree on which chats exist")
            return Set(state.threads.map(\.reference))
        }
        // Still reading: nothing is proven gone, so nothing leaves.
        let reading = refreshed(shown, loading: true)
        XCTAssertTrue(ids(reading).isSuperset(of: ["t2", "t35"]), "A still-reading answer keeps every row it lacks")
        // Complete: the answer is the truth for page 1 and the pages it covers.
        for start in [shown, reading] {
            let complete = refreshed(start, loading: false)
            XCTAssertTrue(ids(complete).isDisjoint(with: ["t2", "t35"]), "An archived row must leave on a complete answer")
            XCTAssertTrue(ids(complete).contains("t1"))
            XCTAssertNotNil(complete.nextCursor, "Older rows stay reachable through Show more")
        }
        // Offline cache: restored rows show until the Mac answers, then only what it lists.
        let restored = ProjectThreadsState(saved: Array(all.prefix(15)))
        XCTAssertTrue(ids(restored).contains("t2"))
        XCTAssertFalse(ids(refreshed(restored, loading: false)).contains("t2"))
        XCTAssertTrue(ids(refreshed(restored, loading: true)).contains("t2"))
        // New folders restart paging; the first complete page still decides.
        XCTAssertFalse(ids(refreshed(shown.restartingPaging(), loading: false)).contains("t2"))
    }

    // Contract: pinning changes placement, never the identity of a native
    // thread. Attachment, stale catalog/page overlap and repeated Project
    // membership must not replace it, duplicate it or select another Mac.
    func testPinMovementRetainsNativeIdentityAcrossLayoutsAndAttachment() throws {
        for family in AgentFamily.allCases {
            let reference = family.rawValue + ":same-native-session"
            func summary(pinned: Bool, attached: Bool) -> ProjectThreadSummary {
                ProjectThreadSummary(reference: reference, conversationId: attached ? "attached-chat" : nil,
                    title: "Review tests", family: family, updatedAt: 10, isPinned: pinned,
                    hasUnread: true, isWorking: true, needsInputOnMac: true)
            }
            func host(_ id: String, pinned: Bool, attached: Bool, stalePage: Bool = false, duplicateProject: Bool = false) -> SidebarHostProjects {
                var page = ProjectThreadsState()
                page.apply(ProjectThreadsPage(threads: [summary(pinned: stalePage ? !pinned : pinned, attached: attached)],
                    nextCursor: nil, partial: []), replacing: true)
                return SidebarHostProjects(hostID: id, name: id, isOnline: true, supportsProjects: true,
                    projects: duplicateProject ? [project("a"), project("b")] : [project("a")],
                    threads: duplicateProject ? ["a": page, "b": page] : ["a": page],
                    pinned: pinned ? [PinnedProjectThread(projectId: "a", thread: summary(pinned: true, attached: attached))] : [])
            }
            func threadRows(_ rows: [SidebarRow]) -> [(id: String, thread: ProjectThreadSummary, selected: Bool)] {
                rows.compactMap {
                    switch $0 {
                    case .pinned(_, _, _, _, let thread, let selected), .thread(_, _, let thread, let selected), .chat(_, _, _, let thread, let selected):
                        return ($0.id, thread, selected)
                    default: return nil
                    }
                }
            }
            var identity: String?
            for layout in [SidebarLayout.byProject, .allChats] {
                for (pin, attached, stale) in [(false, false, false), (true, false, true), (true, true, true), (false, true, false)] {
                    let hosts = [host("studio", pinned: pin, attached: attached, stalePage: stale),
                                 host("laptop", pinned: pin, attached: attached, stalePage: stale)]
                    let rows = SidebarProjection.rows(hosts: hosts,
                        expanded: [SidebarProjection.expansionKey(host: "studio", project: "a"), SidebarProjection.expansionKey(host: "laptop", project: "a")],
                        selectedHost: "studio", selectedConversation: "attached-chat", search: "Review", layout: layout)
                    let threads = threadRows(rows)
                    XCTAssertEqual(threads.count, 2, "One row per Mac, including catalog/page overlap")
                    XCTAssertEqual(Set(rows.map(\.id)).count, rows.count)
                    let studio = try XCTUnwrap(threads.first { $0.id.contains("studio") })
                    if let identity { XCTAssertEqual(studio.id, identity, "Pin, layout and attachment move the same thread") }
                    else { identity = studio.id }
                    XCTAssertEqual(studio.selected, attached)
                    XCTAssertFalse(try XCTUnwrap(threads.first { $0.id.contains("laptop") }).selected)
                    XCTAssertTrue(threads.allSatisfy { $0.thread.hasUnread && $0.thread.isWorking && $0.thread.waitsOnMac })
                }
                let repeated = SidebarProjection.rows(hosts: [host("studio", pinned: false, attached: true, duplicateProject: true)],
                    expanded: [SidebarProjection.expansionKey(host: "studio", project: "a"), SidebarProjection.expansionKey(host: "studio", project: "b")],
                    selectedHost: "studio", selectedConversation: "attached-chat", layout: layout)
                XCTAssertEqual(threadRows(repeated).count, 1, "A native session reported in two Projects still has one sidebar row")
            }
        }
    }

    // Contract: only live requests mapped by the host to this exact conversation
    // require input; expiry, answered questions and missing native IDs stay out.
    func testSessionNeedsInputUsesVerifiedIdentityAndExpiry() throws {
        let request = try JSONDecoder().decode(AttentionRequest.self, from: Data(#"{"approvalId":"approval","method":"item/commandExecution/requestApproval","conversationId":"chat","actionNonce":"fixture","params":{"expiresAtMs":200,"availableDecisions":["accept","decline"]}}"#.utf8))
        let question = try JSONDecoder().decode(AsyncQuestion.self, from: Data(#"{"id":"question","conversationId":"chat","turnId":"turn","itemId":"item","questions":[],"state":"pending","expiresAtMs":300}"#.utf8))
        XCTAssertTrue(SidebarSessionFilter.awaitsInput(conversation: "chat", requests: [request], questions: [], now: 199))
        XCTAssertFalse(SidebarSessionFilter.awaitsInput(conversation: "chat", requests: [request], questions: [], now: 200))
        XCTAssertTrue(SidebarSessionFilter.awaitsInput(conversation: "chat", requests: [], questions: [question], now: 299))
        XCTAssertFalse(SidebarSessionFilter.awaitsInput(conversation: "chat", requests: [request], questions: [question], now: 300))
        for id in [nil, "", "other"] as [String?] {
            XCTAssertFalse(SidebarSessionFilter.awaitsInput(conversation: id, requests: [request], questions: [question], now: 100))
        }
    }

    func testAttentionInboxAcrossComputersClearsAndExpires() throws {
        let request = try JSONDecoder().decode(AttentionRequest.self, from: Data(#"{"approvalId":"same","method":"item/commandExecution/requestApproval","conversationId":"chat","actionNonce":"fixture","params":{"expiresAtMs":200}}"#.utf8))
        let question = try JSONDecoder().decode(AsyncQuestion.self, from: Data(#"{"id":"question","conversationId":"chat","turnId":"turn","itemId":"item","questions":[],"state":"pending","expiresAtMs":300}"#.utf8))
        let computers: [AttentionInboxProjection.Computer] = [
            .init(host: "a", requests: [request, request], questions: [question]),
            .init(host: "b", requests: [request], questions: [], failedReplies: ["same"])
        ]
        let items = AttentionInboxProjection.items(computers, now: 100)
        XCTAssertEqual(items.count, 3)
        XCTAssertEqual(Set(items.map(\.id)).count, 3)
        XCTAssertEqual(items.last?.kind, .failedReply)
        XCTAssertEqual(AttentionInboxProjection.items(computers, now: 200).map(\.request), ["question"])
        XCTAssertTrue(AttentionInboxProjection.items(computers, now: 300).isEmpty)
        XCTAssertTrue(AttentionInboxProjection.items([.init(host: "a", requests: [], questions: [])], now: 100).isEmpty)
        let outcome = AttentionInboxProjection.ReviewOutcome(conversation: "chat", item: "reply", failed: false)
        XCTAssertEqual(AttentionInboxProjection.items([.init(host: "a", requests: [], questions: [], outcomes: [outcome])], now: 100).first?.kind, .finished)
    }

    // Contract: only the 30 most recently opened project conversations keep
    // their saved history; reopening moves one to the front instead of duplicating it.
    func testRecentlyOpenedConversationsAreBounded() {
        var recent = RecentlyOpenedConversations()
        for index in 1...31 { recent.note("c\(index)") }
        XCTAssertEqual(recent.ids.count, RecentlyOpenedConversations.limit)
        XCTAssertEqual(recent.ids.first, "c31")
        XCTAssertFalse(recent.ids.contains("c1"))
        recent.note("c2")
        XCTAssertEqual(Array(recent.ids.prefix(2)), ["c2", "c31"])
        XCTAssertEqual(recent.ids.count, RecentlyOpenedConversations.limit)
        let restored = try? JSONDecoder().decode(RecentlyOpenedConversations.self, from: JSONEncoder().encode(recent))
        XCTAssertEqual(restored, recent)
    }

}

// Composer contracts for project threads: access choices, reasoning defaults
// and where a draft belongs when a Project is removed.
final class ProjectComposerContractTests: XCTestCase {
    private func models(_ json: String) throws -> [AgentOptions.Model] {
        try JSONDecoder().decode(AgentOptions.self, from: Data(#"{"models":\#(json),"timezone":"UTC","allowedApprovalPolicies":[]}"#.utf8)).models
    }
    private func catalog() throws -> [AgentOptions.Model] {
        try models(#"""
        [{"id":"claude:haiku","displayName":"Haiku","hidden":false,"reasoningEfforts":[],"agentFamily":"claude"},
         {"id":"claude:sonnet","displayName":"Sonnet","hidden":false,"reasoningEfforts":[{"id":"low","label":"Low"},{"id":"medium","label":"Medium"},{"id":"high","label":"High"}],"defaultReasoningEffort":"high","isDefault":true,"agentFamily":"claude"},
         {"id":"gpt-hidden","displayName":"Hidden","hidden":true,"reasoningEfforts":[],"agentFamily":"codex"},
         {"id":"gpt-a","displayName":"GPT A","hidden":false,"reasoningEfforts":[{"id":"low","label":"Low"},{"id":"medium","label":"Medium"},{"id":"xhigh","label":"xhigh"}],"defaultReasoningEffort":"medium","isDefault":true,"agentFamily":"codex"},
         {"id":"gpt-b","displayName":"GPT B","hidden":false,"reasoningEfforts":[{"id":"low","label":"Low"},{"id":"high","label":"High"}],"defaultReasoningEffort":"high","agentFamily":"codex"},
         {"id":"gpt-c","displayName":"GPT C","hidden":false,"reasoningEfforts":[{"id":"minimal","label":"Minimal"},{"id":"low","label":"Low"}],"defaultReasoningEffort":"minimal","agentFamily":"codex"}]
        """#)
    }
    private func visible(_ family: AgentFamily, _ all: [AgentOptions.Model]) -> [AgentOptions.Model] {
        all.filter { !$0.hidden && $0.family == family }
    }

    func testAccessChoicesChangeOnlyWhatTheHostAccepts() {
        func patch(_ choice: ProjectAccessChoice, from access: ProjectAccess, _ family: AgentFamily, modes: Bool = true) -> [String: String] {
            let next = choice.result(from: access, capabilities: .published(family), supportsModes: modes)
            return access.changes(to: next, capabilities: .published(family), supportsModes: modes).mapValues { "\($0)" }
        }
        XCTAssertEqual(patch(.acceptEdits, from: ProjectAccess(), .claude), ["claudeApproval": "accept_edits"])
        XCTAssertEqual(patch(.plan, from: ProjectAccess(), .claude), ["planMode": "true"])
        XCTAssertEqual(patch(.manual, from: ProjectAccess(claudeApproval: .ask, planMode: true), .claude), ["planMode": "false"])
        XCTAssertEqual(patch(.plan, from: ProjectAccess(planMode: true), .claude), ["planMode": "false"], "Choosing Plan again leaves it")
        XCTAssertEqual(patch(.fullAccess, from: ProjectAccess(planMode: true), .claude), ["accessMode": "full_access", "planMode": "false"])
        XCTAssertEqual(patch(.auto, from: ProjectAccess(accessMode: .fullAccess, claudeApproval: .ask), .claude),
                       ["accessMode": "workspace", "claudeApproval": "auto"])
        XCTAssertEqual(patch(.fullAccess, from: ProjectAccess(planMode: true), .codex), ["accessMode": "full_access"], "A Codex access level leaves plan mode alone")
        XCTAssertEqual(patch(.manual, from: ProjectAccess(claudeApproval: .auto), .codex), [:], "Codex threads reject claudeApproval")
        XCTAssertEqual(patch(.fullAccess, from: ProjectAccess(), .claude, modes: false), ["accessMode": "full_access"])
        XCTAssertEqual(patch(.plan, from: ProjectAccess(), .claude, modes: false), [:], "Older hosts reject planMode")
        XCTAssertEqual(patch(.auto, from: ProjectAccess(), .claude), [:], "Choosing the current row saves nothing")
        XCTAssertEqual(patch(.fullAccess, from: ProjectAccess(), .opencode), [:])
        XCTAssertEqual(patch(.manual, from: ProjectAccess(), .opencode), [:])
        XCTAssertEqual(patch(.plan, from: ProjectAccess(), .opencode), ["planMode": "true"])
        var draft = NewChatDraft(family: .codex, accessMode: .fullAccess)
        draft.chooseFamily(.opencode, capabilities: .published(.opencode))
        XCTAssertEqual(draft.accessMode, .workspace, "An unsupported permission must not follow a provider change")
    }

    // Contract: the draft, the created thread and the menu read the same fields.
    func testDraftAccessAndDetailShareOneShape() throws {
        var draft = NewChatDraft(family: .claude)
        draft.access = ProjectAccessChoice.acceptEdits.result(from: draft.access, capabilities: .published(.claude), supportsModes: true)
        XCTAssertEqual([draft.accessMode.rawValue, draft.claudeApproval?.rawValue], ["workspace", "accept_edits"])
        XCTAssertNil(draft.planMode)
        draft.access = ProjectAccessChoice.plan.result(from: draft.access, capabilities: .published(.claude), supportsModes: true)
        XCTAssertEqual(draft.planMode, true)
        draft.chooseFamily(.codex, capabilities: .published(.codex))
        XCTAssertNil(draft.claudeApproval, "A Codex draft never carries a Claude approval")
        let detail = try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(#"{"conversationId":"c","projectId":"p","projectName":"P","title":"t","family":"claude","model":null,"effort":null,"accessMode":"workspace","workingFolder":"/w","workingFolderName":"w","isPinned":false,"hasUnread":false,"hasNativeSession":true,"folderInProject":true,"notice":null,"claudeApproval":"auto","planMode":true}"#.utf8))
        XCTAssertEqual(detail.access, ProjectAccess(accessMode: .workspace, claudeApproval: .auto, planMode: true))
        let older = try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(#"{"conversationId":"c","projectId":"p","projectName":"P","title":"t","family":"codex","model":null,"effort":null,"accessMode":"read_only","workingFolder":"/w","workingFolderName":"w","isPinned":false,"hasUnread":false,"hasNativeSession":true,"folderInProject":true,"notice":null}"#.utf8))
        XCTAssertEqual(older.access, ProjectAccess(accessMode: .readOnly))
    }

    // Contract: A missing Project destination moves to the default project
    // and keeps its words; a committed
    // project request never moves.
    func testMissingDestinationsSettleIntoTheDefaultProject() {
        func project(_ id: String, family: AgentFamily? = nil) -> ProjectSummary {
            ProjectSummary(id: id, name: id, folders: [ProjectFolder(id: id + "f", path: "/" + id, name: id, isPrimary: true, isAvailable: true)], lastFamily: family)
        }
        let projects = [project("first", family: .claude), project("second")]
        for retired in [nil, ChatDestination.project(id: "deleted")] {
            var draft = NewChatDraft(destination: retired, text: "Fix the parser")
            XCTAssertTrue(draft.settle(in: projects, capabilities: ProviderCapabilities.published))
            XCTAssertEqual(draft.destination, .project(id: "first"))
            XCTAssertEqual(draft.family, .claude)
            XCTAssertEqual(draft.text, "Fix the parser")
        }
        var current = NewChatDraft(destination: .project(id: "second"), text: "Keep")
        XCTAssertFalse(current.settle(in: projects, capabilities: ProviderCapabilities.published))
        XCTAssertEqual(current.destination, .project(id: "second"))
        var frozenProject = NewChatDraft(destination: .project(id: "deleted"), text: "Sent", submittedBody: "Sent")
        let request = frozenProject.requestID
        XCTAssertFalse(frozenProject.settle(in: projects, capabilities: ProviderCapabilities.published))
        XCTAssertEqual(frozenProject.requestID, request)
        var none = NewChatDraft(text: "Nowhere to go")
        XCTAssertFalse(none.settle(in: [], capabilities: ProviderCapabilities.published))
        XCTAssertNil(none.destination)
    }

    // Copy thread/session ID copies the provider's own ID, never Cupola's
    // reference prefix, and a draft without a provider thread has none.
    func testNativeIDIsTheProvidersThreadID() {
        func thread(_ reference: String, _ family: AgentFamily) -> ProjectThreadSummary {
            ProjectThreadSummary(reference: reference, conversationId: "c", title: "t", family: family, updatedAt: 0)
        }
        XCTAssertEqual(thread("codex:019a-77", .codex).nativeID, "019a-77")
        XCTAssertEqual(thread("claude:5e55-10a1", .claude).nativeID, "5e55-10a1")
        XCTAssertNil(thread("wonder:c", .codex).nativeID)
        XCTAssertNil(thread("codex:", .codex).nativeID)
        XCTAssertNil(thread("codex:019a", .claude).nativeID)
    }
}
